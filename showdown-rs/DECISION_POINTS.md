# Design: Decision-Point Engine (P1.2 + P1.4-fast-path)

Status: 2026-07-11 — steps 1–3 LANDED (`2f2a144` sampled executor, `047f319` request flow);
step 4 (FlowVec bridge) and step 5 (gate hardening) in flight via delegated agents.
Implementation deviations from the draft: the executor mode is `Exec::{Enumerate,Sample}` with
seam pruning (exact ancestral sampling) rather than a per-fork `Forker` trait — same guarantee,
certified by `tests/sampled_distribution.rs`; pivot pause is signaled by a no-op
`Instruction::PivotPending` under `Pivot::Pause` instead of a request-state return; pivot-free
turns run the whole-turn resolver unchanged (composition risk confined to pivot turns, see
`tests/request_flow.rs`). Known mask gap (deliberate, documented): trapping (Arena Trap/Shadow
Tag/partial-trap) is not modeled in switch legality yet.

Goal: train on the *real* rules (faint replacements, pivots, tera) at 10×+ current throughput.

## Why (two problems, one refactor)

1. **Correctness:** the bridge MDP deviates from PS — a faint replacement consumes a whole turn
   (opponent gets a free hit), U-turn never pivots (bridge passes pivot=None), tera is absent.
   Policies trained on these rules learn exploits that don't transfer (sim2real fail) —
   disqualifying for anything past P1.
2. **Throughput:** `generate_instructions` enumerates every stochastic branch (16 damage rolls ×
   crit × secondary × …) so the bridge can sample one. Measured: this is ~97% of step cost
   (bench_steps: 2.0k turns/s single-thread; encode is 2.6%). A **sampled executor** that rolls
   at each stochastic point and follows ONE path (like PS itself) removes the enumeration
   entirely. PS's request model is the correct shape for both.

## Core abstraction: the decision point

The engine advances a battle as a state machine that *pauses whenever any side must choose*:

```
enum Request {
    Turn      { side_mask },   // normal simultaneous choice (move/switch/tera)
    Replace   { side, forced } // faint replacement (mid-turn or end-of-turn), pivot landing
}
step_sampled(state, rng, choices) -> (events, Request | Terminal)
```

- `choices` answers the *pending* request only; the executor runs until the next request.
- Chained requests (multi-faint cascades, pivot-then-faint) surface as successive `Replace`s —
  exactly PS's request stream, which cosim already records/replays as "units".
- The executor is *sampled*: every stochastic fork draws from `rng` and commits. No branch
  vectors, no enumeration, no `Copy` of alternatives.

## Two executors, one mechanics body

The mechanics functions (damage, effects, EOT) stay single-source. They already thread outcomes
through `Branch`/`push()`; the refactor parameterizes the *outcome policy*:

- `Enumerate` — current behavior: expand all forks with probabilities (verification, and later
  search). Unchanged semantics; cosim keeps its exactness guarantees.
- `Sample(rng)` — pick one fork per stochastic point, weighted identically. Training/eval path.

Implementation: a `Forker` trait with the two impls, threaded where branches currently multiply
(damage roll, crit, hit/miss, secondary, speed tie, multi-hit counts, duration draws, drags).
The mutation suite + exact-distribution oracle then certify that `Sample` follows the same tree:
run `Sample` N times seeded, bin outcomes, compare to `Enumerate`'s distribution (chi-square in
CI on a fixed fixture set) — cheap, and it pins the two executors together permanently.

## Action space (13) — policy view

```
0..=3   move slot (as today)
4..=8   switch to k-th bench slot (as today; also answers Replace requests)
9..=12  move slot with Terastallize
```

- Mask rules: tera actions legal iff `!side.tera_used` and not committed (rampage/charge/encore);
  during `Replace`, only 4..=8 legal (bench-alive); during a locked move, only the locked slot.
- **Pivot landing is a first-class `Replace` request** after U-turn/Volt Switch damage resolves —
  no pre-declared pivot target. This matches PS timing exactly: real players also pick the
  landing mon after seeing the damage, so the policy gets the same information a human has.

## Bridge/env changes

- `BattleVec.step_all` keeps the batched shape but becomes request-driven: it returns
  `(request_kind, side_to_act, obs, mask)` batches; envs advance independently ("advance until
  the learner's next decision" wrapper stays in Rust for self-play symmetry).
- Turn-boundary bookkeeping (frames, Φ deltas, GAE step attribution) keys on *decision points*,
  not turns. The buffer's step unit changes accordingly — this is the main agents/ side cost.
- 9-action checkpoints don't transfer (action head shape changes). Acceptable: we're training
  a new policy (Rob, 2026-07-11). Keep obs layout append-only regardless.

## Verification obligations (before any big run)

1. Cosim replay of the new executor in `Enumerate` mode over the full corpus — must stay
   1,532/1,532 + smoke 18/18 + mutations 8/8 (the refactor must not move mechanics).
2. Sample-vs-Enumerate distribution pinning (above) in CI.
3. Legality diff vs PS request JSONs extended to the 13-action mask (tera/committed/replace).
4. On-policy cosim happens in P2 with the new policy (deferred per Rob).

## Migration steps (each lands green)

1. `Forker` trait; port `generate_instructions` internals; `Enumerate` passes all gates.
2. `step_sampled` + request state machine on top of `Sample`; bench (expect ≥10× single-thread).
3. 13-action mask + tera instruction wiring in the sampled path; legality diff vs PS requests.
4. `BattleVec` request-driven API + agents/ buffer rework; retire the free-hit replacement MDP.
5. CI: distribution-pinning test + gate hardening (fail on unsupported>0 — task #6).
