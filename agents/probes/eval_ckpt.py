"""Powered eval of any checkpoint against the standard ladder (random / heuristic), stdout JSON.

    .venv/bin/python -m probes.eval_ckpt <ckpt.pt> [--games 300] [--baselines random,heuristic]
"""

from __future__ import annotations

import argparse
import json

import torch

from ppo.flow_eval import evaluate_flow, make_scripted_heuristic
from probes.mcts_calib import POOL, load_ckpt


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ckpt")
    ap.add_argument("--games", type=int, default=300)
    ap.add_argument("--envs", type=int, default=128)
    ap.add_argument("--baselines", type=str, default="random,heuristic")
    args = ap.parse_args()

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    net, step = load_ckpt(args.ckpt, device)
    for name in args.baselines.split(","):
        opp = "random" if name == "random" else make_scripted_heuristic()
        r = evaluate_flow(net, opp, device, n_games=args.games, num_envs=args.envs,
                          team_pool=POOL, seed=31337, fog_species=net.fog_species)
        print(json.dumps({"ckpt": args.ckpt, "step": step, "baseline": name,
                          **{k: r[k] for k in ("win_rate", "ci_low", "ci_high",
                                               "wins", "losses", "draws")}}), flush=True)


if __name__ == "__main__":
    main()
