"""E2: value-guided 1-ply expectiminimax at inference — zero training, pure eval-time wrapper.

For every Turn request, build the 13×13 joint-action payoff matrix from the value head evaluated
on engine-cloned successors (terminals use the true ±1 outcome), solve the matrix game with
regret matching, and play the resulting mixed strategy. Single-sided requests (replacements,
pivot landings) fall back to the raw policy — there is no simultaneous game to solve there.

    .venv/bin/python -m probes.value_search runs/scale2/ckpt_X.pt --games 300 --samples 2 \
        --opponent heuristic

Reports win-rate AND measured ms/action for both arms (the plan's matched-time rule).
"""

from __future__ import annotations

import argparse
import json
import time

import numpy as np
import torch

from ppo.flow_eval import (_policy_actions, evaluate_flow, make_mcts_opponent,
                           make_scripted_heuristic)
from probes.mcts_calib import POOL, load_ckpt

N = 13


def solve_matrix_game(Q: np.ndarray, rows: np.ndarray, cols: np.ndarray,
                      iters: int = 400) -> np.ndarray:
    """Regret matching for the zero-sum matrix game Q[rows, cols] (row = maximizer).

    Returns the row player's average mixed strategy over `rows`. 400 iterations on a ≤13×13
    matrix is sub-millisecond and well within the noise floor of the sampled leaves.
    """
    A = Q[np.ix_(rows, cols)]
    nr, nc = A.shape
    if nr == 1:
        return np.array([1.0])
    r_reg = np.zeros(nr)
    c_reg = np.zeros(nc)
    r_avg = np.zeros(nr)
    r_s = np.full(nr, 1.0 / nr)
    c_s = np.full(nc, 1.0 / nc)
    for _ in range(iters):
        r_avg += r_s
        u_r = A @ c_s            # row payoffs vs current column strategy
        u_c = -(r_s @ A)         # column payoffs (zero-sum)
        r_reg += u_r - r_s @ u_r
        c_reg += u_c - c_s @ u_c
        rp = np.maximum(r_reg, 0.0)
        cp = np.maximum(c_reg, 0.0)
        r_s = rp / rp.sum() if rp.sum() > 0 else np.full(nr, 1.0 / nr)
        c_s = cp / cp.sum() if cp.sum() > 0 else np.full(nc, 1.0 / nc)
    return r_avg / r_avg.sum()


def _leaf_values(net, device, obs_np, ids_np) -> np.ndarray:
    """Leaf evaluation: the outcome head when the checkpoint has one (W7), else the critic."""
    if len(obs_np) == 0:
        return np.zeros(0, dtype=np.float32)
    with torch.no_grad():
        _, v = net.forward(torch.as_tensor(obs_np, device=device), None,
                           obs_ids=torch.as_tensor(ids_np, device=device))
        w = net.outcome_pred if getattr(net, "outcome_pred", None) is not None else v
    return w.float().cpu().numpy()


def _solve_child(q_flat: np.ndarray, cnt_flat: np.ndarray) -> float:
    """Value (to the row player) of a successor Turn state's 13×13 matrix game."""
    q = np.where(cnt_flat > 0, q_flat / np.maximum(cnt_flat, 1), 0.0).reshape(N, N)
    rows = np.flatnonzero((cnt_flat.reshape(N, N) > 0).any(axis=1))
    cols = np.flatnonzero((cnt_flat.reshape(N, N) > 0).any(axis=0))
    if rows.size == 0 or cols.size == 0:
        return 0.0
    strat = solve_matrix_game(q, rows, cols)
    A = q[np.ix_(rows, cols)]
    # Game value against the column player's best response to our mixed strategy.
    return float((strat @ A).min())


def make_subgame_search_agent(net, device, topk: int = 4, n_samples: int = 1, seed: int = 0,
                              timing: dict | None = None):
    """Depth-2 search with equilibrium backups (EXPLORATION_PLAN W7, the ReBeL concept scoped).

    Root Turn state: prune both sides' actions to the policy's top-`topk` legal choices, advance
    every joint pair, and where the successor is itself a Turn state, build ITS 13×13 matrix
    from leaf values and back up the SOLVED game value — so every simultaneous node in the
    2-ply tree is resolved by (approximate) Nash, not by assuming the opponent plays π. Play
    the root matrix's mixed strategy. Terminals use true outcomes; single-sided successors are
    leaf-evaluated.
    """
    counter = [seed]

    def fn(vec, envs, mask, rng):
        t0 = time.perf_counter()
        out = np.zeros(len(envs), dtype=np.int64)
        acting = {s: np.asarray(vec.acting_all(s), dtype=bool) for s in (0, 1)}
        turn_rows = [i for i, (e, s) in enumerate(envs) if acting[s][e] and acting[1 - s][e]]
        other_rows = [i for i in range(len(envs)) if i not in turn_rows]

        obs_by = {s: np.asarray(vec.observe_all(s), dtype=np.float32) for s in (0, 1)}
        ids_by = {s: np.asarray(vec.observe_ids_all(s), dtype=np.int64) for s in (0, 1)}
        mask_by = {s: np.asarray(vec.legal_all(s), dtype=bool) for s in (0, 1)}

        if other_rows:
            o = np.stack([obs_by[envs[i][1]][envs[i][0]] for i in other_rows])
            d = np.stack([ids_by[envs[i][1]][envs[i][0]] for i in other_rows])
            m = np.stack([mask[i] for i in other_rows])
            out[np.array(other_rows)] = _policy_actions(net, o, d, m, device)

        # Policy priors for pruning, both perspectives, one forward each.
        def topk_actions(obs_np, ids_np, mask_np):
            with torch.no_grad():
                logits, _ = net.forward(torch.as_tensor(obs_np, device=device),
                                        torch.as_tensor(mask_np, device=device),
                                        obs_ids=torch.as_tensor(ids_np, device=device))
            lg = logits.cpu().numpy()
            picks = []
            for r in range(len(lg)):
                legal = np.flatnonzero(mask_np[r])
                order = legal[np.argsort(-lg[r][legal])]
                picks.append(order[:topk])
            return picks

        pending = []  # (row, pair_idx, sample, kind, done, outc, valid, live_slice)
        all_obs, all_ids = [], []
        my_acts, opp_acts = {}, {}
        fallback_rows = []
        for i in list(turn_rows):
            e, s = envs[i]
            try:
                my = topk_actions(obs_by[s][e:e + 1], ids_by[s][e:e + 1], mask_by[s][e:e + 1])[0]
                op = topk_actions(obs_by[1 - s][e:e + 1], ids_by[1 - s][e:e + 1],
                                  mask_by[1 - s][e:e + 1])[0]
                my_acts[i], opp_acts[i] = my, op
                for pi, a1 in enumerate(my):
                    for pj, a2 in enumerate(op):
                        for k in range(n_samples):
                            counter[0] += 1
                            kind, obs, ids, done, outc, valid = vec.lookahead_pair_obs(
                                e, s, counter[0], int(a1), int(a2))
                            obs = np.asarray(obs); ids = np.asarray(ids)
                            done = np.asarray(done); outc = np.asarray(outc)
                            valid = np.asarray(valid)
                            live = valid & ~done if kind == 2 else (np.array([kind == 1]))
                            start = sum(len(x) for x in all_obs)
                            all_obs.append(obs[live]); all_ids.append(ids[live])
                            pending.append((i, pi, pj, kind, done, outc, valid, live,
                                            start, int(live.sum())))
            except ValueError:
                turn_rows.remove(i)
                fallback_rows.append(i)
        if fallback_rows:
            o = np.stack([obs_by[envs[i][1]][envs[i][0]] for i in fallback_rows])
            d = np.stack([ids_by[envs[i][1]][envs[i][0]] for i in fallback_rows])
            m = np.stack([mask[i] for i in fallback_rows])
            out[np.array(fallback_rows)] = _policy_actions(net, o, d, m, device)

        values = _leaf_values(net, device,
                              np.concatenate(all_obs) if all_obs else np.zeros((0, 1)),
                              np.concatenate(all_ids) if all_ids else np.zeros((0, 1)))

        # Back up: per (env, root pair) accumulate the pair's value over samples.
        acc = {}
        for (i, pi, pj, kind, done, outc, valid, live, start, nlive) in pending:
            if kind == 0:
                v = float(outc[0])
            elif kind == 1:
                v = float(values[start]) if nlive else 0.0
            else:
                q = np.where(done, outc, 0.0).astype(np.float64)
                q[live] = values[start:start + nlive]
                v = _solve_child(np.where(valid, q, 0.0), valid.astype(np.float64))
            key = (i, pi, pj)
            tot, n = acc.get(key, (0.0, 0))
            acc[key] = (tot + v, n + 1)

        for i in turn_rows:
            my, op = my_acts[i], opp_acts[i]
            Q = np.zeros((len(my), len(op)))
            for pi in range(len(my)):
                for pj in range(len(op)):
                    tot, n = acc.get((i, pi, pj), (0.0, 0))
                    Q[pi, pj] = tot / max(1, n)
            strat = solve_matrix_game(Q, np.arange(len(my)), np.arange(len(op)))
            out[i] = int(my[int(rng.choice(len(my), p=strat))])
        if timing is not None:
            timing["s"] = timing.get("s", 0.0) + (time.perf_counter() - t0)
            timing["n"] = timing.get("n", 0) + len(envs)
        return out

    return fn


def make_value_search_agent(net, device, n_samples: int = 2, seed: int = 0,
                            timing: dict | None = None, greedy_pick: bool = False):
    """A `(vec, envs, mask_rows, rng) -> actions` agent: 1-ply matrix-game search over V."""
    counter = [seed]

    def fn(vec, envs, mask, rng):
        t0 = time.perf_counter()
        out = np.zeros(len(envs), dtype=np.int64)
        acting = {s: np.asarray(vec.acting_all(s), dtype=bool) for s in (0, 1)}
        # A Turn request = both sides acting; only there is there a matrix game to solve.
        turn_rows = [i for i, (e, s) in enumerate(envs)
                     if acting[s][e] and acting[1 - s][e]]
        other_rows = [i for i in range(len(envs)) if i not in turn_rows]

        if other_rows:  # single-sided requests: raw policy
            obs_by = {s: np.asarray(vec.observe_all(s), dtype=np.float32)
                      for s in {envs[i][1] for i in other_rows}}
            ids_by = {s: np.asarray(vec.observe_ids_all(s), dtype=np.int64)
                      for s in obs_by}
            o = np.stack([obs_by[envs[i][1]][envs[i][0]] for i in other_rows])
            d = np.stack([ids_by[envs[i][1]][envs[i][0]] for i in other_rows])
            m = np.stack([mask[i] for i in other_rows])
            out[np.array(other_rows)] = _policy_actions(net, o, d, m, device)

        if turn_rows:
            # Gather every sample of every pair of every env, one big value forward.
            all_obs, all_ids, metas = [], [], []
            fallback_rows = []
            for i in list(turn_rows):
                e, s = envs[i]
                try:
                    for k in range(n_samples):
                        counter[0] += 1
                        obs, ids, done, outc, valid = vec.lookahead_obs(e, s, counter[0])
                        obs = np.asarray(obs); ids = np.asarray(ids)
                        done = np.asarray(done); outc = np.asarray(outc)
                        valid = np.asarray(valid)
                        live = valid & ~done
                        all_obs.append(obs[live]); all_ids.append(ids[live])
                        metas.append((i, done, outc, valid, live))
                except ValueError:
                    # Both sides acting but not a Turn (double faint-replacement) — no
                    # simultaneous move game to solve; raw policy handles it.
                    turn_rows.remove(i)
                    fallback_rows.append(i)
            if fallback_rows:
                obs_by = {s: np.asarray(vec.observe_all(s), dtype=np.float32)
                          for s in {envs[i][1] for i in fallback_rows}}
                ids_by = {s: np.asarray(vec.observe_ids_all(s), dtype=np.int64) for s in obs_by}
                o = np.stack([obs_by[envs[i][1]][envs[i][0]] for i in fallback_rows])
                d = np.stack([ids_by[envs[i][1]][envs[i][0]] for i in fallback_rows])
                m = np.stack([mask[i] for i in fallback_rows])
                out[np.array(fallback_rows)] = _policy_actions(net, o, d, m, device)
            values = _leaf_values(net, device,
                                  np.concatenate(all_obs) if all_obs else np.zeros((0, 1)),
                                  np.concatenate(all_ids) if all_ids else np.zeros((0, 1)))
            # Scatter back into per-env Q matrices, then solve.
            Q = {i: np.zeros(N * N) for i in turn_rows}
            cnt = {i: np.zeros(N * N) for i in turn_rows}
            off = 0
            for i, done, outc, valid, live in metas:
                q = np.where(done, outc, 0.0).astype(np.float64)
                nlive = int(live.sum())
                q[live] = values[off:off + nlive]
                off += nlive
                Q[i] += np.where(valid, q, 0.0)
                cnt[i] += valid
            for i in turn_rows:
                e, s = envs[i]
                with np.errstate(invalid="ignore"):
                    q = np.where(cnt[i] > 0, Q[i] / np.maximum(cnt[i], 1), 0.0).reshape(N, N)
                rows = np.flatnonzero(mask[i])
                # Column legality: any column with a sampled entry in ANY legal row.
                cols = np.flatnonzero((cnt[i].reshape(N, N)[rows] > 0).any(axis=0)) if rows.size else np.array([], dtype=np.int64)
                if rows.size == 0:
                    out[i] = 0
                    continue
                if cols.size == 0:
                    out[i] = int(rows[0])
                    continue
                strat = solve_matrix_game(q, rows, cols)
                pick = int(np.argmax(strat)) if greedy_pick else int(rng.choice(len(rows), p=strat))
                out[i] = int(rows[pick])
        if timing is not None:
            timing["s"] = timing.get("s", 0.0) + (time.perf_counter() - t0)
            timing["n"] = timing.get("n", 0) + len(envs)
        return out

    return fn


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ckpt")
    ap.add_argument("--games", type=int, default=300)
    ap.add_argument("--envs", type=int, default=64)
    ap.add_argument("--samples", type=int, default=2)
    ap.add_argument("--depth", type=int, default=1, choices=[1, 2])
    ap.add_argument("--topk", type=int, default=4, help="depth 2: policy-prior action pruning")
    ap.add_argument("--opponent", type=str, default="heuristic",
                    choices=["heuristic", "raw", "random", "mcts"])
    ap.add_argument("--mcts-ms", type=int, default=100)
    ap.add_argument("--greedy-pick", action="store_true",
                    help="argmax of the mixed strategy instead of sampling it")
    args = ap.parse_args()

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    net, step = load_ckpt(args.ckpt, device)
    timing: dict = {}
    if args.depth == 2:
        agent = make_subgame_search_agent(net, device, topk=args.topk,
                                          n_samples=args.samples, timing=timing)
    else:
        agent = make_value_search_agent(net, device, n_samples=args.samples, timing=timing,
                                        greedy_pick=args.greedy_pick)
    opp = {"heuristic": lambda: make_scripted_heuristic(),
           "raw": lambda: net,
           "random": lambda: "random",
           "mcts": lambda: make_mcts_opponent(args.mcts_ms)}[args.opponent]()

    t0 = time.perf_counter()
    r = evaluate_flow(agent, opp, device, n_games=args.games, num_envs=args.envs,
                      team_pool=POOL, seed=20_2607)
    print(json.dumps({
        "ckpt": args.ckpt, "step": step, "arm": f"value-search d{args.depth} x{args.samples}" + (f" topk{args.topk}" if args.depth == 2 else ""),
        "opponent": args.opponent,
        **{k: r[k] for k in ("win_rate", "ci_low", "ci_high", "wins", "losses", "draws")},
        "search_ms_per_action": round(timing.get("s", 0.0) / max(1, timing.get("n", 1)) * 1000, 3),
        "wall_s": round(time.perf_counter() - t0, 1)}, indent=2))


if __name__ == "__main__":
    main()
