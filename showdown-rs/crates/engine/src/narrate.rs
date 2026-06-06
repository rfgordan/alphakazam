//! Human-readable battle commentary.
//!
//! A **separate render layer**, not part of the turn engine: it reads `(pre-state, the two
//! actions, the resulting instruction stream)` and reports text — exactly the same shape as the
//! observation encoder, just aimed at a person instead of a network. The hot path emits nothing;
//! this is computed only when someone wants to watch (e.g. the bridge's `step(narrate=True)`).
//!
//! It works by replaying the chosen branch's instructions on a throwaway clone, so HP
//! percentages, names, and faints are read from the exact state at each step. Move/species
//! names are PS `toID` strings, lightly prettified (a proper display-name table is future work).

use crate::generate::MoveChoice;
use crate::ids::{BoostIndex, Status};
use crate::instruction::{Instruction, SideConditionId};
use crate::state::{SideId, State};

/// Produce the commentary lines for one resolved turn.
///
/// `a1`/`a2` are the side-One / side-Two choices; `instructions` is the chosen outcome branch.
pub fn narrate_turn(pre: &State, a1: MoveChoice, a2: MoveChoice, instructions: &[Instruction]) -> Vec<String> {
    let mut lines = Vec::new();
    lines.push(format!("\u{2500}\u{2500} Turn {} \u{2500}\u{2500}", pre.turn));

    // Announce move actions up front, in turn order (switches narrate themselves below, when
    // their Switch instruction resolves). Priority first, then speed (Trick Room flips speed).
    let mut movers: Vec<(SideId, MoveChoice)> = vec![(SideId::One, a1), (SideId::Two, a2)];
    movers.sort_by(|&(sa, ca), &(sb, cb)| order_key(pre, sb, cb).cmp(&order_key(pre, sa, ca)));
    for (side, choice) in movers {
        if let MoveChoice::Move(idx) = choice {
            let p = pre.side(side).active();
            let move_id = p.moves[idx as usize].id;
            if move_id != crate::ids::MoveId::None {
                lines.push(format!(
                    "{}'s {} used {}!",
                    player(side),
                    prettify(p.species.to_id()),
                    prettify(move_id.to_id())
                ));
            }
        }
    }

    // Walk the instruction stream on a clone, narrating consequences in resolution order.
    let mut s = *pre;
    for &ins in instructions {
        narrate_instruction(&mut lines, &s, ins);
        s.apply_one(ins);
        // A faint is visible only after the damage applies.
        if let Instruction::Damage { side, slot, .. } = ins {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            if p.hp <= 0 {
                lines.push(format!("{}'s {} fainted!", player(side), prettify(p.species.to_id())));
            }
        }
    }
    lines
}

fn narrate_instruction(lines: &mut Vec<String>, s: &State, ins: Instruction) {
    use Instruction::*;
    match ins {
        Switch { side, next, .. } => {
            let p = &s.sides[side.index()].pokemon[next as usize];
            lines.push(format!("{} sent out {}!", player(side), prettify(p.species.to_id())));
        }
        Damage { side, slot, amount } => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            lines.push(format!(
                "{}'s {} lost {}% HP.",
                player(side),
                prettify(p.species.to_id()),
                pct(amount, p.max_hp)
            ));
        }
        Heal { side, slot, amount } => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            lines.push(format!(
                "{}'s {} restored {}% HP.",
                player(side),
                prettify(p.species.to_id()),
                pct(amount, p.max_hp)
            ));
        }
        ChangeStatus { side, slot, new, .. } if new != Status::None => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            lines.push(format!("{}'s {} {}.", player(side), prettify(p.species.to_id()), status_phrase(new)));
        }
        Boost { side, stat, amount } if amount != 0 => {
            let name = prettify(s.side(side).active().species.to_id());
            let (dir, mag) = if amount > 0 { ("rose", amount) } else { ("fell", -amount) };
            let qualifier = match mag {
                1 => "",
                2 => " sharply",
                _ => " drastically",
            };
            lines.push(format!("{}'s {}'s {} {}{}!", player(side), name, stat_name(stat), dir, qualifier));
        }
        ChangeWeather { new, .. } if new != crate::ids::Weather::None => {
            lines.push(weather_phrase(new).to_string());
        }
        ChangeTerrain { new, .. } if new != crate::ids::Terrain::None => {
            lines.push(terrain_phrase(new).to_string());
        }
        SetSideCondition { side, condition, new, previous } if new > previous => {
            lines.push(format!("{} on {}'s side.", hazard_phrase(condition), player(side)));
        }
        ChangeItem { side, slot, new, previous } if new == crate::ids::Item::None && previous != crate::ids::Item::None => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            lines.push(format!("{}'s {} lost its item!", player(side), prettify(p.species.to_id())));
        }
        ToggleTerastallized { side, slot } => {
            let p = &s.sides[side.index()].pokemon[slot as usize];
            lines.push(format!(
                "{}'s {} Terastallized into {}!",
                player(side),
                prettify(p.species.to_id()),
                prettify(p.tera_type.to_id())
            ));
        }
        _ => {} // internal bookkeeping (last-move, streak, reveal, pending, ...) — nothing to say
    }
}

/// Sort key for turn order: higher = acts first. (priority, effective speed).
fn order_key(state: &State, side: SideId, choice: MoveChoice) -> (i16, i32) {
    // Switches act before all moves.
    let priority = match choice {
        MoveChoice::Switch(_) => 100,
        MoveChoice::Move(idx) => {
            let id = state.side(side).active().moves[idx as usize].id;
            crate::data::move_data(id).priority as i16
        }
    };
    let mut speed = state.side(side).active().stat(crate::ids::StatIndex::Speed) as i32;
    if state.trick_room {
        speed = -speed; // slower acts first under Trick Room
    }
    (priority, speed)
}

fn player(side: SideId) -> &'static str {
    match side {
        SideId::One => "Red",
        SideId::Two => "Blue",
    }
}

fn pct(amount: i16, max_hp: i16) -> i32 {
    let m = max_hp.max(1) as i32;
    ((amount.abs() as i32 * 100 + m / 2) / m).clamp(0, 100)
}

fn status_phrase(s: Status) -> &'static str {
    match s {
        Status::Burn => "was burned",
        Status::Paralysis => "was paralyzed",
        Status::Sleep => "fell asleep",
        Status::Freeze => "was frozen",
        Status::Poison => "was poisoned",
        Status::Toxic => "was badly poisoned",
        Status::None => "",
    }
}

fn stat_name(b: BoostIndex) -> &'static str {
    match b {
        BoostIndex::Attack => "Attack",
        BoostIndex::Defense => "Defense",
        BoostIndex::SpecialAttack => "Sp. Atk",
        BoostIndex::SpecialDefense => "Sp. Def",
        BoostIndex::Speed => "Speed",
        BoostIndex::Accuracy => "Accuracy",
        BoostIndex::Evasion => "Evasion",
    }
}

fn weather_phrase(w: crate::ids::Weather) -> &'static str {
    use crate::ids::Weather::*;
    match w {
        Sun | HarshSun => "The sunlight turned harsh!",
        Rain | HeavyRain => "It started to rain!",
        Sand => "A sandstorm kicked up!",
        Snow => "It started to snow!",
        StrongWinds => "Mysterious strong winds are protecting Flying-types!",
        None => "",
    }
}

fn terrain_phrase(t: crate::ids::Terrain) -> &'static str {
    use crate::ids::Terrain::*;
    match t {
        Electric => "An electric current ran across the battlefield!",
        Grassy => "Grass grew to cover the battlefield!",
        Misty => "Mist swirled around the battlefield!",
        Psychic => "The battlefield got weird!",
        None => "",
    }
}

fn hazard_phrase(c: SideConditionId) -> &'static str {
    match c {
        SideConditionId::StealthRock => "Pointed stones float in the air",
        SideConditionId::Spikes => "Spikes were scattered",
        SideConditionId::ToxicSpikes => "Poison spikes were scattered",
        SideConditionId::StickyWeb => "A sticky web spread out",
        SideConditionId::Reflect => "Reflect raised Defense",
        SideConditionId::LightScreen => "Light Screen raised Sp. Def",
        SideConditionId::AuroraVeil => "Aurora Veil went up",
        SideConditionId::Tailwind => "The tailwind blew",
    }
}

/// "makeitrain" -> "Makeitrain". A light touch; full display names need a name table.
fn prettify(id: &str) -> String {
    let mut c = id.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
