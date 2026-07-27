"""E1 data generation: heuristic-vs-heuristic decision points from the Rust engine.

Collects (obs, ids, mask, action, return) for every ACTING decision of both sides while the
scripted heuristic plays itself, with the value target as the γ-discounted terminal outcome
from that side's perspective. At Rust-heuristic speed this produces ~2M decisions in minutes.

    .venv/bin/python -m probes.bc_dataset --out runs/probes/bc-heur.npz --n 2000000
"""

from __future__ import annotations

import argparse
import time

import numpy as np

import showdown_engine as se

POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--n", type=int, default=2_000_000)
    ap.add_argument("--envs", type=int, default=4096)
    ap.add_argument("--gamma", type=float, default=0.995)
    ap.add_argument("--seed", type=int, default=42)
    args = ap.parse_args()

    vec = se.FlowVec(args.envs, seed=args.seed, team_pool=POOL)
    rng = np.random.default_rng(args.seed)
    obs_dim, id_dim = vec.obs_dim, vec.id_dim

    O, I, M, A, R = [], [], [], [], []
    # Per env & side: indices into the row buffers for the current episode, back-filled with the
    # discounted outcome when the episode ends.
    pending: list[list[list[int]]] = [[[], []] for _ in range(args.envs)]
    n_done = 0
    t0 = time.time()
    while n_done < args.n:
        acts = {}
        for side in (0, 1):
            mask = np.asarray(vec.legal_all(side), dtype=bool)
            acting = np.asarray(vec.acting_all(side), dtype=bool)
            a = np.asarray(vec.heuristic_actions_all(
                np.full(args.envs, side, dtype=np.int64)), dtype=np.int64)
            # -1 = no heuristic opinion -> random legal (mirrors training-time fallback)
            for e in np.flatnonzero(acting & (a < 0)):
                legal = np.flatnonzero(mask[e])
                a[e] = int(rng.choice(legal)) if legal.size else 0
            obs = np.asarray(vec.observe_all(side), dtype=np.float32)
            ids = np.asarray(vec.observe_ids_all(side), dtype=np.int64)
            for e in np.flatnonzero(acting):
                pending[e][side].append(len(A))
                O.append(obs[e]); I.append(ids[e]); M.append(mask[e]); A.append(int(a[e]))
                R.append(0.0)
            filler = (rng.random(mask.shape) * np.where(mask.any(1)[:, None], mask, True)).argmax(1)
            acts[side] = np.where(a >= 0, a, filler)
        done_np, win_np = vec.step_all(acts[0], acts[1], True)
        done = np.asarray(done_np, dtype=bool)
        winner = np.asarray(win_np, dtype=np.int64)
        for e in np.flatnonzero(done):
            for side in (0, 1):
                rows = pending[e][side]
                z = 0.0 if winner[e] < 0 else (1.0 if winner[e] == side else -1.0)
                for k, row in enumerate(reversed(rows)):
                    R[row] = z * (args.gamma ** k)
                n_done += len(rows)
                rows.clear()
        if int(time.time() - t0) % 15 == 0 and done.any():
            print(f"\r{n_done:,}/{args.n:,} decisions  {n_done/max(1e-9,time.time()-t0):,.0f}/s",
                  end="", flush=True)

    # Episodes still in flight have no outcome; drop their rows (indices in any pending list).
    drop = {r for env_p in pending for side_p in env_p for r in side_p}
    keep = np.array([i for i in range(len(A)) if i not in drop], dtype=np.int64)
    print(f"\nkeeping {len(keep):,} of {len(A):,} rows ({len(drop):,} from unfinished episodes)")
    np.savez_compressed(
        args.out,
        obs=np.stack(O)[keep], ids=np.stack(I)[keep], mask=np.stack(M)[keep],
        action=np.array(A, dtype=np.int64)[keep], ret=np.array(R, dtype=np.float32)[keep],
        gamma=args.gamma, obs_dim=obs_dim, id_dim=id_dim)
    print(f"wrote {args.out} in {time.time()-t0:,.0f}s")


if __name__ == "__main__":
    main()
