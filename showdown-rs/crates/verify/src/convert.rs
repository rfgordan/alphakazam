//! Conversion between a trace's `TState` (Pokémon Showdown ground truth) and the
//! engine's `State`.
//!
//! `parse_state` builds an engine `State` from PS data, recording any ids outside the
//! current enum slice into `unmapped` so the runner can report exactly what coverage is
//! missing instead of silently guessing. `project_state` is the inverse, used to diff
//! the engine's representation back against PS.

use engine::ids::{Ability, BoostIndex, Item, MoveId, Species, StatIndex, Status, Terrain, Type, Weather};
use engine::state::{MoveSlot, Pokemon, Side, State};
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
        Item::None
    });
    p.stats[StatIndex::Hp as usize] = tp.max_hp;
    p.stats[StatIndex::Attack as usize] = tp.stats.atk;
    p.stats[StatIndex::Defense as usize] = tp.stats.def;
    p.stats[StatIndex::SpecialAttack as usize] = tp.stats.spa;
    p.stats[StatIndex::SpecialDefense as usize] = tp.stats.spd;
    p.stats[StatIndex::Speed as usize] = tp.stats.spe;
    p.terastallized = tp.terastallized;
    p.tera_type = Type::from_id(&tp.tera_type).unwrap_or(Type::None);
    for (i, m) in tp.moves.iter().take(4).enumerate() {
        let id = MoveId::from_id(&m.id).unwrap_or_else(|| {
            unmapped.note("move", &m.id);
            MoveId::None
        });
        p.moves[i] = MoveSlot { id, pp: m.pp, max_pp: m.max_pp, disabled: m.disabled };
    }
    p
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
