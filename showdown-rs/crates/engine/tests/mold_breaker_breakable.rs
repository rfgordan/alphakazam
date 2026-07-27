//! Mold Breaker suppresses `flags: { breakable: 1 }` abilities, and nothing else.
//!
//! PS `sim/battle.ts:836` — `if (effect.effectType === 'Ability' && effect.flags['breakable'] &&
//! this.suppressingAbility(effectHolder)) continue;`, with `suppressingAbility` (`:365`) meaning
//! "an active move is resolving and it has `ignoreAbility`". The engine used to blank the
//! defender's ability wholesale in `compute_damage`, which deleted the abilities PS deliberately
//! left OUT of the flag. The three witnesses are the scoreboard's Mold Breaker bucket:
//!
//! * rb1612 — **Shadow Shield** (`flags: {}`) is not Multiscale (`breakable: 1`).
//! * rb1588 — the four **Ruin** abilities (`flags: {}`).
//! * rb1430 — **Contrary** IS `breakable: 1`, and the engine was not suppressing it, so a Mold
//!   Breaker Play Rough's −1 Attack came out as a raise.

use engine::generate::generate_instructions;
use engine::ids::{Ability, BoostIndex, MoveId, Species, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::MoveChoice;

fn mon(species: &str, ability: &str, m: &str) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = engine::data::species_types(p.species);
    p.base_types = p.types;
    p.hp = 400;
    p.max_hp = 400;
    p.stats = [400, 250, 200, 250, 200, 200];
    p.ability = Ability::from_id(ability).unwrap();
    p.moves[0] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    p
}

/// Side One attacks; side Two defends with `splash`. Returns the defender's HP loss on the
/// modal (non-crit, average-roll) branch — the max over branches is enough to separate a ×0.5.
fn damage_taken(atk_ability: &str, def_species: &str, def_ability: &str, mv: &str) -> i16 {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("excadrill", atk_ability, mv);
    s.sides[0].pokemon[0].stats[5] = 400; // moves first
    s.sides[1].pokemon[0] = mon(def_species, def_ability, "splash");
    s.sides[1].pokemon[0].stats[5] = 1;
    let hp0 = s.sides[1].pokemon[0].hp;
    generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .map(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            hp0 - r.sides[1].pokemon[0].hp
        })
        .max()
        .unwrap()
}

#[test]
fn shadow_shield_is_not_breakable() {
    // Lunala at full HP: Shadow Shield halves, with or without Mold Breaker.
    let plain = damage_taken("sandforce", "lunala", "shadowshield", "ironhead");
    let breaker = damage_taken("moldbreaker", "lunala", "shadowshield", "ironhead");
    assert_eq!(plain, breaker, "Shadow Shield has flags: {{}} — Mold Breaker must not pierce it");
}

#[test]
fn multiscale_is_breakable() {
    // The control: same shape, an ability that IS flagged.
    let plain = damage_taken("sandforce", "dragonite", "multiscale", "ironhead");
    let breaker = damage_taken("moldbreaker", "dragonite", "multiscale", "ironhead");
    assert!(breaker > plain, "Multiscale has breakable: 1 — Mold Breaker must pierce it");
}

#[test]
fn tablets_of_ruin_is_not_breakable() {
    let plain = damage_taken("sandforce", "wochien", "tabletsofruin", "ironhead");
    let breaker = damage_taken("moldbreaker", "wochien", "tabletsofruin", "ironhead");
    assert_eq!(plain, breaker, "the Ruin abilities have flags: {{}}");
}

#[test]
fn contrary_is_breakable() {
    // Play Rough's 10% −1 Attack secondary into a Contrary holder. Enumeration gives both the
    // proc and no-proc branches; the proc branch is the one whose Attack boost moved.
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("tinkaton", "moldbreaker", "playrough");
    s.sides[0].pokemon[0].stats[5] = 400;
    s.sides[1].pokemon[0] = mon("malamar", "contrary", "splash");
    s.sides[1].pokemon[0].stats[5] = 1;

    let boosts: Vec<i8> = generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .map(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            r.side(SideId::Two).boost(BoostIndex::Attack)
        })
        .collect();
    assert!(boosts.contains(&-1), "Mold Breaker suppresses Contrary: expected a −1, got {boosts:?}");
    assert!(!boosts.contains(&1), "no branch may raise Attack: {boosts:?}");
}

#[test]
fn contrary_still_inverts_without_mold_breaker() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("tinkaton", "pickpocket", "playrough");
    s.sides[0].pokemon[0].stats[5] = 400;
    s.sides[1].pokemon[0] = mon("malamar", "contrary", "splash");
    s.sides[1].pokemon[0].stats[5] = 1;

    let boosts: Vec<i8> = generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .map(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            r.side(SideId::Two).boost(BoostIndex::Attack)
        })
        .collect();
    assert!(boosts.contains(&1), "without Mold Breaker Contrary inverts the drop: {boosts:?}");
}

/// Intimidate is not a move, so `activeMove.ignoreAbility` is never set — a Mold Breaker holder
/// switching in next to a Contrary mon must NOT suppress it.
#[test]
fn intimidate_does_not_break_abilities() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("excadrill", "moldbreaker", "ironhead");
    s.sides[0].pokemon[1] = mon("incineroar", "intimidate", "ironhead");
    s.sides[1].pokemon[0] = mon("malamar", "contrary", "splash");
    s.sides[1].pokemon[0].types = [Type::Dark, Type::Psychic];

    let out = generate_instructions(&s, MoveChoice::Switch(1), MoveChoice::Move(0));
    let boosts: Vec<i8> = out
        .iter()
        .map(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            r.side(SideId::Two).boost(BoostIndex::Attack)
        })
        .collect();
    assert!(
        boosts.iter().all(|&b| b >= 0),
        "Intimidate is not a move: Contrary must still invert it, got {boosts:?}"
    );
}
