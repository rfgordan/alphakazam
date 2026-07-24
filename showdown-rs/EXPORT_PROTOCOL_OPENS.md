# Exporter / Protocol — open-item specs (ps-export branch)

Specs for the two open classes whose FIX touches campaign-owned engine files
(`generate.rs`, `seedgate.rs`, `drawdiff.rs`). Written now; implement after the
campaign tranche lands. Everything not needing those files is already done on
`ps-export` (round-trip 4832/4832; transplant 74/110 exact, 1735 decisions;
FlowVec/Battle protocol capture + `export_state`).

---

## (3) Transplant "turn-cascade" — per-decision damage divergence, NOT turn-labeling

### What it is (diagnosed, evidenced)
9 transplant games (`c1a c2a3 c2a4 c3c2s82 c6a1s105 c6a2s111 c6a2s114 d3 d4` class)
diverge with a `turn`/`activeTurns` off-by-one plus one missed end-of-turn residual.
This is **not** a turn-labeling bug. The gate now tags them
`[request-phase: extra mid-turn faint — turn-cascade root; per-decision damage divergence]`.

Root, traced on `c6a2s114` (diverges d60, t49):
- Through d59 every MODELED field round-trips exact and the request structure matches.
- At decision D (=d60) the transplant's resolution deals damage that KOs a mon which
  **survived in the recording** (or vice-versa). The transplant then sits at a
  `forceSwitch` request that the recording never issued.
- The gate's index-driven loop keeps feeding move choices into the transplant's
  now-mid-turn switch phase; the battle freezes mid-turn (turn counter stuck), so the
  observed `turn`/`activeTurns`/one-missed-Leftovers diffs are all downstream symptoms.

### PS turn/midTurn rule (sim/battle.ts, pin b9dc987d — reference for any fix)
- `battle.turn` increments **only** inside `endTurn()`, reached by `turnLoop()` when the
  action queue drains **without** a mid-turn pause (`turnLoop` returns early whenever
  `this.requestState || this.ended`, line ~2973).
- `midTurn` is `true` from turn start until `endTurn()` completes, and **stays true across
  mid-turn pauses**: a resumed `turnLoop` skips the `beforeTurn`/`residual` inserts
  (`if (!this.midTurn)`, line ~2964) and does not re-increment.
- The recorder captures `decision.turn`/`decision.midTurn` = `battle.turn`/`battle.midTurn`
  **after** the choices resolve. So a mid-turn faint decision keeps the current `turn` and
  sets `midTurn=true`; the following forced-replacement decision advances the turn only when
  the replacement completes the queue.

### What the fix needs (engine/exporter, pending)
The transplant is pure PS driven by recorded choices, so the divergence means the EXPORTED
state (a clean turn-start boundary ~30 decisions earlier) differs from PS's true state in a
way that only changes the damage at decision D — i.e. an **unmodeled PS field** the exporter
doesn't carry, or a **PRNG desync from an unmodeled draw** consumed between the transplant
point and D. Same class as the already-fixed Supreme-Overlord (`abilityState.fallen`) and
Choice-lock (`choicelock.move`) cases.

Actionable steps (do per game, after the campaign tranche):
1. Bisect the transplant point later (export at D-1) — if D then matches, the drift is a
   draw-count difference accumulated over d(start..D); if it still diverges, it's a field
   present at D-1.
2. At the diverging move, compare the transplant's `getDamage` inputs (attacker
   boosts/ability-state, defender def/ability, item, `abilityState.*`, `volatiles.*`,
   `species`, weather/terrain) against the recorded snapshot — the delta is the unmodeled
   field to export (as with `fallen`).
3. Harness robustness (no engine files, optional): drive by the battle's live request phase
   rather than the recorded decision index, so a diverged game reports the exact first
   divergence instead of freezing. (The gate already flags D correctly + tags the root, so
   this is diagnostics-only.)

---

## (4) Protocol semantic-zero — instruction facts protocol.rs needs fed

`protocol.rs` is a pure display layer over the reversible `Instruction` stream. After the
DecrementPp move-boundary + amount-guard + effectiveness work, `c1` is at 46 semantic diffs
(from 270); the residual gaps below each need a fact the current stream doesn't carry. All
FIXES are in `generate.rs` (campaign-owned) — spec now.

### 4a. Entry-hazard `-damage` on faint-replacement / landing switch-ins
`generate::switch_into` / `switch_into_pair` apply Stealth Rock `-damage`, Toxic Spikes
`-status`, Sticky Web `-unboost`, and switch-in ability/item effects by **mutating `State`
directly** and returning nothing. So `protocol_emit.rs` (and `request.rs`'s replacement path)
emit only the bare `|switch|` line (via `protocol::switch_line`) and miss the hazard events
PS emits right after the switch.

**Needed:** `switch_into`/`switch_into_pair` return the `Vec<Instruction>` they apply (or
take `&mut Vec<Instruction>` out-param). Then both callers append those to the emitter:
`protocol_turn`-style walk emits the `|-damage|`/`|-status|`/`|-unboost|`/`|-ability|` lines
in order. Zero cost when the out-param is `None`. protocol.rs already handles every one of
those instruction variants — it just needs them delivered.

### 4b. Haze / Clear Smog group-clear → `|-clearallboost|`
PS emits one `|-clearallboost|` (or `|-clearnegativeboost|`, `|-clearboost|`) when a
boost-reset move fires. The engine instead emits N individual `Boost` instructions (per stat,
`amount = -(current stage)`), indistinguishable at the instruction level from a legitimate
multi-stat unboost. protocol.rs cannot group them without the move context.

**Needed (pick one):**
- Preferred: a dedicated `Instruction::ClearBoosts { side, which }` (`which` = all /
  negative-only / positive-only) emitted by the Haze/Clear-Smog/Haze-family handlers, which
  protocol.rs maps 1:1 to `|-clearallboost|` / `|-clearnegativeboost|`. Keeps the state delta
  reversible and the display exact.
- Or: pass the active-move id into `protocol_turn`'s per-instruction context so it can
  coalesce the Boost run under a known clear-move. (More fragile; ordering-sensitive.)

Currently allowlisted as cosmetic in `harness/protocol-parity.mjs`.

### 4c. Residual-heal attribution (~9 on c1) and `|-crit|`/`|-miss|`/`|-fail|`/ability-`|-immune|`
- Residual heals: the engine emits Leftovers/Regenerator/etc. `Heal` instructions the parity
  gate over- or under-matches vs PS's split-paired, `[from]`-tagged lines. After the
  `[from]`/split-collapse normalization these are mostly reconciled; the ~9 residual counts
  left on c1 are a heal-ordering detail (which mon's residual fires first vs PS's
  `onResidualOrder`). Fixable by ordering the engine's EOT heal instructions to PS's residual
  order — an EOT-ordering detail in `generate.rs`.
- `|-crit|`/`|-miss|`/`|-fail|` and ability-based `|-immune|` carry **no state delta** and are
  not in the instruction stream. To emit them protocol.rs must be **fed the fact** (not the
  transition changed): e.g. `generate_instructions_annotated` already exposes the per-branch
  `DrawEvent`s (the crit `randomChance`, the accuracy roll) — thread those into `protocol_turn`
  as an optional annotation slice so it can emit `|-crit|` (crit draw succeeded), `|-miss|`
  (accuracy draw failed → no Damage), `|-fail|` (move used, no effect). Ability-`|-immune|`
  (Levitate/Flash Fire/Good-as-Gold) needs an ability-block marker; document as allowlisted
  until such a marker exists.

### Cosmetic allowlist (explicit, unchanged)
`[from]`/`[silent]`/`[of]` source tags · `|split|` private+public HP pairs · public `/100` vs
exact HP · status-suffixed HP (`51/371 psn`) · species/move DISPLAY name vs `toID` (no
name table yet) · `|turn|` index (pre-turn-1 offset) · `|t:|`/`|player|`/setup chrome ·
`|debug|` · `|-crit|`/`|-miss|`/`|-fail|`/`|-activate|`/`|-anim|` · `|-clearallboost|` family ·
ability-`|-immune|`.
