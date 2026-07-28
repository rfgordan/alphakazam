//! Knock Off's `onAfterHit` still fires when a nullifying ability zeroed the damage.
//!
//! PS's Disguise / Ice Face are `onDamage` handlers that return the NUMBER `0`
//! (`data/abilities.ts:966` / `:1090`). `spreadDamage` treats `0` as a real result rather than a
//! failure (`sim/battle.ts:2119`), so `damage[i]` stays a number, the target lands in
//! `damagedTargets` (`sim/battle-actions.ts:1131-1137`), and step 8's `onAfterHit`
//! (`:1144`) runs — taking the item (`data/moves.ts:9977-9982`).
//!
//! Witness op-40005 d6 t5: a Hariyama Knock Off busts an intact Mimikyu's Disguise and PS ends the
//! turn with its Life Orb gone; the engine's nullifying arms returned before `apply_post_damage`
//! and kept the item.

use engine::generate::generate_move_action;
use engine::ids::{Ability, Item, MoveId, Species, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};

/// Attacker on side One using `move_name`; defender on side Two with `ability`/`species`, holding
/// a Life Orb.
fn nullifier_state(move_name: &str, species: &str, ability: Ability) -> State {
    let mut state = State::EMPTY;

    let attacker = &mut state.sides[0].pokemon[0];
    *attacker = Pokemon::EMPTY;
    attacker.species = Species::from_id("hariyama").unwrap();
    attacker.base_species = attacker.species;
    attacker.level = 100;
    attacker.hp = 300;
    attacker.max_hp = 300;
    attacker.types = [Type::Fighting, Type::None];
    attacker.base_types = attacker.types;
    attacker.live_types = attacker.types;
    attacker.stats = [300, 250, 180, 180, 180, 150];
    attacker.moves[0] = MoveSlot {
        id: MoveId::from_id(move_name).unwrap(), pp: 10, max_pp: 10, disabled: false,
    };

    let defender = &mut state.sides[1].pokemon[0];
    *defender = Pokemon::EMPTY;
    defender.species = Species::from_id(species).unwrap();
    defender.base_species = defender.species;
    defender.level = 100;
    defender.hp = 400;
    defender.max_hp = 400;
    defender.types = [Type::Ghost, Type::Fairy];
    defender.base_types = defender.types;
    defender.live_types = defender.types;
    defender.ability = ability;
    defender.base_ability = ability;
    defender.item = Item::LifeOrb;
    defender.stats = [400, 180, 300, 180, 220, 130];
    state
}

/// Every generated outcome of side One's first move, as (probability, resulting state).
fn outcomes(state: &State) -> Vec<(f32, State, Vec<engine::Instruction>)> {
    generate_move_action(state, SideId::One, 0, None, None)
        .into_iter()
        .map(|outcome| {
            let mut result = *state;
            result.apply_instructions(&outcome.instructions);
            (outcome.percentage, result, outcome.instructions)
        })
        .collect()
}

#[test]
fn knock_off_takes_the_item_through_disguise() {
    let original = nullifier_state("knockoff", "mimikyu", Ability::Disguise);
    let results = outcomes(&original);
    assert!(!results.is_empty());

    let busted = Species::from_id("mimikyubusted").unwrap();
    let mut saw_bust = false;
    for (_, result, instructions) in &results {
        let target = result.side(SideId::Two).active();
        if target.species != busted {
            continue; // the accuracy-miss branch
        }
        saw_bust = true;
        assert_eq!(target.item, Item::None, "Knock Off's onAfterHit runs on a 0-damage hit");
        assert_eq!(target.hp, target.max_hp - target.max_hp / 8, "only the bust chip lands");

        let mut restored = *result;
        restored.reverse_instructions(instructions);
        assert_eq!(restored, original, "the item removal must reverse with the branch");
    }
    assert!(saw_bust, "Knock Off must produce a Disguise-busting branch");
}

#[test]
fn knock_off_takes_the_item_through_ice_face() {
    let mut original = nullifier_state("knockoff", "eiscue", Ability::IceFace);
    original.sides[1].pokemon[0].types = [Type::Ice, Type::None];
    original.sides[1].pokemon[0].base_types = original.sides[1].pokemon[0].types;
    original.sides[1].pokemon[0].live_types = original.sides[1].pokemon[0].types;
    let results = outcomes(&original);
    assert!(!results.is_empty());

    let noice = Species::from_id("eiscuenoice").unwrap();
    let mut saw_break = false;
    for (_, result, instructions) in &results {
        let target = result.side(SideId::Two).active();
        if target.species != noice {
            continue;
        }
        saw_break = true;
        assert_eq!(target.item, Item::None, "Knock Off's onAfterHit runs on a 0-damage hit");
        assert_eq!(target.hp, target.max_hp, "Ice Face nullifies the damage outright");

        let mut restored = *result;
        restored.reverse_instructions(instructions);
        assert_eq!(restored, original);
    }
    assert!(saw_break, "Knock Off must produce an Ice-Face-breaking branch");
}

/// Control: only Knock Off has the `onAfterHit`. A different physical move that Disguise nullifies
/// leaves the item alone.
#[test]
fn a_plain_nullified_hit_leaves_the_item() {
    let original = nullifier_state("playrough", "mimikyu", Ability::Disguise);
    let busted = Species::from_id("mimikyubusted").unwrap();
    for (_, result, _) in &outcomes(&original) {
        let target = result.side(SideId::Two).active();
        if target.species != busted {
            continue;
        }
        assert_eq!(target.item, Item::LifeOrb, "only Knock Off takes the item");
    }
}
