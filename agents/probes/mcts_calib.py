"""P0b calibration: a checkpoint vs pmariglia's MCTS, with per-action timing on both arms.

    .venv/bin/python -m probes.mcts_calib runs/scale2/ckpt_XXXX.pt --games 200 --time-ms 100

Prints win-rate + Wilson CI and the measured per-action wall clock of policy and MCTS —
the matched-time bookkeeping EXPLORATION_PLAN's fairness rule requires.
"""

from __future__ import annotations

import argparse
import json
import time

import numpy as np
import torch

import showdown_engine as se

from ppo.flow_eval import evaluate_flow, make_mcts_opponent
from ppo.model import ActorCritic

POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"


def load_ckpt(path: str, device):
    ck = torch.load(path, map_location=device, weights_only=False)
    meta = se.Battle(seed=0)
    embed = {"n_mons": meta.n_mons, "cols": meta.id_columns(), "vocab": meta.vocab_sizes(),
             "dim": ck.get("embed_dim", 32)}
    # Checkpoints carry whatever optional heads their run trained (--aux, --outcome-head);
    # build to match or the strict load fails — silently, if a window script eats stderr.
    has_aux = any(k.startswith("aux_") for k in ck["model"])
    has_outcome = any(k.startswith("outcome_head.") for k in ck["model"])
    net = ActorCritic(ck["obs_dim"], ck["n_actions"], ck.get("hidden_dim", 256),
                      ck.get("n_hidden_layers", 2), embed=embed, aux=has_aux,
                      outcome=has_outcome).to(device)
    net.load_state_dict(ck["model"])
    net.eval()
    # Ride the fog setting along so evaluators construct MATCHING envs — a fog-trained net
    # evaluated on leaky obs (or vice versa) is silently out of distribution.
    net.fog_species = bool(ck.get("fog_species", False))
    return net, ck.get("global_step", 0)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ckpt")
    ap.add_argument("--games", type=int, default=200)
    ap.add_argument("--time-ms", type=int, default=100)
    ap.add_argument("--envs", type=int, default=32,
                    help="small: MCTS is sequential per acting env")
    args = ap.parse_args()

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    net, step = load_ckpt(args.ckpt, device)
    print(f"checkpoint {args.ckpt} @ step {step:,}; mcts@{args.time_ms}ms; "
          f"{args.games} games on {args.envs} envs")

    # Wrap the opponent to measure its per-action cost (the policy's is measured separately
    # below — evaluate_flow itself doesn't time arms).
    inner = make_mcts_opponent(args.time_ms)
    t_mcts = [0.0, 0]

    def timed(vec, envs, mask, rng):
        t0 = time.perf_counter()
        out = inner(vec, envs, mask, rng)
        t_mcts[0] += time.perf_counter() - t0
        t_mcts[1] += len(envs)
        return out

    t0 = time.perf_counter()
    r = evaluate_flow(net, timed, device, n_games=args.games, num_envs=args.envs,
                      team_pool=POOL, seed=777, fog_species=net.fog_species)
    wall = time.perf_counter() - t0

    # Policy per-action cost at the same batch size, for the matched-time report.
    from ppo.flow_env import FlowEnvVec
    env = FlowEnvVec(args.envs, seed=3, team_pool=POOL)
    obs, ids, mask, _ = env._sides()[0]
    for _ in range(3):
        from ppo.flow_eval import _policy_actions
        _policy_actions(net, obs, ids, mask, device)
    t1 = time.perf_counter()
    for _ in range(20):
        _policy_actions(net, obs, ids, mask, device)
    per_policy_action = (time.perf_counter() - t1) / (20 * args.envs)

    out = {**{k: r[k] for k in ("win_rate", "ci_low", "ci_high", "wins", "losses", "draws", "games")},
           "opponent": f"mcts@{args.time_ms}ms",
           "mcts_ms_per_action": round(t_mcts[0] / max(1, t_mcts[1]) * 1000, 2),
           "policy_ms_per_action": round(per_policy_action * 1000, 4),
           "wall_s": round(wall, 1), "ckpt_step": step}
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
