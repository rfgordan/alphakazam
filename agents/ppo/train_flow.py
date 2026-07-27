"""Long-horizon PPO self-play on the **Rust decision-point engine** (`FlowVec`).

The trainer for scale runs: real Showdown rules (faint replacements, pivot landings, Tera),
13-action space, teams drawn from the pinned PS random-battle generator pool, and true
checkpoint/resume so a multi-day run survives restarts.

    python -m ppo.train_flow --num-envs 512 --total-steps 2000000000 --run-dir runs/scale1

Resume after any interruption (weights, optimizer, opponent snapshot, counters all restored):

    python -m ppo.train_flow --resume runs/scale1

Differences from `selfplay.py` (which drives the legacy whole-turn `Battle` bridge MDP):
  * `flow_env.FlowEnvVec` instead of `engine_env.EngineVecEnv` — see that module's docstring
    for why the bridge MDP is disqualifying, and for the `active` (acting-side) mask.
  * a single `--run-dir` that is resumed in place, rather than a fresh timestamped dir per launch.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import signal
import time
from pathlib import Path

import numpy as np
import torch

from .buffer import RolloutBuffer
from .config import PPOConfig
from .flow_env import FlowEnvVec
from .flow_eval import HEURISTIC_STATS, evaluate_flow, make_scripted_heuristic, standard_baselines
from .league import OpponentSlots, SnapshotLeague
from .model import ActorCritic
from .run_logger import RunLogger
from .train import ppo_update, resolve_device, set_seed

DEFAULT_POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"

# Set by the SIGTERM/SIGINT handler so the loop can finish the update it is in and checkpoint
# before exiting — a kill mid-update otherwise loses up to one rollout.
_STOP = False


def _install_signal_handlers():
    def handler(signum, _frame):
        global _STOP
        _STOP = True
        print(f"\n[train_flow] signal {signum} — finishing the current update, then checkpointing")

    signal.signal(signal.SIGTERM, handler)
    signal.signal(signal.SIGINT, handler)


@torch.no_grad()
def greedy_actions(net, obs, ids, mask, device):
    """Argmax over legal actions (the opponent/eval policy)."""
    logits, _ = net.forward(
        torch.as_tensor(obs, device=device),
        torch.as_tensor(mask, device=device),
        obs_ids=torch.as_tensor(ids, device=device),
    )
    return logits.argmax(dim=-1).cpu().numpy()


def build_model(cfg, env, device, aux: bool):
    embed = {"n_mons": env.n_mons, "cols": env.id_columns, "vocab": env.vocab, "dim": cfg.embed_dim}
    return ActorCritic(env.obs_dim, env.n_actions, cfg.hidden_dim, cfg.n_hidden_layers,
                       embed=embed, aux=aux).to(device)


def train(args):
    cfg = PPOConfig(
        num_envs=args.num_envs, rollout_steps=args.rollout_steps, total_steps=args.total_steps,
        lr=args.lr, gamma=args.gamma, gae_lambda=args.gae_lambda, clip_eps=args.clip_eps,
        entropy_coef=args.entropy_coef, value_coef=args.value_coef,
        update_epochs=args.update_epochs, minibatch_size=args.minibatch_size,
        hidden_dim=args.hidden_dim, n_hidden_layers=args.n_hidden_layers,
        embed_dim=args.embed_dim, seed=args.seed, device=args.device or "auto",
        shaping_coef=args.shaping_coef, aux=args.aux, target_kl=args.target_kl,
    )
    set_seed(cfg.seed)
    device = resolve_device(cfg.device)
    # TF32 matmuls: measured 2× on ppo_update at this run's exact shapes (2.61s -> 1.31s per
    # 131k-sample update on the A100) with fp32 accumulation — the standard Ampere trade.
    torch.set_float32_matmul_precision("high")
    run_dir = Path(args.run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    state_path = run_dir / "training_state.pt"

    pool = args.team_pool
    if pool and not os.path.isabs(pool):
        pool = str((Path(__file__).resolve().parents[1] / pool).resolve())
    if pool and not os.path.exists(pool):
        raise SystemExit(f"team pool not found: {pool} (run harness/gen-team-pool.mjs, or pass --team-pool '')")

    env = FlowEnvVec(cfg.num_envs, seed=cfg.seed, team_pool=pool or None,
                     max_requests=args.max_requests, shaping_coef=cfg.shaping_coef, gamma=cfg.gamma)
    if env.pool_size == 0:
        print("[train_flow] WARNING: no team pool loaded — every env replays the same fixed "
              "debug matchup. This is not a real training distribution.")

    # Peek at the saved state BEFORE building the model: architecture belongs to the run, not to
    # whatever flags this relaunch happened to pass — resuming a 1024-wide run with the default
    # --hidden-dim 256 must not die on a state_dict shape mismatch.
    ck = None
    if args.resume and state_path.exists():
        ck = torch.load(state_path, map_location=device, weights_only=False)
        for k in ("hidden_dim", "n_hidden_layers", "embed_dim"):
            if k in ck and getattr(cfg, k) != ck[k]:
                print(f"[train_flow] resume: {k} {getattr(cfg, k)} -> {ck[k]} (from checkpoint)")
                setattr(cfg, k, ck[k])

    model = build_model(cfg, env, device, aux=cfg.aux)
    # fused=True: the per-minibatch Adam step was 23% of trainer wall time (py-spy, post-TF32) —
    # 32 unfused step() calls per update over 10M params is kernel-launch soup. The fused CUDA
    # kernel shares the same state-dict format, so resume is unaffected.
    fused = device.type == "cuda"
    opt = torch.optim.Adam(model.parameters(), lr=cfg.lr, eps=1e-5, fused=fused)

    # League, not a single frozen self. Training only against the most recent snapshot lets the
    # pair co-adapt and forget everything it stopped seeing; the reservoir keeps past checkpoints
    # in the mix, PFSP-weighted (see ppo/league.py).
    # Scripted league members beyond `random`. The heuristic is the important one: the eval
    # opponent must also be a TRAINING opponent, or win-rate against it is pure transfer from
    # self-play (scale1: 0.18 at 26M steps; the poke-env recipe, which trained on it at base
    # weight 2, hit 0.46 in its first 22 minutes).
    scripted_opps: dict = {}
    scripted_weights: dict[str, float] = {}
    heur_stats: dict = {}
    if args.league_heuristic_weight > 0:
        scripted_opps["heuristic"] = make_scripted_heuristic(heur_stats)
        scripted_weights["heuristic"] = args.league_heuristic_weight
    league = SnapshotLeague(run_dir / "pool", keep=args.pool_size, pfsp_power=args.pfsp_power,
                            mode=args.pfsp_mode, random_weight=args.random_weight,
                            scripted_weights=scripted_weights)
    slots = OpponentSlots(args.opponent_slots, lambda: build_model(cfg, env, device, aux=cfg.aux),
                          device, scripted=scripted_opps)

    # The anchor is the run's random-init policy, frozen before any training — the fixed reference
    # that says whether the agent has learned anything at all.
    anchor = build_model(cfg, env, device, aux=False)
    anchor_path = run_dir / "anchor.pt"

    global_step, update, total_games = 0, 0, 0
    if ck is not None:
        model.load_state_dict(ck["model"])
        opt.load_state_dict(ck["opt"])
        global_step, update, total_games = ck["global_step"], ck["update"], ck["total_games"]
        league.load_state(ck.get("league", {}))
        print(f"[train_flow] resumed {state_path} @ update {update}, step {global_step:,}")
    elif args.resume:
        print(f"[train_flow] --resume: no {state_path} yet, starting fresh")

    # The anchor must be the SAME weights for the life of the run, or the "absolute" curve is
    # measured against a moving reference. Written once, reloaded on every resume.
    if anchor_path.exists():
        anchor.load_state_dict(torch.load(anchor_path, map_location=device, weights_only=True))
    else:
        torch.save(anchor.state_dict(), anchor_path)
    anchor.eval()
    for prm in anchor.parameters():
        prm.requires_grad_(False)
    # Seed the reservoir so the first updates have something to play that is not just `random`.
    if not league.snapshots():
        league.add(model, global_step)

    buffer = RolloutBuffer(cfg.rollout_steps, cfg.num_envs, env.obs_dim, env.n_actions, device,
                           id_dim=env.id_dim, aux=cfg.aux)
    logger = RunLogger(str(run_dir.parent), run_dir.name,
                       wandb_project=args.wandb_project if args.wandb else None)
    logger.config({"cfg": vars(cfg), "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                   "pool_size": env.pool_size, "params": model.num_params(),
                   "device": str(device), "env": "FlowEnvVec", "run_dir": str(run_dir)})

    batch = cfg.num_envs * cfg.rollout_steps
    indefinite = cfg.total_steps <= 0
    print(f"device={device}  params={model.num_params():,}  obs_dim={env.obs_dim}  "
          f"actions={env.n_actions}  batch={batch}  pool={env.pool_size}  "
          f"target={'INDEFINITE' if indefinite else f'{cfg.total_steps:,} steps'}")

    def save(tag: str | int):
        torch.save({"model": model.state_dict(), "opt": opt.state_dict(),
                    "league": league.save_state(), "global_step": global_step,
                    "update": update, "total_games": total_games,
                    "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                    "hidden_dim": cfg.hidden_dim, "n_hidden_layers": cfg.n_hidden_layers,
                    "embed_dim": cfg.embed_dim},
                   state_path)
        # A separate, weights-only artifact per checkpoint — this is what the on-policy cosim
        # sidecar and offline evals load, and it must never be a half-written training_state.
        ckpt = run_dir / f"ckpt_{int(global_step):012d}.pt"
        torch.save({"model": model.state_dict(), "global_step": global_step, "update": update,
                    "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                    "hidden_dim": cfg.hidden_dim, "n_hidden_layers": cfg.n_hidden_layers,
                    "embed_dim": cfg.embed_dim}, ckpt)
        if args.keep_checkpoints > 0:
            for old in sorted(glob.glob(str(run_dir / "ckpt_*.pt")))[:-args.keep_checkpoints]:
                os.remove(old)
        (run_dir / "LATEST").write_text(ckpt.name + "\n")

    baselines = standard_baselines(anchor, device, pool or None) if args.eval_every else []

    window: list[int] = []
    start, start_step = time.time(), global_step

    opp_rng = np.random.default_rng(cfg.seed ^ 0xBEEF)
    while not _STOP and (indefinite or global_step < cfg.total_steps):
        update += 1
        # Fresh draw from the reservoir each rollout, so an update sees several past selves.
        slots.assign(league, model.state_dict())
        for t in range(cfg.rollout_steps):
            obs_l, ids_l, mask_l, _ = env.learner_view()
            obs_o, ids_o, mask_o, _ = env.opponent_view()

            obs_t = torch.as_tensor(obs_l, device=device)
            ids_t = torch.as_tensor(ids_l, device=device)
            mask_t = torch.as_tensor(mask_l, device=device)
            with torch.no_grad():
                action, log_prob, _, value = model.act(obs_t, mask_t, obs_ids=ids_t)
            opp_action = slots.actions(obs_o, ids_o, mask_o, opp_rng,
                                       vec=env.vec, sides=1 - env.learner_side)

            reward, done, active, dyn, outcome = env.step(action.cpu().numpy(), opp_action)

            buffer.add(t, obs_t, mask_t, action, log_prob, value,
                       torch.as_tensor(reward, device=device),
                       torch.as_tensor(done, device=device),
                       obs_ids=ids_t,
                       opp_action=torch.as_tensor(opp_action, device=device) if cfg.aux else None,
                       dyn_target=torch.as_tensor(dyn, device=device) if cfg.aux else None,
                       active=torch.as_tensor(active.astype(np.float32), device=device))
            global_step += cfg.num_envs

            for e in np.flatnonzero(done):
                r = outcome[e]
                total_games += 1
                window.append(1 if r > 0 else (-1 if r < 0 else 0))
                # Credit the result to the opponent that actually played it — that is what makes
                # the PFSP weights mean anything.
                league.record(slots.env_key(int(e)), 1.0 if r > 0 else (0.0 if r < 0 else 0.5))

        with torch.no_grad():
            obs_l, ids_l, mask_l, _ = env.learner_view()
            _, last_value = model.forward(
                torch.as_tensor(obs_l, device=device),
                torch.as_tensor(mask_l, device=device),
                obs_ids=torch.as_tensor(ids_l, device=device))
        buffer.compute_gae(last_value, cfg.gamma, cfg.gae_lambda)
        stats = ppo_update(model, opt, buffer.flat_view(), cfg, batch)

        win_rate = float(np.mean([r == 1 for r in window])) if window else float("nan")
        sps = int((global_step - start_step) / max(1e-9, time.time() - start))
        print(f"update {update:>8}  step {global_step:>12,}  games {total_games:>7}  "
              f"win_rate(vs snapshot) {win_rate:5.2f}  pi {stats['policy_loss']:+.3f}  "
              f"v {stats['value_loss']:.3f}  ent {stats['entropy']:.3f}  "
              f"kl {stats['approx_kl']:.4f}  {sps} sps", flush=True)
        logger.metrics({"update": update, "step": global_step, "games": total_games,
                        "win_rate_vs_snapshot": win_rate, "sps": sps, **stats})

        if args.snapshot_every and update % args.snapshot_every == 0:
            league.add(model, global_step)
            window.clear()
            print(f"    [league +snapshot @ step {global_step:,}; "
                  f"pool={len(league.snapshots())}] {json.dumps(league.stats())[:220]}", flush=True)
            if heur_stats.get("fallbacks"):
                # The scripted league heuristic degrading to random would silently gut the
                # curriculum — same failure mode the eval path warns about, so warn here too.
                print(f"    [league] !! heuristic fallbacks {heur_stats['fallbacks']}"
                      f"/{heur_stats.get('calls', 0)} ({heur_stats.get('last_error', '?')})",
                      flush=True)
                heur_stats.clear()
            # `metrics()` keeps only scalars, so flatten the league summary into the
            # `pool/<opp>/{wr,weight}` keys train_long.py used for the nightX runs — snapshots
            # aggregate under `self` (mean wr over played snapshots / summed weight), which is
            # the nightX name for the frozen-checkpoint opponent.
            st = league.stats()
            scripted = {league.RANDOM, *league.scripted_weights}
            flat = {"update": update, "step": global_step, "league": st,
                    "pool_size": len(league.snapshots())}
            for k in scripted:
                if k in st:
                    flat[f"pool/{k}/wr"] = st[k]["wr"]
                    flat[f"pool/{k}/weight"] = st[k]["w"]
            snaps = {k: v for k, v in st.items() if k not in scripted}
            played = [v["wr"] for v in snaps.values() if v["n"] > 0]
            flat["pool/self/wr"] = (sum(played) / len(played)) if played else 0.5
            flat["pool/self/weight"] = sum(v["w"] for v in snaps.values())
            logger.metrics(flat)

        # --- absolute progress: fixed opponents, the only curve that means anything on its own ---
        if args.eval_every and update % args.eval_every == 0:
            for name, opp in baselines:
                r = evaluate_flow(model, opp, device, n_games=args.eval_games,
                                  num_envs=min(cfg.num_envs, 128), team_pool=pool or None,
                                  seed=cfg.seed + update)
                note = ""
                if name == "heuristic":
                    st = HEURISTIC_STATS.get("ref", {})
                    fb = st.get("fallbacks", 0)
                    if fb:
                        # A scripted baseline that cannot produce a move degrades to random and
                        # flatters the agent. Say so loudly rather than logging a pretty number.
                        note = (f"  !! {fb}/{st.get('calls', 0)} fallbacks "
                                f"({st.get('last_error', '?')}) — baseline NOT trustworthy")
                    st.clear()
                print(f"    [eval @ {update}] vs {name:>12}: win_rate {r['win_rate']:.3f} "
                      f"[{r['ci_low']:.3f},{r['ci_high']:.3f}] "
                      f"W/L/D {r['wins']}/{r['losses']}/{r['draws']}{note}", flush=True)
                logger.eval({"update": update, "step": global_step, "baseline": name, **r})
        if args.ckpt_every and update % args.ckpt_every == 0:
            save(update)

    save(update)
    if _STOP:
        print(f"[train_flow] stopped cleanly at update {update}, step {global_step:,}")
    else:
        print(f"[train_flow] done. update {update}, step {global_step:,}")


def main():
    p = argparse.ArgumentParser(description="Long-horizon PPO self-play on the Rust decision-point engine.")
    p.add_argument("--run-dir", type=str, default=None, help="run directory (created if absent)")
    p.add_argument("--resume", type=str, default=None, metavar="RUN_DIR",
                   help="resume this run directory in place (implies --run-dir)")
    p.add_argument("--num-envs", type=int, default=512)
    p.add_argument("--rollout-steps", type=int, default=64, help="PER ENV; batch = num_envs * this")
    p.add_argument("--total-steps", type=int, default=0, help="<= 0 trains indefinitely")
    p.add_argument("--max-requests", type=int, default=1000, help="decision points before truncation")
    p.add_argument("--team-pool", type=str, default=DEFAULT_POOL,
                   help="gzipped JSONL team pool; '' for the fixed debug matchup (not for real runs)")
    p.add_argument("--lr", type=float, default=3e-4)
    p.add_argument("--gamma", type=float, default=0.995)
    p.add_argument("--gae-lambda", type=float, default=0.95)
    p.add_argument("--clip-eps", type=float, default=0.2)
    p.add_argument("--entropy-coef", type=float, default=0.005)
    p.add_argument("--value-coef", type=float, default=0.5)
    p.add_argument("--update-epochs", type=int, default=4)
    p.add_argument("--minibatch-size", type=int, default=4096)
    p.add_argument("--hidden-dim", type=int, default=256)
    p.add_argument("--n-hidden-layers", type=int, default=2)
    p.add_argument("--embed-dim", type=int, default=32)
    p.add_argument("--shaping-coef", type=float, default=0.0)
    p.add_argument("--aux", action="store_true", help="auxiliary opponent-action / world-model heads")
    p.add_argument("--snapshot-every", type=int, default=20,
                   help="add the current policy to the league reservoir every N updates")
    p.add_argument("--pool-size", type=int, default=20, help="league reservoir size (0 = unbounded)")
    p.add_argument("--opponent-slots", type=int, default=4,
                   help="distinct league opponents played per rollout (one forward pass each)")
    p.add_argument("--pfsp-power", type=float, default=2.0)
    p.add_argument("--pfsp-mode", type=str, default="frontier", choices=["frontier", "hard"],
                   help="frontier = contested opponents first; hard = whoever currently beats us")
    p.add_argument("--random-weight", type=float, default=0.25,
                   help="fixed share of league sampling given to the uniform-random opponent")
    p.add_argument("--league-heuristic-weight", type=float, default=0.0,
                   help="PFSP base weight for the scripted heuristic as a TRAINING opponent "
                        "(0 = off). The proven curriculum used 2 vs 1-per-snapshot.")
    p.add_argument("--target-kl", type=float, default=0.0,
                   help=">0: cut the epoch loop when approx_kl exceeds this (recipe: 0.03)")
    p.add_argument("--eval-every", type=int, default=25,
                   help="powered eval vs fixed baselines every N updates (0 = never)")
    p.add_argument("--eval-games", type=int, default=300, help="games per baseline eval")
    p.add_argument("--ckpt-every", type=int, default=25)
    p.add_argument("--keep-checkpoints", type=int, default=5, help="0 keeps all")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--device", type=str, default=None, help="auto|cpu|cuda")
    p.add_argument("--wandb", action="store_true")
    p.add_argument("--wandb-project", type=str, default="deep-showdown")
    args = p.parse_args()

    if args.resume:
        args.run_dir = args.resume
        args.resume = True
    else:
        if not args.run_dir:
            args.run_dir = time.strftime("runs/flow-%Y%m%d-%H%M%S")
        args.resume = True  # resuming a fresh dir is a no-op; makes relaunch always safe
    _install_signal_handlers()
    train(args)


if __name__ == "__main__":
    main()
