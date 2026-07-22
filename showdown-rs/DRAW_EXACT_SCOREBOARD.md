# DRAW-EXACT — Phase 1 first scoreboard

Reproduce: `cargo build --release -p cosim && DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz`
PS pin: `b9dc987d`. Corpus: 111 traces / 3831 move units.

## PRNG algorithm (Phase 0)
Battle seeds use the **Gen-5 64-bit LCG** over a `[4×u16]` state, NOT the sodium/ChaCha path.
`PRNG.setSeed` dispatches on the seed string; the recorder builds every battle with a 4-number
array seed (`[n, n+7, n+13, n+29]`), which `PRNG`'s constructor joins to a digit-leading string
`"n,..."` → the `Gen5RNG` branch. `x ← (a·x+c) mod 2^64`, `a=0x5D588B656C078965`, `c=0x00269EC3`;
`next()` returns bits 63..32. Raw gate: 25.6M draws × 8 seeds × every call pattern, bit-identical
(`crates/engine/src/psprng.rs`, fixture `tests/psprng_gate.txt`).

## Scoreboard
```
units: 3831  supported: 3831  unsupported: 0
DRAW-EXACT: 2183 / 3831 = 56.98%

mismatch categories (ranked):
   989  rust-finished-with-unconsumed-draws     (PS drew more than the engine emitted)
   385  rust-requested-draw-not-next-in-log      (engine drew where PS didn't, or wrong kind)
   274  args-mismatch                            (same kind, different call args)
     0  state-mismatch-despite-draw-match
```

`DRAW-EXACT` = the engine requested exactly the recorded sequence (same count, order, kinds,
args) AND a branch reproduces PS's `stateAfter`. Realized roll *values* are validated by the
state, so only draw shapes are compared.

## Top-20 first-mismatch labels (the burn-down queue) + hypotheses

| n | label | hypothesis |
|---|-------|-----------|
| 147 | `randomChance[100,100]@accuracy` (rust extra) | Engine emits an accuracy roll where PS skips `hitStepAccuracy` — accuracy-`true` moves, or a move on an absent/fainted target. Guard accuracy annotation on PS's actual roll condition. |
| 123 | `shuffle[2,0,2]@generic` | **PS `speedSort` shuffles equal-priority EVENT-HANDLER lists** (2-element), independent of actor speed — fires ~3×/turn even at 421-vs-106 speed. The engine models no handler-order shuffle. Charter's Phase-2 item #3, the keystone: many downstream pos-0/1 mismatches are just this shifting the alignment. |
| 96 | `random[100]@closecombat` | `move.self.boosts` draw-and-discard: PS `selfDrops` rolls `random(100)` even at 100% self-drop; the engine applies the self drop deterministically with no draw. (Same for dracometeor 46, rapidspin 44, makeitrain 32, headlongrush 23.) |
| 81 | `shuffle[4,2,4]@generic` | `speedSort` of a 4-element equal-key handler run (slice `[2,4)`) — e.g. residual/switch-in/faint order. Same family as #2, larger group. |
| 57 | `randomChance[90,100]@thunderwave` | **Status-move accuracy**: status moves resolve in `execute_move_inner`, a path with no accuracy annotation yet. (willowisp 28, leechseed 21, spore 17, encore 21 — all status-move accuracy / `randomChance(100,100)`.) |
| 46 | `random[100]@dracometeor` | self-boost draw-and-discard (see #3). |
| 44 | `random[100]@rapidspin` | self-boost draw-and-discard (Spe +1). |
| 43 | `shuffle[3,1,3]@generic` | `speedSort` 3-element handler run, slice `[1,3)`. Handler-order family. |
| 32 | `randomChance[1,3]@stall` | Protect/Detect consecutive-use counter `randomChance(1, 3^k)`; the engine models Protect success without the stall roll. args-mismatch because the engine's next draw is accuracy. |
| 32 | `random[100]@makeitrain` | self-boost (SpA −1) draw-and-discard. |
| 31 | `random[2,5]@slp` | **Sleep DURATION** `random(2,5)` rolled when sleep is applied; the engine sets the counter deterministically. Duration site (charter Phase-2 #2). |
| 30 | `shuffle[5,3,5]@generic` | `speedSort` 5-element handler run, slice `[3,5)`. |
| 28 | `randomChance[85,100]@willowisp` | status-move accuracy (see #5). |
| 26 | `random[100]@thunderbolt` | target secondary `random(100)` not emitted on the realized branch (likely a KO branch, where PS still logs the roll for a surviving-target path but the engine's secondary split short-circuits). Audit secondary emission vs target liveness. |
| 23 | `random[100]@headlongrush` | self-boost draw-and-discard. |
| 21 | `randomChance[100,100]@encore` | status-move accuracy (Encore is `accuracy: true`? then this is the reverse of #1 — PS rolls where engine doesn't). Audit which status moves roll. |
| 21 | `randomChance[90,100]@leechseed` | status-move accuracy. |
| 19 | `sample[20]@bulletseed` | **Variable multi-hit COUNT** draw. PS picks the 2–5 hit count via a `sample`; the engine folds the count into the sumset-DP without a draw. Multi-hit-count site (charter Phase-2 #2). |
| 17 | `randomChance[100,100]@spore` | status-move accuracy. |
| 15 | `randomChance[1,24]@bodypress` | args-mismatch: engine emits accuracy `[100,100]` where PS's next is the crit `[1,24]` — an alignment slip downstream of a missing leading handler shuffle (#2). |

## Phase-2 burn-down log

Each row: the class fixed, the differ % before/after (over 3831 supported units), and the
PS-source basis. The fix-decay curve (% gained per class) is kill-criterion #2 evidence.

| date | class fixed | before | after | Δ | basis |
|------|-------------|--------|-------|---|-------|
| 2026-07-22 | **self-boost draw-and-discard** (`random[100]@<move>`: Close Combat / Draco Meteor / Rapid Spin / Make It Rain / Headlong Rush / Superpower / Overheat / …) | 56.98% | 62.39% | +5.41 (+207 u) | PS `selfDrops` (battle-actions.ts:1338) rolls `random(100)` for `move.self.boosts` even at guaranteed 100% (no `self.chance`), consumed after the damage rolls and before target `secondaries`. |
| 2026-07-22 | **secondary rolls when the effect can't land** (target-secondary + flinch on a KO, e.g. `random[100]@icebeam/psychic/moonblast/thunderbolt/saltcure`; 100%-self-only secondaries `random[100]@rapidspin`) | 62.39% | 69.75% | +7.36 (+282 u) | PS `secondaries()`/`selfDrops` roll `random(100)` per secondary as long as the target object is present (a fainted target is NOT `false`) and `ModifySecondaries` didn't strip it. Only Shield Dust / Covert Cloak (alive) strip target-facing secondaries pre-roll; Inner Focus / already-flinched / dead target still roll (block happens later in the volatile-add path). A `secondary:{chance:100,self:{boosts}}` (Rapid Spin +Spe, Trailblaze, Power-Up Punch) is a secondary → one roll. |
| 2026-07-22 | **status-move accuracy** (`randomChance[acc,100]@<statusmove>`: Thunder Wave / Will-O-Wisp / Leech Seed / Spore / Encore / Taunt / Hypnosis / …) | 69.75% | 73.19% | +3.44 (+132 u) | PS `hitStepAccuracy` rolls `randomChance(accuracy,100)` for a foe-targeting status move with numeric accuracy. Bypassed (accuracy → `true`, no roll) for `accuracy:true` moves, self-targeting status moves, and Toxic-by-a-Poison-type. Emitted at the general accuracy branch in `execute_status_move` on `b` so hit+miss inherit. |
| 2026-07-23 | **accuracy roll only when the move reaches `hitStepAccuracy`** (removes `accuracy` rust-extra draws vs an immune / fainted / semi-invulnerable target) | 73.19% | 78.57% | +5.38 (+206 u) | PS's hit-step order rolls accuracy AFTER invulnerability + type/ability immunity. A damaging move vs a fainted foe (no target), a semi-invuln dodger, or a type/ability/flag-immune target fails earlier and never rolls. Gate the accuracy annotation on `foe_alive && !semi_invuln && connects` (the same conditions the engine already used for its early-fail branches, just moved ahead of the draw). |
| 2026-07-23 | **duration draws** (`random[2,5]@slp`, `random[2,6]@confusion`, `random[2,4]@lockedmove`) | 78.57% | 80.19% | +1.62 (+62 u) | PS rolls a duration on the `onStart` of freshly-applied sleep (`random(2,5)`), confusion (`random(2,6)`), and a rampage lock (`lockedmove` `trueDuration = random(2,4)`). The engine already branched these counters — emit the draw in `branch_sleep_counter` / `branch_confusion_counter` / the rampage-start arm so the draw stream carries them. (residual `slp` at pos 4 = secondary-sleep ordering, left for a later pass) |
| 2026-07-23 | **Protect stall counter** (`randomChance[1,3^n]@stall`) | 80.19% | 80.68% | +0.49 (+19 u) | PS `stall` volatile `onStallMove` rolls `randomChance(1, counter)` with `counter = 3^n` (capped 729) on each *consecutive* protect use; the first use has no `stall` volatile yet, so no roll. The engine's `stall_counter` is exactly `n` — emit `randomChance(1, 3^n)` on both the success and fail branches when `n >= 1`. |
| 2026-07-23 | **special-cased status moves' missing draws** (`random[100]@curse`, `randomChance[100,100]@strengthsap`) | 80.68% | 81.44% | +0.76 (+29 u) | Curse and Strength Sap return early in `execute_status_move` before the general draw sites. Non-Ghost Curse's `onTryHit` rewrites the move to `move.self = {boosts}` → `selfDrops` rolls one `random(100)`. Strength Sap is a foe-targeting numeric-accuracy status move → `hitStepAccuracy` rolls `randomChance(100,100)`. Emit each in its special-case block. |

## Assessment
The mismatch classes are **structured and finite** — handler-order `speedSort` shuffles,
self-boost/secondary `random(100)` draw-and-discard, status-move accuracy, duration rolls,
multi-hit-count draws. None indicate unobservable-state dependence (kill criterion #1 not met),
and the queue is a clean ranked work list. The single highest-leverage item is modeling PS's
`speedSort` handler-order shuffles, which underlies both the direct `shuffle@generic` misses
and many downstream alignment slips. Direction is viable; proceed to the Phase-2 burn-down.

## Gates (all green)
- `cargo test --release -p engine`: all suites pass (psprng raw gate incl.).
- `sampled_distribution`: Sample ≡ Enumerate (annotation didn't move Sample).
- Full corpus state-verify: **3831/3831 matched, 100% exact, 0 unsupported**.
