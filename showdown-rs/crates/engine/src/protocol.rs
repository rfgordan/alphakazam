//! Pokémon Showdown battle-**protocol** emitter.
//!
//! A second display layer alongside [`crate::narrate`]: same inputs `(pre-state, the two
//! choices, the resolved instruction stream)`, but it emits PS's machine protocol lines
//! (`|move|…`, `|-damage|…`, `|switch|…`, …) instead of English. The emitted stream feeds the
//! Showdown replay player (see `harness/make-replay.mjs`) and the log-parity gate.
//!
//! Like `narrate`, it replays the chosen branch's instructions on a throwaway clone so every HP,
//! status and faint is read from the exact state at each step, and emits nothing on the hot path.
//!
//! ## What it can and cannot emit
//! The engine's `Instruction` stream is a set of reversible STATE DELTAS, not PS's event log, so
//! this layer emits every line that corresponds to a state change (`|move|`, `|switch|`/`|drag|`,
//! `|-damage|`, `|-heal|`, `|-boost|`/`|-unboost|`, `|-status|`/`|-curestatus|`, `|-weather|`,
//! `|-fieldstart|`/`|-fieldend|`, `|-sidestart|`/`|-sideend|`, `|-start|`/`|-end|`, `|-item|`/
//! `|-enditem|`, `|-terastallize|`, `|faint|`, `|-supereffective|`/`|-resisted|`, `|win|`,
//! `|turn|`, `|upkeep|`). Moves are announced at their `DecrementPp` — the stream's move-USE
//! marker — so the move boundary, execution order, and self-vs-foe target are exact and a move is
//! never invented for a mon KO'd or blocked before acting; type effectiveness is recomputed from
//! the type chart at that move's damage. Lines that carry NO state delta and cannot be derived —
//! `|-crit|`, `|-miss|`, `|-fail|`, ability-based `|-immune|`, `|-activate|`/`|-anim|` flavor, and
//! PS's grouped `|-clearallboost|` (the engine clears boosts per-stat) — are the documented gap
//! (the log-parity gate's cosmetic/annotation allowlist).
//!
//! Names use PS `toID` strings lightly prettified (a display-name table is future work); this is a
//! documented cosmetic difference vs PS's proper species names.

use crate::damage::type_multiplier;
use crate::data::move_data;
use crate::generate::MoveChoice;
use crate::ids::{BoostIndex, Item, MoveCategory, Status, Terrain, Type, Weather};
use crate::instruction::{Instruction, SideConditionId};
use crate::state::{SideId, State};

/// How to render HP in `|-damage|`/`|-heal|`/`|switch|` lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HpStyle {
    /// PS public spectator form: `cur/100` (percent of max, ceil like PS), plus `fnt` at 0.
    Percent,
    /// Exact form: `cur/max`.
    Exact,
}

/// Emit the PS protocol lines for one resolved turn. `a1`/`a2` are side-One/side-Two choices;
/// `instructions` is the chosen outcome branch. `hp_style` selects public `/100` vs exact HP.
pub fn protocol_turn(
    pre: &State,
    a1: MoveChoice,
    a2: MoveChoice,
    instructions: &[Instruction],
    hp_style: HpStyle,
    out: &mut Vec<String>,
) {
    out.push(format!("|turn|{}", pre.turn));
    let _ = (a1, a2);
    emit_instructions(pre, instructions, hp_style, out);
    out.push("|upkeep".to_string());
}

/// Walk an instruction list `pre -> …`, emitting the PS protocol lines for each state change (no
/// `|turn|`/`|upkeep|` framing). Reused for a turn's body AND for a faint-replacement / landing
/// switch-in stream (from `generate::switch_into`), which starts with a `Switch` and then carries
/// entry-hazard `|-damage|` / switch-in-ability lines. Moves are announced at their `DecrementPp`
/// (PS's move-use point) — the move BOUNDARY the flat stream provides — so a move is never invented
/// for a mon KO'd or blocked before acting; `current_move` attributes type effectiveness.
pub fn emit_instructions(pre: &State, instructions: &[Instruction], hp_style: HpStyle, out: &mut Vec<String>) {
    let mut s = *pre;
    let mut current_move: Option<(SideId, crate::ids::MoveId)> = None;
    for (i, &ins) in instructions.iter().enumerate() {
        if let Instruction::DecrementPp { side, slot, move_index, .. } = ins {
            let move_id = s.sides[side.index()].pokemon[slot as usize].moves[move_index as usize].id;
            if move_id != crate::ids::MoveId::None {
                emit_move(out, &s, side, move_id);
                current_move = Some((side, move_id));
            }
        }
        // Struggle has no PP slot (no DecrementPp), so announce it at its SetLastMove instead.
        if let Instruction::SetLastMove { side, new, .. } = ins {
            if new.to_id() == "struggle" {
                emit_move(out, &s, side, new);
                current_move = Some((side, new));
            }
        }
        // PS handles switch-OUT bookkeeping silently: boost/stat resets, and Regenerator's 1/3
        // heal on the outgoing mon. The engine emits explicit Boost/ClearBoosts/Heal deltas for
        // these. Suppress a Boost/ClearBoosts/Heal whose side's next event is a Switch (no
        // intervening move for that side) — it is switch-out bookkeeping, not a move/residual.
        let switch_out = matches!(
            ins,
            Instruction::Boost { .. } | Instruction::ClearBoosts { .. } | Instruction::Heal { .. }
        ) && is_switch_out_reset(instructions, i);
        if !switch_out {
            emit_instruction(out, &s, ins, hp_style, &current_move);
        }
        s.apply_one(ins);
        if let Instruction::Damage { side, slot, .. } = ins {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            if p.hp <= 0 {
                out.push(format!("|faint|{}", ident(&s, side, slot)));
            }
        }
    }
}

/// Is the boost instruction at `i` a switch-OUT reset (boosts zeroed as the mon leaves)? True when
/// the same side's next relevant event is a `Switch` before any move (`DecrementPp`) for that side.
fn is_switch_out_reset(instructions: &[Instruction], i: usize) -> bool {
    let side = match instructions[i] {
        Instruction::Boost { side, .. }
        | Instruction::ClearBoosts { side, .. }
        | Instruction::Heal { side, .. } => side,
        _ => return false,
    };
    for ins in &instructions[i + 1..] {
        match ins {
            Instruction::Switch { side: sw, .. } if *sw == side => return true,
            Instruction::DecrementPp { side: mv, .. } if *mv == side => return false,
            _ => {}
        }
    }
    false
}

/// A `|switch|pNa: Name|Details|HP` line for the mon at `side`'s active slot — used by the
/// request-flow driver for faint-replacement / landing switch-ins (which apply through
/// `switch_into` and so are not in the instruction stream). Hazard `-damage` after the switch is
/// the documented gap (it lives in `switch_into`).
pub fn switch_line(state: &State, side: SideId, hp_style: HpStyle) -> String {
    let slot = state.side(side).active_index;
    let p = &state.sides[side.index()].pokemon[slot as usize];
    format!("|switch|{}|{}|{}", ident(state, side, slot), details(p), hp_frac(p.hp, p.max_hp, hp_style))
}

/// Emit a `|move|USER|Move|TARGET`. PS points the animation at the FOE only for foe-directed
/// targets; self / field / all-targeting moves (Roost, Swords Dance, Haze, Trick Room, Perish
/// Song, …) point at the user.
fn emit_move(out: &mut Vec<String>, s: &State, side: SideId, move_id: crate::ids::MoveId) {
    use crate::data::MoveTarget::*;
    let user = ident_active(s, side);
    // `AllAdjacent` (Surf, Earthquake, Muddy Water, …) hits the foe in singles → foe-directed.
    let foe_directed = matches!(
        move_data(move_id).target,
        Normal | AdjacentFoe | AllAdjacent | AllAdjacentFoes | Any | RandomNormal | Scripted | FoeSide
    );
    let target = if foe_directed { ident_active(s, side.other()) } else { user.clone() };
    out.push(format!("|move|{}|{}|{}", user, prettify(move_id.to_id()), target));
}

fn emit_instruction(
    out: &mut Vec<String>,
    s: &State,
    ins: Instruction,
    hp_style: HpStyle,
    current_move: &Option<(SideId, crate::ids::MoveId)>,
) {
    use Instruction::*;
    match ins {
        Switch { side, next, .. } => {
            let p = &s.sides[side.index()].pokemon[next as usize];
            // |drag| for a forced switch is indistinguishable here; emit |switch|.
            out.push(format!(
                "|switch|{}|{}|{}",
                ident(s, side, next),
                details(p),
                hp_frac(p.hp, p.max_hp, hp_style)
            ));
        }
        Damage { side, slot, amount } if amount != 0 => {
            // Effectiveness line (recomputed from the type chart) precedes the damage, PS-style,
            // when this damage is attributable to the currently-announced attacking move.
            if let Some((atk_side, mv)) = current_move {
                if *atk_side == side.other() {
                    let md = move_data(*mv);
                    if md.category != MoveCategory::Status {
                        let defender = &s.sides[side.index()].pokemon[slot as usize];
                        let eff = type_multiplier(md.typ, defender.types);
                        if eff == 0.0 {
                            out.push(format!("|-immune|{}", ident(s, side, slot)));
                        } else if eff > 1.0 {
                            out.push(format!("|-supereffective|{}", ident(s, side, slot)));
                        } else if eff < 1.0 {
                            out.push(format!("|-resisted|{}", ident(s, side, slot)));
                        }
                    }
                }
            }
            let p = &s.sides[side.index()].pokemon[slot as usize];
            let new_hp = (p.hp - amount).max(0);
            out.push(format!(
                "|-damage|{}|{}",
                ident(s, side, slot),
                hp_frac(new_hp, p.max_hp, hp_style)
            ));
        }
        Heal { side, slot, amount } if amount != 0 => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            let new_hp = (p.hp + amount).min(p.max_hp);
            out.push(format!(
                "|-heal|{}|{}",
                ident(s, side, slot),
                hp_frac(new_hp, p.max_hp, hp_style)
            ));
        }
        ChangeStatus { side, slot, previous, new } => {
            if new != Status::None && new != previous {
                out.push(format!("|-status|{}|{}", ident(s, side, slot), new.to_id()));
            } else if new == Status::None && previous != Status::None {
                out.push(format!("|-curestatus|{}|{}", ident(s, side, slot), previous.to_id()));
            }
        }
        Boost { side, stat, amount } if amount != 0 => {
            let tag = if amount > 0 { "-boost" } else { "-unboost" };
            out.push(format!("|{}|{}|{}|{}", tag, ident_active(s, side), boost_id(stat), amount.abs()));
        }
        ClearBoosts { side, .. } => {
            out.push(format!("|-clearallboost|{}", ident_active(s, side)));
        }
        ChangeWeather { new, .. } if new != Weather::None => {
            out.push(format!("|-weather|{}", weather_name(new)));
        }
        ChangeWeather { new, .. } if new == Weather::None => {
            out.push("|-weather|none".to_string());
        }
        ChangeTerrain { new, .. } if new != Terrain::None => {
            out.push(format!("|-fieldstart|move: {}", terrain_name(new)));
        }
        ChangeTerrain { new, .. } if new == Terrain::None => {
            out.push("|-fieldend|move: Terrain".to_string());
        }
        ToggleTrickRoom { new_turns, .. } => {
            if !s.trick_room {
                out.push("|-fieldstart|move: Trick Room".to_string());
            } else {
                out.push("|-fieldend|move: Trick Room".to_string());
            }
            let _ = new_turns;
        }
        SetSideCondition { side, condition, new, previous } => {
            if new > previous {
                out.push(format!("|-sidestart|{}|{}", side_name(side), condition_name(condition)));
            } else if new < previous {
                out.push(format!("|-sideend|{}|{}", side_name(side), condition_name(condition)));
            }
        }
        ApplyVolatile { side, volatile } if ps_announces_volatile(volatile) => {
            out.push(format!("|-start|{}|{}", ident_active(s, side), volatile_name(volatile)));
        }
        RemoveVolatile { side, volatile } if ps_announces_volatile(volatile) => {
            out.push(format!("|-end|{}|{}", ident_active(s, side), volatile_name(volatile)));
        }
        ChangeItem { side, slot, new, previous } => {
            if new == Item::None && previous != Item::None {
                out.push(format!("|-enditem|{}|{}", ident(s, side, slot), prettify(previous.to_id())));
            } else if new != Item::None {
                out.push(format!("|-item|{}|{}", ident(s, side, slot), prettify(new.to_id())));
            }
        }
        ToggleTerastallized { side, slot } => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            out.push(format!("|-terastallize|{}|{}", ident(s, side, slot), type_name(p.tera_type)));
        }
        _ => {}
    }
}

// ---- identity / formatting ---------------------------------------------------------------

/// PS position ident for a specific party slot's mon: `pNx: Name`. Singles → the active slot is
/// position `a`; a non-active slot uses its would-be letter (only ever appears mid-switch).
fn ident(s: &State, side: SideId, slot: u8) -> String {
    let p = &s.sides[side.index()].pokemon[slot as usize];
    format!("{}: {}", pos_ref(s, side, slot), prettify(p.species.to_id()))
}

fn ident_active(s: &State, side: SideId) -> String {
    ident(s, side, s.side(side).active_index)
}

/// `pNa` in singles. The letter is `a` for the active mon; other slots are only referenced at a
/// switch instant, where PS still uses the incoming active slot `a`.
fn pos_ref(s: &State, side: SideId, slot: u8) -> String {
    let letter = if slot == s.side(side).active_index { 'a' } else { 'a' };
    format!("p{}{}", side.index() + 1, letter)
}

/// PS `|switch|` details field: `Species` (+ `, L{n}` if not 100). Gender omitted (cosmetic).
fn details(p: &crate::state::Pokemon) -> String {
    if p.level != 100 {
        format!("{}, L{}", prettify(p.species.to_id()), p.level)
    } else {
        prettify(p.species.to_id())
    }
}

/// HP fraction. `Percent`: PS public form — `ceil(100*cur/max)/100`, `0 fnt` at 0. `Exact`: `cur/max`.
fn hp_frac(cur: i16, max: i16, style: HpStyle) -> String {
    if cur <= 0 {
        return "0 fnt".to_string();
    }
    match style {
        HpStyle::Exact => format!("{}/{}", cur, max),
        HpStyle::Percent => {
            let m = max.max(1) as i32;
            let pct = ((cur as i32 * 100 + m - 1) / m).clamp(1, 100); // ceil, min 1 while alive
            format!("{}/100", pct)
        }
    }
}

fn side_name(side: SideId) -> &'static str {
    match side {
        SideId::One => "p1: Red",
        SideId::Two => "p2: Blue",
    }
}

fn boost_id(b: BoostIndex) -> &'static str {
    match b {
        BoostIndex::Attack => "atk",
        BoostIndex::Defense => "def",
        BoostIndex::SpecialAttack => "spa",
        BoostIndex::SpecialDefense => "spd",
        BoostIndex::Speed => "spe",
        BoostIndex::Accuracy => "accuracy",
        BoostIndex::Evasion => "evasion",
    }
}

fn weather_name(w: Weather) -> &'static str {
    use Weather::*;
    match w {
        Sun => "SunnyDay",
        Rain => "RainDance",
        Sand => "Sandstorm",
        Snow => "Snow",
        HarshSun => "DesolateLand",
        HeavyRain => "PrimordialSea",
        StrongWinds => "DeltaStream",
        None => "none",
    }
}

fn terrain_name(t: Terrain) -> &'static str {
    use Terrain::*;
    match t {
        Electric => "Electric Terrain",
        Grassy => "Grassy Terrain",
        Misty => "Misty Terrain",
        Psychic => "Psychic Terrain",
        None => "Terrain",
    }
}

fn condition_name(c: SideConditionId) -> &'static str {
    match c {
        SideConditionId::StealthRock => "move: Stealth Rock",
        SideConditionId::Spikes => "Spikes",
        SideConditionId::ToxicSpikes => "move: Toxic Spikes",
        SideConditionId::StickyWeb => "move: Sticky Web",
        SideConditionId::Reflect => "Reflect",
        SideConditionId::LightScreen => "Light Screen",
        SideConditionId::AuroraVeil => "move: Aurora Veil",
        SideConditionId::Tailwind => "move: Tailwind",
    }
}

/// Whether PS emits a `|-start|`/`|-end|` line for this volatile. Engine-internal bookkeeping
/// volatiles (per-turn stat-change flags, the Protean type-shift marker, single-turn markers PS
/// handles via other lines) carry no PS event and are skipped.
fn ps_announces_volatile(v: crate::volatile::VolatileStatus) -> bool {
    use crate::volatile::VolatileStatus::*;
    // ChoiceLock, Protosynthesis/QuarkDrive (announced by PS as [silent]/-activate, not -start),
    // and the single-turn / internal bookkeeping volatiles carry no engine-visible PS -start line.
    !matches!(
        v,
        StatsRaisedThisTurn | StatsLoweredThisTurn | TypeShifted | Roosted | Protect | Endure
            | Flinch | Charge | MustRecharge | LockedMove | Unburden | ChoiceLock
            | Protosynthesis | QuarkDrive
    )
}

fn volatile_name(v: crate::volatile::VolatileStatus) -> String {
    use crate::volatile::VolatileStatus::*;
    match v {
        Confusion => "confusion".into(),
        Substitute => "Substitute".into(),
        LeechSeed => "move: Leech Seed".into(),
        Taunt => "move: Taunt".into(),
        Encore => "move: Encore".into(),
        Disable => "Disable".into(),
        other => prettify(&format!("{other:?}")),
    }
}

fn type_name(t: Type) -> String {
    let id = t.to_id();
    let mut c = id.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// "makeitrain" -> "Makeitrain". Cosmetic; a proper display-name table is future work.
fn prettify(id: &str) -> String {
    let mut c = id.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emits_turn_and_upkeep() {
        let mut s = State::EMPTY;
        s.turn = 5;
        let mut p = crate::state::Pokemon::EMPTY;
        p.species = crate::ids::Species::from_id("blissey").unwrap();
        p.hp = 100;
        p.max_hp = 200;
        p.moves[0] = crate::state::MoveSlot { id: crate::ids::MoveId::from_id("softboiled").unwrap(), pp: 10, max_pp: 16, disabled: false };
        s.sides[0].pokemon[0] = p;
        s.sides[0].active_index = 0;
        s.sides[1].pokemon[0] = p;
        s.sides[1].active_index = 0;
        let mut out = Vec::new();
        // A move is announced at its DecrementPp (the move-use marker), not from the choice.
        let ins = [Instruction::DecrementPp { side: SideId::One, slot: 0, move_index: 0, amount: 1 }];
        protocol_turn(&s, MoveChoice::Move(0), MoveChoice::Move(0), &ins, HpStyle::Percent, &mut out);
        assert_eq!(out.first().unwrap(), "|turn|5");
        assert_eq!(out.last().unwrap(), "|upkeep");
        assert!(out.iter().any(|l| l.starts_with("|move|p1a: Blissey|Softboiled")));
    }

    #[test]
    fn hp_percent_and_faint() {
        assert_eq!(hp_frac(0, 200, HpStyle::Percent), "0 fnt");
        assert_eq!(hp_frac(100, 200, HpStyle::Percent), "50/100");
        assert_eq!(hp_frac(1, 200, HpStyle::Percent), "1/100");
        assert_eq!(hp_frac(100, 200, HpStyle::Exact), "100/200");
    }
}
