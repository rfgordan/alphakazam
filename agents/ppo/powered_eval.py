"""Powered (properly-sampled) greedy evaluation of a checkpoint — the ONLY eval that counts for
claims. In-run n=60 evals are a ±0.10 instrument and manufacture false breakthroughs (see
EXPERIMENTS.md decision principle #7).

    python -m ppo.powered_eval runs/night4/model_final.pt --opponent heuristic --episodes 500

Needs a Showdown server on :8000. Flake-tolerant. Handles any checkpoint era load_model supports.
"""

from __future__ import annotations

import argparse
import math

import numpy as np
import torch

from poke_env.player import RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer

from .model_player import load_model
from .pokeenv_env import DeepShowdownSinglesEnv, MaskedSingleAgentEnv

OPPONENTS = {"random": RandomPlayer, "maxbp": MaxBasePowerPlayer, "heuristic": SimpleHeuristicsPlayer}


def powered_eval(ckpt: str, opponent: str = "heuristic", episodes: int = 300,
                 seed: int = 11, fmt: str = "gen9randombattle") -> tuple[float, float, int]:
    model, _, _ = load_model(ckpt)
    frames = getattr(model, "frames", 1)

    def make():
        env = DeepShowdownSinglesEnv(battle_format=fmt)
        return env, MaskedSingleAgentEnv(
            env, OPPONENTS[opponent](battle_format=fmt, start_listening=False), frames=frames)

    env, wrap = make()
    np.random.seed(seed)
    wins = done = flakes = 0
    while done < episodes:
        try:
            obs, mask = wrap.reset()
            over = False
            while not over:
                with torch.no_grad():
                    lg, _ = model.forward(torch.as_tensor(obs).unsqueeze(0).float(),
                                          torch.as_tensor(mask).unsqueeze(0).bool())
                obs, mask, _, over = wrap.step(int(lg.argmax(-1).item()))
            wins += env.battle1.won
            done += 1
        except Exception:
            flakes += 1
            if flakes > 5:
                raise
            try:
                env.close()
            except Exception:
                pass
            env, wrap = make()
    env.close()
    p = wins / episodes
    return p, 1.96 * math.sqrt(p * (1 - p) / episodes), wins


def main():
    ap = argparse.ArgumentParser(description="Powered greedy eval (the eval that counts).")
    ap.add_argument("checkpoint")
    ap.add_argument("--opponent", choices=list(OPPONENTS), default="heuristic")
    ap.add_argument("--episodes", type=int, default=300)
    ap.add_argument("--seed", type=int, default=11)
    ap.add_argument("--format", default="gen9randombattle")
    args = ap.parse_args()
    p, ci, wins = powered_eval(args.checkpoint, args.opponent, args.episodes, args.seed, args.format)
    print(f"POWERED: {args.checkpoint} vs {args.opponent}: "
          f"{p:.3f} +/- {ci:.3f} ({wins}/{args.episodes})")


if __name__ == "__main__":
    main()
