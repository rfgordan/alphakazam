# P1.1 Rust–Showdown Behavioral Parity Progress

Last updated: 2026-07-11

## Objective

Achieve bidirectional behavioral equivalence between the fast Rust RL engine and the pinned
Pokémon Showdown implementation for the supported Generation 9 random-battle scope. For
stochastic mechanics, equivalence means matching both the exact set of possible outcomes and
the probability assigned to every outcome—not merely reproducing individual sampled traces.

Pinned Pokémon Showdown commit: `b9dc987d344635789116ae46c48f8e2480e0ddc2`.

## Approach

The work uses several complementary verification layers:

1. **Sampled transition replay.** Showdown battles are recorded with requests, choices, PRNG
   draws, and state snapshots. The Rust engine replays each decision and compares canonicalized
   resulting states in both directions. This quickly exercises long battle sequences and detects
   deterministic state, damage, mechanic, and action-legality differences.

2. **Exact distribution enumeration.** The Showdown oracle intercepts its PRNG and recursively
   enumerates every possible draw. Equal serialized states are coalesced, producing exact outcome
   support and probability mass rather than Monte Carlo estimates.

3. **Factorized move-action kernels.** Whole-turn enumeration can multiply the stochastic paths
   of two attacks and residual actions. The oracle therefore pauses after each queued action and
   emits conditional move kernels. Rust exposes `generate_move_action`, allowing each move kernel
   to be compared independently without the cross-product explosion.

4. **Request-boundary certification.** A forced replacement after a faint is treated as a
   policy-neutral endpoint. The verifier compares the exact probability of reaching that boundary
   without arbitrarily choosing the next Pokémon. Pivot requests with a living active Pokémon
   remain policy-dependent and explicitly unsupported.

5. **Randomized seed campaigns.** Deterministic random-battle teams and choices generate new
   exact fixtures. Every mismatch is reduced to its first differing field or probability mass,
   fixed in Rust, and then checked against focused seeds, the sampled corpus, release tests, and
   the bounded exact-distribution smoke suite.

6. **Reversibility checks.** Engine instructions must apply and reverse exactly because RL search
   depends on reversible state mutation. Mechanics such as forme changes and consumed items are
   tested for round-trip identity.

## Major work completed

### Verification infrastructure

- Added bidirectional canonical state comparison and exact distribution comparison.
- Added action-staged Showdown enumeration and Rust single-move generation.
- Added conditional kernel metadata for side, move, and the opponent's pending move.
- Added forced-switch boundary comparison with exact aggregate probability mass.
- Fixed sentinel/live-active comparison at faint boundaries.
- Added a sequential forced-switch fixture (seed 90) to the distribution smoke gate.
- Reduced oracle memory by retaining only consumed move kernels and storing staged checkpoints as
  compact JSON strings. For seed 145, output fell from roughly 145 MB to 74 MB and measured peak
  memory footprint fell from roughly 1.26 GB to 760 MB, with identical terminal and move-kernel
  hashes.

### Engine parity fixes

The discrepancy campaign has implemented or corrected, among others:

- Light Ball, Strong Jaw, Slow Start, Mind Plate, and exact final-modifier chaining.
- Fiery Dance stochastic self boosts and flinch action kernels.
- Wish phase timing, Take Heart, Payback, and confusion's exact `33/100` probability.
- Mid-turn no-active projection and forced-switch boundary state handling.
- Electromorphosis, Charge damage/consumption semantics, and sampled Charge lifecycle behavior.
- Focus Sash survival and item consumption across fixed, ordinary multi-hit, and indexed multi-hit
  damage paths.
- Ice Face forme changes and stat recomputation.
- Exact multi-hit Ice Face behavior: the first Pokémon-connected physical hit breaks the face and
  deals zero damage, subsequent hits use Noice Form Defense, hit counters are correct, and
  Substitute damage/break/overflow ordering is preserved with a bounded convolution.
- Variable/high-hit Sturdy and Focus Sash behavior now remains sequential inside the bounded
  convolution: the first lethal hit leaves 1 HP, Focus Sash is consumed, the following hit can
  faint the target, and nominal hits after fainting no longer increment the hit counter.
- Action-legality comparison now recognizes rampage commitment when evaluating switching and
  Terastallization.
- Immediate pivot request endpoints can be certified without choosing a replacement when the sole
  exact move kernel is identical to the terminal switch-request distribution. Mixed or multi-action
  pivot paths remain explicitly unsupported.

## Current evidence

- Release engine/cosim tests pass, including focused Ice Face and instruction round-trip tests.
- Sampled corpus: **1,532 / 1,532 matched**, zero divergences, zero unsupported units.
- Sampled exactness: **100%**.
- Sampled coverage: **100%**.
- Sampled move coverage: **204 move IDs**.
- Previously reported sampled legality discrepancies: **eliminated**.
- Exact distribution smoke: **18 / 18 matched**, including forced-switch seed 90.
- Full-cap heavy seeds 145 and 147 matched after the memory changes.
- Previously capped seeds 161, 166, 170, 179, 182, 185, and 192 subsequently matched at the
  100,000-path cap. Seed 190 reached a policy-dependent pivot boundary without a Rust divergence.
- Seed campaign 181–220: all 32 tractable seeds matched; 8 exceeded the conservative 20,000-path
  campaign cap; no behavioral discrepancy was found.
- Earlier campaigns through seed 180 exposed and drove the Wish, Take Heart, confusion, Payback,
  Mind Plate, Electromorphosis, Ice Face, and Focus Sash fixes.

These results establish exact agreement over all currently completed fixtures. They are not yet a
mathematical proof over every possible supported battle state.

## Resource safeguards

- Rust release builds/tests use two Cargo jobs.
- Normal distribution smoke uses four bounded workers, while the larger seed 90 boundary fixture
  runs sequentially.
- Heavy oracle seeds are run sequentially and monitored. A run is stopped before memory pressure
  threatens the host.
- Individual oracle campaigns use explicit path caps. A cap is reported as unverified coverage,
  never counted as a match.

## Remaining frontiers

- Continue randomized exact seed burn-down and target mechanics/move combinations not yet reached.
- Improve worst-case oracle memory further; some full-cap cases can still approach roughly 850 MB.
- Revisit seeds that hit conservative path caps under safe sequential limits.
- Extend policy-aware verification beyond forced-faint endpoints if pivot/revival request transitions
  need whole-transition certification; current move kernels remain independently comparable.
- Audit supported-scope completeness rather than inferring universal parity from a finite corpus.
- Run a final clean-tree audit and preserve exact regression fixtures for every newly discovered
  discrepancy.

## Relevant commits

- `a9c35d1` — exact bidirectional distribution comparison.
- `3cf0892` — staged Showdown enumeration and distribution smoke.
- `f8c374b` — factorized move kernels and several mechanic fixes.
- `c68b1ef` — flinch kernel and exact final modifier chaining.
- `a624ec6` — Wish, Take Heart, confusion, Payback, Mind Plate, and boundary projection fixes.
- `4e539a0` — Ice Face, Electromorphosis, and Charge.
- `08353ae` — Focus Sash consumption.
- `354e7e6` — exact multi-hit Ice Face with focused tests.
- `8f587fb` — bounded-memory exact oracle checkpoints.
- `2300c70` — forced-switch distribution boundary certification.

## Token usage

The persistent goal tracker reports **177,996 tokens used** and **2,559 seconds of tracked goal
runtime**. Its status is `usageLimited`, so this is the last available tracker-reported total and
may not include work performed after the tracker stopped updating. It should be treated as a
reported lower bound, not a precise final accounting for the entire ongoing task.
