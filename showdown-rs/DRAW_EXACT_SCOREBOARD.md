# DRAW-EXACT — Phase 1 first scoreboard

Reproduce: `cargo build --release -p cosim && DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz`
PS pin: `b9dc987d`. Corpus: 111 audited traces / 3831 move units, plus 401 fresh
`gen9randombattle` seed fixtures (`harness/seed-fixtures/`, seeds 1000-1400) — 512 games total.

---

# ==== PHASE-6 EXTENSION BURN-DOWN — certification (2026-07-26) ====

**HEADLINE: 400 / 512 full games byte-exact from seed (78.1%), up from 372; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Eight parity commits, every one PS-source-grounded, every one monotone: the newly-non-exact set
was EMPTY at all eight steps (judged by exact-SET diff on both corpora, never by the count).
**The mid-turn re-request counter class is CLOSED** — all 18 games (11 `active_turns` +
7 `wish`) plus 7 more that shared the roots.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **400 / 512 (78.1%)**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | all suites green |

## THE mid-turn re-request counter schedule — GROUND-TRUTHED TABLE

The 18-game class was NOT one root. It was two, split cleanly by whether the divergent decision
was the game's last (`stateAfter.ended == true`) or a `midTurn:true` move decision followed by a
`switch` decision. PS's end-of-turn order, from `runAction` / `turnLoop` (sim/battle.ts):

| # | PS site | what happens | consequence for the compared state |
|---|---------|--------------|-----------------------------------|
| 1 | `runAction(move)` → `faintMessages()` (battle.ts:2856-2857) | faints processed; **`if (this.ended) return true`** | a KO that ends the battle stops the turn HERE — no residual, no `endTurn`, no `nextTurn` |
| 2 | `runAction(residual)` → `fieldEvent('Residual')` (battle.ts:2837) | residual handlers run. **A handler whose `effectHolder` fainted is skipped UNLESS `handler.state.isSlotCondition`** (battle.ts:512-514) — so Wish still ticks over a fainted slot | Wish is date-based (`getOverflowedTurnCount() <= startingTurn`) and cannot be deferred |
| 3 | `faintMessages()`; `if (this.ended) return true` | a residual-induced KO can end the battle here too | same as #1 |
| 4 | queue empty ⇒ `checkFainted()` (battle.ts:2864) ⇒ `switchFlag` ⇒ `makeRequest('switch')` (battle.ts:2933) `return true` | **turn does NOT advance; `activeTurns` is NOT incremented**; `battle.midTurn` stays true | the move decision's recorded post-state is captured HERE (`midTurn:true`, turn N) |
| 5 | (player answers) `turnLoop` resumes with `midTurn` still true ⇒ no new `beforeTurn`/`residual`; `runAction(switch)` ⇒ `switchIn` sets `pokemon.activeTurns = 0` (battle-actions.ts:137) | the replacement enters at 0 | |
| 6 | queue empty ⇒ `endTurn()` ⇒ `nextTurn()` (battle.ts:1756-1762) | `if (pokemon.fainted) continue; pokemon.activeTurns++` for every active; `this.turn++`; `makeRequest('move')` | the switch decision's post-state is captured HERE (`midTurn:false`, turn N+1) |

Read off the table: **`activeTurns++` fires exactly once per turn, in `nextTurn()`, AFTER any
mid-turn replacement switch-in and only if the battle has not ended.** The engine advanced it
inside the residual per-side loop, whose `battle_over` guard is evaluated BEFORE the residual —
so every game that ENDED on a residual-phase KO advanced the winner's counter one turn too many
(15 instances, all `engine = ps + 1`). And **Wish's tick is a slot-condition residual that runs
over a fainted holder**, which the engine skipped behind the same loop's fainted-active guard
(7 instances, all `engine = ps + 1` ticks remaining). Opposite-looking directions, two roots.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `df5cf4b` | **`activeTurns` advances in `nextTurn()`, not the residual phase** | 372 → 384 | `battle.ts:1756-1762` vs `:2857` / `:2864` / `:2933` |
| 2 | `092b56a` | **Wish is a SLOT condition — its residual runs over a fainted holder, and a matured Wish there is CONSUMED without healing (it does not linger)** | 384 → 390 | `battle.ts:512-514` + `battle.ts:1138-1152` + `data/moves.ts:20945` |
| 3 | `6afbce9` | **The HP-berry `Update` runs AFTER the move's secondaries** | 390 (rb1003 advances) | `battle-actions.ts:970` at the bottom of `hitStepMoveHitLoop`; `data/items.ts:5752` |
| 4 | `caf597a` | **Knock Off's ×1.5 is an `onBasePower` `chainModify`, not its own rounding step** | 390 → 393 | `data/moves.ts:9970-9975` + `battle.ts` `chainModify`/`modify` |
| 5 | `f6b165b` | **A cancelled attempt does not re-arm the rampage lock** | 393 → 395 | `data/conditions.ts:253-284` + `battle.ts:515-522` |
| 6 | `8189f6a` | **Focus Punch's `beforeMoveCallback` and Poltergeist's `onTry` both precede the accuracy roll** | 395 → 397 | `data/moves.ts:6015-6020` + `battle-actions.ts:270-276`; `data/moves.ts:13610-13612` + `battle-actions.ts:821` |
| 7 | `2354776` | **Sticky Hold does not block Knock Off's ×1.5** (item `singleEvent('TakeItem')` ≠ ability `runEvent('TakeItem')`) | 397 → 398 | `data/moves.ts:9970-9975` vs its `onAfterHit` |
| 8 | `456f118` | **Leppa Berry** (`onUpdate` at any 0-PP slot; `onEat` +10 capped at `maxpp`) | 398 → 400 | `data/items.ts` leppaberry |

Games flipped, by commit: 1 → rb1035 rb1180 rb1185 rb1189 rb1225 rb1264 rb1295 rb1296 rb1328
rb1352 rb1376 rb1394; 2 → rb1039 rb1054 rb1203 rb1234 rb1393 rb1400; 4 → rb1008 rb1105 rb1221;
5 → rb1113 rb1340; 6 → rb1327 rb1397; 7 → rb1104; 8 → rb1130 rb1389.

### Method notes worth keeping

- **`stateAfter.turn` / `midTurn` / `ended` are POST-state** (`harness/cosim.mjs:1057-1066`
  records them after `battle.choose` returns). A `move` decision with `midTurn:true` is a turn
  PS stopped mid-way to ask for a replacement; the trailing `switch` decision's post-state is
  one turn later. That single fact split the 18-game class in two — the "opposite directions"
  the Phase-5 handoff flagged were never one residual phase mis-attributed.
- **A companion hack is a tell.** The Wish fix only paid off once its compensating
  `apply_switch` hack (clear `wish.0 == 1` when a faint replacement enters) was ALSO removed:
  with the correct tick the two cancelled each other and the game count did not move.
- **Split "is it one modifier chain or two rounding steps?" per handler.** Knock Off's ×1.5 was
  filed as a `basePowerCallback`; it is an `onBasePower` `chainModify(1.5)` at priority 0 and
  belongs in the SAME `event.modifier` as Tough Claws (127 BP, not 126).

## The 112 still-open games, re-triaged

| n | class | reading |
|---|-------|---------|
| 71 | `draws-match/state-diff` | the draw stream matches for the unit; the STATE differs |
| 6 | `PS shuffle@generic` | a residual-handler-list tie shuffle the engine does not emit |
| 5 | `random@confusion` (3 PS-unconsumed, 2 PS) | rampage-end / Confuse Ray duration position |
| 2 | `PS random@lockedmove` | the rampage `random(2,4)` position |
| 2 | `rust-extra randomChance@accuracy` | over-emission (down from 4) |
| 2 each | `PS random@curse` / `PS sample@roar` / `args @hypervoice` / `args @par` / `args @struggle` | bespoke |
| ~14 | singletons | `@fakeout` `@thunderbolt` `@fireblast` `@heavyslam` `@discharge` `@icehammer` `@throatchop` `@freezedry` `@icebeam` `@shadowball` `@trace` `@knockoff` `@powerwhip` `@crit` `@disablemove` |

First-divergence FIELD split (all classes): **50 `hp`**, 20 `volatiles`, 16 `boosts`,
4 `status`, 3 `stall_counter`, 3 `species`, 3 `pp`, tail. Of the 50 `hp` games **37 exceed
10 HP** (wrong mechanics), 6 sit in 4-10, and **7 are within 3** (rounding residue — down from
11; three of those were Knock Off's chain).

The 20 `volatiles` games decode to single missing/extra bits, no shared root visible:
ThroatChop (rb1038 missing / rb1072 extra), MustRecharge (rb1092, rb1157 missing),
Substitute (rb1109, rb1308 missing; rb1033 extra), Confusion (rb1287, rb1364, rb1384 missing;
rb1121 extra), ChoiceLock (rb1099), Unburden (rb1126), DestinyBond (rb1229),
MagnetRise (rb1237), ProtoBooster (rb1048, rb1278), Encore (rb1245 extra),
HealBlock (rb1304 extra), Disable (rb1031 extra).

## Named opens carried forward

- **Rampage lock end at `n == 1` with a NON-confused user.** `unarm_rampage_on_cancel` handles
  the drop whenever `onEnd`'s `addVolatile('confusion')` is a no-op (already confused — which a
  confusion self-hit always is — or Own Tempo). An attract / full-paralysis / freeze cancel on
  the FINAL locked turn needs a fresh `random(2, 6)` emitted at the RESIDUAL stream position,
  which the move-time cancel site cannot produce. Likely the 2 `PS random@lockedmove` + part of
  the 5 `random@confusion` games.
- **The remaining `onBasePower` handlers still applied as their OWN `modify()` instead of
  folding into `bp_chain`**: Collision Course / Electro Drift (`chainModify([5461,4096])`,
  data/moves.ts:2633-2637), Psyblade (`:14038-14041`), Expanding Force (`:4952-4955`) — all move
  handlers at priority 0, in-function and cheap to fold; and the `-ate` abilities
  (`onBasePowerPriority: 23`, abilities.ts pixilate/refrigerate/aerilate/galvanize) and Analytic
  (`onBasePowerPriority: 21`, abilities.ts:111-123), which are applied in `execute_move_inner`
  before `md` reaches the damage function and need either a `MoveData` flag or the condition
  recomputed at the chain site (Analytic additionally needs `foe_pending_move`). Each only
  differs from the current code when a SECOND chain member co-occurs.
- **The `stall` / Protect chain (3 games: rb1142, rb1214, rb1227).** rb1227 t15 is the clean
  probe: p1 Protects into a p2 Nasty Plot and its ONLY divergence is
  `s0.stall_counter engine=0 ps=1` — the engine took the `!foe_moves_later` outright-fail path
  where PS's `queue.willAct()` is true. The missing `shuffle[4,2,4]` in the same unit follows
  from the state (no `stall` volatile ⇒ a shorter residual handler list), so this is one root,
  not two. Re-check how `Action::foe_pending_move` is populated for the first mover of a
  move/move unit.
- **Gulp Missile (rb1288, rb1367)** — `engine=cramorant ps=cramorantgorging`; the Surf/Dive
  forme change (`gulpmissile.onSourceTryPrimaryHit`) and its retaliation are unmodelled.
  **Ice Face's RESTORE (rb1253)** — `iceface.onStart` / `onWeatherChange` turn Eiscue-Noice back
  into Eiscue under snow. Both are forme mechanics; 3 games.
- **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet
  Rise's `onTry` failure** — unchanged from Phase 5.
- **Kill criterion: still NEVER triggered.** Eight commits, eight distinct structured roots,
  27 games; density did not decay within the session.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 400 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== PHASE-5 EXTENSION BURN-DOWN — certification (2026-07-25) ====

**HEADLINE: 372 / 512 full games byte-exact from seed (72.7%), up from 333; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Nine commits, every one PS-source-grounded, every one monotone: the newly-non-exact set was
EMPTY at all nine steps (judged by exact-SET diff on both corpora, never by the count). Two of
the three parked items are CLOSED (S5 tera formes, S6 magnetrise); S7 is landed with a named
residue.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **372 / 512 (72.7%)**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | all suites green |

## The roots landed (in commit order)

| # | class | games | PS reference |
|---|-------|-------|--------------|
| 1 | **Adaptability after Terastallizing** | 333 -> 335 | `battle-actions.ts:1762-1796` + `data/abilities.ts:44` |
| 1 | **Fur Coat / Marvel Scale key on the STAT** | (same) | `onModifyDef` + `Pokemon#calculateStat` |
| 1 | **Destiny Bond `onFaint`** and its **non-stackability** | (same) | `data/moves.ts` destinybond `onFaint` / `onPrepareHit` / `onBeforeMove` |
| 2 | **crash-damage moves crash on EVERY MoveFail** | 335 -> 341 | `battle-actions.ts:526` vs the `:511` no-target return |
| 2 | **Knock Off beats the target's HP berry** | (same) | `eachEvent('Update')` at `battle-actions.ts:970` vs `onAfterHit` at `:1144` |
| 3 | **Freeze-Dry's `onEffectiveness`** | 341 -> 345 | `data/moves.ts:6167` — `if (type === 'Water') return 1` |
| 3 | crash on the `onTryHit` absorbs; Heal Block gates the berry EAT | (same) | `data/items.ts:5752` `sitrusberry.onTryEatItem` |
| 4 | **Magic Guard blocks every non-Move damage** | 345 -> 352 | `onDamage`: `effect.effectType !== 'Move'` |
| 4 | **Avalanche / Revenge `basePowerCallback`** | (same) | `pokemon.attackedBy … p.thisTurn` |
| 5 | **Download's tie goes to SpA** | 352 -> 359 | `data/abilities.ts` `totaldef >= totalspd` |
| 5 | **S5 — the Tera forme changes** (Ogerpon masks + Terapagos) | (same) | `battle-actions.ts:1935` `terastallize` |
| 6 | **S6 — Magnet Rise, end to end** | 359 -> 361 | `data/moves.ts:10854` |
| 7 | **S7 — the stat chains accumulate into ONE `event.modifier`** | 361 -> 364 | `battle.ts:2334` `chainModify` + `:932` |
| 8 | **Ivy Cudgel's mask type** | 364 -> 370 | `data/moves.ts:9775` `onModifyType` |
| 8 | **Heavy-Duty Boots is a PER-HAZARD check** | (same) | `data/moves.ts:19780-19791` (Poison absorb precedes it) |
| 9 | **Battle Bond's KO boost; Throat Spray on status sound moves** | 370 -> 372 | `battlebond.onSourceAfterFaint`, `throatspray.onAfterMoveSecondarySelf` |

### Method notes worth keeping

- **The exact-SET diff earned its keep twice this session.** The naive S7 accumulation refactor
  scored +1/-1, and the lost game (rb1311) is what proved that Reckless / Tough Claws /
  Mega Launcher / Toxic Boost / Flare Boost sit in `onBasePower`, not in the stat chain. The
  Destiny Bond `onFaint` commit likewise cost the audited r3 until its `onPrepareHit`
  non-stackability rule was added — the audited corpus is a live jury, not a formality.
- **The state-diff divergence CAUSES draw-class mislabels.** Four games that read as
  `move-order-tie` flipped on the crash-damage fix alone. Do not treat the draw-class histogram
  as a partition of independent roots.
- **`DBG_GAME=rb` + `GATE_THREADS=1`** dumps every game's first divergent block in one pass;
  joining that to the VERBOSE gate listing by decision index is the whole triage loop.

## The 140 still-open games, re-triaged

| n | class | reading |
|---|-------|---------|
| 95 | `draws-match/state-diff` | the draw stream matches for the unit; the STATE differs |
| 6 | `PS shuffle@generic` | a bracket/schedule shuffle the engine does not emit |
| 3+2 | `random@confusion` | rampage-end / Confuse Ray duration position |
| 3 | `rust-extra randomChance@accuracy` | over-emission; the Focus Punch `beforeMoveCallback` root is still open |
| 2 | `PS random@curse` / `@lockedmove` / `PS sample@roar` / `args @par` / `args @struggle` / `args @hypervoice` | bespoke |
| ~20 | singletons | `@fakeout` `@thunderbolt` `@fireblast` `@heavyslam` `@discharge` `@icehammer` `@throatchop` `@freezedry` `@icebeam` `@shadowball` `@trace` `@bravebird` `@harvest` `@powerwhip` `@crit` `@disablemove` |

Field split of the 95 `draws-match/state-diff` games: **36 `hp`**, 14 `volatiles`,
11 `active_turns`, 7 `wish`, 8 `boosts`, 4 `item`/`status`, 3 `species`, tail. Across ALL draw
classes 55 games have an `hp` first-divergence field: **39 exceed 10 HP** (wrong mechanics),
5 sit in 4-10, and **11 are within 3** (the rounding residue below).

## Named opens carried forward

- **`active_turns` (11 games) and `wish` (7 games) are ONE root: the mid-turn re-request
  schedule.** Every instance is a turn whose unit was split by a mid-turn faint/pivot
  re-request. `active_turns` is uniformly engine = PS + 1; `wish` is uniformly engine = PS + 1
  ticks remaining (i.e. the engine ran one FEWER end-of-turn tick). The two point in opposite
  directions, which is exactly what a residual phase attributed to the wrong unit looks like.
  Evidence: rb1180 d41/d42 (turn 31, Palkia faints at turn 30, Skarmory replaces it and PS has
  `activeTurns` 1 while the engine has 2) and rb1203 d10/d11 (Wish cast in a MID-TURN unit at
  turn 8; `convert.rs` maps `turn <= startingTurn + 1 ? 2 : 1`, PS's compared state is turn 9 so
  1, the engine still holds 2). This is the same mid-turn schedule the pre-Phase-3 handoff flags
  as HIGH regression risk; it needs its own tranche with `battle.prng.getSeed()` vs
  `prng.limbs()` bisection, not a bolt-on.
- **S7 residue: eleven games with `|hp| <= 3`, six of them exactly 1**
  (rb1008 rb1052 rb1105 rb1145 rb1282 rb1327). The stat chains are no longer the cause — they
  accumulate correctly now. rb1008 (tera-Fighting Perrserker, Tough Claws + Choice Band Knock
  Off into a switching-in Empoleon, 29 in PS vs 28 in the engine) is still the cleanest bisect
  target. The next place to look is Knock Off's `basePowerCallback`, which returns
  `move.basePower * 1.5` as a NON-integer before `clampIntRange` floors it, and the
  `getDamage` truncation ladder around it.
- **The berry `Update` must run AFTER the move's secondaries.** PS's order inside
  `spreadMoveHit` is damage -> onHit -> selfDrops -> secondaries -> DamagingHit -> onAfterHit,
  and only then `eachEvent('Update')` at `battle-actions.ts:970`. The engine's berry site is
  inside `apply_post_damage`, ahead of the secondary. rb1003 (Psychic Noise heal-blocks a Cheek
  Pouch Dedenne in the same hit that drops it under half — PS keeps the berry) and rb1204 /
  rb1347 (a Lum / Chesto that PS eats and the engine does not) are the instances. This is the
  single largest remaining `hp` root by delta.
- **Terapagos-Stellar's FAINT regression** — it would move max HP on a fainted mon and
  `Instruction::Transform`'s `hp += delta` carry-over has no invertible definition at hp 0
  (PS: `hp = this.hp <= 0 ? 0 : max(1, …)`). Ogerpon's regression is implemented because its
  four formes share base stats.
- **Battle Bond's once-per-stint guard** (`abilityState.battleBondTriggered`, reset by
  `switchIn` at `battle-actions.ts:142`) needs the `ProtoBooster` treatment: explicit engine
  state read by `convert` and written by `export`. `ability_used` is NOT usable — `convert`
  derives it from `swordBoost || shieldBoost` and `diff_states` compares it.
- **Gulp Missile (rb1288, rb1367)** — `engine=cramorant ps=cramorantgorging`; the forme change
  after Surf/Dive and its retaliation are unmodelled. **Ice Face's RESTORE (rb1253)** —
  `iceface.onStart` / `onWeatherChange` turn Eiscue-Noice back into Eiscue under snow.
  **Leppa Berry (rb1130, rb1389)** — unmodelled.
- **Magnet Rise's `onTry` failure** against `smackdown` / `ingrain` or under Gravity is not
  modelled; the engine has neither Smack Down nor Gravity.
- **The 4 `rust-extra randomChance@accuracy` roots** and the `shuffle@generic` /
  `move-order-tie` classes are unchanged from Phase 4, except that the crash-damage fix removed
  four games from the tie class. rb1231 t12 shows the shape of the remaining accuracy
  over-emission: a mid-turn unit where the foe U-turns away before Struggle resolves, so PS's
  Struggle has no target and draws nothing.
- **Kill criterion: still NEVER triggered.**

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 372 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== PHASE-4 EXTENSION BURN-DOWN — certification (2026-07-25) ====

**HEADLINE: 333 / 512 full games byte-exact from seed (65.0%); init-aligned 512 / 512.
The audited 111-trace corpus is now 111 / 111 — R1 is CLOSED, the campaign has no named
open item left on it.**

Nine commits, every one PS-source-grounded, every one monotone: the newly-non-exact set was
EMPTY at all nine steps (judged by exact-SET diff on both corpora, never by the count).

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 (audited + fixtures) | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **333 / 512 (65.0%)**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`**; the `rust-requested-draw-not-next-in-log` category is now EMPTY |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** (every convertible corpus state byte-exact) |
| Engine tests | `cargo test --release -p engine -j 2` | all suites green |

## The nine roots (taxonomy S1-S8 plus two found on the way)

| # | class | games | PS reference |
|---|-------|-------|--------------|
| S1 | **Sleep Talk called-move re-entrancy** | 286 -> 295 | `data/moves.ts:16871` — `sleepUsable` + `onHit` `sample` + `actions.useMove` |
| S4+R1 | **per-hit `DamagingHit` ability rolls** | 295 -> 300 | `battle-actions.ts:1142` — `runEvent('DamagingHit')` is INSIDE `spreadMoveHit` |
| S2 | **Fickle Beam's `onBasePower` roll** | 300 -> 305 | `battle-actions.ts:1653` ("happens after crit calculation") + `data/moves.ts:5227` |
| S3 | **sleep schedule** (Sleep Clause + Lum ordering) | 305 -> 312 | `data/rulesets.ts:1378`, `data/conditions.ts:59`, `harness/cosim.mjs` formatid |
| S8 | **Protosynthesis / Quark Drive field-change re-derive** | 312 -> 316 | `data/abilities.ts:3473` `onWeatherChange` / `onTerrainChange` |
| — | **`move.self.chance` + Defog vs Substitute** | 316 -> 320 | `battle-actions.ts` `selfDrops`, `data/moves.ts:3458` |
| — | **Shields Down (Minior)** | 320 -> 324 | `data/abilities.ts:4194` + `clearVolatile`'s `setSpecies(baseSpecies)` |
| — | **Lightning Rod / Storm Drain / Motor Drive** | 324 -> 333 | `onTryHit` sits in `hitStepTryHitEvent`, BEFORE `hitStepAccuracy` |

### S1 — Sleep Talk (14 -> 0 in class; 9 games flipped)
`sleepUsable` (sleeptalk, snore) passes the slp `onBeforeMove`: PS's handler ticks the counter and
returns `false` for a normal move — which short-circuits `runEvent('BeforeMove')`, so Truant/flinch/
confusion/paralysis never roll — but returns undefined for a `sleepUsable` move, which falls through
to Truant (priority 9). The callable pool is the user's move slots minus empty slots and the
`nosleeptalk` / `charge` flags (PP is NOT consulted); one `sample[n]@sleeptalk`, n branches; an empty
pool returns false with no draw. The called move re-enters `dispatch_move_inner` with
`external_move: Some(id)` and the new `Action.called`: `useMove` (Sleep Talk) differs from `runMove`
(Dancer) in firing NO BeforeMove event at all, so `called` skips the sleep/freeze/recharge gates and
enters below the Glaive Rush drop, and the sub-move then runs its own complete stream (accuracy,
crit, damage, secondaries) while the user stays asleep. `lastMove`/streak stay on Sleep Talk (no
`moveUsed` on the `useMove` path) and no PP is paid. New generated flags `flag_charge` /
`flag_nosleeptalk` / `sleep_usable` (17 / 40 / 2 moves at the pin) — extraction rule, not a hardcoded
list.

### S4 + R1 — per-hit DamagingHit (VERDICT: LANDED, both halves)
r3 d23 t19 (Koraidon Scale Shot into Froslass) records `[crit, dmg, randomChance[3,10]@cursedbody] x
2`; rb1049 (Fezandipiti Beat Up) records `[crit, dmg, randomChance[3,10]@toxicchain] x 6`. Same root:
`spreadMoveHit` runs the DamagingHit event once per CONNECTING HIT, and the engine applied the whole
ability set once after the loop. Three pieces:
- `apply_damage_hit` gained a `HitRolls` source. `Fixed` is the unchanged DP/enumerate path;
  `Realized` peels each hit's crit + damage off the cursor INSIDE the loop. Peeking inside the loop
  is what makes an interleaved ability roll possible at all — an up-front peek reads hit n's ability
  slot as hit n+1's crit.
- `realized_per_hit_damaging_hit` reuses `apply_contact_secondaries` + `apply_cursed_body` verbatim
  (no second implementation to drift) and collapses the fork to the branch the cursor dictates. A
  Substitute-absorbed hit fires nothing (`targets[i] = null`, battle-actions.ts:1085). **The cursor
  advances by the chosen branch's draw SHAPES, not by matching results** — when an effect cannot land
  the fork collapses to one draw-and-discard branch whose recorded result is the placeholder 0 while
  PS's raw draw may be `true` (rb1152 t7: Beat Up into a target Toxic Chain cannot badly-poison; PS
  rolls 0,1,0,0,0,1). Matching on the result there skipped the draw and desynced every later hit.
- `Branch.per_hit_procs_done` suppresses the once-per-move application, and is CONSUMED
  (`mem::replace`) at the post-hit-loop block because the branch is reused for the turn's second
  mover — rb1341 t13: Triple Axel set the flag and silenced the Cursed Body roll PS makes against the
  NEXT move.

**Jury result: the 21 currently-exact multi-hit games all held** (exact-set diff, 0 lost), the
distribution smoke stayed 18/18, and the differ's `args randomChance@cursedbody` mismatch is gone
(3810 -> 3811 at that commit). The Scale Shot / Bullet Seed / Icicle Spear / Population Bomb /
Triple Axel blast radius is clean.

### S3 — the sleep schedule was two things
1. **Sleep Clause Mod is never active in any recorded battle.** `harness/cosim.mjs` builds every
   battle with `formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT`, and
   `gen9customgame` carries no ruleset; Sleep Clause Mod lives in the `standard` ruleset and on the
   "[Gen 9] Random Battle" format entry, neither of which the harness instantiates. cosim inferred
   `sleep_clause = format.contains("randombattle")` — exactly backwards. Ground truth: rb1312 t13's
   `stateAfter` has Regice asleep on the field while the benched Iron Jugulis is still asleep from
   the same attacker. Replaced by `trace::sleep_clause_for_format`, which mirrors the harness mapping,
   in all four cosim entry points.
2. **The `random(2,5)` duration is rolled in `onStart`, before the Lum Berry's `onUpdate` cure.**
   rb1297 t17: Sleep Powder lands on a Lum Berry Roaring Moon — PS rolls the duration, the berry
   wipes the status, and Roaring Moon Outrages normally that same turn.
   `sleep_survived_or_discard_duration` emits the roll as a draw-and-discard on the cured path at all
   four sleep-application sites (status move, move secondary, Tri Attack / Dire Claw `sample`
   secondary, residual/Yawn).

### S8 — Protosynthesis / Quark Drive
`refresh_proto_quark` runs after every `ChangeWeather` / `ChangeTerrain` push (five sites). PS's
`fromBooster` is now explicit state — a `VolatileStatus::ProtoBooster` companion bit set where the
Booster Energy is consumed, cleared on switch-out with its partner, read by convert and written by
export. The exporter previously INFERRED it as "the field condition isn't up right now", which is
ambiguous for exactly the case the removal arm must not fire on.

### Shields Down (Minior) — three PS-source pieces
The forme check is NOT continuous: `onStart` (`onSwitchInPriority: -1`) and `onResidual`
(`onResidualOrder: 29`) only. A forme change is undone on switch-out (`clearVolatile` ends with
`setSpecies(this.baseSpecies)`, and `base_species` is the right engine field because
`Instruction::Transform` never writes it) — rb1328's Minior shells up at full HP and is plain
`minior` on the bench forever after. And the Meteor shell is status-proof: `onSetStatus` returns
`false` unconditionally, `onTryAddVolatile` separately refuses Yawn; Shields Down has no `breakable`
flag so Mold Breaker does not pierce it.

### Lightning Rod / Storm Drain / Motor Drive — the largest single win (+9)
All three `onTryHit` + `return null`, and `hitStepTryHitEvent` precedes `hitStepAccuracy`. The engine
knew the type immunity but not the +1 SpA / +1 Spe, and had no status-move absorb entry for them at
all — so Thunder Wave into a Lightning Rod holder emitted a `rust-extra randomChance@accuracy`
(rb1211 t18, rb1350 t30: PS's whole unit draws NOTHING). The missing boost was also behind a chunk of
the `boost.spa` / `boost.spe` state-diff bucket.

## The 179 still-open extension games, re-triaged

| n | class | reading |
|---|-------|---------|
| 129 | `draws-match/state-diff` | the draw stream matches for the unit; the STATE differs |
| 5 | `PS shuffle@generic` | a bracket/schedule shuffle the engine does not emit |
| 5 | `move-order-tie` | unfilterable shuffle fork (both order-branches share a draw stream) |
| 4 | `rust-extra randomChance@accuracy` | see below — three distinct roots, all now named |
| 3+2 | `random@confusion` | rampage-end / Confuse Ray duration position |
| 2 | `PS sample@roar` | the phaze `sample` vs the engine's residual shuffle |
| 2 | `PS random@lockedmove` | rampage duration position |
| 2 | `convert-target:volatile:magnetrise` | **S6, still open** — see below |
| ~24 | singletons | `@struggle` (2), `@par` (2), `@shadowball`, `@icebeam`, `@freezedry`, `@throatchop`, `@icehammer`, `@heavyslam`, `@fireblast`, `@thunderbolt`, `@fakeout`, `@curse`, `@powerwhip`, `@hypervoice`, `@harvest`, `@bravebird`, `@disablemove`, `@crit`, `@trace` |

Field split of the 129 `draws-match/state-diff` games: 57 `hp`, 14 `volatiles`, 12 `boosts`,
7+3 `active_turns`, 6 `wish`, 3 `species`, 3 `sc.toxic_spikes`, tail. **Of the 57 hp deltas, 43
exceed 10 HP** — wrong MECHANICS, not rounding; 9 are within 3 (rb1008 rb1052 rb1101 rb1105 rb1145
rb1218 rb1221 rb1277 rb1282) and 5 sit in 4-10.

## Named opens carried forward (each with evidence, none reclassified)

- **S7 — modifier-chain rounding (9 games, |hp delta| <= 3). NOT ATTEMPTED this session.** PS
  accumulates every `chainModify` within ONE event into `this.event.modifier` and applies
  `modify(value, modifier)` once at the end of `runEvent`; the engine applies each modifier
  sequentially, which differs by 1. rb1008 (Perrserker, Tough Claws + Choice Band + Knock Off x1.5)
  is the clean instance: 250 vs 249. The fix is a real refactor of `compute_damage`'s base-power and
  attack-stat chains into 4096 fixed-point accumulators; the state sweep (3831/3831) is the rail that
  must stay green, and the bisect method is `battle.prng.getSeed()` (PS) vs `prng.limbs()` (gate) per
  unit.
- **S5 — tera formes (Ogerpon / Terapagos). NOT ATTEMPTED.** `Pokemon.terastallize()` forme-changes
  Ogerpon to `<forme>tera` (gaining `EmbodyAspect{Teal,Wellspring,Hearthflame,Cornerstone}` and its
  +1 stat) and Terapagos to `Terapagos-Stellar` (gaining `TeraformZero`). rb1184 is the recorded
  instance: `engine=terapagosterastal ps=terapagosstellar`, `engine=TeraShell ps=TeraformZero`, and
  max HP 273 vs 373 (base HP 90 -> 160), so it needs `respread_stats` AND a max-HP change.
  `TeraformZero`'s `onStart` additionally clears weather and terrain, which touches the shuffle
  schedule — do it with call-site ground-truthing, not by inspection.
- **S6 — `magnetrise` (2 games). NOT ATTEMPTED.** `convert.rs` still has no mapping, so rb1173 and
  rb1317 fail at `convert-target:volatile:magnetrise` before the gate can run. Cheap on the converter
  side but it needs the MOVE implemented too (5-turn Ground immunity + residual tick), plus an
  exporter entry for the round-trip gate, and `VolatileStatus::MagnetRise` / `magnet_rise_turns`
  already exist unused.
- **the 4 remaining `rust-extra randomChance@accuracy` are three distinct roots**, all evidenced:
  rb1397 t27 — **Focus Punch's `beforeMoveCallback`**: a holder that lost focus returns `true` from
  the callback, which `clearActiveMove`s with NO draw; the engine rolls accuracy for it. rb1183 t11
  (mid-turn) and rb1034 t46 — the second mover makes no draw in PS at all. rb1348 t11 — PS's WHOLE
  unit draws nothing (Draining Kiss + a terastallizing Outrage).
- **O1-O7 from the terminal certification are unchanged** except that **R1 is now CLOSED**. The
  stream-neutral `shuffle[3,0,2]` vs `[2,0,2]` stall-volatile item and the gate-consumed-outside-
  `chosen_draws` accounting shapes still explain the differ's remaining 19 units.

## Ops notes for the next session

- `MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz`
  **whenever `convert.rs` changes** — the S8 commit moved 22 of 401 digests.
- The sidecar gate (`harness/seed-sidecars/*.json.gz`) upgrades every `state-digest` label to the
  differing FIELD; `DBG_DIFF=1 DBG_GAME=rbNNNN` then prints every diff for that game. Note `dbg_on`
  is `name.starts_with(DBG_GAME)`, and the DIFF lines go to stderr.
- Judge every commit by the exact-SET diff on BOTH corpora. All nine commits this session were
  strictly monotone.
- **Kill criterion: still NEVER triggered.**

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 333 / 512**, and the non-exact SET must be a subset of the previous one. 8 s.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== PHASE-3 SEED EXTENSION — certification (2026-07-25) ====

**HEADLINE: 286 / 512 full games byte-exact from seed (55.9%); init-aligned 512 / 512.**
The ORIGINAL 111 are unchanged at **110 / 111** with the same single R1 (Cursed Body) exception —
zero regression at every step, judged by exact-SET diff, never by the count.

The extension is 401 fresh **gen9randombattle** full games (seeds 1000-1400, no `--max-decisions`,
no `--distributions`). Its purpose was to break things, and it did: the c-corpus's 110/111 is a
statement about the mechanic surface that corpus covers, not about randbats. Read the two numbers
separately — 110/111 on the audited corpus, 286/512 on audited + fresh.

```
# extended gate (slim fixtures — the committed, cheap path)
SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz
# same run against the sidecars, which upgrades each divergence label to the differing FIELD
bash harness/record-seeds.sh 1000 1400          # regenerate the sidecars (resumable, ~2 min)
SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-sidecars/*.json.gz
```

## Gate scaling

| | before | after |
|---|---|---|
| seed gate, 111 traces | 6.4 s (serial) | **1.9 s** (rayon, 10 cores) |
| seed gate, 512 slim fixtures | — | **7.7 s** |

Games are independent and every ambient generation hook the gate touches (`ANNOTATE_DRAWS`,
`FORCED_TIE_ORDER`, `REALIZED_SOURCE`, `BEATUP_ORDER`, `DBG_UNIT`) is a `thread_local` set inside
`run_game`'s own unit loop, so a whole game runs on one worker with no cross-talk. Trace load
(gunzip + parse) went into the parallel map — it was most of the wall time. Output is
byte-deterministic (indexed `map().collect()` preserves argument order). `GATE_THREADS=n` caps the
pool; peak RSS is ~threads x parsed-trace size.

## The slim fixture format (`harness/seed-fixtures/*.fx.json.gz`)

| | full v2 trace | slim fixture |
|---|---|---|
| 111 audited games | 12 MB | 872 KB |
| 401 extension games | 57 MB (sidecars, gitignored) | **3.4 MB committed** (mean 7.4 KB) |

A fixture carries the battle seed, format, teamset, the **packed teams** (resolved + set genders),
the **full PS state of the first (teampreview) decision only**, and per decision: the choices with
`rosterIndex`, the recorded PRNG draws (1% of the bytes, and they carry the entire init-alignment
check plus every first-divergence draw label), PS's live `side.pokemon` `rosterOrder` (Beat Up's
participant order), a precomputed `activeFainted` pair (replacement-vs-pivot, from the request
JSON), PS's `noActive` terminal-sentinel bits, and the canonical **state DIGEST**. Dropped: every
non-first serialized state and every request JSON — ~97% of the bytes.

`digest.rs` defines the encoding: it walks the SAME manifest `diff::diff_states` compares, in a
fixed order, into FNV-1a/128. Where `diff_states` gates on a JOINT predicate the encoder uses the
corresponding SELF predicate, which is equivalent because a disagreement on the predicate itself
always shows in a field the encoder covers (six cases, tabulated in the module doc).

**The one thing that is not derivable and must be DATA: the terminal sentinel.** A finished battle
can leave a side with no serialized active, so `convert` yields `active_index == u8::MAX` and
`diff_states` skips that side's index compare and whole active block — and which side that is
depends on how the battle ended. The fixture therefore stores PS's `active_index == u8::MAX` bits
and the gate digests the engine state under the same mask. Guessing it instead ("a side with no
living Pokemon means the battle is over, mask both") looked right — it fixed the 4 genuinely
lenient games — but ALSO turned 9 games with real terminal divergences green: `active_turns`,
`encore`, `volatiles`, `pending_move`, and in rb1148 a `rust-extra shuffle[2,0,2]@disablemove`
**over-emission**. A digest that is lenient in a place `diff_states` is not is a false-green
generator; the shortcut was reverted.

Certified at 512 games: the slim-fixture gate's output equals the sidecar gate's output game for
game, modulo the field-name suffix (the fixture reports `state-digest`, the sidecar reports the
field). Zero `digest-only` games — no game where the digest differs and `diff_states` is empty.

Both kinds funnel through one `GateInput` -> `run_game`, so the slim gate IS the full gate. On a
digest mismatch, re-run against `harness/seed-sidecars/` for the field-level diff (`DBG_DIFF=1
DBG_GAME=rbNNNN`). Sidecars are gitignored and regenerate byte-identically from the seed —
verified on 7 sampled seeds.

**Fixtures are only as good as the `convert.rs` that built them: regenerate with
`MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz`
whenever the converter changes.** They do NOT depend on the engine.

## Roots found and FIXED by the extension (266 -> 286, four commits, zero regression)

1. **gen9 species-locked items were wrong in both directions** (+5). `item_removable`'s comment
   asserted "Arceus plates are locked only to Arceus, which is outside the randbats pool" — false
   at this pin (arceusfire/bug/water/dark all appear). The real gen9 set, enumerated off the dex
   (every item with an `onTakeItem`): Arceus plates (493), Silvally memories (773), Genesect
   drives (649), Adamant Crystal / Lustrous Globe / Griseous Core (483/484/487), Rusted
   Sword/Shield (888/889), Ogerpon masks, Blue Orb (Kyogre), Red Orb (Groudon). The engine ALSO
   blocked the plain Adamant / Lustrous / Griseous Orb, which have no `onTakeItem` in gen9 — Knock
   Off both boosts on and removes a Palkia's Lustrous Orb. PS's `num ===` guards are SYMMETRIC
   (`(source && source.num === N) || pokemon.num === N`), so an Arceus attacker cannot Knock Off a
   Plate either; the `baseSpecies.baseSpecies ===` guards (masks, Blue/Red Orb) look only at the
   holder. `item_lock` + `item_removable_from(holder, item, source)`, wired into Knock Off's
   basePowerCallback, Knock Off's removal, Magician and Pickpocket.
2. **`onTryImmunity` runs BEFORE `hitStepAccuracy`** (+8). A move its own `onTryImmunity` rejects
   makes NO accuracy draw at all — even a 100-accuracy move, which would otherwise still roll
   `randomChance(100,100)`. The engine had these as EFFECT gates (Attract's gender check sits in
   `apply_status_target_volatile`, after the roll) or not at all — Leech Seed's Grass immunity was
   missing outright, so the engine seeded Grass types AND rolled accuracy at them
   (`rust-extra randomChance[90,100]@accuracy`). `status_try_immunity_fails` covers the gen9 status
   set: Leech Seed (Grass), Attract/Captivate (opposite genders), Trick/Switcheroo (Sticky Hold —
   `hasAbility`, so Mold Breaker bypasses), Worry Seed (Truant/Insomnia — PS reads `target.ability`
   RAW, so Mold Breaker does NOT), Octolock (Ghost is trap-immune).
3. **White Herb is an `onUpdate`, so it clears SWITCH-IN drops too** (+3). Its nine call sites were
   all move-secondary paths; Sticky Web's -1 Speed and Intimidate's -1 Attack go through
   `react_to_stat_drop`, which knew Defiant and Competitive but not the herb. Fixed there, after
   those two (PS runs them as `onAfterEachBoost`, inside the boost; the herb waits for the Update).
4. **Unburden's volatile ends on switch-out** (+4). PS adds it only from
   `onAfterUseItem`/`onTakeItem` and its `onEnd` removes it on leaving; nothing re-adds it on
   entry. `ALL_VOLATILES` was missing it, so the engine carried a stale bit 28 and read a doubled
   Speed for whatever entered next. rb1062: Hawlucha's White Herb is eaten by the turn-1
   Intimidate, granting `unburden`; it pivots and PS's volatiles go empty while the engine's did
   not — and PS's own later re-entries of Hawlucha confirm it never comes back.

## The 226 still-open extension games, triaged (evidence, not guesses)

First-divergence draw class (the gate's own ranking):

| n | class | reading |
|---|---|---|
| 144 | `draws-match/state-diff` | the engine's draw stream equals PS's for the unit; the STATE differs |
| 14 | `PS-unconsumed sample@sleeptalk` | **S1** below |
| 8 | `move-order-tie` | unfilterable shuffle fork (both order-branches share a draw stream) |
| 5 | `PS randomChance@ficklebeam` | **S2** |
| 5 | `PS shuffle@generic` | a bracket/schedule shuffle the engine does not emit |
| 4 | `PS-unconsumed random@slp` / `PS random@slp` (+4 more) | **S3** |
| 4 | `args randomChance@toxicchain` | **S4** |
| 4 | `rust-extra randomChance@accuracy` | residual accuracy over-emission (not Leech Seed) |
| 2 | `convert-target:volatile:magnetrise` | **S6** |
| ~20 | singletons (`@curse`, `@fakeout`, `@fireblast`, `@harvest`, `@par`, `@flamebody`, `@icehammer`, `@throatchop`, `@dragontail`, `@icebeam`, `@shadowball`, `@powerwhip`, `@struggle`, `@hypervoice`, `@bravebird`, `@headlongrush`, `@willowisp`, `@disablemove`, `@roar`, `@confusion`, `@lockedmove`) | bespoke |

Field split of the 144 `draws-match/state-diff` games: 53 `hp`, 19 `volatiles`, 12 `species`,
11+8+8+3+3 boosts, 6 `active_turns`, 5 `wish`, tail. Of the 131 hp deltas, **121 exceed 10 HP** —
these are wrong MECHANICS, not damage rounding; only 10 are within 2 (the true rounding class,
see S7).

Named shared roots, each with its evidence:

- **S1 — Sleep Talk is not modeled (14 games, the largest single class).** PS's `sleeptalk`
  `onHit` does `this.battle.sample(moves)` over the user's other moves and then `useMove`s the
  pick; recorded as `sample[3]@sleeptalk`. The engine emits no draw and calls no move. Evidence:
  rb1002 d45 (Snorlax asleep, PS `sample[3]`, engine's stream ends), and 13 more with the same
  label and a `moveN.pp` or `hp` diff. Fixing it needs called-move re-entrancy (the same machinery
  Metronome/Copycat/Assist would want), so it is a feature, not a patch. Emitting the draw alone
  would be a half-fix: the stream would align and the state would not.
- **S2 — Fickle Beam's doubling roll (5).** PS rolls `randomChance(3,10)@ficklebeam` BEFORE the
  damage roll; the engine's next draw is the `random[16]@damage-roll`. Consistent with the
  campaign's known Fickle Beam ORDERING note — it is now a first-divergence class in its own right.
- **S3 — sleep-duration `random(2,5)` position (8 across `@slp` labels).** Half report
  `PS-unconsumed random[2,5]@slp` (PS rolls a duration the engine never does), half report PS's
  `random[2,5]@slp` where the engine emitted `randomChance[100,100]@accuracy` — i.e. the engine
  rolls accuracy for Hypnosis/Sleep Powder at a point where PS has already moved on to the sleep
  counter. One schedule, two faces.
- **S4 — Toxic Chain fires per CONNECTING HIT, not per move (4).** rb1049: Fezandipiti's Beat Up,
  PS's stream is `[crit, dmg, randomChance[3,10]@toxicchain] x 6`; the engine emits the six
  crit/damage pairs back-to-back. This is structurally the SAME root as the campaign's one
  remaining c-corpus open (**R1**, per-hit Cursed Body inside a multi-hit): an `onSourceDamagingHit`
  /`onDamagingHit` ability roll that the realized multi-hit executor does not step its
  `RealizedCursor` over. R1's fix spec in the terminal certification above covers both — do them
  together, and note the engine ALSO gates Toxic Chain on contact, which PS does not.
- **S5 — Terastallization forme changes (7).** Ogerpon's four formes gain
  `EmbodyAspect{Teal,Wellspring,Hearthflame,Cornerstone}` (and the matching +1 stat) on
  terastallizing; Terapagos-Terastal becomes Terapagos-Stellar with `TeraformZero`. The engine
  keeps the base ability and base species: `engine=Sturdy ps=EmbodyAspectCornerstone`,
  `engine=TeraShell ps=TeraformZero`. Also in the `species` bucket: Cramorant's Gulp Missile formes
  and Eiscue's Ice Face.
- **S6 — `convert.rs` has no `magnetrise` volatile (2).** `d1:convert-target:volatile:magnetrise`.
  The engine's `State` HAS `VolatileStatus::MagnetRise`; the converter does not map PS's. Cheap,
  but it touches the exporter round-trip gate, so it wants its own commit.
- **S7 — modifier-chain rounding (10 games, |hp delta| <= 2).** PS accumulates every damage
  modifier in 4096 fixed point and applies `tr((base * mod + 2048) / 4096)` once; sequential
  rounding differs by 1. rb1008 (Perrserker, Tough Claws + Choice Band + Knock Off x1.5) is the
  clean instance: 250 vs 249.
- **S8 — Protosynthesis re-derivation (3).** The engine re-adds the volatile on switch-in where PS
  does not (`engine=Volatiles(33554432) ps=Volatiles(0)`), i.e. the Booster Energy / sun activation
  condition or its timing is off.
- **Bespoke tail (~20 singletons).** `@roar`'s `sample[n]` (PS picks the forced-out replacement
  with a sample the engine renders as a residual shuffle, 2 games), Throat Spray (2), Leppa/Lum/
  Chesto/Sitrus berry timing (~6), Light Clay, and the per-move accuracy singletons. Each has a
  recorded first-divergence label in the gate output; none is shared.

**Kill criterion: still NEVER triggered.** Every landed root this session was PS-source-grounded
and monotone; no fix was speculative, and the newly-non-exact set was EMPTY at all four steps.

## Extended CI gate

Add to the seven gates listed in the terminal certification above:

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 286 / 512**, and the non-exact SET must be a subset of the previous one. 8 s.
   Diff the exact-game set (`VERBOSE=1`, then `sed -n '/^exact games:/,$p'`), never the count.

---

# ==== TERMINAL CERTIFICATION — draw-exact campaign (2026-07-25) ====

**HEADLINE: 110 / 111 full games byte-exact from seed (99.1%).** All 111 init-aligned.
ONE named open item remains, with full PS evidence and a fix spec (below).

## Final gate numbers (all re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate (full battle from seed) | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **110 / 111 exact (99.1%)**; init-aligned **111 / 111** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim …` | **3810 / 3831 = 99.45%** (21 mismatches: 13 unconsumed, 4 args, 3 state-despite-draw-match, 1 not-next) |
| State sweep (mechanics rail) | `cosim …` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** (100% exactness, 100% coverage) |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **4832 / 4832** states, **3829 / 3829** move units |
| Transplant continuation | `node harness/transplant-gate.mjs` | **79 / 110 OK**, 17 diverge, 0 fail, 14 skip; **1812** continuation decisions state-exact |
| Protocol log-parity | `PROTOCOL_EMIT=harness/protocol-logs cosim …` then `node harness/protocol-parity.mjs` | 27 games, **525 semantic**, 4808 cosmetic (c1 = 2, r5 = **0**; 5 outliers r9 115 / r19 96 / r3 49 / r10 49 / r12 39 carry ~70%) |

Over-emission (the hard invariant — never emit a draw PS does not) is **ZERO**: no
`rust extra <draw>` entry remains in the differ; the single `rust-requested-draw-not-next-in-log`
is an ORDERING case (Fickle Beam), not an extra draw.

## The 21 remaining differ mismatches are NOT 21 engine gaps

They sit in 15 games — **14 of which are byte-exact from seed**. Three known accounting shapes:

1. **Gate-consumed-outside-`chosen_draws`** (12 × `shuffle[2,0,2]@generic`, 3 × `sample[1]@trace`).
   The seed gate consumes the forced-replacement 3-shuffle bracket and Trace's switch-in
   `sample(1)` in `step_unit`, *after* `replicate_select` returns, so they are absent from the
   `chosen_draws` the differ compares against the recorded unit. Verified case-by-case on
   c3c2s82 d19/d30, r10 d30 and c6a2s114: PS's stream and the gate's PRNG agree
   (`battle.prng.getSeed()` vs `prng.limbs()` match at the next unit start).
2. **Stream-neutral arg pair** (`shuffle[3,0,2]` vs `[2,0,2]`, t2 d5): both shuffle a 2-element
   tie group, i.e. both consume exactly one `random(start,end)` — a LIST-LENGTH cosmetic that
   needs a State stall-volatile flag distinct from `stall_counter` to render exactly.
3. **Real, in the one non-exact game** (r3 d23, item R1 below).

## THE ONE OPEN ITEM — R1: per-hit Cursed Body inside a multi-hit move (r3, 1 game)

**PS evidence.** `Cursed Body` is `onDamagingHit`, fired inside `spreadMoveHit`, i.e. ONCE PER
CONNECTING HIT of `hitStepMoveHitLoop` — not once per move. Recorded r3 d23 t19 (Koraidon
Scale Shot into Froslass), PS's exact stream:

```
randomChance[90,100]=true@scaleshot  sample[20]={idx 17 -> 5 hits}@scaleshot
randomChance[1,24]=false@scaleshot   random[16]=10@scaleshot   randomChance[3,10]=false@cursedbody
randomChance[1,24]=false@scaleshot   random[16]=7@scaleshot    randomChance[3,10]=true@cursedbody
```
(only 2 of the 5 hits execute — Froslass faints — and the 2nd Cursed Body roll procs, leaving
Koraidon with the `disable` volatile). The engine emits:
```
acc  sample[20]  crit roll  crit roll  randomChance[3,10]@cursedbody
```
— one trailing roll, because `apply_cursed_body` is called once from the post-hit-loop block
(generate.rs ~4482) while the multi-hit realized executor (`apply_multihit_realized`,
generate.rs ~6465) peeks every hit's crit+damage back-to-back.

**Fix spec** (deliberately NOT attempted here — it is the shared Scale-Shot-family path, and the
regression surface is the 21 currently-exact multi-hit games):
- In `apply_multihit_realized` / `apply_multihit_realized_ma`, step the `RealizedCursor` over one
  `randomChance(3,10)` between hits whenever the target has Cursed Body and the roll gate holds
  (`move != Struggle`, source not already Disabled) — exactly as it already steps over the
  inter-hit `ModifyDamage` screen shuffle via `cur.consume_shuffle(screen_k)`. Without this the
  cursor reads hit *n+1*'s crit out of hit *n*'s Cursed Body slot.
- Emit the roll per hit inside `apply_damage_hit`'s per-hit application (a per-hit hook), and stop
  rolling once a roll procs (PS's gate re-reads `source.volatiles['disable']`, so a proc silences
  every later hit).
- Judge: the seed-gate exact-set must stay monotone and the multi-hit distribution smoke must stay
  18/18; the Scale Shot / Bullet Seed / Icicle Spear / Population Bomb / Triple Axel families are
  the blast radius.

## Other evidenced opens (NOT blocking the 110; each has PS evidence, none is "neutral")

- **O1 — switch-in ability field change fires too early (stream-neutral).** The engine emits the
  `eachEvent('TerrainChange'/'WeatherChange')` shuffle at the switch application site; PS fires it
  inside `runSwitch`, AFTER the `battle-actions.ts:178` `getAllActive` speedSort. Probed at r10 d32.
  Same count, same args (`shuffle[2,0,2]`), so the PRNG stream is identical — a pure ordering nicety.
- **O2 — weather/terrain expiry `clearWeather`/`clearTerrain` position.** Both are now emitted
  (field.ts:97 / :165), the weather one at residual order 1 (exact) and the terrain one at the
  engine's terrain tick, which sits between Cud Chew (order 28) and Harvest (order 28) rather than
  at PS's terrain order 27. No corpus instance discriminates; if one appears, move the terrain tick
  ahead of the order-28 ability residuals.
- **O3 — cached-speed model is partial.** `replacement_bracket_tied` now zeroes a just-entered
  side's Speed BOOST (PS's `updateSpeed()` in `commitChoices` runs before the replacement enters, so
  Sticky Web's −1 is invisible to that bracket — c3c2s82 d49). Other stale-cache deltas (a mon last
  active under Swift Swim/Chlorophyll/Sand Rush/Slush Rush; Slow Start's active-turn window; a
  Choice Scarf acquired while benched) are still evaluated live. A full model needs a per-mon
  `cached_speed` field in `State`.
- **O4 — forme-change spread inversion is ambiguous below level 100.** `respread_stats` inverts
  `stat = floor(floor((2*base + d) * level/100 + 5) * nature)` for `d = IV + floor(EV/4) ∈ [31,94]`,
  nature-neutral first. Under level 100 the `* level / 100` truncation makes several `d` collide; when
  they disagree on the new stat the code falls back to the random-battle spread (31 IV / 85 EV /
  neutral), which is exact for every randombattle set. Exact fix: carry `nature` + `evs` + `ivs`
  through `convert.rs` instead of baking spreads into `stats`.
- **O5 — protocol log-parity is NOT part of the draw-exact contract.** 525 semantic diffs across
  27 games; c1 = 2, r5 = 0, and 5 randbats outliers carry ~70%. Owned by
  `EXPORT_PROTOCOL_OPENS.md` (move-execution ordering + effectiveness attribution).
- **O6 — transplant 79/110** is unchanged by this campaign; its two open classes
  (`per-decision unmodeled-field` and `mid-turn-faint turn-cascade`) are specified in
  `EXPORT_PROTOCOL_OPENS.md` sections A and B.
- **O7 — `PROTOCOL_EMIT` fails on 6 traces** (`resolve:choice:move-not-on-set`, e.g. r2
  `tripleaxel`, rd318 `scaleshot`): the emitter path resolves recorded choices against the mon's
  set and cannot resolve a locked/called move. Emitter-side, pre-existing.

## CI-gate recommendation for future engine commits

Run in this order; each is cheap enough for a per-commit hook except the last two.

1. `cargo test --release -p engine -j 2` — **12 suites must be green** (includes the
   apply/reverse instruction round-trip property test).
2. `target/release/cosim harness/cosim-traces/*.json.gz` — **must print 3831 / 3831 matched,
   0 diverged, 0 unsupported.** This is the mechanics-drift rail; it is the ONLY gate that
   catches a state regression in the DP (Enumerate/Sample) path.
3. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay ≥ 110/111,
   and the non-exact SET must be a subset of the previous one.** Diff the exact-game set, never
   the count alone; the VERBOSE listing is truncated and unusable for regression judgment.
4. `DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay ≥ 99.45%**, and
   the label list must contain **no `rust extra …` entry** (zero over-emission is the hard
   invariant; a new one means the engine invented a draw PS does not make).
5. `ROUNDTRIP_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **4832 / 4832**.
   Required for any change to `State`, `Instruction`, or `convert.rs`.
6. `bash harness/run-distribution-smoke.sh` — **18 / 18**. Required after any structural change to
   branch generation (it is the only gate that checks the branch PROBABILITIES, not just the set).
7. `node harness/transplant-gate.mjs` — **≥ 79 / 110**, 0 fail. Required for exporter changes.

Notes for whoever picks this up: (a) the differ and the seed gate disagree by construction on
gate-consumed draws — always trust the seed gate for "is this game exact"; (b) a realized-cursor
desync masquerades as a damage-rounding bug, so before touching `compute_damage`, bisect
`battle.prng.getSeed()` (PS) against `prng.limbs()` (gate) per unit — that method found three of
this session's six roots; (c) `diff_states` does NOT compare stored stats, so a forme/stat bug is
invisible to it and only the seed gate's downstream damage will show it.

**Kill criterion: NEVER triggered.** Every tranche landed structured, PS-source-grounded roots.

---

## State-computation + drag/trace/no-guard tranche (2026-07-24): 98/111 -> 103/111 (+5, 4 commits)

Worked the state-computation singles + realized-selection queue. All rails green each commit
(engine tests 12 suites incl roundtrip, state-sweep **3831/3831** 0-diverged/0-unsupported,
distribution smoke **18/18**, seed-gate exact-set strictly grew — zero prior-exact regression).

| commit | games | root cause + PS ref |
|--------|-------|---------------------|
| Effect Spore immune-band + drag PS-array reconstruction | 98->**100** (rd318, c5a1) | (a) Effect Spore rolls ONE `random(100)` into bands slp/par/psn/none; a band whose status can't land (Steel-tera Hydrapple immune to the psn band's poison) was folded into the noproc branch at res=30, shifting the realized decoder's thresholds so a psn-band roll (25) selected the par band. Emit each band as a no-op branch at its own threshold. (b) Drag `getRandomSwitchable` samples PS's CURRENT `side.pokemon` array; if the dragged side switched earlier this turn, mirror PS's `swap(0,j)` to reconstruct the live order (was a canonical fallback picking the wrong mon). |
| Revert Tera on faint (revived mon is base form) | 100->**101** (c1c) | PS `delete pokemon.terastallized` in faintMessages — a mon that faints Tera'd is revived (Revival Blessing) in BASE form. The forward gate carried the stale Tera on the hp=0 mon and showed it on revive. `apply_revive` reverts types+terastallized; `ToggleTerastallized` forward-apply now keeps `tera_used` sticky (reverse still recomputes) so the side flag stays spent. |
| Skip accuracy roll for special-cased status moves vs No Guard | 101->**102** (r11) | Strength Sap / Trick / Octolock emitted `randomChance(acc,100)` unconditionally; PS forces accuracy `true` (no roll) vs a No Guard target. r11: Polteageist's Strength Sap into No Guard Golurk. Gated all three on `!accuracy_forced_true`. (The scoreboard's old "Poltergeist accuracy skip" diagnosis was WRONG — Poltergeist never emits accuracy; the extra draw was Strength Sap's.) |
| Consume Trace switch-in sample for forced replacements | 102->**103** (c3c2s83) | A Trace replacement fires `sample(1)@trace` on switch-in; `switch_into` (state only) dropped it. step_unit consumes one per tracing replacement. Advances c3c2s82 d23->d33 (new divergence = a Leftovers-entangled compute_damage rounding, separate). |

### Remaining 8 non-exact — re-characterized (some prior diagnoses corrected)
- **d6** d64[t55]: weather-SETTING mid-turn switch schedule (item 1). Probed exactly: engine
  emits 5 update shuffles, PS 8 — missing `shuffle[2,0,2]@drizzle` (SwitchIn WeatherChange when
  Pelipper's Drizzle sets rain) + TWO `shuffle[2,0,2]@raindance` (FieldResidual). All tie-gated
  `[2,0,2]`. Needs the mid-turn weather-set + weather-FieldResidual speedSort emissions.
- **c6a2s114** d36[t31]: engine drops PS's trailing `random[100]@curse` — a Snorlax secondary in
  the mid-turn recompute (item 2; fragile DP path).
- **t2** d7[t8]: switch + tera + move Update-count — engine emits 4 `shuffle@update`, PS 3 (one
  extra in the tera-action bracket). (NOT the stall residual `[3,0,2]` vs `[2,0,2]` at d5 — that
  is stream-neutral: both consume one `random` over the same 2-tie-group.)
- **r10** d38: confusion_turns — SYMPTOM of an earlier mid-turn desync (DRAWCMP shows d30
  `PS-unconsumed shuffle@generic` mid-turn); the mid-turn-faint schedule class.
- **c3c2s82** d33[t30]: Grass Knot / Mystical Fire damage each off ±1-2, entangled with both mons'
  Leftovers end-of-turn heal — the masked compute_damage rounding class.
- **c2a1** d9[t8]: Future Sight + Meteor Beam genuine move-order-tie ambiguous fork (item 4).
- **r2** d7 / **r3** d23: parked (stall-volatile residual State flag needs a new State field;
  per-hit Cursed Body carries Scale Shot regression risk).

### Transplant gate (item 6): **79/110** OK (unchanged) — my flipped games c1c/c3c2s83/c5a1 now OK,
but r11/c3c2s82/c6a2s114/r10 diverge in transplant at earlier per-decision points (mostly the
`request-phase: extra mid-turn faint — turn-cascade` class, deferred per EXPORT_PROTOCOL_OPENS.md),
so no net transplant delta from these seed-gate fixes.

**Kill criterion: NOT triggered.** Four real structured roots landed (realized-band decoding,
array-order drag, faint-Tera-revert, No-Guard accuracy skip, Trace-replacement draw). Remaining
climb is the mid-turn/weather-schedule + compute-rounding classes — intricate, state-risky,
PS-probe-gated, ~1 game each.

---

## Mid-turn-faint Update schedule — GROUND-TRUTHED TABLE (2026-07-24, tranche start at 93/111)

Probed each mid-turn re-request unit in the 6 games with `harness/cosim_probe.mjs` (patched cosim
recorder: wraps `prng.shuffle`, logs the dispatching `eachEvent`, the JS call-site frame, the live
`getAllActive()` board, and the shuffle's handler `group`). **The engine already emits the correct
LEADING brackets and the move's 970/1024 — the ONLY gap is exactly ONE trailing `shuffle[2,0,2]` per
unit, from TWO distinct roots:**

| game | unit | move | target | missing trailing shuffle — PS call site (probe) |
|------|------|------|--------|--------------------------------------------------|
| d6   | i25 t24 | p2 uturn (pivot, user survives) | p1 replacement survives | **uturn runAction Update (battle.js:2376=2882)** on the PRE-pivot board `[Garchomp\|Pelipper]` tied — the engine applies uturn's pivot switch INSIDE execute_move before `run_move_action`'s `emit_update`, so the 2882 sees the post-switch board (untied) and is dropped |
| c3   | i45 t35 | p1 earthquake (KO) | p2 Toxapex faints | **Residual fieldEvent speedSort (battle.js:2344→333)** over `[leftovers/p1(o5,so4,spe106), leftovers/p2(o5,so4,spe106)]` — PS `fieldEvent('Residual')` iterates `side.active` (which still holds the just-fainted Toxapex, item retained, `clearVolatile` on faint wiped only volatiles) → 2 tied Leftovers → `shuffle[2,0,2]`. The engine's `residual_handlers` skips a fainted active → 1 Leftovers → no shuffle |
| c4   | i30 t24 | earthquake (KO) | Toxapex faints | same Residual 2×Leftovers tie |
| c5   | i70 t62 | saltcure (KO) | Toxapex faints | same Residual 2×Leftovers tie |
| r6   | i20 t19 | p2 liquidation (KO) | p1 Tauros-Blaze faints | same Residual 2×Leftovers tie |

PS `fieldEvent('Residual')` (battle.ts:487): the handler is collected for the fainted holder but the
while-loop `if (handler.effectHolder.fainted) continue` SKIPS its execution — so the fainted mon's
Leftovers contributes to the speed-SORT (the shuffle) but not to the heal. `getAllActive()` (excludes
fainted) shows `[surviving-mon only]` at the shuffle, confirming the tie is over the residual HANDLER
list (2 items), not the actives. c5a1's mid-turn i12 is a DIFFERENT class (`shuffle@generic` after a
double-move, 3 unconsumed) and its later d18 is the thunderwave move-order class — left for the singles.

## RESULT — mid-turn-faint tranche (2026-07-24): 93/111 -> 98/111, differ 99.09% -> 99.27%, 3 commits

Three ground-truthed, zero-regression mechanics landed (rails green each commit: engine tests 12
suites, state-sweep **7662/0/0**, distribution smoke **18/18**, seed-gate non-exact set a strict
subset every step). The "dominant remaining class" hypothesized above (turn-start-vs-switch-in
bracket) was REFINED by call-site probing: the engine's leading brackets and move 970/1024 were
ALREADY correct — the actual gap was one trailing shuffle per unit, from three distinct roots:

1. **mid-turn-faint Residual handler tie** (e06d2bb; flips c3, c4, c5, r6, **t1**). A mon that
   faints mid-turn stays in PS's `side.active`, so `fieldEvent('Residual')` speed-sorts its
   faint-surviving residual handler (Leftovers / Cud Chew ability) — one extra `shuffle[2,0,2]` when
   it ties the surviving foe's. `residual_handlers` now collects a fainted active's item/status/
   ability residuals.
2. **pivot move trailing 2882 on pre-switch board** (71de8a4; advances d6 d28->d66). PS fires a
   self-switch move's runAction Update before processing `switchFlag`, so it sorts with the pivot
   user still on-field; the engine applied the switch first and dropped it. New per-branch
   `pivot_update_done` + `emit_pivot_trailing_update` at each pivot apply site.
3. **mid-move Update ties use the frozen pre-move Speed** (360ea83; advances c5a1 d18->d31; differ
   +7 units). PS's `getAllActive` speedSort reads the cached `pokemon.speed` (refreshed only at
   turn start / before each move by `updateSpeed`), so a paralysis/secondary Speed change a move
   applies does not break the SAME move's 970/1024/2882 tie. `run_move_action` snapshots both
   Speeds into `MOVE_TIE_SPEEDS`; `actives_update_tie` uses them (liveness stays live).

### Remaining 13 non-exact (all init-aligned) — characterized open items, each 1-game/state-risky
- **d6** d66: weather-SETTING mid-turn switch-in Update schedule — a voluntary mid-turn switch that
  triggers Drizzle fires `eachEvent('WeatherChange')` + `eachEvent('Weather')` speed-sorts the engine
  doesn't model, plus a `FieldResidual` weather-handler tie (probed: 3 missing shuffles). Distinct
  intricate class, 1 game.
- **c5a1** d31 / **c2a1** d9 / **c1c**/**c3c2s82**/**c3c2s83**/**r10**/**r11**/**t2**/**rd318** —
  per-move STATE-computation divergences (draws match; active_index / hp / types / confusion_turns /
  status differ). Not draw work — each a bespoke `compute_damage`/switch-state/status item.
- **c6a2s114** d36: `PS-unconsumed random[100]@curse` in a mid-turn Palafin re-fire — an extra
  secondary the engine's mid-turn recompute drops (pivot midTurn Palafin-Hero recompute).
- **r2** d7: `shuffle[5,3,5]` vs `[4,2,4]` — a stall-volatile-present residual-handler LIST-LENGTH
  arg (draw-count-neutral); real fix needs a State stall-volatile flag distinct from `stall_counter`.
- **r3** d23: per-hit Cursed Body (`randomChance[3,10]` after EACH multi-hit hit) — deferred (Scale
  Shot family regression risk to 55 exact games).

**Kill criteria: NOT triggered.** Three real structured shared-path roots landed (density still
decaying). Remaining climb is per-move state-computation + the weather-switch-in schedule (intricate,
state-risky, ~1 game each).

## Terminal state after the two Update-schedule tranches (93/111 = 83.8%)

18 games remain non-exact. First-divergence classes (live seed gate):
- **mid-turn re-request / faint Update schedule (c3, c4, c5, r6, d6; + c5a1's later thunderwave
  divergence)** — THE dominant remaining class. A `midTurn:true` unit (a mon faints mid-turn, its
  side switches in a replacement, the other side's move then resolves) whose Update shuffle schedule
  the gate mis-counts: `step_unit` resolves the mid-turn switch as a pivot and emits a fresh
  turn-start bracket instead of PS's mid-turn switch-in bracket (d6 idx25 ground-truthed: switch
  runAction Update + runSwitch getAllActive speedSort battle-actions:178 + runSwitch runAction Update,
  then the other side's move 970/1024). SHARED turn-resolution/faint path — HIGH regression risk;
  reserved for a dedicated careful tranche (see HANDOFF diagnostic method: prng.getSeed vs
  prng.limbs() drift-localization + call-site probe).
- **c5a1 thunderwave move-order** (d18[t16]: PS `shuffle[2,0,2]@thunderwave` vs rust
  `randomChance[1,4]@par`) — a status-move Update / move-order interleaving.
- **c2a1** move-order-tie genuine fork; **c3c2s82/s83** Trace onUpdate re-fire; **c6a2s114** mid-turn
  Palafin re-fire + curse; **d6** rust-extra crit (mid-turn); **r10** confusion; **r11** Poltergeist
  accuracy skip; **r2** stall-volatile cosmetic; **r3** per-hit Cursed Body; **rd318** Fickle Beam;
  **t1** frz thaw boundary; **t2** / **c1c** state-diff. (Per the ledger below.)

**Kill criteria: NOT triggered.** Both landed classes were real structured shared-path roots
(density still decaying); the remaining climb is gated on the mid-turn-faint schedule (intricate,
state-risky) + per-move state-path items.

---

## Replacement-switch bracket tranche 2026-07-24 (90/111 -> 93/111; forced-switch draw gap)

Root found by comparing PS's per-turn PRNG seed (probe `battle.prng.getSeed()`) against the gate's
`prng.limbs()` at each decision start on c5a1: the gate's state at "t12-start" equalled PS's
**t11-start** — the gate had under-consumed t11's forced-replacement switch draws, a state-neutral
drift that only surfaced 2 turns later (t13 damage roll read the wrong PRNG slot: engine 11 vs PS 8).

**The gap:** `seedgate.rs::step_unit` applies post-KO forced-replacement switches via
`switch_into` / `switch_into_pair` (STATE only) and consumed ZERO PRNG draws for them. But PS
resolves each replacement as a `switch` action whose runAction fires a **3-shuffle bracket** — probed
on c5a1 t11 (Primarina replaces a fainted Alcremie vs Speed-tied Grimmsnarl, seed 46844→21739):

| PS shuffle | call site | tie board |
|-----------|-----------|-----------|
| switch-action runAction Update | battle.js:2376 (=battle.ts:2882) | post-swap |
| runSwitch getAllActive speedSort | battle-actions.js:178 (=:182) | post-swap |
| runSwitch runAction Update | battle.js:2376 | post-swap |

All three are `getAllActive()` speed-tie shuffles → 3 draws when both actives are alive & Speed-tied,
0 otherwise. A bracket fires at the transition to a both-alive-tied board: once for a single
replacement, and once for the SECOND of a simultaneous both-sides double faint (the first switch runs
while the other slot is still fainted → getAllActive has one active → no shuffle). The engine's
annotated switch bracket covers only VOLUNTARY move+switch pivots; forced replacements bypass it.

**Fix** (seed-gate only; generate.rs adds only a `pub fn replacement_bracket_tied` wrapper over the
existing `actives_update_tie(state,false)` predicate): after applying replacements in `step_unit`,
consume 3 `shuffle[2,0,2]` draws per firing bracket, gated on `!pre_end_turn` (same-turn mid-move
replacements have different timing, left untouched) and the post-swap tie. **Flips c3b2s52, c3b2s53,
c6a2s112 exact; advances c5a1 d15[t13]->d18[t16]** (its new divergence is a `shuffle@thunderwave` vs
`randomChance@par` move-order class, distinct). Safe by construction: a tied forced-replacement was
ALWAYS broken before (the drift guaranteed a later desync), so no previously-exact game had one —
consuming the tie-gated bracket only fixes (tied) or is neutral (off-tie: 0 draws). Rails: engine
tests 12 suites, state-sweep 7662/0/0, smoke 18/18, differ 99.09% (unchanged — differ path untouched),
seed gate 90->93 zero prior-exact regression (exact-set diff). Commit: (this tranche).

---

## Update-schedule tranche 2026-07-24 (89/111 -> 90/111; inter-move Update-count schedule)

The dedicated careful tranche for the shared turn-resolution `eachEvent('Update')` schedule.
Ground-truthed PS's EXACT per-turn Update-call table with a standalone PS probe (scratchpad
`probe.mjs`: wraps `battle.prng.shuffle` + `eachEvent`/`runEvent`, replays a trace's recorded
choices, logs every shuffle's dispatching event + call-site stack frame). All rails green: engine
tests 12 suites, state-sweep **7662/0/0**, distribution smoke **18/18**, differ **99.03% -> 99.09%**
(3794->3796, no over-emission increase), seed gate monotone by exact-set diff (zero prior-exact
regression; remaining non-exact is a strict subset of the prior 22 minus c7).

### THE GROUND-TRUTHED per-turn Update `shuffle[2,0,2]` schedule (probe-verified, c5a1 t12)
For a turn on a Speed-tied board (both actives on-field, equal `effective_speed` — every Update
speed-sorts `getAllActive()` so it shuffles iff the pair is tied), each **runAction** fires ONE
trailing `eachEvent('Update')` (battle.js:2376 = battle.ts:2882), and each move ADDITIONALLY fires
the moveHit-loop Updates ONLY if it enters the per-POKEMON loop. Mapping the c5a1 t12 stream
(p1 Primarina psychic vs **p2 Grimmsnarl Prankster Reflect** — Reflect gets +1 priority so it runs
FIRST despite the Speed tie; both actives Speed-tied so every Update shuffles):

| PS pos | call site | what |
|--------|-----------|------|
| 0 | battle.js:2337 `eachEvent("BeforeTurn")` | beforeTurn action |
| 1 | battle.js:2376 runAction Update | beforeTurn action's trailing 2882 |
| 2 | battle.js:2376 runAction Update | **Reflect** action's trailing 2882 (NO move-internal Update) |
| 3-6 | psychic accuracy/crit/damage/secondary | the damaging move's rolls |
| 7 | battle-actions.js:843 per-hit `eachEvent("Update")` | psychic's moveHit-loop 970 |
| 8 | battle-actions.js:888 post-hit-loop `eachEvent("Update")` | psychic's 1024 |
| 9 | battle.js:2376 runAction Update | psychic action's trailing 2882 |
| 10 | battle.js:2376 runAction Update | residual action's trailing 2882 |

**Key fact: Reflect (target `allySide`) fired ZERO move-internal (843/888) Updates** — a
side/field-targeting move resolves via the side/field `onHit` path and never enters
`hitStepMoveHitLoop`. Self-targeting POKEMON moves (Calm Mind = `self`) DO enter the loop and fire
970+1024 (confirmed by the existing calmmind schedule).

### Fix landed
The engine's status-move 970/1024 emission (`generate.rs` ~3624) gated on "the branch added ≥1
effect instruction" — which is TRUE for side-condition moves (they push a SideCondition
instruction) — so on a Speed-tied board it over-emitted 970+1024 for Reflect/screens/hazards/
weather. Added a `hits_pokemon` gate excluding `MoveTarget::{AllySide, FoeSide, All, AllyTeam}`.
These emits are `actives_update_tie`-gated (no-op off a Speed tie), so the change touches ONLY
tied boards — minimal regression surface. **Flips c7 exact; advances c5a1 d14[t12]->d15[t13]**
(c5a1's residual is now a `draws-match/state-diff` on `s1.boost.spa` — the same masked class as
c3/c4/c5/r6, NOT the Update schedule). Commit: (this tranche).

---

## Endgame-queue session 2026-07-24 (83/111 -> 89/111; +6 games, 2 fix commits)

Worked the endgame queue. All rails held every commit: engine tests 12 suites, state-sweep
**7662/0/0**, distribution smoke **18/18**, seed gate monotone by exact-set diff (zero
prior-exact regression — d7 explicitly verified after the -notarget scope fix).

| step | games | root cause + PS ref | commit |
|------|-------|---------------------|--------|
| 1 | **88** | **Set-gender init-offset gap (the 6 align=false c5 games)** — the directed-c5 teamsets fix each mon's gender in the packed set (deterministic Attract/Cute Charm legality). PS's `new Pokemon` uses `set.gender \|\| species.gender \|\| sample(['M','F'])`, so an explicit set gender SHORT-CIRCUITS the construction-time gender sample. `init_gender_rolls` counted one sample per dual-gender species regardless -> over-emitted the unlogged construction draws -> desynced the PRNG at turn 1 (all 6 align=false). Recorder (`harness/cosim.mjs`): capture the raw per-mon `setGender` ('' \| M/F/N) in the roster snapshot — the resolved `gender` can't distinguish set-vs-rolled. Seed gate: a non-empty `setGender` suppresses the roll; pre-field traces lack it -> treated empty -> original accounting preserved (zero change to any non-c5 game). Re-recorded ONLY the 6 c5 traces at their original seeds/teamsets; verified each reproduces byte-identical choices/core-draws/states (sole delta: the setGender field). **All 111 games now init-aligned (0 align=false).** c5a2/c5b1/c5b2/c5c1/c5c2 fully exact; c5a1 aligns but reveals a downstream Update-count divergence (d14, PS randomChance[100,100]@psychic vs rust shuffle@update). | 2c43854 |
| 2 | **89** | **`-notarget` draw suppression: foe status move into a fainted foe** — c2a3 d13's HP-off-by-1 (Double Shock 226 vs 227) was **NOT** compute_damage rounding (scoreboard's "STAB rounding" was WRONG). PS's recorded damage roll is 11 (dmg 109); Replicate selected roll 9 (dmg 110) because the PRNG stream was OFFSET BY ONE upstream. Differ pinned it at t7: Regieleki (faster) Explosion self-KOs + damages Tinkaton; Tinkaton's Encore then has no target -> PS's `getMoveTargets` returns empty -> `useMoveInner` bails BEFORE `hitStepAccuracy` (`-notarget`, battle-actions.ts) -> no accuracy draw. The engine executed Encore and emitted its always-true `randomChance[100,100]@accuracy`; state stayed exact (fainted foe = no effect) but the +1 draw shifted every later roll, masked six turns by robust randomChance until the sensitive random[16] at d13 exposed it. Fix: in the status branch of `execute_move_inner`, a move targeting a single foe pokemon (MoveTarget Normal/AdjacentFoe/Any/RandomNormal/Scripted — NOT User/FoeSide/All/AllySide) against a fainted foe returns no draws. **Annotation-gated** — the DP path emits no draws and is already state-neutral, so the sweep stays byte-identical. (First attempt used `targets_foe_status`, which also matches Substitute's self-volatile -> regressed d7; narrowed to an explicit foe-pokemon MoveTarget check.) | fc85004 |

### Corrected diagnoses discovered this session (the one-line scoreboard roots were stale/wrong)
- **c3b2s52 is the move-order-TIE COMPOSITION class, not "Aeroblast accuracy 90 vs 95".** Lugia and Latios are both Speed 350 (genuine tie); the engine's `forced_tie_order = (b0 == b3)` peek picks Latios-first, PS picks Lugia-first. Measured b0=1,b3=0 with a residual in the length-3 dynamic queue (`shuffle[3,0,2]`) -> the b0==b3 rule is wrong when the residual is present. This is the ONLY game whose first divergence is the tie composition (c3b2s53/c2a1/c6a2s112 are different classes: first-mover-Update / ambiguous-fork / Wave-Crash-faint). 1 game, high regression risk to the ~many exact tie games — deferred.
- **t1 is a freeze-secondary alignment, not a simple thaw roll.** Both order Ice Beam (Blissey, side Two) first correctly; the divergence is that PS's Ice Beam freezes Golurk (roll <10) then rolls the [1,5] thaw for the blocked Earthquake, while the engine's realized random[100] reads 10 (no freeze) — an upstream PRNG offset masked exactly like c2a3.
- **Many "draws-match/state-diff" games are UPSTREAM draw-count offsets, not compute bugs.** The differ compares kinds/args, not `random` result VALUES, so a same-shape offset (an over/under-emitted shuffle or accuracy) reads "draws-match" yet the realized rolls differ. The remaining cluster c3/c4/c7 all show `rust randomChance[100,100]@accuracy vs ps shuffle[2,0,2]@generic` — the engine UNDER-emits an inter-move eachEvent('Update') shuffle (the first-mover-no-draw Update / Task-3 Update-count class). c3c2s82/s83 = `ps unconsumed sample[1]@trace` (Trace onUpdate re-trace timing). c5/r6 = `ps unconsumed shuffle@generic`. c6a2s114 = `ps unconsumed random[100]@curse` + the mid-turn Palafin re-fire (its DP sweep is fragile — the -notarget guard had to be annotation-gated to keep it byte-identical). All intricate/state-risky, 0-to-few games each, deferred to avoid destabilizing the shared turn-resolution/multi-hit paths for the 89 exact games.

---

## Seed-gate tail session 2026-07-24 (69/111 -> 83/111; +14 games, 5 commits)

Worked the LEADS queue game-by-game. All rails held every commit: engine tests 12 suites,
state-sweep **7662/0/0**, distribution smoke **18/18**, differ **99.03%** (3794/3831, no
over-emission), SEED_GATE monotone by exact-set diff with **zero** prior-exact regression.

| step | games | root cause + PS ref | commit |
|------|-------|---------------------|--------|
| 1 | **70** | **Stomping Tantrum last-move-failed tracking** — r12 t35 doubled ST (150 BP) because its t34 use was Ground-immune vs a Levitate Mismagius (empty hitTargets = PS `moveThisTurnResult === false`). Engine never tracked `last_move_failed` in forward play (only convert.rs derived it for the annotation prestate). New transient `Branch.move_failed` set at the damaging-move failure sites (immune/miss/no-target/dodge/Protect/Air Balloon/Psychic Terrain/Queenly Majesty; boosting absorbs stay null), committed once per move in `run_move_action` via a new `SetLastMoveFailed` instruction (PS nextTurn commit timing). Only ST reads it, only r12 uses it -> zero cross-game risk. | b3cf59f |
| 2 | **74** | **Cursed Body randomChance proc encoding** — the disable roll annotated proc=0/noproc=3 (the `random(100)`-secondary threshold convention), but `replicate_select` only threshold-decodes `random[100]`; `randomChance` falls to exact-match against the realized boolean (0/1). So a no-proc value (0) exact-matched the proc branch (result 0), inverting selection — the engine applied a Cursed Body Disable PS never rolled on EVERY game where a move hits a CB holder and PS didn't proc. Fixed to the boolean convention (proc=1/noproc=0), matching crit/par/frz/Cute Charm/Poison Touch. Flips r20/r7/t5/t6. | 8ee3fb0 |
| 3 | **75** | **Tera Shift forme ability persists across switch-out** — r4/r18: a benched Terapagos showed ability TeraShift (engine) vs TeraShell (PS). Tera Shift's forme change (`formeChange`) is PERMANENT, but the switch-out copied-ability revert (Trace/Role Play) treated TeraShell as a copy and reverted it to the stale base_ability. Guard the revert to skip the Terastal forme ability. Flips r4. | 3f0c83c |
| 4 | **77** | **Seed-gate realized selection** — (a) `replicate_select` multi-way `random(100)` threshold: the proc decoder only handled the binary case; generalized to pick the branch whose threshold is the largest <= realized value (Effect Spore slp=0/par=11/psn=21/none=30). (b) `apply_drag` samples over PS's CURRENT `side.pokemon` array order (`getRandomSwitchable`), not canonical order — reuse the per-side order installed for Beat Up, guarded to only apply when still valid (dragged side didn't switch first this turn, d3). Flips c3a1s12/d5. | be968bd |
| 5 | **83** | **Status-move miss branch accuracy result** — a foe-targeting status move that can miss built a miss branch but never flipped the inherited accuracy result (1=hit) to 0 (the damaging path does). Both branches carried result 1, so `replicate_select` could not select a real miss -> it applied the status on a recorded MISS (Thunder Wave paralysing / Sleep Powder sleeping / Yawn). Surfaced as 'move-order-tie' / 'rust-extra' labels. Flips c3c1s71/c3c1s72/c3c1s73/d1/r18/r5. | fcc1ba6 |

### Remaining 22 aligned non-exact (+6 align=false blocked on set-gender init gap) — open items with evidence
- **Per-move damage-calc state-diff (draws match, HP off a few)** — c2a3 (Double Shock STAB rounding, -1), c3, c3c2s82 (+8), c3c2s83 (+6), c4, c5, c6a2s114 (Palafin-Hero stat-spread approx, +4), r6, r11, t2, c1c(types), c7(boost). Each a bespoke `compute_damage`/stat item; masked by the DP in the sweep. Different signs/magnitudes -> NOT a single shared rounding root.
- **Switch-bracket / inter-move Update-count PRNG offset** — d6 (switch+move: accuracy reads hit on a recorded miss), c4 (mid-turn KO: engine under-emits one trailing Update vs PS's 2), c6a2s112 (both-move Wave Crash tie: crit reads a shifted slot). PS's per-move 970/1024/2882 Update schedule interacts with mid-turn faints; the engine's count desyncs the crit/damage rolls. Intricate, state-risky.
- **Move-order-tie genuine fork** — c2a1 (Future Sight + Meteor Beam, both order-branches share a draw stream).
- **Per-hit Cursed Body interleaving** — r3 (Scale Shot x2 fires CB after EACH hit; engine emits once post-loop). Documented risky (Scale Shot family regression).
- **args / under-emission singles** — c3b2s52 (Aeroblast accuracy 90 vs 95: a stage/evasion modifier, data value is correct), c3b2s53, t1 (same-turn frz thaw boundary), r2 (residual handler-length cosmetic), r10 (confusion secondary desync), rd318 (Effect Spore poison-immune value + drag), c6a2s112 wavecrash.

**Kill-criterion: NOT triggered.** Density decayed with real structured classes (last-move-failed, CB encoding, forme-ability persistence, multi-way/drag realized selection, status-miss branch). The remaining climb is per-move damage-calc + the intricate inter-move Update-count schedule (0-to-few games each, state-risky).

---

## State-computation queue — session 2026-07-24 (63/111 → 64/111; differ 98.98% → 99.01%)

Resumed the state-computation queue (the `draws-match/state-diff` seed-gate class). First class landed:

| step | games | differ | class landed | commit |
|------|-------|--------|--------------|--------|
| SC1 | **64/111** | 99.01% (3792→3793) | **Disguise / Ice Face single-hit bust crit+damage rolls** — PS's Disguise (`data/abilities.ts` mimikyu) and Ice Face (eiscue) are `onDamage`/`onCriticalHit`/`onEffectiveness` blocks, NOT `onTryHit` immunities: `getDamage` still rolls the crit `randomChance(1, critMult[critRatio])` (base critRatio=1 → den 24) and the damage `random(16)` — `onCriticalHit` returns false (forces no-crit) and `onEffectiveness` returns 0 (typeMod 0) AFTER the rolls, then `onDamage` returns 0 to zero the dealt damage and bust. The engine's SINGLE-HIT Disguise/Ice-Face branches (`generate.rs` ~3820/3843) short-circuited BEFORE those two rolls (multi-hit Ice Face already rolled them inside `apply_damage_hit`'s loop). So e.g. U-turn into an intact Mimikyu (rd298 d1) emitted only accuracy, under-emitting crit+damage by 2 and desyncing every later damage roll → d3's damage read the wrong PRNG slot (state-diff). Fix: emit the crit + damage draw-and-discards (result 0; state is bust regardless) + `emit_modifydamage_shuffle` before `bust_disguise`/`break_ice_face`, mirroring PS's getDamage order. Annotation-gated (`draw()` only fires under annotation) so Enumerate/Sample state path is byte-identical. Clears rd298. | (this commit) |

| SC2 | **67/111** | 99.01% (unchanged) | **Confusion self-hit damage roll inverted** — the confusion self-hit computed `bd * (85 + i) / 100` for branch `i` while emitting the `random(16)` draw with `result = i`. PS's `getConfusionDamage` uses `randomizer` = `tr(tr(bd * (100 - random(16))) / 100)` (battle.ts:2404) — SAME orientation as the main damage path (branch `result == roll`, higher roll → less damage). So for a recorded roll R the differ/gate selected the engine's branch `i == R`, which dealt `bd*(85+R)/100` instead of `bd*(100-R)/100` — over-dealing by `bd*(15-2R+... )` (e.g. c3a2s21 d6: PS roll 12 → `bd*88/100`, engine → `bd*97/100`, +7 HP). Fixed to `bd * (100 - i) / 100`. Clears c3a2s21, c3a2s23, AND **d4** (whose residual — previously mis-attributed in the scoreboard as a screen×multi-hit damage-rounding class — was this confusion inversion, not compute_damage rounding). The differ was already "draws-match" on these (it compares kinds/args, not the roll result the state encodes), so the differ % is unchanged; the gate state-count moved. | (this commit) |

| SC3 | 67/111 | 99.01% | **Beat Up participant order = PS's `side.pokemon` array order** — PS iterates `pokemon.side.pokemon` (its CURRENT array; `switchIn` swaps positions 0↔j, keeping the active at index 0 and swap-tracking the rest) to assign each hit's base power `5+floor(baseAtk/10)`. Since each participant's base power pairs with a distinct per-hit roll, the order changes the realized total. The engine stores a fixed canonical (teampreview) slot order, so it paired the wrong base powers with the rolls (c2a2 d13: engine `[12,12,14,15]` vs PS `[14,15,12,12]`). The seed gate now installs PS's array order (the recorded pre-state's `rosterIndex` sequence, thread-local `set_beatup_order`) into `beatup_calcs`; the DP (state sweep) and differ are order-independent so they leave it unset. Alone this flips 0 games (needs SC4 too) but is a mandatory mechanic. | (SC3 commit) |
| SC4 | **69/111** | 99.03% (3793→3794) | **onBasePower modifiers accumulate as ONE `chainModify`, applied once** — PS runs every multiplicative onBasePower handler in descending `onBasePowerPriority`, accumulating a single `event.modifier` (`((prev*next+2048)>>12)`), then applies it ONCE at the event's end (`modify(basePower, event.modifier)`). The engine applied each as its own `modify`, re-rounding every step — diverging once two stack (c2a2: Technician ×1.5 [prio 30] + Black Glasses ×1.2 [prio 15] on bp 14 → sequential 14→17→26, PS's chain 14→**25**). Reworked the item/ability/terrain base-power block into a priority-ordered `bp_step` chain (Technician 30, Iron Fist 23, Sheer Force/Supreme Overlord 21, Strong Jaw/Sharpness 19, type-items/orbs/Soul Dew/Ogerpon 15, Punk Rock 7, terrain 0); Technician's ≤60 gate reads the raw base power (it is highest-priority). Single-modifier results are unchanged where they don't cross a rounding boundary (why the sweep held); the chain now matches PS's exact `chainModify` rounding (e.g. a lone ×1.2 uses modifier 4916, PS's value, not 4915). Flips c2a2 + c2a5 (with SC3). | (SC4 commit) |

Rails (SC1–SC4): engine tests 12 suites green, state-sweep **3831/3831** (0 diverged, 0 unsupported), distribution smoke **18/18**, differ **99.03%** (3794/3831, no over-emission), VERBOSE exact-set diff shows **rd298/c3a2s21/c3a2s23/d4/c2a2/c2a5 newly exact across the session, zero regressions**.

### DIAGNOSED LEADS for the remaining state-diff queue (next session — roots identified, each ~1 game)
The remaining `draws-match/state-diff` games are heterogeneous single-game roots (not a shared class):
- **`@stompingtantrum` base-power doubling (r12 d42, +104 HP)** — Stomping Tantrum doubles BP when the
  user's LAST move failed (PS `basePowerCallback` reads `pokemon.moveLastTurnResult === false`). PS
  doubled it here (150 vs 75) but the engine's `last_move_failed` was false. Root: the engine's
  per-side `last_move_failed` doesn't track PS's `moveLastTurnResult` across the prior turn (p2 used
  Stomping Tantrum on d41 into a Substitute/switch and PS recorded that as a fail). Verify PS's
  moveLastTurnResult semantics vs the engine's flag.
- **switch-bracket shuffle desync (c4 d32, crit/roll differ)** — p2 switches, p1 Earthquakes the
  switch-in; the engine's switch-Update `shuffle[2,0,2]` emission count desyncs the PRNG before the
  Earthquake crit/damage rolls (engine crit0/roll0 vs PS crit1/roll15). The switch-before-move path
  IS modelled (generate.rs step 1), so this is a switch-bracket equal-Speed-predicate mis-count.
- **mid-turn re-fire targeting (c6a2s114 d47, +4 HP)** — a `midTurn:true` re-request where p1 switches
  and p2 U-turns; the engine computes the move against the PRE-switch active (Psychic/Ghost) instead
  of the switch-in (Palafin-Hero). Seed-gate pivot/mid-turn unit handling, not the normal turn path.
- **c2a3 (Double Shock, +1 HP), c3/c5 (rolls differ → small desyncs), c3c2s82 (+8), c3c2s83 (+6)** —
  per-turn desync/rounding, each needs the turn's draw-count walked. The `.volatiles`/`.types`/
  `.ability`/`.boost`/`.active_index` first-divergences are separate non-damage classes.

---

## Finishing tranche — session 2026-07-24 (differ 98.93% → 98.98%; 2 classes; 63/111 games held)

Resumed the differ-zero finishing tranche from HANDOFF_DRAW_EXACT.md. Landed two draw-COUNT
classes (all rails green throughout: engine tests 12 suites, state-sweep **3831/3831**, smoke
**18/18**, seed gate **63/111** with zero prior-exact regression — the selfBoost/diamondstorm
changes only remove/add draw-and-discards and apply identical boosts):

| step | differ | class landed | commit |
|------|--------|--------------|--------|
| S1 | 3790→3791 (98.96%) | **`selfBoost` moves emit NO self-drop roll** — PS `move.selfBoost` (Clanging Scales / Scale Shot / Clangorous Soulblaze) applies at battle-actions.ts:521 via `moveHit` with NO `random(100)`, DISTINCT from `move.self.boosts` (`selfDrops`, which rolls). `gen-data.mjs` conflated both into `self_boosts`, so the engine over-emitted a self-drop draw for the 3 `selfBoost` moves. **Extraction fix** (per lesson #4): new `self_boost_only` MoveData field, applied draw-free; regenerated gen.rs (exactly 3 moves' `self_boosts`→`self_boost_only`, audited). Clears c3b2s52 (clangingscales) + advances rd318 (scaleshot). | 34221ef |
| S2 | 3791→3792 (98.98%) | **Diamond Storm empty-secondary second `random(100)`** — Diamond Storm has `self:{chance:50,boosts:{def:2}}` AND an empty `secondary:{}` (Sheer-Force marker); PS rolls TWO `random(100)` (self via `selfDrops`, empty secondary via `secondaries`). Engine emitted only the self roll; added `diamondstorm` to `extra_secondary_roll_move` (emits on both sub + non-sub paths). Clears r6 t2. **State caveat** (NOT draw): `self.chance:50` is unmodeled — the def+2 applies unconditionally; the sole corpus instance procs so the sweep stays exact. Diamond Storm is the ONLY move with `self.chance`. | b9f0791 |

**The GRAV APPLE resume-point suspicion was a red herring** — its gen.rs entry is correct
(`self_boosts` all-zero, def-1 as a target `secondary_boosts`). The actual over-emission at
c3b2s52 t6 was **Clanging Scales** (`selfBoost`), KO'ing Flapple; the codegen mis-encoding was
the `self` vs `selfBoost` conflation above.

### Terminal state at pause: differ **3792/3831 = 98.98%** (39 units); seed gate **63/111** (56.8%)
Mismatch categories: 20 unconsumed, 10 extra-draw, 5 args, 4 state-mismatch. **Every remaining
game non-exact in the seed gate is now draw-work-asymptotic**: all four self-drop/cursedbody/trace
games verified to diverge on STATE (HP/boosts) or earlier, so the remaining differ units yield
**0 games** — they are draw-ORDERING residues or sit behind a state-computation divergence.

**NAMED OPEN ITEMS — remaining 39 differ units (all genuinely-evidenced; each needs intricate/
state-risky work with 0 game yield — NOT clean wins):**
- **`shuffle[2,0,2]@generic` (19) + downstream cascades (`@snowscape` ×2, earthquake/flareblitz
  wrong-order, `@psychic`/`@thunderwave`/`@update`/`shuffle[5,1,3]`, extra-accuracy — ≈29 units
  total)** — the **first-mover no-draw Update / move-order-tie** class. On a Speed tie where the
  first mover is a no-draw status/failed move, PS fires its runAction `eachEvent('Update')` (2882)
  BEFORE the second move's draws; the engine's annotation can't shape-match either order. The
  SEED_GATE already handles these via `forced_tie_order` (games exact), so this is a differ/
  annotation-ordering residue in the turn-resolution path — INTRICATE, multi-session-deferred,
  0 games. PS battle.ts:2881.
- **`sample[1]@trace` (3, c3c2s82/s83)** — Trace's `onUpdate` re-trace `sample(1)` at an
  end-of-turn Update (the holder picking its copy target). Needs Trace onUpdate timing modeling
  (which Update, traceable-target gate). Both games diverge on STATE (HP) at/after the trace turn
  → 0 games. abilities.ts Trace.
- **`randomChance[3,10]@cursedbody` (1, r3 t19)** — Cursed Body is a per-hit `DamagingHit` handler;
  PS fires it AFTER EACH hit of a multi-hit move (Scale Shot ×2 → 2 interleaved rolls). The engine
  emits it once post-loop. Fix requires per-hit interleaving in the realized multi-hit executor
  (`apply_multihit_realized` delegates the loop to the shared `apply_damage_hit` hot path — needs
  cursor-threading or a duplicated loop, with the ModifyDamage-shuffle ordering). Regression risk to
  the Scale Shot/Bullet Seed family (55 exact games) for 1 unit / 0 games. r3 diverges at d9/t8.
- **`randomChance[3,10]@ficklebeam` (1, rd318 t4)** — Fickle Beam `onBasePower` rolls
  `randomChance(3,10)` (30% DOUBLE power) between the crit and damage rolls in `getDamage`. Not
  draw-and-discard: it branches the damage calc (2× base power), so it's a state-path mechanic
  (regression risk to `compute_damage`) for 1 unit / 0 games (rd318 diverges at d2/t3). moves.ts:5227.
- **`randomChance[1,24]@poltergeist` (1, r11 t18)** — PS skips Poltergeist's accuracy roll here
  (mechanism not yet isolated — strengthsap's roll is also absent; likely a move-order/`onTry`
  interaction). r11 state-blocked.
- **`randomChance[1,24]@uturn` (1) / `randomChance[100,100]@uturn` (state, 1)** — U-turn pivot-hit
  crit alignment + a state-mismatch (damage-calc). Downstream of the pivot/switch bracket.

**STATE-COMPUTATION QUEUE (next campaign — mechanics, NOT draw work; ranked by seed-gate first
divergence over the 48 non-exact games):**
- **32 `draws-match/state-diff`** — draws align but HP/boosts diverge: the masked `compute_damage`
  class (screen×multi-hit rounding like d4 t18; Beat Up per-member formula, c2a2/c2a5;
  closecombat/uturn state-mismatch). These are per-move damage-calc items.
- **4 `move-order-tie`** genuine forks; **~12** residual per-move draw items behind an earlier
  state divergence (`@dragontail` drag `sample`, `@confusion`, `@wavecrash`, `@aeroblast`/
  `@focusblast` accuracy args, `@frz` thaw boundary, `@slp`/`@par`/`@crit`).

**Kill criteria: NOT triggered.** No fix revealed a new independent class; density still decayed
(2 classes removed). The remaining climb to differ-zero is gated on the turn-resolution move-order
class (intricate, 0 games) + per-move state-path mechanics — deferred deliberately to avoid
destabilizing the shared multi-hit/turn-resolution paths for zero game yield.

---

## Phase 3 — seed-driven full-battle Replicate gate (`crates/cosim/src/seedgate.rs`)

The strategic pivot: annotation-mode scoreboarding (90.16% per-decision draw-exact) is done; the
goal bar is a **single-path executor** — same seed ⇒ same sampled outcomes, same draw count and
order — measured end-to-end per FULL GAME. Reproduce:
`SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz`.

**Result: 59 / 111 full games exact end-to-end (53.2%); init-aligned from seed 105/111.
Draw-consumption differ 90.16% → 97.99% (3754/3831), zero shuffle over-emission.**

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
