"""Frozen-snapshot self-play PPO training against the Rust engine.

The learner samples actions (its transitions feed PPO) and is assigned a **random side each
episode**, so it learns to play both teams. The opponent plays greedily from a **frozen snapshot**
of the learner, refreshed every `--snapshot-every` updates. Reward is sparse: +1 / -1 on win/loss.

Progress is tracked two ways: `win_rate(vs snapshot)` (marginal improvement over a recent self)
and a periodic **eval vs fixed baselines** (absolute progress — vs a frozen random-init anchor and
a random bot by default; see `baselines.py` to add more). Runs are logged to `runs/<timestamp>/`.

Run:
    uv run python -m ppo.selfplay                                   # finite default run
    uv run python -m ppo.selfplay --total-steps 0                  # train indefinitely (Ctrl-C)
    uv run python -m ppo.selfplay --watch                          # watch one game, no training
"""

from __future__ import annotations

import argparse
import copy
import glob
import os
import time

import numpy as np
import torch

import showdown_engine as se

from .baselines import HeuristicBaseline, MctsBaseline, PokeEnvBaseline, PolicyBaseline, RandomBaseline, evaluate, greedy_actions
from .buffer import RolloutBuffer
from .config import PPOConfig
from .engine_env import BLUE, RED, EngineVecEnv
from .model import ActorCritic
from .run_logger import RunLogger
from .train import ppo_update, resolve_device, set_seed


def train_selfplay(
    cfg: PPOConfig,
    render_every: int = 0,
    max_games: int | None = None,
    snapshot_every: int = 10,
    eval_every: int = 25,
    eval_games: int = 100,
    ckpt_every: int = 50,
    keep_checkpoints: int = 3,
    mcts_eval_ms: int = 0,
    mcts_eval_games: int = 20,
    poke_env_eval: tuple[str, ...] = (),
    logdir: str = "runs",
    run_name: str | None = None,
    wandb_project: str | None = None,
):
    """Frozen-snapshot self-play with periodic fixed-baseline eval and file logging.

    `cfg.total_steps <= 0` trains indefinitely (until Ctrl-C). The opponent is a frozen snapshot
    of the learner refreshed every `snapshot_every` updates (0 = live). Every `eval_every`
    updates the (greedy) learner is evaluated against each fixed baseline in `baselines` — by
    default a frozen random-init anchor and a random bot — giving an *absolute* progress curve.
    Everything is logged to `runs/<timestamp>/` (metrics.jsonl, eval.jsonl, config.json, ckpts).
    """
    set_seed(cfg.seed)
    device = resolve_device(cfg.device)

    envs = EngineVecEnv(cfg.num_envs, seed=cfg.seed, shaping_coef=cfg.shaping_coef, gamma=cfg.gamma)
    # Embedding spec for the categorical IDs (species/ability/item/tera/moves), read from the env.
    embed = {"n_mons": envs.n_mons, "cols": envs.id_columns, "vocab": envs.vocab, "dim": cfg.embed_dim}

    def make_net(aux=False):
        return ActorCritic(envs.obs_dim, envs.n_actions, cfg.hidden_dim, cfg.n_hidden_layers, embed=embed, aux=aux).to(device)

    model = make_net(aux=cfg.aux)  # learner gets the auxiliary prediction heads
    optimizer = torch.optim.Adam(model.parameters(), lr=cfg.lr, eps=1e-5)
    buffer = RolloutBuffer(cfg.rollout_steps, cfg.num_envs, envs.obs_dim, envs.n_actions, device,
                           id_dim=envs.id_dim, aux=cfg.aux)

    # The frozen *training* opponent: a copy of the learner, refreshed periodically.
    use_snapshot = snapshot_every > 0
    opponent = make_net(aux=cfg.aux)  # match architecture so load_state_dict works
    opponent.load_state_dict(model.state_dict())
    opponent.eval()
    for p in opponent.parameters():
        p.requires_grad_(False)
    blue_net = opponent if use_snapshot else model

    # Fixed *evaluation* baselines (absolute progress). The anchor captures the random-init policy
    # before any training. Swap/extend this list to add other periodic baselines (heuristics, a
    # specific checkpoint, ...) — anything implementing baselines.Baseline.
    anchor = copy.deepcopy(model).to(device).eval()
    for p in anchor.parameters():
        p.requires_grad_(False)
    baselines = [
        PolicyBaseline(anchor, device, name="anchor-init"),
        RandomBaseline(seed=cfg.seed, name="random"),
        HeuristicBaseline(),  # poke-env-style heuristic — fast, deterministic middle baseline
    ]
    if mcts_eval_ms > 0:
        # poke-engine MCTS — a strong, external validated reference (heavy; keep eval_games modest).
        baselines.append(MctsBaseline(time_ms=mcts_eval_ms))
    for kind in poke_env_eval:
        # Real poke-env players (MaxBasePower / SimpleHeuristics / Random) as external opponents.
        baselines.append(PokeEnvBaseline(kind=kind))

    logger = RunLogger(logdir, run_name, wandb_project=wandb_project)
    logger.config({"cfg": vars(cfg), "obs_dim": envs.obs_dim, "n_actions": envs.n_actions,
                   "snapshot_every": snapshot_every, "eval_every": eval_every,
                   "eval_games": eval_games, "ckpt_every": ckpt_every, "params": model.num_params(),
                   "device": str(device), "baselines": [b.name for b in baselines]})

    mode = f"frozen snapshot/{snapshot_every}" if use_snapshot else "live"
    indefinite = cfg.total_steps <= 0
    batch_size = cfg.num_envs * cfg.rollout_steps
    num_updates = None if indefinite else max(1, cfg.total_steps // batch_size)
    print(f"device={device}  params={model.num_params():,}  obs_dim={envs.obs_dim}  batch={batch_size}  "
          f"opponent={mode}  eval_every={eval_every}({eval_games})  {'INDEFINITE' if indefinite else num_updates} updates")

    global_step = 0
    total_games = 0
    wins, losses, draws = 0, 0, 0
    window_results: list[int] = []  # since the last snapshot refresh; basis for win_rate(vs snapshot)
    label = "snapshot" if use_snapshot else "live"
    start = time.time()
    update = 0

    def save_ckpt(tag: int):
        torch.save({
            "update": tag, "global_step": global_step,
            "model": model.state_dict(),
            "obs_dim": envs.obs_dim, "n_actions": envs.n_actions,
            "hidden_dim": cfg.hidden_dim, "n_hidden_layers": cfg.n_hidden_layers,
        }, logger.checkpoint_path(tag))
        # Rolling retention: keep only the most recent `keep_checkpoints` (0 = keep all, e.g. for
        # a future league/snapshot pool). Zero-padded names sort chronologically.
        if keep_checkpoints and keep_checkpoints > 0:
            existing = sorted(glob.glob(os.path.join(logger.dir, "ckpt_*.pt")))
            for old in existing[:-keep_checkpoints]:
                os.remove(old)

    try:
        while True:
            update += 1
            # --- collect a rollout of the learner's transitions (from whichever side it's on) ---
            for t in range(cfg.rollout_steps):
                obs_l, ids_l, mask_l = envs.learner_view()
                obs_o, ids_o, mask_o = envs.opponent_view()

                obs_t = torch.as_tensor(obs_l, device=device)
                ids_t = torch.as_tensor(ids_l, device=device)
                mask_t = torch.as_tensor(mask_l, device=device)
                with torch.no_grad():
                    action, log_prob, _, value = model.act(obs_t, mask_t, obs_ids=ids_t)  # learner: sample
                opp_action = greedy_actions(blue_net, obs_o, ids_o, mask_o, device)        # opponent: greedy

                reward_np, done_np, dyn_np, outcome_np = envs.step(action.cpu().numpy(), opp_action)

                buffer.add(t, obs_t, mask_t, action, log_prob, value,
                           torch.as_tensor(reward_np, device=device),
                           torch.as_tensor(done_np, device=device),
                           obs_ids=ids_t,
                           opp_action=torch.as_tensor(opp_action, device=device) if cfg.aux else None,
                           dyn_target=torch.as_tensor(dyn_np, device=device) if cfg.aux else None)
                global_step += cfg.num_envs

                for r, d in zip(outcome_np, done_np):  # true win/loss, not the shaped reward
                    if d:
                        res = 1 if r > 0 else (-1 if r < 0 else 0)
                        total_games += 1
                        window_results.append(res)
                        wins += res == 1
                        losses += res == -1
                        draws += res == 0

            # --- advantages + PPO update ---
            with torch.no_grad():
                obs_l, ids_l, mask_l = envs.learner_view()
                _, last_value = model.forward(
                    torch.as_tensor(obs_l, device=device),
                    torch.as_tensor(mask_l, device=device),
                    obs_ids=torch.as_tensor(ids_l, device=device),
                )
            buffer.compute_gae(last_value, cfg.gamma, cfg.gae_lambda)
            stats = ppo_update(model, optimizer, buffer.flat_view(), cfg, batch_size)

            win_rate = float(np.mean([r == 1 for r in window_results])) if window_results else float("nan")
            sps = int(global_step / (time.time() - start))
            upd_str = f"{update}" if indefinite else f"{update}/{num_updates}"
            print(f"update {upd_str:>9}  step {global_step:>9}  games {total_games:>5}  "
                  f"win_rate(vs {label}) {win_rate:5.2f}  "
                  f"pi {stats['policy_loss']:+.3f}  v {stats['value_loss']:.3f}  "
                  f"ent {stats['entropy']:.3f}  kl {stats['approx_kl']:.4f}  aux {stats.get('aux_loss', 0):.3f}  {sps} sps")
            logger.metrics({"update": update, "step": global_step, "games": total_games,
                            "win_rate_vs_snapshot": win_rate, "sps": sps, **stats})

            # --- refresh the frozen training opponent ---
            if use_snapshot and update % snapshot_every == 0:
                opponent.load_state_dict(model.state_dict())
                window_results.clear()
                print(f"    [snapshot refreshed @ update {update}]")

            # --- periodic eval vs fixed baselines (absolute progress) ---
            if eval_every and update % eval_every == 0:
                for bl in baselines:
                    # MCTS is slow but low-variance (the policy reliably loses), so it uses a
                    # smaller, separate game count; cheap baselines use the larger eval_games.
                    n = mcts_eval_games if isinstance(bl, MctsBaseline) else eval_games
                    r = evaluate(model, bl, device, n_games=n)
                    print(f"    [eval @ {update}] vs {bl.name:>11}: win_rate {r['win_rate']:.2f}  "
                          f"(W/L/D {r['wins']}/{r['losses']}/{r['draws']}, avg_turns {r['avg_turns']:.0f})")
                    logger.eval({"update": update, "step": global_step, **r})

            if ckpt_every and update % ckpt_every == 0:
                save_ckpt(update)

            if render_every and update % render_every == 0:
                print()
                play_one_game(model, device, seed=1000 + update, max_turns=200)
                print()

            if max_games is not None and total_games >= max_games:
                print(f"\nReached {total_games} games (target {max_games}).")
                break
            if not indefinite and update >= num_updates:
                break
    except KeyboardInterrupt:
        print("\ninterrupted — saving final checkpoint.")
    finally:
        save_ckpt(update)
        print(f"record W/L/D = {wins}/{losses}/{draws} over {total_games} games. run dir: {logger.dir}")
        logger.close()

    return model


@torch.no_grad()
def play_one_game(model, device, seed: int = 0, max_turns: int = 200, sample: bool = True):
    """Play one full self-play game to the terminal with natural-language commentary."""
    b = se.Battle(seed=seed)
    print("=" * 64)
    print("  WATCH: one self-play game (both sides = current policy)")
    print("=" * 64)
    print(b.render())

    def pick(side):
        obs = torch.as_tensor(np.asarray(b.observe(side), dtype=np.float32), device=device).unsqueeze(0)
        ids = torch.as_tensor(np.asarray(b.observe_ids(side), dtype=np.int64), device=device).unsqueeze(0)
        mask = torch.as_tensor(np.asarray(b.legal_actions(side), dtype=bool), device=device).unsqueeze(0)
        if sample:
            action, _, _, _ = model.act(obs, mask, obs_ids=ids)
            return int(action.item())
        logits, _ = model.forward(obs, mask, obs_ids=ids)
        return int(logits.argmax(dim=-1).item())

    for _ in range(max_turns):
        ar, ab = pick(RED), pick(BLUE)
        done, winner, lines = b.step(ar, ab, narrate=True)
        for ln in lines:
            print(ln)
        if done:
            name = {0: "Red", 1: "Blue", -1: "Nobody (draw/timeout)"}[winner]
            print(f"\n>>> {name} wins on turn {b.turn}.")
            print(b.render())
            return
    print(f"\n>>> Game hit the {max_turns}-turn cap (draw).")
    print(b.render())


def main():
    parser = argparse.ArgumentParser(description="Frozen-snapshot self-play PPO against the Rust engine.")
    parser.add_argument("--total-steps", type=int, default=300_000, help="<= 0 trains indefinitely (Ctrl-C to stop)")
    parser.add_argument("--games", type=int, default=None, help="stop after this many completed games")
    parser.add_argument("--device", type=str, default=None, help="auto|cpu|mps|cuda")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--render-every", type=int, default=0, help="watch a game every N updates (0 = never)")
    parser.add_argument("--snapshot-every", type=int, default=10,
                        help="refresh the frozen opponent every N updates (0 = live moving-target opponent)")
    parser.add_argument("--eval-every", type=int, default=25, help="eval vs fixed baselines every N updates (0 = never)")
    parser.add_argument("--eval-games", type=int, default=100, help="games per baseline evaluation")
    parser.add_argument("--ckpt-every", type=int, default=50, help="save a checkpoint every N updates (0 = never)")
    parser.add_argument("--keep-checkpoints", type=int, default=3,
                        help="keep only the most recent N checkpoints (0 = keep all)")
    parser.add_argument("--mcts-eval-ms", type=int, default=0,
                        help="also eval vs poke-engine MCTS at this search time per move (0 = off; heavy)")
    parser.add_argument("--mcts-eval-games", type=int, default=20,
                        help="games for the MCTS eval (separate from --eval-games; MCTS is slow + low-variance)")
    parser.add_argument("--pokeenv-eval", type=str, default="",
                        help="comma-separated real poke-env opponents to add to the periodic eval: "
                             "max,heuristic,random (empty = none)")
    parser.add_argument("--shaping-coef", type=float, default=None,
                        help="potential-based reward shaping weight (Φ=team HP diff; 0 = off, ~0.5 to enable)")
    parser.add_argument("--logdir", type=str, default="runs", help="root directory for run logs")
    parser.add_argument("--run-name", type=str, default=None, help="run subdirectory name (default: timestamp)")
    parser.add_argument("--wandb", action="store_true", help="mirror metrics/evals to Weights & Biases")
    parser.add_argument("--wandb-project", type=str, default="deep-showdown", help="W&B project name")
    parser.add_argument("--watch", action="store_true", help="just play one game with the untrained policy and exit")
    args = parser.parse_args()

    cfg = PPOConfig()
    cfg.total_steps = args.total_steps
    cfg.seed = args.seed
    if args.device is not None:
        cfg.device = args.device
    if args.shaping_coef is not None:
        cfg.shaping_coef = args.shaping_coef

    if args.watch:
        device = resolve_device(cfg.device)
        probe = se.Battle(seed=0)
        embed = {"n_mons": probe.n_mons, "cols": probe.id_columns(), "vocab": probe.vocab_sizes(), "dim": cfg.embed_dim}
        model = ActorCritic(probe.obs_dim, probe.n_actions, cfg.hidden_dim, cfg.n_hidden_layers, embed=embed).to(device)
        play_one_game(model, device, seed=args.seed)
        return

    train_selfplay(
        cfg,
        render_every=args.render_every,
        max_games=args.games,
        snapshot_every=args.snapshot_every,
        eval_every=args.eval_every,
        eval_games=args.eval_games,
        ckpt_every=args.ckpt_every,
        keep_checkpoints=args.keep_checkpoints,
        mcts_eval_ms=args.mcts_eval_ms,
        mcts_eval_games=args.mcts_eval_games,
        poke_env_eval=tuple(k.strip() for k in args.pokeenv_eval.split(",") if k.strip()),
        logdir=args.logdir,
        run_name=args.run_name,
        wandb_project=args.wandb_project if args.wandb else None,
    )


if __name__ == "__main__":
    main()
