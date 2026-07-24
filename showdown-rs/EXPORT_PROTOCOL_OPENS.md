# Exporter / Protocol — status & open-item specs (ps-export branch)

Round-trip 4832/4832 · sweep 3831/3831 · smoke 18/18 · transplant **79/110** exact
(1812 decisions) · protocol parity 27 games (c1 270→41 semantic).

## Implemented this pass (generate.rs now free)

### Protocol emitter — fed the missing instruction facts
- `switch_into` / `switch_into_pair` now RETURN their applied `Vec<Instruction>`
  (additive; statement-callers ignore it). `request.rs` faint-replacements and
  `protocol_emit` feed that stream through `protocol::emit_instructions` — entry-hazard
  `|-damage|` / switch-in-ability lines now render (was a bare `|switch|`).
- `Instruction::ClearBoosts { side, previous }` — Haze emits one grouped, exactly-reversible
  delta the display maps to `|-clearallboost|` (state-equivalent to the per-stat Boost run;
  in the apply/reverse roundtrip property test). Clear Smog stays per-stat (different PS line).
- Initial lead `|switch|` at battle start; skip `|-start|` for PS-unannounced volatiles
  (ChoiceLock / Protosynthesis / QuarkDrive — PS uses `[silent]`/`-activate`).

### Transplant unmodeled-field bisect (Supreme-Overlord-class hunt) — +5 games
- **lockedmove `duration`**: emit 1 not 2. PS ends the rampage via `onAfterMove` when
  `duration === 1` (bumped to 2 only DURING the next move by `onRestart`); `trueDuration`
  gates the end-of-lock confusion. duration=2 meant the transplant's Outrage never ended,
  so the confusion + Lum-Berry cure never fired. (rd292)
- **Protosynthesis / Quark Drive `bestStat`**: emit the boosted stat (+`fromBooster`, omitted
  under sun/electric-terrain), computed via `generate::proto_stat`. PS's `onModify*` read it;
  `{}` silently dropped the ×1.3/×1.5 boost → wrong paradox-mon damage. (c2b1, c2b3)
- **Cosmetic-only formes** (Florges colours, Vivillon/Alcremie/…): normalize in the projection
  (engine collapses them to base; gameplay-identical). (r16, rd293)
- Move-choice resolution against the request's OFFERED list (locked moves): fails 3→0.

## Remaining opens (evidenced)

### A. Per-decision unmodeled-field / cascade diffs (8 games) — continue the bisect
Each is a single upstream per-decision divergence (a damage/faint/status difference) that
either shows directly or cascades to an `activeRoster` / Beat-Up-hit-count mismatch:
- `c2a1`, `c2a5`: **Beat Up** hit count off by one (`timesAttacked` 6/5, 5/4). PS's
  `move.allies = side.pokemon.filter(a => a===user || (!a.fainted && !a.status))`; the count
  = eligible party members. The transplant has one extra eligible ally → an upstream party
  status/faint divergence (the sweep proves the engine counts correctly, so it's an exported
  field ~3 decisions earlier). Bisect: export at D-1, compare the p2 party status/fainted.
- `c3a1s12`, `c3a2s23`, `d3`, `d4`: `activeRoster` mismatch — a mon switched/fainted
  differently at decision D (18-19 downstream diffs). `d4` root shows `lastMoveFailed`.
- `c2a2`: a mid-turn faint chain (15 diffs).
Procedure (per game): re-export at D-1; if D then matches, the drift is an accumulated draw
difference (unmodeled draw), else compare the diverging move's `getDamage`/eligibility inputs
(abilityState.*, volatiles.*, status, item) — the delta is the field to export (as with
`fallen` / `bestStat` / lockedmove `duration`).

### B. Mid-turn-faint "turn-cascade" (9 games) — LINKED to the campaign faint-schedule class
Gate-tagged `[request-phase: extra mid-turn faint — turn-cascade root]`. A per-decision damage
difference KOs a mon that survived in the recording, so the transplant reaches a `forceSwitch`
phase the recording didn't. **The root is the same per-decision damage/faint-timing the
draw-exact campaign owns (the reserved faint-schedule class)** — deferred until that tranche
lands, then re-bisect with the corrected schedule. PS turn rule (for reference): `battle.turn`
increments only in `endTurn()`; `midTurn` stays true across mid-turn pauses.

### C. Protocol semantic-zero residuals (c1 at 41)
- EOT heal ordering (~9): order the engine's end-of-turn Heal instructions to PS's
  `onResidualOrder` so the residual-heal sequence aligns (an EOT-ordering detail).
- `|-crit|`/`|-miss|`/`|-fail|`/ability-`|-immune|`: thread `generate_instructions_annotated`'s
  per-branch `DrawEvent`s into `protocol_turn` as an optional annotation slice (crit draw →
  `|-crit|`, accuracy-fail-with-no-Damage → `|-miss|`, etc.). Currently allowlisted-cosmetic.
- Two-turn / Future-Sight move announcements; some randbats games where the engine replay
  itself diverges (r9/r19/…) are engine-exactness items, not protocol.

### Cosmetic allowlist (explicit)
`[from]`/`[silent]`/`[of]` tags · `|split|` HP pairs · public `/100` vs exact HP · status-
suffixed HP · species/move DISPLAY name vs `toID` · `|turn|` index · setup chrome · `|debug|` ·
`|-crit|`/`|-miss|`/`|-fail|`/`|-activate|`/`|-anim|` · `|-clearallboost|` family · ability-
`|-immune|` · `|-hitcount|`.

Sample replay artifacts: `harness/protocol-logs/{c1,r5,c5c1}-replay.html` (OU / randbats /
directed), openable in the Showdown replay player.
