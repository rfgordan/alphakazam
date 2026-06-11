//! Build playable battle states from scratch (for self-play, tests, and the Python bridge).
//!
//! The engine normally reads states out of Pokémon Showdown traces; for RL self-play we need
//! to construct fresh teams without a trace. A `MemberSpec` is a compact, declarative set; the
//! builder computes final stats via [`crate::damage::compute_stat`]/[`compute_hp`] and looks
//! everything else up from the data tables. Unknown abilities/items degrade to `None` (the
//! battle stays valid), so a spec can name anything PS knows even if the engine doesn't model
//! its effect yet.

use crate::damage::{compute_hp, compute_stat};
use crate::data::{base_stats, species_types};
use crate::ids::{Ability, Item, MoveId, Nature, Species, StatIndex, Type};
use crate::state::{MoveSlot, Pokemon, Side, State};

const DEFAULT_IV: u8 = 31;

/// A declarative team member. All identifiers are PS `toID` strings.
#[derive(Debug, Clone, Copy)]
pub struct MemberSpec {
    pub species: &'static str,
    pub ability: &'static str,
    pub item: &'static str,
    pub tera: &'static str,
    pub nature: Nature,
    pub evs: [u8; 6],
    pub moves: [&'static str; 4],
}

/// Construct a fully-statted Pokémon from a spec at the given level.
pub fn build_member(spec: &MemberSpec, level: u8) -> Pokemon {
    let species = Species::from_id(spec.species)
        .unwrap_or_else(|| panic!("team builder: unknown species id {:?}", spec.species));
    let base = base_stats(species);
    let types = species_types(species);

    let mut stats = [0i16; StatIndex::COUNT];
    stats[StatIndex::Hp as usize] = compute_hp(base[0], DEFAULT_IV, spec.evs[0], level);
    for (si, stat) in [
        StatIndex::Attack,
        StatIndex::Defense,
        StatIndex::SpecialAttack,
        StatIndex::SpecialDefense,
        StatIndex::Speed,
    ]
    .into_iter()
    .enumerate()
    {
        let idx = si + 1;
        stats[idx] = compute_stat(base[idx], DEFAULT_IV, spec.evs[idx], level, spec.nature, stat);
    }
    let hp = stats[StatIndex::Hp as usize];

    let mut moves = [MoveSlot::EMPTY; 4];
    for (i, m) in spec.moves.iter().enumerate() {
        if let Some(id) = MoveId::from_id(m) {
            if id != MoveId::None {
                // PS default sets carry max PP Ups: max PP = base × 8/5.
                let pp = (crate::data::move_data(id).pp as u16 * 8 / 5) as u8;
                moves[i] = MoveSlot { id, pp, max_pp: pp, disabled: false };
            }
        }
    }

    let ability = Ability::from_id(spec.ability).unwrap_or(Ability::None);
    Pokemon {
        species,
        level,
        types,
        base_types: types,
        base_species: species,
        base_stats: stats,
        base_moves: moves,
        hp,
        max_hp: hp,
        stats,
        ability,
        base_ability: ability,
        item: Item::from_id(spec.item).unwrap_or(Item::None),
        nature: spec.nature,
        evs: spec.evs,
        moves,
        tera_type: Type::from_id(spec.tera).unwrap_or(Type::None),
        ..Pokemon::EMPTY
    }
}

/// Build a full battle `State` from two teams (up to 6 each), both active on slot 0.
pub fn build_state(team1: &[MemberSpec], team2: &[MemberSpec], level: u8) -> State {
    let make_side = |team: &[MemberSpec]| -> Side {
        let mut pokemon = [Pokemon::EMPTY; 6];
        for (i, spec) in team.iter().take(6).enumerate() {
            pokemon[i] = build_member(spec, level);
        }
        Side { pokemon, ..Side::EMPTY }
    };
    State {
        sides: [make_side(team1), make_side(team2)],
        ..State::EMPTY
    }
}

// --- A pair of plausible gen9 OU teams, for a default self-play matchup ---------------------

const N: [u8; 6] = [0, 0, 0, 0, 0, 0]; // neutral EVs; symmetric and valid (spreads don't matter for RL)

/// "Red" — a balanced gen9 OU-flavoured team.
pub fn red_team() -> [MemberSpec; 6] {
    use Nature::*;
    [
        MemberSpec { species: "greattusk", ability: "protosynthesis", item: "heavydutyboots", tera: "ground", nature: Jolly, evs: N,
            moves: ["headlongrush", "closecombat", "rapidspin", "stealthrock"] },
        MemberSpec { species: "gholdengo", ability: "goodasgold", item: "choicescarf", tera: "flying", nature: Timid, evs: N,
            moves: ["makeitrain", "shadowball", "thunderbolt", "focusblast"] },
        MemberSpec { species: "kingambit", ability: "supremeoverlord", item: "leftovers", tera: "dark", nature: Adamant, evs: N,
            moves: ["kowtowcleave", "suckerpunch", "ironhead", "swordsdance"] },
        MemberSpec { species: "dragapult", ability: "clearbody", item: "heavydutyboots", tera: "dragon", nature: Naive, evs: N,
            moves: ["dragondarts", "shadowball", "uturn", "dracometeor"] },
        MemberSpec { species: "corviknight", ability: "pressure", item: "leftovers", tera: "flying", nature: Impish, evs: N,
            moves: ["bravebird", "bodypress", "roost", "uturn"] },
        MemberSpec { species: "garganacl", ability: "purifyingsalt", item: "leftovers", tera: "water", nature: Careful, evs: N,
            moves: ["saltcure", "recover", "bodypress", "earthquake"] },
    ]
}

/// "Blue" — a second balanced gen9 OU-flavoured team.
pub fn blue_team() -> [MemberSpec; 6] {
    use Nature::*;
    [
        MemberSpec { species: "ironvaliant", ability: "quarkdrive", item: "leftovers", tera: "fairy", nature: Timid, evs: N,
            moves: ["moonblast", "thunderbolt", "closecombat", "calmmind"] },
        MemberSpec { species: "roaringmoon", ability: "protosynthesis", item: "heavydutyboots", tera: "flying", nature: Jolly, evs: N,
            moves: ["dragondance", "knockoff", "earthquake", "acrobatics"] },
        MemberSpec { species: "ragingbolt", ability: "protosynthesis", item: "leftovers", tera: "electric", nature: Modest, evs: N,
            moves: ["thunderclap", "dracometeor", "thunderbolt", "calmmind"] },
        MemberSpec { species: "slowkinggalar", ability: "regenerator", item: "leftovers", tera: "water", nature: Calm, evs: N,
            moves: ["sludgebomb", "flamethrower", "icebeam", "slackoff"] },
        MemberSpec { species: "dragonite", ability: "multiscale", item: "heavydutyboots", tera: "flying", nature: Adamant, evs: N,
            moves: ["dragondance", "earthquake", "firepunch", "roost"] },
        MemberSpec { species: "toxapex", ability: "regenerator", item: "leftovers", tera: "steel", nature: Bold, evs: N,
            moves: ["surf", "sludgebomb", "recover", "haze"] },
    ]
}

/// The default self-play matchup: `red_team()` vs `blue_team()` at level 100.
pub fn default_matchup() -> State {
    build_state(&red_team(), &blue_team(), 100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate::{generate_instructions, MoveChoice};
    use crate::narrate::narrate_turn;

    fn side_lost(state: &State, side: crate::state::SideId) -> bool {
        !state.side(side).pokemon.iter().any(|p| p.species != Species::None && p.is_alive())
    }

    /// First legal action for a side: a usable move if the active is alive, else a switch to
    /// the first alive benched mon.
    fn first_legal(state: &State, side: crate::state::SideId) -> Option<MoveChoice> {
        let s = state.side(side);
        let active = s.active();
        if active.is_alive() {
            for (i, m) in active.moves.iter().enumerate() {
                if m.id != MoveId::None && m.pp > 0 {
                    return Some(MoveChoice::Move(i as u8));
                }
            }
        }
        for (i, p) in s.pokemon.iter().enumerate() {
            if i as u8 != s.active_index && p.species != Species::None && p.is_alive() {
                return Some(MoveChoice::Switch(i as u8));
            }
        }
        None
    }

    #[test]
    fn default_matchup_plays_a_full_game() {
        use crate::state::SideId;
        let mut state = default_matchup();
        // Sanity: teams built with real stats.
        assert!(state.side(SideId::One).active().max_hp > 0);
        assert_eq!(state.side(SideId::One).active().stats.len(), 6);

        let mut total_lines = 0;
        let mut ended = false;
        for _ in 0..300 {
            if side_lost(&state, SideId::One) || side_lost(&state, SideId::Two) {
                ended = true;
                break;
            }
            let a1 = first_legal(&state, SideId::One).expect("side One has a legal action");
            let a2 = first_legal(&state, SideId::Two).expect("side Two has a legal action");
            let branches = generate_instructions(&state, a1, a2);
            assert!(!branches.is_empty(), "transition produced no branches");
            let lines = narrate_turn(&state, a1, a2, &branches[0].instructions);
            total_lines += lines.len();
            state.apply_instructions(&branches[0].instructions);
            state.turn += 1;
        }
        // The deterministic "always first move" policy must resolve to a winner well within 300
        // turns, and narration must produce real commentary.
        assert!(ended, "game did not terminate");
        assert!(total_lines > 10, "narration produced too few lines: {total_lines}");
    }
}
