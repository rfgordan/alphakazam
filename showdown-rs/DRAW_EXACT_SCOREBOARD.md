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
| 2026-07-23 | **accuracy forced `true` skips the roll** (removes `accuracy` rust-extra vs No Guard / weather-perfect / Glaive Rush — the `crit-args` cluster: Hurricane, Poltergeist, Close Combat, Quick Attack, Beat Up) | 81.44% | 82.28% | +0.84 (+32 u) | PS overrides accuracy to `true` (no `randomChance` draw, but crit still rolls) via an event: No Guard (`onAnyAccuracy`), a Glaive Rush target (`onAccuracy`), and weather-perfect accuracy (Blizzard in snow; Thunder/Hurricane/Bleakwind/Wildbolt/Sandsear Storm in rain — `onModifyMove move.accuracy = true`). A plain 100-accuracy move still rolls `randomChance(100,100)`. New `accuracy_forced_true` predicate gates both the damaging and status accuracy draws. |
| 2026-07-23 | **Residual handler-order `speedSort` shuffles** (the `shuffle[N,N-2,N]@generic` end-of-turn tail-ties: `shuffle[4,2,4]`/`[3,1,3]`/`[5,3,5]` — 88+47+35 u — plus the Residual half of `shuffle[2,0,2]`) | 82.28% | **86.19%** | +3.91 (+150 u) | PS `fieldEvent('Residual')` (battle.ts:507) `speedSort`s the collected residual handlers; every tie-group of ≥2 handlers equal under `comparePriority` consumes one `prng.shuffle(list, sorted, sorted+len)`. For the Residual event priority & effectOrder are 0 for all handlers, so a tie ⟺ equal (order, speed, subOrder). New `residual_handlers()` rebuilds PS's list from board state with per-effect (order, subOrder) keys from data at the pin (label-audit-verified: weather 1/5, terrain-field 27/7 + Grassy-per-active 5/2, trick-room 27/1, screens 26/{reflect 1, lightscreen 2, tailwind 5, auroraveil 10}, leftovers/black-sludge 5/4, orbs/sticky-barb 28/3, speedboost/baddreams/harvest/cudchew 28/2, hungerswitch 29/7, wish 4/3, psn/tox 9/0, brn 10/0, ordered volatiles ingrain 7…roost 25 all subOrder 2, protect+stall at order `false`); `emit_residual_shuffles()` is a selection-sort mirror of `comparePriority` emitting one shuffle per tie-group, called (annotation-only) at the top of `apply_end_of_turn`. Verified zero false-positive emissions (no rust-side shuffle mismatch anywhere in the corpus). |

## Burn-down summary
56.98% → **86.19%** over 9 committed classes (+29.21 pts, +1119 units). Fix-decay curve is
healthy: each class landed a real, structured, decaying slice (207, 282, 132, 206, 62, 19,
29, 32, 150 units) with **no** new independent mismatch class revealed per fix beyond the known
queue — kill-criterion #2 (non-decaying density) is **not** triggered. All 9 classes are
draw-*accounting* fixes; none required unobservable state (kill-criterion #1 not triggered).

## The keystone: `speedSort` handler-order shuffles — label audit + what landed

### Label-audit methodology (recorder enhancement, landed)
The recorder (`instrumentPrng` in `harness/cosim.mjs`) now tags every `shuffle` draw with the
triggering `eventid` (via a runEvent/fieldEvent/eachEvent id-stack, since PS sets `battle.event`
*after* the speedSort) and the full handler list with each element's resolved
(effect id/type/name, holder, order, priority, speed, subOrder, effectOrder). A ~28-trace
label-audit set (r*/d*/ou/directed/trap/coverage seeds, same games as the certification corpus)
was regenerated to `/tmp/drawlabels/` — the committed corpus stays the certification set. This
turned the `@generic` blob into an exact per-(event,tie-group) work list. **The audit
decomposes the 438 shuffle draws by bracket:**

| eventid | draws | tie-group composition | scope |
|---|---|---|---|
| `Update` (eachEvent) | 240 | the two actives at **equal Speed** (`shuffle[2,0,2]`) | per-move / per-faint / end-of-turn — see below |
| `Residual` (fieldEvent) | 94 | end-of-turn handler list, tail/cross-side ties | **LANDED** |
| `null` (action queue) | 53 | `move,move` actor tie (partly modeled); `team,team` teampreview | partly modeled |
| `BeforeTurn` (eachEvent) | 36 | actives equal Speed | same family as Update |
| `ModifyDamage` (runEvent) | 11 | both sides' screens (`onAnyModifyDamage`, subOrder 4) equal | small |
| `Weather`/`TerrainChange` (eachEvent) | 3 | actives equal Speed | Update family |
| `TrapPokemon` (runEvent) | (trapF only) | No Retreat + Octolock + partial-trap stack tie | excluded (see below) |
| `DisableMove` | 1 | choicelock + healblock tie | excluded (negligible) |

### Residual bracket — LANDED (82.28% → 86.19%)
See the burn-down row above. `residual_handlers()` + `emit_residual_shuffles()` in `generate.rs`
rebuild PS's `fieldEvent('Residual')` list and selection-sort it, emitting one shuffle per
tie-group. Ties reduce to equal (order, speed, subOrder) because priority & effectOrder are 0 for
every Residual handler. Zero false-positive emissions across the whole corpus (the differ shows
no rust-side shuffle mismatch anywhere), so no exact→fail regression is possible — the model only
ever emits a shuffle PS also emits.

### Remaining bracket — `eachEvent` actives Speed-tie (`Update`/`BeforeTurn`/`Weather`, the residual `shuffle[2,0,2]@generic` = 129 units)
`eachEvent(eventid)` (battle.ts:465) speed-sorts `getAllActive()` with `(a,b)=>b.speed-a.speed`;
in singles the two actives tie **iff their current Speeds are exactly equal**, emitting
`shuffle[2,0,2]`. This is the dominant remaining class and is **concentrated in equal-Speed
matchups** (audit: t1 99 draws / 21 decisions, c1 49/9, r10 32/6, c5a1 30/6, c7 27/4; 22 of 27
audit traces have **zero**). The tie predicate (both actives alive, `effective_speed` equal) is
fully reconstructable from state — **kill-criterion #1 not triggered**. What makes it larger than
Residual is *call-count*: PS fires `eachEvent('Update')` at **seven** sites across a turn
(battle.ts:474 after every Weather event, 2866/2882 in the faint/endturn loop; battle-actions.ts:83
on switch-in, 970/1024 in the move/afterMove path), so a single equal-Speed turn emits an Update
shuffle at turn-start, after each move hit, after each faint, and at end-of-turn — interleaved
with the move kernel's own draws (see c1 dec20: Update, queue-tie, Update, [move draws], Update×3,
Residual, Update). Faithful modeling therefore requires mirroring PS's per-turn Update dispatch
schedule inside `execute_move`/faint handling, not a single end-of-turn injection — a larger,
move-kernel-interleaved change than the discrete Residual phase. Deferred as the next tranche;
the predicate and call-site map above are the complete spec.

### Excluded brackets (documented, cannot/should-not model here)
- **`TrapPokemon` multi-trap stack** (`shuffle[3,0,3]`, trapF only): fires when one mon is trapped
  by ≥2 sources simultaneously (No Retreat + Octolock + partial-trap) whose handlers all tie at
  order `false`/subOrder 2/equal speed. This is a `runEvent('TrapPokemon')` at switch/request time,
  outside end-of-turn; it appears only in the deliberately-pathological trapF game. Reconstructable
  from state (the trap volatiles are all tracked) but out of the Residual scope — a small directed
  item for a later pass, not a corpus-wide class.
- **`null` action-queue length** (`shuffle[3,0,2]` where the engine emits `shuffle[2,0,2]@speed-tie`):
  PS's turn queue also holds the pending `residual` (and sometimes `beforeTurn`) action, so an
  actor Speed-tie shuffles a length-3 list (`shuffle[3,0,2]`), not length-2. The engine's
  action-tie site emits `shuffle[2,0,2]`; the fix is to widen its `list.len` arg to the true queue
  length. Small, state-reconstructable; folded into the action-queue item.

### The mechanism (reference)
The dominant remaining class was PS's `battle.speedSort` shuffles (`shuffle[N, N-2, N]@generic`).
Findings + the design (pin `b9dc987d`):

**Where the draw comes from.** `speedSort(list, comparator)` (battle.ts:429) is a *selection
sort*. At each output position `sorted` it gathers `nextIndexes` = every element tying the
current best under the comparator; if that tie-group has length > 1 it calls
`this.prng.shuffle(list, sorted, sorted + len)` — exactly one `shuffle[list.len, sorted,
sorted+len]` draw (internal consumption already matches `psprng.shuffle`). So the recorded
`shuffle[N, s, e]` says: a length-`N` handler list had a tie-group of `e-s` handlers starting
at sorted-position `s`. The observed `[N, N-2, N]` shape = a 2-handler tie at the tail of the
list.

**The comparator (`comparePriority`, battle.ts:404)** orders by, in decreasing precedence:
`order` (asc, default 2^32), `priority` (desc), `speed` (desc), `subOrder` (asc), `effectOrder`
(asc). A *tie* (→ shuffle) needs ALL of these equal.
  - `effectOrder` is set ONLY for `*SwitchIn` / `*RedirectTarget` callbacks (battle.ts:999);
    for every OTHER event it is 0, so ties there collapse to (order, priority, speed, subOrder).
  - `subOrder` (resolvePriority, battle.ts:950+): `{cb}SubOrder`, else by effectType —
    Condition 2 (slot/side/field 3/4/5), Weather/Field 5, Poison Touch/Perish Body 6, Ability 7,
    Item 8, Stall 9.
  - `speed` = the holder's current Speed; on `*SwitchIn` a fractional `-indexOf(fieldPos)/…`
    offset is subtracted so the two sides' switch-in handlers never tie on speed (position
    breaks it), and hazards on one side tie fully and fall back to `effectOrder` = creation
    order.

**Which events shuffle.** `speedSort(handlers)` is called inside `runEvent`/`findEventHandlers`
(battle.ts:507, 794) for essentially every event that collects ≥1 handler; `eachEvent`
(battle.ts:468) speed-sorts the actives. A shuffle fires only when ≥2 collected handlers tie.
In singles the common tie is **two handlers on the same Pokémon sharing a subOrder** (e.g. two
Conditions/volatiles, both subOrder 2, same speed) or **two same-speed actives' handlers at an
equal subOrder** — hence the tail-2 tie-groups. The `[12,10,12]` at turn 1 is a large
switch-in/`onUpdate` handler list whose last two entries tie.

**The design to build (minimal faithful, data-driven from the pin).**
  1. A per-turn event *schedule*: the ordered list of `runEvent`/`eachEvent`/`singleEvent`
     calls PS makes across a turn (before-move cancel events, damage-calc Modify* events,
     on-hit `DamagingHit`/`AfterMoveSecondary`, switch-in `onStart`/`onSwitchIn`/`onUpdate`,
     end-of-turn `onResidual`). Only events that can collect ≥2 tying handlers matter.
  2. A handler *table*: for each (ability, item, volatile/condition, side/field condition,
     weather) present on the field, which of those events it has a callback for, plus the
     callback's `Order`/`Priority`/`SubOrder`. Extract data-driven from data/*.ts at the pin.
  3. At each scheduled event, build the handler list from the current board, run the selection
     sort, and for every tie-group of length > 1 emit `shuffle[len, start, start+len]`. Feed the
     realized shuffle order back only insofar as it affects later state (the differ verifies via
     `stateAfter`, so order need only be *consumed* correctly, not reproduced in the log).

**Highest-leverage next step (do FIRST):** extend the cosim recorder to log, per `shuffle`
draw, the `eventid` and the tying handlers' effect ids + sort keys (PS has all of this in
`speedSort`/`resolvePriority`). That converts the 300-unit `@generic` blob into an exact,
per-event work list and removes the guesswork of inferring events from shuffle sizes. It is a
recorder-only change (no engine risk) and gates the whole keystone.

**Kill-criterion assessment for the keystone.** Not triggered. For non-switch-in events the tie
key is (order, priority, speed, subOrder) — all reconstructable from board state. The only
insertion-order dependence is `effectOrder` on `*SwitchIn`/hazard handlers, which is *creation
order* — reconstructable from the battle's hazard-application / entry history (state the engine
already has or can track), not hidden JS object identity. So the keystone is effort-bound, not
structurally blocked.

## Other remaining classes (post-Residual, ranked at 86.19%)
Current differ top labels: `shuffle[2,0,2]@generic` (129 — the `eachEvent` Update Speed-tie
bracket documented above), then:
- `random[2,5]@slp` (21, pos 4): residual-applied sleep duration ordering.
- `randomChance[1,2]@harvest` (19): Harvest's end-of-turn 50% berry-restore, a residual ability
  draw at order 28 — interleaves with the residual shuffles (a harvest+protect mon has PS order
  `[harvest randomChance, protect+stall shuffle]`; the shuffle model emits the shuffle but not the
  harvest roll, so these units await the harvest class before going exact).
- `sample[20]@bulletseed` (19): variable multi-hit COUNT `sample([2,2,…,5])` = `random(20)`,
  then per-hit crit/damage. The engine folds count into the sumset DP — needs the DP path to
  emit the count sample (charter Phase-2 #2); structurally the largest non-shuffle item.
- `randomChance[1,24]@bodypress` (14) + crit-immune targets: PS rolls the crit `randomChance(1,
  critMult)` whenever `critRatio ≥ 1` and `willCrit` is undefined — even against a Battle
  Armor / Shell Armor / Lucky Chant target (the `CriticalHit` event only downgrades the
  *result*). The engine skips the crit draw when `crit_p == 0`; it should still roll and force
  the result to no-crit.
- `random[100]@saltcure` (14): a `secondary:{chance:100, volatileStatus}` (Salt Cure) rolls
  `random(100)`; the engine applies the volatile deterministically as a `target_volatile`.
  Same family as the Rapid Spin 100%-self-secondary — needs the codegen to distinguish a
  100%-secondary volatile from a primary `onHit` volatile.
- `randomChance[3,10]@cursedbody` (14, pos>0): Cursed Body's 30% disable roll on a hit that
  KO'd / on a branch the engine short-circuits — same "roll fires even when the effect can't
  land" family as the secondary fix, applied to the contact-ability path.
- `randomChance[1,2]@harvest` (11): Harvest's end-of-turn 50% berry-restore `randomChance(1,2)`
  (skipped in sun) — an end-of-turn ability duration/proc draw.
- `sample[1]@trace` (11): Trace copying an ability picks among eligible targets via `sample`
  (length 1 in singles) — a switch-in ability draw.
- residual `random[2,5]@slp` (21, pos 4): secondary-applied sleep ordering (a sleep secondary
  that lands after other draws) — a placement refinement of the duration class.

## Assessment
The mismatch classes are **structured and finite** — handler-order `speedSort` shuffles,
self-boost/secondary `random(100)` draw-and-discard, status-move accuracy, duration rolls,
multi-hit-count draws. None indicate unobservable-state dependence (kill criterion #1 not met),
and the queue is a clean ranked work list. The single highest-leverage item is modeling PS's
`speedSort` handler-order shuffles, which underlies both the direct `shuffle@generic` misses
and many downstream alignment slips. Direction is viable; proceed to the Phase-2 burn-down.

## Gates (all green, 2026-07-23 after Residual model)
- `cargo test --release -p engine`: all suites pass (12/12 main + psprng raw gate + 29-fixture etc.).
- `sampled_distribution`: Sample ≡ Enumerate (residual shuffles are annotation-guarded — Sample
  unmoved).
- Full corpus state-verify: **3831/3831 matched, 100% exact, 0 unsupported** (shuffles are
  state-neutral; the residual model cannot move state).
- Draw-consumption differ: **3302/3831 = 86.19% draw-exact**, no rust-side shuffle mismatch
  anywhere in the corpus (zero false-positive residual emissions).
