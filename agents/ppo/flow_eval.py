"""Absolute-progress evaluation for the decision-point env — the curve self-play win-rate can't give.

`win_rate(vs snapshot)` is a *relative* measure against a moving target: in frozen-snapshot
self-play it equilibrates toward 0.5 whether or not the agent is improving, so it cannot tell a
plateau from healthy co-adaptation. `RESEARCH_PLAN.md` is explicit that only powered evals against
fixed opponents count. These are those opponents.

Three, in the order a policy should clear them:

  random     uniform over legal actions — the floor
  anchor     the run's own random-init policy, frozen at update 0 — isolates "did we learn
             anything at all" from "is the ladder well-calibrated"
  heuristic  `baselines.HeuristicBaseline`, a port of poke-env's SimpleHeuristicsPlayer, read off
             `FlowVec.state_json`. The informative one: it is the same opponent the poke-env
             agent was certified against (night4 = 0.746), so it is the only number here that is
             comparable to prior work.

Side assignment alternates by env index so the reported win-rate averages over both perspectives.
"""

from __future__ import annotations

import json
import math

import numpy as np
import torch

import showdown_engine as se

from .flow_env import FlowEnvVec

N_ACTIONS = 13

# `type_effectiveness` is a pure lookup over the engine's type chart, so one throwaway `Battle`
# serves every call — building one per env per step would dominate the heuristic's cost.
_TYPE_CHART = se.Battle(seed=0).type_effectiveness

# Populated by `standard_baselines`; lets the trainer log how often the scripted
# heuristic failed to produce a move (should be 0 — anything else invalidates it).
HEURISTIC_STATS: dict = {}


@torch.no_grad()
def _policy_actions(net, obs, ids, mask, device) -> np.ndarray:
    logits, _ = net.forward(
        torch.as_tensor(obs, device=device),
        torch.as_tensor(mask, device=device),
        obs_ids=torch.as_tensor(ids, device=device))
    return logits.argmax(dim=-1).cpu().numpy().astype(np.int64)


def _adapt_side(sd: dict) -> dict:
    """Shape a `state_json` side the way `HeuristicBaseline._action_for` expects it.

    It reads `active["boosts"]`, but the engine stores boosts on the SIDE (only the active mon can
    have them), so the raw dict raises `KeyError: 'boosts'` on every call. `baselines.py` swallows
    that and falls back to `legal[0]` — which is why the "heuristic" opponent there has in fact
    been playing "always move 1". Inject the side's boosts into the active mon instead.
    """
    active_idx = sd["active_index"]
    mons = list(sd["pokemon"])
    active = dict(mons[active_idx])
    active["boosts"] = sd["boosts"]
    mons[active_idx] = active
    return {**sd, "pokemon": mons}


def _heuristic_actions(heur, vec, envs, mask, rng, stats: dict | None = None) -> np.ndarray:
    """Scripted heuristic per env, from the engine's true state.

    The heuristic is written for the 9-action space (4 moves + 5 switches); those indices mean the
    same thing here, and the tera actions (9..12) simply never get chosen — which matches
    SimpleHeuristicsPlayer, who does not terastallize either.

    Fallbacks are COUNTED, not swallowed: a baseline that silently degrades to random makes the
    agent look strong and is worse than no baseline at all.
    """
    out = np.zeros(len(envs), dtype=np.int64)
    for row, (e, side) in enumerate(envs):
        try:
            st = json.loads(vec.state_json(e))
            me, foe = _adapt_side(st["sides"][side]), _adapt_side(st["sides"][1 - side])
            a = heur._action_for(_TYPE_CHART, me, foe, mask[row])
        except Exception as exc:
            if stats is not None:
                stats["fallbacks"] = stats.get("fallbacks", 0) + 1
                stats.setdefault("last_error", f"{type(exc).__name__}: {exc}")
            a = -1
        if a is None or a < 0 or a >= N_ACTIONS or not mask[row][a]:
            if stats is not None and a is not None and a >= 0:
                stats["illegal"] = stats.get("illegal", 0) + 1
            legal = np.flatnonzero(mask[row])
            a = int(rng.choice(legal)) if legal.size else 0
        out[row] = a
        if stats is not None:
            stats["calls"] = stats.get("calls", 0) + 1
    return out


def _heuristic_actions_rust(vec, envs, mask, rng, stats: dict | None = None) -> np.ndarray:
    """The Rust port of `HeuristicBaseline`, batched: one bridge call for the whole env block.

    The Python path costs ~400µs/env (a full `state_json` round-trip per env per step) and was
    measured at 83% of trainer wall time once the heuristic joined the league; the Rust port is
    action-exact (enforced by `probes/heuristic_parity.py`) and ~three orders of magnitude
    cheaper. -1 rows are "no opinion" (the states where the Python version raises or has no
    legal pick) and fall back to a random legal action, counted like the Python path counts them.
    """
    sides = np.zeros(vec.num_envs, dtype=np.int64)
    rows = np.array([e for e, _ in envs], dtype=np.int64)
    sides[rows] = [s for _, s in envs]
    acts = np.asarray(vec.heuristic_actions_all(sides), dtype=np.int64)[rows]
    # Callers hand the heuristic their FULL env block, including rows whose side is not acting
    # at this request (the engine discards those actions). The Rust port returns -1 there; that
    # is routine, not a degraded baseline — only an acting row with no heuristic action counts
    # toward the "baseline NOT trustworthy" alarm.
    acting = {s: np.asarray(vec.acting_all(s), dtype=bool) for s in {s for _, s in envs}}
    out = np.empty(len(envs), dtype=np.int64)
    n_acting = 0
    for row, ((e, s), a) in enumerate(zip(envs, acts)):
        is_acting = bool(acting[s][e])
        n_acting += is_acting
        if a < 0 or not mask[row][a]:
            if stats is not None and is_acting:
                stats["fallbacks"] = stats.get("fallbacks", 0) + 1
                stats.setdefault("last_error", "rust port returned no action on an acting row")
            legal = np.flatnonzero(mask[row])
            a = int(rng.choice(legal)) if legal.size else 0
        out[row] = a
    if stats is not None:
        stats["calls"] = stats.get("calls", 0) + n_acting
    return out


def make_mcts_opponent(time_ms: int = 100):
    """pmariglia's perfect-information MCTS as a flow-eval opponent (EXPLORATION_PLAN P0b).

    Sequential and heavy (~time_ms per acting env per step) — eval windows only, small
    num_envs. Comparisons against it must respect the plan's matched-time rule: report
    per-action wall clock for BOTH arms next to any win-rate.
    """
    from .baselines import MctsBaseline
    m = MctsBaseline(time_ms)

    class _Shim:  # the minimal `Battle` surface MctsBaseline reads
        __slots__ = ("vec", "e")

        def __init__(self, vec, e):
            self.vec, self.e = vec, e

        def state_json(self):
            return self.vec.state_json(self.e)

    def fn(vec, envs, mask, rng):
        # Don't spend time_ms searching rows whose side is not acting — the engine discards
        # those actions anyway (same reasoning as `_heuristic_actions_rust`).
        acting = {s: np.asarray(vec.acting_all(s), dtype=bool) for s in {s for _, s in envs}}
        rows = [i for i, (e, s) in enumerate(envs) if acting[s][e]]
        out = np.zeros(len(envs), dtype=np.int64)
        if rows:
            battles = [_Shim(vec, envs[i][0]) for i in rows]
            sides = [envs[i][1] for i in rows]
            out[rows] = m.actions(None, None, mask[rows], battles=battles, sides=sides)
        return out

    return fn


def make_scripted_heuristic(stats: dict | None = None):
    """A `(vec, envs, mask_rows, rng) -> actions` callable playing `HeuristicBaseline`.

    Shared by the eval ladder and the training league (`OpponentSlots.scripted`) so the opponent
    the agent trains against is the SAME implementation it is evaluated against. Uses the Rust
    port when the bridge has it (anything built after the heuristic joined the league), the
    Python original otherwise.
    """
    if hasattr(se.FlowVec, "heuristic_actions_all"):
        return lambda vec, envs, mask, rng: _heuristic_actions_rust(vec, envs, mask, rng, stats)
    from .baselines import HeuristicBaseline
    heur = HeuristicBaseline()
    return lambda vec, envs, mask, rng: _heuristic_actions(heur, vec, envs, mask, rng, stats)


def evaluate_flow(model, opponent, device, n_games: int = 300, num_envs: int = 128,
                  team_pool: str | None = None, seed: int = 12345, max_requests: int = 600,
                  max_steps: int = 20_000) -> dict:
    """Play `model` (greedy) against `opponent` until `n_games` finish. Returns win-rate + Wilson CI.

    `opponent` is either the string "random", or a callable
    `(obs, ids, mask, rows) -> actions`, or a torch net played greedily.
    """
    env = FlowEnvVec(num_envs, seed=seed, team_pool=team_pool, max_requests=max_requests)
    rng = np.random.default_rng(seed ^ 0x5EED)
    # Learner on RED for even envs, BLUE for odd — the reported rate averages both perspectives.
    learner_side = (np.arange(num_envs) % 2).astype(np.int64)

    wins = losses = draws = 0
    turns_sum = 0
    steps_at_start = np.zeros(num_envs, dtype=np.int64)
    step = 0
    while wins + losses + draws < n_games and step < max_steps:
        red = np.zeros(num_envs, dtype=np.int64)
        blue = np.zeros(num_envs, dtype=np.int64)
        for side in (0, 1):
            obs, ids, mask, _ = env._sides()[side]
            is_learner = learner_side == side
            act = np.zeros(num_envs, dtype=np.int64)
            if is_learner.any():
                # The "model" arm may itself be a callable agent (e.g. the E2 value-search
                # wrapper) with the same signature scripted opponents use.
                if callable(model) and not isinstance(model, torch.nn.Module):
                    l_envs = [(int(e), side) for e in np.flatnonzero(is_learner)]
                    act[is_learner] = model(env.vec, l_envs, mask[is_learner], rng)
                else:
                    act[is_learner] = _policy_actions(model, obs[is_learner], ids[is_learner],
                                                      mask[is_learner], device)
            opp_rows = ~is_learner
            if opp_rows.any():
                if opponent == "random":
                    m = mask[opp_rows]
                    act[opp_rows] = (rng.random(m.shape) * m).argmax(axis=1)
                elif callable(opponent) and not isinstance(opponent, torch.nn.Module):
                    envs = [(int(e), side) for e in np.flatnonzero(opp_rows)]
                    act[opp_rows] = opponent(env.vec, envs, mask[opp_rows], rng)
                else:
                    act[opp_rows] = _policy_actions(opponent, obs[opp_rows], ids[opp_rows],
                                                    mask[opp_rows], device)
            (red if side == 0 else blue)[:] = act
        done_np, win_np = env.vec.step_all(red, blue, True)
        env._cache = None
        step += 1
        done = np.asarray(done_np, dtype=bool)
        winner = np.asarray(win_np, dtype=np.int64)
        for e in np.flatnonzero(done):
            if winner[e] == learner_side[e]:
                wins += 1
            elif winner[e] < 0:
                draws += 1
            else:
                losses += 1
            turns_sum += step - steps_at_start[e]
            steps_at_start[e] = step
            learner_side[e] = rng.integers(0, 2)

    n = max(1, wins + losses + draws)
    p = wins / n
    # Wilson interval — the normal approximation misbehaves near 0 and 1, which is exactly where
    # the floor baselines sit.
    z = 1.96
    denom = 1 + z * z / n
    centre = (p + z * z / (2 * n)) / denom
    half = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / denom
    return {"win_rate": p, "ci_low": centre - half, "ci_high": centre + half,
            "wins": wins, "losses": losses, "draws": draws, "games": n,
            # Both names on purpose: `avg_decisions` is accurate for this env (a decision is not a
            # turn — replacements and pivot landings are separate requests), while `avg_turns` is
            # the key `RunLogger.eval` reads, and it is shared with the poke-env trainers.
            "avg_decisions": turns_sum / n, "avg_turns": turns_sum / n}


def standard_baselines(anchor_net, device, team_pool: str | None):
    """The eval ladder: (name, opponent) pairs, cheapest first."""
    out: list[tuple[str, object]] = [("random", "random")]
    if anchor_net is not None:
        out.append(("anchor-init", anchor_net))
    try:
        stats: dict = {}
        out.append(("heuristic", make_scripted_heuristic(stats)))
        HEURISTIC_STATS.clear()
        HEURISTIC_STATS.update({"ref": stats})
    except Exception as e:  # never let an eval import take down training
        print(f"[eval] heuristic baseline unavailable ({type(e).__name__}: {e})")
    return out
