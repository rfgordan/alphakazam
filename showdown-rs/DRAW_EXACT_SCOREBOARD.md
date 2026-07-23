# DRAW-EXACT — Phase 1 first scoreboard

Reproduce: `cargo build --release -p cosim && DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz`
PS pin: `b9dc987d`. Corpus: 111 traces / 3831 move units.

---

## Phase 3 — seed-driven full-battle Replicate gate (`crates/cosim/src/seedgate.rs`)

The strategic pivot: annotation-mode scoreboarding (90.16% per-decision draw-exact) is done; the
goal bar is a **single-path executor** — same seed ⇒ same sampled outcomes, same draw count and
order — measured end-to-end per FULL GAME. Reproduce:
`SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz`.

**Result: 56 / 111 full games exact end-to-end (50.5%); init-aligned from seed 105/111.
Draw-consumption differ 90.16% → 97.57% (3738/3831), zero shuffle over-emission.**

### Phase-3 realized-cursor screen-shuffle desync (2026-07-23) — the misattributed "compute_damage rounding" class
The `state-mismatch-despite-draw-match` residue flagged as a screen×multi-hit **damage-rounding**
off-by-one (d4 t18, PS 326 vs engine 327) was NOT a `compute_damage` rounding bug — it was a
**draw-accounting desync in the realized multi-hit peek cursor**. `apply_multihit_realized` peeks
all hits' crit+damage up front from a positioned `RealizedCursor`, and `apply_multihit_realized_ma`
peeks inline; both step the cursor only over the crit/damage draws, NOT over the inter-hit
`ModifyDamage` screen-tie `shuffle[k,0,k]` that `apply_damage_hit`/the ma loop actually emit between
hits (`emit_modifydamage_shuffle`, fired when ≥2 screens are on the field). So for a **screened**
multi-hit move (d4 t18: Breloom Bullet Seed into an Amoonguss behind Reflect+Light Screen — k=2, one
shuffle per hit) every hit past the first read the shuffle's PRNG slot as its own crit/damage:
engine realized rolls `[1,12,3,4,4]` vs PS's recorded `[1,14,4,0,4]`, total 54 vs 55, leaving
Amoonguss (post Giga-Drain heal) at 327 not 326. Non-screened multi-hits (k<2) were unaffected —
which is why the DP-masked total only surfaced now and why the corpus's other multi-hit games stayed
exact. **Fix:** new `RealizedCursor::consume_shuffle(k)` (Prng cursor consumes `random(0,k)` draws
per PS `speedSort`; Recorded cursor skips the one logged shuffle entry) + `modifydamage_screen_count`
helper; called between hits in `apply_multihit_realized` and after each hit's emit in
`apply_multihit_realized_ma`. Enumerate/Sample DP path untouched (realized source not installed).
**d4 advanced d18[t18] → d54[t48]** (16/17 → 46/47 decisions); differ 97.55% → 97.57% (state-mismatch
5→4); state-sweep 3831/3831, engine tests all green, smoke 18/18, exact-set identical 56 (0
regression). The Beat Up state-mismatch residue (2 `@beatup` units, c2a2/c2a5) is separate — those
games are unscreened and diverge in SEED_GATE earlier (futuremove/accuracy); a genuine per-member
damage-calc item, still open.

### Phase-3 open-item-burn tranche (2026-07-23, 49 → 52; differ 94.07% → 96.50%)
Five classes landed this session (all rails green throughout: engine tests, corpus state-sweep
**7662 matched / 0 diverged / 0 unsupported**, distribution smoke **18/18**, SEED_GATE monotone —
zero prior-exact game regressed):
| step | games | differ | class landed | commit |
|------|-------|--------|--------------|--------|
| P3.15 | **52/111** | 94.57% | **Rest sleep-duration draw-and-discard** — PS Rest onHit `setStatus('slp')` rolls `random(2,5)` in slp.onStart THEN overrides `time=3`; engine omitted it (incl. Chesto-cured). Advances 3 games (c3a2s22/s23, c3c2s83 family). | 906e360 |
| P3.16 | 52/111 | 94.65% | **Hydration residual handler** — `residual_handlers()` omitted Hydration (Ability, order 5, subOrder 3), sorting ahead of Leftovers/stall/protect. `shuffle[3,1,3]@generic` 4→0; stream-neutral. | c742e69 |
| P3.17 | 52/111 | 94.86% | **Cute Charm / Poison Touch contact procs** — both roll `randomChance(3,10)` on ANY contact hit regardless of whether the effect can land; engine emitted no draw (Cute Charm) / skipped when status couldn't apply (Poison Touch). Poison Touch keeps PS's Shield Dust/Covert Cloak pre-roll bail. | 8710021 |
| P3.18 | 52/111 | 95.28% | **accuracy ModifyAccuracy 4096 chain + accuracy/evasion stages** — unified `accuracy_numerator`: onModifyMove (Hustle x0.8, sun Thunder/Hurricane=50) → ModifyAccuracy chain (Compound Eyes 5325/4096, Wide Lens 4505/4096) → stage boosts (trunc(acc*(3+b)/3) etc., ignoreEvasion moves excluded). `accuracy_of`+`accuracy_arg` now agree; previously accuracy_of ignored stages (no miss branch vs an evading target). | 4e7d4aa |
| P3.19 | 52/111 | 96.50% | **differ tie-order branch selection (MEASUREMENT-ONLY, no engine change)** — `diff_unit` reports Exact iff ANY state-matching branch reproduces PS's exact draw sequence, not whichever tie-order enumerated first; removes the move-order-tie false-positives. shuffle[2,0,2]@generic 23→18, args-mismatch 64→35. | 4f12f90 |

### Phase-3 variable multi-hit realized-executor tranche (2026-07-23) — RESERVED Class 2
The largest remaining differ block: variable multi-hit COUNT `sample` + per-hit crit/damage. The
Enumerate/Sample verification path folds the hit count into a sumset-DP (`apply_multihit_dp`) that
is exact for STATE but emits **no** per-hit draw stream (the full per-hit product is 32^hits — the
reason the DP exists), so Replicate/differ under-consume the PRNG on these moves and desync.

**Architecture (single-path realized executor — DP path untouched).** A thread-local
`RealizedSource` (`generate.rs`), installed ONLY by the seed gate (`RealizedSource::Prng` — the
decision-start PsPrng) and the differ (`RealizedSource::Recorded` — the unit's recorded draw
results), routes a variable multi-hit move through `apply_multihit_realized`: it draws the count +
each hit's crit/damage off the source in PS's exact order (`battle-actions.ts:864` loop) and
produces the ONE branch PS realized, reusing `apply_damage_hit` for per-hit application + KO /
Substitute-break termination + the per-hit crit/damage/ModifyDamage draw emission. The `Prng`
cursor positions a clone by shape-consuming the branch's draws-so-far (draw COUNT, not values,
fixes the position); the `Recorded` cursor indexes by draws-so-far length. Enumerate/Sample never
install a source → DP path byte-identical (state-sweep **3831/3831**, smoke **18/18**, engine tests
all green, no exact game regressed — same 52 exact set).

| family | landed | games | differ |
|--------|--------|-------|--------|
| **[2,5] standard + Loaded Dice + Scale Shot + Skill Link** | bulletseed / iciclespear / rockblast / tailslap / bonerush / pinmissile / scaleshot: count `sample([2×7,3×7,4×3,5×3])` = `sample(20)` → table; a Loaded Dice holder that samples 2/3 re-rolls `5 - random(2)`; **Skill Link** rewrites `[2,5]`→plain 5 in `onModifyMove` (no sample draw — was the iciclespear residual); Scale Shot's self-drop rides the existing `selfDrops` site. | 52→52 | 96.50% → **97.18%** (3697→3723) |
| **multiaccuracy (Triple Axel / Triple Kick + Population Bomb)** | `apply_multihit_realized_ma`: each hit past the first rolls its OWN accuracy `randomChance(acc,100)` (battle-actions.ts:907) and a miss ends the move — UNLESS the holder has **Loaded Dice**, whose `onModifyMove` *deletes* `multiaccuracy` (so every hit lands, no per-hit roll; Population Bomb's count becomes `10 - random(7)`). Ascending-power indexed calcs for Triple Axel; KO/Substitute-break truncation. The enumerated Triple Axel path (32³, Enumerate/Sample only) is unchanged; Population Bomb's Enumerate stays on the DP. | 52→**55** | 97.18% → **97.42%** (3723→3732) |
| **Beat Up** | one hit per participating party member (party order: the user + each alive, status-free ally), per-member base power `5 + floor(baseAtk/10)` but the USER's Atk; each member rolls crit `randomChance(1,24)` + damage `random(16)` (no count draw, not multiaccuracy). Routed through `apply_multihit_realized_ma` with the member `DamageCalc` list (`beatup_calcs`); Enumerate/Sample keep the sumset-DP `apply_beatup`. | 55→**56** | 97.42% → **97.55%** (3732→3737) |

**Beat Up residue (damage-calc, NOT draw).** Beat Up's DRAW stream is now exact — the per-member
crit/damage draws match PS (c2a4 is fully exact end-to-end). But pinning PS's exact rolls unmasks a
per-member `compute_damage` discrepancy in the OTHER Beat Up games (c2a1/c2a2/c2a3/c2a5): the
`randomChance[1,24]@beatup` DRAW mismatches (9) cleared, and a handful became
`state-mismatch-despite-draw-match` (draws align, HP off by a small amount) — the SAME masked
damage-rounding class as d4 t18, not a draw-stream defect. Those games diverge earlier in SEED_GATE
anyway, so no game regressed.

`sample[20]@bulletseed`/`@iciclespear`/`@tailslap`/`@rockblast`/`@scaleshot`, `@populationbomb`,
`@tripleaxel` labels all cleared. **Games flipped exact: 52 → 55** — c6a2s113 (Population Bomb),
r8 (Triple Axel), c3a2s22 (Skill Link iciclespear); zero prior-exact regressions (VERBOSE exact-set
diff vs a clean baseline — the truncated 45-row non-exact list is NOT a reliable regression signal,
so the exact-set diff is the gate). The [2,5]/Triple-Axel first-divergences advanced downstream
(d1 d9→d38, d4 d11→d18, r2 d3→d7).

**Why the realized path was needed for the enumerated Triple Axel too (KO truncation).** The
enumerated Triple Axel path emits its per-hit crit/damage draws for the WHOLE combo then applies with
KO-break — so a k=3 branch whose target faints on hit 2 carries 3 draw-pairs but PS's stream has only
2; `replicate_select`, at the position where the k=2 branch has ended, sees only the k=3 branch has a
draw there and consumes it, over-reading the NEXT decision's draw (r8's pre-existing d23 divergence,
present in the true baseline). Routing Triple Axel/Kick + Population Bomb through
`apply_multihit_realized_ma` (a single KO-truncated branch, only under a realized source) fixes it;
the enumerated fallback (draws never emitted under Enumerate) is retained byte-identical.

One residual on d4 t18 is a **screen×multi-hit damage-rounding** state-diff (draws now match;
PS 326 vs engine 327 — a `compute_damage` screen-halving rounding class the DP masked by reaching the
correct total via a different roll combo, now unmasked; separate from draw streams). Remaining
multi-hit differ label for the next family: `randomChance[1,24]@beatup` (9 — Beat Up per-member crit).

### Coordinator directive (differ-zero) — mandated observed-diff fixes (2026-07-23)
The completion bar is ZERO differ mismatches corpus-wide (every observed count/order/kind/ARGS/
handler-list diff is a fix target regardless of game yield). Landed this session:
| step | differ | class landed | commit |
|------|--------|--------------|--------|
| P3.13 | 93.27% | **stall residual handler (turn-after)** — `[5,2,4]` drained (stream-neutral, differ-only) | stall |
| P3.14 | 94.07% | **TrapPokemon multi-trap shuffle** — `shuffle[3,0,3]` drained (13→0), `[2,0,2]` 41→23; advances t6 d4→d24 | trappokemon |

**NAMED OPEN ITEMS (observed diffs still outstanding at 96.50% — PS evidence, for the next tranche):**
- **`sample[20]@bulletseed`/`@iciclespear`/`@tailslap` (19+5+…), `randomChance[1,24]@beatup` (9),
  `@populationbomb`/`@tripleaxel`** — variable multi-hit COUNT `sample([2..5])` (battle-actions.ts:864)
  + per-hit crit/damage; DP path shared with Enumerate/Sample (HIGH regression risk). **This is now
  the single largest block and the highest game-yield (~7-10 games direct).** Reserved for a
  dedicated tranche: land per sub-move-family, Replicate path only, full rails after each family.
- **`shuffle[2,0,2]@generic` (18, down from 23)** — the residue is the FIRST-MOVER no-draw
  status/failed move on a Speed tie: PS runs it first and fires its runAction `eachEvent('Update')`
  (2882) BEFORE the second move's draws; the differ's tie-order fix cleared the shape-ambiguous
  cases, but where the first mover produces NO draws the engine's enumeration can't shape-match
  either order (one has a leading shuffle, one doesn't). Needs the move-order/annotation path to
  interleave the first mover's 2882 Update ahead of the second move. **NOTE the SEED_GATE (Replicate)
  path already handles these via forced_tie_order — this is a differ/annotation-ordering residue, not
  a game blocker for the affected games (c5/c7 are exact through those turns).** PS battle.ts:2881.
- **`randomChance[100,100]@accuracy` (9)** — dossier Class 9: engine rolls accuracy where PS fails
  earlier at `onTry`. Gigaton Hammer / Blood Moon `cantusetwice` (disabled for SELECTION in PS, so
  the failing mon's recorded choice is murky in the differ's mc-resolution), Counter/Mirror Coat
  (accuracy:true, already no roll). Investigated: the mechanism is selection-time disable, not an
  execution `onTry` — needs the differ mc-resolution to mirror PS's disabled-move handling.
- **`randomChance[1,5]@frz` (7)** — same-turn ice-secondary-freeze decision boundary (a mon frozen by
  the opponent THIS turn then acting rolls the thaw where PS doesn't). `data/conditions.ts` frz.
- **`shuffle[3,0,2]@generic` (5), `[4,2,4]`/`[5,2,4]`/`[5,3,5]` (few)** — DOWNSTREAM/cosmetic. The
  `[3,0,2]` are Residual-event shuffles that appear as cascade artifacts of an upstream desync (no
  game first-diverges on them). The `[4,2,4]`↔`[5,2,4]` is the turn-after `stall` volatile length
  (Gothitelle protected LAST turn keeps `stall` without `protect`, but convert.rs derives
  `stall_counter` from the volatile counter via `log3` which rounds the turn-after counter to 0 →
  the handler is dropped). Stream-neutral (both consume one shuffle over the tie-group). Needs a
  "stall-volatile-present" flag in State/convert distinct from `stall_counter`.
- **`randomChance[33,100]@shedskin` (3)** — end-of-turn Shed Skin cure roll (abilities.ts, onResidual
  order 5 subOrder 3): `if hp && status && randomChance(33,100)` cure. State-affecting; same family as
  the landed Harvest/Cursed Body residual procs, needs correct residual-order placement.
- **`randomChance[90,100]@leechseed` (3)** — Leech Seed accuracy skip vs a specific target; and a tail
  of args cascades (`@seismictoss`/`@icebeam`/`@bodypress` — downstream of the multi-hit desync).

**LANDED this session (were open items): `random[2,5]@slp` (Rest, P3.15), `shuffle[3,1,3]`/[4,2,4]
missing residual handler (Hydration, P3.16), `randomChance[3,10]@poisontouch`/`@cutecharm` (P3.17),
`ModifyAccuracy` arg chain incl. Wide Lens/Compound Eyes/stages (P3.18).**

### Phase-3 deferral-burn-down tranche (2026-07-23, 42 → 49) — working the merged dossier queue
| step | games | class landed | commit |
|------|-------|--------------|--------|
| P3.7 | 43/111 | **double-switch batched/interleaved runSwitch bracket** (both-sides-switch turns) | double-switch |
| P3.8 | 44/111 | **Tri Attack secondary draws** (`random[100]`+`sample[3]` status pick) | triattack |
| P3.9 | 45/111 | **Dire Claw secondary** (`random[100]`+`sample[3]`, sleep-dur on slp) | direclaw |
| P3.10 | 47/111 | **Substitute-blocked secondary rolls** (sub hit still rolls `random(100)`) | sub-secondary |
| P3.11 | 48/111 | **Alluring Voice secondary roll** (100%-secondary emission) | alluringvoice |
| P3.12 | 49/111 | **ModifyDamage screen-tie shuffle** (both/either side screened, `[K,0,K]`) | modifydamage |
| P3.13 | 49/111 | **stall residual handler (turn-after)** — differ-only, stream-neutral | stall |
| P3.14 | 49/111 | **TrapPokemon multi-trap shuffle** (`[N,0,N]`, N≥2 trap sources) — advances t6 | trappokemon |

*P3.12 — ModifyDamage screen shuffle.* PS `getDamage` runs `runEvent('ModifyDamage')` after the
damage roll (battle-actions.ts:1830). Reflect/Light Screen/Aurora Veil register `onAnyModifyDamage`
handlers whose `effectHolder` is the SIDE (no `getStat`) → comparePriority `speed` 0, `subOrder` 4
(side condition); every present screen ties on (order false, priority 0, speed 0, subOrder 4)
regardless of active Speed. `speedSort` shuffles the tie-group once when ≥2 screens are on the field.
Every other ModifyDamage handler (resist berries, Multiscale, Life Orb, …) has speed>0/subOrder 7-8
and sorts BEFORE the speed-0 screens — corpus-wide EVERY mid-move ModifyDamage shuffle is `[K,0,K]`
(scan: 69× `[2,0,2]` + 4× `[3,0,3]`, start always 0). Emitted per damaging hit after the damage
roll in `apply_damage_hit`/`annotate_hits`. This is the mid-move `shuffle@<move>` the earlier analysis
mislabeled a handler-order mystery. Flips d2, advances d4 (bulletseed/Class 2). Corrects the
scoreboard's earlier "equal holder Speed" claim: side-condition handlers have speed 0, so screens
tie unconditionally.

**NOTE — the earlier "970-after-secondaries" model was incomplete:** the `shuffle@<move>` between the
damage roll and the secondary is NOT the per-hit 970 Update (that fires after `spreadMoveHit`); it is
this ModifyDamage screen tie inside `getDamage`.

*P3.7 — double-switch bracket.* A turn-action `sw/sw` turn is NOT batched: PS's `switch` action
(order 103) queues its `runSwitch` (order 101), which preempts the OTHER side's pending `switch`
(103) — so PS runs `switch(A), runSwitch(A), switch(B), runSwitch(B)` interleaved, and each switch
fires the SAME full 4-shuffle bracket as a single switch (switch-out :83, switch runAction 2882,
runSwitch getAllActive speedSort :182, runSwitch runAction 2882). The engine's double-switch loop
emitted only the switch-out :83 Update per switch; the three post-swap Updates per switch were
missing. Each shuffle is gated on the CURRENT incrementally-swapped board's tie (switch(B)'s
switch-out Update sees A already swapped in). Flips c1 (dossier Class 1a, predicted → EXACT); zero
prior-exact regressions.

### Phase-3 keystone tranche (2026-07-23, 38 → 42) — speedSort brackets + Thunder Wave immunity
Five draw-accounting/mechanics classes landed this session (3 commits), all rails green
throughout (engine tests, corpus state-sweep **3831/3831**, distribution smoke **18/18**),
SEED_GATE monotone with **zero** prior-exact game regressing at any step:

| step | games | class(es) landed | commit |
|------|-------|------------------|--------|
| P3.4 | 39/111 | **move-order tie composition** (b0==b3) + **status-move 970/1024 Update** + **Thunder Wave type immunity** | fb2e4cd |
| P3.5 | 42/111 | **switch / runSwitch post-swap Update bracket** (flips c2, c6, rd287) | bb5e6c4 |
| P3.6 | 42/111 | **terastallize turn-start bracket** (extra tera runAction Update; advances t2 d3→d9) | 08f4c0c |

Net flips vs the 38 baseline: **c2, c6, d7, rd287** (0 regressions). The two Update-bracket
classes plus the tie composition also advanced most tied-Speed games to a later (downstream)
divergence.

**Class details.**
1. *Move-order tie composition.* A both-move Speed tie's turn-start bracket is FOUR shuffles
   — commit `queue.sort()` (b0), `eachEvent('BeforeTurn')`, runAction Update, and the gen8
   dynamic re-sort of `[move,move,residual]` (b3, battle.ts:2946). `speedSort` composes the two
   queue sorts: side One executes first iff **b0 == b3**. The prior peek read only b0 (the
   pre-dynamic-resort order) and mis-selected whenever b0 != b3 — the residual both-move-tie
   divergence (c7 d29: surf-first, not the engine's saltcure-first).
2. *Status-move 970/1024 Update.* PS runs a status move through `hitStepMoveHitLoop` exactly
   like a damaging move: a `moveHit` that applies its effect fires the per-hit (battle-actions.ts:970)
   and post-hit-loop (:1024) Updates; a fully-failed move fires neither. Emitted at the
   execute_status_move call site gated on "the branch added ≥1 effect instruction" (protect-fail
   bookkeeping excluded). Both are actives-Speed-tie shuffles → no-op off a tie. RE-LANDS the
   reverted status-move Update; the scoreboard's earlier "calmmind = 970+2882" model was wrong —
   it's **970+1024+2882** (a self-boost leaves damage[i]=0, so line 1022 does not early-return).
3. *Thunder Wave type immunity.* Status moves default `ignoreImmunity = true` (battle-actions.ts:497);
   **Thunder Wave is the ONE status move with `ignoreImmunity: false`**, so a Ground target (0× to
   Electric) fails it outright — no accuracy roll, no paralysis. Every other status move ignores
   type-chart immunity (Toxic vs Steel still rolls then fails at setStatus; Roar/Growl affect
   Ghost; hazards target a side). Real mechanics gate; flips d7.
4. *Switch / runSwitch post-swap Update bracket.* PS runs a `switch` as two queue actions
   (`switch`, `runSwitch`), each ending in a runAction `eachEvent('Update')` (battle.ts:2881), plus
   the `runSwitch` `getAllActive()` speedSort (battle-actions.ts:182). The engine emitted only the
   PRE-swap switch-out Update (battle-actions.ts:83); the three POST-swap shuffles were missing.
   Ground truth (c2 d16): p2 switches a QuarkDrive-speed-boosted Iron Valiant (untied pre-swap)
   into Toxapex (tied post-swap), so the shuffles are all POST-swap — the observed `[Update, null,
   Update]`. The mirror `[BeforeTurn, Update, Update]` is a pre-tied/post-untied switch. Both
   empirical switch schedules now reconstruct from the pre/post-swap effective_speed predicate.
5. *Terastallize turn-start bracket.* A gen9 `terastallize` action (order 106) runs before the
   moves, so a Speed-tied both-move tera turn gains one shuffle: the tera action's runAction Update
   (battle.ts:2882). The commit sort also lengthens to `[tera,move,move]` (`shuffle[k+2,k,k+2]`) —
   stream-/state-neutral — but the missing tera Update was the real desync. Advances t2 (d3→d9).

### Label-audit methodology (this session)
The recorder (`instrumentPrng` in cosim.mjs) already tags every `shuffle` with its dispatching
`eventid` (via a runEvent/fieldEvent/eachEvent id-stack) and the full handler/active list with
each element's (effect id/type, holder, order, priority, speed, subOrder, effectOrder). All 111
traces were regenerated to `/tmp/drawlabels2/` (deterministic per seed+teamset+format), giving a
complete `shuffle` inventory bucketed by eventid: **Update 625, Residual 275, null 162, BeforeTurn
90, TrapPokemon 35, ModifyDamage 34, Weather/WeatherChange/TerrainChange 5, DisableMove 1.** A
one-off per-action tracker (wrapping runAction/switchIn/runSwitch) attributed each switch-turn
shuffle to its exact PS call site (line 83 vs 182 vs 2881), decoding the switch bracket. This audit
plus the seedgate DRAW_DIFF differ (victim-vs-source: the continuous-PRNG SEED_GATE label is the
*victim* decision; the re-synced annotation differ points at the *source* shuffle) drove the fixes.

### Handler-order handler-table — the (b) keystone class, characterized, DEFERRED
The task's "(b) mid-move handler-order speedSort over equal handler lists" reduces, in this corpus,
to almost entirely **`ModifyDamage` screen ties** (34 draws): a damaging move's `runEvent('ModifyDamage')`
collects each side's `onAnyModifyDamage` screen handler (Reflect/Light Screen/Aurora Veil, **subOrder
4**), and when both sides are screened with equal holder Speed the two handlers tie → `shuffle[2,0,2]`,
fired between the damage roll and the secondary (verified d4 t12: two `lightscreen` handlers). The
data-driven design mirrors `residual_handlers`/`emit_residual_shuffles`: a per-(damaging-hit) handler
table of the present screens keyed on (order,priority,holder-speed,subOrder=4), selection-sorted,
one shuffle per tie-group ≥2. Left DEFERRED (not implemented) this session — it blocks no game's
FIRST divergence (all ModifyDamage ties are downstream of a switch/Update source), and the
over-emission risk needs a per-hit incremental gate. Spec is complete; it is the clean next tranche.

### DEFERRED / documented follow-ups (with precise specs)
- **Switch bracket — double-switch (sw/sw)**: only the single-switch (move+switch) path landed;
  PS batches consecutive `runSwitch` into ONE shared speedSort. Both-side-switch turns still need
  the batched-runSwitch schedule. Small, state-reconstructable.
- **`TrapPokemon` runEvent shuffles (35 draws, trap games t1/t6/…)**: fire at switch/request time
  when a mon is trapped by ≥2 tying sources (No Retreat + Octolock + partial-trap, order `false`
  subOrder 2). The EXCLUDED directed class from the prior scoreboard — these desync the trap games'
  streams (t6 d4: an unmodeled `shuffle[2,0,2]@TrapPokemon` shifts the crit roll). Reconstructable
  from the trap volatiles; a directed item, not corpus-wide.
- **First-mover runAction Update on a no-draw first move (c5/c7 t61/t28)**: a both-move tie where
  the FIRST mover is a status/failed move (e.g. Recover at full HP) emits its runAction Update
  (2882) BEFORE the second move's draws — an extra leading `shuffle[2,0,2]` (5-shuffle bracket
  `[commit,BeforeTurn,Update,dynamic,Update]` vs the usual 4). The engine sequences both moves but
  the first mover's 2882 lands after its (empty) draw list; needs the move-order path to interleave
  it ahead of the second move on a tie. Blocks c5, c7 (else predicted exact).
- **Residual handler-length cosmetic (`[4,2,4]` vs `[5,2,4]`)**: PS's `stall` volatile has
  `duration: 2` (conditions.ts:439), so it survives one turn past the `protect` volatile — a mon
  can carry `stall` (an extra residual handler) without `protect`. The engine gates the `stall`
  residual handler on the Protect volatile, dropping it that turn. **Stream- and state-neutral**
  (both list lengths consume one `random(2,4)` over the same tie-group), so it is a label artifact,
  NOT a divergence source — no game hinges on it.


### Phase-3 burn-down progression (games exact end-to-end, from seed)
| step | games | class(es) landed | commit |
|------|-------|------------------|--------|
| baseline (threshold-secondary + move-order forcing) | 21/111 | — | — |
| P3.1 | 36/111 | **multi-hit KO early-termination** + **accuracy hit/miss site-typed selection** | b62bf51 |
| P3.2 | 37/111 | **switch-action pre-swap `eachEvent('Update')` shuffle** | 46668f1 |
| P3.3 | 38/111 | **drag-target `sample`** (Whirlwind/Roar/Dragon Tail) | 3a8ccc4 |

All four classes are realized-path (Replicate/annotation) corrections; the Enumerate/Sample paths
are byte-identical (every new draw site is gated on `annotating()`; the multi-hit KO break is
redundant with the existing lower one). Rails green throughout: engine tests, corpus state-sweep
**3831/3831** (0 diverged, 0 unsupported), distribution smoke 18/18, per-game SEED_GATE monotone
non-decreasing with **zero** previously-exact game regressing at any step (verified by exact-list
diff against the prior binary).

P3.1 — two draw-order corrections, both realized-path (Replicate) only, Enumerate byte-identical:
1. *Multi-hit KO early-termination.* The per-hit crit+damage draws are now emitted INSIDE the
   exact-hit loop (`apply_damage_hit`), after the top-of-loop KO check, matching PS's
   `hitStepMoveHitLoop` (the `targets.every(!hp)` break precedes the next hit's `getDamage`
   crit/damage rolls). A multi-hit that faints the target on hit *k* stops the draw stream at
   *k* pairs; combos that differ only in phantom post-KO rolls collapse to the same
   (draws, instructions), so the Replicate filter never over-consumes the PRNG. Cleared the whole
   `rust-extra randomChance[1,24]@crit` class (23 games' first-divergence).
2. *Accuracy hit/miss selection.* The accuracy `randomChance(acc,100)` draw was annotated on the
   shared pre-split branch with `result = (can-hit)`, so BOTH hit and miss branches carried `1`;
   on a real miss the filter matched neither, fell through to the crit roll, and mis-selected a
   HIT branch. Now the hit branches carry the hit value (1) and the miss branch overrides its copy
   to 0 — site-typed, per-branch, no prefer-longer-branch heuristic. (These two interact: the
   accuracy fix is what exposed/cleared the crit over-roll on the same games.)

### PsPrng from-seed validation (`RAW_DRAW_GATE=1`, `INIT_SCAN=1`)
Every one of the 111 games' recorded **strong** draw streams (the ~3.9k non-shuffle draws with
checkable results) reproduces bit-exactly from the recorded battle seed `[n,n+7,n+13,n+29]` once
the per-game init offset is applied (`no-fit = 0` over init 0..64). This certifies `PsPrng`
(`from_limbs` + the LCG) at the *call* level across whole games, not just the 25.6M raw gate.

### The pre-turn-1 offset — SOLVED (unlogged battle-construction draws)
The recorder's instance-wrap attaches AFTER `new Battle(...)`, so construction draws were
unlogged. Deep instrumentation (`Gen5RNG.prototype.next`, `/tmp/initprobe3.mjs`) identified them:
**per-mon gender rolls**. PS `new Pokemon` (pokemon.js:116) does
`this.gender = genders[set.gender] || species.gender || this.battle.sample(["M","F"])` — one
`sample` draw per mon whose species is **dual-gender** (no fixed `gender` field in the dex) AND
whose set leaves gender empty, in side-then-roster order (`Side.addPokemon`). For c1 that is
exactly 7 (4 + 3 dual-gender mons; the paradox/genderless leads don't roll) — matching the
observed offset (the 8th `next()` from the seed IS the teampreview shuffle value). Replicated in
Rust: `init_gender_rolls()` burns one draw per dual-gender mon (species table
`crates/cosim/src/fixed_gender.txt`, extracted from the pinned dex — 405 fixed-gender ids);
random-battle formats pre-generate teams with explicit set genders → 0 construction rolls. This
aligns 105/111 from seed. The residual 6 are custom directed teams whose SETS specify a gender
for a dual-gender mon (Breloom `|M|`, the loyal-three, …): the set is not in the trace, so a
set-gender is indistinguishable from a rolled gender in the snapshot — a documented gap needing
either the set data or a recorder field. The teampreview action's own shuffle (0 or 1 draw,
speed-tie dependent) is consumed from the recorded draw *shapes* (order-neutral).

### Replicate executor — the draw-stream filter
Reuses the `Enumerate` annotation: `generate_instructions_annotated` emits, per outcome branch,
the ordered PS-form draws. `replicate_select` walks the draw positions, consumes the real
`PsPrng` with each branch's `(kind,args)`, and keeps the branches whose realized value selects
them — narrowing to the single realized outcome (exactly the branch PS's stream dictates).
Handled draw types: `randomChance` (0/1), `random(16)` damage roll (index-exact, `HitCombos`
enumerates all 16), duration `random(m,n)`, `sample` (index), and binary `random(100)`
secondary/flinch/self-drop (the engine annotates proc=0 / noproc=chance — the filter is made
threshold-aware: proc iff `drawn < chance`). Move-order **Speed ties** are resolved by peeking
PS's `commitChoices` `shuffle[2,0,2]` bit (`random(0,2)`) and forcing that order via a new
`Exec`-adjacent thread-local (`set_forced_tie_order` / `move_order_tie` in `generate.rs`); the
generation still emits+consumes the shuffle draw, so the stream stays aligned. This took the gate
8 → 21 games (the threshold-secondary fix alone was 8 → 21; move-order forcing collapses the
ambiguous shuffle forks that are genuine move-order ties).

### First-divergence burn-down queue (the new work list, ranked by games blocked)
| games | class | analysis |
|------|-------|----------|
| 23 | `rust-extra randomChance[1,24]@crit` | **Multi-hit KO early-termination**: PS stops a multi-hit move when the target faints (or a Substitute breaks); the engine's folded hit path rolls crit+damage for all N hits (`times_hit engine=2 ps=1`). The engine over-draws → desync. Needs the hit loop to terminate on KO in the realized (Replicate) path. (Deferred-class "multi-hit multiplicity".) |
| 23 | `draws-match/state-diff` | Filter value mis-selection: the draw SHAPE matches PS but the selected branch's state is off (small HP), i.e. a `random(100)`-family site whose proc/noproc encoding isn't the `{0,chance}` convention the threshold rule assumes (effect-spore d100, multi-way splits), or a residual-ordering placement. Per-site audit of the annotation `result` encoding. |
| 4 | `PS shuffle[2,0,2]@generic` | **Deferred `eachEvent('Update')` Speed-tie sites** the annotation still omits (status-move 970 on a realized moveHit, switch/tera runSwitch brackets). PS shuffles where the engine's next draw is accuracy → the engine reads the shuffle's `random(0,2)` as its accuracy roll → shifted stream. |
| ~10 (1 ea) | `args randomChance@<move>` | **Accuracy-arg modifiers** not yet integer-exact: Compound Eyes / Wide Lens / evasion-stage (needs the 4096 chain), confusion self-hit `[33,100]`, Beat Up per-hit `[1,24]` alignment. The engine emits raw accuracy; PS's is post-`ModifyAccuracy`. |
| 3 | `PS-unconsumed sample@whirlwind` | **Drag-target `sample`** (Whirlwind / Roar / Dragon Tail random replacement) the engine picks deterministically without a draw. |
| few | `PS random[2,5]@slp` / `sample@bulletseed` | Residual sleep-duration ordering placement; variable multi-hit COUNT `sample` (folded into the DP). |

### Post-P3.3 first-divergence spectrum (38 non-exact games, ranked by root cause)
| games | class | root cause (analyzed) |
|------|-------|------------------------|
| 38 | `draws-match/state-diff` | Catch-all: draw SHAPES match PS but state diverges — an **upstream PRNG offset** from an unmodeled draw earlier in the game shifts the damage-roll index. Dominant source is unmodeled `eachEvent('Update')` / handler-order `speedSort` shuffles on **tied-Speed** boards (see below); a residual tail is per-move quirks (Diamond Storm rolls `random(100)` **twice** for its `self`+`secondary`; sleep-duration `random(2,5)` placement vs a same-turn drag `sample`). Each fix advances a game to its *next* divergence, so this bucket both drains and refills. |
| ~11 | `PS shuffle@<ctx>` / `PS-unconsumed shuffle` | **The keystone — unmodeled speedSort shuffles.** Ground-truthed against the pinned PS `eachEvent` trace: on a tied-Speed board PS fires `shuffle[2,0,2]` at (a) each `eachEvent('Update')` during a **status move's** moveHit (Calm Mind mirror = 2 Updates: battle-actions.ts:970 per-hit + the runAction 2882 post-move), and (b) **handler-order** `speedSort` of equal-(order,priority,speed,subOrder) event-handler lists mid-move (`shuffle[4,0,2]`, `[4,2,4]`, … — e.g. both mons' Leftovers/ability handlers for one event). (b) needs PS's per-event handler enumeration + comparator keys — the charter's flagged "largest unknown". These desync the **commitChoices tie bit** the move-order forcing peeks, so they also masquerade as move-order mismatches in tied games (t2: shadowball-first vs PS calmmind-first). |
| 4 | `rust-extra randomChance@accuracy` | Engine emits an accuracy roll where PS makes none: Mirror Coat/Counter **fail before the accuracy step** (no qualifying damage taken → `onTry` fails), and Thunder-Wave-family immunity/paralysis blocks evaluated against the wrong (pre-switch) target. Per-move `reaches_accuracy` refinement. |
| ~9 (1 ea) | `args randomChance@<move>` | **Accuracy-arg 4096-modifier chain** not integer-exact: confusion self-hit `[33,100]`, Beat Up per-hit `[1,24]`, evasion/accuracy stages, Compound Eyes / Wide Lens. Engine emits raw accuracy; PS's is post-`ModifyAccuracy`. |
| 4 | `PS-unconsumed sample@<move>` / `PS random@<move>` | Variable multi-hit COUNT `sample([2,2,…,5])` + per-hit crit/damage in the realized loop (Icicle Spear / Bullet Seed / Tail Slap — the DP path emits no per-hit stream); Tri Attack status `random(100)` split; Dire Claw / sludgewave secondary count. |
| 1 | `move-order-tie` | Genuine unfilterable shuffle fork (both order-branches share an identical draw stream). |

**Kill-criteria status: NOT triggered.** Density is still decaying — the four landed classes each
*removed* a class (multi-hit `rust-extra crit`: 23→0; accuracy hit/miss folded into the filter;
`sample@whirlwind`: 3→0; switch Update: the Move+Switch case). No fix revealed a NEW independent
class, and no blocker needs unobservable state — the keystone (speedSort) is a KNOWN, finite,
bug-for-bug-modelable draw-accounting item (PS is deterministic-by-seed; the recorder even labels
every shuffle's dispatching event + tying-handler group). The remaining climb to ≥60 is gated on
that speedSort modeling (status-move Updates are ground-truthed and mechanical; handler-order ties
need PS's per-event handler lists) plus a decaying tail of per-move draw-count quirks.

### Regression rail (all green — Enumerate/Sample untouched)
- `cargo test --release -p engine`: all suites pass (the `FORCED_TIE_ORDER` thread-local defaults
  `None`, and every P3.1–P3.3 draw site is `annotating()`-gated, so Enumerate/Sample paths are
  byte-identical).
- Corpus state-sweep: **3831/3831 matched, 0 diverged, 0 unsupported, 100.00%**.
- Distribution smoke: **18/18** (run sequentially — one node process at a time).
- Per-game SEED_GATE monotone non-decreasing 21→36→37→38 with **zero** prior-exact game regressing
  (exact-list diff vs the pre-change binary at every step).

---

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
| 2026-07-23 | **Residual handler-order `speedSort` shuffles** (the `shuffle[N,N-2,N]@generic` end-of-turn tail-ties: `shuffle[4,2,4]`/`[3,1,3]`/`[5,3,5]` — 88+47+35 u — plus the Residual half of `shuffle[2,0,2]`) | 82.28% | 86.19% | +3.91 (+150 u) | PS `fieldEvent('Residual')` (battle.ts:507) `speedSort`s the collected residual handlers; every tie-group of ≥2 handlers equal under `comparePriority` consumes one `prng.shuffle(list, sorted, sorted+len)`. For the Residual event priority & effectOrder are 0 for all handlers, so a tie ⟺ equal (order, speed, subOrder). New `residual_handlers()` rebuilds PS's list from board state with per-effect (order, subOrder) keys from data at the pin (label-audit-verified: weather 1/5, terrain-field 27/7 + Grassy-per-active 5/2, trick-room 27/1, screens 26/{reflect 1, lightscreen 2, tailwind 5, auroraveil 10}, leftovers/black-sludge 5/4, orbs/sticky-barb 28/3, speedboost/baddreams/harvest/cudchew 28/2, hungerswitch 29/7, wish 4/3, psn/tox 9/0, brn 10/0, ordered volatiles ingrain 7…roost 25 all subOrder 2, protect+stall at order `false`); `emit_residual_shuffles()` is a selection-sort mirror of `comparePriority` emitting one shuffle per tie-group, called (annotation-only) at the top of `apply_end_of_turn`. Verified zero false-positive emissions (no rust-side shuffle mismatch anywhere in the corpus). |
| 2026-07-23 | **crit roll vs a crit-immune target** (`randomChance[1,24]@bodypress`/`@thunderbolt`/… — crit-immune first-mismatch class 23→1) | 86.19% | 86.43% | +0.24 (+9 u) | PS rolls `randomChance(1, critMult[critRatio])` (battle-actions.ts:1645) whenever `willCrit === undefined` and the crit stage ≥ 1 — INDEPENDENT of target crit-immunity (Battle Armor / Shell Armor / Lucky Chant hook `CriticalHit`, which only downgrades the *result* AFTER the roll). The engine set `crit_p = 0` for such targets and skipped the draw. New `ps_crit_den()` computes PS's exact call denominator ignoring the immunity short-circuit (0 only for always-crit `willCrit=true` moves); `annotate_hits` now emits the forced-no-crit roll. Draw-and-discard — `result` isn't compared, state validates. (Remaining bodypress/beatup now fail at a downstream draw; multi-hit Beat Up rolls in the sumset-DP path, not `annotate_hits`.) |
| 2026-07-23 | **Trace switch-in ability sample** (`sample[1]@trace` — first-mismatch class 11→below-top-20) | 86.43% | **86.64%** | +0.21 (+8 u) | PS Trace `onStart`→`onUpdate` (abilities.ts) picks its copy target via `this.sample(possibleTargets)` — the traceable adjacent foes; in singles that list is length 1 → one `sample[1]` draw before the copy. The engine copied the ability deterministically without the draw. Emit `sample[1]` at the Trace-activates gate in `apply_switch_in_ability` (draw-and-discard; state validates). Covers the immediate switch-in copy; the deferred case (Trace's `onUpdate` re-firing mid-turn when a traceable foe appears only *after* the Trace mon entered against an untraceable/absent foe) stays unmodeled — emitting only when the foe is traceable at switch-in exactly matches when PS samples then, so no false positives (verified: zero `rust extra sample@trace`). |
| 2026-07-23 | **100%-secondary target volatiles** (`random[100]@saltcure` / `@psychicnoise` / `@throatchop`; also sparklingaria / syrupbomb / spiritshackle — all cleared from the queue) | 86.64% | 87.37% | +0.73 (+28 u) | PS models Salt Cure (`saltcure`), Psychic Noise (`healblock`), Throat Chop, Sparkling Aria, Syrup Bomb and Spirit Shackle as `secondary:{chance:100, …}`, so `secondaries()` rolls one `random(100)` per hit (draw-and-discard — the effect always lands). The engine realizes those effects through `target_volatile` / dedicated on-hit handlers with `secondary_chance == 0`, so it emitted no roll. New `extra_secondary_roll_move()` gate at the top of `apply_target_secondary` emits the `random(100)` at the secondaries site, respecting the same strips as any target-facing secondary (Shield Dust / Covert Cloak remove it → no draw; Sheer Force removes it → no draw; a fainted target still rolls). |
| 2026-07-23 | **Harvest end-of-turn roll** (`randomChance[1,2]@harvest` — class 19→0) | 87.37% | — | (in +45 batch) | PS Harvest `onResidual` (order 28) runs for **every** living Harvest holder each end of turn: `if (sun \|\| randomChance(1,2)) { if (!item && lastItem isBerry) restore }`. The `randomChance(1,2)` fires whenever it's **not** sunny — INDEPENDENT of whether a berry can actually be restored (the restore short-circuits *inside* the guard). The engine had rolled only when it was about to restore a berry, so most turns (holder still holding its item, or no consumed berry) emitted nothing. Rewrote the Harvest block: emit the roll for any living non-sunny Harvest holder (single draw-and-discard branch when no berry can be restored, 50/50 split when it can); sun still short-circuits with no draw. State branches unchanged (Enumerate/Sample untouched). |
| 2026-07-23 | **Cursed Body disable roll** (`randomChance[3,10]@cursedbody` — class 14→0) | — | — | (in +45 batch) | PS Cursed Body `onDamagingHit` rolls `randomChance(3, 10)` whenever the holder is hit by a non-Struggle damaging move and the SOURCE isn't already Disabled — the roll fires even when the source can't be disabled (it fainted from the hit; the `.fainted` flag isn't set until after the hit resolves, so the source is still present at the DamagingHit event). The engine skipped the draw whenever it couldn't apply the disable. New structure: `roll_fires` (foe CursedBody, non-Struggle, source not Disabled) gates the draw; when it can't land (source fainted / lacks the move) emit one draw-and-discard branch, else split proc/no-proc with the draw on each. |
| 2026-07-23 | **Octolock status accuracy** (`randomChance[100,100]@octolock` — class 7→0) | — | — | (in +45 batch) | Octolock is a foe-targeting numeric-accuracy (100) status move, but it is special-cased above the general status-accuracy branch in `execute_status_move` and returned early with no accuracy roll. PS `hitStepAccuracy` rolls `randomChance(100,100)` — after `hitStepTryImmunity`, so a Ghost target (immune to `trapped`) fails first and never rolls. Emit the draw in the Octolock block gated on `alive && !ghost`. |
| 2026-07-23 | **Partial-trap duration** (`random[5,7]@infestation` / Fire Spin / Bind / … — class 7→0) | — | **88.54%** | +1.17 (+45 u batch) | PS `partiallytrapped` onStart rolls `this.random(5, 7)` for the 5–6-turn duration — unless Grip Claw fixes it at 8 (no roll); Binding Band changes only the chip divisor and still rolls. The engine branched the duration (5,0.5)/(6,0.5) but emitted no draw. Emit `random(5,7)` on the non-Grip-Claw branches in `apply_partial_trap` (draw-and-discard; state carries the realized turns). |
| 2026-07-23 | **Accuracy draw arg = modified accuracy** (`randomChance[80,100]@closecombat`/`@quickattack` — Hustle; `randomChance[50,100]@hurricane` — sun; classes 11+9+8→0) | 88.54% | 89.32% | +0.78 (+30 u) | PS `hitStepAccuracy` rolls `randomChance(accuracy, 100)` with accuracy AFTER `onModifyMove`/`ModifyAccuracy`; the engine emitted raw `md.accuracy`. New `accuracy_arg()` models the two integer-exact modifiers in the corpus: Hustle (physical ×0.8) and sun-halved Thunder/Hurricane (=50). Compound Eyes / Wide Lens and accuracy/evasion STAGE modifiers stay raw (need the foe evasion boost + 4096 chain rounding). Annotation-only (called only under `annotating()`; accuracy_of / hit-miss split untouched). |
| 2026-07-23 | **Effect Spore contact roll** (`random[100]@effectspore` — class 6→0) | 89.32% | 89.42% | +0.10 (+incl. downstream) | PS Effect Spore `onDamagingHit` rolls one `this.random(100)` on any contact hit — `<11` slp, `<21` par, `<30` psn, else nothing — gated only on `checkMoveMakesContact && source.runStatusImmunity('powder')` (a Grass / Overcoat / Safety Goggles attacker never rolls). The engine branched the outcomes but emitted no draw. Emit one `random(100)` on every branch out of the ability (draw-and-discard). |
| 2026-07-23 | **Update bracket — turn-start + in-kernel damaging per-hit/post-hit-loop + runAction/residual** (the `eachEvent('Update')` Speed-tie `shuffle[2,0,2]` schedule; `shuffle[2,0,2]@generic` 136→91 first-mismatch) | 89.53% | **90.16%** | +0.63 (+24 u) | PS fires `eachEvent('Update')` — `speedSort(getAllActive(), (a,b)=>b.speed-a.speed)`, a `shuffle[2,0,2]` iff both actives are on-field and share `effective_speed` — at seven sites per turn. LANDED sites (all annotation-only, gated on the equal-Speed predicate evaluated on the CURRENT board): (1) turn-start bracket in `generate_branches_ctx` — commitChoices `queue.sort()` (`shuffle[2,0,2]`, full move tie / equal-outgoing-speed switch tie), `eachEvent('BeforeTurn')` + runAction Update (both on speed tie), and the gen8 dynamic re-sort `shuffle[3,0,2]` (both-move full tie, len-3 `[move,move,residual]` queue); (2) in-kernel per-hit `Update` (battle-actions.ts:970) + post-hit-loop `Update` (:1024) on each connecting damaging hit's branch, in PS order (after self-drops/secondaries/DamagingHit) — 970 on the PRE-faint board (a 0-HP-this-hit target still shuffles), 1024 alive-gated (a KO'd target breaks the tie); (3) runAction Update (battle.ts:2882) after every move action via `run_move_action` (hit/miss/cancel/flinch) and after the `residual` action. `move_order`'s enumerated Speed-tie now inherits the commitChoices shuffle from the turn-start bracket (the old per-branch `@speed-tie` draw was removed — no double-emit). Over-emission checked zero (exact count is the reliable signal; no rust-side shuffle anywhere). DEFERRED (documented, over-fire without an exact predicate): the status-move 970 (PS skips `moveHit`/970 when a move fails — immune foe target, Recover at full HP, boost at cap — indistinguishable from the engine's post-effect "hit" branch; a blanket emit measured net-negative), multi-hit 970 multiplicity (the folded exact/DP hit path emits one 970 not N), and the switch/tera runSwitch brackets (`runSwitch getAllActive` speedSort + switch-out/in Updates). |
| 2026-07-23 | **Defrost-move self-thaw** (`randomChance[1,5]@frz` rust-extra, defrost half) | 89.42% | 89.53% | +0.11 (+8 u) | A `defrost`-flagged move (Flame Wheel, Scald, Sacred Fire, Pyro Ball, Hydro Steam, …) thaws its frozen user with NO `randomChance(1,5)` roll (PS frz `onBeforeMove` returns early on `move.flags['defrost']`). The engine modelled no defrost flag, rolling 80/20 for every frozen mover — the no-thaw branch and its draw were both spurious. `is_defrost_move()` + a deterministic-thaw path in the freeze branch of `execute_move`. STATE change (removes the spurious branch); corpus state-sweep 100% + smoke 18/18 re-verified. The 7 remaining `frz` rust-extra are a same-turn ice-secondary-freeze decision-boundary artifact (a mon frozen by the opponent's move this turn, then attempting to act), NOT defrost — left documented. |

## Burn-down summary
56.98% → **90.16%** over 21 committed classes (+33.18 pts, +1271 units). Fix-decay curve is
healthy: each class landed a real, structured, decaying slice (207, 282, 132, 206, 62, 19,
29, 32, 150, 9, 8, 28, 45-unit harvest/cursed-body/octolock/partial-trap batch, 30, 8, 8, 24) with
**no** new independent mismatch class revealed per fix beyond the known queue — kill-criterion #2
(non-decaying density) is **not** triggered. The 21st class is the `eachEvent('Update')` Speed-tie
Update bracket: the turn-start sites (commitChoices/BeforeTurn/runAction/dynamic-resort), the
in-kernel damaging per-hit (970) + post-hit-loop (1024) Updates, and the runAction (2882) /
post-residual Updates all landed and turned the common equal-Speed damaging turn exact; the 970
went in exactly where the move kernel's per-hit resolution completes (position-verified against the
label audit — c1 d46/d47: `[commit,BeforeTurn,runAction][3,0,2]` then `[acc,crit,dmg,sec] 970 1024
2882` per move then `residual 2882`). All draw-*accounting* (annotation-only, gated behind
`annotating()`); Enumerate/Sample byte-unchanged, so state-sweep + smoke are invariant and stayed
green. None required unobservable state (kill-criterion #1 not triggered): the Update tie predicate
is `effective_speed`-equality on the current board, fully reconstructable. Remaining Update units
(status-move 970, multi-hit 970 multiplicity, switch/tera runSwitch brackets) are documented
deferrals — each needs a signal the post-effect branch model doesn't yet expose (did-`moveHit`-run,
per-hit count, per-switch on-field liveness), NOT hidden state.

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

#### COMPLETE per-turn schedule (fully reverse-engineered 2026-07-23 from the label audit)
Let `T` = emit `shuffle[2,0,2]` iff both actives alive AND `effective_speed` equal, evaluated on
**current** state at that moment (speeds change mid-turn: switches, stat drops from the moves
themselves). PS walks the sorted action queue `[beforeTurn(4), switch(103), tera(106), move(200),
residual(300)]` (battle.ts:2971 turnLoop); after EACH action `runAction` fires `eachEvent('Update')`
(battle.ts:2882, gen≥5, choice≠'start'). The exact draw sequence for a fully-tied turn, verified
against the audit (c1 d46/d47, d20/d45/d51, t1):

1. **commitChoices initial `queue.sort()`** (battle.ts:3039 → `speedSort(list)`, eventid=`null`):
   the two committed actions. Two moves (both order 200) tie → `shuffle[2,0,2]`; two switches (order
   103, keyed on the OUTGOING mon's speed) tie → `shuffle[2,0,2]`; a move+switch never ties (orders
   differ). *(The engine already emits this as `shuffle[2,0,2]@speed-tie` at the resolve_moves
   Enumerate tie — line ~1715 — which is why move/move pos-0 already matches.)*
2. **beforeTurn action**: `eachEvent('BeforeTurn')` (battle.ts:2830) → `T`; then runAction Update
   (2882) → `T`; then, only if the NEXT action is a move (peek=='move'), gen8 dynamic-speed re-sort
   (battle.ts:2940-2946 `queue.sort()`, eventid=`null`) over the remaining queue → for two tied
   moves emits `shuffle[len,0,2]` where `len` = remaining-queue length = **3** for move/move (the
   two moves + residual) → this is the `shuffle[3,0,2]` action-queue off-by-one (task item #2).
3. **each switch action** (`runSwitch`, battle-actions.ts): switch-out `eachEvent('Update')`
   (battle-actions.ts:83) → `T` (evaluated on the OLD actives, only if the outgoing mon is unfainted);
   then the mon swaps in + switch-in ability fires (may change speed/faint); runAction Update (2882)
   → `T`; then a `runSwitch` action's `getAllActive()` `speedSort` (battle-actions.ts:182, eventid=
   `null`) → `T`; then runAction Update after runSwitch → `T`. Observed triple for one switch with
   old-actives-not-tied: `[Update, null, Update]` (c1 d20). runSwitch batches consecutive switch-ins
   (double switch → one shared runSwitch speedSort, c1 d45).
4. **each move action** (execute_move): the move's own draws (acc/crit/damage/secondary/contact),
   then per-hit `eachEvent('Update')` (battle-actions.ts:970, **once per connecting hit**, fires even
   on a KO because the target's `.fainted` flag isn't set until faintMessages at :979 — so its tie is
   the PRE-faint-message liveness) → `T`; then post-hit-loop `eachEvent('Update')` (battle-actions.ts:
   1024, once, only if the move dealt damage — target now fainted, so a KO gives NO shuffle here) →
   `T`; then runAction Update (2882) → `T`. A status move that connects fires only the per-hit (970)
   + runAction (2882) Updates (no 1024). A miss / immunity / no-target move fires only runAction (2882).
5. **residual action**: `fieldEvent('Residual')` shuffles (already LANDED via emit_residual_shuffles),
   then runAction Update (2882) → `T`. This is the trailing `[…, Residual, Update]`.

**Why this needs in-kernel emission (not an external post-hoc injection):** the 970/1024 Updates fall
between the move's damage/secondary/contact draws and any afterMoveSecondary contact procs, and their
tie predicate depends on connect/damage/hit-count/faint state known only inside `execute_move`. An
external "emit N trailing Updates after execute_move" approximation cannot know whether a move
connected, how many hits, or whether it KO'd (which flips the 1024/2882 shuffles off but leaves 970
on). So the faithful build emits at the actual sites: (a) turn-start brackets in `generate_branches_ctx`
before/around the switch+move resolution [items 1–3], (b) the per-hit 970 Update at the engine's
per-hit completion point and the 1024 Update at the move's post-damage point inside the move kernel
[item 4], (c) the runAction 2882 Update after each execute_move and after end-of-turn [items 2–5].
Safety rail (as with Residual): only ever emit a `T` shuffle where PS definitely emits one — the
differ's "no rust-side shuffle mismatch anywhere" invariant means over-emission would immediately
show as a new mismatch, so build incrementally per turn-shape (move/move first — the common case —
then switch-involved, then multi-hit) checking the differ after each.

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

## Gates (all green, 2026-07-23 after the Update-bracket class → 90.16%)
- `cargo test --release -p engine`: all suites pass (12 main + 29-fixture + psprng raw gate +
  trapping/generate/etc.). The Update-bracket changes are annotation-only, so no test drifted.
- Distribution smoke (17 diverse seeds + request-boundary randbattle seed 90, run sequentially —
  one node at a time): **18/18**. Unchanged: the new draws live behind `annotating()`, so the
  Enumerate/Sample distribution paths the smoke exercises are byte-identical.
- Full corpus state-verify (`target/release/cosim harness/cosim-traces/*.json.gz`): **3053/3053
  matched, 0 diverged, 0 unsupported** (move-turn units; the differ reports the same corpus as
  3831/3831 supported). Annotation-only changes → state unchanged.
- Draw-consumption differ: **3454/3831 = 90.16% draw-exact** (from 89.53% at the start of this
  session; `shuffle[2,0,2]@generic` first-mismatch 136→91), no rust-side shuffle over-emission
  anywhere in the corpus (verified via the exact-count invariant, not just first-mismatch labels).
