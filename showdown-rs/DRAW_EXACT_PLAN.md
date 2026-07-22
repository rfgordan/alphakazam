# DRAW-EXACT: PRNG-level equivalence with Pokémon Showdown

Branch: `prng-exact` (cloned from main @ 433d299). Directive (Rob, 2026-07-14): for a given
seed, the Rust engine must sample the exact same outcome as PS — internally, the same number
of random draws in the same order. Pursue seriously; abandon only on clear hopelessness
evidence (kill criteria below). The distribution-equivalence campaign on main is PAUSED.

## Why this might now be tractable (vs. the June rejection)

The June decision rejected draw-for-draw matching as brittle. Three things changed:
1. **Semantics are already exact.** Distribution-level parity holds over 3,800+ verified units
   and ~180 exhaustively enumerated battles. Every stochastic event's *outcomes and
   probabilities* agree; what remains is purely draw *accounting* — order, count, and the
   PRNG itself. We are aligning bookkeeping, not re-deriving mechanics.
2. **We have ground truth for the draw stream.** The cosim recorder wraps PS's PRNG and logs
   every draw with semantic labels: (kind, args, result, effect/event context). We never have
   to guess PS's draw order from source reading — we diff against recorded reality.
3. **The pin removes the brittleness argument's force.** We certify against ONE PS commit
   (b9dc987d). PS-upstream refactors were always going to require re-certification; draw-order
   coupling adds no new exposure against a pinned target.

Precedent: `@pkmn/engine` achieved exactly this bar ("cycle-accurate" PS compatibility) for
gens 1–2. Gen 9's event system is far larger — that's what the differ-first strategy below is
designed to de-risk in days, not months.

## What it buys (why Rob wants it)

- **O(1) verification per battle.** Byte-compare full states after every decision for ANY
  seed — no enumeration, no caps, no heavy-seed problem. Verification throughput goes from
  ~80 battles/campaign to millions/day. The ultra-heavy-seed tail (331/349/404) evaporates.
- **Trivial on-policy verification**: training episodes are re-verifiable from (seed, choices)
  alone — the sidecar becomes a byte-diff loop.
- **Perfect repro debugging** and cross-engine replay of any PS battle.

## Architecture

### Phase 0 — PRNG port + raw-sequence gate (days)
Port PS's PRNG at the pin (sim/prng.ts — Gen-5-style 64-bit LCG over [4×u16] seeds; verify
whether the pin uses the LCG or sodium path for battle seeds). Rust `PsPrng` with
`next(from,to)`, `randomChance(n,d)` (= `random(d) < n` — note the clamp lesson), `sample`,
`shuffle` — bit-identical including the internal call structure (`randomChance` consumes ONE
draw via `random`; `shuffle` consumes per-swap; `sample` one). Gate: 10^6-draw sequence
equality against node for each entry point and seed shape.

### Phase 1 — the draw-sequence differ (the core tool; ~1 week)
- **PS side**: extend the existing recorder to dump, per decision, the ordered draw list
  (kind, args, result, label) — it already does this; add a `--draws-only` fast mode.
- **Rust side**: new executor mode `Exec::Replicate(PsPrng)` — at each stochastic point,
  instead of seam-sampling with splitmix, consult the PsPrng with PS's exact call
  (`random(16)` for damage roll, `randomChance(a,m)` for accuracy, etc.) and emit
  (kind, args, site-label) to a draw log. Outcome selection uses the drawn value under PS's
  interpretation, so outcomes automatically match wherever the sequence matches.
- **Differ**: align the two logs per decision; first mismatch (missing draw / extra draw /
  wrong order / wrong args) = one ranked work item. Aggregate across seeds into the same
  burn-down scoreboard the parity campaigns used (`% decisions draw-exact`, ranked mismatch
  categories). This reuses the exact grind loop that took membership parity 15% → 100%.

### Phase 2 — the burn-down (the long haul; agent-driven like C1–C6)
Reorder Rust's internal evaluation until the differ is clean. Known structural work items,
roughly increasing difficulty:
1. Per-event draw sites and order within a move: accuracy → crit → damage roll → secondary
   rolls per hit → contact procs, exactly as PS's `hitStepsGeneric` sequence at the pin —
   including draw-and-discard sites (places PS rolls even when the result can't matter).
   Rust's `execute_move` already sequences these semantically; the work is 1:1 site auditing.
2. Draws currently folded into branch structure: duration draws (sleep/confusion/rampage
   `random(2,4)`-class), variable multi-hit count, drag-target `sample`, Shell Side Arm tie,
   metronome-class — each must consume the PsPrng at PS's exact call moment (the sumset-DP
   compressions are Enumerate-mode only; Replicate mode follows the realized path so no
   explosion).
3. `speedSort` tie draws: PS shuffles equal-priority event handlers AND equal-speed actors
   consuming draws (battle.speedSort → prng.shuffle). Need PS's comparator keys (priority,
   speed, subOrder…) per queue type and the shuffle's exact consumption. This is the largest
   unknown; the differ will expose every instance with labels.
4. Event-handler draw sites outside moves: residual order draws (if any), switch-in effects,
   item procs (Custap eat has no draw; Effect Spore d100; Cute Charm 30%…) — sites already
   modeled, need order audit only.

### Phase 3 — certification
- Seed-exact gate: for seeds 1–N (same seeds as the campaigns), drive both engines from the
  seed alone (PS normally; Rust via Replicate + the same choice script) and byte-compare
  converted states after every decision + the draw logs. Scoreboard identical in shape to the
  old parity table. Goal: 100% over ≥500 seeds + full-battle parity on complete games (not
  just 2 decisions).
- Keep ALL existing gates green on this branch (distribution tests, corpus sweep, mutations):
  Replicate is a third Exec mode; Enumerate/Sample semantics must not move.

## Kill criteria (what "clearly hopeless" means — check at each phase gate)

1. **Unobservable-state dependence**: if the differ shows PS draw order depending on state we
   cannot reconstruct from the battle (e.g., JS object insertion order that varies with
   history the engine doesn't and shouldn't track), documented across >~5 irreducible sites
   after real analysis — that's structural, not effort-bound. (Assessment: unlikely — PS is
   deterministic-by-seed by design; replays depend on it.)
2. **Phase-1 differ shows mismatch density that doesn't decay**: if after the first 20
   burn-down items the %-draw-exact curve is flat (each fix reveals ≥1 new independent
   mismatch class), reassess scope with data in hand.
3. Budget checkpoints: Phase 0+1 ≈ 1 agent-week equivalent. If the differ isn't producing a
   ranked work queue by then, stop and report.

## Non-goals / guardrails

- Enumerate/Sample modes and all main-branch verification stay untouched in behavior; this
  branch adds Replicate alongside (the distribution gates keep running as regression rails).
- No attempt to match PS's draw stream across UNPINNED versions.
- Team generation already matches by construction (pool sampled from PS's own generator);
  battle-seed draw matching is the scope.

## Status

- [x] Branch cloned from main @ 433d299 (c6a landed; c6b/C7/seed-finalization paused with
      the main campaign — see memory/equivalence-campaign-state.md for their resume points).
- [ ] Phase 0 (PRNG port + gate)
- [ ] Phase 1 (Replicate mode + differ)
- [ ] Phase 2 burn-down
- [ ] Phase 3 seed-exact certification
