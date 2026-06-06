"""Offline evaluation of a trained policy against fixed baselines — including pmariglia's
poke-engine MCTS, our strong/validated reference opponent.

    uv run python -m ppo.eval --ckpt runs/<ts>/ckpt_000050.pt --baseline mcts --mcts-ms 100 --games 50
    uv run python -m ppo.eval --baseline random --games 200            # random-init model sanity

The MCTS baseline plays a perfect-information game, so it is at full strength. If our policy beats
strong MCTS by a non-trivial margin, suspect a bug (engine mismatch, illegal-move fallback, etc.)
rather than skill — MCTS here is meant to be hard to beat.
"""

from __future__ import annotations

import argparse
import json

import torch

import showdown_engine as se

from .baselines import HeuristicBaseline, MctsBaseline, RandomBaseline, evaluate
from .config import PPOConfig
from .model import ActorCritic
from .train import resolve_device


def build_model(ckpt_path, device, cfg):
    probe = se.Battle(0)
    embed = {"n_mons": probe.n_mons, "cols": probe.id_columns(), "vocab": probe.vocab_sizes(), "dim": cfg.embed_dim}
    if ckpt_path:
        ckpt = torch.load(ckpt_path, map_location=device, weights_only=False)
        model = ActorCritic(ckpt["obs_dim"], ckpt["n_actions"], ckpt["hidden_dim"], ckpt["n_hidden_layers"], embed=embed).to(device)
        model.load_state_dict(ckpt["model"])
        tag = f"{ckpt_path} (update {ckpt.get('update')})"
    else:
        model = ActorCritic(probe.obs_dim, probe.n_actions, cfg.hidden_dim, cfg.n_hidden_layers, embed=embed).to(device)
        tag = "random-init (no checkpoint)"
    model.eval()
    return model, tag


def wilson_interval(wins: int, n: int, z: float = 1.96):
    """95% Wilson score interval for a win-rate — meaningful at small N."""
    if n == 0:
        return (0.0, 0.0)
    p = wins / n
    denom = 1 + z * z / n
    center = (p + z * z / (2 * n)) / denom
    half = z * ((p * (1 - p) / n + z * z / (4 * n * n)) ** 0.5) / denom
    return (max(0.0, center - half), min(1.0, center + half))


def main():
    parser = argparse.ArgumentParser(description="Evaluate a policy vs fixed baselines (incl. poke-engine MCTS).")
    parser.add_argument("--ckpt", type=str, default=None, help="checkpoint path (omit = random-init model)")
    parser.add_argument("--baseline", type=str, default="mcts", choices=["mcts", "random", "heuristic"])
    parser.add_argument("--mcts-ms", type=int, default=100, help="MCTS search time per move (ms)")
    parser.add_argument("--games", type=int, default=50)
    parser.add_argument("--num-envs", type=int, default=4, help="parallel battles (MCTS is heavy — keep modest)")
    parser.add_argument("--device", type=str, default=None)
    parser.add_argument("--seed", type=int, default=1234567)
    parser.add_argument("--out", type=str, default=None, help="append the result as a JSON line to this file")
    args = parser.parse_args()

    cfg = PPOConfig()
    if args.device:
        cfg.device = args.device
    device = resolve_device(cfg.device)

    model, tag = build_model(args.ckpt, device, cfg)
    if args.baseline == "mcts":
        baseline = MctsBaseline(time_ms=args.mcts_ms)
    elif args.baseline == "heuristic":
        baseline = HeuristicBaseline()
    else:
        baseline = RandomBaseline(seed=args.seed)

    print(f"policy: {tag}")
    print(f"baseline: {baseline.name}  | {args.games} games, {args.num_envs} parallel battles, device={device}")
    print("evaluating (greedy policy, side-balanced)...")

    r = evaluate(model, baseline, device, n_games=args.games, num_envs=args.num_envs, seed=args.seed)
    lo, hi = wilson_interval(r["wins"], r["n_games"])
    print(f"\nwin_rate {r['win_rate']:.3f}  [95% CI {lo:.3f}, {hi:.3f}]  "
          f"W/L/D {r['wins']}/{r['losses']}/{r['draws']}  avg_turns {r['avg_turns']:.0f}")

    if args.out:
        with open(args.out, "a") as f:
            f.write(json.dumps({"ckpt": args.ckpt, **r, "ci_low": lo, "ci_high": hi, "mcts_ms": args.mcts_ms}) + "\n")


if __name__ == "__main__":
    main()
