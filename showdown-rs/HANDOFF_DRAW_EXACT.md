# HANDOFF: Draw-Exact Campaign (branch `prng-exact`)

**PHASE 6 (2026-07-26): 400/512 full games byte-exact from seed (78.1%), up from 372;
init-aligned 512/512. The audited 111 stayed 111/111 at every step.** Corpus: 111 audited
traces + 401 fresh gen9randombattle seed fixtures (`harness/seed-fixtures/`, seeds 1000-1400).
Differ 99.50% (3812/3831), zero `rust extra`; sweep 3831/3831; smoke 18/18; round-trip PASS.
Kill criteria NEVER triggered.

**Read the first section of `DRAW_EXACT_SCOREBOARD.md` — "PHASE-6 EXTENSION BURN-DOWN" — before
anything else.** It carries the eight roots this phase landed (PS file:line each), the
GROUND-TRUTHED mid-turn re-request schedule table, the re-triaged 112 open games, and the named
opens. **The mid-turn re-request counter class is CLOSED** (all 18 games), and so are the berry
`Update` ordering and the `|hp| <= 3` Knock Off lead.

The mid-turn class was TWO roots, not one — the "opposite directions" were an artifact of
reading `active_turns` and `wish` as one phase:
1. `activeTurns++` lives in `nextTurn()` (battle.ts:1762), reached only after the whole turn
   survives to `endTurn()`. A KO that ENDS the battle returns from `runAction` first
   (battle.ts:2857), so nobody is advanced — the engine advanced it in the residual loop.
2. Wish is a SLOT condition, and `fieldEvent('Residual')` runs slot-condition handlers even over
   a FAINTED holder (battle.ts:512-514) — the engine skipped the tick behind its fainted-active
   guard, and its "matured Wish lingers" model (plus a compensating `apply_switch` hack) was the
   opposite of PS, which CONSUMES it with no heal.

The three biggest remaining levers, in order:

1. **The 37 games whose first `hp` divergence exceeds 10 HP.** Wrong MECHANICS, one shared root
   at a time — the loop that produced this phase. Knock Off recurred 7x in the divergent units
   before its two fixes; re-run the "which move ids recur in the divergent unit" scan
   (scratchpad recipe in the scoreboard) after every landing.
2. **The `stall` / Protect chain (rb1227 t15 is a single-field probe).** `s0.stall_counter
   engine=0 ps=1` with the ONLY other symptom a missing `shuffle[4,2,4]` that follows from it.
   The engine took `!foe_moves_later` where PS's `queue.willAct()` is true.
3. **The remaining `onBasePower` chainModify handlers** still applied as their own `modify()`
   (Collision Course / Electro Drift / Psyblade / Expanding Force, the `-ate` abilities,
   Analytic). Same root shape as the Knock Off fix; each only bites when a second chain member
   co-occurs.

Practical notes:
- Recording: `bash harness/record-seeds.sh <first> <last>` — sequential, one node process, ~2 min
  for 400 games, RESUMABLE. Sidecars are gitignored; rebuild fixtures with
  `MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz`.
  **Regenerate fixtures whenever `convert.rs` changes** — they bake in its digests. Phase 6
  touched only `generate.rs` / `instruction.rs`, so no digest moved.
- Triage loop: `GATE_THREADS=1 DBG_DIFF=1 DBG_GAME=rb SEED_GATE=1 cosim
  harness/seed-sidecars/*.json.gz 2> dbg.txt` dumps every game's first divergent block in ONE
  pass (serial, so the blocks are not interleaved); join it to `VERBOSE=1 SEED_GATE=1 …` by
  decision index. `DBG_GAME` is a `starts_with` prefix; DIFF lines go to stderr.
- **`stateAfter.turn` / `midTurn` / `ended` in a trace are POST-state** (`harness/cosim.mjs:1057`
  records them after `battle.choose` returns). `midTurn:true` on a `move` decision means PS
  stopped mid-turn to ask for a replacement; its trailing `switch` decision is one turn later.
- **Judge every commit by the exact-SET diff on BOTH corpora, never by the count.** Also watch
  for COMPENSATING hacks: the Wish fix scored +0 until the `apply_switch` hack it had been
  paired with was removed too.
- The state divergence itself creates draw-class mislabels. Never treat the draw-class histogram
  as a partition of roots.

--- historical (pre-Phase-6) below ---

**PHASE 5 (2026-07-25): 372/512 full games byte-exact from seed (72.7%), up from 333;
init-aligned 512/512. The audited 111 stayed 111/111 at every step.** Corpus: 111 audited
traces + 401 fresh gen9randombattle seed fixtures (`harness/seed-fixtures/`, seeds 1000-1400).
Differ 99.50% (3812/3831), zero `rust extra`; sweep 3831/3831; smoke 18/18; round-trip PASS.
Kill criteria NEVER triggered.

**Read the first section of `DRAW_EXACT_SCOREBOARD.md` — "PHASE-5 EXTENSION BURN-DOWN" — before
anything else.** It carries the fifteen roots this phase landed (with PS file:line for each),
the re-triaged 140 open games, and the named opens. **S5 (tera formes) and S6 (magnetrise) are
CLOSED. S7 (chainModify accumulation) is landed** — the ModifyAtk/ModifySpA and
ModifyDef/ModifySpD chains now accumulate into one `event.modifier` — with an eleven-game
`|hp| <= 3` residue that is NOT the stat chains.

The three biggest remaining levers, in order:

1. **The mid-turn re-request schedule — 18 games in one root.** `active_turns` (11) is
   uniformly engine = PS + 1 and `wish` (7) is uniformly one tick behind; both only occur in
   turns split by a mid-turn faint/pivot re-request, and they point in OPPOSITE directions,
   which is what a residual phase attributed to the wrong unit looks like. rb1180 d41/d42 and
   rb1203 d10/d11 are the clean instances. Bisect with PS's `battle.prng.getSeed()` against the
   gate's `prng.limbs()` per unit. HIGH regression risk — its own tranche, with call-site
   ground-truthing.
2. **The defender's HP-berry `Update` must run AFTER the move's secondaries.** PS's order in
   `spreadMoveHit` is damage -> onHit -> selfDrops -> secondaries -> DamagingHit -> onAfterHit,
   then `eachEvent('Update')` at `battle-actions.ts:970`; the engine's berry site sits inside
   `apply_post_damage`, ahead of the secondary. rb1003, rb1204, rb1347.
3. **The remaining hp > 10 games (39).** Same loop that produced this phase: they are wrong
   MECHANICS, one shared root at a time.

Practical notes:
- Recording: `bash harness/record-seeds.sh <first> <last>` — sequential, one node process, ~2 min
  for 400 games, RESUMABLE. Sidecars are gitignored; rebuild fixtures with
  `MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz`.
  **Regenerate fixtures whenever `convert.rs` changes** — they bake in its digests (the S6
  magnetrise commit moved exactly 3 of 401; justify every moved digest).
- Triage loop: `GATE_THREADS=1 DBG_DIFF=1 DBG_GAME=rb SEED_GATE=1 cosim
  harness/seed-sidecars/*.json.gz 2> dbg.txt` dumps every game's first divergent block in ONE
  pass (serial, so the blocks are not interleaved); join it to `VERBOSE=1 SEED_GATE=1 …` by
  decision index. `DBG_GAME` is a `starts_with` prefix; DIFF lines go to stderr.
- **Judge every commit by the exact-SET diff on BOTH corpora, never by the count.** It caught a
  real regression twice this session (r3 on Destiny Bond, rb1311 on the S7 refactor), and both
  times the lost game named the missing PS rule.
- The state divergence itself creates draw-class mislabels: four `move-order-tie` games flipped
  on a pure damage fix. Never treat the draw-class histogram as a partition of roots.

--- historical (pre-Phase-5) below ---

**TERMINAL (2026-07-25): 110/111 full games byte-exact from seed (99.1%); ALL 111 init-aligned;
differ 99.45% (3810/3831); sweep 3831/3831; smoke 18/18; round-trip 4832/4832; transplant 79/110.**

--- historical (pre-terminal) below ---

**(2026-07-24): 93/111 full games byte-exact from seed (83.8%); ALL 111
init-aligned; draw-consumption differ 99.09% (3796/3831).** Kill criteria NEVER triggered.
DRAW_EXACT_SCOREBOARD.md is the source of truth (this file's older sections below are historical).

## THE dominant remaining class — mid-turn re-request / faint Update schedule (NEXT SESSION)
Six of the 18 remaining non-exact games (c3, c4, c5, r6, d6, + c5a1's later divergence) share ONE
root: a **mid-turn re-request unit** (`midTurn:true` decision, e.g. d6 idx25 t24 "p1 switch 5 /
p2 move 4" — a mon fainted mid-turn, its side switches in a replacement, then the other side's move
resolves) whose `eachEvent('Update')` shuffle schedule the seed gate mis-counts. Diagnostic method
(proven this session): compare PS's per-turn `battle.prng.getSeed()` (scratchpad `seedcmp.mjs`,
teamset-parameterized) against the gate's `prng.limbs()` (temporarily print in `seedgate.rs`
step-loop) — the drift localizes to the exact mid-turn unit, then probe the shuffle call-sites
(`probe_d6.mjs` wraps `prng.shuffle`+`eachEvent`) to ground-truth the schedule. d6 idx25 PS schedule
(6 shuffles): [switch runAction Update, runSwitch getAllActive speedSort (battle-actions:178),
runSwitch runAction Update, {p2 move: acc/crit/dmg}, move 970 Update, move 1024 Update, ... ]. The
gate's `step_unit` resolves p1's mid-turn "switch" as a pivot (mc) and emits a fresh turn-start
bracket — mismatching PS's mid-turn switch-in bracket. This is the shared turn-resolution/faint path;
HIGH regression risk to the 93 exact games — needs its own careful tranche with full call-site
ground-truthing of the mid-turn re-request schedule (do NOT bolt it on).

## Landed this session (2026-07-24, 89 -> 93, 2 commits, all rails green, zero regression)
1. **Side/field-targeting status moves fire no moveHit Update** (89->90; flips c7). A status move
   targeting a SIDE/FIELD (Reflect/screens/hazards/weather) never enters PS's per-pokemon
   hitStepMoveHitLoop, so it fires no 970/1024 Update — the engine over-emitted them on tied boards.
   Ground-truthed on c5a1 t12 (Prankster Reflect). generate.rs `hits_pokemon` gate.
2. **Forced-replacement switch bracket** (90->93; flips c3b2s52, c3b2s53, c6a2s112). The gate applied
   post-KO replacements (`switch_into`) to state but consumed ZERO PRNG draws; PS fires a 3-shuffle
   bracket (switch runAction Update + runSwitch speedSort + runSwitch runAction Update), tie-gated.
   seedgate.rs `step_unit` now consumes it via `replacement_bracket_tied`.

--- historical (pre-89) below ---

Paused 2026-07-24 at session limits, mid-finishing-tranche. **State: 63/111 full games
byte-exact from seed (56.8%); draw-consumption differ 98.93% (3790/3831 units; 41 mismatched
units remain).** Twelve tranches complete; kill criteria NEVER triggered.

## The goal (Rob's directive, verbatim intent)
For a given seed the Rust engine must produce the exact same sampled outcome as pinned PS
(b9dc987d) — internally the same number of PRNG draws in the same order. IDENTICAL behavior:
every observed diff (draw count/order/kinds/ARGS/handler composition) is a mandatory fix;
corpus impact orders work but never filters it; the completion bar is **differ-zero
corpus-wide + games byte-exact**; anything genuinely unfixable stays a NAMED OPEN ITEM with
PS source evidence — never reclassified as "neutral"/"artifact". Branch model breakable on
this branch; Enumerate/Sample must keep passing their tests + the 3831/3831 corpus
state-sweep + smoke 18/18 as the mechanics-drift rail.

## Read these first (all in showdown-rs/)
- `DRAW_EXACT_PLAN.md` — charter, phases, kill criteria.
- `DRAW_EXACT_SCOREBOARD.md` — progression log + the LIVE LEDGER of remaining classes
  (each with PS evidence). The single source of truth for what's left.
- `DIVERGENCE_DOSSIER.md` — 73-game mechanism triage (some roots stale; re-survey live).
- Machinery: `crates/engine/src/psprng.rs` (bit-certified PS PRNG, 25.6M-draw gate);
  `generate.rs` (annotation sites, bracket emissions incl. speedSort/Update models,
  `RealizedSource` + `apply_multihit_realized*`); `crates/cosim/src/drawdiff.rs`
  (DRAW_DIFF=1 per-decision differ + DRAW_DBG); `seedgate.rs` (SEED_GATE=1 from-seed
  full-battle gate + DBG_GAME/DBG_I).

## Verification commands (run from prng-exact/showdown-rs; `. "$HOME/.cargo/env"`)
- Differ: `DRAW_DIFF=1 target/release/cosim harness/cosim-traces/*.json.gz` → scoreboard block.
- Seed gate: `SEED_GATE=1 target/release/cosim harness/cosim-traces/*.json.gz`.
- Rails per commit: `cargo test --release -p engine -j 2`; plain sweep (must stay 3831/3831,
  0 unsupported); `bash harness/run-distribution-smoke.sh` (18/18) after structural changes.
- SEED_GATE regression check: diff the EXACT-GAME SET before/after (VERBOSE listing is
  truncated — do not use it for regression judgment).
- Machine: 16GB — cargo -j 2, ONE node process at a time, monotone discipline.

## Where the interrupted agent was (resume point)
Investigating a **self-drop draw over-emission on Grav Apple** (c3b2s52 t6): the self-drop
random(100) draw-and-discard fires when `md.self_boosts` is non-zero, but Grav Apple drops
the TARGET's Def (a secondary) — suspicion: codegen mis-encodes it as self_boosts (check
`gen.rs` entry + `harness/gen-data.mjs` extraction rule; if codegen is wrong, fix the
extraction not a special case — and check which OTHER moves the same mis-encoding hits).
Also queued in its plan: a cursedbody straggler (both "potentially clean" wins).

## Remaining 41 differ units (from its last capture; re-run differ for live state)
- ~18 "Class-1 midturn": first-mover no-draw Update ordering — the differ/annotation path
  emits the first mover's runAction-2882 Update in the wrong position when the first move is
  a no-draw/failed status move. Differ-side annotation interleaving; games already exact
  there via Replicate's forced_tie_order. (Untouched by the last two tranches — intricate.)
- ~23 tail: the Grav Apple self-drop over-emission (above), cursedbody straggler,
  cantusetwice mc-resolution (~9: differ must mirror PS's selection-time disable for
  Gigaton Hammer/Blood Moon when resolving recorded choices), queue-length shuffle[3,0,2]
  (~5: PS's action queue still holds the pending residual action at that shuffle), beatup
  per-member state-mismatch (2, genuine damage-calc item — verify PS's per-member formula),
  bodypress (2), trace mid-turn re-fire (3), + stragglers. All in the scoreboard ledger.

## Recently landed (this tranche, all committed)
stall-volatile lifetime + roost residual handler; par/sub-blocked accuracy ordering;
rampage-end confusion duration (+2 games); Trick/Switcheroo accuracy roll + post-modifier
arg (+1 game); Future Sight/Doom Desire delayed-strike realized stream. Working tree at
pause: CLEAN (verify with git status; if dirty, it's the interrupted agent's WIP on the
Grav Apple item — review the diff deliberately).

## Process lessons (hard-won — follow them)
1. Commit small; stalls/limits kill sessions mid-work. Green rails → commit immediately.
2. One full differ pass capturing ALL mismatch locations beats repeated slow passes.
3. Realized-cursor desyncs masquerade as state/rounding bugs (see the "PS 326 vs 327"
   misdiagnosis — it was a missed inter-hit shuffle consume, not compute_damage).
4. Codegen mis-encodings produce systematic over/under-emission — fix extraction rules,
   never per-move special cases.
5. The dossier's roots go stale as fixes land — re-survey the live differ before chasing.
6. Zero over-emission is a hard invariant: never emit a draw PS doesn't.

## After differ-zero
Run full rails + seed gate; the games number at differ-zero is the headline. Any game still
non-exact at differ-zero has a state-computation divergence by definition — triage those as
mechanics bugs (like beatup). Then: Phase-3 certification per the charter (extend to fresh
seeds beyond the corpus; full-battle replay gate as CI).
