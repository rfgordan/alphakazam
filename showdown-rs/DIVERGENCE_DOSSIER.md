# DRAW-EXACT — Divergence Dossier (Phase-3 triage)

Definitive first-divergence classification for **all 73 non-exact games** in the seed-driven
full-battle gate, so fix tranches start from specs, not diagnosis.

- PS pin: `b9dc987d`. Corpus: 111 traces / 3831 move units. Binary: `prng-exact` branch.
- State: **38 / 111 full-game exact** (`SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz`);
  draw-consumption differ at **90.16%** (per `DRAW_EXACT_SCOREBOARD.md`).
- **19 / 73 non-exact games are SHADOWED by the speedSort keystone** (their first divergence *is* a
  shuffle the running handler-order agent will model or shift). The other 54 are independent.

## Method

For every non-exact game I took two measurements:
1. `SEED_GATE=1` — the **first state divergence** (cumulative single-PRNG-stream Replicate): decision
   index, turn, and the diffed field.
2. `DRAW_DIFF=1 DRAW_DBG="]"` — the **ordered per-decision draw mismatches** (annotation mode,
   re-synced from PS state each decision). The *earliest* draw mismatch that either shifts the PRNG
   stream (an unconsumed/extra/wrong-kind draw) or changes state (a hit/miss-flipping accuracy) is the
   true root of the cumulative state divergence.
3. `DBG_GAME=<g> DBG_I=<i> SEED_GATE=1` — per-unit chosen-branch draw dump for the ambiguous cases,
   cross-checked against the trace's recorded `draws[]` (via `python3 -c "import gzip,json…"`).

**Key mechanism (why `draws-match/state-diff` dominates the raw SEED_GATE labels — 38 of 73):**
the gate runs ONE continuous `PsPrng` across the whole game. An earlier decision that under- or
over-consumes the stream (an unmodeled shuffle, a missing/extra secondary roll, a mis-placed duration
draw) leaves the stream **offset**; a later damaging move then reads its `random(16)` at the wrong
position and computes wrong HP even though its own draw *shapes* still match PS's recorded log. So the
SEED_GATE label points at the *victim* decision; the annotation differ points at the *source*. Example
— `c1` d21: chosen `[…acc,crit,dmg=7]` vs PS recorded `[…acc,crit,dmg=14]` (same shapes, shifted
value); the source is a `shuffle[2,0,2]@generic` at t17 the engine didn't emit.

Cosmetic mismatches ignored when picking the root: an accuracy-arg diff where **both** numerators are
`>=100` (e.g. rust `[100,100]` vs ps `[110,100]`) is stream-neutral AND state-neutral (both always
hit) — it never causes a divergence, only a label.

---

## Ranked task-spec table

| # | class | games | direct yield (games this is the first-divergence for) | est. → EXACT | risk | shadowed |
|---|-------|-------|------|------|------|------|
| 1b | **Mid-move / status-move / handler-order `speedSort` shuffles** (the keystone, running agent's core) | 11 | 11 | ~5 | med (over-emit regresses exact games) | ✅ |
| 1a | **Switch / `runSwitch` bracket `speedSort` shuffles** (keystone sub-item — may be outside running agent's scope) | 8 | 8 | ~7 | med (same invariant; new switch-path emission) | ✅ |
| 5 | **`ModifyAccuracy` args chain** (accuracy/evasion stages, Wide Lens/Compound Eyes; confusion self-hit `[33,100]`; frz-thaw `[1,5]` vs accuracy) | 13 | 13 | ~5 | med (4096 rounding; state-affecting when acc<100) | ❌ |
| 3 | **Secondary / effect / crit `random(100)` emission gap** (100%-secondaries, flinch, Diamond-Storm double-roll, U-turn/Tri-Attack crit) | 11 | 11 | ~5 | low (annotation-only draw-and-discard) | ❌ |
| 2 | **Variable multi-hit COUNT `sample` + per-hit crit/damage** (Bullet Seed/Icicle Spear/Rock Blast/Scale Shot/Tail Slap/Triple Axel + Beat Up per-hit) | 10 | 10 | ~4 | **high** (DP path shared with Enumerate/Sample) | ❌ |
| 9 | **rust-extra accuracy roll** (PS fails/immune before `hitStepAccuracy`: Counter/Mirror-Coat `onTry`, Spore/immunity vs wrong target) | 6 | 6 | ~3 | med (must not drop legit accuracy rolls) | ❌ |
| 4 | **Sleep / confusion DURATION placement** (`random[2,5]@slp`, `random[2,6]@confusion` emitted at the wrong stream position / as a secondary) | 6 | 6 | ~3 | low (reorder an already-emitted draw) | ❌ |
| 7 | **Init-alignment set-gender gap** (`align=false`; unlogged set gender on a dual-gender mon) | 6 | 6 | 6* | n/a (recorder/data, not engine) | ❌ |
| 6 | **Ability/item residual proc roll** (Toxic Chain `randomChance[3,10]`, Poison Touch, Cute Charm) | 1 | 1 | ~1 | low | ❌ |
| 8 | **Genuine state-compute / `replicate_select` selection** (byte-identical draws, HP still diverges) | 1 | 1 | ? | unknown — **investigate** | ❌ |

\* Class 7 goes exact only if the recorder/set-data gap is closed; not an engine mechanics fix.

**Rank by yield/risk (fix order):** 1b → 1a (keystone lands first; unblocks 19 and shifts ~6 onto the
tail classes) → **3** (11 games, lowest risk, pure annotation draw-and-discard) → **4** (6, low risk) →
**9** (6, med) → **5** (13, med — biggest single class but rounding-sensitive) → **6** (1) → **2** (10,
gated last: DP-path change is the only high-regression-risk item) → **7** (recorder) → **8** (triage).

**SHADOWED-by-keystone total: 19** (Class 1a 8 + Class 1b 11). Of these, ~13 are predicted to go
**fully exact** post-keystone (their entire mismatch list is shuffles); ~6 shift to a tail class
(see per-game "post-keystone" column below).

**Suspected genuinely-new: 1 game (`r12`)** — kill-criterion-relevant, detailed in §Class 8.

---

## Class specs

### Class 1a — Switch / `runSwitch`-bracket `speedSort` shuffles  · 8 games · SHADOWED
`c1 c2 c3 c3b2s53 c4 c6 d6 rd287`

- **Mechanism.** On a turn where at least one side *switches*, PS fires extra `eachEvent('Update')`
  shuffles the engine does not: the switch-out `eachEvent('Update')` (`sim/battle-actions.ts:83`), the
  `runSwitch` `getAllActive()` `speedSort` (`sim/battle-actions.ts:182`), and the `runAction`
  `eachEvent('Update')` after the switch (`sim/battle.ts:2882`). Each is a `shuffle[2,0,2]` **iff both
  actives are alive and share `effective_speed`**. The engine's next draw on such a turn is the
  incoming/other mon's accuracy, so it reads the shuffle's `random(0,2)` as its accuracy roll →
  stream offset for the rest of the game.
- **Signature.** All 8 first-mismatch on a `[Switch(n), Move]`/`[Move, Switch]`/`[Switch, Switch]`
  turn as `pos 0: rust randomChance[100,100]@accuracy vs ps shuffle[2,0,2]@generic`.
- **PS ref.** `sim/battle.ts:429` `speedSort` (selection sort; one `prng.shuffle(list,s,s+len)` per
  tie-group >1), `:404` `comparePriority`, `:465`/`:2882` `eachEvent`/`runAction`;
  `sim/battle-actions.ts:83`,`:182` runSwitch bracket.
- **Impl site.** `crates/engine/src/generate.rs` — the switch resolution in `generate_branches_ctx`
  (mirror the runSwitch/switch-out/switch-in Update schedule documented in the scoreboard's
  "COMPLETE per-turn schedule" item 3). Annotation-only, gated on the equal-`effective_speed`
  predicate on the *pre-swap* board for the switch-out Update and the *post-swap* board for the
  runAction Update.
- **Predicted post-keystone.** `c1 c2 c3 c3b2s53 c4 c6 rd287` → **EXACT** (entire list is shuffles).
  `d6` → shifts to **Class 2** (`sample[20]@bulletseed` at t40).
- **Risk.** Same as any shuffle work: over-emission instantly shows as a rust-side shuffle mismatch in
  the differ (the "no rust-side shuffle anywhere" invariant). Build per turn-shape, check the differ
  after each. NOTE: this is a **distinct code path from the running mid-move agent** — if that agent
  scopes only mid-move + status Updates, these 8 stay open and need this item.

### Class 1b — Mid-move / status-move / handler-order `speedSort` shuffles  · 11 games · SHADOWED
`c3c2s81 c5 c6a2s112 c7 d2 d4 t1 t2 t4 t5 t6`

- **Mechanism.** Two sub-cases, both the running agent's stated scope:
  1. **Status/secondary-move per-hit Update** (`sim/battle-actions.ts:970`) — a connecting
     status/secondary move fires an Update the engine omits: `ps shuffle[2,0,2]@recover / @calmmind /
     @moonblast / @blizzard / @foulplay / @surf`. The engine's post-effect "hit" branch is
     indistinguishable from a move that didn't `moveHit`, which is exactly why it was deferred.
  2. **Handler-order tie list-length** — the engine *does* emit a residual/handler shuffle but with the
     wrong list length: `rust shuffle[4,0,2] vs ps [2,0,2]`, `rust [4,2,4] vs ps [5,2,4]`,
     `rust [2,0,2] vs ps [3,1,3]`, `rust [3,0,2] vs ps [2,0,2]` (t1/t2/t4/t5/d2/r10). This is PS's
     per-event handler enumeration + `comparePriority` (order,priority,speed,subOrder,effectOrder) —
     the charter's "largest unknown".
- **PS ref.** `sim/battle-actions.ts:970` (per-hit `eachEvent('Update')`), `:1024` (post-hit-loop
  Update, KO-gated); `sim/battle.ts:404`/`:429`/`:950+` (comparePriority/speedSort/resolvePriority
  subOrder table).
- **Impl site.** `generate.rs` — the in-kernel 970 emission at the per-hit completion point of
  `execute_move` for status/secondary moves; the handler-list rebuild feeding
  `emit_residual_shuffles`-style selection sort with correct per-effect (order,subOrder) keys.
- **Predicted post-keystone.** `c5 c6a2s112 c7 d4 t2 t5 t6` → **EXACT**. `c3c2s81` → **Class 4**
  (slp at t26). `d2` → **Class 5** (frz `[1,5]` at t16) then residual shuffle-length. `t1` → **Class 5**
  (frz at t10). `t4` → **Class 4** (slp at t30).
- **Risk.** Handler-order length is the sensitive part; the per-hit 970 for status moves needs the
  "did moveHit run" signal. Over-fire measured net-negative historically — gate tightly, differ after.

### Class 2 — Variable multi-hit COUNT `sample` + per-hit crit/damage (DP path)  · 10 games · independent
`c3a2s21 c3c1s72 d1 d5 r20 r8 rd318 r2 r5 c2a4`

- **Mechanism.** For a `[2,5]` multihit, PS draws the hit count via `this.battle.sample([2,2,2,2,2,2,2,3,3,3,3,3,3,3,4,4,4,5,5,5])` (**`sim/battle-actions.ts:864`**, gen≥5 35/35/15/15 table)
  = one `sample[20]` draw, **then** enters the hit loop drawing crit `randomChance(1,24)` + damage
  `random(16)` *per hit*. The engine folds the count into a sumset-DP and emits **no** count sample and
  no per-hit stream, so the whole move under-consumes → stream offset. Beat Up (`c2a4`) and Triple
  Axel (`r8`) are the per-hit-crit variant (each strike its own `randomChance(1,24)`); Scale Shot
  (`rd318`) additionally has the `self` speed drop where the engine emits `random[100]@self-drop` at
  the count-sample position. `rd298` (U-turn crit unconsumed) and `r5` (crit-before-accuracy ordering
  on Rock Blast) are per-hit-crit-emission cousins.
- **PS ref.** `sim/battle-actions.ts:864` (count sample), `:886`+ hit loop, `getDamage` crit/damage.
- **Impl site.** The sumset-DP multihit path in `generate.rs` must, in the annotating/Replicate path,
  emit the count `sample` then per-hit crit+damage draws (respecting the KO/`slp` break at `:886`).
- **Risk. HIGH** — the DP path is shared with Enumerate/Sample; a naive per-hit expansion changes the
  branch set the state-sweep and distribution smoke exercise. Must stay behind `annotating()` and
  reconcile the DP's folded-count enumeration with the realized single count. Gate this LAST.

### Class 3 — Secondary / effect / crit `random(100)` emission gap  · 11 games · independent
`c1a c1b c2a1 c6a1s104 r14 r15 r6 rd316 c3c1s73 c3b2s52 rd298`

- **Mechanism.** PS rolls a `random(100)` (or `randomChance`) the engine doesn't emit, for effects the
  engine realizes deterministically: target secondaries on specific branches (`@thunderbolt`,
  `@ironhead` flinch), 100%-secondaries (`@ceaselessedge` hazard, `@sludgewave`, `@direclaw`), the
  Future Sight placement `randomChance[100,100]@futuremove`, Tri Attack's status `random(100)@triattack`,
  and **Diamond Storm's DOUBLE roll** (`@diamondstorm` rolls `random(100)` for its `self` boost *and*
  its `secondary` — the engine emits one). `c3b2s52` is the inverse (rust **extra** `random[100]@self-drop`
  — an over-emission on a move whose self-drop PS resolves without a roll). `c3c1s73` skips Leech
  Seed's accuracy against a specific target (emission gap, not extra).
- **PS ref.** `sim/battle-actions.ts` `secondaries()` / `selfDrops()` (`:1338`) — one `random(100)` per
  surviving secondary/self-boost as long as the target object is present; `data/moves.ts` per-move
  `secondary`/`self` shapes.
- **Impl site.** `apply_target_secondary` / the `selfDrops` site in `generate.rs` (same gate family as
  the landed Salt Cure / Rapid Spin 100%-secondary fixes) — extend `extra_secondary_roll_move()` and
  add the Diamond-Storm second roll; remove the `c3b2s52` over-emission.
- **Risk. LOW** — annotation-only draw-and-discard, respecting the existing Shield Dust / Covert Cloak
  / Sheer Force strips.

### Class 4 — Sleep / confusion DURATION placement  · 6 games · independent
`c3a2s22 c3a2s23 c3c2s83 r17 rd292 r19`

- **Mechanism.** PS rolls the sleep duration `random(2,5)` (and confusion `random(2,6)`) at the
  condition's `onStart`. The engine emits the duration draw (that class landed) but at the **wrong
  stream position** when sleep/confusion is applied as a *secondary* (post damage/secondary), so it
  reads as `pos 0` where PS has it later, or vice-versa — a placement/ordering slip, not a missing
  draw. `r19` = confusion `random[2,6]` at t7; the rest are `random[2,5]@slp` mis-placed.
- **PS ref.** `data/conditions.ts` `slp.onStart` `this.battle.random(2,5)`, `confusion.onStart`
  `random(2,6)`; ordering follows the secondary that inflicts it in `battle-actions.ts` `secondaries()`.
- **Impl site.** `branch_sleep_counter` / `branch_confusion_counter` emission point in `generate.rs` —
  move the draw to the secondary-application ordering rather than the move-start ordering.
- **Risk. LOW** — reorders an already-emitted draw.

### Class 5 — `ModifyAccuracy` args chain (+ confusion self-hit, frz-thaw)  · 13 games · independent
`c1c c3a1s12 c3a1s13 c3c1s71 c6a1s108 c6a2s113 r11 r18 r3 r4 c3c2s82 c6a2s114 r10`

- **Mechanism.** PS's `hitStepAccuracy` rolls `randomChance(accuracy,100)` with accuracy AFTER
  `onModifyMove`/`ModifyAccuracy` (accuracy/evasion **stages** and items via the ×4096 chain). The
  engine emits raw `md.accuracy`, so args diverge: `[100,100]` vs `[75,100]` (evasion +1, `c3c1s71`),
  `[100,100]` vs `[166,100]`/`[133,100]` (accuracy stages, `c3a1s12/13/r10`), `[90,100]` vs `[99,100]`
  (Wide Lens, `c6a2s113/r7`). **State-affecting whenever the true accuracy <100** (the realized roll
  can miss where the engine hits). Two adjacent quirks folded here: **confusion self-hit** — the engine
  rolls `random[16]@confusion-damage` (decided the mon hit itself) where PS's next draw is the move's
  accuracy (`c3c2s82` — confusion `randomChance[50,100]`/`[33,100]` self-hit rate); **frz-thaw args** —
  the engine emits `randomChance[1,5]@frz` where PS emits accuracy or Curse's `random(100)`
  (`c1c c3a1s13 c6a1s108 c6a2s114 r18 r3 r4`), a same-turn-freeze / defrost decision-boundary.
- **PS ref.** `sim/battle-actions.ts` `hitStepAccuracy` + `runEvent('ModifyAccuracy')`; the 4096 boost
  table in `sim/battle.ts` (`chainModify`/`modify`); `data/conditions.ts` `frz.onBeforeMove` (defrost),
  `confusion.onBeforeMove` self-hit.
- **Impl site.** Extend `accuracy_arg()` in `generate.rs` (already models Hustle + sun-halved) with the
  accuracy/evasion stage lookup and Wide Lens/Compound Eyes ×4096 rounding; fix the confusion self-hit
  and defrost decision boundaries.
- **Risk. MED** — 4096 rounding must be integer-exact; the frz/confusion cases change state (branch
  selection), so re-verify state-sweep. Cosmetic (both-≥100) sub-cases carry no state risk.

### Class 9 — rust-extra accuracy roll (PS fails before `hitStepAccuracy`)  · 6 games · independent
`c2a2 c2a3 c2a5 c2a6 d7 d8`

- **Mechanism.** The engine emits `randomChance[acc,100]@accuracy` where PS makes **no** accuracy roll
  because the move fails earlier: Counter/Mirror Coat `onTry` fail (no qualifying damage taken), Spore
  / Thunder-Wave-family immunity or paralysis evaluated against the wrong (pre-switch) target, etc. The
  extra draw shifts the stream → downstream `draws-match/state-diff`. (`c2a2`/`c2a5` surface as
  `draws-match`, `c2a3`/`c2a6`/`d7`/`d8` surface directly as the `rust-extra` label — same root.)
  `d8` additionally skips Spore's accuracy on another turn (`ps unconsumed randomChance[100,100]@spore`).
- **PS ref.** `sim/battle-actions.ts` hit-step order — accuracy is rolled only AFTER
  `hitStepTryImmunity` + `onTry`; `data/moves.ts` `counter`/`mirrorcoat` `onTry`.
- **Impl site.** The `reaches_accuracy` / `accuracy_forced_true` predicate family in `generate.rs` —
  add the pre-accuracy fail conditions (no qualifying damage for Counter/Mirror Coat; immunity vs the
  correct current target).
- **Risk. MED** — must not drop accuracy rolls the move legitimately makes; verify against the landed
  "accuracy only when move reaches hitStepAccuracy" class.

### Class 6 — Ability/item residual proc roll  · 1 game · independent
`r7`

- **Mechanism.** The engine emits accuracy `[100,100]` where PS's next draw is a Toxic Chain
  `randomChance(3,10)` proc (`onAfterMoveSecondary`); the ability proc roll isn't emitted → offset.
  Same family as the landed Poison Touch / Cute Charm items (which also appear later in `r4`/`c5b2`).
- **PS ref.** `data/abilities.ts` `toxicchain.onAfterMoveSecondarySelf` `randomChance(3,10)`.
- **Impl site.** The contact/after-move ability roll site in `generate.rs` (same structure as the
  landed Cursed Body / Effect Spore rolls).
- **Risk. LOW.**

### Class 7 — Init-alignment set-gender gap (`align=false`)  · 6 games · data gap
`c5a1 c5a2 c5b1 c5b2 c5c1 c5c2`

- **Mechanism.** These 6 custom-directed-team games are the documented residual of the pre-turn-1
  offset: a **set-specified gender on a dual-gender mon** (Breloom `|M|`, the loyal-three, …) is
  indistinguishable in the snapshot from a rolled gender, so `init_gender_rolls()` burns the wrong
  number of construction `sample(["M","F"])` draws (`sim/pokemon.ts:116`), and the whole stream is
  offset from d1 (all diverge at d1, `align=false`; downstream shuffle/bulletseed labels are
  artifacts of the misalignment).
- **Fix.** Not an engine mechanics fix — needs a recorder field for the set gender, or the team-set
  data threaded into `fixed_gender`. Documented in `DRAW_EXACT_SCOREBOARD.md`.
- **Kill-criterion note.** This is an *observability* gap (the set isn't in the trace), NOT hidden
  battle state — resolvable with a one-field recorder change. Not kill-criterion #1.

### Class 8 — Genuine state-compute / `replicate_select` selection  · 1 game · SUSPECTED NEW
`r12`

- **Signature (kill-criterion-relevant).** `r12` has **zero** draw mismatches across the entire game
  (annotation differ clean) AND at its divergence (d42 t35) the chosen branch's draws are
  **byte-identical to PS** — `randomChance[100,100]=1@accuracy randomChance[1,24]=0@crit
  random[16]=8@damage-roll`, exactly matching the recorded `[acc=true, crit=false, dmg=8]` — yet
  `s0#2.hp` diverges. This is NOT a PRNG/offset issue.
- **Context.** d42 is `p1: switch 6` + `p2: move stompingtantrum`. 32 outcomes, `chosen=8`; the
  state-verifier confirms *some* branch reproduces PS's `stateAfter` with the same draws, but
  `replicate_select` picked a different one. Two draw-equivalent branches differ in HP → the split is
  a **non-drawn** decision the engine branches on: most likely the switch-target resolution (which mon
  comes in / which takes the hit) or a Stomping-Tantrum power-doubling ("previous move failed") state
  the engine models differently. It is a deterministic mechanic/selection bug, not unobservable state.
- **Assessment.** Isolated (1 game). Kill-criterion #1 (unobservable state) is **NOT** triggered — the
  branch key is reconstructable. Needs a targeted investigation: dump all 32 outcomes for `r12` d42
  (`DBG_GAME=r12.json DBG_I=42`) and compare each branch's `s0#2.hp` to find the mis-selected split.
  Flagged as the one genuinely-new item; low severity, high diagnostic value.

---

## Full per-game appendix (73 games)

Columns: game · SEED_GATE first divergence (decision/turn, diffed field) · class · root (earliest
divergence-causing draw mismatch) · post-keystone (for shadowed games).

| game | SEED_GATE first-div | class | root / note |
|------|--------------------|-------|-------------|
| c1 | d21/t18 s0#1.hp | 1a | t17 ps shuffle@generic (switch) → **EXACT** |
| c2 | d16/t14 s1#5.hp | 1a | t14 ps shuffle@generic (switch) → **EXACT** |
| c3 | d9/t9 s0#5.hp | 1a | t9 ps shuffle@generic (switch) → **EXACT** |
| c3b2s53 | d27/t23 s1#3.hp | 1a | t23 ps shuffle@generic (switch) → **EXACT** |
| c4 | d27/t22 s1#5.hp | 1a | t22 ps shuffle@generic (switch) → **EXACT** |
| c6 | d5/t5 s1#3.hp | 1a | t4 ps shuffle@generic (switch) → **EXACT** |
| d6 | d31/t30 s0#2.hp | 1a | t24 ps shuffle@generic (switch) → **Class 2** (bulletseed t40) |
| rd287 | d4/t5 s0#0.hp | 1a | t4 ps shuffle@generic (switch) → **EXACT** |
| c3c2s81 | (draws-match) | 1b | t11 ps shuffle@moonblast → **Class 4** (slp t26) |
| c5 | d37/t31 s0#5.hp | 1b | t30 ps unconsumed shuffle@generic → **EXACT** |
| c6a2s112 | d6/t5 s1#1.hp | 1b | t4 ps unconsumed shuffle@generic → **EXACT** |
| c7 | d29/t27 s0#5.hp | 1b | t26 ps unconsumed shuffle@recover → **EXACT** |
| d2 | d13/t12 s0#3.hp | 1b | t10 ps shuffle@blizzard → **Class 5** (frz t16) + shuffle-len |
| d4 | d11/t12 s1#4.hp | 1b | t10 ps unconsumed shuffle@foulplay → **EXACT** |
| t1 | d6/t6 s0#1.hp | 1b | t4 ps unconsumed shuffle@generic → **Class 5** (frz t10) |
| t2 | d2/t3 s0#0.hp | 1b | t2 rust shuffle[4,0,2] vs ps[2,0,2] (handler-len) → **EXACT** |
| t4 | d33/t32 s0#0.hp | 1b | t12 rust shuffle[4,2,4] vs ps[5,2,4] → **Class 4** (slp t30) |
| t5 | d69/t69 s0.volatiles | 1b | t75 rust shuffle[4,2,4] vs ps[5,2,4] → **EXACT** |
| t6 | d4/t5 s1#0.hp | 1b | t4 ps unconsumed shuffle@generic → **EXACT** |
| c3a2s21 | d6/t6 s1#0.hp | 2 | t7 ps unconsumed randomChance[1,24]@iciclespear (per-hit) |
| c3c1s72 | d15/t13 s0#4.hp | 2 | t13 ps unconsumed sample[20]@iciclespear |
| d1 | d9/t10 s0#3.hp | 2 | t10 ps unconsumed sample[20]@bulletseed |
| d5 | d17/t17 s1#1.hp | 2 | t15 ps unconsumed sample[20]@bulletseed |
| r20 | d10/t9 s0.volatiles | 2 | t22 ps unconsumed sample[20]@bulletseed |
| r8 | d23/t18 s1#2.hp | 2 | t15 ps unconsumed randomChance[1,24]@tripleaxel |
| rd318 | d2/t3 s0.active_index | 2 | t4 rust self-drop vs ps sample[20]@scaleshot |
| r2 | d2/t3 s0#0.hp | 2 | sample[20]@tailslap (enum-explodes; count+per-hit) |
| r5 | d2/t3 s1.volatiles | 2 | t7 rust crit[1,24] vs ps accuracy (rockblast per-hit order) |
| c2a4 | d2/t3 s0#4.hp | 2 | t3 rust accuracy vs ps randomChance[1,24]@beatup (per-hit) |
| c1a | d7/t5 s0#5.hp | 3 | t4 ps unconsumed random[100]@thunderbolt |
| c1b | d53/t44 s1#3.hp | 3 | t40 ps unconsumed random[100]@ironhead (flinch) |
| c2a1 | d9/t8 s1#1.hp | 3 | t8 ps unconsumed randomChance[100,100]@futuremove |
| c6a1s104 | d15/t10 s0#5.hp | 3 | t10 ps unconsumed random[100]@ceaselessedge |
| r14 | d4/t5 s0#2.hp | 3 | t5 ps random[100]@sludgewave (100%-secondary) |
| r15 | d22/t21 s0#2.hp | 3 | t21 ps unconsumed random[100]@direclaw |
| r6 | d2/t3 s1#2.hp | 3 | t2 ps unconsumed random[100]@diamondstorm (DOUBLE roll) |
| rd316 | d2/t3 s0#2.hp | 3 | t2 ps unconsumed random[100]@triattack |
| c3c1s73 | d9/t9 s0#0.hp | 3 | t6 ps unconsumed randomChance[90,100]@leechseed (accuracy skip) |
| c3b2s52 | d8/t8 s1#3.hp | 3 | t6 rust **extra** random[100]@self-drop (over-emit) |
| rd298 | d3/t3 s0#0.hp | 3 | t1 ps unconsumed randomChance[1,24]@uturn (crit skip) |
| c3a2s22 | d13/t11 s1#2.hp | 4 | t11 rust accuracy vs ps random[2,5]@slp (placement) |
| c3a2s23 | d9/t8 s0#1.hp | 4 | t11 ps unconsumed random[2,5]@slp |
| c3c2s83 | d25/t23 s1#2.hp | 4 | t21 ps unconsumed random[2,5]@slp |
| r17 | d34/t26 s1#4.hp | 4 | t25 ps unconsumed random[2,5]@slp |
| rd292 | d3/t4 s0#0.hp | 4 | t3 ps unconsumed random[2,6]@confusion |
| r19 | d15/t11 s0#2.hp | 4 | t7 ps unconsumed random[2,6]@confusion |
| c1c | d35/t27 s0#3.types | 5 | t1 rust frz[1,5] vs ps accuracy (defrost boundary) |
| c3a1s12 | d22/t20 s0#4.hp | 5 | t20 rust[95,100] vs ps[100,100] accuracy stage (+ burningjealousy t39) |
| c3a1s13 | d25/t21 s0#2.status_counter | 5 | t18 rust frz[1,5] vs ps[90,100] |
| c3c1s71 | d9/t8 s1#0.hp | 5 | t17 rust[100,100] vs ps[75,100] (evasion +1) |
| c6a1s108 | d29/t24 s1#3.hp | 5 | t5 rust frz[1,5] vs ps[100,100] |
| c6a2s113 | d2/t3 s0#0.hp | 5 | t3 rust[90,100] vs ps[99,100] (Wide Lens) |
| r11 | d25/t18 s1.volatiles | 5 | t18 rust[100,100] vs ps[1,24]@poltergeist (crit/acc swap) |
| r18 | d17/t13 s1#2.hp | 5 | t9 rust frz[1,5] vs ps[90,100] |
| r3 | d9/t8 s0.volatiles | 5 | t5 rust frz[1,5] vs ps[85,100] |
| r4 | d20/t19 s1.volatiles | 5 | t5 rust frz[1,5] vs ps[100,100] (+ poisontouch t26) |
| c3c2s82 | d23/t21 s1#5.hp | 5 | t14 rust confusion-damage vs ps[95,100]@strangesteam (self-hit) |
| c6a2s114 | d47/t39 s0#1.hp | 5 | t43 rust frz[1,5] vs ps random[100]@curse |
| r10 | d23/t19 s1#2.hp | 5 | t17 ps unconsumed randomChance[133,100]@trick (acc stage) + shuffle t23 |
| r7 | d3/t3 s1.boost.spd | 6 | t3 rust accuracy vs ps randomChance[3,10]@toxicchain |
| c5a1 | d1/t2 s1.active_index | 7 | align=false set-gender |
| c5a2 | d1/t2 s1.active_index | 7 | align=false set-gender |
| c5b1 | d1/t2 s1#0.status_counter | 7 | align=false set-gender |
| c5b2 | d1/t2 s0#0.hp | 7 | align=false set-gender |
| c5c1 | d1/t2 s0#0.hp | 7 | align=false set-gender |
| c5c2 | d1/t2 s0#3.hp | 7 | align=false set-gender |
| c2a2 | d7/t5 s1#5.hp | 9 | t3 rust extra randomChance[100,100]@accuracy |
| c2a3 | d5/t6 s1#3.hp | 9 | t5 rust extra randomChance[100,100]@accuracy |
| c2a5 | d6/t6 s1#0.hp | 9 | t5 rust extra randomChance[100,100]@accuracy |
| c2a6 | d13/t11 s1#3.hp | 9 | t10 rust extra randomChance[100,100]@accuracy |
| d7 | d23/t22 s0#5.status | 9 | t22 rust extra randomChance[90,100]@accuracy |
| d8 | d25/t24 s0#5.status | 9 | t24 rust extra randomChance[90,100]@accuracy (+ spore t26) |
| r12 | d42/t35 s0#2.hp | 8 | **zero draw mismatches; byte-identical draws, HP diverges — investigate** |

---

## Kill-criterion assessment

**Not triggered.** Every class is a known, finite, state-reconstructable draw-accounting item:
- The 19 shuffle-shadowed games clear on the keystone (a deterministic-by-seed, label-audited PS
  mechanism); ~13 go fully exact, ~6 shift onto the tail classes below.
- The 5 independent draw classes (2,3,4,5,9) + Class 6 are the same structured, decaying tail the
  Phase-2 burn-down has been draining — each needs a draw *emitted/placed/argued* correctly, none
  needs hidden state.
- Class 7 is an observability/recorder gap (documented), not hidden battle state.
- The **single** genuinely-new signature (`r12`, byte-identical draws + HP diff) is a
  `replicate_select` / deterministic-mechanic selection bug whose branch key is reconstructable — NOT
  unobservable state. It is the one item to watch, but isolated to 1 game.

No fix in this dossier revealed a new *non-decaying* mismatch class; density continues to decay.
