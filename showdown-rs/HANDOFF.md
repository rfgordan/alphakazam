# Deep-Showdown Handoff: Divergence Burn-Down

**State as of 2026-06-12 (commit 4811b49):** 1532 cosim units, **1494 matched / 38 diverged / 0 unsupported** → EXACTNESS 97.52%, COVERAGE 100%. The OU corpus (`c*.json.gz`, 389 units) is at **100.00% and must stay there** — it is the regression gate. Campaign so far: 237 → 122 → 82 → 70 → 53 → 46 → 44 → 40 → 38.

**The directive:** keep grinding the 38 remaining divergences to zero, one cluster at a time, committing each verified batch.

---

## 1. What this project is

A clean, RL-oriented Rust Pokémon engine (gen9) verified to compute *the same function* as Pokémon Showdown. PS is pinned at commit `b9dc987d344635789116ae46c48f8e2480e0ddc2` (lockfile + check script in `harness/`; `engines/` is gitignored — clone PS there if missing).

## 2. Architecture

### Engine (`crates/engine/`)
- `state.rs` — flat `Copy` `State`: two `Side`s, six `Pokemon` each, field/side conditions. Notable fields added during this campaign: `Side.throat_chop_turns/heal_block_turns/healing_wish`, `State.sleep_clause` (true only for randombattle formats), `Pokemon.transformed/base_species/base_stats/base_moves/slept_by_foe/last_berry`.
- `instruction.rs` — reversible `Instruction` enum (apply/reverse arms). Recent additions: `Transform` (max-HP-aware for Terapagos), `SetHealingWish`, `SetSleptByFoe`, `SetLastBerry`, `ActiveCounter::{ThroatChop, HealBlock}`.
- `generate.rs` — the big one. `generate_instructions_ex(state, s1, s2, pivot, tera)` returns `Vec<Branch>` where `Branch { prob, state, ins }`; `push()` applies an instruction and records it. Branching covers crits, damage rolls, secondaries, confusion durations, etc. `apply_end_of_turn` returns `Vec<Branch>` with ordered expansion stages: **Harvest → Shed Skin → Yawn → Future Sight**. `switch_into_pair` handles simultaneous double switch-ins with PS event semantics (both enter + hazards in speed order, *then* switch-in abilities fire in speed order — so Intimidate sees the other fresh switch-in).
- `gen.rs` — generated, do not hand-edit. Regenerate with `node harness/gen-data.mjs` after editing that script. MoveData carries flags incl. `flag_heal/flag_powder/flag_bypass_sub`; `MANUAL_TARGET_BOOSTS` override map; confusion is deliberately excluded from the 100%-chance targetVol fold so it stays a *branching* secondary (duration 2–5).
- `examples/` — throwaway empirical-repro programs (e.g. `conf_test.rs`). Build small State fixtures, run generate, print branches. Note: the dex subset is randbats-oriented — `machamp` is absent; use species that appear in `gen.rs` (e.g. golurk).

### Co-sim verifier (`crates/cosim/`)
PS-led recorded games: each unit is one decision point with the full PS `State.serializeBattle` pre/post snapshots, the resolved choices (with rosterIndex), and **labeled PRNG draws** (kind/args/result/effect/event, e.g. `randomChance[33,100]=true[shedskin/Residual]`).
- `convert.rs` — PS serialized JSON → engine `State`. Full of hard-won PS serialization conventions (see §4).
- `replay.rs` — converts the pre-state, runs the engine's branch enumeration with the recorded choices, and checks whether **any branch exactly matches** the converted post-state. Match → unit matched; no match → diverged (diff vs closest branch is printed). `sleep_clause` is set from `trace.format.contains("randombattle")`.
- `diff.rs` — exact full-field comparison (base_* fields deliberately not compared).
- `main.rs` — reporting; `VERBOSE=1` prints per-unit diffs + choice summaries; `DRAWS=1` additionally prints PS's labeled draws for diverged units.

### Corpora (`harness/cosim-traces/`)
- `c1–c8` — OU customgame, 389 units. **Regression gate: 100.00% always.**
- `d1–d8` — directed customgame scenarios.
- `r1–r20` — gen9randombattle (where all 38 remaining divergences live, except 5 in d*).

## 3. The validation loop (the grind procedure)

```bash
cd showdown-rs

# 0) Always after any change:
cargo test --release -p engine            # 10 + 2 tests must pass

# 1) Full sweep + divergence list:
VERBOSE=1 cargo run --release -p cosim -- harness/cosim-traces/*.json.gz 2>/dev/null | grep -iE "diverg|EXACTNESS"

# 2) PS's labeled PRNG draws for a diverged unit (often names the unmodeled effect directly):
DRAWS=1 VERBOSE=1 cargo run --release -p cosim -- harness/cosim-traces/r5.json.gz

# 3) Dump raw trace JSON for one unit (find hidden state: items, abilities, volatiles, statusState):
node -e 'const z=require("zlib"),fs=require("fs");
const t=JSON.parse(z.gunzipSync(fs.readFileSync("harness/cosim-traces/r5.json.gz")));
const u=t.units.find(u=>u.turn===3); console.log(JSON.stringify(u,null,1).slice(0,20000))'
# (field names may differ slightly — print Object.keys(t) / Object.keys(u) first)

# 4) Empirical repro: write crates/engine/examples/foo.rs building the exact fixture,
#    cargo run --release -p engine --example foo, compare branch set to PS post-state.

# 5) Cross-check PS source at the PIN (not upstream master!):
#    engines/pokemon-showdown @ b9dc987d... — grep sim/, data/moves.ts, data/abilities.ts, data/items.ts
```

**Per-cluster cycle:** pick a cluster → dump trace + draws → form hypothesis → check PS source at the pin → implement → `cargo test` → full sweep → confirm cluster cleared, count dropped, **c\* still 100.00%** → commit as `parity: <what> — N -> M`.

**Final checks when count hits 0:** run `harness/run-mutations.sh` (mutation kill suite — damage-path code changed a lot, the verifier must still catch injected bugs), and update the project memory file.

## 4. PS serialization conventions already decoded (converter knowledge)

- stall counter serializes as 3^n (engine stores n; converter does `log3.round()`).
- wish slotConditions are an **array** of per-slot objects with `startingTurn` + fractional `hp`, no duration; PS heals at the first residual with a live occupant → converter always maps a present wish to remaining=1; engine lingers when the slot occupant fainted; a second Wish fails while pending.
- `lastMove` is `{move: "[Move:id]", hit, ...}`; species of transformed mons come from the `species` "[Species:x]" ref, not `details`.
- lockedmove volatile: `duration` resets to 2 per use; `trueDuration` is the real remaining → `PendingMove::Rampaging(mv, trueDuration.max(1))`.
- confusion serializes `time` = rolled duration 2–5.
- sleep `statusState` carries `source`/`target` "[Pokemon:p1a]" refs → `slept_by_foe` (Sleep Clause only counts foe-induced sleep, and only in randombattle).
- `abilityState.libero/protean` marks Protean already used this switch-in → `TypeShifted` volatile.
- `lastItem` + `ateBerry` → `last_berry` (Harvest).
- PS taunt duration: 3, +1 only if `target.activeTurns` truthy && !willMove (fresh switch-ins get 3).
- Protect fails outright when `queue.willAct()` is false (no action after it) and the stall counter resets.

## 5. The 38 remaining divergences (fresh list, commit 4811b49)

Grouped by suspected cause. Format: `file tN | choice summary | diff`.

### A. Damage-formula tails (one-off modifier wrong or missing) — likely several distinct small bugs
1. `r1 t16` p1:psyblade p2:hypervoice | s0#5.hp engine=204 ps=157 — Psyblade/terrain or spread-mod interplay.
2. `r19 t18` liquidation vs surgingstrikes | s1#5.hp 103 vs 89 — Surging Strikes is always-crit multihit; check crit interaction with the defender's mods (Multiscale?).
3. `r6 t18` doubleedge vs liquidation | s1#0.hp 58 vs 129 — large; dump trace (ability/item on defender).
4. `r3 t2` collisioncourse vs stoneedge | s1#0.hp 188 vs 155 — Collision Course ×5461/4096 was added; maybe ordering vs other chainmods, or the *other* mon's damage.
5. `r3 t19` flareblitz vs destinybond | s1#3.hp 46 vs 48 — recoil ±2 (recoil rounding: PS uses `this.clampIntRange(Math.round(damage/3),1)` on *damage dealt incl. sub?*).
6. `r15 t20` acrobatics vs earthquake | s1#5.hp 190 vs 189 — ±1 rounding.
7. `r8 t18` ivycudgel vs photongeyser | s1#2.hp 189 vs 186 — ±3; Photon Geyser category by boosted stat? Ivy Cudgel mask forme type?
8. `r11 t6` thunderbolt vs thunderwave, Bellibolt | s0#0.hp 212 vs 246 — Bellibolt has **Electromorphosis** (Charge when hit) — likely PS's Charge doubles a later Electric hit, or LO recoil diff. Dump trace.
9. `r2 t38` protect vs sacredfire, Necrozma-Dusk-Mane | s0#0.hp 243 vs 272 — Prism Armor reducing super-effective damage?
10. `r4 t30` uturn vs calmmind, Snorlax | s1#2.hp 221 vs 251 — 30 hp; check defender item/ability in trace.
11. `r12 t35` Pincurchin switch-in vs stompingtantrum | s0#2.hp 156 vs 62 — engine deals far less; Stomping Tantrum doubles after a failed move? Electric Surge interplay?
12. `r2 t23` psychicnoise vs tripleaxel, Entei | s0#0.hp 138 vs 216 — Psychic Noise heal-block vs something; 2.1M branches — also check Triple Axel branch explosion sanity.
13. `r16 t17` stickyweb vs leafstorm | both sides' hp off by 36/16 — possibly hazard + Contrary/White Herb chain, or wrong target of Sticky Web boost-drop.

### B. Stored Power chain (same game, Polteageist) — one cluster
14. `r5 t3` storedpower vs rapidspin | s1#0.hp 77 vs 10.
15. `r5 t27` storedpower vs terablast | s1#1.hp 251 vs 224.
   Stored Power BP = 20+20×positive stages was added — check which stages PS counts (incl. from White Herb timing, Stockpile?) and whether the *user's* boosts at damage time differ. Dump the Polteageist boost table from the trace.

### C. Substitute interactions
16. `d1 t21` substitute vs bulletseed | s0#5.hp engine=0 ps=312 — engine kills *through* the sub on multihit; PS stops remaining hits at sub break? No — PS continues hitting the mon? Actually PS: each hit of a multihit hits the sub until it breaks, remaining hits hit... (gen5+: remaining hits do hit the pokemon, BUT this diff says ps=312 i.e. mon untouched → at this gen PS stops? Verify in pinned `sim/battle-actions.ts` hitStepMoveHitLoop + getDamage vs sub).
17. `r12 t27` substitute vs struggle | engine sub gone, ps sub at 15 hp — Struggle vs sub: recoil basis / sub damage wrong.
18. `r12 t28` shadowball vs struggle, Dusknoir | s1#3.species engine=Species(591) ps=Species(188) — species mismatch on bench slot; almost certainly an Illusion or forme/convert bug, not battle logic. Dump trace slot 3.

### D. Volatile/status bookkeeping
19. `r10 t29` + 20. `r10 t43` drainingkiss/roost vs malignantchain | engine missing confusion (confusion_turns 0 vs 4) — Malignant Chain's 50% is *toxic chain* → badly poisoned, not confusion… but PS shows confusion: dump trace; maybe the mon has a confusion-inducing item/ability (berserk gene?) or it's leftover from the codegen confusion-fold fix not covering this path (secondary chance 50?).
21. `r17 t29` throatchop vs irondefense | engine has volatile bit 2^29 set, PS none — Throat Chop volatile applied when PS blocked it (Shield Dust/Covert Cloak handled — check this target) or wrong duration expiry.
22. `r18 t21` swordsdance, Urshifu-Rapid-Strike benched | s1#3.types engine=[Dark,None] ps=[Water,Dark] — **Protean/Libero type not reverting on switch-out** (or converter applying TypeShifted types to a benched mon). Likely quick fix: restore base types in `apply_switch`.
23. `r9 t43` outrage | s1.wish engine=(1,165) ps=(0,0) and 24. `r9 t45` outrage vs wish | engine still Rampaging+LockedMove, PS none — same game: wish consumed/expired differently AND rampage ended in PS (move failed → rampage breaks? Wish heal target fainted?). The two are probably one causal chain; replay that game's turns 43–45.

### E. Intimidate / switch-in ordering edges
25. `r2 t15` Flareon switch vs Arcanine | s0.boost.atk engine=-1 ps=0 — Intimidate fired in engine, not in PS (or PS's was blocked: Inner Focus? Oblivious? Own Tempo? Guard Dog reverses!). Check Flareon's ability in trace.
26. `r2 t30` Qwilfish-Hisui switch vs Arcanine | same pattern, same game.
27. `r1 t9` Squawkabilly-White vs Vikavolt double replacement | s0#1/s1#5 hp diffs of 18/16 — switch-in damage (hazards? Spiky Shield residue?) on simultaneous replacement; `switch_into_pair` landed but this unit still diverges — re-dump.
28. `d1 t46`, 29. `d6 t43` sludgebomb units | s0.active_turns off by one (engine high) — active_turns increment on a turn the mon was switched/dragged? Compare PS `activeTurns` semantics at residual vs start-of-turn.
30. `d8 t50` gigadrain/hydropump + Grimmsnarl replacement | s1.active_turns 2 vs 1 — same family.

### F. Species/forme mysteries
31. `r2 t27` Empoleon vs sacredfire, Qwilfish-Hisui benched | s0#3.species engine=559 ps=560 — engine has Qwilfish-Hisui (559?) PS has 560; converter forme-mapping off by one for regional forme after... something. Check what Species(559)/(560) are in gen.rs and what the trace's `details`/`species` refs say.
32. (same family as #18 — species 591 vs 188.)

### G. PP off-by-ones (Pressure)
33. `r11 t22` earthquake vs poltergeist | s1#3.move1.pp engine=1 ps=2 — engine deducted one extra (double Pressure application? Pressure on failed/immune move?).

### H. Tera mechanics
34. `r18 t10` freezedry vs terastarstorm | s1#2.hp 194 vs 238 — Tera Stellar: Tera Starstorm BP/type vs non-Terapagos? Stellar one-time ×2/×1.2 boost bookkeeping.

### I. Sturdy / endure-like
35. `d6 t52` Skarmory vs overdrive | s0#3.hp engine=1 ps=14 — engine triggered Sturdy (left at 1?) but PS shows 14 — i.e. damage roll difference, not Sturdy; or engine's Sturdy clamp applied when damage wouldn't have KO'd.

### J. Berry/heal timing
36. `r17 t6`, 37. `r17 t7` icepunch vs drainingkiss | hp ±8/±12 — Draining Kiss 75% drain rounding vs Big Root? Sitrus timing? (The pinch-berry double-heal fix did NOT clear these.)
38. `r17 t17` icepunch vs filletaway | s1#0.hp 72 vs 65 — Fillet Away cost rounding or Sitrus-after-cost.
   Also `r17 t22` trailblaze vs roost | s0.boost.spe 2 vs 1 + hp diff — engine double-applied Trailblaze boost in *this* unit only; empirical repro showed engine gives +1 correctly in isolation → cause is upstream state in the same game (boost carried in by converter? White Herb?). Replay the preceding units of r17.

> Note: items 16/18/31/32 overlap in numbering with cluster sizes — total distinct diverged units = 38; trust the VERBOSE sweep output over this list if they disagree.

## 6. Process rules & hard-won gotchas

1. **OU gate:** `c*` must print 100.00% after every change. Any regression → revert/fix before proceeding.
2. **Patching:** apply edits with exact-match assertions (python3 heredocs with `assert old in src` were used; Edit tool equivalent is fine). **Never** gate a patch on `grep ... && patch` — a failed probe silently skips the patch while builds still pass (this bit us: the entire rampage state machine silently never landed).
3. **PS source of truth is the pin**, not upstream master (e.g. the pin has no `notrace` on Dauntless Shield/Intrepid Sword — upstream knowledge was wrong for us once).
4. **Old `verify` crate** (legacy, slated for retirement): only run under `ulimit -v 6000000` — its branch enumeration can OOM the machine.
5. Commit after each verified cluster: `parity: <description> — N -> M`. Push to `git@github.com:rfgordan/alphakazam.git` (SSH authorized).
6. Codegen edits go in `harness/gen-data.mjs`, then regenerate `gen.rs`; never hand-edit `gen.rs`.
7. PS failure semantics: PP (+Pressure) is deducted even for target-immunity failures (Prankster-vs-Dark) — fail checks ordering matters relative to `record_move_use`.
8. Engine examples are the fastest hypothesis test — converting a unit by hand into a fixture takes minutes and gives the full branch set.

## 7. Backlog (after the burn-down, in rough priority)

- **Mode 2** (task #14): PS-side exhaustive enumerator — verify *distribution equality*, not just that PS's sampled path exists among engine branches.
- **Engine-led co-sim** (task #15): engine drives, PS verifies sampled trajectories (catches engine-only branches that PS would never produce).
- Mutation kill suite re-run (`harness/run-mutations.sh`) as soon as the count hits 0.
- Retire the old `verify` crate.
- Update `~/.claude/.../memory/deep-showdown-rust-engine.md` with final results.
