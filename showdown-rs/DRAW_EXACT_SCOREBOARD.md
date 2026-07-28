# DRAW-EXACT — Phase 1 first scoreboard

Reproduce: `bash harness/gate-912.sh [out-nonexact-set-file]` — the customgame rail in one
command. **`bash harness/gate-rb.sh [out-set]`** — the real-format rail. PS pin: `b9dc987d`.

**Current: 905 / 912 customgame (99.2%) and 1413 / 1500 randbats (94.2%). Read the RANDBATS-1500
TRANCHE section immediately below before anything else.**

**Two corpora now, and they are not interchangeable.**

* **2412 games total.** The customgame rail is 912: 111 audited traces / 3831 move units
  (`harness/cosim-traces/`), 401 seed fixtures (`harness/seed-fixtures/`, seeds 1000-1400) and 400
  more (`harness/seed-fixtures-fresh/`, seeds 1401-1800). The randbats rail is **1500**:
  `harness/seed-fixtures-rb/` (101, seeds 5001-5100 + 5139),
  `harness/seed-fixtures-rb-fresh/` (399, seeds 5101-5500 minus the already-pinned 5139) and
  `harness/seed-fixtures-rb-1000/` (1000, seeds 5501-6500).
* Every one of the 912 STAMPS `format: gen9randombattle` and was **played as `gen9customgame`**,
  because `cosim.mjs` used to rewrite the formatid.
  `cosim::trace::ruleset_for` keys off a separate, explicit `ruleset` field for exactly this
  reason; absent ⇒ customgame. Only the 1500 are real random battles.

---

# ==== RANDBATS-1500 TRANCHE — the format rail TRIPLES, and the deferral bill comes due (2026-07-28) ====

**HEADLINE: 1413 / 1500 randbats (94.2%) and 905 / 912 customgame (99.2%, UNMOVED for the
SIXTEENTH consecutive parity commit).** One corpus commit, **six parity commits (8 games)** and one
follow-up fix, newly-non-exact EMPTY on BOTH rails at every one, audited **111/111** at every one.

## Final gate numbers (re-run at the certifying commit `be55929`)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT) |
| **Seed gate, all 912 customgame** | `bash harness/gate-912.sh` | **905 / 912 = 99.2%** (SET identical at every commit) |
| Seed gate, randbats pinned 101 | (inside `gate-rb.sh`) | **98 / 101 = 97.0%** |
| Seed gate, randbats fresh 399 | (inside `gate-rb.sh`) | **377 / 399 = 94.5%** (was 374) |
| Seed gate, randbats new 1000 | (inside `gate-rb.sh`) | **938 / 1000 = 93.8%** (opened at 933) |
| **Seed gate, randbats 1500** | `bash harness/gate-rb.sh` | **1413 / 1500 = 94.2%**; init-aligned **1500 / 1500** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS **100.00%**, coverage 100.00% |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3816 / 3831 = 99.61%**; **zero `rust extra`** |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | all 26 targets green |

**The randbats rail is now 1500 games.** Seeds 5501-6500 were recorded and their fixtures
committed to `harness/seed-fixtures-rb-1000/`. `gate-rb.sh` runs all THREE dirs and now COUNTS the
total from the globs instead of hardcoding it. Determinism checked, not assumed: rebuilding 59 of
the already-pinned fixtures from their sidecars reproduces the committed files BYTE-IDENTICALLY.

| # | commit | what | rb | 912 |
|---|--------|------|----|-----|
| 1 | `0143dd8` | **Knock Off's step-8 guard is read AFTER the step-7 chip** — and Magician / Pickpocket ride with it | 473/500 | 905 |
| 2 | `23f2381` | **Cursed Body reads the PRE-HIT ability** — a faint had already reverted the Imposter transform | 474/500 | 905 |
| 3 | `17d5be1` | a **`sleepUsable` move does not short-circuit the BeforeMove ladder** | 475/500 | 905 |
| 0 | `384cd88` | **corpus**: 1000 fresh randbats fixtures; first gate **1408 / 1500** | 1408 | 905 |
| 4 | `feb67db` | a **FIXED-DAMAGE move into an intact Ice Face / Disguise** rolls neither crit nor damage | 1410 | 905 |
| 5 | `c287c25` | **`choicelock` clears ITSELF at endTurn** — and it is still in the DisableMove sort when it does | 1413 | 905 |
| 6 | `be55929` | fix: that step's STATE effect must run with annotation OFF (the sweep rail) | 1413 | 905 |

## The finding: FOUR of the five roots are the DEFERRAL bill, and the campaign has now paid it twice

The engine's move pipeline runs `apply_post_damage` (drain / recoil / Life Orb / Knock Off /
Magician / Pickpocket / the faint reverts) at the END of the hit loop, and DEFERS
`runEvent('DamagingHit')` — step 7 — past the caller's step-5 secondary split. PS's order is the
reverse on both counts. Every deferred handler therefore stands one PS line out of place, and this
tranche found three separate consumers of that one displacement:

1. **A guard read too early** (rb5164). `after_hit_user_alive` is snapshotted before step 7 has
   run, so a Knock Off user killed by a step-7 Rocky Helmet chip still looked alive at step 8.
2. **An input read too late** (rb5199). `apply_cursed_body` read the holder's ability LIVE, and
   `apply_post_damage`'s faint block had already run `revert_transform` on the Imposter Ditto that
   copied Cursed Body — so the deferred handler saw a plain Ditto.
3. **A sibling left behind** (rb5267 / rb5217 / rb5335). Moving Knock Off's `takeItem` to the
   step-7/8 boundary and leaving Pickpocket in `apply_post_damage` split an ORDERED PS sequence:
   Knock Off strips the target, and Pickpocket then finds an itemless holder and steals the
   attacker's item in its place.

> **A deferred handler must be HANDED its inputs, not read them — and everything PS orders around
> it has to move with it.** `apply_damaging_hit_reactions` has taken `def_item` / `def_ability` as
> parameters since the beginning, and the Gulp Missile hoist is a third witness. Cursed Body was
> the copy that still read live state.

Three engine-side answers now exist to "is the attacker standing", and they are all different:
`is_alive()` (post-orb), `after_hit_user_alive` (pre-step-7) and `step8_user_alive`
(`hp + late_self_damage > 0`) — the composition, and the only one that is PS's `pokemon.hp` at
`battle-actions.ts:1144`.

## Four more rules this tranche cost a landing each

1. **`data/conditions.ts:77` — slp's `onBeforeMove` ends `if (move.sleepUsable) return;` before its
   `return false`.** For Sleep Talk and Snore it returns UNDEFINED, so `runEvent` does not
   short-circuit and everything below priority 10 still runs: Truant (9), Disable / Taunt (7/6/5),
   **confusion (3) with its `time--`**, Attract (2), paralysis (1). rb5386 d6 t7: a Snorlax with
   ONE confusion turn left Sleep Talks, PS decrements to 0, removes the volatile and rolls no
   `randomChance[33,100]` at all. This is the third arm of the same routing rule
   (`before_move_lower_ladder`) — slp and frz return `undefined` on a WAKE or a THAW too — and the
   tick had to move UP into `execute_move`, because a confusion / Attract / paralysis cancel
   branch never reaches `execute_move_inner` and PS ticked at priority 10 regardless.
2. **`getDamage` returns on `ohko` / `damageCallback` / `damage` BEFORE the crit roll**
   (`battle-actions.ts:1608-1615`). A fixed-damage move never reaches the crit `randomChance` or
   the `random(16)` — so the two draw-and-discards the Ice Face / Disguise arms emit have nothing
   to discard. rb6463 d4 t4: a Seismic Toss into a switching-in Eiscue is ONE draw in PS and was
   three in the engine.
3. **A condition that clears ITSELF clears LATE, and `runEvent` sorts before it runs.** PS's
   `choicelock` has no `onEnd` and no item hook; it removes itself from inside `onDisableMove`
   (`endTurn`, `battle.ts:1688`) and `onBeforeMove`. `runEvent` COLLECTS and speed-sorts the
   handler list first, so the doomed handler is still in the sort. `on_item_lost` removed the
   volatile eagerly at the Knock Off and deleted it from the count. rb6454 d11 t12 named it;
   rb5869 and rb5909 came with it as pure stream offsets.
4. **`Mold Breaker` nulls only `breakable` abilities, and the `def_ab` the immunity checks use is
   not a general-purpose capture.** Passing it to Cursed Body cost rb1403 (a TURBOBLAZE Reshiram
   Outrages a Banette and PS rolls the Cursed Body chance all the same) and had to be replaced
   with the raw pre-hit ability.

## The Roar phaze: DIAGNOSED, implemented, and REVERTED on the rails' verdict

rb5523 d31 t27 and rb5933 d28 t26 both record `shuffle[2,0,2]@roar`, `shuffle[2,0,2]@roar`,
`sample[n]@roar` on a Speed-tied board where the engine records only the sample. The mechanism is
certain: a landing phaze reaches the per-hit 970 Update and the post-hit-loop 1024 before
`dragIn`, and a REFUSED one reaches neither (`forceSwitch` writes `damage[i] = false` for a Status
move, `hitStepMoveHitLoop` breaks at `:950`, and `if (hit === 1) return damage.fill(false)` skips
the 1024). Emitting them, guarded on the drag actually landing, restored rb1750 / rb1467 and moved
rb5523 from d31 to d32 and rb5933 from d28 to d32.

**It still lost rb6013 net, so it is not in the tree.** rb6013 d9 exposes why: the engine's
POST-drag shuffles (`emit_drag_switchin_sort`'s `runSwitch` sort and the action's trailing 2882)
fire where PS's do not, and the game was passing only because two wrong shuffles after the sample
happened to equal two right shuffles before it. **The phaze Update fix cannot land until the
post-drag pair is right.** That is the named open for the next tranche, with its own witness.

## The 87 remaining randbats opens + 7 customgame

The seven customgame opens are UNCHANGED for the third tranche running: `rb1011 rb1012 rb1525
rb1572 rb1581 rb1681 rb1769`.

**57 of the 87 randbats opens are `draws-match/state-diff`** — the damage/HP asymptote, now 65% of
the population. The 30 with a named draw label are:

| bucket | n | games |
|---|---|---|
| `result random[16]@X` (stream OFFSET — `PRNG_TRACE` first) | 13 | rb5037 rb5146 rb5350 rb5400 rb5465 rb5605 rb5893 rb6098 rb6131 rb6246 rb6282 rb6292 rb6317 |
| **Struggle** — a forced Struggle under Encore / Disable / Heal Block | **3** | rb5142 rb5214 rb5927 |
| **Imposter Ditto switch-in Speed tie** (`PS shuffle@generic` ahead of the first accuracy; the recorded groups show Ditto at the foe's EXACT Speed) | **2** | rb5424 rb5936 |
| **Roar phaze Updates** (diagnosed above; blocked on the post-drag pair) | **2** | rb5523 rb5933 |
| `PS randomChance@<move>` where rust has `shuffle@update` | 2 | rb5940 rb5963 |
| Bullet Seed | 2 | rb5301 rb6421 |
| move-order tie (the LAST member) | 1 | rb5100 |
| `args randomChance[1,4]@par` | 1 | rb5358 |
| Shed Tail | 1 | rb5268 |
| Scale Shot | 1 | rb5982 |
| Future Sight | 1 | rb6260 |
| **`rust-extra randomChance[1,4]@par`** | 1 | rb6117 |

**rb6117 is the one `rust extra` on the rail and it is NOT an over-emission root.** d41 t32: a
Tera-STEEL Kilowattrel's Discharge should deal 272 to a 244-HP Yanmega on a roll of 0 and KO it;
the engine deals 240, the Yanmega survives on 4, gets paralysed, and its Bug Buzz rolls four draws
PS never rolls. **The extra draw is a damage bug wearing an invariant violation** — the whole unit's
recorded draws match up to and including the `random[16] = 0`, so the pre-roll damage is 272 in PS
and 267-ish in the engine. Take it as a damage-formula game (Tera STAB on a move whose type matches
the ORIGINAL type but not the Tera type), not as a draw-emission game.

## Asymptote assessment

**The kill criterion was not approached: six parity commits, six non-zero yields** (1, 1, 1, 2, 3
and the corpus commit's own baseline). No commit flipped zero on the randbats rail. **All six
flipped exactly zero on the customgame rail — sixteen consecutive parity commits now, across two
tranches, with the 912 SET byte-identical at every single one.** The rails are not merely
decoupled; the customgame corpus is inert.

**The 1000 new games opened at 93.3%, against the previous fresh 399's 94.5% and the pinned 101's
97.0%.** The marginal return on corpus size has NOT fallen — it is still the case that the newest
games are where the bugs are, and 1000 games bought 67 opens where 400 bought 25.

**Recommendation for the next tranche, in order:**

1. **The post-drag shuffle pair (rb6013 d9), then land the phaze Updates (rb5523, rb5933).** The
   phaze half is written, tested and reverted in this tranche's history; it needs only the
   post-drag half to stop cancelling it. Three games, one mechanism, and the diagnosis is done.
2. **The Imposter Ditto switch-in Speed tie (rb5424, rb5936).** Both recorded groups show the
   Ditto at the foe's exact Speed in a `shuffle[2,0,2]` the engine does not emit. NOTE the
   endgame-tranche warning: folding Imposter into the switch bracket's post-`SwitchIn` Speed
   refresh once regressed EIGHT games, because `transformInto` calls `setSpecies` BEFORE it copies
   `storedStats`. This is a SORT-time question, not a cache-refresh question.
3. **The Struggle class is now THREE (rb5142, rb5214, rb5927)** — the largest structural bucket
   left. rb5142's `midTurn: true` still wants the mid-turn request semantics pinned, and the same
   answer applies to rb1681's Wish counter.
4. **rb6117 as a DAMAGE game**, not an over-emission game: Tera-Steel Kilowattrel, Discharge,
   272 vs ~267 pre-roll.
5. **`PRNG_TRACE` the 13 `result random[16]` games before reading a single state diff.** Two of
   this tranche's five roots (rb6463, rb6454) were found that way, and both turned out to be a
   draw-COUNT bug several units upstream of the label.
6. **Recording more seeds is STILL the highest-yield buy** and this is the third tranche in a row
   to measure it. 6501-8500 would be the natural next block.


---

# ==== RANDBATS-500 TRANCHE — the format rail quintuples, and it is where the bugs are (2026-07-28) ====

**HEADLINE: 472 / 500 randbats (94.4%, from 96/101 = 95.0% on a corpus one fifth the size) and
905 / 912 customgame (99.2%, UNMOVED).** One corpus commit and **twelve parity commits, 15 games,
newly-non-exact EMPTY on BOTH rails at every one, audited 111/111 at every one.**

**The randbats rail is now 500 games** — `harness/seed-fixtures-rb/` (101, seeds 5001-5100 + 5139)
plus `harness/seed-fixtures-rb-fresh/` (399, seeds 5101-5500 minus the already-pinned 5139).
`gate-rb.sh` runs both and unions the non-exact SET, exactly as `gate-912.sh` does. Determinism was
checked, not assumed: rebuilding the pinned 101 from their sidecars reproduces the committed
fixtures BYTE-IDENTICALLY.

| # | commit | what | rb | 912 |
|---|--------|------|----|-----|
| 0 | `05468c0` | **corpus**: 399 fresh randbats fixtures; first gate **456 / 500** | 456 | 905 |
| 1 | `86d2dd8` | **Beak Blast's burn is PER HIT** — hits 2+ are computed on the halved Atk | 457 | 905 |
| 2 | `4a4e31a` | a multi-hit move **ENDS when the USER dies mid-loop** (`!pokemon.hp`) | 458 | 905 |
| 3 | `19d50c2` | **Oblivious blocks Taunt at `hitStepTryHitEvent`** — step 2, before accuracy | 459 | 905 |
| 4 | `8be00dd` | the wind-move list was hand-copied and **missing `whirlwind`** | 460 | 905 |
| 5 | `914295e` | a target's Flame Body and an attacker's Poison Touch **BOTH roll** | 461 | 905 |
| 6 | `cd059ec` | a **SLEEP-CLAUSE-blocked status move did not land** — no 970, no 1024 | 462 | 905 |
| 7 | `309e59c` | a **NULLIFIED hit still reaches step 8** — Knock Off through Ice Face / Disguise | 464 | 905 |
| 8 | `c4abf23` | **`setType` REFUSES a terastallized user** — Double Shock does nothing | 466 | 905 |
| 9 | `c3fe0ac` | a **rampage lock expires at the RESIDUAL**, not at move time | 468 | 905 |
| 10 | `30b3b83` | the **Toxic stage STOPS at 15** | 470 | 905 |
| 11 | `440ec5f` | **Yawn's sleep is residual order 23**, ahead of Harvest's coin (28) | 471 | 905 |
| 12 | `5e7e358` | a **SLEEPING rampager drops the lock** — and the expiry belongs in the TAIL | 472 | 905 |

## The finding: the two rails have fully decoupled, and the format rail is the live one

**Twelve parity commits moved the randbats rail sixteen games and the customgame rail ZERO.** The
previous tranche was the mirror image (eleven commits, eleven customgame games, zero randbats), and
the conclusion it drew — "the randbats rail is now the more interesting corpus, and it is 101 games
against 912" — is now measured rather than argued. Recording +400 randbats seeds was the right buy:
**the 399 fresh games opened at 90.2% against the pinned 101's 95.0%**, i.e. the fresh half carried
39 opens where the pinned half carried 5, and twelve of the sixteen games closed this tranche were
fresh.

**Yield was 1.33 games/commit against last tranche's exactly 1.00, and three commits paid two
games each** — the first clusters the campaign has seen in three tranches. All three clusters were
mechanism clusters, not species clusters: two nullifying abilities on one missing step, two
witnesses for one `setType` guard, two rampage games on one residual placement.

**Sleep Clause Mod earned its corpus.** `cd059ec` is a bug that CANNOT exist on the customgame
rail — the clause is only live under `gen9randombattle` — and it was the root of the scoreboard's
priority named open (below). 12 of the 500 games activate the clause, 15 activations, against 46
sleep inflictions overall.

## rb5021 is closed, and the `b0 == b3` composition it was named for was never wrong

The scoreboard's **priority named open** recorded d21 t19 as `move-order tie composed wrong`: two
Speed-96 actives, four turn-start shuffles, `b0=1 b1=1 b2=1 b3=0`, PS resolving Amoonguss where the
engine resolved Snorlax. **`PRNG_TRACE` says the engine was already TWO DRAWS AHEAD when d21
began** (`d20 engine=52 ps=52`, `d21 engine=60 ps=58`).

**The recorded shuffle GROUPS settle the composition question outright.** The sidecar stores each
`speedSort` group as it stood BEFORE the shuffle:

* d21's commit sort group reads `[p1: Amoonguss, p2: Snorlax]`
* d21's dynamic re-sort group reads `[p2: Snorlax, p1: Amoonguss]` — so the commit shuffle SWAPPED,
  i.e. **b0 = 1**
* PS then executes Amoonguss, so the dynamic shuffle swapped back — **b3 = 1**

`b0 == b3` -> side One first -> Amoonguss. **The rule reproduces PS exactly.** The instrument the
last tranche asked for already existed in the corpus; what was needed was to read the group arrays
instead of counting bit positions.

The real bug was at d20 t18, a Spore into a side that already had a sleeper. Sleep Clause Mod is an
`onSetStatus` returning FALSE, so `trySetStatus` fails, `moveHit` returns false,
`hitStepMoveHitLoop` breaks at `hit === 1`, and PS fires NEITHER the per-hit Update (970) nor the
post-hit-loop one (1024). The engine detects "the status move landed" as ">= 1 effect instruction
that is not protect bookkeeping" — and `SleepClauseBlocked` is an instruction. On a tied board that
is two phantom shuffles.

> **`SleepClauseBlocked` records that the CLAUSE SPOKE, not that the move landed.**

## Zero `rust extra` again — and two of the three were the same PS line

The fresh 399 reintroduced three over-emissions, the campaign's one hard invariant. All three are
closed:

1. **rb5280** `randomChance[90,100]@accuracy`: a 40-HP Meowscarada Triple Axels a Rocky Helmet
   Pecharunt and dies to the hit-1 chip. `hitStepMoveHitLoop`'s LAST statement is
   `this.battle.eachEvent('Update'); if (!pokemon.hp && targets.length === 1) { hit++; break; }` —
   a check on the USER, after the per-hit Update. `apply_damage_hit_rolls` carried the opposite
   assumption verbatim in a comment: *"A user faint mid-loop (recoil/contact item) is folded into
   `apply_post_damage`, so it never truncates the loop here; the corpus has no such multi-hit
   matchup."* The 399 fresh games have one.
2. **rb5477** `randomChance[1,24]@crit`: an Enamorus Taunts an Oblivious Whiscash and PS draws
   NOTHING for the turn. `oblivious.onTryHit` returns `null` for attract / captivate / taunt, from
   `hitStepTryHitEvent` — **moveStep 2**, three ahead of `hitStepAccuracy`. The engine had the rule
   only as an EFFECT gate at the volatile-application site. The `rust extra` label sat TWO decisions
   downstream of its cause.
3. **rb5343** `sample[1]@drag`: a Skarmory Whirlwinds a Wind Rider Shiftry. The handler was present
   (`flag_blocked`, built for Well-Baked Body / Soundproof / Bulletproof); `is_wind_move` was a
   hand-written list of 14 where the pinned dex has **17**, missing `whirlwind`, `sandstorm` and
   `tailwind`.

## Six rules this tranche cost a landing each to learn

1. **A per-hit `onHit` that changes a damage input must be applied PER HIT.** Beak Blast's contact
   burn is step 3 of `spreadMoveHit`, so a Triple Axel's hit 1 burns the attacker and hits 2-3 are
   computed on the halved Attack. The arithmetic is exact: 48 / 48 / 81 = 177 = PS's 284 -> 107,
   against the engine's 48 / 96 / 162. The machinery already existed for a Flame Body burn
   (rb1198's `restat_dirty`); Beak Blast is the same fact one step earlier in the same function.
2. **`hitStepTryHitEvent` is moveStep 2 and it is a THIRD accuracy-suppressing gate**, alongside
   `hitStepTryImmunity` (4) and `accuracy_forced_true`. Two of this tranche's three `rust extra`s
   were on that one line. When an ability refuses a move, ask **at which hit step**, because
   anything before 5 deletes the accuracy draw and not just the effect.
3. **`runEvent('DamagingHit')` runs the target's handlers AND the source's — both, not either.**
   `apply_contact_secondaries` had Flame Body / Static / Poison Point and Poison Touch in one
   `match`, so a Flame Body defender silently deleted the attacker's Poison Touch roll. rb5413 d2
   shows PS rolling `randomChance[3,10]` TWICE for one Shadow Sneak. The Shield Dust / Covert Cloak
   gate belongs with the SOURCE handlers, where PS puts it, and it suppresses the DRAW.
4. **A hit NULLIFIED by Ice Face / Disguise still runs steps 4 through 8.** `onDamage` returns the
   NUMBER 0, so the target stays in `targets`. This file had already learned it for step 4
   (rb1093) and for the two Updates (rb1191) and still ended both arms at step 7; Knock Off's
   `onAfterHit` was simply absent.
5. **`setType` refuses a terastallized user** ("cannot have their base type changed except via
   forme change"). Double Shock's `onTryMove` gate is `hasType('Electric')`, which goes through
   `getTypes()` and short-circuits on `terastallized` — so a Pawmot terastallized to ELECTRIC
   passes the gate, deals damage, and changes nothing. The engine's comment at the site asserted
   the gate already excluded terastallized users; it excludes every tera type except the one that
   matters.
6. **Where a residual handler lives in this file IS its residual order**, and the branching tail is
   not an order. Anything that forks gets written into the tail because the deterministic core
   cannot branch, and silently acquires order 28+. Two commits in a row, opposite directions:
   **Yawn** (order 23) had to move OUT of the tail to draw its `random(2,5)` ahead of Harvest's
   coin, and the **rampage expiry** had to move INTO it — `lockedmove` has no `onResidualOrder`, so
   PS sorts it LAST, and from the core it could not see the sleep the tail had just applied. (Shed
   Skin, hoisted from the same tail to order 5/3 two tranches ago, is the precedent both cite.)

## The rampage lock, in full

Three games and two arms of one condition, both about the RESIDUAL rather than the move:

* **`duration: 2` is what ends the volatile, and PS ticks it whether or not the mon used the move.**
  rb5059's Lilligant KOs the Chi-Yu at t26; **t27 is the replacement turn** — a `switch` request
  whose `go()` still inserts `beforeTurn` + `residual` — so PS releases with no move phase at all.
  The engine released only in the `n == 1` arm at MOVE time, and left the lock armed forever on a
  turn with no use. The `onEnd` confusion's `random(2,6)` is pre-forked in `apply_end_of_turn`
  exactly as Shed Skin's 33% roll is, since the residual body cannot branch.
* **A sleeping rampager's lock is `delete`d, not `end`ed** — `if (target.status === 'slp') { delete
  target.volatiles['lockedmove']; }`, with PS's own comment "don't lock, and bypass confusion for
  calming". rb5160's Outrage user is Yawned to sleep on the turn it starts the rampage.

## Final gate numbers (re-run at the certifying commit `5e7e358`)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **399 / 401 = 99.5%** |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **395 / 400 = 98.8%** |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **905 / 912 = 99.2%** (unmoved; SET identical at every commit) |
| Seed gate, randbats pinned 101 | (inside `gate-rb.sh`) | **98 / 101 = 97.0%** (was 96) |
| Seed gate, randbats fresh 399 | (inside `gate-rb.sh`) | **374 / 399 = 93.7%** (was 360) |
| **Seed gate, randbats 500** | `bash harness/gate-rb.sh` | **472 / 500 = 94.4%** (was 456); init-aligned 500 / 500 |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS **100.00%**, coverage 100.00% |
| State sweep, randbats 101 | `cosim harness/seed-sidecars-rb/rb50*.json.gz` | EXACTNESS **99.75%**, coverage 100.00% |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3816 / 3831 = 99.61%**; **zero `rust extra`** |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | all 26 targets green |

## The 35 remaining opens — 7 customgame, 28 randbats

The seven customgame opens are **UNCHANGED** from the endgame tranche: `rb1011 rb1012 rb1525
rb1572 rb1581 rb1681 rb1769`. Six are a bare `hp`; rb1681 is the Wish counter REPRESENTATION
question (not a mechanics bug) and was not re-examined this tranche.

**The 28 randbats opens, by first divergent FIELD:** 20 bare `hp`, 2 `item`, 2 `volatiles`, and one
each of `times_hit` / `status_counter` / `boost.spa` / `substitute_hp`.

| game | first divergence | evidence | note |
|---|---|---|---|
| rb5026 | d30 t23 `draws-match/state-diff` | `s1#1.hp` 233 vs 204 | bare `hp`, gap 29 |
| rb5037 | d12 t11 `result random[16]@gunkshot (rust =3)` | `s0#0.hp` 39 vs 1, `status` None vs Poison | offset — `PRNG_TRACE` first |
| rb5100 | d11 t9 `move-order-tie (ambiguous shuffle fork)` | `s1#3.hp` 134 vs 127 | **the LAST tie-machinery game**; rb5021's twin closed as an offset, so read the recorded shuffle GROUPS here before touching the composition |
| rb5134 | d25 t22 `draws-match/state-diff` | `s1#2.hp` 309 vs 271 | bare `hp` |
| rb5142 | d45 t35 `args randomChance[1,24]@struggle (rust [100,100])` | `s1#4.times_hit` 0 vs 1 | a MID-TURN Struggle: Encore locks a Latias onto Recover, Psychic Noise's Heal Block disables it, all four slots are gone. The engine ran only the foe's move for the unit |
| rb5146 | d37 t33 `result random[16]@foulplay (rust =13)` | `s1#0.hp` 209 vs 203 | offset |
| rb5163 | d63 t54 `draws-match/state-diff` | `s0#5.hp` 256 vs 216 | bare `hp` |
| rb5164 | d44 t35 `draws-match/state-diff` | `s1#0.item` None vs RockyHelmet | **NAMED OPEN, see below** |
| rb5199 | d6 t5 `PS-unconsumed randomChance[3,10]@cursedbody` | `s0.volatiles` + `s0.disable` (0,0) vs (618,3) | the engine skipped a Cursed Body roll PS made — d6, cheap |
| rb5207 | d48 t44 `draws-match/state-diff` | `s1#2.hp` 141 vs 125, `status` None vs Toxic | |
| rb5214 | d56 t48 `PS-unconsumed randomChance[1,24]@struggle` | `s0#5.hp` 180 vs 125 | second Struggle game, opposite sign to rb5142 |
| rb5236 | d80 t70 `draws-match/state-diff` | `s1#1.hp` 155 vs 81 | the longest game in the corpus |
| rb5246 | d19 t17 `draws-match/state-diff` | `s1.boost.spa` 2 vs 3 | one missing +1 SpA |
| rb5268 | d48 t40 `PS shuffle[2,0,2]@shedtail (rust randomChance[1,4]@par)` | `s0.substitute_hp` 66 vs 38 | Shed Tail's sub HP + an order mismatch |
| rb5289 | d40 t36 `draws-match/state-diff` | `s1#2.item` None vs SitrusBerry, `last_berry` inverted | **Harvest rolled TRUE and the engine did not restore the berry** — the roll matched, the restore did not fire |
| rb5300 | d34 t26 `draws-match/state-diff` | `s1#4.hp` 62 vs 79, `status` Burn vs None | the engine burned where PS did not |
| rb5301 | d8 t7 `PS randomChance[1,24]@bulletseed (rust shuffle[2,0,2]@disablemove)` | `s0#5.hp`, `times_hit` 4 vs 5 | |
| rb5346 | d27 t23 `draws-match/state-diff` | `s0#4.hp` 156 vs 25 | biggest bare-`hp` gap in the corpus (131) |
| rb5350 | d30 t26 `result random[16]@thunderbolt (rust =8)` | `s0#2.hp` 149 vs 150, `status` None vs Paralysis | offset |
| rb5358 | d31 t29 `args randomChance[1,4]@par (rust [100,100])` | `s0#3.hp` 116 vs 121 | |
| rb5366 | d54 t46 `draws-match/state-diff` | `s1#0.hp` 277 vs 234 | bare `hp` |
| rb5377 | d50 t44 `draws-match/state-diff` | `s1#0.status_counter` 0 vs 1 | a Toxic applied this turn; PS's stage is 1 where the engine's is 0 |
| rb5386 | d6 t7 `draws-match/state-diff` | `s0.confusion_turns` 1 vs 0 | the confusion counter is decremented at a different site than PS's `onBeforeMove`; d6, cheap |
| rb5400 | d33 t26 `result random[16]@ironhead (rust =1)` | `s0#2.hp` 133 vs 148 | offset |
| rb5424 | d15 t13 `PS shuffle[2,0,2]@generic (rust randomChance[90,100]@accuracy)` | `s1#4.hp` 75 vs 68 | a residual `@generic` sort the engine does not emit |
| rb5451 | d61 t50 `draws-match/state-diff` | `s0#1.hp` 16 vs 0 | PS KOs, the engine leaves 16 |
| rb5465 | d10 t8 `result random[16]@leafstorm (rust =9)` | `s0#3.hp` 243 vs 245, `s1.boost.def` 1 vs 0 | offset; the stray +1 Def is the tell |
| rb5490 | d47 t42 `draws-match/state-diff` | `s0#2.hp` 221 vs 257 | bare `hp` |

### rb5164: `after_hit_user_alive` is one snapshot too EARLY for a step-7 kill. NAMED OPEN.

A 9-HP Okidogi Knock Offs an Amoonguss holding a **Rocky Helmet** and dies to the 1/6 chip. Knock
Off's `onAfterHit` is `if (source.hp) { target.takeItem() }` — step 8 — and the helmet chip is
`onDamagingHit`, step **7**, genuinely ahead of it. So PS's guard is FALSE and the helmet survives;
the engine took it.

The engine's `after_hit_user_alive` is snapshotted at the top of `apply_post_damage`, which is
correct for the case it was built for (the user dying to its OWN Life Orb, which PS applies after
step 8 — rb1314) and wrong here, because the engine **defers the step-7 flush past
`apply_post_damage`**. Closing it means moving that flush ahead of `apply_post_damage`, which the
step-5-before-step-7 secondaries ordering currently forbids. It is the same "ask at which PS line"
lesson as the endgame tranche's rule 1, with a third answer.

## Asymptote assessment

**The kill criterion was not approached: twelve parity commits, twelve non-zero yields.** No commit
flipped zero games on the randbats rail. It was, however, approached on the OTHER rail — **all
twelve flipped exactly zero customgame games**, which is the real result.

**Corpus size, not cleverness, is the binding constraint, and it has been for two tranches.** The
endgame tranche argued a fourth 400-seed CUSTOMGAME recording "would buy roughly four more
singletons at 99.2% base rate" and that +400 randbats was the better buy by a wide margin. Measured:
the 399 fresh randbats games opened at **90.2%** and yielded **twelve** closed games in twelve
commits, three of them in two-game clusters. That is three times the predicted customgame return,
and every root was format-agnostic mechanics that the 912 corpus had simply never exercised.

**The remaining population is a genuine bare-`hp` asymptote for the first time.** 20 of the 28
randbats opens and 6 of the 7 customgame opens are a bare `hp` with no second field — 26 of 35, up
from 7 of 12. The structurally distinct classes the last tranche pointed at are gone: the
move-order-tie class is down from two members to one (rb5100), and rb5021 left it by being an
offset. What is left of the non-`hp` tail is eight singletons.

**Recommendation for the next tranche, in order:**

1. **Record randbats seeds 5501-6500 — a THOUSAND, not four hundred.** This is the second tranche
   in a row where the recommendation to grow the format corpus paid, and the marginal return has
   not fallen: the fresh 399 are still at 93.7% against the pinned 101's 97.0%, so the newest games
   are still where the bugs are. At a 94.4% base rate, +1000 games buys roughly 56 opens, and the
   evidence of this tranche is that they arrive in mechanism clusters rather than singletons.
2. **Take rb5199 (d6) and rb5386 (d6) first** — the two cheapest reproductions left, both with a
   named non-`hp` field, both a handful of turns into the game.
3. **rb5142 + rb5214 are a Struggle pair** and the only two-member class remaining. Both involve a
   forced Struggle under a disable/Encore/Heal-Block interaction; rb5142's `midTurn: true` decision
   is the harder half and would want the mid-turn request semantics pinned down first — the same
   question rb1681's Wish counter has been waiting on for two tranches.
4. **rb5289's Harvest is a one-line bug wearing a state diff**: the `randomChance[1,2]` MATCHES and
   the berry is not restored. Read `maybe_eat_sitrus` / the `can_restore` predicate against a mon
   that has a `last_berry` and an empty item slot.
5. **Do NOT touch rb5100's tie composition without reading the recorded shuffle groups first.**
   rb5021 spent a tranche as a named tie-machinery open and was two draws of Sleep Clause drift.
   The sidecar's `group` / `full` arrays are the instrument; `b0 == b3` has now been confirmed twice,
   once by a 92-game flip experiment and once by direct group inspection.

---

# ==== ENDGAME TRANCHE — the three named leads, and the last two `rust extra`s (2026-07-28) ====

**HEADLINE: 905 / 912 customgame (99.2%, up from 894) and 96 / 101 randbats (95.0%, unmoved).**
Eleven parity commits, newly-non-exact EMPTY on BOTH rails at every one, audited 111/111 at every
one. **Per-commit yield 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1 — exactly one game each, eleven for eleven.**
All three named leads closed, and both remaining `rust extra` draws in the corpus are gone.

| # | commit | what | 912 | rb |
|---|--------|------|-----|----|
| 1 | `172c798` | **`getConfusionDamage` is a STANDALONE formula** — burn does not halve it | 895 | 96 |
| 2 | `13449d1` | **the 970 `eachEvent('Update')` is PER HIT**, not per move | 896 | 96 |
| 3 | `6c90118` | the **Tera 60-BP floor applies under a STELLAR tera** too | 897 | 96 |
| 4 | `5f7db02` | **Shell Side Arm's category tie is a DRAW**, not just a branch | 898 | 96 |
| 5 | `1fdbc9d` | a **SUBSTITUTE-absorbed hit never phazes** | 899 | 96 |
| 6 | `3e4d9b7` | two terastallizing sides make **TWO commitChoices tie shuffles** | 900 | 96 |
| 7 | `477eefe` | the switch bracket's **THIRD Update is past `fieldEvent('SwitchIn')`** | 901 | 96 |
| 8 | `77bac7b` | **Effect Spore's roll is gated at DamagingHit**, not after Life Orb | 902 | 96 |
| 9 | `d794089` | **Soul-Heart does not fire on the KO that ends the battle** | 903 | 96 |
| 10 | `8fdf08d` | **Life Orb's recoil is not gated on damage dealt** | 904 | 96 |
| 11 | `dc10230` | a target the **Knock Off just KO'd still loses its item** | 905 | 96 |

## The three named leads, all closed — and two of the three were the OTHER kind of bug

**rb1448 was the damage bug it looked like.** `getConfusionDamage` (`battle-actions.ts:1854-1866`)
is four truncated divisions, a 16-bit truncation and `randomizer`, and then it is done. It never
calls `getDamage`/`modifyDamage`, so **every modifier that lives there is absent — including the
burn halving** (`:1845`). `conditions.ts brn` carries only
`onModifyAtk() {} // hardcoded in BattleActions#modifyDamage()`, which is precisely why it cannot
reach a path that does not call it. A burned, +1-Atk Roaring Moon: bd 62, roll 14 → PS 53, engine
`modify(53, 0.5)` = 26.

**rb1661 was an OFFSET.** `this.battle.eachEvent('Update')` is the LAST STATEMENT OF
`hitStepMoveHitLoop`'S LOOP BODY (`battle-actions.ts:965`) — a five-hit Scale Shot fires FIVE of
them, and the engine emitted one. Every hit past the first read its crit roll out of the shuffle
the engine had not emitted.

**rb1347 was an OFFSET wearing the Terapagos pair's clothes.** The scoreboard grouped it with
rb1795 because both were 3-HP gaps on a Terapagos, and rb1795 really was a damage bug (the Tera
60-BP floor, which PS applies to a Stellar tera under a DIFFERENT predicate rather than skipping).
rb1347's Psychic-into-Tera-Shell had nothing wrong with it at all: `PRNG_TRACE=rb1347` reads
`d78 engine=235 ps=235`, `d80 engine=243 ps=244`, and the missing draw is **Shell Side Arm's
`randomChance(1, 2)` category tie**, two decisions earlier. The engine forked both categories at
half each and never emitted the coin.

> **The campaign lesson held for two of the three named leads, and the label lied about which.**
> Run `PRNG_TRACE` on a `result random[…]` or a bare-`hp` game BEFORE reading its state diff. It is
> one command and it separates "the engine computed the wrong number" from "the engine is standing
> in the wrong place", which are different investigations with different costs.

## Both remaining `rust extra` draws are closed

Zero over-emission is the campaign's hard invariant, and the seed rail had exactly two violations
left. They had opposite roots and the same shape — the engine standing one draw ahead of PS.

1. **rb1760: a Substitute-absorbed hit never phazes.** `spreadMoveHit` step 0 sets
   `targets[i] = null` on `HIT_SUBSTITUTE` (`battle-actions.ts:1083-1085`); step 6's `forceSwitch`
   (`:1125`, `:1377`) iterates `targets` and skips the null. It is the SAME nulling that already
   suppressed `onHit`, the self-drops and the secondaries in the engine — the phaze was the one
   consumer that had not been wired to it, and it emitted a `sample[5]@drag` for the privilege.
2. **rb1751: the switch bracket's third Update is on the far side of `fieldEvent('SwitchIn')`.**
   `runSwitch` (`battle-actions.ts:180-193`) does the `getAllActive(true)` speedSort FIRST, then
   `fieldEvent('SwitchIn')`, and only then returns to `runAction`'s trailing Update (`:2882`). A
   switch-in ability that `formeChange`s the entrant has REFRESHED the Speed cache the first two
   sorted on — `setSpecies` ends in `this.speed = this.storedStats.spe`, the RAW stat. A
   Minior-Green enters at 235 against a Speed-tied Scream Tail, the first two shuffles fire, Shields
   Down makes it Minior-Meteor at a raw 140, and PS's third Update does not tie.

## Seven rules this tranche cost a landing each to learn

1. **`step 8 precedes `faintMessages`` is a rule with more than one consumer.** `after_hit_user_alive`
   was introduced for Ceaseless Edge / Stone Axe / Glaive Rush; this tranche found it wanted at
   **Effect Spore** (a 3-HP Life Orb user dies to its own orb, and the engine read the corpse and
   skipped the `random(100)` PS had already rolled) and, mirrored onto the DEFENDER, at **Knock
   Off** (`takeItem` never reads `pokemon.hp`, and `isActive` stays true until a replacement enters,
   so a target the Knock Off just KO'd still loses its item). When a guard asks "is it alive", ask
   *at which PS line*.
2. **`Battle#boost` refuses everything once the foe side is empty.**
   `if (this.gen > 5 && !target.side.foePokemonLeft()) return false;` (`battle.ts:2028`), and
   `faintMessages` decrements `pokemonLeft` BEFORE `runEvent('Faint')` — so no KO-boost ability
   fires on the KO that ends the battle. Moxie / the Neighs / As One / Beast Boost all carried the
   guard; **Soul-Heart was the copy that drifted.** Fifth pair this campaign.
3. **`speedSort` shuffles EVERY tie group, not the interesting one.** Two terastallizing sides give
   `commitChoices` a `[tera, tera, move, move]` queue and TWO draws — `shuffle[4,0,2]` then
   `shuffle[4,2,4]`. The comment beside the code called the tera tie "vanishingly rare" and left it
   unmodelled; rb1464 is the witness, and the seed gate's forced-tie peek carried the same
   assumption ("still consumes exactly one `random` draw") and had to be stepped forward too.
4. **A gate can be right for the wrong reason and a fork can be right with no draw.** Shell Side Arm
   enumerated its two categories at ½ each — correct probabilities, correct state, and no coin. The
   probability path never notices; the seed rail slides one draw for the rest of the game.
5. **Life Orb is not part of the damage bookkeeping.** `onAfterMoveSecondarySelf` tests only
   `move.category !== 'Status'` and a truthy `moveResult` — a hit NULLIFIED to 0 by Ice Face or
   Disguise is one. It had been sitting inside `apply_post_damage`'s `if any_damage` block next to
   drain and recoil, both of which PS really does gate, and the two nullifying arms return before
   `apply_post_damage` at all.
6. **The Stellar tera arm of a rule is a DIFFERENT PREDICATE, not an exclusion.** The 60-BP floor
   reads `source.terastallized === 'Stellar' ? !source.stellarBoostedTypes.includes(move.type) :
   source.hasType(move.type)` (`:1664`). The engine skipped Stellar outright. `stellarBoostedTypes`
   stays unmodelled for the reason `damage::stab_mod` already documents — `:1785` never pushes for
   Terapagos-Stellar, the only Stellar user randbats produces.
7. **A species change is not always a `formeChange`.** Folding Imposter into the switch bracket's
   post-`SwitchIn` Speed refresh regressed EIGHT games at once (rb1060 rb1241 rb1303 rb1359 rb1591
   rb1598 rb1669 rb1749 + rb5081). `transformInto` runs `setSpecies(species, effect, true)` first —
   caching the Speed computed for the copied species from the TRANSFORMER's own level/IVs/EVs/nature
   — and only then overwrites `storedStats` with the target's, leaving `speed` at that intermediate.
   The engine's post-state carries the copied stat, not the intermediate.

## The twelve remaining opens, evidenced

**7 on the customgame rail, 5 on randbats.** Every one has `align=true` and a per-game first
divergence below. `SEED_GATE=1 VERBOSE=1 DBG_GAME=<g> DBG_DIFF=1 cosim harness/seed-sidecars[-rb]/<g>.json.gz`
reproduces each; the slim fixtures cannot supply the DIFF FIELD.

| game | rail | first divergence | evidence | what would close it |
|---|---|---|---|---|
| rb1011 | fresh | d43 t33 `draws-match/state-diff` | `s0#3.hp` 140 vs 77 | bare `hp`, gap 63 — a whole extra hit or a doubled modifier; `DBG_INSTR` the unit |
| rb1012 | fresh | d60 t52 `draws-match/state-diff` | `s0#2.hp` 138 vs 185 | bare `hp`, engine deals 47 MORE than PS |
| rb1525 | fresh | d23 t19 `draws-match/state-diff` | `s1#5.hp` 211 vs 231 | bare `hp`, gap 20 |
| rb1572 | fresh | d29 t22 `draws-match/state-diff` | `s0#1.hp` 191 vs 190 | bare `hp`, gap **1** — a rounding/chain-order item, the most expensive shape there is |
| rb1581 | fresh | d36 t30 `draws-match/state-diff` | `s0#2.hp` 205 vs 131 | bare `hp`, gap 74 |
| rb1681 | fresh | d45 t34 `draws-match/state-diff` | `s1.wish` (1,214) vs (2,214) | **REPRESENTATION, not behaviour** — see below |
| rb1769 | fresh | d2 t3 `draws-match/state-diff` | `s1#0.hp` 242 vs 228 | bare `hp`, gap 14, and it is **decision 2** — the cheapest reproduction in the corpus |
| rb5021 | randbats | d21 t19 `PS randomChance[100,100]@gigadrain (rust shuffle[2,0,2]@update)` | `s1#4.hp` 363 vs 364 | **move-order tie composed wrong** — see below |
| rb5026 | randbats | d30 t23 `draws-match/state-diff` | `s1#1.hp` 233 vs 204 | bare `hp`, gap 29 |
| rb5037 | randbats | d12 t11 `result random[16]@gunkshot (rust =3)` | `s0#0.hp` | run `PRNG_TRACE=rb5037` FIRST |
| rb5059 | randbats | d33 t28 `draws-match/state-diff` | `s1.volatiles` bit 20 + `pending_move` `Rampaging(601, 1)` vs `None` | **rampage lock outlives PS's** — a Petal Dance at `SetMoveStreak 3→4` that PS has already released |
| rb5100 | randbats | d11 t9 `move-order-tie (ambiguous shuffle fork)` | `s1#3.hp` | the tie machinery's own class |

### rb1681 is a representation mismatch, not a mechanics one

PS's Wish slot condition **has no `duration` field at all** — `addSlotCondition` copies
`status.duration`, and `wish`'s condition declares none (`data/moves.ts:20937-20958`). It runs off
`this.effectState.startingTurn` and an `onResidual` that returns while
`getOverflowedTurnCount() <= startingTurn`. `convert.rs:709` therefore derives the engine's counter
as `if turn <= startingTurn + 1 { 2 } else { 1 }`.

Measured over the 401 pinned sidecars: **118 of the 124 recorded wish snapshots have
`turn - startingTurn == 2` (→ 1) and all 6 with `== 1` (→ 2) are `midTurn: true` decisions.** The
engine ticks 2→1 at the cast turn's residual, which is right for every end-of-turn snapshot and
wrong for a MID-TURN snapshot taken after that residual. The heal TIMING is correct in both. A fix
is a converter/engine counter re-basing, not a mechanics change, and it wants the mid-turn boundary
semantics pinned down first.

### rb5021: the move-order tie composition is right, and this game still disagrees

`emit_turn_start_bracket` emits four shuffles and the gate composes side One first iff
`b0 == b3` (commit `queue.sort()` bit vs the gen-8 dynamic re-sort bit). **That rule was tested and
confirmed this tranche**: flipping it to `b0 != b3` costs **92 games** on the 912 rail (905 → 810)
and 6 on randbats. It is heavily exercised and correct.

rb5021 d21 t19 nevertheless comes out backwards. Both actives are at Speed 96; PS's four turn-start
shuffles are `[2,0,2] [2,0,2] [2,0,2] [3,0,2]` in exactly the modelled positions; the bits are
`b0=1 b1=1 b2=1 b3=0`, so the engine resolves Snorlax first while PS resolves Amoonguss first. The
symptom is the engine emitting Snorlax's `runAction` Update BEFORE Giga Drain's accuracy roll
instead of after it — one shuffle moved, and Giga Drain then reads roll 9 where PS read 11.
Note `b0 == b2` would give PS's answer here; that is numerology, not a hypothesis, and it should not
be adopted without a second witness. **NAMED OPEN.** rb5100's `move-order-tie (ambiguous shuffle
fork)` is the same machinery and is the obvious second witness to take with it.

## Final gate numbers (re-run at the certifying commit `dc10230`)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **399 / 401 = 99.5%** (was 397) |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **395 / 400 = 98.8%** (was 386) |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **905 / 912 = 99.2%** (was 894) |
| **Seed gate, randbats 101** | `bash harness/gate-rb.sh` | **96 / 101 = 95.0%** (unmoved); init-aligned 101 / 101 |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS **100.00%**, coverage 100.00% |
| State sweep, randbats | `cosim harness/seed-sidecars-rb/*.json.gz` | 3702 / 3712 = **99.73%**, coverage 100.00% |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; **zero `rust extra`** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** — 3829/3829 move units, 4832/4832 states |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | all suites green |
| Pivot property fuzz | `PIVOT_FUZZ_GAMES=200000 cargo test -p engine --test pivot_landing_bench` | **0 violations / 200 000** |

## Asymptote assessment

**The kill criterion was never approached: eleven commits, eleven games, no commit flipped zero on
the customgame rail.** But the shape of the work changed completely, and that is the finding.

**Yield is now exactly 1.00 games/commit and the variance is gone.** Every previous tranche had a
cluster somewhere — Illusion paid 2 games on one commit, the offset tranche paid 5. This one paid
one, eleven times, because **there are no clusters left**: eleven roots, eleven species/item/ability
interactions, no two sharing a mechanism. The bucket table the last three tranches used as a
triage instrument is now meaningless — twelve opens, twelve distinct roots, one entry each.

**The randbats rail did not move at all.** Eleven customgame games and zero randbats games is the
first time the two rails have fully decoupled. All eleven roots are format-independent mechanics
(confusion damage, per-hit Updates, STAB floors, Life Orb, Knock Off), so this is not "the fixes
did not generalize" — it is that the 5 remaining randbats opens are a different population: two of
the five are the move-order-tie machinery (rb5021, rb5100), one is a rampage-lock lifetime
(rb5059), and only two are the bare-`hp` tail. **The randbats rail is now the more interesting
corpus**, and it is 101 games against 912.

**What the remaining 12 actually are.** Six bare `hp` (rb1011 rb1012 rb1525 rb1572 rb1581 rb1769,
plus rb5026), one representation mismatch that is not a bug (rb1681), two move-order-tie
(rb5021 rb5100), one rampage-lock lifetime (rb5059), one `result random[16]` that has not yet been
PRNG_TRACEd (rb5037). **Five of the twelve are NOT the bare-`hp` asymptote**, which is a better
ratio than any tranche since XII.

**Recommendation for the next tranche, in order:**

1. **Take the move-order-tie pair (rb5021 + rb5100) first.** Two witnesses on one machinery, the
   composition rule is proven correct in general (the 92-game flip experiment), and a third
   instrument is needed — instrument `speedSort` against the recorded shuffle ARGS per tie group
   rather than reasoning about bit positions. This is the only remaining class with two members.
2. **rb1769 d2 and rb5059 d33 are the cheapest reproductions in the corpus** — decision 2 and a
   zero-draw unit respectively. A bare-`hp` bug at d2 needs no replay at all.
3. **rb1681 wants a decision about the wish counter's REPRESENTATION**, not a mechanics fix. Pin
   down what `turn` means on a `midTurn: true` decision record first; the same question probably
   governs `futuremove` (`convert.rs:713`), which uses the identical `endingTurn` arithmetic.
4. **Record seeds 1801-2200 — this recommendation is now FOUR tranches old, and this tranche is the
   first evidence that it may no longer pay.** The fresh half has closed to within 0.7 points of the
   pinned half (98.8% vs 99.5%, from 5.5 points three tranches ago). Shared classes are no longer
   regrowing at this exactness level: nine of the eleven flips were fresh games, but every root was
   found by reading PS source against ONE witness, not by clustering. **A fourth 400-seed recording
   would buy roughly four more singletons at 99.2% base rate, each costing a full investigation.**
   Recording +400 RANDBATS seeds (5101-5500) is now the better buy by a wide margin: that rail is
   101 games at 95.0%, its opens are structurally different, and it is the format the engine is
   actually for.

---

# ==== OFFSET & PIVOT TRANCHE — the Toxic offset, the PivotLanding root, and five buckets (2026-07-27) ====

**HEADLINE: 894 / 912 customgame (98.0%, up from 878) and 96 / 101 randbats (95.0%, up from 94).**
Eight commits, newly-non-exact EMPTY on BOTH rails at every one, audited 111/111 at every one.

| # | commit | what | 912 | rb |
|---|--------|------|-----|----|
| 1 | `55d46b1` | **gen-8 Toxic by a Poison-type never rolls accuracy** — and an already-statused target is not a reason to skip the roll | 881 | 95 |
| 2 | `6faa812` | **`Pivot::Pause` belongs to the move that RUNS** — the `PivotLanding with no live bench` root | 881 | 95 |
| 3 | `3a1bb4c` | **Mold Breaker suppresses `flags: { breakable: 1 }` and nothing else** | 886 | 95 |
| 4 | `6ab73bd` | **Harvest rolls in SPEED order**; **`thawsTarget` cures the freeze AFTER the secondaries** | 888 | 96 |
| 5 | `12772d3` | **Transform FIRES the copied ability**; the switch-out abilities read the LIVE one | 890 | 96 |
| 6 | `912f71b` | the **nullifying-ability arms dropped the MISS branch and both `eachEvent('Update')`s** | 892 | 96 |
| 7 | `888e80c` | **`ignoreDefensive` is a second flag** — only the `ignoreEvasion` half was wired | 893 | 96 |
| 8 | `32d2ba7` | **Dry Skin's third handler**: `onFoeBasePower` x1.25 against Fire | 894 | 96 |

## The offset lead, resolved: it was Toxic, and the d6 note was wrong

The scoreboard's named lead — "rb1670 is an OFFSET bug wearing a damage bug's clothes" — was
exactly right about the shape. `PRNG_TRACE=rb1670` put the first divergence at d42, one unit BEFORE
the `result random[16]@secretsword` label, with `engine=141 ps=140`: **one phantom draw**. The unit
is a Clodsire's Toxic behind a Substitute, and Clodsire is Poison/Ground.

> **`hitStepAccuracy` hard-codes `move.alwaysHit || (move.id === 'toxic' && gen >= 8 &&
> pokemon.hasType('Poison'))` into the `accuracy = true` arm (`battle-actions.ts:726`).** An
> `accuracy === true` makes NO `randomChance` draw at all — a different thing from a numeric 100,
> which still rolls `randomChance(100, 100)`.

The rule now lives in `accuracy_forced_true` (stream) and `accuracy_of` (probability), next to No
Guard and the weather-perfect moves, instead of an ad-hoc `md.id == "toxic"` guard that sat at ONE
of the two status-move accuracy sites. The second site — the substitute-blocked early return —
carried a `target_already_statused` gate justified by "d6 t58-62: Toxic on an already-paralyzed,
subbed Garchomp draws nothing".

> **d6's Toxic user is Toxtricity, Electric/POISON.** The observation was real and the explanation
> was this rule wearing the wrong name. `hitStepTryImmunity` (`:661-684`) has no status check at
> all and `setStatus` fails inside `moveHit`, long after step 4 — an already-statused target does
> NOT suppress the accuracy roll. rb5039 d46 (Toxic into an already-badly-poisoned, subbed Keldeo,
> engine one draw BEHIND) and rb1642 d35 (Will-O-Wisp, same shape) are the counter-witnesses.

All three of the Keldeo / Secret Sword cluster closed on that one commit, plus rb1649.

## The PivotLanding root

**`Flow::run_turn` decides the pause from the CHOSEN move; `run_move_action` then substitutes the
move.** Two substitutions it cannot see: **Struggle** (`no_usable_move`) and the **Encore
`OverrideAction` redirect**. Every `match pivot` arm keys on the ACTION, so the substitute inherited
the pause.

The lethal instance, reproduced deterministically in
`tests/pivot_pause_survives_move_substitution.rs`: a PP-stalled mon whose bench is entirely FAINTED
picks Revival Blessing, which grants `Pivot::Pause` off `has_fainted_bench`; `no_usable_move`
replaces it with Struggle; Struggle's damaging path pushes `PivotPending`; and Struggle's own
recoil — applied AFTER the pivot match — then kills the user, so `resume_pivot` gets a request for
a side with nothing alive at all.

Two fixes: `run_move_action` re-derives the pause from the EXECUTED move (`Pivot::Target` is left
alone — the verification paths supply it from the recorded choice, which IS the executed move), and
every `PivotPending` site additionally requires `has_alive_bench`, which is PS's own gate —
`sim/battle.ts:2904`, `if (switches[i] && !this.canSwitch(this.sides[i]))` clears `switchFlag` and
drops the side out of `switches`.

**`tests/pivot_landing_bench.rs` is the property**, as a random-play fuzz over small, hazard-laden,
PP-starved boards built from pivots / draggers / Revival Blessing: **12 violations per 20 000 games
against the pre-fix engine, 0 in 200 000 after.** `PIVOT_FUZZ_GAMES` sets the budget (default
20 000, ~7 s). The `resume_pivot` / `resume_revive` eprintlns stay as tripwires.

## Six rules this tranche cost a landing each to learn

1. **A `breakable: 1` flag is the WHOLE of Mold Breaker.** `sim/battle.ts:836`. The engine blanked
   the defender's ability wholesale in `compute_damage`, which deleted the abilities PS deliberately
   left out of the flag — **Shadow Shield** (`flags: {}`, unlike Multiscale's `breakable: 1`) and the
   four **Ruin** abilities — and suppressed nothing at all on the boost path, where **Contrary IS
   `breakable: 1`**. New `ability_breakable()` is the pinned dex's 83 names. `Full Metal Body` is
   `cantsuppress`, which is why the blocker set has to be filtered per-ability, not as a group.
   `suppressingAbility` needs an ACTIVE MOVE, so Intimidate / Sticky Web / Octolock / a contact
   ability's own drop are outside it.
2. **Two identically-shaped consecutive draws are invisible to the differ and load-bearing for the
   SELECTOR.** Two Harvest holders both roll `randomChance[1,2]`; the seed gate hands the first
   recorded result to whichever the engine rolls first, and PS `speedSort`s the Residual list.
   rb5073 d51 read `draws-match/state-diff` and was a pure ORDERING bug.
3. **`frz.onAfterMoveSecondary`, not `onHit`.** A `thawsTarget` move cures the freeze at
   `hitStepMoveHitLoop`'s trailing `afterMoveSecondaryEvent` (`battle-actions.ts:1026`), so a frozen
   target hit by Scald is STILL FROZEN when the 30% burn is tried and ends the turn with NO status.
   The Fire-type arm is a different handler (`frz.onDamagingHit`) and stays inside the hit.
4. **`transformInto` ends with `setAbility(target.ability, this, true)`, and `setAbility` runs
   `singleEvent('Start', ability, ...)` for every gen > 3 — the copied ability ACTIVATES.** An
   Imposter Ditto copying Intimidate Intimidates. And the switch-OUT abilities (Natural Cure,
   Regenerator) read the LIVE ability, from `runEvent('BeforeSwitchOut')` before `clearVolatile()`,
   so a Ditto-as-Klawf still has Regenerator when it leaves.
5. **An early return out of the damaging path has to bring `miss_out` with it.** Both nullifying-
   ability arms (Ice Face, Disguise) returned `out` alone, so the engine generated NO miss outcome —
   and `replicate_select`, finding nothing that matches, falls through to "keep all" and takes the
   only branch there is. They were also missing BOTH `eachEvent('Update')`s (970 and 1024): a
   nullified hit is still a connecting hit.
6. **`ignoreEvasion` and `ignoreDefensive` are two fields.** The same four moves (Chip Away,
   Darkest Lariat, Nihil Light, Sacred Sword) carry both and only the accuracy half was wired.
   The flag form is unconditional — `defBoosts = 0` for a negative stage too, which is what
   separates it from the crit rule the engine already had.

> **The "two engine copies drift" rule struck twice more, and once it was THREE.** The Toxic guard
> existed at one of two status-move accuracy sites. The `miss_out` extend existed at one of three
> early returns. When you find a PS predicate in the engine, grep for the second implementation —
> and then for the third.

## What is left, and the named leads

**18 open on the customgame rail, 5 on randbats.** The bucket table is now nearly flat — the named
clusters are spent. Re-run the bare-`hp` classifier before anything (`SEED_GATE=1 VERBOSE=1` over
`harness/seed-sidecars/` gives the DIFF FIELD, which the slim fixtures cannot).

| bucket | n | games |
|---|---|---|
| Terapagos (Tera Shell / Stellar STAB), both ≤3 HP | 2 | rb1347 rb1795 |
| Regenerator / pivot, 1 HP | 1 | rb1572 |
| singletons | 15 | rb1011 rb1012 rb1314 rb1416 rb1448 rb1464 rb1525 rb1573 rb1581 rb1629 rb1661 rb1681 rb1751 rb1760 rb1769 |
| randbats | 5 | rb5021 rb5026 rb5037 rb5059 rb5100 |

Three leads, all localized:

1. **rb1448 d8 — a phantom 26-damage hit with no draws.** PS's whole unit is
   `randomChance[33,100]@confusion` + `random[16]@confusion-damage` (Fake Out failed: the user was
   not on its first turn). The engine emits the SAME two draws and then two `Damage` instructions,
   26 and 16, where PS has one confusion hit of 69. A damage-instruction bug with a clean draw
   stream — start from `DBG_INSTR` on that unit.
2. **rb1661 d55 — Scale Shot with Loaded Dice.** PS's per-hit stream is
   `crit, damage, shuffle[2,0,2]@scaleshot` five times over; the engine's shuffle goes missing after
   one of them (`PS shuffle[2,0,2]@scaleshot (rust randomChance[1,24]@crit)`). Same class as the
   Ice Face / Disguise Updates just fixed, in the realized-multi-hit loop.
3. **The Terapagos pair are both exactly 3 HP** — rb1347 (Psychic into a Tera Shell holder, engine
   118 vs PS 121) and rb1795 (a Stellar-terastallized Terapagos's Rapid Spin, engine 132 vs PS 129).
   One is Tera Shell's halve position, the other the Stellar STAB rule; 3 HP twice on two different
   sides of the same species is unlikely to be coincidence.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **397 / 401 = 99.0%** (was 395) |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **386 / 400 = 96.5%** (was 372) |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **894 / 912 = 98.0%** (was 878) |
| **Seed gate, randbats 101** | `bash harness/gate-rb.sh` | **96 / 101 = 95.0%** (was 94); init-aligned 101 / 101 |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS **100.00%**, coverage 100.00% |
| State sweep, randbats | `cosim harness/seed-sidecars-rb/*.json.gz` | 3702 / 3712 = **99.73%**, coverage 100.00% |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; **zero `rust extra`** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** — 3829/3829 move units, 4832/4832 states |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | all suites green (5 new files, 27 new tests) |
| Pivot property fuzz | `PIVOT_FUZZ_GAMES=200000 cargo test -p engine --test pivot_landing_bench` | **0 violations / 200 000** (12 / 20 000 pre-fix) |

## Asymptote assessment

**Not an exhausted seam, but the cluster supply is.** Eight commits, per-commit yield across the two
rails 4, 0, 5, 3, 2, 2, 1, 1 — **2.25 games/commit**, the highest of any tranche so far (Illusion
1.8, ruleset 0.5, burn-down XIII 1.7). But the shape changed inside the tranche: the first four
commits paid 12 games between them and the last four paid 6, and the table above now has one bucket
of 2 and twenty singletons. The kill criterion (three consecutive commits flipping zero on both
rails) was never approached — commit 2 flipped zero, but it is the trainer-crash root, not a parity
commit.

**Recommendation for the next tranche, in order:**

1. Take the three named leads above — all three are draw-stream or instruction-stream bugs with the
   unit already identified, which is a different and cheaper kind of work than the bare-`hp` tail.
2. Record seeds 1801-2200. This recommendation is now three tranches old and the fresh half is still
   1.6 points behind the pinned half (96.5% vs 98.8%) — much closer than the 5.5 it was, which is
   itself evidence that the remaining tail is seed-independent mechanics rather than corpus gaps.
3. The singleton tail wants a different instrument than the classifier: every one of the 15 is a
   distinct species/item/ability interaction, and the classifier's value was clustering, which it
   can no longer do.


# ==== ILLUSION TRANCHE — the disguise, three named roots, and the bare-hp classifier (2026-07-27) ====

**HEADLINE: 878 / 912 on the customgame rail (96.3%, up from 870) and 94 / 101 on the real-format
rail (93.1%, up from 93). Illusion is modelled end-to-end and verified against four Zoroark
witnesses that were already in the randbats corpus.** Six parity commits, newly-non-exact EMPTY on
BOTH rails at every one, per-commit yield 0/1/1/2/2+1rb/2 — no commit flipped zero on both rails,
so the kill criterion was never approached.

| # | commit | what | 912 | rb |
|---|--------|------|-----|----|
| 1 | `68a3672` | **Illusion end-to-end** — `Side::roster`, the disguise, the break, protocol masking, `observe`, `maybeTrapped`, Transform | 870 | 93 |
| 2 | `8099dc3` | **Disguise busts on hit 1 of a MULTI-HIT move** (and Ice Face in the third hit loop) | 871 | 93 |
| 3 | `e6e3b1e` | **`OverrideAction` is the FIRST thing `runMove` does** — the Encore redirect ran 130 lines too late and discarded every move modifier | 872 | 93 |
| 4 | `be2ecd4` | **`onAfterHit` is step 8** — after step 7, before recoil/Life Orb. Both hazard-layer roots | 874 | 93 |
| 5 | `ce24469` | **The Stellar type, Tera Shell's REPLACEMENT rule, Terapagos-Stellar's faint regression** | 876 | **94** |
| 6 | `8556ee2` | **Ice Face: a nullified hit still PIVOTS**, and a draw-and-discard must record the REAL value | 878 | 94 |

---

## Illusion, as landed

The ability was entirely unmodelled — it appeared only in Trace's exclusion list, and
`Ruleset::illusion_level_mod` was a flag with no implementation surface.

**The state.** Two new fields, one on each level:

* `Pokemon::illusion: Option<u8>` — the CANONICAL PARTY SLOT of the disguise target, i.e. PS's
  `pokemon.illusion` pointer. Per-MON, and it survives a switch-out.
* `Side::roster: [u8; 6]` — **PS's LIVE `side.pokemon` array**, as canonical slots. The engine's
  party slots are fixed; PS's are not. `switchIn` SWAPS the outgoing and incoming entries
  (`sim/battle-actions.ts:128-131`), so `roster[0]` is the active from the first switch onward and
  the rest is a permuted list. `Instruction::Switch` performs the swap; it is an INVOLUTION, so
  apply and reverse are literally the same operation. The engine already needed this order for Beat
  Up and had been INJECTING it from the recording (`set_beatup_order`); it is now modelled.

**The disguise** is chosen at `onBeforeSwitchIn` as the last ABLE entry of the live array behind the
entrant. Three details each cost a reading of the source:

1. The choice runs AFTER `switchIn`'s array swap and BEFORE `add('switch')`, so `SetIllusion` is
   pushed AHEAD of the `Switch` instruction and the `|switch|` line is already masked when the
   protocol layer renders it.
2. A fainted entry is SKIPPED, not a stopping point — PS's `break` sits INSIDE the `!fainted` arm.
   (A terastallized entrant behind an Ogerpon/Terapagos ends the scan with NO disguise: the
   assignment is skipped but the `break` still fires.)
3. **A switch-OUT never breaks it.** `switchIn` sets `beingCalledBack = true` before firing the
   ability's `End`, and `onEnd` bails on that flag. The disguise rides along on the bench — which is
   why the recorded corpus shows `illusion: "[Pokemon:p1a]"` on a BENCHED Zoroark (rb5017 d42): the
   serialized reference is to the OBJECT, and it reads out whatever array index that object has
   drifted to since.

**The break** is `onDamagingHit` → `singleEvent('End')` → `onEnd`: `|replace|` with the real
details, `|-end|…|Illusion|`, and under Illusion Level Mod the level hint. Both idents are the REAL
mon's, because PS nulls the pointer before either `add`. Gated on `damagedDamage.length` like the
rest of step 7 — a Substitute hit does not break it; a KO does, because `pokemon.fainted` is not set
until `faintMessages`.

**Protocol masking is two functions.** `Pokemon#toString` (`sim/pokemon.ts:532-535`) takes the SLOT
from the real mon and the NAME from the disguise, so masking `protocol::ident` masks the entire line
stream in one place. `getFullDetails` (`:545-556`) shows the disguise's SPECIES always and its LEVEL
only under **Illusion Level Mod** — without the rule a disguised mon wears its own level under a
foreign name, which is the classic Zoroark tell. `[Gen 9] Random Battle` has the rule; custom game
does not. That is the flag's first real behaviour.

**Two things Illusion changes that are NOT protocol:**

* `transformInto` bails when EITHER mon is disguised (`sim/pokemon.ts:1274`).
* **The `maybeTrapped` inference sweeps the APPARENT species** — `const species = (source.illusion
  || source).species` (`sim/battle.ts:1732`). A Zoroark disguised as a Dugtrio makes the foe's
  request carry `maybeTrapped: true`. This is the one place the disguise reaches a gated field.

**`State::observe`** substitutes the disguise's whole identity block (species / forme / level /
gender / typing / stats) into the foe's view of the active slot and scrubs the pointer; HP, status,
boosts and volatiles stay truthful, because those are exactly what the log reports about the SLOT
regardless of who is standing in it. Deliberately NOT hidden: the disguise target keeps its own
party entry, so the observed roster shows that species twice — which matches this model's standing
assumption that the foe's ROSTER is public while the identity on the field is not.

### The witnesses: four, already committed, and a dead guard

`harness/seed-sidecars-rb/` already contained **four Zoroark games — rb5005, rb5016, rb5017,
rb5047** — so no recording run was needed to clear the ≥3 bar. Between them they exercise every
transition: the disguise being set at switch-in (rb5005 d11, rb5017 d18/d31, rb5047 d8), the visible
break (rb5005 d12, rb5017 d27/d52), re-entry re-choosing a DIFFERENT target after the array has been
permuted (rb5047 d12 → d29, `[Pokemon:p1f]` → `[Pokemon:p1a]` → `[Pokemon:p1d]`), and the disguise
persisting on a benched Zoroark.

All four were byte-exact BEFORE the tranche, and the reason is a converter bug worth recording:

> **`convert.rs`'s `if b(p, "illusion") { return Err(unsup(...)) }` guard was DEAD.** `illusion`
> serializes as a `[Pokemon:pNx]` STRING and `serde_json`'s `as_bool` on a string is `None`, so the
> guard never fired and four Zoroark games were silently converted without their disguises. A
> "we don't support this yet" gate that is keyed on the wrong JSON type is worse than no gate.

### The manifest decision (the regeneration question)

`roster` and `illusion` join **`diff_states` but NOT the digest walk**, and `digest.rs` now states
the asymmetry at the top. The reasoning:

* Every committed slim fixture stores a digest computed at BUILD time, so widening the digest walk
  invalidates all 902 of them and forces a mechanical ~900-file regeneration commit.
* Neither field can change any other field's value. The disguise is protocol-only apart from the
  `maybeTrapped` inference, which the request comparison covers directly; the roster order feeds
  only Beat Up and Illusion.
* Both fields ARE gated, on the full-state rails: the state sweep runs `diff_states` over complete
  PS snapshots for `harness/cosim-traces/` (3831 units) and `harness/seed-sidecars-rb/` (3712 units,
  where the four witnesses live). Adding them left both sweeps exactly where they were — audited
  100.00%, randbats 99.62% — which is the verification.

The exporter had to move with them: `export::emit_order` now emits `Side::roster` verbatim instead
of "active-first then ascending", so `convert(export(S)) == S` still holds with `roster` compared.
Round-trip stayed PASS at 3829/3829 move units and 4832/4832 states.

**Residual, stated plainly:** the `|replace|` / `|-end|` / `|-hint|` lines are covered by unit tests
and PS source, not by log-parity against a real Zoroark game — `harness/protocol-parity.mjs` only
handles games whose teams are in its embedded `TEAMS` table. Extending it to drive a randbats
sidecar's `packedTeams` is the way to close that.

---

## The three named roots, and what two of them actually were

**rb1621 — Disguise on a multi-hit move.** The scoreboard's diagnosis was right. PS's Disguise is an
`onDamage` block guarded by the target's CURRENT `species.id`, and `onUpdate`'s forme change into
Mimikyu-Busted lands at the PER-HIT `eachEvent('Update')` (`battle-actions.ts:970`) — so hit 1 is
nullified and hits 2..n damage normally. The engine gated the whole mechanic on `md.hits_max == 1`.
The engine has **THREE hit loops**; two already handled Ice Face per-hit and the third —
`apply_multihit_realized_ma`, which is what Triple Axel goes through — handled NEITHER. Also a good
illustration of why the bare-`hp` tail is hard: PS and the engine killed the Mimikyu with the same
three recorded rolls, so the HP landed on 0 in both and the ONLY surviving symptom was
`s1#3.species` 495 vs 496 on a corpse.

**rb1734 — NOT "Encore does not redirect".** The engine's redirect worked. `runEvent
('OverrideAction')` is at `battle-actions.ts:228`, the very TOP of `runMove`, before `getActiveMove`
builds the object that every `onModifyType` / `onModifyMove` edits. The engine's copy sat ~130 lines
down, after the whole modifier chain, and re-assigned `md = move_data(enc.0)` — throwing the chain
away. The Arceus-Electric encored into Judgment therefore fired **Normal**-type Judgment (the Zap
Plate's `onModifyType` had been applied to Recover's `MoveData` and discarded), and Normal is IMMUNE
to the Ghost-type Sableye in front of it, so the move failed at moveStep 3 with no accuracy roll
where PS KO'd the Sableye. **The redirect was never missing; its POSITION was.**

**rb1765 / rb1591 — one inversion seen from both sides.** `spreadMoveHit` runs
`runEvent('DamagingHit')` (step 7) and then `if (moveData.onAfterHit && pokemon.hp)` (step 8), and
everything that can kill the ATTACKER is later still: `move.recoil` at the end of
`hitStepMoveHitLoop`, Life Orb's `onAfterMoveSecondarySelf` at `useMoveInner:533`. The engine had
the step-8 payload group sitting between selfDrops and secondaries, with recoil/Life Orb ALREADY
applied.

* rb1765: a 16-HP Life Orb Samurott-Hisui lands Ceaseless Edge and dies to its own orb. PS lays the
  Spikes at `onAfterHit`, where `pokemon.hp` is still 16. New transient
  `Branch::after_hit_user_alive` — snapshotted at the top of `apply_post_damage`, i.e. after the hit
  loop and before drain/recoil/Life Orb — is now what the three self-gated onAfterHit payloads
  (Ceaseless Edge, Stone Axe, Glaive Rush) read.
* rb1591: a Ditto transformed into Glimmora uses Mortal Spin into the real Glimmora. Toxic Debris
  (step 7) scatters a Toxic Spikes layer on the ATTACKER's side; Mortal Spin's `onAfterHit` (step 8)
  then removes the spinner's OWN side conditions, including that layer. Net zero. `apply_spin_clear`
  moved to the step-7 boundary in both arms of the secondary composition.

---

## The bare-`hp` classifier, and the table it produced

The instruction was not to go game-by-game blind, and the classifier is the reason two of this
tranche's six commits exist. The pass is mechanical and takes ~4 minutes for the whole open set:

```
SEED_GATE=1 VERBOSE=1 cosim <sidecar>                  # -> dN[tT]:label | field
SEED_GATE=1 DBG_GAME=g DBG_I=N DBG_INSTR=1 DBG_DIFF=1  # -> Damage instructions + every DIFF
<read the sidecar's own d(N-1) stateAfter>             # -> item/ability/weather/screens/HP of both actives
```

The script is 60 lines of Python and is worth re-creating rather than preserving: the useful output
is the TABLE, and the table's lesson is that **the bare-`hp` tail is not one class, it is a dozen
SPECIES/ITEM clusters**, and a cluster of three is findable in an afternoon where a singleton is not.

**Buckets at the START of the pass (46 open games), ranked:**

| bucket | n | games | outcome |
|---|---|---|---|
| **Terapagos** (Tera Shell / Tera Starstorm / forme regression) | 6 | rb1040 rb1184 rb1347 rb1795 rb5064 rb5100 | **3 closed** (commit 5); the other 3 now differ by ≤5 HP |
| **Eiscue / Ice Face** | 3 | rb1410 rb1629 rb1710 | **2 closed** (commit 6) |
| **Exeggutor-Alola / Harvest / Sitrus** | 3 | rb1683 rb1711 rb5073 | open |
| **Keldeo / Secret Sword** (`overrideDefensiveStat`) | 3 | rb1642 rb1670 rb5039 | open — see below |
| **Loaded Dice** | 3 | rb1314 rb1416 rb1661 | open |
| **Mold Breaker on the attacker** | 3 | rb1430 rb1588 rb1612 | open |
| **Regigigas / Slow Start** | 2 | rb1525 rb1649 | open |
| **Mimikyu / Disguise** (the OTHER direction) | 2 | rb1191 rb1421 | open |
| singletons | 21 | — | open |

The two clusters that were taken paid 5 games between them. Both were invisible from the divergence
FIELD alone (`s0#0.hp`, `s1.active_index`, `s0.active_turns`, `s1#2.hp`) and obvious from the
species column.

**Two named leads for whoever picks this up, both already localized:**

1. **Secret Sword (rb1670 d43).** PS's recorded draws are `randomChance[1,24]=True` (a CRIT) then
   `random[16]=7`, and the crit branch's arithmetic checks out by hand against the engine's own
   formula: base 99 → crit 148 → roll 7 → 137 → STAB 205 → Fighting-vs-Poison ÷2 → **102**, which is
   exactly PS's damage. The engine nevertheless resolved the unit on a NON-crit branch. That means
   the crit branch was not live in the selector, which means the PRNG position was already off by a
   draw when the crit roll happened — **a draw-count imbalance that produced no state difference in
   any earlier decision**. `PRNG_TRACE=rb1670` is the tool; this is an offset bug wearing a damage
   bug's clothes, and the third open game in the cluster (rb1642, `result random[16]@shadowball`)
   has the same shape.
2. **Exeggutor-Alola (3 games).** Every one has a Harvest holder with a Sitrus Berry on at least one
   side and a large HP gap (78 / 20 / 78). Harvest's regrowth roll, the Sitrus threshold and the
   `eachEvent('Update')` position are all in play; start from `DBG_INSTR` on rb5073 d51, the biggest
   gap.

---

## Rules this tranche cost a landing each to learn

> **`draw(...)` is not just a stream-advance, it is a branch PREDICATE.** The seed gate does not
> score branches for closeness: it draws from the live PRNG and keeps only the branches whose
> RECORDED result equals what it drew (`seedgate.rs:330-352`). The Ice Face / Disguise arms emitted
> their discarded crit and damage rolls with the results hardcoded to `0`, on the reasoning that the
> values cannot matter. They cannot matter to the branch. They eliminate the branch. Any site that
> emits a draw whose result it does not care about must still record the REALIZED value.

> **`runEvent('OverrideAction')` is the first thing `runMove` does**, before the move object every
> `onModifyType` / `onModifyMove` edits exists. A redirect implemented anywhere downstream silently
> discards the modifier chain.

> **Tera Shell REPLACES the net type mod with −1.** `runEffectiveness` returns `-1`, it does not add
> a resist step — so a 4× hit and a resisted hit both come out at exactly ×0.5, and the engine's
> "one more `down`" was right only for a neutral hit. Any `onEffectiveness` handler that RETURNS a
> value is a replacement, not a modifier; Freeze-Dry (already modelled) is the same shape.

> **Stellar never consults the type chart.** `runEffectiveness` opens with `if (this.terastallized
> && move.type === 'Stellar') totalTypeMod = 1` and skips the per-type loop: ×2 into any
> Terastallized target, neutral into everyone else, no immunities and no resistances. Its STAB rule
> is separate too (`isSTAB ? 2 : [4915, 4096]`, with `ModifySTAB` skipped).

> **`formeRegression` restores the SET species, not `baseSpecies`.** For Ogerpon the two agree,
> which is why the existing arm worked; for Terapagos they do not, because Tera Shift's own
> permanent forme change already moved `baseSpecies`. It is also the first regression that MOVES max
> HP, and `updateMaxHp` is `this.hp = this.hp <= 0 ? 0 : Math.max(1, newMaxHP - (this.maxhp -
> this.hp))` — a corpse stays at 0 instead of going negative.

> **The "two engine copies drift" rule is now a THREE-copy rule.** The hit loop exists three times
> (`apply_damage_hit_rolls`, `apply_damage_hit_indexed`, `apply_multihit_realized_ma`) and the third
> had neither Ice Face nor Disguise. The `onDamage`-returns-0 abilities exist twice (Ice Face,
> Disguise) and only one of them handled `pivot`. When you find a PS predicate in the engine, grep
> for the second implementation — and then for the third.

---

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **395 / 401 = 98.5%** (was 393) |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **372 / 400 = 93.0%** (was 366) |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **878 / 912 = 96.3%** (was 870); init-aligned 912 / 912 |
| **Seed gate, randbats 101** | `bash harness/gate-rb.sh` | **94 / 101 = 93.1%** (was 93); init-aligned 101 / 101 |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS **100.00%** |
| State sweep, randbats | `cosim harness/seed-sidecars-rb/*.json.gz` | **99.73%** (was 99.62%), coverage 100.00% |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; zero `rust extra` |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** — 3829/3829 move units, 4832/4832 states |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | 18 suites green (12 new Illusion tests) |

## Asymptote assessment

Not an exhausted seam. Six parity commits, per-commit yield 1, 1, 2, 2, 3, 2 games across the two
rails — **1.8 games/commit, up from the ruleset tranche's 0.5 and burn-down XIII's 1.7** — and no
commit flipped zero. The classifier is why: it converted a flat list of 46 "bare `hp`" games into
eight named clusters, and the two clusters taken paid 5 games for 2 commits.

**Recommendation for the next tranche, in order:**

1. **Re-run the classifier first, every time.** It is cheap, it re-ranks after every landing, and it
   is the only thing that turns the tail into work items.
2. Take the Exeggutor-Alola / Harvest cluster (3) and the Mold Breaker cluster (3).
3. `PRNG_TRACE` the Keldeo cluster — the arithmetic says the damage formula is RIGHT there and the
   stream is off, which makes it an offset bug and a different kind of fix.
4. Record seeds 1801-2200. Burn-down XIII's recommendation is still unspent, and the fresh half is
   still 5.5 points behind the pinned half (93.0% vs 98.5%) even after this tranche moved it +6.
5. If Illusion needs to be gated harder than the state sweeps gate it, extend
   `harness/protocol-parity.mjs` to drive a randbats sidecar's `packedTeams` and diff the
   `|replace|` / `|-end|` lines of rb5005 / rb5017 / rb5047 against the real PS log.

---

# ==== RULESET TRANCHE — configurable formats + the first real-format corpus (2026-07-27) ====

**HEADLINE: format rules are configurable, and `[Gen 9] Random Battle` is now a gated corpus —
93 / 101 byte-exact from seed (92.1%), init-aligned 101 / 101.** The customgame rail moved
868 → **870 / 912** as a side effect, with the newly-non-exact set EMPTY at all ten commits.

## What landed

| # | commit | what |
|---|--------|------|
| 1 | `ab1068e` | **`engine::ruleset::Ruleset`** — one `Copy` struct on `State`, two presets, `from_format` |
| 2 | `f7ecaf2` | **the `trunc` arm** — 13-bit Speed wrap, the 10000 Speed cap, 16-bit damage |
| 3 | `b3c6f03` | **Sleep Clause Mod** — the block emits PS's message/hint pair; H4 assertion; 6 tests |
| 4 | `100d3bf` | **recorder** — real formatid, synthetic `start` decision, `sleepclausemod` pseudo-weather |
| 5 | `fd2ef0b` | **the corpus** (101 games, 88 exact) + `hp_frac`'s 99 clamp + `maybe_trapped` |
| 6 | `d5bd66f` | `hitStepTryImmunity` is moveStep 3 — hoisted above every move special case |
| 7 | `cb2eaa5` | Pressure taxes the CALLER's PP for the CALLED move's targets |
| 8 | `e36decc` | slp / frz return `undefined` on WAKE / THAW — the BeforeMove ladder continues |
| 9 | `c1208d3` | a Struggling action has STRUGGLE's priority, not the empty slot's |
| 10 | `840ce8e` | Queenly Majesty / Dazzling / Armor Tail never block a SELF-targeting priority move |

Commits 6-10 are parity fixes found by the new corpus: 88 → 93. Two of them (8) also paid on the
old rail (rb1418, rb1555).

## The `Ruleset` design as landed

`Ruleset` is a small all-scalar `Copy` struct built once at battle init and never mutated. It sits
**on `State`**, replacing the old bare `sleep_clause: bool`, so the interior of `generate.rs` can
read it without a parameter threaded through 13k lines — but it is deliberately **absent from the
field manifest** `cosim::diff` / `cosim::digest` walk. It is battle CONFIGURATION, not battle
state.

Two presets, `GEN9_CUSTOM_GAME` (exactly the pre-tranche behaviour, and the default everywhere
including `State::EMPTY`) and `GEN9_RANDOM_BATTLE`. Flags, and which layer each lives in:

| flag | layer | draws? |
|---|---|---|
| `sleep_clause` | core | **yes** — suppresses the `random(2,5)` duration roll |
| `bit_truncation` | core | **yes, indirectly** — Speed wrap → turn order → tie shuffles |
| `endless_battle_clause` | core | no — `false` in both presets, unimplemented behind the flag |
| `infer_foe_trapping_abilities` | request | no |
| `report_exact_hp`, `emit_debug_lines`, `illusion_level_mod`, `cancel_mod`, `rule_lines` | protocol | no |
| `team_preview` | protocol + entry contract | no |
| `max_team_size`, `max_move_count`, `picked_team_size` | request shape | no |

**The stamping hazard, and how it was defused.** All 912 committed recordings claim
`gen9randombattle` and were played as customgame. So the resolver does **not** key off `format`.
A new, explicit, optional `ruleset` field on the trace and the fixture names the formatid actually
handed to `new Battle`; **absent ⇒ `gen9customgame`**, which is what every legacy recording really
was. An unknown id errors loudly. Zero fixture churn, and the two stamps cannot disagree.

**The entry contract** is one shared helper, `trace::first_decision_state(&ruleset)` — decision 0
is `"teampreview"` with Team Preview and `"start"` without. `runPickTeam` is a complete no-op in a
no-preview format, so the recorder emits a SYNTHETIC decision 0 carrying the draws `battle.start()`
consumed and the board at the first move request: the exact role a teampreview decision plays.
Decision 1 onward is shape-identical, so `replay.rs` / `seedgate.rs` / `drawdiff.rs` /
`protocol_emit.rs` needed one predicate rather than four format-aware rewrites.

**Recorder change that made it possible:** p2 is held out of the `new Battle` options and added
with `battle.setPlayer('p2', …)` AFTER `instrumentPrng` — `setPlayer` is what calls `start()`
(`sim/battle.ts:3279`), and without Team Preview `start()` runs the whole `'start'` action and
turn-1 setup inline. Only the no-preview arm defers, so the Team-Preview arm stays byte-identical
to how the 912 were recorded and their sidecars stay regenerable.

## The trunc arm: **the 13-bit Speed wrap is real, and the SPEC's example is wrong**

`RULESET_SPEC.md` §9 predicts 504 → +6 2016 → Scarf 3024 → Tailwind 6048 → Swift Swim 12096 →
"wraps to 3904". **It does not.** `getStat` caps Speed at 10000 (`sim/pokemon.ts:638`) BEFORE
`getActionSpeed` truncates (`:649`), and that cap carries the same `!format.battle?.trunc` guard —
so it fires in exactly the formats that also truncate. 12096 caps to 10000, then truncs to
**1808**.

> **The reachable action-speed range under randbats is `[0, 8191]` for raw ≤ 8191, `[0, 1808]` for
> raw in `[8192, 10000]`, and the single value 1808 for every raw above that. `(1808, 8191]` is
> unreachable BY WRAPPING.** The practical wrap window is raw Speed 8192..10000 — 1809 wide, not
> "everything past 8192". Raw 8192 truncates to exactly **0**, which both inverts turn order
> against any ordinary foe and drops the mon into the speed-0 tie group the field-effect handlers
> occupy (H4's second edge case).

**No wrap was observed in the 101-game corpus** — 92.1% exactness was reached without one, and no
divergence label mentions turn order. That is the expected result: randbats levels are ≤ 100 and
the wrap needs a +6 / Scarf / Tailwind / weather-ability stack on an already-fast mon. It is
implemented, unit-tested (`crates/engine/tests/ruleset_trunc.rs`) and inert until it is not.

**Known residual, documented at the call site.** PS interposes Trick Room's `speed = 10000 - speed`
BETWEEN the cap and the truncation; the engine models Trick Room by inverting the comparison. The
two induce the same order AND the same tie set for every Speed ≤ 1808 (there
`trunc(10000 - s, 13) == 1808 - s`, strictly decreasing), so a disagreement needs Trick Room and a
>1808 effective Speed at once. Fixing it means making ~20 comparison sites read a signed action
speed instead of flipping — not worth the regression surface.

The 16-bit damage truncation is implemented and, as predicted, unreachable at legal levels (base
damage would have to reach 65536).

## Sleep Clause Mod

Mechanics were already right; this tranche added the evidence and PS's output.
`Instruction::SleepClauseBlocked` is a protocol-only marker (apply and reverse are both `{}`)
pushed at all five status sites and rendered as PS's exact pair — **both lines on every
activation**, because `hint()` is called without its `once` argument (`sim/battle.ts:3092`), and
**no `|-fail|` after them**, because the block makes `didAnything` `null` rather than `false`.

Each of the five sites is now `pre_clause` / `clause` / `applies` rather than a flat conjunction.
That is not cosmetic: subOrder 5 means the clause runs LAST, so a Safeguard or Misty Terrain block
short-circuits before it and PS prints nothing. The old flat form could not tell those apart.

`Ruleset::set_status_rule_handlers()` is H4's assertion, with a test: the clause's tuple in a
`SetStatus` list is `(∞, 0, 0, 5, 0)` and nothing in either preset shares it, so it is
shuffle-neutral. A second subOrder-5 Rule would cost one `prng.shuffle` per SetStatus.

**Corpus composition, from PS's own `battle.log`** (the recorder now stamps
`sleepClauseActivations` / `sleepInflictions`, because a blocked sleep leaves NO trace in the
serialized state — it is the absence of a status and of a draw):

> **Sleep is rare in gen-9 randbats: 5 of 100 games inflict any sleep at all.** Four of the base
> 100 exercise the clause (rb5008, rb5012, rb5021, rb5086; 5 activations). rb5139 was recorded
> from a 5101-5200 scan to clear the ≥5 bar — that scan found exactly ONE more clause game in 100.
> A directed tranche, not a bigger blind range, is the way to raise this.

## Observation layer

**`hp_frac`'s missing 99 clamp is fixed** — a live bug independent of the format work.
`getHealth` does `ceil(100*hp/maxhp)` and then forces 99 whenever `hp < maxhp`; we did
`.clamp(1, 100)`. A 403/404 mon rendered `100/100` where PS says `99/100` — the log claimed an
untouched mon. Reachable for any `maxhp > 100` at `hp = maxhp - 1`. It never fired because
customgame's `format.debug` puts every recording on the exact-HP arm.

`HpStyle::for_ruleset` derives the style from `report_exact_hp` at all three call sites. **`HP
Percentage Mod` is NOT the switch** — `reportPercentages || gen >= 7` takes the percent branch
either way in gen 9; the real switch is `format.debug`. The request JSON's own `condition` is
always exact regardless (`getHealth().secret`).

`generate::maybe_trapped` implements the `FoeMaybeTrapPokemon` sweep (17 gen-9 species carry Arena
Trap / Shadow Tag / Magnet Pull, enumerated from the pin), and `replay.rs` now compares `trapped`
and `maybeTrapped` **separately** under a ruleset that runs the sweep. Conflating them was only
sound while customgame skipped it: under randbats a mon facing a Sand Veil Dugtrio gets
`maybeTrapped: true` on a perfectly legal switch.

**Illusion Level Mod is a flag with no implementation surface** — the engine does not model
Illusion at all (the ability appears only in Trace's exclusion list). Flagged for whoever adds it.

## Five roots the real format found, and the shape they share

1. **`hitStepTryImmunity` is moveStep 3, before accuracy (4).** `status_try_immunity_fails` was
   correct but sat 700 lines BELOW the `md.id` special-case chain, and Trick/Switcheroo is IN that
   chain and emits its own accuracy draw — under a comment asserting Sticky Hold blocks "later, at
   `onTakeItem`". It does not. Now hoisted to the top of `execute_status_move`.
2. **Pressure taxes the CALLER's PP, for the CALLED move's targets** (`battle-actions.ts:472-483`).
   Sleep Talk has `pp: 10`, so it is a `callerMoveForPressure`: a Sleep Talk that rolls a
   foe-targeting move into a Pressure holder costs **2 PP of Sleep Talk** and 0 of what it called.
3. **`slp` / `frz` `onBeforeMove` return `undefined` when the mon WAKES or THAWS** — `false` only
   while it stays asleep/frozen. So a mon that woke this turn still runs Truant (9), Disable/Taunt
   (7/6/5), **confusion (3) and Attract (2)**. The engine returned straight into the move
   machinery. Everything below slp/frz is now one function with three callers.
4. **A Struggling action has Struggle's priority.** `runMove` swaps the move out
   (`battle-actions.ts:255-275`) before `getActionSpeed` reads `action.move.priority`. The engine
   read the empty slot's — so a Choice-locked mon out of PP on Protect Struggled at **+4**.
5. **Queenly Majesty / Dazzling / Armor Tail read the move's TARGET, not just its priority.**
   `source.isAlly(dazzlingHolder)` in `onFoeTryMove(target, source, move)` means "the move's
   resolved target is the holder" — so a SELF-targeting priority move (Protect, Detect, King's
   Shield) is never blocked. The engine failed the foe's Protect.

> **Four of those five are "two engine copies of one PS computation drifted", and in three of them
> the OTHER copy was already right** — `status_try_immunity_fails` vs the Trick branch, the
> BeforeMove ladder vs the sleep short-circuit, Psychic Terrain's `target != User` vs Queenly
> Majesty's missing one. Burn-downs XII and XIII said this about hand-copied LISTS and duplicated
> COMPUTATIONS. It is now the single most productive thing to grep for: when you find a PS
> predicate implemented in the engine, look for the second implementation before doing anything
> else.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| **Seed gate, randbats 101** | `bash harness/gate-rb.sh` | **93 / 101 = 92.1%**; init-aligned **101 / 101** |
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **393 / 401 = 98.0%** |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **366 / 400 = 91.5%** |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **870 / 912 = 95.4%**; init-aligned **912 / 912** |
| Draw differ, audited | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; zero `rust extra` |
| Draw differ, randbats | `DRAW_DIFF=1 cosim harness/seed-sidecars-rb/*.json.gz` | **3688 / 3712 = 99.35%** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831**, EXACTNESS 100.00% |
| State sweep, randbats | `cosim harness/seed-sidecars-rb/*.json.gz` | **99.62%**, coverage 100.00% |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine + cosim tests | `cargo test --release -p engine -p cosim -j 2` | 17 suites, 102 tests, green |

## The 8 remaining randbats opens are ONE class

`rb5021 rb5026 rb5037 rb5039 rb5059 rb5064 rb5073 rb5100` — **all eight are the damage/HP
asymptote**, the same class that is 31 of the 42 customgame opens. Five report
`draws-match/state-diff` on a bare `hp`, two report a `result random[16]` (the differ picked a
different damage roll because the HP it had to reproduce was off), and rb5021 is a **one-HP** Giga
Drain difference with the draws matching exactly.

rb5021 also carries a finding worth keeping even though it costs nothing: the engine emits FIVE
pre-move shuffles where PS emits four, and FOUR post-move where PS emits five. The totals are
equal, so the stream stays aligned and the game's only divergence is the 1 HP — but there is a
real ordering difference between the pre-move `BeforeTurn`/`Update` bracket and the post-move one
hiding under an accidentally-matching count.

**Recommendation for the next tranche.** The randbats corpus is at 92.1% after ONE tranche, versus
87.0% for the fresh customgame 400 when it was new and 91.5% now after fourteen. There is nothing
format-shaped left in it — take the damage/HP asymptote, on either rail, with the differ's
`randomChance[3, 10]@contact-status` (a `rust extra`) and the 7-unit
`ps unconsumed shuffle[2, 0, 2]@generic` class as the two named leads.

---

# ==== BURN-DOWN XIII — certification (2026-07-27) ====

**HEADLINE: 868 / 912 games byte-exact from seed (95.2%), up from 851.** Per corpus: audited
**111 / 111**, pinned-401 **393 / 401** (so 504 / 512 = 98.4% on the old "512" reading), fresh-400
**364 / 400 = 91.0%**. Init-aligned 912 / 912.

Ten parity commits, each PS-source-grounded, judged by the exact-SET diff over all 912 at every
step: **the newly-non-exact set was EMPTY at all ten.** Per-commit yield 1, 2, 2, 2, 3, 1, 1, 1,
3, 1 — **no commit flipped zero**, so the kill criterion (three consecutive zero-yield parity
commits) was never approached. 17 games / 10 commits = **1.7 games/commit**, up from XII's 1.0.
The tranche stopped on its 12-commit budget.

## The fresh corpus is now first-class

`402a3ac` committed `harness/seed-fixtures-fresh/` (400 slim fixtures, 3.4 MB) and
`harness/gate-912.sh`, which runs the seed gate over all three corpora and writes the NON-EXACT
game SET to a file. **The regression judgment is a set diff, never a count diff:**

```
bash harness/gate-912.sh /tmp/before.txt     # at the parent commit
...fix...
bash harness/gate-912.sh /tmp/after.txt
comm -13 /tmp/before.txt /tmp/after.txt      # newly-non-exact — MUST BE EMPTY
comm -23 /tmp/before.txt /tmp/after.txt      # the yield
```

> **The script passes `VERBOSE=1` deliberately.** Without it `seedgate.rs:973` truncates the
> per-game divergence listing at 45 rows and the set comes out silently SHORT — this cost the
> first reading of the tranche (858/912 instead of 851/912). Any tool that scrapes that listing
> must set it.

**Burn-down XII's recommendation is confirmed by the yield.** Of the 17 games flipped, 16 are
fresh and 1 is pinned (rb1126, one of the nine standing pinned opens, closed as a side effect of
the Liquid Ooze commit). The marginal open game really is cheaper in the fresh half.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 = 100%** (ABSOLUTE INVARIANT, held at every commit) |
| Seed gate, pinned 401 | `SEED_GATE=1 cosim harness/seed-fixtures/*.fx.json.gz` | **393 / 401 = 98.0%** |
| Seed gate, fresh 400 | `SEED_GATE=1 cosim harness/seed-fixtures-fresh/*.fx.json.gz` | **364 / 400 = 91.0%** |
| **Seed gate, all 912** | `bash harness/gate-912.sh` | **868 / 912 = 95.2%**; init-aligned **912 / 912** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; zero `rust extra` |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, 0 unsupported; EXACTNESS 100.00% |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS**, every convertible state |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |
| Cosim tests | `cargo test --release -p cosim -j 2` | green |

## The roots landed (in commit order)

| # | commit | root | 912 |
|---|--------|------|-----|
| 0 | `402a3ac` | **corpus**: the fresh 400 become committed fixtures; `gate-912.sh` | baseline 851 |
| 1 | `8ccc8c6` | **A faint replacement that ENDS the battle freezes `statsRaised/LoweredThisTurn`** — `go()` returns on `this.ended` before `nextTurn()` | 852 (rb1433) |
| 2 | `2e4396b` | **PS reaches `nextTurn` only when BOTH active slots are filled.** `go()` also returns on `this.requestState`; both early exits look identical in state (a fainted mon in an active slot). One predicate `next_turn_reached` replaces two divergent guards | 854 (rb1529, rb1628) |
| 3 | `647ce86` | **Well-Baked Body and Wind Rider block STATUS moves too**, at `hitStepTryHitEvent` (step 2) — so no accuracy draw. Soundproof/Bulletproof folded in | 856 (rb1432, rb1650) |
| 4 | `85622a6` | **Knock Off (`onAfterHit`, step 8) cannot take an item that consumed itself at `DamagingHit` (step 7)** — Weakness Policy fires first | 858 (rb1447, rb1544) |
| 5 | `05c947a` | **Disable (7) / Throat Chop, Heal Block, Gravity (6) / Taunt (5) outrank confusion (3), Attract (2) and paralysis (1)** in the `BeforeMove` ladder; a cancel there rolls nothing and pays no PP | 861 (rb1412, rb1493, rb1682) |
| 6 | `606bc2d` | **`DamagingHit` is NOT speed-sorted** (`compareLeftToRightOrder`, `battle.ts:789`) — every TARGET handler rolls before every SOURCE handler, so Cursed Body precedes Toxic Chain regardless of Speed | 862 (rb1520) |
| 7 | `374de81` | **Electric Terrain refuses the Yawn VOLATILE** on a grounded target (`onTryAddVolatile`) — a different, shorter list than "can be put to sleep", and Misty Terrain is NOT on it | 863 (rb1778) |
| 8 | `acbf34b` | **An absorbing ability does not save an Exploding user from its own faint** — `useMoveInner:501` precedes `trySpreadMoveHit:519` | 864 (rb1774) |
| 9 | `e928258` | **Liquid Ooze's `canOoze` list is `['drain','leechseed','strengthsap']`**, not `drain` alone; the ooze damage is uncapped by missing HP because `TryHeal` runs before `heal`'s full-HP bail | 867 (rb1126, rb1739, rb1745) |
| 10 | `d029c37` | **`runSwitch` is the one bracket sort that passes `includeFainted`** (`battle-actions.ts:181`), and a fainted slot sorts on the `clearVolatile`-restored `storedStats.spe` | 868 (rb1706; also moved rb1710 from d7 to d18) |
| 11 | this commit | docs | — |

## Rules this tranche added, each of which cost a landing to learn

> **`go()` returns early on `this.ended` OR `this.requestState`, and both look the same in state:
> a FAINTED MON IN AN ACTIVE SLOT.** Everything `nextTurn` does — the per-turn marker resets — has
> not happened yet on such a board. Two independent copies of the `statsRaisedThisTurn` clear had
> two different, both incomplete, guards; they are now one `next_turn_reached`. Whenever you model
> "end of turn", ask whether PS actually got there.

> **`runEvent` speed-sorts its handlers EXCEPT for four event ids.**
> `['Invulnerability', 'TryHit', 'DamagingHit', 'EntryHazard']` use
> `Battle.compareLeftToRightOrder` (`sim/battle.ts:789-790`): `order` ascending with undefined
> mapped to **4294967296** (so ordered handlers run FIRST and unordered last), then `priority`,
> then `index` — which is 0 for a single target, leaving a STABLE sort over the collection order.
> `findEventHandlers` collects the target's `on<Event>` first and the source's `onSource<Event>`
> last. Speed is not consulted at all.

> **A volatile is refused by `onTryAddVolatile`, a status by `onSetStatus`, and the two lists are
> different.** Electric Terrain blocks `yawn` and not `confusion`; Misty Terrain blocks
> `confusion` and not `yawn`. `status_blocked_by_field` answers the status question and is NOT a
> drop-in for the volatile one.

> **The `BeforeMove` ladder, enumerated from the pin** (`…filter(x => x.onBeforeMove)`, sorted by
> `onBeforeMovePriority`): `100` glaiverush/grudge/rage/chillyreception, `11` mustrecharge,
> `10` slp + frz, `9` truant, `8` flinch, `7` disable, `6` gravity + healblock + throatchop,
> `5` taunt, `3` confusion, `2` attract, `1` par, `0` choicelock + gorillatactics,
> `-1` destinybond. `runEvent` short-circuits on the first `false`, so everything below the
> firing handler — including its DRAW — does not happen.

> **`getAllActive()` excludes fainted mons; `getAllActive(true)` does not, and exactly one sort in
> the switch bracket passes `true`.** That is why a pivot landing next to a corpse consumes ONE
> shuffle rather than zero or three. And the corpse sorts on `storedStats.spe`, because
> `clearVolatile` -> `setSpecies(baseSpecies)` -> `this.speed = this.storedStats.spe`. This is
> burn-down XII's cache rule again, now for a fainted ACTIVE rather than a benched mon — the third
> tranche in a row in which `pokemon.speed`-is-a-cache paid.

> **Two engine copies of the same PS computation always drift.** This tranche merged three such
> pairs: the two marker clears (`next_turn_reached`), the two `BeforeMove` 7/6/5 checks
> (`before_move_blocked_7_6_5`), and the absorb list (status and damaging moves now share one
> block). Burn-down XII said it about hand-copied LISTS; it is equally true of duplicated logic.

## The 44 still-open games, evidenced (`DBG_DIFF` on the SIDECARS)

**8 pinned + 36 fresh.** A slim fixture has no `stateAfter` and can only ever say `state-digest`,
so every line below was taken against `harness/seed-sidecars/`.

```
PINNED (8)
  rb1011 d43 t33  s0#3.hp 140/77
  rb1012 d60 t52  s0#2.hp 138/185
  rb1040 d2  t3   s0#0.hp 230/217
  rb1184 d5  t6   s1#4.hp 196/142
  rb1191 d17 t14  s0#1.hp 25/33     PS shuffle@thunderbolt vs rust randomChance@accuracy
  rb1236 d37 t29  s0#4.hp 51/18
  rb1314 d45 t38  s1#0.item LightClay/None   surfaces at a Revival Blessing
  rb1347 d69 t64  s1#1.hp 245/259

FRESH (36) — first divergent FIELD
  bare .hp, no second field (24)
    rb1448 rb1502 rb1525 rb1572 rb1581 rb1588 rb1612 rb1636 rb1642 rb1670 rb1683 rb1713
    rb1751 rb1769 rb1781 rb1795 rb1416 rb1464 rb1555 rb1629 rb1734 rb1711 rb1421 rb1710
  named / multi-field (12)
    rb1418  s0.volatiles bit0 Confusion + confusion_turns 1/0   engine confuses, PS does not
    rb1421  same bit0 + confusion_turns 5/0, plus species 496/495 (Mimikyu-Busted/Mimikyu)
    rb1621  species 495/496 — the OPPOSITE direction; Triple Axel into an intact Mimikyu.
            **Disguise is single-hit-gated in the engine (`md.hits_max == 1`); PS busts on
            hit 1 and damages with hits 2-3.** rb1421 points the other way, so they are two
            roots, not one.
    rb1430  s0.boost.atk 3/1        engine over-boosts by two stages
    rb1573  s0.boost.spa 3/2 + StatsRaisedThisTurn set where PS has none (downstream)
    rb1649  s0.volatiles bit1 Substitute + substitute_hp 80/0 + two move PPs
    rb1661  s0.substitute_hp 12/13  PS shuffle@scaleshot vs rust randomChance@crit
    rb1680  field.terrain None/Electric
    rb1681  s1.wish (1,214)/(2,214) — a Wish counter one turn out
    rb1734  **Encore does not redirect an already-chosen action.** Sableye's Prankster Encore
            lands on an Arceus-Electric that picked Recover; PS's `onOverrideAction` turns it
            into Judgment and KOs Sableye. The engine let Recover run.
    rb1760  s1.active_index 3/2     `rust-extra sample[5]@drag`
    rb1765  s1.sc.spikes 0/1        a Spikes layer PS has and the engine does not
    rb1591  s1.sc.toxic_spikes 1/0  the mirror case
```

**The bare-`hp` tail is 24 fresh + 7 pinned = 31 games and is still the campaign's asymptote.**
Nothing localizes them but `DBG_INSTR` to the first diverging instruction plus PS source at the
pin, one modifier at a time. Everything cheaper than that has now been taken twice over.

## Asymptote assessment

The kill criterion did not fire and the yield per commit went UP (1.0 -> 1.7), so this is not an
exhausted seam — it is a seam whose remaining ore is unevenly distributed:

* **12 of 44 opens still carry a second field**, and a second field is what makes a root findable
  in one sitting. Every root this tranche landed came from one. Three of the twelve are already
  named above with the PS source line that explains them (Disguise multi-hit, Encore's
  `onOverrideAction`, the two hazard-layer mismatches); they are the next tranche's obvious start.
* **The remaining 31 are a bare `hp`**, and their cost has not moved across three tranches. They
  are damage-formula or damage-ordering bugs with no other symptom, and the only tool is
  instruction-level bisection.
* **The fresh half is still 7 points behind the pinned half** (91.0% vs 98.4%) after this tranche
  moved it +4.0 and the pinned half +0.2. That gap is the honest estimate of how much of the
  engine the pinned corpus never exercises, and it says a THIRD batch of seeds would again find
  roots the first two cannot. Recording is cheap (400 games / 510 s / one node process); the
  fixtures are 3.4 MB.
* Nothing in the 44 is a PRNG-offset class any more except rb1191, rb1661 and rb1760 — the offset
  class is three singletons with named draw-label mismatches, not a cluster.

## Recommendation for the next tranche

1. **Take the three named multi-field roots first** — Disguise on a multi-hit move (rb1621),
   Encore's `onOverrideAction` redirect (rb1734), and the Spikes / Toxic Spikes layer pair
   (rb1765, rb1591) — then the confusion pair (rb1418, rb1421).
2. **Record seeds 1801-2200** before starting the bare-`hp` grind, and gate on 1312. The measured
   pinned-vs-fresh gap says the marginal fresh game is still the cheaper one.
3. `bash harness/gate-912.sh <out>` on every commit; `comm -13 before after` MUST be empty.
4. `DBG_INSTR` is the triage tool, `DBG_DIFF` must be run against a SIDECAR to name a field, and
   `PRNG_TRACE` is a one-command confirmation that localizes an offset to the unit that CREATED
   it (it found both halves of commit 10).

## Extended CI gate

```
cargo test --release -p engine -j 2            # 12 suites
cargo test --release -p cosim  -j 2
target/release/cosim harness/cosim-traces/*.json.gz          # 3831/3831, EXACTNESS 100.00%
DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz   # >= 99.45%, zero `rust extra`
ROUNDTRIP_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz
bash harness/run-distribution-smoke.sh         # 18/18
bash harness/gate-912.sh /tmp/after.txt        # audited 111/111 (HARD), 912 total >= 868
comm -13 /tmp/before.txt /tmp/after.txt        # MUST be empty
```

---

# ==== BURN-DOWN XII — certification (2026-07-27) ====

**HEADLINE: 503 / 512 pinned-corpus games byte-exact from seed (98.2%), up from 497; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.** And, separately: **400
FRESH games were recorded (seeds 1401-1800) and gated for the first time — 348 / 400 = 87.0%
exact, init-aligned 400 / 400.**

Seven parity commits, each PS-source-grounded, judged by the exact-SET diff on BOTH corpora at
every step: **the newly-non-exact set was EMPTY at all seven.** Per-commit yield on the pinned
corpus 1, 1, 1, 1, 1, 1, 0 — the trailing zero is the Cramorant commit, which has NO pinned
witness at all and flipped **three** fresh games instead. Kill criterion (three consecutive
zero-yield commits) not approached.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, pinned 512 | `SEED_GATE=1 cosim … seed-fixtures/*.fx.json.gz` | **503 / 512 = 98.2%**; init-aligned **512 / 512** |
| Seed gate, FRESH 400 | seeds 1401-1800, fixtures built from the new sidecars | **348 / 400 = 87.0%**; init-aligned **400 / 400** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; zero `rust extra` |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, 0 unsupported |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS**, every convertible state |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |
| Cosim tests | `cargo test --release -p cosim -j 2` | green |

## The roots landed (in commit order)

| # | commit | root | pinned |
|---|--------|------|--------|
| 1 | `68d3a7c` | **A DRAG never refreshes the incoming mon's Speed cache.** `switchIn(..., isDrag=true)` on gen ≥ 5 calls `runSwitch(pokemon)` DIRECTLY (`sim/battle-actions.ts:145-150`) instead of `queue.insertChoice({choice:'runSwitch'})` — and `insertChoice` is the ONLY caller of `updateSpeed()` (`sim/battle-queue.ts:374`) | 497 → 498 (rb1360) |
| 2 | `990184f` | **A transformed mon's identity was wrong in TWO import fields** — `base_types` (element-wise patch) and `base_moves` (unrecoverable from a snapshot; now rebuilt from the packed teams) | 498 → 499 (rb1359) |
| 3 | `70d474c` | **Trace is an `onUpdate` handler** — a failed copy RETRIES at every `eachEvent('Update')` until a traceable foe appears | 499 → 500 (rb1244) |
| 4 | `2b004c1` | **The priority BLOCKERS carried their own hand-copied ModifyPriority list and it was missing Triage**; `onFoeTryMove` is three abilities, not one | 500 → 501 (rb1348) |
| 5 | `bd70ee9` | **A FIXED-damage move ignored the target's Substitute** (and would not have counted `timesAttacked` if it had) | 501 → 502 (rb1326) |
| 6 | `53b3c36` | **Beak Blast burns anything that makes CONTACT while the beak is heating** — wholly unmodelled | 502 → 503 (rb1108) |
| 7 | `382fc45` | **Cramorant-Gulping / Gorging is NOT a permanent forme** — it reverts on switch-out and faint, and the missile fires from a Cramorant that FAINTED to the hit | 503 (no pinned witness); fresh +3 |
| 8 | this commit | docs | — |

## THE FRESH-SEED QUESTION, ANSWERED WITH DATA

`bash harness/record-seeds.sh 1401 1800` — 400 games, 0 failures, 510 s, one node process. Fixtures
built with `MAKE_FIXTURE=… cosim harness/seed-sidecars/rb1[4-8]*.json.gz`.

**The fresh-game exact rate is 87.0% (348/400), against 98.2% on the pinned 512.** That gap is the
whole answer to "is the pinned corpus mined out": it is not mined out of BUGS, it is mined out of
*this sample's* bugs. Eleven points of exactness live in mechanics the 401 old seeds never touched.

**Shared classes regrow — measured, not argued.** Two of this tranche's roots were found on one
corpus and paid on the other:

- **Beak Blast** was diagnosed from pinned rb1108 and flipped fresh **rb1453, rb1616, rb1719** as
  well. Its pinned signature (`hp` + `status Burn`) recurs five times in the fresh batch.
- **Cramorant** was diagnosed from the fresh batch's `.species` cluster (rb1459, rb1780, rb1783 all
  reading `engine=cramorantgulping ps=cramorant`) and has NO pinned witness. It would never have
  been found by staring at the 512.

That is the empirical test burn-down XI asked for, and it comes out for recording more seeds.

### The 52 fresh opens, by FIRST DIVERGENT FIELD (per trap #1 — never by move name)

```
  25  .hp                (bare, no second field — the expensive shape, same as the pinned tail)
  10  .volatiles         rb1412 rb1418 rb1421 rb1433 rb1520 rb1529 rb1628 rb1649 rb1739 rb1778
   3  .boost.atk         rb1430 rb1544 rb1681
   2  .boost.def         rb1432 rb1650
   2  .active_index      rb1760 + 1
   1 each  field.terrain / .wish / .substitute_hp / .species / .sc.toxic_spikes / .sc.spikes /
           .move0.pp / .item / .boost.spa / .active_turns
```

Named sub-clusters already visible inside that table, with the volatile bits decoded
(discriminant = bit index, `volatile.rs`):

- **bit 39 `StatsLoweredThisTurn`** — rb1433, rb1529, rb1739 (`ps=Volatiles(549755813888)`, engine 0).
- **bit 38 `StatsRaisedThisTurn`** — rb1573, rb1628 (`ps=Volatiles(274877906944)`, engine 0), and
  rb1573 pairs it with `boost.spa 3/2`. Five games on the two "stats moved this turn" markers.
- **`.boost.def 0/2` + `hp` + `status Burn`** — rb1432, rb1650: PS grants +2 Def and NO burn where
  the engine burns. A Fire-move absorber (Well-Baked Body shape) on a mon the engine burns instead.
- **`.boost.atk` + `.boost.spa` both `0/2`** — rb1544, rb1681: a +2/+2 pair (Weakness Policy shape).
- **Mimikyu** — rb1421 (`engine=mimikyubusted ps=mimikyu`) and rb1621 (`engine=mimikyu
  ps=mimikyubusted`) point in OPPOSITE directions, so they are two roots, not one; do not merge them
  with the Cramorant fix, whose `isPermanent` argument is exactly what separates the two species.

**Triage verdict: of the 52, none is a re-run of a pinned open's diagnosed root** (all seven pinned
roots this tranche closed are gone from both corpora), **and at least 12 sit in four fresh
multi-game clusters.** They are NEW roots, and they are cheaper per game than the pinned tail.

## The 9 still-open pinned games — the evidenced table

`DBG_DIFF`'s FIRST divergent field per game, at the certifying commit (run the SIDECARS, not the
fixtures — a slim fixture has no `stateAfter` and can only report `state-digest`).

```
  rb1011 d43 t33  s0#3.hp 140/77
  rb1012 d60 t52  s0#2.hp
  rb1040 d2  t3   s0#0.hp
  rb1126 d7  t5   s1.volatiles bit28 UNBURDEN missing + s1#5.hp 396/275 + item Sitrus/None
                                                       + last_berry None/Sitrus
  rb1184 d5  t6   s1#4.hp
  rb1191 d17 t14  s0#1.hp 25/33          PS shuffle@thunderbolt vs rust randomChance@accuracy
  rb1236 d37 t29  s0#4.hp 51/18
  rb1314 d45 t38  s1#0.item LightClay/None    surfaces at a Revival Blessing
  rb1347 d69 t64  s1#1.hp
```

Six of the nine are a bare `hp` mismatch with no second field. rb1126's berry/Unburden difference is
a SYMPTOM of a 121-HP damage gap, not a root. **The PRNG-offset class is empty again** — rb1360 was
its last member and commit 1 closed it.

## Rules this tranche added, each of which cost a landing to learn

> **A cache is only refreshed where PS refreshes it, and `insertChoice` is the only site that
> refreshes ONE mon's.** A drag bypasses it entirely, so the incoming mon sorts on the value every
> BENCHED mon carries — its unboosted `storedStats.spe`, because `clearVolatile` ends in
> `setSpecies(baseSpecies)` which ends in `this.speed = this.storedStats.spe` (`sim/pokemon.ts:1419`).
> **Verified rather than assumed: 197714 benched snapshots across the 401 sidecars, ZERO with
> `speed !== storedStats.spe`.** When a rule can be checked against the recorded corpus, check it —
> it is one node script and it converts a guess into a fact.

> **`formeChange(species, effect)` vs `formeChange(species, effect, /*isPermanent*/ true)` is the
> entire difference between a forme that survives a switch and one that does not.** `isPermanent`
> rewrites `baseSpecies`, and `clearVolatile`'s `setSpecies(this.baseSpecies)` is what reverts.
> Mimikyu-Busted and Palafin-Hero pass `true`; Gulp Missile does not. Check the third argument
> before deciding a forme is permanent.

> **A hand-copied PS list is a liability — and the second copy of a computation is a list too.**
> The engine had THREE copies of "modified priority" and only the turn-order one carried Triage.
> Enumerate from the pin (`Dex.forGen(9).abilities.all().filter(a => a.onModifyPriority)`,
> `…filter(a => a.onFoeTryMove)`) and keep ONE function.

> **PS's serialized `baseTypes` is not the restore target.** It is frozen at construction from
> `baseSpecies.types` (`sim/pokemon.ts:446-447`), BEFORE `setSpecies` runs `ModifySpecies` — so a
> Rusted Shield Zamazenta serializes `["Fighting"]` while `clearVolatile` restores
> `["Fighting","Steel"]`. Seven games broke on trusting the field.

> **`apply_post_damage` runs BEFORE the deferred `apply_damaging_hit_step7`, and PS's order is the
> reverse.** Anything that reads state a faint clears (the Gulp Missile forme) must fire at the
> faint site, not wait for step 7. This is a latent ordering hazard for every future step-7 effect.

## Named opens carried forward

- The nine pinned games above, plus the 52 fresh ones.
- **`Volatiles` bits 38/39 (`StatsRaisedThisTurn` / `StatsLoweredThisTurn`) are the single largest
  fresh cluster (5 games)** and should be taken first in the next tranche.
- `apply_end_of_turn`'s **`switched` parameter is still vestigial**; delete when `request.rs` is
  next touched.
- Unchanged from XI: the engine's **Imposter** copies the target's BOOSTED Speed into `storedStats`;
  the **BeforeMove ladder's confusion / Attract / paralysis half** has no witness; **Rampage
  BeforeMove-cancel at `n == 1`**, **Terapagos-Stellar's FAINT regression**, **Battle Bond's
  once-per-stint guard**, **Magnet Rise's `onTry` failure**; the mover's own `onAfterMove` and the
  MOVE's own `onAfterMove` are deliberately not in the `AfterMove` list.
- **A repository hazard, not an engine one:** `engines` is a TRACKED symlink whose target is the
  main worktree's own `engines` path. Merging `prng-exact` into `main` checks that symlink out AT
  that path, turning it into a self-loop and destroying the gitignored PS clone underneath. It
  happened during this tranche; the clone was re-fetched at the pin
  (`git fetch --depth 1 origin b9dc987d…`). Either untrack the symlink or give the clone a
  different name.

## Recommendation for the next tranche

1. **Record the fresh batch's fixtures into the repo and gate on 912 games, not 512.** The evidence
   above says the marginal open game is cheaper in the fresh half, and four fresh clusters are
   already named.
2. **Take the `StatsRaised/LoweredThisTurn` cluster first** (5 games), then the `boost.def + Burn`
   pair and the `+2 Atk / +2 SpA` pair (2 each).
3. **The bare-`hp` tail is now 25 fresh + 6 pinned = 31 games and is the campaign's real asymptote.**
   Nothing localizes them but `DBG_INSTR` to the first diverging instruction and PS source at the
   pin, one modifier at a time.
4. `PRNG_TRACE` remains a one-command CONFIRMATION; **`DBG_INSTR` is the triage tool**, and
   `DBG_DIFF` must be run against a SIDECAR to name a field.

---

# ==== BURN-DOWN XI — certification (2026-07-27) ====

**HEADLINE: 497 / 512 full games byte-exact from seed (97.1%), up from 484; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Nine parity commits, every one PS-source-grounded, judged by the exact-SET diff on BOTH corpora
at every step: **the newly-non-exact set was EMPTY at all nine.** 13 games / 9 parity commits =
**1.44 games/commit**; per-commit yield 2, 2, 3, 1, 1, 1, 1, 1, 1 — **no commit flipped zero**,
so the revised early-stop line (<1 game/commit over three consecutive landings) was never
approached. The tranche stopped on its 10-commit budget, not on yield.

**This tranche spent the fixture-regeneration budget** (twice — see below) and used it to land
the two changes that were parked precisely for it: the STAB/Tera root and the Roost encoding
artifact. It also closed the whole named `struggle` cluster, which was three games.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim … seed-fixtures/*.fx.json.gz` | **497 / 512 = 97.1%**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, 0 unsupported |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** — 4832 / 4832 states, 3829 / 3829 move units |
| Transplant continuation | `node harness/transplant-gate.mjs` | **79 / 110 OK**, 17 diverge, 0 fail, 14 skip, 1812 decisions — the documented baseline, unmoved |
| Protocol log-parity | `PROTOCOL_EMIT=… cosim` then `node harness/protocol-parity.mjs` | 27 games, **508 semantic**, 4808 cosmetic (see the note below) |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |
| Cosim tests | `cargo test --release -p cosim -j 2` | 4 / 4 (two new round-trip fixtures) |

> **Protocol-parity number correction.** The scoreboard has carried **525 semantic** since
> burn-down VII. That number was measured against a `harness/protocol-logs/` directory that had
> gone stale (only 4 of its 108 files are tracked). Regenerating the logs at the PRE-tranche
> commit gives **511**, and at the certifying commit **508** — so 525 was never a live reading,
> and this tranche moved the true number by −3. Regenerate the logs before quoting it.

## The roots landed (in commit order)

| # | commit | root | games |
|---|--------|------|-------|
| 1 | `ee73a8c` | **PS's live `pokemon.types` becomes real engine state** — the STAB/Tera root and the Roost encoding artifact, together, with the fixture regeneration they needed | 484 → 486 (rb1119, rb1125) |
| 2 | `9644b0a` | **`Battle.boost()` refuses outright when the boosted mon's side has no living foes** (`gen > 5 && !target.side.foePokemonLeft()`, `sim/battle.ts:2028`) — `move.selfBoost` is the one engine site downstream of PS's `faintMessages` | 486 → 488 (rb1233, rb1310) |
| 3 | `7855252` | **Struggle is a REQUEST-time verdict, not a PP test** — `getMoves` returns `[]` when every slot is DISABLED, and the request turns that into Struggle. The whole named cluster | 488 → 491 (rb1024, rb1103, rb1231) |
| 4 | `71cb405` | **A berry brought into range by end-of-turn chip is eaten at the residual action's TRAILING Update**, not at the chip — the residual queue fires no `eachEvent('Update')` of its own | 491 → 492 (rb1030) |
| 5 | `89e46de` | **Encore's `failencore` flag list was 6 of PS's 18** — Sleep Talk is the one randbats hits | 492 → 493 (rb1387) |
| 6 | `b5aeb38` | **`twoturnmove` OUTLIVES the strike** — the volatile alone is not "still charging"; only the pair with its marker volatile is | 493 → 494 (rb1345) |
| 7 | `3ddaa2a` | Three independent singletons: a **dragged-out mon's queued action never runs**; the **Protect `stall` counter is not cleared by acting**; a **Chesto is EATEN** (records `lastItem`) | 494 → 495 (rb1239) |
| 8 | `cca90f4` | **Ice Face RESTORES the moment snow starts** (`onWeatherChange` / `onStart`), not on the next switch-in | 495 → 496 (rb1253) |
| 9 | `37742f1` | **The Disguise / Ice Face arms skipped `spreadMoveHit` step 4 (`selfDrops`)** and went straight to step 5 | 496 → 497 (rb1093) |
| 10 | this commit | docs | — |

## The structural change: `Pokemon::live_types` — PS's `pokemon.types`, verbatim

Standing blocker for three tranches, and the reason this tranche needed a regeneration budget.

The engine stored ONE type list (`types`) meaning "effective typing" — PS's `getTypes()`, with
Tera folded in and Roost's Flying stripped out. PS stores the RAW array and resolves both at
lookup time, and two of its lookups read the raw array directly. So the engine now stores both:

- **`types`** — unchanged meaning: the RESOLVED typing every damage/immunity site already reads.
- **`live_types`** — PS's `pokemon.types` verbatim. Tera does not touch it (`getTypes`
  short-circuits on `terastallized` before reading it) and neither does Roost (whose `onType`
  filters the RESULT, not the array). `base_types` stays the SPECIES typing (PS's `baseTypes`).

What that bought, in one batch:

1. **STAB (rb1125).** PS's `isSTAB` is
   `move.forceSTAB || pokemon.hasType(type) || pokemon.getTypes(false, true).includes(type)`
   (`sim/battle-actions.ts:1768`), and `getTypes(false, true)` returns `this.types` — the LIVE
   array. The engine fed `species_types(attacker.species)`, so a Meowscarada turned Poison by
   Protean and then Terastallized into Dark still got Grass STAB on Flower Trick, killed the
   Gastrodon PS left at 27/331, and swallowed PS's next accuracy roll.
2. **Roost typing (rb1119).** The digest and the state diff now compare `live_types`, which no
   encoding touches, so the artifact is gone BY CONSTRUCTION — the resolved value is a function
   of `live_types` + `terastallized`/`tera_type` + the `Roosted` marker, and the marker is
   already masked as a single-turn flag. `convert.rs` applies the Flying filter when it sees the
   volatile (so a state converted mid-Roost still simulates forward), and
   `restore_roost_typing_side` restores to `live_types` instead of the species types.
   (rb1119 turned out to be a TERA state mislabelled as a Roost one — the fix landed it anyway.)
3. **`export.rs:317-325`'s latent bug**, fixed: it recovered PS's array from `base_types` for a
   terastallized mon, which is right only when the typing was never changed. It writes
   `live_types` now. Two new round-trip unit tests cover the three-way split (effective / live /
   base) and the Roost window.

**Instruction model.** `ChangeTypes` keeps the EFFECTIVE typing and a new `ChangeLiveTypes`
carries the array, because the two move independently: an ENFORCED `setType` (`setSpecies`,
`transformInto`) rewrites the array under a terastallized mon whose effective typing does not
move, and Tera / Roost move the effective typing with the array standing still. `TransformData`
carries both for the same reason. Sites audited against PS: Protean/Libero and Double Shock push
both; Tera and Roost push only `ChangeTypes`; switch-out (`clearVolatile` ->
`setSpecies(baseSpecies)`) and faint/revive reset the array **even under Tera**; `transformInto`
copies the TARGET's `getTypes(true, true)` (its live array, `roost.typeWas` unfiltered) rather
than the target's resolved typing.

## The two fixture regenerations, and what moved

| batch | trigger | files touched | decision digests moved | non-digest bytes |
|---|---|---|---|---|
| commit 1 | `digest.rs` swapped `types` → `live_types`; `convert.rs` derives `types` | **397 / 401** | **11012 / 19381** | zero |
| commit 6 | `convert.rs` stopped reading a bare `twoturnmove` as `Charging` | **2 / 401** | **2 / 19381** | zero |

The first number looks alarming and is exactly right: from the moment a mon Terastallizes, every
later decision digests its PRE-tera array instead of `[tera]`, and 397 of 401 randbats games
contain at least one Tera. The second is the whole population of its change — the two mid-turn
boundaries in the corpus that fall between a charged strike and the end of its turn.

Regenerate with
`MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz`
and diff old-vs-new digest arrays before believing any count.

## Recurring shapes this tranche made explicit

> **A decision PS makes at REQUEST time cannot be re-derived mid-turn.** Struggle is the
> canonical case (commit 3): `getMoves` runs when the request is built, and PS's
> `onOverrideAction` only redirects an already-chosen action — it never re-consults `disabled`.
> Evaluating the predicate at execution time got rb1231 d14 wrong in the opposite direction
> before the flag was hoisted onto `Action`. The same shape is why a dragged-out mon's action
> dies (commit 7): PS's queue names a POKEMON, the engine's action names a SIDE.

> **`onUpdate` handlers fire ONLY at an `eachEvent('Update')`, and the residual queue contains
> none.** Every berry trigger is an `onUpdate`. The engine ran its pinch-berry check inline at
> residual order ~14; PS runs it at `runAction`'s trailing Update (`sim/battle.ts:2882`), after
> the whole queue — which is why Harvest (28/2) sees a hand the engine had already emptied
> (commit 4).

> **A PS "flag list" hand-copied into the engine is a liability.** Encore's `failencore` set was
> six of eighteen (commit 5). Enumerate them from the pin —
> `Dex.forGen(9).moves.all().filter(m => m.flags.X)` — and paste the whole thing with the
> command in the comment.

> **A block that returns `0` from `onDamage` keeps the target LIVE**, so `spreadMoveHit` runs the
> REST of its numbered steps — not just the secondaries the engine already knew about. Disguise
> and Ice Face both skipped step 4, `selfDrops` (commit 9). The step table is in
> `apply_self_drop`'s doc comment; step 4 precedes step 5.

## The 15 still-open games — the evidenced table

`DBG_DIFF`'s FIRST divergent field per game, at the certifying commit.

```
  rb1011 d43 t33  s0#3.hp 140/77
  rb1012 d60 t52  s0#2.hp 138/185
  rb1040 d2  t3   s0#0.hp 230/217
  rb1108 d4  t5   s0#2.hp 89/73 + status None/Burn
  rb1126 d7  t5   s1.volatiles bit28 UNBURDEN missing + s1#5.hp 396/275 + item Sitrus/None
  rb1184 d5  t6   s1#4.hp 196/142
  rb1191 d17 t14  s0#1.hp 25/33                        PS shuffle@thunderbolt vs rust accuracy
  rb1236 d37 t29  s0#4.hp 51/18
  rb1244 d10 t7   s1#4.ability Trace/WaterAbsorb       PS-unconsumed sample[1]@trace
  rb1314 d45 t38  s1#0.item LightClay/None             surfaces at a Revival Blessing
  rb1326 d50 t40  s1.substitute_hp 66/48 + s1#2.times_hit 2/1
  rb1347 d69 t64  s1#1.hp 245/259                      was d61; commit 7 advanced it 8
  rb1348 d12 t11  s0#1.hp 159/107 + s1.boost.def 1/0   rust-extra randomChance@accuracy
  rb1359 d7  t7   s0#0.types [Normal,Fire]/[Normal,None] + move0 None/transform
  rb1360 d7  t6   s0#0.hp 99/105                       was d6; commit 7 advanced it 1 — see below
```

**Volatile bit key** (discriminant = bit index, `volatile.rs`): 4 Encore, 28 Unburden,
38 StatsRaisedThisTurn, 39 StatsLoweredThisTurn.

**The PRNG-offset class is no longer empty — it has exactly ONE member, and it is diagnosed.**
`PRNG_TRACE` over all 15 (309 boundary lines) shows fourteen aligning step-for-step with PS's
cumulative advance count through their first divergence. The exception is **rb1360**, below.

### NEW named open, fully diagnosed and NOT fixed: the trailing 2882 Update after a DRAG

**rb1360 d6 t6.** Both sides pick Dragon Tail; p1's is faster and drags Dipplin out for
Empoleon. PS's draw stream for the unit is eleven draws and **ENDS at the drag `sample[4]`**;
the engine emits a twelfth, a `shuffle[2, 0, 2]@update`. The unit's DIGEST matches — this is a
pure draw-count bug — and `PRNG_TRACE` reports `engine=53 ps=52` at the d7 boundary, the corpus's
only remaining offset.

It is the burn-down-X lever in a new costume: **`pokemon.speed` is a cache**, and the move
action's trailing `eachEvent('Update')` (`sim/battle.ts:2882`) sorts on whatever the last
`updateSpeed()` saw — which for this action is the board BEFORE the drag, carrying **Dipplin's**
Speed against Hydrapple's (untied). The engine sorts the post-drag board, where **Empoleon** ties
Hydrapple, and draws a shuffle PS never draws. `MOVE_TIE_SPEEDS` is the existing hook; the
residual action already does exactly this with `pre_residual_speeds`. It wants a
`pre_move_speeds` capture around `run_move_action`, which is a broader change than a tranche's
last commit should carry.

### Clusters worth naming (by DIVERGENT FIELD, per trap #1 — never by move name)

There are **none**. Every one of the 15 is, as far as the evidence goes, a singleton: eight are
a bare `hp` disagreement with no second field, and the seven that carry a second field carry a
different one each (status, volatiles+item, ability, item, substitute_hp, boost.def, types+move).
The two-game Sitrus signature that looked like a cluster mid-tranche (rb1030 + rb1126) split:
rb1030 was the residual-Update ordering (commit 4) and rb1126's berry difference is downstream of
a 121-HP damage gap, i.e. a symptom, not the root.

## Named opens carried forward

- **The rb1360 drag-Update cache**, above — the single best-diagnosed remaining lever.
- `apply_end_of_turn`'s **`switched` parameter is still vestigial** (and still threads through
  `apply_end_of_turn_inner`). Delete both when `request.rs` is next touched.
- The engine's **Imposter** copies the target's BOOSTED Speed into `storedStats`; the **BeforeMove
  ladder's confusion / Attract / paralysis half** still has no witness; **Rampage BeforeMove-cancel
  at `n == 1`**, **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**,
  **Magnet Rise's `onTry` failure** — all unchanged.
- Deliberately NOT modelled in the `AfterMove` list, for want of a witness: the mover's own
  `onAfterMove` handlers and the MOVE's own `onAfterMove`.
- **Ice Face's `onStart` restore** and the **`selfDrops` fix in the Ice Face arm** both landed
  without a corpus witness (they are the same PS source as their witnessed twins). Neither moved
  a game; both are correctness.

## Asymptote assessment — the corpus is NOT mined out, but its shared structure is

The revised early-stop was "re-check after three landings; if the yield falls below 1 game per
commit, write the assessment". It never did — the nine parity commits went 2, 2, 3, 1, 1, 1, 1,
1, 1 and **not one of them flipped zero games**. The tranche stopped on budget. So the honest
reading is:

- **The last multi-game roots are gone.** Burn-down X said "every one of the 28 is now a
  singleton" and was wrong twice — the `struggle` cluster really was one root over three games,
  and the `selfBoost` refusal took two. This tranche's evidence table supports NO cluster at all:
  the 15 survivors share no divergent field pair, and the one signature that repeated split into
  two unrelated roots on inspection. Expect **1 game/commit** from here, and expect the work per
  game to be a full mechanic each time.
- **Eight of the fifteen are a bare `hp` mismatch with no second field**, which is the most
  expensive shape to localize: `DBG_INSTR` shows a damage number, and the root is somewhere in a
  ~40-term modifier chain. The seven with a second field (rb1108's Burn, rb1126's Unburden,
  rb1244's Trace, rb1314's Light Clay, rb1326's substitute_hp, rb1348's boost.def, rb1359's
  transform) are the cheap tail and should be taken first.
- **Would +400 fresh seeds regrow shared classes?** Probably yes, and this is the recommendation
  if the next tranche wants games/commit back above 1.5. The argument is empirical: every
  multi-game root this campaign has closed was found because two or more games happened to hit
  the same mechanic, and the rate at which that happens scales with corpus size, not with how
  long you stare at a fixed corpus. At 497/512 the current corpus yields ~15 open games; a fresh
  401-game batch at the same 97% exactness would yield ~12 more, drawn from a DIFFERENT sample of
  mechanics, and any mechanic that appears twice across the union becomes a cluster. The cost is
  ~2 minutes of recording (`bash harness/record-seeds.sh 1401 1800`) plus a fixture build.
  The counter-argument is that the marginal open game is now a rare mechanic BY CONSTRUCTION —
  the common ones are all modelled — so a fresh batch's opens will also be rare mechanics, and
  rare mechanics collide less often. Both effects are real; the empirical test is cheap enough
  that it should just be run.
- **The corpus has NOT reached its evidenced ceiling.** Fifteen games with fifteen concrete
  first-divergent fields, one of them (rb1360) diagnosed to the PS line, is not an exhausted
  frontier — it is a queue. The ceiling claim would need the opens to be unlocalizable or to
  require unmodellable state, and neither is true of any of them.

## Recommendation for the next tranche

1. **rb1360 first** — it is diagnosed to the line, it is the corpus's only PRNG offset, and the
   `pre_move_speeds` capture it needs is the same shape the residual action already has.
2. **Then the seven with a second field**, cheapest first: rb1359 (transform, and its `types`
   diff is now readable against `live_types`), rb1314 (an item that is wrong on a FAINTED mon and
   only surfaces at a Revival Blessing — walk it backwards from the revive), rb1244 (Trace),
   rb1348, rb1326, rb1108, rb1126.
3. **Then decide the fresh-seeds question with data, not argument**: record seeds 1401-1800,
   build fixtures, run the gate once. If the new batch's opens share ANY divergent-field
   signature with the current 15, the shared-class well is not dry.
4. `PRNG_TRACE` is a one-command confirmation, not a triage tool — with one offset left it will
   say "aligned" fourteen times out of fifteen. **`DBG_INSTR` remains the primary tool.**

---

# ==== BURN-DOWN X — certification (2026-07-27) ====

**HEADLINE: 484 / 512 full games byte-exact from seed (94.5%), up from 476; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

**THE RESULT THAT MATTERS MORE THAN THE COUNT: the PRNG-OFFSET class is EMPTY.** All six of
burn-down IX's localized offset games are closed, and re-running `PRNG_TRACE` over ALL 29 remaining
open games shows **every one of them aligns step-for-step with PS's cumulative advance count,
through and including the unit where its state first diverges** (753 boundary lines, zero delta
mismatches). Nothing left in the corpus misaligns the stream. Every open game is now a pure
MECHANICS bug — wrong state on a perfectly aligned stream — including the ten the gate still labels
with a draw-CLASS name (`args randomChance@struggle`, `PS-unconsumed random@icehammer`, …), which
are same-step-count disagreements, not miscounts. **Trap #8 ("a draw-CLASS label is not a root
label") is now the ONLY way to read the remaining labels.**

Six parity commits, every one PS-source-grounded, judged by the exact-SET diff on BOTH corpora at
every step: **the newly-non-exact set was EMPTY at all six.** 8 games / 6 parity commits = **1.33
games/commit**. Kill criterion NOT triggered — the longest run below 1 game/commit is ZERO
(per-commit yield 1, 2, 1, 1, 2, 1). `convert.rs` untouched, so **no fixture regeneration**.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim … seed-fixtures/*.fx.json.gz` | **484 / 512 = 94.5%**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3813 / 3831 = 99.53%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, 0 unsupported |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |

## The roots landed (in commit order)

| # | commit | root | games |
|---|--------|------|-------|
| 1 | `7923336` | **The post-residual `eachEvent('Update')` sorts on the PRE-residual cached Speed.** `runAction`'s `case 'residual'` calls `updateSpeed()` at its START (`sim/battle.ts:2835`) and that is the LAST refresh before the same action's trailing Update at `:2882`, so a Speed change the residual phase itself makes cannot break or create its tie. The switch bracket's cache rule, one event later. The rb1310 / rb1369 MIRROR PAIR, both Regigigas games where Slow Start's counter expires that turn | 476 → 477 (rb1369; rb1310 advances d36 → d46) |
| 2 | `dacc956` | **Shed Skin runs at residual order 5/3**, not in the branching tail after Harvest (28) — ten orders late, so a cured holder still took the order-9/10 status chip. See "The structural change" below | 477 → 479 (rb1315, rb1380) |
| 3 | `4147434` | **Good as Gold blocks a status move BEFORE the accuracy roll.** `onTryHit` fires in `hitStepTryHitEvent` (step 1) and `hitStepAccuracy` is step 4 (`sim/battle-actions.ts:551-563`), so the holder's target makes NO accuracy draw; the engine rolled one and blocked only the move's PAYLOAD. It returns `null`, not `false`, so `moveThisTurnResult` is null and `move_failed` must stay clear | 479 → 480 (rb1277) |
| 4 | `aba469e` | **A mon that faints mid-turn sorts its residual handlers on its UNBOOSTED Speed.** `faintMessages` runs `clearVolatile(false)` (`sim/battle.ts:2576`), zeroing boosts and volatiles, and the residual action's own `updateSpeed()` then recomputes the cache from that cleared board. rb1021: a Sticky-Webbed Magnezone is KO'd and its Leftovers handler ties the foe's at 151/151, not 100/151 | 480 → 481 (rb1021) |
| 5 | `60a1be2` | **A simultaneous both-sides replacement makes TWO draws the gate never consumed**, on two DIFFERENT Speed pairs. See "The new PRNG call site" below | 481 → 483 (rb1271, rb1329) |
| 6 | `ac6c9ff` | **Curing a status DISCARDS its counter.** `cureStatus()` goes through `setStatus('')`, which replaces `pokemon.statusState` wholesale (`sim/pokemon.ts`). The engine cured `status` at eight sites and left `status_counter` standing: a mon woken by the slp BeforeMove cancel (at `counter == 1`) carried a phantom 1, so a Toxic applied later started at stage 1 and its FIRST residual dealt 2·maxhp/16 instead of 1·maxhp/16. New `clear_status_counter` at all eight sites — a no-op for every status but `slp` and `tox`. THE two-game cluster below, landed | 483 → 484 (rb1300; rb1030 advances d53 → d67, 57/58 decisions) |
| 7 | this commit | docs | — |

## The structural change: Shed Skin's 33% split inside a single-`&mut Branch` residual core

Standing blocker for three tranches. `apply_end_of_turn`'s deterministic core is one `&mut Branch`
walk over orders 1..29 and Shed Skin is a SPLIT, so the core cannot branch mid-walk.

Resolved by **hoisting the split into a thin wrapper**: `apply_end_of_turn` enumerates the outcome
combinations up front (at most one holder per side → at most four), scales the branch probability
per combination, and runs the core once per combination with each holder's outcome FORCED. The core
takes `shed: [Option<bool>; 2]` and, at the order-5/3 slot, emits `randomChance(33,100)` with the
forced result and applies the cure; `None` means "no roll on that side". The site re-checks PS's own
short-circuit (`pokemon.hp && pokemon.status`) on the LIVE board, so a holder that faints to the
order-1 weather chip makes no draw and the two forced branches collapse to a harmless duplicate
whose probabilities still sum to the parent's.

The site iterates `residual_side_order` (speed order), because with two holders the residual handler
list is `speedSort`ed and the faster one rolls first. It sits between Wish (4) and the orders 5-7
loop; the ≤1-slot inversion against Grassy Terrain (5/2) is unobservable — no order-5 handler reads
or writes `status`, and none of them draws.

**This is the template for any future mid-residual split** (a second one would nest the same way).

## The new PRNG call site: `insertChoice`'s `random(firstIndex, lastIndex + 1)`

`sim/battle-queue.ts:395`. When an inserted action TIES (by `comparePriority`) with a run of queue
positions, `insertChoice` picks its slot with a bare `this.battle.random(from, to)` — **not** a
shuffle. This is the first time the engine models that call site anywhere.

A simultaneous both-sides forced replacement fires up to TWO draws before the replacement bracket,
on two different Speed pairs, and each witness fires exactly ONE of them — which is what proves they
are two draws and not one:

| draw | PS site | ties on | witness |
|---|---|---|---|
| `shuffle[2,0,2]` | `commitChoices`' `queue.sort()` over the two `instaswitch` actions (order 3) | the OUTGOING, just-fainted mons' `getActionSpeed()` — post-`clearVolatile`, so unboosted | **rb1271 d10 t8** — the unit's ONLY PS draw; incoming Torkoal 85 / Iron Bundle 257 untied |
| `random[0,2]` | `switchIn`'s `queue.insertChoice({runSwitch})` | the INCOMING mons' `switch_entry_speed` (= `replacement_bracket_tied`) | **rb1329 d23 t16** — outgoing Squawkabilly 205 / Qwilfish 189 untied |

Why the second one fires at all: `instaswitch` is order 3 and `runSwitch` is 101, so the FIRST
replacement's `runSwitch` sorts BEHIND the second side's still-pending `instaswitch` and is still in
the queue when the second replacement inserts its own. The two `runSwitch` actions share order and
priority → `comparePriority` returns 0 on equal Speed → `firstIndex !== lastIndex` → one draw.

**Corpus census.** `random` draws with args `[0,2]` appear exactly TWICE in all 401 sidecars:
rb1329 d23 and rb1368 d0. rb1368 d0 is the battle-start `case 'start'` action, where BOTH leads'
`switchIn` run back-to-back inside ONE action and hit the same `insertChoice` tie — the same rule,
already accounted for on that path.

## The recurring shape this tranche made explicit

Three of the six roots are the SAME sentence in different clothes, and it is worth stating once:

> **`pokemon.speed` is a cache, and every speed-tie predicate reads whatever board the last
> `updateSpeed()` saw — never the live board.**

Burn-down IX found it at `insertChoice` (the switch bracket). This tranche found it at the residual
action's start (commit 1) and at `faintMessages`' `clearVolatile` (commit 4). The remaining
`updateSpeed()` sites — `commitChoices` and "before each move action" — are already modelled.
**Whenever a tie disagrees, ask which `updateSpeed()` was last, not what the Speed is now.**

A second recurring shape, now three tranches deep:

> **An immunity the engine models as an EFFECT gate is often a HIT STEP in PS.**
> Queenly Majesty / Psychic Terrain (burn-down VIII), Shield Dust / Covert Cloak (IX), Good as Gold
> (this one). The tell is always the same: the engine rolls a draw PS never makes.

## The 28 still-open games — the evidenced table, all stream-clean

Every one of these has an ALIGNED PRNG stream at its first divergence (verified by `PRNG_TRACE`
this tranche, over the 29 open at the time; commit 6 then closed rb1300). The field column is the
FIRST divergent field from `DBG_DIFF`.

```
  rb1011 d43 t33  s0#3.hp 140/77
  rb1012 d60 t52  s0#2.hp 138/185
  rb1024 d81 t73  s0#3.hp 308/250 + move0.pp 3/4 + s1#1.hp 0/121    STRUGGLE / request legality
  rb1030 d67 t59  s0#0.item Sitrus/None + last_berry None/Sitrus   was d53; commit 6 advanced it 14
  rb1040 d2  t3   s0#0.hp 230/217
  rb1093 d22 t17  s0.boost.spe -2/-3
  rb1103 d37 t32  s0#0.hp 222/136 + times_hit 5/6                   STRUGGLE / request legality
  rb1108 d4  t5   s0#2.hp 89/73 + status None/Burn
  rb1119 d8  t7   s1#4.types [Fire,None]/[Fairy,None]               Roost ENCODING artifact
  rb1125 d2  t3   s0#0.hp 0/27 + s1#0.times_hit 1/2                STAB / Tera — see the named open
  rb1126 d7  t5   s1.volatiles bit28 UNBURDEN missing + s1#5.hp 396/275
  rb1184 d5  t6   s1#4.hp 196/142
  rb1191 d17 t14  s0#1.hp 25/33
  rb1231 d15 t12  s0#3.hp 274/208 + move1.pp 5/6                    STRUGGLE / request legality
  rb1233 d39 t32  s0.boost.def -2/-1 + bit39 extra                  Clanging Scales self Def -1
  rb1236 d37 t29  s0#4.hp 51/18
  rb1239 d64 t51  s1.stall_counter 0/1
  rb1244 d10 t7   s1#4.ability Trace/WaterAbsorb
  rb1253 d12 t10  s1#2.species 222/221                              Ice Face RESTORE
  rb1310 d46 t37  s1.boost.def -1/0, spe 1/0 + bits38,39 extra      engine's mon did not leave
  rb1314 d45 t38  s1#0.item LightClay/None
  rb1326 d50 t40  s1.substitute_hp 66/48 + s1#2.times_hit 2/1
  rb1345 d42 t32  s1.pending_move None/Charging(meteorbeam)
  rb1347 d61 t56  s1#1.last_berry None/ChestoBerry
  rb1348 d12 t11  s0#1.hp 159/107 + s1.boost.def 1/0
  rb1359 d7  t7   s0#0.types [Normal,Fire]/[Ghost,None] + move0 None/transform
  rb1360 d6  t6   s1#2.move3.pp 7/8
  rb1387 d36 t32  s0#3.hp 337/218 + times_hit 2/3 + bit4 ENCORE extra   spurious Encore
```

**Volatile bit key** (discriminant = bit index, `volatile.rs`): 4 Encore, 28 Unburden,
38 StatsRaisedThisTurn, 39 StatsLoweredThisTurn.

### Clusters worth naming (by DIVERGENT FIELD, per trap #1 — never by move name)

- **`status_counter` 2 vs 1 — rb1030, rb1300 — LANDED this tranche (commit 6).** The reasoning is
  worth keeping because it is the method: the engine's chip was exactly one extra toxic stage in
  both (rb1030 −17 with maxhp/16 = 17; rb1300 −22 with maxhp/16 = 22), and the two toxics arrived by
  two DIFFERENT application paths (the move Toxic; Toxic Chain's `DamagingHit` secondary). Two paths
  with one symptom points at neither path — it points at the shared state they both read. It was a
  stale counter a previous CURE had left behind (rb1030's Indeedee slept at t30 and woke at t36),
  and the fix went at the cure sites, not the application sites.
- **STRUGGLE / request legality — rb1024, rb1103, rb1231** (unchanged, and now with direct
  evidence): in all three the engine's PP is exactly one LOWER than PS's while PS's draw is a
  Struggle crit roll. The engine let the mon use a real move where PS forced Struggle. rb1024 is
  still the largest single gap in the corpus (58 HP at t73).
- **`times_hit` one lower in the engine + a `PS-unconsumed` accuracy roll — rb1125, rb1387.** PS
  makes an accuracy roll for a move the engine never runs. **These turned out to be TWO different
  roots, not one** (trap #1 again, in a new costume — a shared draw-CLASS shape is no more a root
  than a shared move name). rb1125 is the STAB bug below, fully diagnosed. rb1387 comes with a
  spurious **Encore** the engine still holds (bit 4) and PS does not — i.e. the engine's mover is
  locked out of the move PS used; check the Encore duration tick before anything else.
- **Roost typing (rb1119, rb1359) is still an ENCODING artifact** and still deliberately not fixed
  — the `convert.rs` change would cost a full fixture regeneration for zero measured gain.

## NEW named open, fully diagnosed: STAB reads the SPECIES types, PS reads the LIVE pre-tera types

**This one is diagnosed to the line on both sides and deliberately NOT fixed — it needs a new state
field and a `convert.rs` change, i.e. a full fixture regeneration.** Do not start it without that
budget.

PS (`sim/battle-actions.ts:1768`):

```text
const isSTAB = move.forceSTAB || pokemon.hasType(type) || pokemon.getTypes(false, true).includes(type);
```

`getTypes(false, true)` is `preterastallized = true`, and it returns **`this.types`** — the mon's
LIVE type list, which a Soak / Forest's Curse / Burn Up / Reflect Type / Conversion has already
rewritten. It is **not** `this.baseTypes` (the species types). So a mon whose typing was changed and
which then Terastallizes gets STAB on its CHANGED types, never on its species' original ones.

The engine feeds `attacker_base_types: crate::data::species_types(attacker.species)`
(`generate.rs:6328`) — the species table. `damage.rs:206` then reads it for exactly PS's
`getTypes(false, true)` branch. (Note `generate.rs:10893` already passes `caster.base_types` for the
same field — the two call sites disagree, which is how the outlier was spotted.)

**Witness rb1125 d2 t3.** p2's Meowscarada is `types: ['Poison']` / `baseTypes: ['Grass','Dark']` in
the sidecar at the end of turn 1, then Terastallizes into **Dark** and uses **Flower Trick** (Grass,
`accuracy: true`, always-crit — PS's only draw for it is `random[16]`). PS: `hasType('Grass')` is
false (post-tera `getTypes()` is `['Dark']`) and `getTypes(false, true)` is `['Poison']`, which does
not include Grass → **isSTAB false, stab 1.0** → 304 damage, Gastrodon survives at 27/331. The
engine read the species types `[Grass, Dark]`, found Grass, applied **1.5x**, and killed it (damage
clamped to 331). p1's Gastrodon is then dead and never uses Ice Beam — which is the whole visible
symptom, `PS-unconsumed randomChance[100,100]@icebeam`.

**Why it is not a one-liner.** Swapping `species_types(attacker.species)` for
`attacker.base_types` changes nothing: both are the species types. The engine models Tera by
REWRITING `p.types` to `[tera_type, None]` and keeps `p.base_types` from PS's `baseTypes`
(`convert.rs:279-282`), so the pre-tera LIVE types are simply not retained anywhere. The fix is a
new `Pokemon` field (pre-tera live types), written at the Tera rewrite, read by `damage.rs`, plus
`convert.rs` / `export.rs` support.

**`export.rs:317-325` already assumes the field exists** — it recovers PS's raw `types` from
`p.base_types` for a terastallized mon, which is correct only when the mon's typing was never
changed. That is a latent exporter bug on the same state; the round-trip gate passes because no
audited corpus state hits it. Fix both together.

## Named opens carried forward (unchanged unless noted)

- **`apply_end_of_turn`'s `switched` parameter is still vestigial** — and it now also threads
  through the new `apply_end_of_turn_inner`. Delete both when `request.rs` is next touched.
- The engine's **Imposter** copies the target's BOOSTED Speed into `storedStats`; the **BeforeMove
  ladder's confusion / Attract / paralysis half** still has no witness; **Ice Face's RESTORE**,
  **Rampage BeforeMove-cancel at `n == 1`**, **Terapagos-Stellar's FAINT regression**, **Battle
  Bond's once-per-stint guard**, **Magnet Rise's `onTry` failure** — all unchanged.
- Deliberately NOT modelled in the `AfterMove` list, for want of a witness: the mover's own
  `onAfterMove` handlers and the MOVE's own `onAfterMove`.

## Recommendation for the next tranche

The corpus is NOT mined out, but its character has changed. There are no stream bugs left, so
`PRNG_TRACE` has done its job and the next tranche's primary tool is **`DBG_INSTR`** (with
`DBG_GAME`/`DBG_I`) — the only thing that localizes a `draws-match/state-diff` unit — plus the
`DBG_DIFF` field table above. The two-game `status_counter` cluster was the last multi-game root the
evidence supported and commit 6 closed it; **every one of the 28 is now, as far as the evidence
goes, a SINGLETON.** Expect ~1 game/commit from here, and re-check that estimate against the kill
criterion after three landings.

---

# ==== BURN-DOWN IX — certification (2026-07-27) ====

**HEADLINE: 476 / 512 full games byte-exact from seed (93.0%), up from 466; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Ten parity commits, every one PS-source-grounded. Judged by the exact-SET diff on BOTH corpora at
every step: **the newly-non-exact set was EMPTY at all ten.** 10 games / 10 commits = **1.0
games/commit**. The early-stop line (<1 game/commit across 3 CONSECUTIVE commits) was approached
but never reached — commits 7 and 8 flipped 0 games each, commits 9 and 10 flipped 1 each. The
tranche stopped on its 10-commit budget, so no asymptote assessment is due. See "Kill criterion"
below for the evidence.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **476 / 512 = 93.0%**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |

`convert.rs` was NOT touched, so the seed fixtures' baked digests did not move and **no fixture
regeneration was needed**. `gen.rs` was NOT regenerated. Nine of the ten commits are
`crates/engine/src/generate.rs`-only; `40fe02c` also touches `crates/cosim/src/seedgate.rs` (the
two new triage tools plus the replacement-bracket accounting) and `b73ef76`/`6139501` adjust the
gate's call into `replacement_bracket_tied`.

## THE TOOL: `PRNG_TRACE` — the thing that made this tranche

> `PRNG_TRACE=<game-prefix>` prints, at EVERY unit boundary, the engine's absolute position in the
> PRNG stream (steps replayed from the seed) against PS's cumulative recorded advance count. The
> FIRST unit where the two per-unit deltas differ is the unit that MISALIGNED the stream.

A `result random[16]@…` first-divergence label names the unit that *reads* a misaligned stream, not
the one that misaligned it — the scoreboard has carried that caveat for three tranches without a
way to act on it. `PRNG_TRACE` closes the gap in one pass: it turned **eight** "offset" labels into
eight concrete units, and those units then collapsed into **three shared roots** (the switch
bracket's cached Speed, the pivot bracket, the secondary-roll gates). It also proved that a NINTH
game — rb1362, carried as a diagnosed `replicate_select` bug — was an offset game too, and that
`replicate_select` was never wrong.

The companion `DBG_SELECT=1` dumps `replicate_select`'s per-position candidate count, the distinct
draw shapes still live, and the realized value. Both are env-gated and inert by default.

**Run `PRNG_TRACE` FIRST next tranche.** The recipe:

```
GATE_THREADS=1 PRNG_TRACE=rb SEED_GATE=1 target/release/cosim harness/seed-sidecars/<open>.json.gz \
  2> pt.txt >/dev/null
# then, per game, the first unit whose engine-delta != ps-delta
```

## THE LEVER: `pokemon.speed` is a CACHE, and the switch bracket reads it before the switch runs

Four of the ten commits are one rule, found because the sidecars record PS's `pokemon.speed`
**verbatim** — a fact nobody had used before. It is ground truth for every speed-tie predicate.

`switchIn` (`sim/battle-actions.ts:135-155`) does, in this order: swap the slot, reset
`abilityState`/`itemState` with `initEffectState`, `runEvent('BeforeSwitchIn')`, then
`queue.insertChoice({choice: 'runSwitch', pokemon})` — and `insertChoice` calls
`choice.pokemon.updateSpeed()` (`sim/battle-queue.ts:373-375`). That is the LAST `updateSpeed`
before the bracket runs, and it lands **before `runSwitch`**, i.e. before entry hazards and before
every switch-in ability's `onStart`. All three bracket shuffles sort on that value.

So the incoming mon's bracket Speed excludes:

| excluded | PS mechanism | witness |
|---|---|---|
| entry-hazard Speed −1 (Sticky Web) | hazards fire in `runSwitch`'s `fieldEvent('SwitchIn')` | c3c2s82 d49 (324 vs live 216); **rb1021 d58** — sidecar records cached **151**, live 100, and 151 ties the foe Sylveon exactly |
| Slow Start's ×0.5 | `onStart` sets `effectState.counter`, which `onModifySpe` reads; `switchIn` just cleared `abilityState` | **rb1369 d44** (PS ties, engine did not) and **rb1310 d11** (the mirror — the FOE carries Slow Start, PS is untied, the engine tied) |
| Protosynthesis / Quark Drive Speed boost | both `onStart` | — |
| an Imposter transform | `onStart` | **rb1057 d24** — a Ditto replaces into a +1 Venomoth; PS caches Ditto's own Speed |
| the weather the switch-in's OWN ability sets | `setWeather` runs inside `runSwitch` | — |

NOT excluded: paralysis, Choice Scarf, Tailwind, and the weather/terrain Speed abilities
(Chlorophyll / Swift Swim / Sand Rush / Slush Rush / Surge Surfer) — none has a state gate.

The model landed as `switch_entry_speed(pre, side, slot)`: evaluate the incoming party mon on the
**pre-switch board**, in the slot, with boosts / volatiles / `active_turns` cleared. It is installed
for the bracket through the existing `MOVE_TIE_SPEEDS` cache and is now the ONE definition behind
the turn-action switch bracket, the double-switch bracket, the pivot bracket and the gate's
`replacement_bracket_tied` — four call sites that had been modelling one leg of it each.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `40fe02c` | **A switch-in ability that CHANGES weather/terrain fires one extra `eachEvent` speedSort inside the forced-replacement bracket.** `Field.setWeather`/`setTerrain` each END with `eachEvent('WeatherChange'/'TerrainChange')`. rb1362 d21: a fainted p1 is replaced by a Drizzle Politoed on a tied board — PS records FOUR `shuffle[2,0,2]`, the third tagged `drizzle`/`SwitchIn`; the engine consumed three. **This is the game the standing note called a `replicate_select` `random(100)` decode bug — it is not; the selector is correct and the stream feeding it was off by one.** | 466 → 467 | `sim/field.ts:87` `:155`; `sim/battle-actions.ts:175-190` |
| 2 | `bfdac1c` | **The switch bracket sorts on the Speed cached at `insertChoice`** (see the lever above). Also closes the named open "rb1391 d20's pre-swap Speed tie is evaluated differently by the two" | 467 → 468 (rb1391) | `sim/battle-queue.ts:373-375`; `sim/battle-actions.ts:135-155`; `sim/pokemon.ts:866` |
| 3 | `b73ef76` | **…and that cache is the PRE-switch mon's**, not the post-entry one. rb1057 d24's Imposter Ditto | 468 → 469 | same |
| 4 | `6139501` | **A PIVOT's mid-turn switch fires the full 3-shuffle bracket.** `runAction` (`sim/battle.ts:2897-2932`) turns a `switchFlag` into a `switch` REQUEST whose choice resolves exactly like a turn-action switch. It does NOT fire the pre-swap switch-out Update — that block runs `BeforeSwitchOut` itself and sets `skipBeforeSwitchOutEventFlag`, which gates `switchIn`'s `eachEvent('Update')` at `:80-84`. rb1029 d18: a U-turn to a 195-Speed Cramorant vs a 195-Speed Meganium | 469 → 471 (rb1029, rb1121) | `sim/battle.ts:2897-2932`; `sim/battle-actions.ts:80-84` |
| 5 | `01a1058` | **Sand Stream is NOT a sandstorm immunity.** The only `onImmunity` abilities are Overcoat / Sand Force / Sand Rush / Sand Veil, plus Magic Guard separately — so the Tyranitar that PUT the sand up is chipped by it, and so is a Trace user that copied the ability. rb1116 d7 (Tera Ghost Tyranitar: Leftovers +17 then sand −17, netting 251 — the engine healed to 268; `getTypes()` returns `[teraType]`, so the tera also removes the Rock immunity), rb1283 d17 (a Gardevoir that Traced Sand Stream) | 471 → 473 | `data/conditions.ts:659-661`; `sim/battle.ts:2113`; `data/abilities.ts:3064` `:3921` `:3935` `:3962` |
| 6 | `7c23423` | **A switch clears `moveLastTurnResult`** — it is a PER-MON field that `clearVolatile` wipes on switch-out, and Stomping Tantrum doubles only on an explicit `false`. The engine kept one flag per SIDE, written only from a move action. rb1243 d11: a Walking Wake misses Hydro Pump on t8, Amoonguss comes in on t9 and Stomping Tantrums on t10 — PS 64, engine 126 | 473 → 474 | `sim/pokemon.ts:1546-1547`; `sim/battle.ts:1671-1672` |
| 7 | `b34927d` | **Shed Skin (5/3) and `lockedmove` (false/2) are residual handlers PS collects.** Both corpus-neutral, by census — see "Census" below | 474 → 474 | `data/abilities.ts:4142-4151`; `data/conditions.ts:253-262` |
| 8 | `7e24f59` | **The `runEvent('AfterMove')` tie shuffle** — the last `** NOT MODELLED **` row in burn-down VIII's census. White Herb / Eject Pack / Mirror Herb / Opportunist all register `onAnyAfterMove`, collected from EVERY active, order `false`, Item subOrder 8 / Ability 7. rb1345 goes 7/32 → 31/32 decisions | 474 → 474 | `sim/battle-actions.ts:312`; `data/items.ts:1729` `:4176` `:7694`; `data/abilities.ts:3024` |
| 9 | `4c192bb` | **A Substitute hit still rolls a closure-payload secondary.** A sub hit records `damage[i] === true`, and the target filter is `if (!damage[i] && damage[i] !== 0)` — truthy, so the target survives into step 5 and `secondaries()` rolls. `emit_sub_secondary_rolls` decided from `md.secondary_chance`, which is blind to an `onHit`-closure secondary (Tri Attack, Dire Claw). rb1033 d42 | 474 → 475 | `sim/battle-actions.ts:1108-1110` `:1364` |
| 10 | `3289506` | **Shield Dust / Covert Cloak still strip a secondary from a target that JUST fainted.** PS gates an ability on `ignoringAbility()`, whose only liveness test is `!this.isActive` — and `isActive` is cleared in `faintMessages`, which runs at the END of the action. rb1343 d34: Flamethrower KOs a 22-HP Shield Dust Ribombee and PS makes no roll at all; the engine rolled the 10% burn | 475 → 476 | `sim/pokemon.ts:866`; `sim/battle.ts:2579` |

Games flipped, by commit: 1 → rb1362; 2 → rb1391; 3 → rb1057; 4 → rb1029 rb1121; 5 → rb1116
rb1283; 6 → rb1243; 7 → none; 8 → none (rb1345 advanced 24 decisions); 9 → rb1033; 10 → rb1343.

Named opens CLOSED outright: **the `replicate_select` `random(100)` decode "bug"** (it was never a
bug — commit 1), **rb1391's pre-swap Speed-tie disagreement** (commit 2), **the
`AfterMove | whiteherb~whiteherb` census row** (commit 8), and **`lockedmove` as a residual
handler** (commit 7 — settled by census, not by refactor).

## Census — the standing worklist, re-measured

**Shuffle-signature census (401 sidecars), `(eventid | tied-handler group)` → count / games.**
**Every signature the corpus contains is now modelled.**

```
   792  48  Update        | MON~MON                 modelled (eachEvent Update)
   267  64  -             | MON~MON                 modelled (commitChoices queue.sort)
   263 102  Residual      | stall~protect           modelled
   112  42  BeforeTurn    | MON~MON                 modelled
    99  53  Residual      | protect~stall           modelled
    31   4  ModifyDamage  | lightscreen~reflect     modelled
    12   2  Residual      | slowstart~slowstart     modelled   rb1310 rb1369
     8   5  DisableMove   | choicelock~encore       modelled
     5   2  ModifyDamage  | reflect~lightscreen     modelled
     5   1  Residual      | grassyterrain~…         modelled   rb1360
     4   4  DisableMove   | choicelock~healblock    modelled
     4   2  Residual      | leftovers~leftovers     modelled   rb1021 rb1141
     4   3  Weather       | MON~MON                 modelled
     3   3  DisableMove   | healblock~choicelock    modelled
     3   3  DisableMove   | taunt~choicelock        modelled
     3   1  DisableMove   | choicelock~disable      modelled   rb1103
     3   3  WeatherChange | MON~MON                 modelled   rb1195 rb1250 rb1362
     2   2  Residual      | flinch~stall            modelled
     2   2  DisableMove   | choicelock~taunt        modelled
     2   2  DisableMove   | choicelock~throatchop   modelled
     2   1  AfterMove     | whiteherb~whiteherb     ** FIXED this tranche (commit 8) **
     1   1  DisableMove   | disable~taunt           modelled
     1   1  DisableMove   | disable~healblock       modelled
     1   1  DisableMove   | choicelock~encore~healblock  modelled
     1   1  TerrainChange | MON~MON                 modelled   rb1099
     1   1  Residual      | whiteherb~whiteherb     modelled
```

**Residual handler-LIST census.** Of the 37 distinct `(effect, effectType, order, subOrder)` triples
the corpus's `full` lists contain, the engine now models all 37. `shedskin` and `lockedmove` appear
in NONE of them — no recorded residual shuffle has ever fired with a Shed Skin holder or a live
rampage on the field — which is simultaneously why they had no witness and why adding them was
provably free (both gates and the differ byte-identical across commit 7).

**AfterMove handler census.** The only rows the whole corpus records are four `whiteherb/Item/false/8`
entries, all in rb1345. Nothing else lengthens that list anywhere in 401 games.

## The 36 still-open games, re-triaged at the certifying commit

First-divergence CLASS split (from the 512 gate):

| n | class |
|---|-------|
| 20 | `draws-match/state-diff` |
| 5 | `result random[16]@…` — a draw miscount in an EARLIER unit |
| 2 | `args randomChance@struggle` |
| 1 each | `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer` / `randomChance@freezedry` / `randomChance@icebeam` / `randomChance@sleeptalk` / `sample@trace`, `args shuffle@generic`, `rust-extra randomChance@accuracy` |

Every open game, its first divergent unit, and the first divergent field:

```
  rb1011 d43 t33  s0#3.hp: engine=140 ps=77
  rb1012 d60 t52  s0#2.hp: engine=138 ps=185
  rb1021 d104 t93 s0#0.hp: engine=61 ps=58            OFFSET, localized to d102 t91 (-1)
  rb1024 d81 t73  s0#3.hp: engine=308 ps=250          struggle / request legality
  rb1030 d53 t46  s1#5.hp: engine=61 ps=78
  rb1040 d2 t3    s0#0.hp: engine=230 ps=217
  rb1093 d22 t17  s0.boost.spe: engine=-2 ps=-3
  rb1103 d37 t32  s0#0.hp: engine=222 ps=136          struggle / request legality
  rb1108 d4 t5    s0#2.hp: engine=89 ps=73
  rb1119 d8 t7    s1#4.types: engine=[Fire,None] ps=[Fairy,None]
  rb1125 d2 t3    s0#0.hp: engine=0 ps=27
  rb1126 d7 t5    s1.volatiles: engine=0 ps=2^28      (Unburden MISSING)
  rb1184 d5 t6    s1#4.hp: engine=196 ps=142
  rb1191 d17 t14  s0#1.hp: engine=25 ps=33
  rb1231 d15 t12  s0#3.hp: engine=274 ps=208          struggle / request legality
  rb1233 d39 t32  s0.boost.def: engine=-2 ps=-1       Clanging Scales self Def -1
  rb1236 d37 t29  s0#4.hp: engine=51 ps=18
  rb1239 d64 t51  s1.stall_counter: engine=0 ps=1
  rb1244 d10 t7   s1#4.ability: engine=Trace ps=WaterAbsorb
  rb1253 d12 t10  s1#2.species: engine=222 ps=221     Ice Face RESTORE
  rb1271 d11 t9   s0#3.hp: engine=225 ps=221          OFFSET, localized to d9 t7 (-1)
  rb1277 d12 t9   s0#5.hp: engine=293 ps=290          OFFSET, localized to d9 t8 (+1)
  rb1300 d52 t48  s0#1.hp: engine=152 ps=174
  rb1310 d36 t29  s0#5.hp: engine=322 ps=280          OFFSET, localized to d35 t28 (-1)
  rb1314 d45 t38  s1#0.item: engine=LightClay ps=None
  rb1315 d28 t26  s0#2.hp: engine=189 ps=205          Shed Skin residual ORDER (see below)
  rb1326 d50 t40  s1.substitute_hp: engine=66 ps=48
  rb1329 d24 t17  s0#5.hp: engine=112 ps=120          OFFSET, localized to d22 t15 (-1)
  rb1345 d42 t32  s1.pending_move: engine=None ps=Charging(534)
  rb1347 d61 t56  s1#1.last_berry: engine=None ps=ChestoBerry
  rb1348 d12 t11  s0#1.hp: engine=159 ps=107
  rb1359 d7 t7    s0#0.types: engine=[Normal,Fire] ps=[Ghost,None]
  rb1360 d6 t6    s1#2.move3.pp: engine=7 ps=8
  rb1369 d51 t47  s1#2.hp: engine=232 ps=234          OFFSET, localized to d49 t45 (+1)
  rb1380 d15 t15  s0#1.hp: engine=157 ps=173          Shed Skin residual ORDER
  rb1387 d36 t32  s0#3.hp: engine=337 ps=218
```

**The `knockoff` cluster is GONE.** It was five games (rb1116 rb1243 rb1283 rb1315 rb1369) and it
had no shared Knock Off root at all — it was two sandstorm-immunity games, one Stomping Tantrum
game, one Shed Skin ordering game and one stream offset. Four of the five are fixed; rb1315 is the
Shed Skin one. **The lesson is the same one the scoreboard has recorded twice before in different
words: a move-name recurrence is not a root, it is a coincidence of which move happened to be
holding the stream when the real bug landed.** Current recurrences, for what little they are worth:
`struggle` 3x (rb1024 rb1103 rb1231 — one known root), `icebeam` 2x, `switch` as one half 5x.

## The six remaining OFFSET games, localized (this is the ready-made next queue)

`PRNG_TRACE` gives each one an exact unit and a signed delta. All six are ±1 and four of the six
are the same SHAPE — one trailing `shuffle[2,0,2]` at the end of a unit that the two disagree about:

```
  rb1021  unit d102 t91  engine=3  ps=4   -1   PS has one extra trailing shuffle after hypervoice
  rb1271  unit d9   t7   engine=15 ps=16  -1   PS has one extra trailing shuffle at the unit's end
  rb1277  unit d9   t8   engine=1  ps=0   +1   engine rolls an accuracy PS does not roll at all
  rb1310  unit d35  t28  engine=10 ps=11  -1   PS has one extra shuffle after the residual sort
  rb1329  unit d22  t15  engine=6  ps=7   -1   PS records a bare `random[0,2]@generic` the engine lacks
  rb1369  unit d49  t45  engine=2  ps=1   +1   engine emits a trailing `@update` shuffle PS does not
```

The −1/+1 mirror pair rb1310 / rb1369 is the strongest signal: both are Regigigas games, both turn
on the post-residual `eachEvent('Update')`, and `runAction`'s `case 'residual'` calls
`this.updateSpeed()` at its START (`sim/battle.ts:2835`) — so **that trailing Update sorts on the
Speed cached BEFORE the residual ran**, and a Speed change the residual itself makes (Slow Start's
counter hitting 0 at order 28) must not break its tie. That is the same cache rule as the switch
bracket, one event later, and it is the first thing to try next tranche.

## Named opens carried forward

- **Shed Skin's residual ORDER is 5/3, and the engine still runs it in the branching tail** (after
  Harvest 28/2). PS cures at order 5, BEFORE the psn/tox chip at order 9, so a Shed Skin holder
  cured this turn takes NO status damage. rb1315 d28 is exact on that one instruction: PS 205,
  engine 189 = 205 − 258/16. rb1380 is the second witness. **The handler LIST entry landed this
  tranche (commit 7); only the execution position is left.** The blocker is unchanged and now
  precisely scoped: `apply_end_of_turn`'s deterministic core is a single `&mut Branch` loop running
  orders 1..29 per side, and Shed Skin is a 33% SPLIT. Moving it needs the residual's core to
  branch mid-loop. The Hydration block (order 5/3, same slot, no draw) is the exact template for
  where it goes.
- **The `struggle` cluster (rb1024, rb1103, rb1231) is REQUEST LEGALITY**, unchanged. rb1024 is
  still the largest single gap in the corpus (58 HP at t73).
- **The engine's Imposter copies the target's BOOSTED Speed into `storedStats` and then re-applies
  the boost** — a +1 199-base Venomoth is copied as live 447 (= 199 × 1.5 × 1.5). Found while
  fixing rb1057; invisible there because the bracket zeroes boosts. No first-divergence witness yet.
- **The BeforeMove ladder's other half is still OPEN and still has NO witness.** confusion (3),
  Attract (2) and paralysis (1) are resolved in `execute_move`'s outer chain, ahead of the order-5..7
  cancels in `execute_move_inner`. It needs a lock applied THIS TURN by a faster foe landing on a
  confused / attracted / paralysed mover. Directed trace or leave it.
- **Roost typing is an ENCODING artifact** — unchanged, and again deliberately not fixed: rb1119 and
  rb1359 are real type CHANGES, not a stripped Flying, so the `convert.rs` fix would cost a full
  fixture regeneration for zero measured gain.
- **Clanging Scales' self Def −1** (rb1233), **Ice Face's RESTORE** (rb1253, still the only
  `species` first-divergence), **Rampage BeforeMove-cancel at `n == 1` with a NON-confused user**,
  **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet Rise's
  `onTry` failure** — all unchanged.
- **`apply_end_of_turn`'s `switched` parameter is still vestigial** — delete its `request.rs`
  plumbing next time that file is touched.
- Deliberately NOT modelled in the new `AfterMove` list, for want of a witness: the mover's own
  `onAfterMove` handlers (`lockedmove`, Condition subOrder 2) and the MOVE's own `onAfterMove`,
  which `runEvent` unshifts as a `sourceEffect` at subOrder 0 (`sim/battle.ts:783`). Either would
  lengthen the list without joining the White Herb tie.

## Kill criterion — evidence, NOT triggered

The rule is <1 game/commit across **3 consecutive** commits. Per-commit yield this tranche:

```
  commit  1  2  3  4  5  6  7  8  9 10
  games   1  1  1  2  2  1  0  0  1  1     mean 1.0
```

The longest run below the line is **two** (commits 7 and 8), and both were deliberate
completeness landings rather than attempts to flip a game: commit 7 was settled BY census (it
cannot flip anything in this corpus, by construction) and commit 8 advanced rb1345 from 7/32 to
31/32 decisions — real progress the game counter cannot see. Commits 9 and 10 then flipped one
each. **No asymptote assessment is due, and recording fresh seeds is NOT yet indicated**: 36 open
games still carry 20 distinct `draws-match/state-diff` mechanics bugs and six fully-localized
stream offsets, i.e. the existing corpus has not been mined out.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 484 / 512** (raised from 476 at burn-down X), and the non-exact SET must be a
   subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== BURN-DOWN VIII — certification (2026-07-27) ====

**HEADLINE: 466 / 512 full games byte-exact from seed (91.0%), up from 457; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Six parity commits, every one PS-source-grounded. Judged by the exact-SET diff on BOTH corpora at
every step: **the newly-non-exact set was EMPTY at all six.** 9 games / 6 commits = **1.5
games/commit**, so the early-stop line (<1 game/commit across 3 consecutive commits) was never
reached and no asymptote assessment is due. The tranche stopped on its session budget, not on a
kill criterion.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **466 / 512 = 91.0%**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |

`convert.rs` was NOT touched, so the seed fixtures' baked digests did not move and **no fixture
regeneration was needed**. `gen.rs` was NOT regenerated (no new `MoveData` fields). Five of the six
commits are `crates/engine/src/generate.rs`-only; `d62ee20` also touches `crates/cosim/src/seedgate.rs`
and `04717e6` touches `crates/engine/src/request.rs` (one `Branch` field constructor).

## THE LEVER: `spreadMoveHit`'s per-hit step table, as landed

Written from the pin FIRST, then used to place every call site. `sim/battle-actions.ts:1044-1155`;
`hitStepMoveHitLoop` calls the WHOLE of this once per hit.

| step | PS | engine counterpart | was it in the right place? |
|------|----|--------------------|-----------------------------|
| 0 | `tryPrimaryHitEvent` (Substitute) | sub routing inside the three hit loops | ✓ |
| 1 | `getSpreadDamage` → `getDamage` | per-hit crit `randomChance(1,den)` + `random(16)` + the `ModifyDamage` screen shuffle | ✓ |
| 2 | `spreadDamage` | `Instruction::Damage`, Sturdy / Focus Sash | ✓ |
| 3 | `runMoveEffects` (`onHit`) | `apply_bug_bite`, `apply_thaw_on_hit`, `apply_spirit_shackle`, `apply_sparkling_aria`, `apply_relic_song_forme` | ~ (they sit after step 4/5; no witness) |
| 4 | `selfDrops` | `apply_self_drop`, `start_rampage_lock` | ✓ |
| 5 | `secondaries()` | `apply_damage_secondaries`, `apply_burning_jealousy`, `apply_target_secondary`, `apply_alluringvoice_confusion`, `apply_triattack_secondary`, `apply_direclaw_secondary`, `apply_partial_trap`, `apply_flinch_split` | ✓ |
| 6 | `forceSwitch` | `apply_drag` | ✓ |
| 7 | `runEvent('DamagingHit')` | **`apply_damaging_hit_step7`** = `apply_damaging_hit_reactions` (Rough Skin / Iron Barbs / Rocky Helmet / Gulp Missile / Electromorphosis / Stamina / Water Compaction / Seed Sower / Toxic Debris / Gooey) + Justified / Rattled / Thermal Exchange / Weak Armor, then the DRAWING handlers `apply_contact_secondaries` / `apply_cursed_body`, then `apply_weakness_policy` | **✗ — was BEFORE step 5. Fixed.** |
| 8 | `onAfterHit` | Stone Axe / Ceaseless Edge hazards, `apply_spin_clear` | ~ |
| 9 | `eachEvent('Update')` | `apply_pinch_berry`, `consume_lum_if_statused`, `emit_update_hit` | ✓ |

The reorder mechanism: the three realized/enumerated hit loops (`apply_damage_hit_rolls`,
`apply_damage_hit_indexed`, `apply_multihit_realized_ma`) now DEFER the event onto
`Branch::pending_damaging_hit` instead of firing it inline. `apply_damaging_hit_step7` flushes it
**(a)** at the top of the next iteration — PS finishes hit n, step 7 included, before starting hit
n+1, and whatever the event moved is an input to hit n+1's `getDamage`, so the `restat_dirty` /
`calc` re-derivation moved with it; **(b)** for the LAST hit, after the caller's step-5 secondary
split and ahead of the drawing step-7 handlers, because PS orders Rough Skin / Iron Barbs
(`onDamagingHitOrder: 1`) and Rocky Helmet (2) before the unordered contact-status set — a chip
that faints the attacker must suppress its paralysis/burn.

Two deliberate consequences: Justified / Rattled / Thermal Exchange / Weak Armor now fire ONCE PER
HIT (they are `onDamagingHit`, like Stamina already was), and Weak Armor is now gated on `hit_sub`
(`runEvent('DamagingHit')` is gated on `damagedDamage.length`, and a Substitute hit records
`damage[i] === true`, not a number).

Two places deliberately NOT deferred, both because the reorder is provably a no-op there:
`realized_per_hit_damaging_hit` (the drawing half) stays inline — the realized multi-hit family
([2,5] + Population Bomb + Beat Up) has no `secondary`, so nothing separates it from step 5 — and
`apply_post_damage`'s once-per-move fallback (`!per_hit_done`: fixed-damage moves and the
enumeration DP paths), which has no `secondary` either and whose current position keeps the event
ahead of the Moxie / `onSourceAfterFaint` block that follows it.

**The one regression this tranche produced, and the lesson.** The first cut of the reorder dropped
rb1198 / rb1302 / rb1395 — exactly the three games burn-down VI's commit 4 had won. Deferring the
event moved the `pre_inputs` snapshot past `realized_per_hit_damaging_hit`, so a Flame Body burn
inflicted by the DRAWING half stopped invalidating the cached `DamageCalc`. The fix was to snapshot
before the drawing handlers and again around the flush. **Any change to a hit loop must keep a
`damage_inputs` snapshot straddling EVERY step-7 handler, drawing or not.**

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `04717e6` | **`spreadMoveHit` steps 5 and 7 were swapped** — the move's SECONDARIES run BEFORE `runEvent('DamagingHit')`. rb1122 d5: Palossand at Def +5, Azumarill's Liquidation lands and its 20% Def drop procs — PS takes 5 → 4 (secondary) → 6 (Water Compaction +2), the engine took 5 → 6 (clamped at the cap) → 5 | 457 → 458 | `sim/battle-actions.ts:1044-1155` (`:1116` secondaries, `:1142` DamagingHit) |
| 2 | `d62ee20` | **A double-switch Speed TIE is NOT state-neutral.** Two `switch` actions at order 103 are speed-sorted on the OUTGOING active's Speed; on a tie `commitChoices`' `queue.sort()` breaks it with one `shuffle[2,0,2]`, and the winner's `runSwitch` (order 101) preempts the loser's still-pending `switch` (103) — so the winner's switch-in ability fires while the LOSER'S OLD mon is still on the field. There is no second queue sort to compose with (the gen8 dynamic re-sort is gated on `queue.peek()?.choice === 'move'`), so side One goes first iff that single bit is 0 | 458 → 459 | `sim/battle.ts:3038`, `:2940` |
| 3 | `39bcf16` | **Queenly Majesty and Psychic Terrain only guarded DAMAGING moves.** Both engine blocks sat BELOW the `md.category == Status` dispatch, which returns first — and a status move is exactly what they block, because Prankster is what gives it priority. rb1061 d34: Klefki's Prankster Thunder Wave into a Queenly Majesty Tsareena; PS's whole unit is Tsareena's own move (the block makes NO draw), the engine rolled `randomChance(90,100)` and paralysed her. Psychic Terrain also picked up `effect.target === 'self'` exemption and the EFFECTIVE priority (`getActionSpeed` writes `action.move.priority` after `ModifyPriority`) | 459 → 463 | `sim/battle-actions.ts:485-492`; `data/abilities.ts:3671`; `data/moves.ts:14120-14123` |
| 4 | `6ce4a13` | **Four residual handlers PS collects and the engine never listed** — `whiteherb` (Item, 29/8), `shieldsdown` (Ability, 29/7), the `flinch` volatile (Condition, false/2 — duration-only) and `twoturnmove` (Condition, false/2 — duration-only, plus a SECOND handler for the semi-invulnerable moves' own condition). A missing handler both shortens `shuffle[len,i,j]` and can delete a tie group | 463 → 465 | `sim/battle.ts:486` (`getKey = 'duration'`), `:1102/:1111/:1119/:1126`, `:955-991` (subOrder table); `data/items.ts:7697`; `data/conditions.ts:198`, `:287` |
| 5 | `12e5770` | **Trick / Switcheroo never consulted `onTakeItem`.** `trick.onHit` bails the moment either transfer is refused, and then re-runs `singleEvent('TakeItem')` with the holders CROSSED, so an item that cannot be HELD by the other end blocks the swap too. rb1099 d57: a Choice Scarf Chandelure Tricks an Arceus-Dark holding a Dread Plate — PS fails outright, the engine swapped and handed the Choice lock to the Arceus. `item_removable_from` already existed for Knock Off / Magician / Pickpocket / Thief | 465 → 466 | `data/moves.ts:19889-19904`; `data/items.ts:1581-1586` |

(Commit 1's first cut also carried the `restat_dirty` regression fix described above; it is part of
`04717e6`, which is why the table has five roots across six commits — the sixth commit is the
docs/certification commit this section belongs to.)

Games flipped, by commit: 1 → rb1122; 2 → rb1250; 3 → rb1061 rb1245 rb1252 rb1370;
4 → rb1034 rb1378; 5 → rb1099.

Three named opens CLOSED outright: **the step-5/7 lever itself** (which also subsumes the Phase-7
"`apply_rattled` / `apply_thermal_exchange` / `apply_weak_armor` still run before the secondaries"
open), **the double-switch Speed tie** (rb1250, the only game in its class), and the
`args randomChance@hypervoice` / `@powerwhip` pair (rb1245 / rb1252 / rb1370, all taken by commit 3
without being touched — they were Queenly Majesty games mislabelled by their draw class).

## What the re-triage method produced this tranche — the new cheap step

Burn-down VII's `|Δhp| == 0` sweep still works and produced the Trick root (rb1099's item pair).
The NEW step, worth repeating first thing next time, is a **handler-list census off the sidecars**:

> Every recorded `shuffle` draw carries `group` (the tied handlers) AND `full` (PS's entire sorted
> handler list) with each entry's `effect` / `effectType` / `order` / `subOrder` / `speed`. Group
> the whole 401-game corpus by `(eventid, effect, effectType, order, subOrder, cb)` and diff the
> result against the engine's model. It is PS's own answer to "what is in this list", measured, not
> guessed.

That census produced commit 4 in one pass: of the 37 distinct residual triples the corpus contains,
`residual_handlers` was missing exactly four, and each came with its own witness decision. It also
produced the *shape* census below, which is the standing worklist for the next tranche.

**Shuffle-signature census (401 sidecars), `(eventid | tied-handler group)` → count / games.**
Everything not listed under "modelled" is a shuffle the engine does not emit at all:

```
   792  48  Update      | MON~MON              modelled (eachEvent Update)
   267  64  -           | MON~MON              modelled (commitChoices queue.sort)
   263 102  Residual    | stall~protect        modelled
   112  42  BeforeTurn  | MON~MON              modelled
    99  53  Residual    | protect~stall        modelled
    31   4  ModifyDamage| lightscreen~reflect  modelled
    12   2  Residual    | slowstart~slowstart   modelled   rb1310 rb1369
     8   5  DisableMove | choicelock~encore    modelled
     5   1  Residual    | grassyterrain~…      modelled   rb1360
     4   2  Residual    | leftovers~leftovers  modelled   rb1021 rb1141
     4   4  DisableMove | choicelock~healblock modelled
     4   3  Weather     | MON~MON              modelled
     3   3  WeatherChange | MON~MON            modelled   rb1195 rb1250 rb1362
     2   2  Residual    | flinch~stall         FIXED this tranche (commit 4)
     2   1  AfterMove   | whiteherb~whiteherb  ** NOT MODELLED **  rb1345
     1   1  Residual    | whiteherb~whiteherb  FIXED this tranche (commit 4)
     1   1  TerrainChange | MON~MON            modelled   rb1099
```

## The 46 still-open games, re-triaged at the certifying commit

First-divergence CLASS split (from the 512 gate):

| n | class |
|---|-------|
| 22 | `draws-match/state-diff` |
| 8 | `result random[16]@…` — a draw miscount in an EARLIER unit |
| 2 each | `PS shuffle@generic`, `args randomChance@struggle` |
| 1 each | `PS random@confusion`, `PS randomChance@heavyslam`, `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer` / `randomChance@freezedry` / `randomChance@icebeam` / `sample@trace`, `args randomChance@par`, `args shuffle@generic`, `rust-extra randomChance@accuracy`, `rust-extra randomChance@crit` |

The 8 `result random[16]` games (rb1369's `@knockoff` survives; the set is otherwise unchanged):
rb1021 d59(thunderbolt, rust=13), rb1057 d26(sludgewave, 9), rb1271 d11(hydropump, 9),
rb1277 d12(gigadrain, 14), rb1310 d18(outrage, 1), rb1329 d24(heatcrash, 1),
rb1343 d36(voltswitch, 8), rb1369 d46(knockoff, 14).

Every open game, its first divergent unit, the move pair, and the first divergent field:

```
  rb1011 d43 t33 [closecombat switch]          s0#3.hp: engine=140 ps=77
  rb1012 d60 t52 [gigadrain scald]             s0#2.hp: engine=138 ps=185
  rb1021 d59 t50 [thunderbolt wish]            s1#3.hp: engine=145 ps=214
  rb1024 d81 t73 [struggle switch]             s0#3.hp: engine=308 ps=250
  rb1029 d22 t18 [gunkshot swordsdance]        s0#3.hp: engine=222 ps=246
  rb1030 d53 t46 [toxic hypervoice]            s1#5.hp: engine=61 ps=78
  rb1033 d44 t33 [substitute discharge]        s0.volatiles: engine=3 ps=1  (bit 1 Substitute EXTRA)
  rb1040 d2 t3   [stealthrock earthpower]      s0#0.hp: engine=230 ps=217
  rb1057 d26 t25 [sludgewave psychicnoise]     s0#5.hp: engine=88 ps=76
  rb1093 d22 t17 [icehammer switch]            s0.boost.spe: engine=-2 ps=-3
  rb1103 d37 t32 [strengthsap struggle]        s0#0.hp: engine=222 ps=136
  rb1108 d4 t5   [shadowsneak beakblast]       s0#2.hp: engine=89 ps=73
  rb1116 d7 t6   [knockoff closecombat]        s0#4.hp: engine=268 ps=251
  rb1119 d8 t7   [moonblast sludgewave]        s1#4.types: engine=[Fire,None] ps=[Fairy,None]
  rb1121 d18 t15 [hurricane uturn]             s1.volatiles: engine=1 ps=0  (bit 0 Confusion EXTRA)
  rb1125 d2 t3   [icebeam flowertrick]         s0#0.hp: engine=0 ps=27
  rb1126 d7 t5   [sludgebomb strengthsap]      s1.volatiles: engine=0 ps=2^28  (Unburden MISSING)
  rb1184 d5 t6   [terastarstorm tachyoncutter] s1#4.hp: engine=196 ps=142
  rb1191 d17 t14 [thunderbolt playrough]       s0#1.hp: engine=25 ps=33
  rb1231 d15 t12 [struggle uturn]              s0#3.hp: engine=274 ps=208
  rb1233 d39 t32 [clangingscales wish]         s0.boost.def: engine=-2 ps=-1
  rb1236 d37 t29 [dracometeor voltswitch]      s0#4.hp: engine=51 ps=18
  rb1239 d64 t51 [hurricane dazzlinggleam]     s1.stall_counter: engine=0 ps=1
  rb1243 d11 t10 [stompingtantrum knockoff]    s1#1.hp: engine=94 ps=156
  rb1244 d10 t7  [voltswitch switch]           s1#4.ability: engine=Trace ps=WaterAbsorb
  rb1253 d12 t10 [snowscape bellydrum]         s1#2.species: engine=222 ps=221  (Ice Face restore)
  rb1271 d11 t9  [switch hydropump]            s0#3.hp: engine=225 ps=221
  rb1277 d12 t9  [sludgebomb gigadrain]        s0#5.hp: engine=293 ps=290
  rb1283 d17 t13 [knockoff switch]             s1#2.hp: engine=114 ps=99
  rb1300 d52 t48 [sleeptalk focusblast]        s0#1.hp: engine=152 ps=174
  rb1310 d18 t14 [dragondance outrage]         s1.pending_move: Rampaging(589,2) vs (589,1)
  rb1314 d45 t38 [stickyweb revivalblessing]   s1#0.item: engine=LightClay ps=None
  rb1315 d28 t26 [earthquake knockoff]         s0#2.hp: engine=189 ps=205
  rb1326 d50 t40 [superfang protect]           s1.substitute_hp: engine=66 ps=48
  rb1329 d24 t17 [heatcrash earthquake]        s0#5.hp: engine=112 ps=120
  rb1343 d36 t27 [voltswitch kowtowcleave]     s0#4.hp: engine=60 ps=52
  rb1345 d11 t9  [icebeam icebeam]             s1#4.hp: engine=233 ps=231
  rb1347 d61 t56 [trick rest]                  s1#1.last_berry: engine=None ps=ChestoBerry
  rb1348 d12 t11 [drainingkiss outrage]        s0#1.hp: engine=159 ps=107
  rb1359 d7 t7   [switch calmmind]             s0#0.types: engine=[Normal,Fire] ps=[Ghost,None]
  rb1360 d6 t6   [dragontail dragontail]       s1#2.move3.pp: engine=7 ps=8
  rb1362 d24 t20 [icebeam thunderbolt]         s0#1.hp: engine=138 ps=120
  rb1369 d46 t42 [knockoff bodyslam]           s1#0.hp: engine=162 ps=70
  rb1380 d15 t15 [scaleshot toxic]             s0#1.hp: engine=157 ps=173
  rb1387 d36 t32 [encore freezedry]            s0#3.hp: engine=337 ps=218
  rb1391 d20 t15 [switch heavyslam]            s0#3.hp: engine=207 ps=204
```

Recurrences to re-scan after every landing: **`knockoff` 5x** (rb1116 rb1243 rb1283 rb1315 rb1369 —
still the largest move cluster), `icebeam` 3x (rb1125 rb1345 rb1362), `struggle` 3x (rb1024 rb1103
rb1231), `thunderbolt` 3x (rb1021 rb1191 rb1362), `voltswitch` 3x (rb1236 rb1244 rb1343),
`switch` as one half 6x.

## Named opens carried forward — newly located first

- **NEW, fully diagnosed, 1 game: the `runEvent('AfterMove')` tie shuffle is not modelled.**
  White Herb / Eject Pack / Mirror Herb (items) and Opportunist (ability) all register
  `onAnyAfterMove`, so EVERY move action collects one handler per holder on the field; two holders
  at equal Speed tie and consume a `shuffle[2,0,2]`. rb1345 d11 records it twice, with `full` =
  exactly `[whiteherb/false/8/171 ×2]`, positioned between the move's internal 970/1024 Updates and
  the trailing runAction 2882. Sizing it correctly also needs the mover's own volatile handlers
  (`lockedmove`, `charge`, `beakblast` conditions) and the MOVE's own `onAfterMove` (Mind Blown,
  Steel Beam, Sparkling Aria, Ice Ball, Rollout, Spit Up — `runEvent` unshifts a `sourceEffect`
  handler at `sim/battle.ts:783`, subOrder 0, so it lengthens the list without joining the tie).
  rb1345 is the only game in the corpus that needs it.
- **NEW: `lockedmove` is a residual handler PS collects and the engine still does not.** It has
  `duration: 2` AND an `onResidual` (`data/conditions.ts:255-262`), order `false`, Condition
  subOrder 2 — so a rampaging mon must contribute one. It was left OUT of commit 4 because no
  recorded residual `shuffle` in the corpus fired while a rampage was live, so there is no witness
  to size it against. Adding it blind changes `shuffle[len,…]` for every rampage turn; do it with a
  witness or with a directed trace.
- **NEW: rb1362 d24 is a `random(100)` branch-SELECTION failure, not a mechanics bug.** PS's
  Thunderbolt rolls `random(100) = 2` against a 10% paralysis and procs; the engine's chosen branch
  carries the no-proc placeholder (`random[100]=10`) and then never rolls the victim's
  `randomChance(1,4)@par`, so the streams desync by one draw and the second mover's Ice Beam freeze
  is selected off the wrong slot. `replicate_select`'s threshold decode
  (`seedgate.rs:255-273`) needs `distinct[0] == 0`; work out which earlier position eliminated the
  proc branch. One game, but the mechanism is shared with every secondary split.
- **NEW: rb1391 d20's pre-swap Speed tie is evaluated differently by the two.** PS's unit opens
  with the move's accuracy — no turn-start or switch-out shuffle at all — while the engine emits
  three `shuffle[2,0,2]` before it. The engine believes the PRE-switch board is Speed-tied and PS
  does not; the POST-switch board is tied in both (PS's four trailing shuffles match).
- **Roost typing is an ENCODING artifact, not PS state** — unchanged from burn-down VII, and
  DELIBERATELY not fixed this tranche: no open game's first divergence is a Roost window
  (`rb1119` and `rb1359` are both real type CHANGES, not a stripped Flying), so the principled
  `convert.rs`/`digest.rs` fix would cost a full fixture regeneration — every baked digest moves —
  for zero measured gain. Fix it when a witness appears, or fold it into the next tranche that has
  to regenerate fixtures anyway.
- **The `struggle` cluster (rb1024, rb1103, rb1231) is REQUEST LEGALITY**, unchanged. rb1024 is
  still the largest single gap in the corpus (121 HP at t73).
- **Shed Skin is residual order 5/3** and still runs in the branching tail after Harvest (28/2);
  fixing it moves a DRAW, so the residual's deterministic tail must become branch-based first.
  Witnesses rb1315, rb1380.
- **Clanging Scales' self Def −1** (rb1233 d39), **Ice Face's RESTORE** (rb1253, still the only
  `species` first-divergence), **Rampage BeforeMove-cancel at `n == 1` with a NON-confused user**,
  **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet
  Rise's `onTry` failure** — all unchanged.
- **The BeforeMove ladder's other half is still OPEN and is now correctly scoped.** confusion (3),
  Attract (2) and paralysis (1) are still resolved in `execute_move`'s outer chain, i.e. ahead of
  the order-5..7 cancels (Disable 7, Throat Chop / Heal Block / Gravity 6, Taunt 5) that live in
  `execute_move_inner`. Flinch (8) is already correct — `flinch_cancel_chain` never reaches the
  low ladder. **The four `eff=par ev=BeforeMove` games are NOT this bug** (rb1034 and rb1061 were
  taken by commits 4 and 3; rb1243 and rb1362 are a knock-off HP gap and the `replicate_select`
  bug above), so the ladder currently has NO witness in the corpus. The combination it needs is a
  lock applied THIS TURN by a faster foe (Taunt / Heal Block are `onDisableMove` too, so the
  request already excludes them otherwise) landing on a confused / attracted / paralysed mover.
  Either find one with a directed trace or leave it; it is not worth a blind refactor.
- **`apply_end_of_turn`'s `switched` parameter is still vestigial** — delete its `request.rs`
  plumbing next time that file is touched.
- **Kill criterion: NOT triggered.** 1.5 games/commit across five parity commits; the worst single
  commit still flipped one game, and the best flipped four.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 466 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== BURN-DOWN VII — certification (2026-07-26) ====

**HEADLINE: 457 / 512 full games byte-exact from seed (89.3%), up from 444; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Seven parity commits, every one PS-source-grounded, plus the missing burn-down VI certification
(written retroactively — see the section below it). Judged by the exact-SET diff on BOTH corpora
at every step: **the newly-non-exact set was EMPTY at all seven.** 13 games / 7 commits =
**1.86 games/commit**, so the early-stop line (<1 game/commit across 3 consecutive commits) was
never approached and no asymptote assessment is due.

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **457 / 512 = 89.3%**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |

`convert.rs` was NOT touched, so the seed fixtures' baked digests did not move and no fixture
regeneration was needed. `gen.rs` was NOT regenerated (no new `MoveData` fields). Every parity
commit is `crates/engine/src/generate.rs`-only.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `6d1c053` | **The residual pass ABORTS the instant the battle ends** — `fieldEvent` runs `this.faintMessages(); if (this.ended) return;` after EVERY handler, and `turnLoop` then returns on `this.ended` WITHOUT calling `endTurn()`, so no later residual handler runs, the trailing `eachEvent('Update')` never fires, and endTurn's per-active bookkeeping (the `stats*ThisTurn` reset, the DisableMove / TrapPokemon sorts) never happens. **AND psn / tox are `onResidualOrder: 9` while brn is 10** — different orders, so PS's one queue runs EVERY poison chip before ANY burn chip, speed-sorted within an order; the engine folded both into one per-side loop that always ran side One first | 444 → 448 | `sim/battle.ts:565-566` `:519` `:2974` `:1675-1676`; `data/conditions.ts:15` (brn 10) `:133` (psn 9) `:154` (tox 9) |
| 2 | `9ca8c04` | **Disable (7), Throat Chop / Heal Block / Gravity (6) and Taunt (5) are BELOW slp / frz (10), mustrecharge (11), Truant (9) and flinch (8)** in the `BeforeMove` ladder, and `runEvent` short-circuits on the first `false` — so a sleeping mon never reaches the Taunt cancel and slp's `time--` still fires. The engine ran the cancel block first, freezing the sleep counter | 448 → 450 | `data/moves.ts` disable / throatchop / healblock / gravity / taunt `onBeforeMovePriority`; `data/conditions.ts` slp `onBeforeMove` |
| 3 | `fd03ce2` | **Justified is an `onDamagingHit`, so a Substitute cancels it** (it sat one line above the engine's existing `!hit_sub` guard) — **and Speed Boost's gate is `pokemon.activeTurns`, which a DRAG zeroes too**, not the "chose to switch" flag the engine read | 450 → 451 | `sim/battle-actions.ts:1142` `:137`; `data/abilities.ts:4412` |
| 4 | `07fca24` | **Protect outranks every absorbing ability** — Protect's condition is `onTryHitPriority: 3`, Sap Sipper / Lightning Rod / Storm Drain / Motor Drive are 1 and Volt Absorb / Water Absorb / Dry Skin / Earth Eater / Flash Fire are 0, and all of them are handlers of the SAME `runEvent('TryHit')` in `hitStepTryHitEvent`. A protected target gets no heal, no redirect boost, no Flash Fire | 451 → 452 | `data/moves.ts:13989` (protect) vs `data/abilities.ts` `onTryHitPriority: 1` |
| 5 | `d9b1b4f` | **Throat Chop's lock IS a `secondary`** (`{chance: 100, onHit: addVolatile}`), so Sheer Force deletes it — and the same codegen invisibility had kept Throat Chop OUT of `sheer_force_active`, costing it the ×1.3. **AND Aroma Veil blocks a 100%-secondary Heal Block** (the `target_volatile` path never checked it) | 452 → 454 | `data/moves.ts` throatchop `secondary`; `data/abilities.ts:235-243` (aromaveil) |
| 6 | `8414464` | **Gulp Missile** — `onSourceTryPrimaryHit` loads Cramorant into `cramorantgulping` / `cramorantgorging` (at or below half HP) when its Surf connects; `onDamagingHit` on the loaded forme deals 1/4 of the ATTACKER's `baseMaxhp`, then Def −1 or paralysis, then reverts. A fainted attacker suppresses the revert too | 454 → 457 | `data/abilities.ts` gulpmissile |

Games flipped, by commit: 1 → rb1048 rb1148 rb1237 rb1278; 2 → rb1009 rb1356; 3 → rb1147;
4 → rb1299; 5 → rb1304 rb1072; 6 → rb1288 rb1367 rb1372.

Three named opens CLOSED outright: **the `stats*ThisTurn` trio** (rb1048 / rb1237 / rb1278 — the
shared structure the burn-down VI re-triage had just identified as the largest untried one in the
non-`hp` half), **the `status_counter` class** (rb1009 / rb1356, both of them), and **Gulp Missile**
(rb1288 / rb1367, open since Phase 5).

## What the re-triage method produced this tranche

Three of the seven roots came from the SAME cheap step, worth repeating verbatim next time:

1. Take every open game whose first divergence has **|Δhp| == 0** — a boost, a volatile bit, a
   counter. Those are single-mechanic bugs with no downstream noise.
2. Decode the volatile bitmasks against the enum order in `crates/engine/src/volatile.rs`
   (discriminant = bit index) and read the *direction* (engine EXTRA vs engine MISSING).
3. Join on direction + PS handler. Three MISSING `stats*ThisTurn` bits from three different boost
   paths named one root; two EXTRA `ThroatChop`/`HealBlock` bits named two more.

The other lever that paid: **checking `stateAfter.ended` on the divergent unit.** All three
witnesses for root 1 were the LAST decision of their game with `ended: true` and `midTurn: true` —
which is precisely the signature of "PS returned before `endTurn`".

## The 55 still-open games, re-triaged at the certifying commit

First-divergence CLASS split (from the 512 gate):

| n | class |
|---|-------|
| 25 | `draws-match/state-diff` |
| 9 | `result random[16]@…` — a draw miscount in an EARLIER unit |
| 2 each | `args randomChance@struggle`, `args randomChance@par`, `rust-extra randomChance@accuracy`, `PS shuffle@generic` |
| 1 each | `PS random@confusion`, `PS randomChance@heavyslam`, `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer` / `randomChance@freezedry` / `randomChance@icebeam` / `sample@trace`, `args randomChance@hypervoice` / `randomChance@powerwhip` / `shuffle@generic`, `rust-extra randomChance@crit` |

First-divergence FIELD split: **33 `hp`**, 5 `volatiles`, 6 `boosts` (3 def / 2 atk / 1 spe),
2 `types`, 1 each `status` / `stall_counter` / `ability` / `species` / `pending_move` / `item` /
`substitute_hp` / `last_berry` / `pp`. Of the 33 `hp` games **25 exceed 10 HP**, 5 sit in 4-10, and
3 are within 3.

The 9 `result random[16]` games and the roll that first disagrees (unchanged set):
rb1021 d59(thunderbolt, rust=13), rb1057 d26(sludgewave, 9), rb1271 d11(hydropump, 9),
rb1277 d12(gigadrain, 14), rb1310 d18(outrage, 1), rb1329 d24(heatcrash, 1),
rb1343 d36(voltswitch, 8), rb1369 d46(knockoff, 14), rb1378 d8(ceaselessedge, 6).

The 5 remaining `volatiles` games, decoded (bit index = `volatile.rs` discriminant):
rb1033 bit 1 `Substitute` engine EXTRA; rb1099 bit 24 `ChoiceLock` on the WRONG SIDE (a Trick
divergence, with the items also swapped); rb1121 bit 0 `Confusion` engine EXTRA; rb1126 bit 28
`Unburden` engine MISSING (PS ate the Sitrus, the engine still holds it); rb1245 bit 4 `Encore`
engine EXTRA. Down from 10 — the whole `stats*ThisTurn` pair and both `ThroatChop`/`HealBlock`
bits are gone.

Every open game, its first divergent unit, the move pair, the PS handlers its draws name, and
`[first-divergent-field, max |Δhp| in the block]`, sorted by hp gap:

```
  rb1024 d81 t73 p1:struggle p2:switch [hp 121]
  rb1126 d7 t5 p1:sludgebomb p2:strengthsap [volatiles 121]
  rb1387 d36 t32 p1:encore p2:freezedry [hp 119]
  rb1029 d22 t18 p1:gunkshot p2:swordsdance [hp 97]
  rb1369 d46 t42 p1:knockoff p2:bodyslam [hp 92]
  rb1103 d37 t32 p1:strengthsap p2:struggle [hp 86]
  rb1125 d2 t3 p1:icebeam p2:flowertrick/T [hp 79]
  rb1021 d59 t50 p1:thunderbolt p2:wish [hp 69]
  rb1348 d12 t11 p1:drainingkiss p2:outrage/T [hp 69]
  rb1231 d15 t12 p1:struggle p2:uturn [hp 66]
  rb1252 d21 t15 p1:powerwhip p2:partingshot [boost.atk 66]
  rb1011 d43 t33 p1:closecombat p2:switch [hp 63]
  rb1243 d11 t10 p1:stompingtantrum p2:knockoff eff=par ev=BeforeMove [hp 62]
  rb1362 d24 t20 p1:icebeam p2:thunderbolt eff=par ev=BeforeMove [hp 56]
  rb1184 d5 t6 p1:terastarstorm p2:tachyoncutter [hp 54]
  rb1012 d60 t52 p1:gigadrain p2:scald eff=futuremove ev=End [hp 47]
  rb1034 d57 t46 p1:knockoff p2:triplearrows eff=par ev=BeforeMove [boost.def 35]
  rb1236 d37 t29 p1:dracometeor p2:voltswitch [hp 33]
  rb1300 d52 t48 p1:sleeptalk p2:focusblast eff=toxicchain ev=DamagingHit [hp 22]
  rb1326 d50 t40 p1:superfang p2:protect eff=stall ev=StallMove [substitute_hp 18]
  rb1030 d53 t46 p1:toxic p2:hypervoice eff=harvest ev=Residual [hp 17]
  rb1116 d7 t6 p1:knockoff/T p2:closecombat [hp 17]
  rb1108 d4 t5 p1:shadowsneak p2:beakblast eff=cursedbody ev=DamagingHit [hp 16]
  rb1315 d28 t26 p1:earthquake p2:knockoff eff=shedskin,toxicchain ev=DamagingHit,Residual [hp 16]
  rb1380 d15 t15 p1:scaleshot p2:toxic eff=shedskin ev=Residual [hp 16]
  rb1033 d44 t33 p1:substitute p2:discharge eff=confusion ev=BeforeMove [volatiles 15]
  rb1283 d17 t13 p1:knockoff p2:switch eff=trace ev=Update [hp 15]
  rb1057 d26 t25 p1:sludgewave p2:psychicnoise [hp 14]
  rb1378 d8 t8 p1:nastyplot p2:ceaselessedge/T [hp 14]
  rb1040 d2 t3 p1:stealthrock p2:earthpower/T [hp 13]
  rb1191 d17 t14 p1:thunderbolt p2:playrough [hp 8]
  rb1329 d24 t17 p1:heatcrash p2:earthquake [hp 8]
  rb1343 d36 t27 p1:voltswitch p2:kowtowcleave [hp 8]
  rb1061 d34 t27 p1:thunderwave p2:rapidspin eff=par ev=BeforeMove [hp 7]
  rb1121 d18 t15 p1:hurricane p2:uturn eff=toxicchain ev=DamagingHit [volatiles 6]
  rb1370 d3 t4 p1:hypervoice p2:thunderwave [status 5]
  rb1271 d11 t9 p1:switch p2:hydropump/T [hp 4]
  rb1277 d12 t9 p1:sludgebomb p2:gigadrain [hp 3]
  rb1391 d20 t15 p1:switch p2:heavyslam [hp 3]
  rb1345 d11 t9 p1:icebeam p2:icebeam [hp 2]
  rb1093 d22 t17 p1:icehammer p2:switch [boost.spe 0]
  rb1099 d57 t47 p1:trick p2:calmmind [volatiles 0]
  rb1119 d8 t7 p1:moonblast p2:sludgewave [types 0]
  rb1122 d5 t6 p1:shadowball p2:liquidation [boost.def 0]
  rb1233 d39 t32 p1:clangingscales p2:wish [boost.def 0]
  rb1239 d64 t51 p1:hurricane p2:dazzlinggleam [stall_counter 0]
  rb1244 d10 t7 p1:voltswitch/T p2:switch [ability 0]
  rb1245 d18 t15 p1:hypervoice p2:encore [volatiles 0]
  rb1250 d32 t29 p1:switch p2:switch [boost.atk 0]
  rb1253 d12 t10 p1:snowscape p2:bellydrum [species 0]
  rb1310 d18 t14 p1:dragondance p2:outrage eff=lockedmove ev=Start [pending_move 0]
  rb1314 d45 t38 p1:stickyweb p2:revivalblessing [item 0]
  rb1347 d61 t56 p1:trick p2:rest [last_berry 0]
  rb1359 d7 t7 p1:switch p2:calmmind [types 0]
  rb1360 d6 t6 p1:dragontail p2:dragontail/T [move3.pp 0]
```

Recurrences to re-scan after every landing: **`knockoff` 6x** (rb1034 rb1116 rb1243 rb1283 rb1315
rb1369 — still the largest move cluster), `icebeam` 4x (rb1125 rb1345 rb1362 + rb1345's mirror),
`struggle` 3x (rb1024 rb1103 rb1231), `thunderbolt` 3x (rb1021 rb1191 rb1362), `hypervoice` 3x
(rb1030 rb1245 rb1370), `voltswitch` 3x (rb1236 rb1244 rb1343). Handler recurrences: `eff=par
ev=BeforeMove` 4x (rb1034 rb1061 rb1243 rb1362), `eff=toxicchain ev=DamagingHit` 3x (rb1121
rb1300 rb1315), `eff=shedskin ev=Residual` 2x (rb1315 rb1380).

## Named opens carried forward — with the newly-located ones first

- **THE BIGGEST STRUCTURAL ONE, now localized: the move's SECONDARIES run AFTER the per-hit
  `runEvent('DamagingHit')` in the engine, and BEFORE it in PS.** `spreadMoveHit`'s numbered steps
  are 5. `secondaries()` then 7. `runEvent('DamagingHit')`; the engine's hit loop fires the
  DamagingHit group at the end of each hit (`apply_damage_hit_rolls`) and the caller applies the
  secondaries afterwards. **rb1122 d5 is the exact witness**: Palossand sits at Def +5, Azumarill's
  Liquidation lands and its 20% Def drop procs. PS takes 5 → 4 (secondary) → 6 (Water Compaction
  +2). The engine takes 5 → 6 (clamped, Water Compaction) → 5 (secondary). This subsumes the
  Phase-7 "`apply_rattled` / `apply_thermal_exchange` / `apply_weak_armor` still run before the
  secondaries" open (Justified left that list in commit 3). It is invisible in the DRAW stream —
  the no-draw handlers make no roll — so only `stateAfter` can catch it, and fixing it means moving
  the secondaries INTO the hit loop, which will move `random[100]@secondary` draws for multi-hit
  moves. Wants its own tranche.
- **A double SWITCH on a Speed TIE must branch on the shuffle's outcome.** rb1250 d32 is the
  witness and it is fully diagnosed: Heatran and Malamar are both at 167 Speed, the sidecar's first
  draw is the `shuffle[2,0,2]` over the two `switch` actions (order 103), and PS's realized order
  put p2 first — so Salamence's Intimidate landed on the OUTGOING Heatran and Rabsca came in clean.
  The engine treats that shuffle as annotation-only and always resolves side One first. The
  interleaving itself is already right (`switch` 103 vs `runSwitch` 101 — the faster side completes
  its whole entry before the slower side switches; a double REPLACEMENT is `instaswitch` at order 3
  and correctly batches instead), so only the tie-break is missing.
- **Clanging Scales' self Def −1 lands in the engine and not in PS** (rb1233 d39, 2 diffs). Not yet
  diagnosed; the target is a Sap Sipper Farigiraf and the user a Soundproof Kommo-o.
- **The `struggle` cluster (rb1024, rb1103, rb1231) is REQUEST LEGALITY**, unchanged. rb1024 is
  still the largest single gap in the corpus (121 HP).
- **Shed Skin is residual order 5/3** and still runs in the branching tail after Harvest (28/2);
  fixing it moves a DRAW, so the residual's deterministic tail must become branch-based first.
  Witnesses rb1315, rb1380.
- **Ice Face's RESTORE (rb1253)** — unchanged; now the ONLY `species` first-divergence left.
- **Rampage BeforeMove-cancel at `n == 1` with a NON-confused user** — unchanged; rb1310 is still a
  `result random[16]@outrage` prng-offset game, not this.
- **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet Rise's
  `onTry` failure** — unchanged from Phase 5/6.
- **NEW, opened by commit 2: confusion (3), Attract (2) and paralysis (1) are still resolved in the
  OUTER pre-move chain**, i.e. ahead of the order-5..7 cancels rather than behind them. PS would
  cancel on Taunt without ticking the confusion counter or rolling its 1/3 — a DRAW-order
  consequence, so it wants its own commit and its own witness.
- **NEW, opened by commit 3: `apply_end_of_turn`'s `switched` parameter is vestigial.** Its plumbing
  through `request.rs`'s `Pending::Pivot` / `Pending::Revive` can be deleted the next time that
  file is touched.
- **NEW, opened by commit 1: Roost's typing is an ENCODING artifact, not PS state.** PS never
  mutates `pokemon.types` for Roost — the `roost` volatile only filters Flying out of `getTypes()`
  via `onType` — while the engine strips the type and restores it at residual order 25. The
  battle-ended abort path now un-strips it explicitly, but ANY decision boundary that falls inside
  a live Roost window (a pivot or faint request after the Roost, on the same turn) has the same
  mismatch. The principled fix is in `convert.rs`/`digest.rs` (compare the restored typing when
  `Roosted` is set, the way the `Roosted` BIT is already masked), and it costs a fixture
  regeneration.
- **Kill criterion: NOT triggered.** 1.86 games/commit across seven commits; the worst single
  commit still flipped one game.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 457 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== BURN-DOWN VI — certification (2026-07-26, written retroactively) ====

**HEADLINE: 444 / 512 full games byte-exact from seed (86.7%), up from 433; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

This section was MISSING: the tranche that landed `3d99bcf..51fed0b` ended without writing it.
It is reconstructed from the seven commit bodies plus a FULL re-run of every gate and a fresh
re-triage of the 68 open games at `51fed0b`. Every number below is measured, not carried over.

## Final gate numbers (re-run at `51fed0b`, 2026-07-26)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **444 / 512 = 86.7%**; init-aligned **512 / 512** |
| Fresh-seed half alone | `GATE_THREADS=1 SEED_GATE=1 cosim harness/seed-sidecars/*.json.gz` | **333 / 401 = 83.0%** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** (13 `rust-finished-with-unconsumed-draws`, 3 `args-mismatch`, 3 `state-mismatch-despite-draw-match`) |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |

`convert.rs` was NOT touched by any of the seven commits, so the seed fixtures' baked digests did
not move and no fixture regeneration was needed. `gen.rs` was NOT regenerated (no new `MoveData`
fields this tranche). Six of the seven commits are `generate.rs`-only; `cb13835` also added
`DBG_INSTR=1` to `seedgate.rs`.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `3d99bcf` | **Toxic Debris is an `onDamagingHit`, so it scatters a layer PER HIT** — and a hit that does no damage lays none. Moved out of `apply_post_damage` (once per move, outside the `any_damage` gate) into `apply_damaging_hit_reactions` | 433 → 433 (latent; all 8 Glimmora games already exact and stayed exact) | `data/abilities.ts:5061`; `sim/battle-actions.ts:1142` inside `hitStepMoveHitLoop` |
| 2 | `cb13835` | **Wish resolves at residual order 4** — ahead of Grassy Terrain / Leftovers (5), Ingrain (7), Leech Seed (8) and the poison/burn chip (9/10). The engine resolved it at the END of the residual pass. Orders differ whenever both touch the same bar, because `heal()` clamps | 433 → 437 | `data/moves.ts:20945` (`onResidualOrder: 4`); `sim/battle.ts:507`, `:512-514` (slot conditions exempt from the fainted-holder skip) |
| 3 | `67c93ee` | **The residual pass is ONE globally ordered queue** — seven handler groups were on the wrong side of another. Grassy Terrain 5/2 before Leftovers 5/4; Hydration 5/3 (so a cured holder takes NO chip that turn); Curse 12 ahead of Salt Cure / trap 13 and Octolock 14; the counters (Taunt 15 … Perish Song 24) before 26/27/28; Roost 25; screens/Tailwind 26 before terrain/Trick Room 27; Bad Dreams / Cud Chew / Speed Boost 28/2 before the orbs 28/3; Hunger Switch 29 and the rampage `lockedmove` tick (order `false`) LAST, not first | 437 → 437 (exact SET byte-identical — the expected shape for an ordering fix whose witnesses commit 2 already took) | `sim/battle.ts:507`; `data/abilities.ts:1880` (Hydration); `data/moves.ts:3298` (Curse) |
| 4 | `03682fe` | **A per-hit `DamagingHit` STATUS is visible to the next hit's `getDamage`** — PS re-derives the damage calc every loop iteration, so a Flame Body burn on hit 1 halves the attacker's Atk for hits 2-3. Replaced the hard-coded `damaging_hit_reactions_change_stats` ability list with a before/after snapshot of the real formula inputs (`DamageInputs`: both actives' status/boosts/ability/item/types, weather, terrain, defending screens) | 437 → 440 | `sim/battle-actions.ts:1142`, `:874-970` |
| 5 | `e7a9816` | **A switch action ends with `eachEvent('Update')`** — the entering mon eats its Sitrus / cures with Lum on the spot, so a mon that switches into hazards and lands at or below half heals THERE. The engine emitted the Update's shuffle but never ran its payload on a switch-in | 440 → 442 | `sim/battle.ts:2882` (`runAction`); `data/items.ts` sitrusberry / lumberry / chestoberry |
| 6 | `dca2e5d` | **Photon Geyser's `onModifyMove` flips it to Physical when Atk > SpA** — `getStat(stat,false,true)` is boosted-but-unmodified, strict `>`, tie stays Special. Ultra Necrozma's `lightthatburnsthesky` carries the identical hook and is covered by the same arm | 442 → 443 | `data/moves.ts:13342-13357` |
| 7 | `51fed0b` | **A move's self-HP cost (Substitute) triggers the pinch berry at the move-action `Update`** — the Update's PAYLOAD, not just its shuffle | 443 → 444 | `sim/battle.ts:2882` / `sim/battle-actions.ts:970` |

Games flipped, by commit: 1 → none (latent); 2 → rb1209 rb1226 rb1229 rb1274; 3 → none;
4 → rb1198 rb1302 rb1395; 5 → rb1003 rb1016; 6 → rb1280; 7 → rb1227.
**Newly-non-exact was EMPTY at all seven.** 11 games / 7 commits = 1.57 games/commit.

Two clusters named in the Phase-8 taxonomy dissolved as MISLABELS, which is the tranche's main
taxonomic result:
- **`eff=flamebody` (rb1198 rb1302)** was not a Flame Body bug at all — both were commit 4's
  stale-damage-calc root. The cluster is closed.
- **`eff=par ev=BeforeMove` (rb1226) and `eff=cursedbody` (rb1229)** were residual-ORDER games
  taken by commit 2. Both clusters shrank by one without either mechanic being touched.
- **rb1395**, filed under `result random[16]@tripleaxel` (i.e. a prng-offset game), was also
  closed by commit 4 — confirming the `result`-class reading that the miscount lives in an
  EARLIER unit.

## The 68 still-open games, re-triaged at `51fed0b`

First-divergence CLASS split (from the 512 gate):

| n | class |
|---|-------|
| 37 | `draws-match/state-diff` |
| 9 | `result random[16]@…` — a draw miscount in an EARLIER unit |
| 2 each | `args randomChance@struggle`, `args randomChance@par`, `rust-extra randomChance@accuracy`, `PS shuffle@generic` |
| 1 each | `PS random@confusion`, `PS randomChance@heavyslam`, `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer` / `randomChance@freezedry` / `randomChance@icebeam` / `sample@trace`, `args randomChance@hypervoice` / `randomChance@powerwhip` / `shuffle@generic`, `rust-extra randomChance@crit` / `shuffle@disablemove` |

First-divergence FIELD split: **34 `hp`**, 10 `volatiles`, 9 `boosts` (4 atk / 3 def / 2 spe),
3 `species`, 2 `status_counter`, 2 `types`, 1 each `status` / `encore` / `ability` /
`pending_move` / `item` / `substitute_hp` / `last_berry` / `pp`.
Of the 34 `hp` games **26 exceed 10 HP**, 5 sit in 4-10, and 3 are within 3.

The 9 `result random[16]` games and the roll that first disagrees:
rb1021 d59(thunderbolt, rust=13), rb1057 d26(sludgewave, 9), rb1271 d11(hydropump, 9),
rb1277 d12(gigadrain, 14), rb1310 d18(outrage, 1), rb1329 d24(heatcrash, 1),
rb1343 d36(voltswitch, 8), rb1369 d46(knockoff, 14), rb1378 d8(ceaselessedge, 6).

The 10 `volatiles` games, decoded against `crates/engine/src/volatile.rs` (bit index = enum
discriminant) — every one is a SINGLE bit:

| game | bit | direction |
|------|-----|-----------|
| rb1033 | 1 `Substitute` | engine EXTRA (and `substitute_hp` 41 vs 0) |
| rb1048 | 39 `StatsLoweredThisTurn` | engine MISSING (p1 defog lowers the foe's evasion) |
| rb1072 | 29 `ThroatChop` | engine EXTRA |
| rb1099 | 24 `ChoiceLock` | on the WRONG SIDE — with the items also swapped; a Trick divergence, not a volatile one |
| rb1121 | 0 `Confusion` | engine EXTRA (`confusion_turns` 5 vs 0) |
| rb1126 | 28 `Unburden` | engine MISSING (PS ate the Sitrus; the engine still holds it) |
| rb1237 | 38 `StatsRaisedThisTurn` | engine MISSING (p1 swordsdance raises its own Atk) |
| rb1245 | 4 `Encore` | engine EXTRA (`encore` `(MoveId(420), 2)` vs none) |
| rb1278 | 39 `StatsLoweredThisTurn` | engine MISSING (p1 closecombat's SELF Def/SpD drop) |
| rb1304 | 30 `HealBlock` | engine EXTRA |

**Shared-root candidate surfaced by this decode: 3 of the 10 are the `stats*ThisTurn` pair
(rb1048, rb1237, rb1278) and all three are MISSING in the engine.** The three witnesses are a
foe-evasion drop (Defog), a self-boost by a status move (Swords Dance) and a move's SELF drop
(Close Combat) — i.e. the engine appears to set the flags only on some boost paths. PS sets
`statsRaisedThisTurn` / `statsLoweredThisTurn` inside `boostBy` / `Pokemon#boostBy`'s caller in
`sim/battle.ts`'s `boost()`, which every one of those paths goes through. This is the single
largest untried shared structure in the non-`hp` half.

Every open game, its first divergent unit, the move pair, the PS handlers its draws name, and
`[first-divergent-field, max |Δhp| in the block]`, sorted by hp gap:

```
  rb1024 d81 t73 p1:struggle p2:switch [hp 121]
  rb1126 d7 t5 p1:sludgebomb p2:strengthsap [volatiles 121]
  rb1387 d36 t32 p1:encore p2:freezedry [hp 119]
  rb1029 d22 t18 p1:gunkshot p2:swordsdance [hp 97]
  rb1369 d46 t42 p1:knockoff p2:bodyslam [hp 92]
  rb1103 d37 t32 p1:strengthsap p2:struggle [hp 86]
  rb1125 d2 t3 p1:icebeam p2:flowertrick/T [hp 79]
  rb1021 d59 t50 p1:thunderbolt p2:wish [hp 69]
  rb1348 d12 t11 p1:drainingkiss p2:outrage/T [hp 69]
  rb1231 d15 t12 p1:struggle p2:uturn [hp 66]
  rb1252 d21 t15 p1:powerwhip p2:partingshot [boost.atk 66]
  rb1011 d43 t33 p1:closecombat p2:switch [hp 63]
  rb1243 d11 t10 p1:stompingtantrum p2:knockoff eff=par ev=BeforeMove [hp 62]
  rb1362 d24 t20 p1:icebeam p2:thunderbolt eff=par ev=BeforeMove [hp 56]
  rb1184 d5 t6 p1:terastarstorm p2:tachyoncutter [hp 54]
  rb1372 d14 t13 p1:focusblast p2:surf [hp 49]
  rb1012 d60 t52 p1:gigadrain p2:scald eff=futuremove ev=End [hp 47]
  rb1034 d57 t46 p1:knockoff p2:triplearrows eff=par ev=BeforeMove [boost.def 35]
  rb1236 d37 t29 p1:dracometeor p2:voltswitch [hp 33]
  rb1048 d44 t33 p1:defog p2:thunderwave [volatiles 22]
  rb1300 d52 t48 p1:sleeptalk p2:focusblast eff=toxicchain ev=DamagingHit [hp 22]
  rb1326 d50 t40 p1:superfang p2:protect eff=stall ev=StallMove [substitute_hp 18]
  rb1030 d53 t46 p1:toxic p2:hypervoice eff=harvest ev=Residual [hp 17]
  rb1072 d27 t21 p1:earthquake p2:throatchop [volatiles 17]
  rb1116 d7 t6 p1:knockoff/T p2:closecombat [hp 17]
  rb1108 d4 t5 p1:shadowsneak p2:beakblast eff=cursedbody ev=DamagingHit [hp 16]
  rb1304 d16 t12 p1:psychicnoise p2:switch [volatiles 16]
  rb1315 d28 t26 p1:earthquake p2:knockoff eff=shedskin,toxicchain ev=DamagingHit,Residual [hp 16]
  rb1380 d15 t15 p1:scaleshot p2:toxic eff=shedskin ev=Residual [hp 16]
  rb1033 d44 t33 p1:substitute p2:discharge eff=confusion ev=BeforeMove [volatiles 15]
  rb1278 d42 t31 p1:closecombat p2:willowisp eff=poisontouch ev=DamagingHit [volatiles 15]
  rb1283 d17 t13 p1:knockoff p2:switch eff=trace ev=Update [hp 15]
  rb1057 d26 t25 p1:sludgewave p2:psychicnoise [hp 14]
  rb1378 d8 t8 p1:nastyplot p2:ceaselessedge/T [hp 14]
  rb1040 d2 t3 p1:stealthrock p2:earthpower/T [hp 13]
  rb1191 d17 t14 p1:thunderbolt p2:playrough [hp 8]
  rb1329 d24 t17 p1:heatcrash p2:earthquake [hp 8]
  rb1343 d36 t27 p1:voltswitch p2:kowtowcleave [hp 8]
  rb1061 d34 t27 p1:thunderwave p2:rapidspin eff=par ev=BeforeMove [hp 7]
  rb1121 d18 t15 p1:hurricane p2:uturn eff=toxicchain ev=DamagingHit [volatiles 6]
  rb1370 d3 t4 p1:hypervoice p2:thunderwave [status 5]
  rb1271 d11 t9 p1:switch p2:hydropump/T [hp 4]
  rb1277 d12 t9 p1:sludgebomb p2:gigadrain [hp 3]
  rb1391 d20 t15 p1:switch p2:heavyslam [hp 3]
  rb1345 d11 t9 p1:icebeam p2:icebeam [hp 2]
  rb1009 d4 t5 p1:rest p2:taunt [status_counter 0]
  rb1093 d22 t17 p1:icehammer p2:switch [boost.spe 0]
  rb1099 d57 t47 p1:trick p2:calmmind [volatiles 0]
  rb1119 d8 t7 p1:moonblast p2:sludgewave [types 0]
  rb1122 d5 t6 p1:shadowball p2:liquidation [boost.def 0]
  rb1147 d38 t29 p1:substitute p2:knockoff eff=substitute ev=TryPrimaryHit [boost.atk 0]
  rb1148 d36 t28 p1:poisonjab p2:superfang [encore 0]
  rb1233 d39 t32 p1:clangingscales p2:wish [boost.def 0]
  rb1237 d42 t30 p1:swordsdance p2:stealthrock [volatiles 0]
  rb1239 d34 t31 p1:roar p2:darkpulse [boost.spe 0]
  rb1244 d10 t7 p1:voltswitch/T p2:switch [ability 0]
  rb1245 d18 t15 p1:hypervoice p2:encore [volatiles 0]
  rb1250 d32 t29 p1:switch p2:switch [boost.atk 0]
  rb1253 d12 t10 p1:snowscape p2:bellydrum [species 0]
  rb1288 d11 t10 p1:surf p2:energyball [species 0]
  rb1299 d35 t30 p1:bulletseed p2:protect [boost.atk 0]
  rb1310 d18 t14 p1:dragondance p2:outrage eff=lockedmove ev=Start [pending_move 0]
  rb1314 d45 t38 p1:stickyweb p2:revivalblessing [item 0]
  rb1347 d61 t56 p1:trick p2:rest [last_berry 0]
  rb1356 d58 t50 p1:taunt p2:coil [status_counter 0]
  rb1359 d7 t7 p1:switch p2:calmmind [types 0]
  rb1360 d6 t6 p1:dragontail p2:dragontail/T [move3.pp 0]
  rb1367 d38 t31 p1:surf p2:energyball [species 0]
```

Recurrences to re-scan after every landing: **`knockoff` 7x** (rb1034 rb1116 rb1147 rb1243
rb1283 rb1315 rb1369 — up from 4, now the single largest move cluster), `struggle` 3x (rb1024
rb1103 rb1231), `icebeam` 3x (rb1125 rb1345 rb1362), `thunderbolt` 3x (rb1021 rb1191 rb1362),
`hypervoice` 3x (rb1030 rb1245 rb1370), `voltswitch` 3x (rb1236 rb1244 rb1343), `surf` 3x
(rb1288 rb1367 rb1372 — two of them are the Gulp Missile pair). Handler recurrences: `eff=par
ev=BeforeMove` 4x (rb1034 rb1061 rb1243 rb1362), `eff=toxicchain ev=DamagingHit` 3x (rb1121
rb1300 rb1315), `eff=shedskin ev=Residual` 2x (rb1315 rb1380).

## Named opens carried forward

- **Shed Skin is residual order 5/3** and still runs in the branching tail after Harvest (28/2).
  Named by commit 3 as its own known-remaining item: fixing it moves a DRAW, not just state, and
  needs the residual's deterministic tail to become branch-based first. Two witnesses
  (rb1315, rb1380).
- **The `struggle` cluster (rb1024, rb1103, rb1231) is REQUEST LEGALITY**, unchanged. rb1024 is
  now the largest single gap in the corpus (121 HP).
- **Gulp Missile (rb1288, rb1367)** and **Ice Face's RESTORE (rb1253)** — unchanged, and they are
  exactly the three `species` first-divergences.
- **Rampage BeforeMove-cancel at `n == 1` with a NON-confused user** — unchanged. rb1310 is still
  the `pending_move` game and still a `result random[16]@outrage` prng-offset game, not this.
- **The remaining `onDamagingHit` handlers still applied BEFORE the secondaries**:
  `apply_justified`, `apply_rattled`, `apply_thermal_exchange`, `apply_weak_armor` — unchanged
  from Phase 7. (Toxic Debris left this list at commit 1.)
- **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet
  Rise's `onTry` failure** — unchanged from Phase 5/6.
- **`DRAWCMP=1`'s "PS-unconsumed `shuffle[2,0,2]`" at a forced-replacement unit is a FALSE
  POSITIVE** — unchanged Phase-8 trap, still costs a probe cycle if forgotten.
- **Kill criterion: NOT triggered.** 1.57 games/commit over the tranche, above the 1/commit
  early-stop line; but two of the seven commits (1 and 3) flipped zero games, so the margin is
  thinner than in Phase 8 (2.0).

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 444 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== PHASE-8 EXTENSION BURN-DOWN — certification (2026-07-26) ====

**HEADLINE: 433 / 512 full games byte-exact from seed (84.6%), up from 425; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Four parity commits, every one PS-source-grounded. Judged by the exact-SET diff on BOTH corpora
at every step: **the newly-non-exact set was EMPTY at all four.**

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **433 / 512 (84.6%)**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | 12 suites, all green |

`convert.rs` was NOT touched, so the seed fixtures' baked digests did not move and no fixture
regeneration was needed. `gen.rs` WAS regenerated (two new `MoveData` fields, commit 3);
regenerating it from the pinned PS reproduces the previous file byte-for-byte apart from those
fields — verified by `diff` with the new fields stripped.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `791e33b` | **`runEvent('DamagingHit')` fires once per HIT, not once per move** — Stamina, Water Compaction, Rough Skin / Iron Barbs, Rocky Helmet, Electromorphosis, Seed Sower, Gooey / Tangling Hair. The stat one of them moves is visible to the NEXT hit's `getDamage` | 425 → 427 | `sim/battle-actions.ts:1142` inside the `hitStepMoveHitLoop` at `:874-970`; `data/abilities.ts:4471` `:5375` `:3893` `:2179` `:1164` `:4074` `:1597` `:4861`; `data/items.ts:5290` |
| 2 | `95cb229` | **Lum / Chesto are `onUpdate`**, so the post-hit `eachEvent('Update')` cures EITHER active — including a status a step-7 `onDamagingHit` ability put on the ATTACKER | 427 → 428 | `data/items.ts` lumberry / chestoberry + `sim/battle-actions.ts:970` |
| 3 | `f51de84` | **Pressure skips `foeSide` moves** (`pressureTargets = []`) unless `mustpressure` — so Spikes / Stealth Rock / Toxic Spikes ARE taxed and **Sticky Web is NOT** — and **Curse has already retargeted itself** to `self` for a non-Ghost user before the pressure resolution | 428 → 430 | `sim/pokemon.ts:853-860`; `sim/battle-actions.ts:429` (`ModifyMove`) vs `:467` (`getMoveTargets`); `data/moves.ts:3277` `:3304` |
| 4 | `a00fd66` | **`hitStepTryImmunity` precedes `hitStepAccuracy`** — Endeavor / Dream Eater / Synchronoise fail with NO accuracy roll — **plus a RESULT-level draw check in the gate's triage** | 430 → 433 | `sim/battle-actions.ts:560` vs `:563`; `data/moves.ts:4796` `:4260` `:18663` |

Games flipped, by commit: 1 → rb1155 rb1202; 2 → rb1204; 3 → rb1152 rb1377; 4 → rb1145 rb1223
rb1282.

## THE diagnostic that changed the taxonomy

`first_draw_mismatch` (seedgate.rs) compared each draw's KIND and ARGS and **never its RESULT**.
The gate drives the real PRNG through `replicate_select`, so a matching shape with a differing
result is proof that the engine entered the unit with its prng at a different OFFSET — a draw
MISCOUNT in an EARLIER unit that happened to leave the compared state alone. That is a
completely different bug class from a state-computation divergence, and it was invisible inside
`draws-match/state-diff`.

The check is deliberately restricted to the DAMAGE ROLL (`random(16)`). The engine's
`random(100)` secondary / self-drop draws log a canonical representative — a fork that cannot
land its effect collapses to a single "draw-and-discard" branch whose logged result is the
placeholder 0 — so comparing them flags 509 of 512 games, including every exact one. `random(16)`
is always the realized value.

**11 games reclassify** from `draws-match/state-diff` to `result random[16]@<move>`. Four of
them (rb1271 |Δ|=4, rb1277 3, rb1345 2, rb1391 3) were the games previously filed as the
"`|hp| <= 3` rounding residue" — **they are not rounding at all**, they are a damage roll taken
at the wrong prng offset. That whole sub-bucket is closed as a mislabel.

**Caveat, hard-won:** `DRAWCMP=1`'s "PS-unconsumed `shuffle[2,0,2]`" at any unit with a forced
replacement is a FALSE POSITIVE. The replacement bracket is consumed directly off `prng` in
`step_unit` and never enters `chosen_draws`, so the comparison always reports it missing. Do not
chase those (this cost a probe cycle: `replacement_bracket_tied` was true and the bracket had
already fired).

## The 79 still-open games, re-triaged

| n | class | reading |
|---|-------|---------|
| 48 | `draws-match/state-diff` | the draw stream matched for the unit AND the damage roll agreed; the STATE differs |
| 11 | `result random[16]@…` | **NEW** — a draw miscount in an EARLIER unit; the state divergence is downstream |
| 2 each | `PS shuffle@generic`, `args randomChance@hypervoice` / `@par` / `@struggle`, `rust-extra randomChance@accuracy` | draw-position offsets |
| 1 each | `PS random@confusion`, `PS randomChance@heavyslam`, `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer` / `randomChance@freezedry` / `randomChance@icebeam` / `sample@trace`, `args randomChance@powerwhip`, `args shuffle@generic`, `rust-extra randomChance@crit`, `rust-extra shuffle@disablemove` | bespoke |

First-divergence FIELD split: **45 `hp`**, 10 `volatiles`, 9 `boosts`, 3 `species`,
2 `status_counter`, 2 `types`, 2 `status`, 1 each `encore` / `ability` / `pending_move` / `item` /
`substitute_hp` / `pp`. Of the 45 `hp` games **35 exceed 10 HP**, 7 sit in 4-10, and **3 are
within 3** (down from 5 — and all three of those are now `result`-class, i.e. prng offset).

The 11 `result random[16]` games and where their damage roll first goes wrong:
rb1021 d59(thunderbolt), rb1271 d11(hydropump), rb1277 d12(gigadrain), rb1310 d18(outrage),
rb1329 d24(heatcrash), rb1343 d36(voltswitch), rb1369 d46(knockoff), rb1378 d8(ceaselessedge),
rb1395 d10(tripleaxel), plus rb1145/rb1282 which the commit-4 fix closed.
Two of them have a CLEAN preceding shape mismatch to start from:
- **rb1343 d34**: `rust-extra random[100]@secondary` — the engine rolls a secondary PS does not
  (PS: `…@bugbuzz ×4, @flamethrower ×3`; the engine appended a 4th `random[100]` to flamethrower).
- **rb1029 d22 / rb1348 d12**: PS records **zero draws for the whole unit** where the engine
  rolls accuracy+crit+damage. Same shape as the Endeavor root that commit 4 closed — find which
  pre-accuracy PS step failed (rb1348: `drainingkiss` vs a Tera'd `outrage` user).

The 10 `volatiles` games are unchanged single bits: Substitute (rb1033 extra),
StatsLoweredThisTurn (rb1048, rb1278 missing; rb1233 extra), ThroatChop (rb1072 extra),
ChoiceLock (rb1099 missing), Confusion (rb1121 extra), Unburden (rb1126 missing),
StatsRaisedThisTurn (rb1237 missing), Encore (rb1245 extra), HealBlock (rb1304 extra).

The 44 open units whose divergent state carries an hp gap over 10 HP (35 of them with `hp` as the
FIRST divergent field) — unit, move pair, the PS handler its draws name, |Δhp|. `eff=`/`ev=` are
the union over the unit's recorded draws and are what surfaced the `par@BeforeMove`,
`harvest@Residual`, `flamebody`/`cursedbody@DamagingHit` and `shedskin@Residual` clusters:

```
  rb1003 d34 t27 p1:hydropump p2:waterpulse [152]
  rb1126 d7 t5 p1:sludgebomb p2:strengthsap [121]
  rb1387 d36 t32 p1:encore p2:freezedry [119]
  rb1198 d29 t20 p1:lavaplume p2:tripleaxel eff=flamebody ev=DamagingHit [114]
  rb1369 d46 t42 p1:knockoff p2:bodyslam [92]
  rb1103 d37 t32 p1:strengthsap p2:struggle [86]
  rb1227 d39 t32 p1:substitute p2:scald eff=harvest ev=Residual [72]
  rb1016 d23 t20 p1:ragefist p2:iciclespear [72]
  rb1021 d59 t50 p1:thunderbolt p2:wish [69]
  rb1252 d21 t15 p1:powerwhip p2:partingshot [66]
  rb1231 d15 t12 p1:struggle p2:uturn [66]
  rb1011 d43 t33 p1:closecombat p2:switch [63]
  rb1243 d11 t10 p1:stompingtantrum p2:knockoff eff=par ev=BeforeMove [62]
  rb1024 d81 t73 p1:struggle p2:switch [58]
  rb1184 d5 t6 p1:terastarstorm p2:tachyoncutter [54]
  rb1057 d11 t11 p1:protect p2:switch [53]
  rb1348 d12 t11 p1:drainingkiss p2:outrage/T [52]
  rb1372 d14 t13 p1:focusblast p2:surf [49]
  rb1302 d7 t6 p1:tailslap p2:stealthrock eff=flamebody ev=DamagingHit [48]
  rb1012 d60 t52 p1:gigadrain p2:scald eff=futuremove ev=End [47]
  rb1280 d13 t13 p1:switch p2:photongeyser eff=harvest ev=Residual [35]
  rb1236 d37 t29 p1:dracometeor p2:voltswitch [33]
  rb1226 d29 t26 p1:yawn p2:toxic eff=par ev=BeforeMove [30]
  rb1209 d28 t22 p1:leechseed p2:flipturn eff=par ev=BeforeMove [30]
  rb1125 d2 t3 p1:icebeam p2:flowertrick/T [27]
  rb1029 d22 t18 p1:gunkshot p2:swordsdance [24]
  rb1300 d52 t48 p1:sleeptalk p2:focusblast eff=toxicchain ev=DamagingHit [22]
  rb1274 d15 t10 p1:lavaplume p2:psychicnoise [22]
  rb1048 d44 t33 p1:defog p2:thunderwave [22]
  rb1362 d24 t20 p1:icebeam p2:thunderbolt eff=par ev=BeforeMove [18]
  rb1326 d50 t40 p1:superfang p2:protect eff=stall ev=StallMove [18]
  rb1116 d7 t6 p1:knockoff/T p2:closecombat [17]
  rb1072 d27 t21 p1:earthquake p2:throatchop [17]
  rb1030 d53 t46 p1:toxic p2:hypervoice eff=harvest ev=Residual [17]
  rb1380 d15 t15 p1:scaleshot p2:toxic eff=shedskin ev=Residual [16]
  rb1315 d28 t26 p1:earthquake p2:knockoff eff=toxicchain,shedskin ev=DamagingHit,Residual [16]
  rb1304 d16 t12 p1:psychicnoise p2:switch [16]
  rb1108 d4 t5 p1:shadowsneak p2:beakblast eff=cursedbody ev=DamagingHit [16]
  rb1283 d17 t13 p1:knockoff p2:switch eff=trace ev=Update [15]
  rb1278 d42 t31 p1:closecombat p2:willowisp eff=poisontouch ev=DamagingHit [15]
  rb1033 d44 t33 p1:substitute p2:discharge eff=confusion ev=BeforeMove [15]
  rb1378 d8 t8 p1:nastyplot p2:ceaselessedge/T [14]
  rb1040 d2 t3 p1:stealthrock p2:earthpower/T [13]
  rb1034 d57 t46 p1:knockoff p2:triplearrows eff=par ev=BeforeMove [11]
```

Recurrences worth a scan after every landing: `knockoff` 4x (rb1116 rb1283 rb1315 rb1369),
`struggle` 3x (rb1024 rb1103 rb1231), `eff=par ev=BeforeMove` 5x (rb1209 rb1226 rb1243 rb1362
rb1061), `eff=harvest ev=Residual` 3x (rb1030 rb1227 rb1280), `eff=cursedbody` 2x (rb1108
rb1229), `eff=flamebody` 2x (rb1198 rb1302).

## Named opens carried forward

- **`hitStepTryImmunity`'s STATUS half was already complete** — `status_try_immunity_fails`
  (generate.rs) covers leechseed / attract / captivate / trick / switcheroo / worryseed /
  octolock. Commit 4 added the DAMAGING half. That set is now closed.
- **Toxic Debris is still applied once per move** and outside the `any_damage` gate, while PS's
  is an `onDamagingHit` (`data/abilities.ts:5061`) that caps at 2 layers. No witness, so it was
  left in `apply_post_damage`; a 2-hit physical move into Glimmora/Grimmsnarl is the probe.
- **Rampage BeforeMove-cancel at `n == 1` with a NON-confused user** — unchanged and still open.
  rb1310, the one game with a `pending_move` first divergence, is NOT that case: it is a
  `result random[16]@outrage` prng-offset game (its `Rampaging(_, 2)` vs `(_, 1)` is the engine
  having selected the `random(2,4)=3` branch off a drifted prng, not a residual-tick bug — the
  EOT decrement was verified firing with a probe).
- **The `struggle` cluster (rb1024, rb1103, rb1231) is REQUEST LEGALITY**, unchanged.
- **Gulp Missile (rb1288, rb1367)** and **Ice Face's RESTORE (rb1253)** — unchanged, 3 games.
- **The remaining `onDamagingHit` handlers still applied BEFORE the secondaries**:
  `apply_justified`, `apply_rattled`, `apply_thermal_exchange`, `apply_weak_armor` — unchanged.
- **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet
  Rise's `onTry` failure** — unchanged from Phase 5/6.
- **Kill criterion: still NEVER triggered.** Four commits, four distinct structured roots plus
  one diagnostic, 8 games; 2.0 games/commit, well above the 1/commit early-stop line.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 433 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

---

# ==== PHASE-7 EXTENSION BURN-DOWN — certification (2026-07-26) ====

**HEADLINE: 425 / 512 full games byte-exact from seed (83.0%), up from 400; init-aligned
512 / 512. The audited 111-trace corpus stayed 111 / 111 at EVERY step.**

Eight parity commits, every one PS-source-grounded. Judged by the exact-SET diff on BOTH
corpora at every step: the newly-non-exact set was EMPTY at all eight. (It was NOT empty at one
intermediate step — see "Alluring Voice" below — and that regression named the next root.)

## Final gate numbers (re-run at the certifying commit)

| gate | command | result |
|------|---------|--------|
| Seed gate, audited 111 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz` | **111 / 111 exact (100%)** |
| Seed gate, 512 | `SEED_GATE=1 cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz` | **425 / 512 (83.0%)**; init-aligned **512 / 512** |
| Draw-consumption differ | `DRAW_DIFF=1 cosim harness/cosim-traces/*.json.gz` | **3812 / 3831 = 99.50%**; **zero `rust extra`** |
| State sweep (mechanics rail) | `cosim harness/cosim-traces/*.json.gz` | **3831 / 3831 matched**, 0 diverged, **0 unsupported** |
| Distribution smoke | `bash harness/run-distribution-smoke.sh` | **18 / 18** |
| Exporter round-trip | `ROUNDTRIP_GATE=1 cosim …` | **PASS** |
| Engine tests | `cargo test --release -p engine -j 2` | all suites green |

`convert.rs` was NOT touched, so the seed fixtures' baked digests did not move and no fixture
regeneration was needed. `gen.rs` WAS regenerated (two new `MoveData` fields); regenerating it
from the pinned PS reproduces the previous file byte-for-byte apart from the new fields —
verified by diff before the fields were added.

## The roots landed (in commit order)

| # | commit | class | games | PS reference |
|---|--------|-------|-------|--------------|
| 1 | `7bf572f` | **The last six `onBasePower` `chainModify` handlers folded into the one `event.modifier`** — Collision Course / Electro Drift, Psyblade, Expanding Force, the `-ate` abilities, Analytic | 400 (latent) | `data/moves.ts:2633-2637` `:4619-4623` `:14038-14042` `:4952-4956`; `data/abilities.ts:3263-3266` `:110-125` |
| 2 | `f9b3192` | **A move's `volatileStatus` does not mean it targets the foe** — Protect / Substitute / absorbing abilities / Prankster gated on `MoveTarget` | 400 → 403 | `data/moves.ts` substitute `onTryPrimaryHit` (`target === source`); `sim/battle-actions.ts:671-673` |
| 3 | `b1f063a` | **PS never drops `lockedmove` on a move FAILURE** — `onAfterMove` removes it at `duration === 1`, so a failed rampage on the final locked turn still confuses | 403 → 405 | `sim/battle-actions.ts:311-312` + `data/conditions.ts:253-284` + `data/moves.ts:1015-1020` |
| 3b | `b1f063a` | **The rampage START draw sits at the SELF-DROP position** (`self: {volatileStatus:'lockedmove'}` is applied by `selfDrops`, before secondaries and DamagingHit) | 405 → 407 | `sim/battle-actions.ts:1117` + `data/conditions.ts:264-267` |
| 4 | `af31239` | **`AfterEachBoost` fires once per CHANGED stat, not once per boost event** — a two-stat drop wakes Defiant / Competitive TWICE | 407 → 409 | `sim/battle.ts:2073` + `data/abilities.ts:891-905` / `:635-649` |
| 5 | `66594aa` | **Flinch is a SECONDARY** — rolled at step 5, before the `DamagingHit` ability rolls at step 7 | 409 → 410 | `sim/battle-actions.ts:1120` vs the `damagedTargets` `runEvent('DamagingHit')` below it |
| 5b | `66594aa` | **Protect / Substitute key on `move.target` + the `protect` FLAG**, not on the codegen-visible payload | 410 → 416 | `sim/battle.ts:1300-1308` (`checkMoveBypassesProtect`) |
| 6 | `e3d3829` | **The recharge turn**: `mustrecharge` volatile lockstep with `pending_move`, PS's explicit `removeVolatile('truant')`, and the gate's missing `"recharge"` pseudo-move | 416 → 418 | `data/conditions.ts:364-373`; `crates/cosim/src/convert.rs:536-539` |
| 7 | `63610ab` | **An Ice Face / Disguise nullification still runs the move's secondaries** — `onDamage` returns 0, a NUMBER, so the target stays live | 418 → 423 | `data/abilities.ts:960-968`; `sim/battle-actions.ts:1127-1129` |
| 8 | `669a727` | **Alluring Voice's conditional confusion** + **Weakness Policy is `onDamagingHit`** (step 7, after `secondaries()`) | 423 → 425 | `data/moves.ts` alluringvoice; `data/items.ts:7591-7605` |

Games flipped, by commit: 2 → rb1109 rb1142 rb1308; 3 → rb1098 rb1384; 3b → rb1031 rb1321;
4 → rb1102 rb1211; 5 → rb1392; 5b → rb1059 rb1066 rb1190 rb1214 rb1287 rb1307; 6 → rb1092
rb1157; 7 → rb1038 rb1135 rb1143 rb1279 rb1371; 8 → rb1052 rb1364.

### Method notes worth keeping

- **`spreadMoveHit`'s numbered steps are the spine of the draw order.** Three of this phase's
  roots were "the engine ran X at the wrong step": flinch (a step-5 secondary) after the step-7
  DamagingHit abilities; the rampage lock (a step-4 `self` effect) at the end of the move;
  Weakness Policy (step 7) before the step-5 secondaries. When a `@move` draw and an `@ability`
  draw swap places, look up the step numbers before looking for a missing mechanic.
- **The codegen's `MoveData` payload is not PS's targeting.** `target_volatile` is `gen-data.mjs`
  folding `move.volatileStatus`, which SELF-targeting moves carry too (Protect, Substitute,
  Magnet Rise, Destiny Bond, …), and it is ABSENT for every move whose foe-facing effect is an
  `onHit` callback (Strength Sap, Trick, Pain Split, Topsy-Turvy). Any predicate that means
  "does this reach the foe's mon?" must read `md.target`.
- **A regression is a lead.** Landing Alluring Voice's confusion cost rb1178, and that one game
  named the Weakness Policy ordering bug. Never revert on a count; read the lost game.
- **The `stall` / `queue.willAct()` lead was WRONG.** rb1227's Protect never reached
  `execute_status_move` at all — a Substitute from three turns earlier was up and the engine
  read Protect as a foe-targeting volatile move. Single-field probes tell you the SYMPTOM, not
  the site; instrument the return path before theorising about the predicate.

## The 87 still-open games, re-triaged

| n | class | reading |
|---|-------|---------|
| 64 | `draws-match/state-diff` | the draw stream matches for the unit; the STATE differs |
| 2 | `PS shuffle@generic` | a residual-handler-list tie shuffle the engine does not emit |
| 2 each | `args randomChance@hypervoice` / `@par` / `@struggle`, `rust-extra randomChance@accuracy` | draw-position offsets |
| 1 each | `PS random@confusion`, `PS randomChance@heavyslam`, `PS randomChance@struggle`, `PS shuffle@thunderbolt`, `PS-unconsumed random@icehammer`, `PS-unconsumed randomChance@freezedry`, `PS-unconsumed randomChance@icebeam`, `PS-unconsumed sample@trace`, `args randomChance@knockoff`, `args randomChance@powerwhip`, `args shuffle@generic`, `rust-extra randomChance@crit`, `rust-extra shuffle@disablemove` | bespoke |

First-divergence FIELD split: **48 `hp`**, 10 `volatiles`, 9 `boosts`, 3 `status`, 3 `species`,
2 `status_counter`, 2 `types`, 4 `pp`, tail. Of the 48 `hp` games **35 exceed 10 HP** (wrong
mechanics), 8 sit in 4-10, and **5 are within 3** (rounding residue — down from 7).

The 10 `volatiles` games are now single-bit, no shared root visible: Substitute (rb1033 extra),
StatsLoweredThisTurn (rb1048, rb1278 missing), ThroatChop (rb1072 extra), ChoiceLock (rb1099
missing), Confusion (rb1121 extra), Unburden (rb1126 missing), StatsRaisedThisTurn (rb1237
missing), Encore (rb1245 extra), HealBlock (rb1304 extra). **Note the four
`statsRaisedThisTurn` / `statsLoweredThisTurn` bits** — PS clears them in `nextTurn`
(`sim/battle.ts:1675`) and on switch-out (`sim/battle-actions.ts:123`); they are cheap to
re-check and they gate Burning Jealousy, Lash Out and Alluring Voice.

The 35 `hp > 10` games, with the divergent unit's move pair:
rb1003 d34(hydropump+waterpulse), rb1011 d43(closecombat+SW), rb1012 d60(gigadrain+scald),
rb1016 d23(ragefist+iciclespear), rb1021 d59(thunderbolt+wish), rb1024 d81(struggle+SW),
rb1029 d22(gunkshot+swordsdance), rb1030 d53(toxic+hypervoice), rb1040 d2(stealthrock+earthpower),
rb1057 d11(protect+SW), rb1103 d37(strengthsap+struggle), rb1108 d4(shadowsneak+beakblast),
rb1116 d7(knockoff+closecombat), rb1125 d2(icebeam+flowertrick),
rb1184 d5(terastarstorm+tachyoncutter), rb1198 d29(lavaplume+tripleaxel),
rb1209 d28(leechseed+flipturn), rb1226 d29(yawn+toxic), rb1227 d39(substitute+scald),
rb1231 d15(struggle+uturn), rb1236 d37(dracometeor+voltswitch),
rb1243 d11(stompingtantrum+knockoff), rb1274 d15(lavaplume+psychicnoise),
rb1280 d13(SW+photongeyser), rb1283 d17(knockoff+SW), rb1300 d52(sleeptalk+focusblast),
rb1302 d7(tailslap+stealthrock), rb1315 d28(earthquake+knockoff), rb1348 d12(drainingkiss+outrage),
rb1362 d24(icebeam+thunderbolt), rb1369 d46(knockoff+bodyslam), rb1372 d14(focusblast+surf),
rb1378 d8(nastyplot+ceaselessedge), rb1380 d15(scaleshot+toxic), rb1387 d36(encore+freezedry).
`knockoff` still recurs 5x (rb1116, rb1243, rb1283, rb1315, rb1369) and `struggle` 3x.

## Named opens carried forward

- **The `struggle` cluster (rb1024, rb1103, rb1231) is a REQUEST-LEGALITY divergence, not a
  mechanics one.** rb1231 d15: PS resolves p1's "move 1" to `struggle` while the engine's
  `move1` still had PP (`s0#3.move1.pp engine=5 ps=6`), so the engine used the real move and
  the whole unit's draw stream shifted. Start from `check_legality` in `crates/cosim/src/replay.rs`
  — PS's request JSON is the ground truth and is recorded in the sidecar.
- **Rampage lock end at `n == 1` with a NON-confused user, CANCELLED by a BeforeMove handler**
  (attract / full paralysis / freeze). `runMove` returns before `useMove`, so there is no
  `onAfterMove`; the volatile survives to the residual loop, whose `duration` 1 → 0 ends it
  there (`sim/battle.ts:515-522`), putting the `random(2, 6)` at the RESIDUAL stream position.
  `unarm_rampage_on_cancel` still leaves that case alone. This is the one PHASE-6 named open
  that is genuinely still open — the other rampage leads all landed as commit 3/3b.
- **Gulp Missile (rb1288, rb1367)** — `engine=cramorant ps=cramorantgorging`; the Surf/Dive
  forme change (`gulpmissile.onSourceTryPrimaryHit`) and its retaliation are unmodelled.
  **Ice Face's RESTORE (rb1253)** — `iceface.onStart` / `onWeatherChange` turn Eiscue-Noice back
  into Eiscue under snow. Both are forme mechanics; 3 games.
- **The remaining `onDamagingHit` handlers are still applied BEFORE the secondaries**:
  `apply_justified`, `apply_rattled`, `apply_thermal_exchange`, `apply_weak_armor`. Only
  Weakness Policy was moved (it was the one with a witness). Each is a `runEvent('DamagingHit')`
  handler and belongs beside `apply_contact_secondaries` / `apply_cursed_body`.
- **Terapagos-Stellar's FAINT regression**, **Battle Bond's once-per-stint guard**, **Magnet
  Rise's `onTry` failure** — unchanged from Phase 5/6.
- **Kill criterion: still NEVER triggered.** Eight commits, ten distinct structured roots,
  25 games; density did not decay within the session.

## Extended CI gate

8. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz harness/seed-fixtures/*.fx.json.gz`
   — **must stay >= 425 / 512**, and the non-exact SET must be a subset of the previous one.
9. `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz` — **must stay 111 / 111.**

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
