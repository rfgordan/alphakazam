"""Action-exactness gate for the Rust port of `HeuristicBaseline`.

The scripted heuristic is both a training opponent (league) and the eval ladder's reference
baseline, so the Rust port in `pybridge::heuristic_action_of` must pick the IDENTICAL action to
`baselines.HeuristicBaseline._action_for` in every reachable state — otherwise the agent trains
against a subtly different opponent than it is measured against, and the win-rate curve stops
being comparable to prior runs.

Drives random battles through `FlowVec` and, at every acting request on both sides, compares the
two implementations. Any mismatch prints the full state for diagnosis and fails.

    .venv/bin/python -m probes.heuristic_parity            # ~20k decisions, a few seconds
    .venv/bin/python -m probes.heuristic_parity --steps 2000 --envs 64
"""

from __future__ import annotations

import argparse
import json
import sys

import numpy as np

import showdown_engine as se

from ppo.baselines import HeuristicBaseline
from ppo.flow_eval import _TYPE_CHART, _adapt_side

POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"


def python_action(heur, vec, e, side, mask) -> int:
    """`_action_for` with the same exception→-1 convention the Rust port uses."""
    try:
        st = json.loads(vec.state_json(e))
        me, foe = _adapt_side(st["sides"][side]), _adapt_side(st["sides"][1 - side])
        a = heur._action_for(_TYPE_CHART, me, foe, mask)
        return -1 if a is None else int(a)
    except Exception:
        return -1


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--envs", type=int, default=128)
    ap.add_argument("--steps", type=int, default=200)
    ap.add_argument("--seed", type=int, default=99)
    args = ap.parse_args()

    vec = se.FlowVec(args.envs, seed=args.seed, team_pool=POOL)
    heur = HeuristicBaseline()
    rng = np.random.default_rng(args.seed)

    checked = mismatches = 0
    for step in range(args.steps):
        acts = {}
        for side in (0, 1):
            mask = np.asarray(vec.legal_all(side), dtype=bool)
            acting = np.asarray(vec.acting_all(side), dtype=bool)
            rust = np.asarray(vec.heuristic_actions_all(
                np.full(args.envs, side, dtype=np.int64)), dtype=np.int64)
            for e in np.flatnonzero(acting):
                py = python_action(heur, vec, int(e), side, mask[e])
                if py != rust[e]:
                    mismatches += 1
                    print(f"MISMATCH step {step} env {e} side {side}: "
                          f"python={py} rust={rust[e]} mask={mask[e].astype(int).tolist()}")
                    print(json.dumps(json.loads(vec.state_json(int(e)))["sides"][side])[:600])
                checked += 1
            # Drive the battle forward with the heuristic itself where acting (so parity is
            # checked along heuristic-shaped trajectories, not just random ones), random filler
            # elsewhere.
            legal_fill = np.where(mask.any(axis=1)[:, None], mask, True)
            filler = (rng.random(legal_fill.shape) * legal_fill).argmax(axis=1)
            acts[side] = np.where((rust >= 0) & acting, rust, filler)
        vec.step_all(acts[0], acts[1], True)

    print(f"checked {checked} acting decisions: {mismatches} mismatches")
    if mismatches:
        sys.exit(1)
    if checked < 1000:
        print("WARNING: fewer than 1000 decisions checked — raise --steps/--envs")
        sys.exit(2)


if __name__ == "__main__":
    main()
