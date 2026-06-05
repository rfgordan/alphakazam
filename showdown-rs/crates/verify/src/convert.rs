//! Conversion between a trace's `TState` (Pokémon Showdown ground truth) and the
//! engine's `State`.
//!
//! `parse_state` builds an engine `State` from PS data, recording any ids outside the
//! current enum slice into `unmapped` so the runner can report exactly what coverage is
//! missing instead of silently guessing. `project_state` is the inverse, used to diff
//! the engine's representation back against PS.

use engine::ids::{Ability, BoostIndex, Item, MoveId, Species, StatIndex, Status, Terrain, Type, Weather};
use engine::state::{MoveSlot, PendingMove, Pokemon, Side, State};
use engine::volatile::VolatileStatus;

use crate::trace::*;

/// Collects ids encountered in a trace that the engine's slice does not know about.
#[derive(Default)]
pub struct Unmapped {
    pub items: Vec<String>,
}

impl Unmapped {
    fn note(&mut self, category: &str, id: &str) {
        let entry = format!("{category}:{id}");
        if !self.items.contains(&entry) {
            self.items.push(entry);
        }
    }
}

fn parse_types(types: &[String], unmapped: &mut Unmapped) -> [Type; 2] {
    let mut out = [Type::None, Type::None];
    for (i, t) in types.iter().take(2).enumerate() {
        match Type::from_id(t) {
            Some(ty) => out[i] = ty,
            None => unmapped.note("type", t),
        }
    }
    out
}

fn parse_pokemon(tp: &TPokemon, unmapped: &mut Unmapped) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(&tp.species).unwrap_or_else(|| {
        unmapped.note("species", &tp.species);
        Species::None
    });
    p.level = tp.level;
    p.types = parse_types(&tp.types, unmapped);
    p.base_types = p.types;
    p.hp = tp.hp;
    p.max_hp = tp.max_hp;
    p.status = Status::from_id(&tp.status).unwrap_or_else(|| {
        unmapped.note("status", &tp.status);
        Status::None
    });
    p.status_counter = tp.status_counter;
    p.ability = Ability::from_id(&tp.ability).unwrap_or_else(|| {
        unmapped.note("ability", &tp.ability);
        Ability::None
    });
    p.base_ability = p.ability;
    p.item = Item::from_id(&tp.item).unwrap_or_else(|| {
        unmapped.note("item", &tp.item);
        // Preserve "holds an item" for unmodeled items (Knock Off, Acrobatics, ...).
        if tp.item.is_empty() || tp.item == "none" { Item::None } else { Item::Other }
    });
    p.stats[StatIndex::Hp as usize] = tp.max_hp;
    p.stats[StatIndex::Attack as usize] = tp.stats.atk;
    p.stats[StatIndex::Defense as usize] = tp.stats.def;
    p.stats[StatIndex::SpecialAttack as usize] = tp.stats.spa;
    p.stats[StatIndex::SpecialDefense as usize] = tp.stats.spd;
    p.stats[StatIndex::Speed as usize] = tp.stats.spe;
    p.terastallized = tp.terastallized;
    p.tera_type = Type::from_id(&tp.tera_type).unwrap_or(Type::None);
    p.times_hit = tp.times_hit;
    p.ability_used = tp.ability_used;
    for (i, m) in tp.moves.iter().take(4).enumerate() {
        let id = MoveId::from_id(&m.id).unwrap_or_else(|| {
            unmapped.note("move", &m.id);
            MoveId::None
        });
        p.moves[i] = MoveSlot { id, pp: m.pp, max_pp: m.max_pp, disabled: m.disabled };
    }
    p
}

/// Parse the trace's pending-move string ("charge:<move>" / "recharge" / "rampage:<m>:<n>").
fn parse_pending(s: &str) -> PendingMove {
    let mut it = s.split(':');
    match it.next() {
        Some("charge") => match it.next().and_then(MoveId::from_id) {
            Some(m) => PendingMove::Charging(m),
            None => PendingMove::None,
        },
        Some("recharge") => PendingMove::Recharging,
        Some("rampage") => {
            let m = it.next().and_then(MoveId::from_id).unwrap_or(MoveId::None);
            let n = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
            PendingMove::Rampaging(m, n)
        }
        _ => PendingMove::None,
    }
}

/// Encode a `PendingMove` back into the trace string form.
fn pending_to_string(p: PendingMove) -> String {
    match p {
        PendingMove::None => String::new(),
        PendingMove::Charging(m) => format!("charge:{}", m.to_id()),
        PendingMove::Recharging => "recharge".to_string(),
        PendingMove::Rampaging(m, n) => format!("rampage:{}:{}", m.to_id(), n),
    }
}

fn parse_side(ts: &TSide, unmapped: &mut Unmapped) -> Side {
    let mut s = Side::EMPTY;
    s.active_index = ts.active_index;
    s.boosts[BoostIndex::Attack as usize] = ts.boosts.atk;
    s.boosts[BoostIndex::Defense as usize] = ts.boosts.def;
    s.boosts[BoostIndex::SpecialAttack as usize] = ts.boosts.spa;
    s.boosts[BoostIndex::SpecialDefense as usize] = ts.boosts.spd;
    s.boosts[BoostIndex::Speed as usize] = ts.boosts.spe;
    s.boosts[BoostIndex::Accuracy as usize] = ts.boosts.accuracy;
    s.boosts[BoostIndex::Evasion as usize] = ts.boosts.evasion;
    for v in &ts.volatiles {
        match VolatileStatus::from_id(v) {
            Some(vs) => {
                s.volatiles.insert(vs);
            }
            None => unmapped.note("volatile", v),
        }
    }
    s.substitute_hp = ts.substitute_hp;
    s.last_used_move = MoveId::from_id(&ts.last_used_move).unwrap_or(MoveId::None);
    // PS stores the Protect "stall" as the next failure divisor (1, 3, 9, …); the engine
    // stores the exponent n (success = 1/3ⁿ), so n = round(log₃(divisor)).
    s.stall_counter = match ts.stall_counter {
        0 | 1 => 0,
        d => (d as f32).log(3.0).round() as u8,
    };
    s.pending_move = parse_pending(&ts.pending_move);
    s.active_turns = ts.active_turns;
    let sc = &ts.side_conditions;
    s.side_conditions.stealth_rock = sc.stealth_rock;
    s.side_conditions.spikes = sc.spikes;
    s.side_conditions.toxic_spikes = sc.toxic_spikes;
    s.side_conditions.sticky_web = sc.sticky_web;
    s.side_conditions.reflect = sc.reflect;
    s.side_conditions.light_screen = sc.light_screen;
    s.side_conditions.aurora_veil = sc.aurora_veil;
    s.side_conditions.tailwind = sc.tailwind;
    for (i, tp) in ts.pokemon.iter().take(6).enumerate() {
        s.pokemon[i] = parse_pokemon(tp, unmapped);
    }
    s
}

/// Build an engine `State` from a trace's PS state.
pub fn parse_state(ts: &TState, unmapped: &mut Unmapped) -> State {
    let mut state = State::EMPTY;
    state.turn = ts.turn;
    state.weather = Weather::from_id(&ts.weather).unwrap_or_else(|| {
        unmapped.note("weather", &ts.weather);
        Weather::None
    });
    state.weather_turns = ts.weather_turns;
    state.terrain = Terrain::from_id(&ts.terrain).unwrap_or_else(|| {
        unmapped.note("terrain", &ts.terrain);
        Terrain::None
    });
    state.terrain_turns = ts.terrain_turns;
    state.trick_room = ts.trick_room;
    state.trick_room_turns = ts.trick_room_turns;
    for (i, side) in ts.sides.iter().take(2).enumerate() {
        state.sides[i] = parse_side(side, unmapped);
    }
    state
}

// --- inverse: engine State -> TState ----------------------------------------

fn project_types(types: [Type; 2]) -> Vec<String> {
    types
        .iter()
        .filter(|t| **t != Type::None)
        .map(|t| t.to_id().to_string())
        .collect()
}

fn project_pokemon(p: &Pokemon) -> TPokemon {
    TPokemon {
        species: p.species.to_id().to_string(),
        level: p.level,
        types: project_types(p.types),
        hp: p.hp,
        max_hp: p.max_hp,
        status: p.status.to_id().to_string(),
        status_counter: p.status_counter,
        ability: p.ability.to_id().to_string(),
        item: p.item.to_id().to_string(),
        stats: TStats {
            atk: p.stats[StatIndex::Attack as usize],
            def: p.stats[StatIndex::Defense as usize],
            spa: p.stats[StatIndex::SpecialAttack as usize],
            spd: p.stats[StatIndex::SpecialDefense as usize],
            spe: p.stats[StatIndex::Speed as usize],
        },
        terastallized: p.terastallized,
        tera_type: p.tera_type.to_id().to_string(),
        moves: p
            .moves
            .iter()
            .filter(|m| m.id != MoveId::None)
            .map(|m| TMove {
                id: m.id.to_id().to_string(),
                pp: m.pp,
                max_pp: m.max_pp,
                disabled: m.disabled,
            })
            .collect(),
        times_hit: p.times_hit,
        ability_used: p.ability_used,
    }
}

fn project_volatiles(side: &Side) -> Vec<String> {
    // Iterate every known volatile and keep those present in the bitset.
    const ALL: &[VolatileStatus] = &[
        VolatileStatus::Confusion,
        VolatileStatus::Substitute,
        VolatileStatus::LeechSeed,
        VolatileStatus::Taunt,
        VolatileStatus::Encore,
        VolatileStatus::Disable,
        VolatileStatus::Protect,
        VolatileStatus::Endure,
        VolatileStatus::Flinch,
        VolatileStatus::Roost,
        VolatileStatus::Charge,
        VolatileStatus::Yawn,
        VolatileStatus::PerishSong,
        VolatileStatus::DestinyBond,
        VolatileStatus::Curse,
        VolatileStatus::Nightmare,
        VolatileStatus::Attract,
        VolatileStatus::Torment,
        VolatileStatus::SaltCure,
        VolatileStatus::GlaiveRush,
        VolatileStatus::LockedMove,
        VolatileStatus::MustRecharge,
        VolatileStatus::PartiallyTrapped,
        VolatileStatus::Roosted,
        VolatileStatus::ChoiceLock,
        VolatileStatus::Protosynthesis,
        VolatileStatus::QuarkDrive,
    ];
    let mut out: Vec<String> = ALL
        .iter()
        .filter(|v| side.volatiles.contains(**v))
        .map(|v| v.to_id().to_string())
        .collect();
    out.sort();
    out
}

fn project_side(s: &Side) -> TSide {
    TSide {
        active_index: s.active_index,
        boosts: TBoosts {
            atk: s.boosts[BoostIndex::Attack as usize],
            def: s.boosts[BoostIndex::Defense as usize],
            spa: s.boosts[BoostIndex::SpecialAttack as usize],
            spd: s.boosts[BoostIndex::SpecialDefense as usize],
            spe: s.boosts[BoostIndex::Speed as usize],
            accuracy: s.boosts[BoostIndex::Accuracy as usize],
            evasion: s.boosts[BoostIndex::Evasion as usize],
        },
        volatiles: project_volatiles(s),
        substitute_hp: s.substitute_hp,
        side_conditions: TSideConditions {
            stealth_rock: s.side_conditions.stealth_rock,
            spikes: s.side_conditions.spikes,
            toxic_spikes: s.side_conditions.toxic_spikes,
            sticky_web: s.side_conditions.sticky_web,
            reflect: s.side_conditions.reflect,
            light_screen: s.side_conditions.light_screen,
            aurora_veil: s.side_conditions.aurora_veil,
            tailwind: s.side_conditions.tailwind,
        },
        // Emit only real Pokémon (the engine always carries 6 array slots; unused ones
        // are `Species::None`) so array length matches the PS team size.
        pokemon: s
            .pokemon
            .iter()
            .filter(|p| p.species != Species::None)
            .map(project_pokemon)
            .collect(),
        last_used_move: s.last_used_move.to_id().to_string(),
        // Project the exponent n back to PS's divisor (3ⁿ) for symmetry with parse_side.
        stall_counter: 3u32.pow(s.stall_counter.min(8) as u32).min(u16::MAX as u32) as u16,
        pending_move: pending_to_string(s.pending_move),
        active_turns: s.active_turns,
    }
}

/// Project an engine `State` back into the trace `TState` shape for diffing.
pub fn project_state(state: &State) -> TState {
    TState {
        turn: state.turn,
        weather: state.weather.to_id().to_string(),
        weather_turns: state.weather_turns,
        terrain: state.terrain.to_id().to_string(),
        terrain_turns: state.terrain_turns,
        trick_room: state.trick_room,
        trick_room_turns: state.trick_room_turns,
        sides: state.sides.iter().map(project_side).collect(),
    }
}
