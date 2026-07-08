"""PPO training against the **poke-env / live Showdown** environment (not the Rust engine).

Reuses our `ActorCritic` net + `RolloutBuffer`; the environment is `MaskedSingleAgentEnv` over a
`DeepShowdownSinglesEnv` (poke-env `SinglesEnv` on a local Showdown server) vs a fixed poke-env
opponent. Single synchronous env (battles are network-bound), auto-reset on game over.

Adds **KL early-stopping** to the PPO update — the Rust-engine run diagnosed instability from
over-large policy steps (approx_kl spiking to ~0.5); we stop a update's epochs once mean KL exceeds
`target_kl`, keeping each step trust-region-sized.

Prereq: a Showdown server on localhost:8000
    (cd engines/pokemon-showdown && node pokemon-showdown start --no-security)

Run:
    python -m ppo.pokeenv_train --total-steps 20000 --opponent random --eval-every 5
"""

from __future__ import annotations

import argparse
import json
import resource
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn

from poke_env.player import RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer

from .buffer import RolloutBuffer
from .model import ActorCritic
from .pokeenv_env import DeepShowdownSinglesEnv, MaskedSingleAgentEnv, OBS_DIM, N_ACTIONS

OPPONENTS = {
    "random": RandomPlayer,
    "maxbp": MaxBasePowerPlayer,
    "heuristic": SimpleHeuristicsPlayer,
}


def make_env(battle_format: str, opponent: str, hp_value=0.5, fainted_value=1.0, victory_value=30.0):
    env = DeepShowdownSinglesEnv(battle_format=battle_format, hp_value=hp_value,
                                 fainted_value=fainted_value, victory_value=victory_value)
    opp = OPPONENTS[opponent](battle_format=battle_format, start_listening=False)
    return MaskedSingleAgentEnv(env, opponent=opp)


def _t(x, device, dtype=torch.float32):
    return torch.as_tensor(np.asarray(x), dtype=dtype, device=device).unsqueeze(0)


@torch.no_grad()
def evaluate(model, eval_envs: dict, device, episodes: int = 20):
    """Greedy (argmax over legal actions) win-rate vs each fixed opponent.

    Takes pre-built, REUSED envs — creating a fresh env per eval leaks file descriptors
    (each PokeEnv opens websockets + an asyncio event loop that close() doesn't release),
    which exhausts `ulimit -n` after a few dozen evals.
    """
    out = {}
    for name, env in eval_envs.items():
        wins = 0
        for _ in range(episodes):
            obs, mask = env.reset()
            done, steps = False, 0
            while not done and steps < 500:
                logits, _ = model.forward(_t(obs, device), _t(mask, device, torch.bool))
                a = int(logits.argmax(-1).item())
                obs, mask, _, done = env.step(a)
                steps += 1
            wins += 1 if env.env.battle1.won else 0
        out[name] = wins / episodes
    return out


def train(args):
    device = torch.device(args.device)
    torch.manual_seed(args.seed)
    np.random.seed(args.seed)
    # Belt-and-suspenders vs FD exhaustion (each poke-env env holds websockets + an event loop).
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, min(hard, 8192)), hard))

    env = make_env(args.format, args.opponent, args.hp_value, args.fainted_value, args.victory_value)
    # Persistent eval envs (built once, reused every eval — see evaluate() docstring).
    eval_names = [s for s in args.eval_opponents.split(",") if s] if args.eval_every else []
    eval_envs = {n: make_env(args.format, n) for n in eval_names}
    if getattr(args, "arch", "mlp") == "slot":
        from .slot_model import SlotActorCritic
        model = SlotActorCritic(OBS_DIM, N_ACTIONS, args.hidden_dim, args.n_hidden_layers).to(device)
    else:
        model = ActorCritic(OBS_DIM, N_ACTIONS, args.hidden_dim, args.n_hidden_layers,
                            embed=None, aux=False).to(device)
    if getattr(args, "init_from", None):
        model.load_state_dict(torch.load(args.init_from, map_location=device))
        print(f"initialized weights from {args.init_from}")
    # Kickstarting anchor: a FROZEN teacher (usually the BC checkpoint). An annealed KL(teacher||policy)
    # penalty keeps PPO from drifting off the good BC policy while the (random) value head catches up.
    bc_model = None
    if getattr(args, "kick_from", None):
        bc_model = ActorCritic(OBS_DIM, N_ACTIONS, args.hidden_dim, args.n_hidden_layers,
                               embed=None, aux=False).to(device)
        bc_model.load_state_dict(torch.load(args.kick_from, map_location=device))
        bc_model.eval()
        for pth in bc_model.parameters():
            pth.requires_grad_(False)
        print(f"kickstart anchor from {args.kick_from} (coef {args.kick_coef}, annealed)")
    opt = torch.optim.Adam(model.parameters(), lr=args.lr, eps=1e-5)
    buf = RolloutBuffer(args.rollout_steps, 1, OBS_DIM, N_ACTIONS, device)

    run_dir = Path(args.run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    metrics_f = (run_dir / "metrics.jsonl").open("w")
    eval_f = (run_dir / "eval.jsonl").open("w")
    print(f"device={device} params={model.num_params():,} obs_dim={OBS_DIM} n_actions={N_ACTIONS} "
          f"opponent={args.opponent} format={args.format}")

    obs, mask = env.reset()
    obs_t, mask_t = _t(obs, device), _t(mask, device, torch.bool)
    # Per-episode reward accumulators, split into shaping (dense HP/faint) vs victory (terminal).
    ep_ret = ep_shaping = ep_victory = 0.0
    recent_returns, recent_wins, recent_shaping, recent_victory = [], [], [], []

    batch = args.rollout_steps
    num_updates = args.total_steps // batch
    global_step = 0
    start = time.time()

    for update in range(1, num_updates + 1):
        if args.anneal_lr:  # linear LR decay to 0 — a standard PPO stabilizer for smoother late training
            for g in opt.param_groups:
                g["lr"] = args.lr * (1.0 - (update - 1) / num_updates)

        # --- collect rollout ---
        for t in range(args.rollout_steps):
            with torch.no_grad():
                action, log_prob, _, value = model.act(obs_t, mask_t)
            a = int(action.item())
            next_obs, next_mask, reward, done = env.step(a)
            buf.add(t, obs_t.squeeze(0), mask_t.squeeze(0), action.squeeze(0),
                    log_prob.squeeze(0), value.squeeze(0),
                    torch.tensor(reward, device=device), torch.tensor(float(done), device=device))
            ep_ret += reward
            ep_shaping += env.last_shaping
            ep_victory += env.last_victory
            if done:
                recent_returns.append(ep_ret)
                recent_wins.append(1.0 if env.env.battle1.won else 0.0)
                recent_shaping.append(ep_shaping)
                recent_victory.append(ep_victory)
                ep_ret = ep_shaping = ep_victory = 0.0
                next_obs, next_mask = env.reset()
            obs_t, mask_t = _t(next_obs, device), _t(next_mask, device, torch.bool)
            global_step += 1

        # --- GAE ---
        with torch.no_grad():
            _, last_value = model.forward(obs_t, mask_t)
        buf.compute_gae(last_value.squeeze(0), args.gamma, args.gae_lambda)
        data = buf.flat_view()

        # Critic quality: explained variance of the value fn over the rollout (scale-free, ->1 = good).
        # This — not raw value_loss (which just tracks reward scale) — is the critic's "convergence" signal.
        with torch.no_grad():
            y_true, y_pred = data["returns"], data["values"]
            var = y_true.var()
            explained_var = float(1 - (y_true - y_pred).var() / (var + 1e-8)) if var > 0 else 0.0

        # Entropy annealing: explore early to reach a high level, then decay the entropy bonus so
        # the policy COMMITS late -> stable high plateau (resolves the level-vs-smoothness trade-off).
        ent_coef = args.entropy_coef * (1.0 - (update - 1) / num_updates) if args.anneal_entropy \
            else args.entropy_coef

        # --- PPO update with KL early-stop (+ optional annealed kickstart anchor) ---
        kick = args.kick_coef * (1.0 - (update - 1) / num_updates) if bc_model is not None else 0.0
        stats = ppo_update(model, opt, data, args, batch, ent_coef, bc_model, kick)
        stats["explained_var"] = explained_var
        stats["ent_coef"] = ent_coef

        if update % args.log_every == 0:
            mean = lambda xs: float(np.mean(xs[-50:])) if xs else float("nan")
            wr, ret = mean(recent_wins), mean(recent_returns)
            ret_shaping, ret_victory = mean(recent_shaping), mean(recent_victory)
            sps = int(global_step / (time.time() - start))
            row = dict(update=update, step=global_step, win_rate=wr, ep_return=ret,
                       ret_victory=ret_victory, ret_shaping=ret_shaping, sps=sps, **stats)
            print(f"upd {update:3d}/{num_updates} step {global_step:>6} wr {wr:.2f} "
                  f"ret {ret:+.2f} (win {ret_victory:+.2f} / shape {ret_shaping:+.2f}) "
                  f"pi {stats['policy_loss']:+.3f} v {stats['value_loss']:.2f} ev {explained_var:+.2f} "
                  f"ent {stats['entropy']:.3f} kl {stats['approx_kl']:.4f} epochs {stats['epochs']} {sps}sps")
            metrics_f.write(json.dumps(row) + "\n"); metrics_f.flush()

        if args.eval_every and update % args.eval_every == 0:
            try:
                wr = evaluate(model, eval_envs, device, episodes=args.eval_episodes)
                row = dict(update=update, step=global_step, **wr)
                print(f"  [eval] " + "  ".join(f"{k}={v:.2f}" for k, v in wr.items()))
                eval_f.write(json.dumps(row) + "\n"); eval_f.flush()
            except Exception as e:  # a flaky eval must never kill a long training run
                print(f"  [eval] skipped: {type(e).__name__}: {e}")

        if args.save_every and update % args.save_every == 0:
            torch.save(model.state_dict(), run_dir / f"model_{global_step}.pt")

    torch.save(model.state_dict(), run_dir / "model_final.pt")
    metrics_f.close(); eval_f.close()
    env.env.close()
    for e in eval_envs.values():
        e.env.close()
    print(f"done. saved {run_dir/'model_final.pt'}")
    return model


def ppo_update(model, opt, data, args, batch_size, ent_coef, bc_model=None, kick_coef=0.0) -> dict:
    idx = np.arange(batch_size)
    adv = data["advantages"]
    if args.norm_advantages:
        adv = (adv - adv.mean()) / (adv.std() + 1e-8)
    # Gradient accumulation: take one optimizer step per `accum` minibatches, so the effective
    # gradient batch = accum * minibatch_size without holding it all in memory at once. accum=1
    # is plain minibatch SGD. (Only needed once the net/minibatch is memory-bound — e.g. on GPU.)
    accum = max(1, args.grad_accum)
    last = {}
    epochs_run = 0
    for _ in range(args.update_epochs):
        np.random.shuffle(idx)
        epoch_kls = []
        starts = list(range(0, batch_size, args.minibatch_size))
        opt.zero_grad()
        for i, start in enumerate(starts):
            mb = torch.as_tensor(idx[start:start + args.minibatch_size], device=data["obs"].device)
            _, new_lp, entropy, new_v = model.act(data["obs"][mb], data["masks"][mb], data["actions"][mb])
            ratio = (new_lp - data["log_probs"][mb]).exp()
            a = adv[mb]
            pol = -torch.min(ratio * a, torch.clamp(ratio, 1 - args.clip_eps, 1 + args.clip_eps) * a).mean()
            vloss = 0.5 * (new_v - data["returns"][mb]).pow(2).mean()
            loss = pol + args.value_coef * vloss - ent_coef * entropy.mean()
            if bc_model is not None and kick_coef > 0:
                # KL(teacher || policy) on this minibatch — pins the policy near the BC teacher.
                with torch.no_grad():
                    bc_logits, _ = bc_model.forward(data["obs"][mb], data["masks"][mb])
                cur_logits, _ = model.forward(data["obs"][mb], data["masks"][mb])
                bc_lp, cur_lp = torch.log_softmax(bc_logits, -1), torch.log_softmax(cur_logits, -1)
                loss = loss + kick_coef * (bc_lp.exp() * (bc_lp - cur_lp)).sum(-1).mean()
            loss = loss / accum
            loss.backward()
            if (i + 1) % accum == 0 or (i + 1) == len(starts):
                nn.utils.clip_grad_norm_(model.parameters(), args.max_grad_norm)
                opt.step()
                opt.zero_grad()
            with torch.no_grad():
                epoch_kls.append((data["log_probs"][mb] - new_lp).mean().item())
            last = dict(policy_loss=pol.item(), value_loss=vloss.item(), entropy=entropy.mean().item())
        epochs_run += 1
        mean_kl = float(np.mean(epoch_kls))
        last["approx_kl"] = mean_kl
        if args.target_kl is not None and mean_kl > args.target_kl:
            break  # KL early-stop: this update already moved the policy far enough
    last["epochs"] = epochs_run
    return last


def main():
    p = argparse.ArgumentParser(description="PPO on poke-env / live Showdown.")
    p.add_argument("--total-steps", type=int, default=20_000)
    p.add_argument("--rollout-steps", type=int, default=2048,
                   help="on-policy transitions per update (THE batch; bigger = more stable)")
    p.add_argument("--opponent", choices=list(OPPONENTS), default="random")
    p.add_argument("--format", default="gen9randombattle")
    p.add_argument("--device", default="cpu")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--hidden-dim", type=int, default=256)
    p.add_argument("--n-hidden-layers", type=int, default=2)
    p.add_argument("--arch", choices=["mlp", "slot"], default="mlp",
                   help="policy head: flat MLP or slot-equivariant shared scorers")
    p.add_argument("--lr", type=float, default=3e-4)
    p.add_argument("--gamma", type=float, default=0.99)
    p.add_argument("--gae-lambda", type=float, default=0.95)
    p.add_argument("--clip-eps", type=float, default=0.2)
    p.add_argument("--entropy-coef", type=float, default=0.01)
    p.add_argument("--value-coef", type=float, default=0.5)
    p.add_argument("--max-grad-norm", type=float, default=0.5)
    p.add_argument("--update-epochs", type=int, default=4)
    p.add_argument("--minibatch-size", type=int, default=256)
    p.add_argument("--grad-accum", type=int, default=1,
                   help="optimizer step per N minibatches (effective batch = N * minibatch-size)")
    p.add_argument("--norm-advantages", action="store_true", default=True)
    p.add_argument("--target-kl", type=float, default=0.03, help="KL early-stop threshold; 0 disables")
    # Reward weights: winning dominates when victory_value >> hp_value*6 + fainted_value*6.
    p.add_argument("--hp-value", type=float, default=0.5, help="per-mon HP-fraction shaping weight")
    p.add_argument("--fainted-value", type=float, default=1.0, help="per-KO shaping weight")
    p.add_argument("--victory-value", type=float, default=30.0, help="terminal win/loss reward (dominant)")
    p.add_argument("--anneal-lr", action="store_true", help="linearly decay LR to 0 over training")
    p.add_argument("--anneal-entropy", action="store_true", help="linearly decay entropy bonus to 0 (explore->commit)")
    p.add_argument("--eval-every", type=int, default=0, help="updates between evals (0 = off)")
    p.add_argument("--eval-episodes", type=int, default=20)
    p.add_argument("--eval-opponents", default="random,maxbp,heuristic", help="comma list of eval opponents")
    p.add_argument("--save-every", type=int, default=50, help="updates between checkpoints (0 = off)")
    p.add_argument("--log-every", type=int, default=1)
    p.add_argument("--run-dir", default="runs/pokeenv")
    p.add_argument("--init-from", default=None, help="load policy weights from this checkpoint before PPO")
    p.add_argument("--kick-from", default=None, help="frozen teacher checkpoint for the kickstart KL anchor")
    p.add_argument("--kick-coef", type=float, default=1.0, help="initial kickstart KL weight (annealed to 0)")
    args = p.parse_args()
    if args.target_kl == 0:
        args.target_kl = None
    train(args)


if __name__ == "__main__":
    main()
