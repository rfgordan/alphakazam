# Deep-Showdown Handoff: Divergence Burn-Down

**State as of 2026-06-12 (latest):** 1532 cosim units, **1507 matched / 25 diverged / 0 unsupported** → EXACTNESS 98.37%, COVERAGE 100%. The OU corpus (`c*.json.gz`, 389 units) is at **100.00% and must stay there** — it is the regression gate. Campaign so far: 237 → … → 38 → 37 → 35 → 32 → 31 → 29 → 28 → 27 → 26 → 25.

**The directive:** keep grinding the remaining divergences to zero, one cluster at a time, committing each verified batch.

### Fixes landed this session (38 → 25)
1. **Protean/Libero type revert on switch-out** — engine cleared the `TypeShifted` volatile but never reverted the `types` field; PS's `clearVolatile` resets to base types (unless terastallized).
2. **Turn-action double switch resolves sequentially** in speed order — PS's `runSwitch` (queue order 101) preempts the slower side's pending `switch` (order 103), so the faster switch-in's Intimidate hits the foe's *outgoing* mon (wasted) and only the slower one lands. This differs from a double *replacement* (both fainted) which still uses `switch_into_pair`.
3. **Faint-replacement `activeTurns` timing** (two bugs): (a) two replacements on the SAME side (hazard-faint cascade) wrongly took the `switch_into_pair` path and +1'd the staying side — now gated on different sides; (b) a mid-turn faint replacement snapshots BEFORE PS's `endTurn` `activeTurns++` (detect via `unit.last().turn == move.turn`), so the staying side is decremented.
4. **Variable multi-hit through Substitute** — the sumset-DP path (`apply_multihit_dp`) ignored the target's sub; added `apply_multihit_dp_sub` keyed on `(sub_remaining, mon_damage)`.
5. **Poison Puppeteer** (Pecharunt) — confuses a foe it poisons/badly-poisons with a move (added in `apply_target_secondary`).
6. **Throat Chop re-hit** doesn't refresh its 2-turn countdown (PS condition has no `onRestart`) — only set the counter when applying the volatile fresh.
7. **Zero to Hero** (Palafin) — forme change to Palafin-Hero on switch-out (Terapagos-style Transform with recomputed stats).
8. **Punk Rock ×1.3 → base power** (PS `onBasePower`), not the attack stat — the floor lands where PS's does. NOTE: Technician/Tough Claws/Iron Fist/Sharpness/Strong Jaw/Mega Launcher/Reckless are ALSO `onBasePower` in PS but still applied to `atk_stat` in the engine; moving them risks multi-modifier rounding regressions, so do it surgically when a specific divergence demands it.
9. **Transform reverts on faint** — `revert_transform` helper called from `apply_post_damage` (target/attacker faint) and struggle recoil; **plus** a converter bug: `baseSpecies` is a `"[Species:x]"` ref that wasn't being prefix-stripped, so a transformed mon's base species fell back to its transformed species.

### Two divergences deemed NOT engine-fixable from the trace (skip / investigate the recorder)
- **Stored Power cluster** (`r5 t3`, `r5 t27`): PS's recorded damage requires Stored Power BP 60 (= `20 + 20·positiveBoosts()` with +2 boosts), but the serialized `boosts` are 0 in every snapshot of that game, and PS's formula is identical to the engine's. The trace's post-state boosts are internally inconsistent with the damage dealt — likely a recorder artifact (boosts captured at a different point than when `basePowerCallback` ran). Confirmed Polteageist SpA 254, Delibird SpD 147, BP would be 20 → engine deals max 36, PS dealt 103.

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

**Final checks when count hits 0:** run `harness/run-mutations.sh` (mutation kill suite — damage-path code changed a lot, the verifier must still catch injected bugs; currently 8/8 killed), and update the project memory file. **GOTCHA: the mutation suite restores the source but leaves the last mutant's compiled binary in place.** After running it, `touch crates/engine/src/generate.rs && cargo build --release -p cosim` before any sweep, or you'll see ~40 phantom hazard divergences from the leftover `StealthRock→Spikes` mutant.

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

## 5. The 23 remaining divergences (refreshed, current HEAD)

Run `VERBOSE=1 cargo run --release -p cosim -- harness/cosim-traces/*.json.gz 2>/dev/null | grep "diverged t"` for the live list. All are randombattle (r*) except the storedpower pair (those live in r5). Grouped by diagnosis:

### A. Per-unit damage-formula tails (each a distinct small calc bug; needs precise PS-formula reproduction)
- `r1 t16` psyblade vs hypervoice | s0#5.hp 204 vs 157 (engine LOW) — Psyblade ×1.5 in Electric Terrain; check terrain/spread interplay.
- `r19 t18` liquidation vs surgingstrikes | s1#5.hp 103 vs 89 — Surging Strikes is always-crit 3-hit; check crit×multihit interleave.
- `r5 t18` doubleedge vs liquidation | s1#0.hp 58 vs 129 (engine LOW by ~70) — large; dump defender ability/item.
- `r8 t18` ivycudgel vs photongeyser | s1#2.hp 189 vs 186 — ±3; Photon Geyser category-by-boosted-stat, or Ivy Cudgel mask type.
- `r4 t30` uturn vs calmmind, Snorlax | s1#2.hp 221 vs 251 — 30hp; check defender item/ability.
- `r2 t38` protect vs sacredfire, Necrozma-Dusk-Mane | s0#0.hp 243 vs 272 — Prism Armor (×0.75 super-effective) is handled at line ~2161; verify it fires here.
- `r16 t17` stickyweb vs leafstorm | both sides' hp off — Contrary/White-Herb + hazard-boost interaction.
- `r17 t6`,`t7` icepunch vs drainingkiss | hp ±8/±12 — attacker is Feraligatr (Sheer Force + Life Orb); the constant-26 SF+LO cluster was supposedly fixed — re-examine Draining Kiss 75% drain rounding / Big Root.
- `r17 t17` icepunch vs filletaway | s1#0.hp 72 vs 65 — small icepunch damage diff (Fillet Away should FAIL: Veluza hp 112 < cost 146).
- `r15 t20` acrobatics vs earthquake | s1#5.hp 190 vs 189 — ±1 rounding.

> Several of these may be `onBasePower` abilities applied to `atk_stat` instead of base power (see the Punk Rock fix). Technician/Tough Claws/Iron Fist/Sharpness/Strong Jaw/Mega Launcher/Reckless are ALL `onBasePower` in PS but still on `atk_stat`. Check each diverging unit's attacker ability; if it's one of these, move it to the base-power chain (next to Punk Rock at ~line 2255) — but watch for multi-modifier rounding regressions on the OU gate.

### B. Diagnosed, NOT engine-fixable from current traces (need recorder change or are artifacts)
- `r5 t3`, `r5 t27` **storedpower** | engine LOW — PS damage requires BP 60 (= +2 positiveBoosts) but serialized `boosts` are 0 in every snapshot and PS's formula is identical to the engine's. The trace's post-state boosts are internally inconsistent with the damage. **Recorder artifact** — skip, or patch the recorder to capture boosts at basePowerCallback time.
- `r12 t35` Pincurchin-switch vs **stompingtantrum** | s0#2.hp 156 vs 62 (engine LOW) — Stomping Tantrum doubles to 150 BP because the user's PREVIOUS move failed (`moveLastTurnResult === false`). **`moveLastTurnResult` is NOT serialized by the recorder** (confirmed absent), so the converter can't reconstruct it. Either patch the recorder to emit it, or heuristically infer from `lastMove.hitTargets == []` in the converter (fragile).

### C. Complex state machines (deferred — need careful semantics)
- `r9 t43` outrage, p2 wish then faint-replacements | s1.wish engine=(1,165) ps=(0,0) — a wish that LINGERED past its residual (slot occupant absent at startingTurn+1) is consumed/cleared as the replacement enters; engine keeps it. Wish-linger + faint-replacement interaction.
- `r9 t45` outrage vs wish | engine still Rampaging(outrage,2), PS ended | the rampage's `trueDuration` mid-turn snapshot (idx 50, a t44 faint-replacement) reads td:2 BEFORE PS decrements it, so the converter maps 2 turns remaining when only the final turn is left. Same pre-endTurn-snapshot class as the activeTurns fix, but for lockedmove trueDuration.
- `r17 t22` trailblaze vs roost | s0.boost.spe 2 vs 1 + hp — engine gives +2 Spe here but +1 in isolation; cause is UPSTREAM state in the same game (replay preceding r17 units).

### D. Other identified mechanics
- `r10 t6` thunderbolt, p2 thunderwave + Bellibolt switch-in | s0#0.hp 212 vs 246 (engine over-damaged its OWN s0#0) — Bellibolt has **Electromorphosis** (Charge on being hit); the s0#0 hp diff is on p1's side though, so this may be a damage carryover / Static-para / different cause — re-dump.
- `r10 t22` earthquake vs poltergeist | s1#3.move1.pp 1 vs 2 (engine deducted one extra) — Pressure PP over-deduction on a benched mon (was active earlier); upstream Pressure accounting.
- `r12 t27` substitute vs struggle | engine sub gone (0), PS sub at 15 — engine's struggle dealt ≥60 to a 60-hp sub (broke it), PS dealt 45 (survives). Struggle damage-vs-sub magnitude diff.
- `r18 t10` freezedry vs terastarstorm | s1#2.hp 194 vs 238 — Tera Stellar / Tera Starstorm one-time boost bookkeeping.
- `r2 t23` psychicnoise vs tripleaxel, Entei | s0#0.hp 138 vs 216 — Psychic Noise heal-block + Triple Axel; 2.1M-branch unit (sanity-check the explosion).
- `r1 t9` Squawkabilly-White vs Vikavolt double switch | s0#1/s1#5 hp off by ~18/16 — simultaneous switch-in hazard/residue on a double switch.
- `r3 t19`-ish flareblitz vs destinybond | recoil ±2 rounding (PS recoil = round(damage/3)).

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
