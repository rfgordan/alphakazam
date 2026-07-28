//! The forced-replacement switch bracket's THIRD shuffle sorts on a Speed the first two do not.
//!
//! All three shuffles read PS's cached `pokemon.speed`, which `queue.insertChoice` set for the
//! incoming mon before it entered (`replacement_bracket_tied` / `switch_entry_speed`). Only the
//! third — `runSwitch`'s own `runAction` `eachEvent('Update')` (`sim/battle.ts:2882`) — runs after
//! `runSwitch`'s `fieldEvent('SwitchIn')` (`sim/battle-actions.ts:184`), and a switch-in forme
//! change rewrites that cache: `Pokemon.setSpecies` ends with
//! `this.speed = this.storedStats.spe` (`sim/pokemon.ts:1419`).
//!
//! Witness op-40001 d22 t18 (Shields Down) and the rb1119 d8 / rb1241 d21 counter-witness
//! (Imposter, where `transformInto` overwrites `storedStats` *after* `setSpecies` and so leaves
//! the cache alone).

use engine::generate::{
    effective_speed, replacement_bracket_tied, replacement_bracket_tied_after_entry, switch_into,
};
use engine::ids::{Ability, Item, Species, Type};
use engine::state::{Pokemon, SideId, State};

/// Side One's active is a fainted mon awaiting replacement by party slot 1; side Two has a live
/// active whose Speed stat is `foe_speed`.
fn replacement_state(bench: Pokemon, foe_speed: i16) -> State {
    let mut state = State::EMPTY;

    let fainted = &mut state.sides[0].pokemon[0];
    *fainted = Pokemon::EMPTY;
    fainted.species = Species::from_id("regice").unwrap();
    fainted.base_species = fainted.species;
    fainted.level = 79;
    fainted.hp = 0;
    fainted.max_hp = 250;
    fainted.types = [Type::Ice, Type::None];
    fainted.base_types = fainted.types;
    fainted.live_types = fainted.types;
    fainted.stats = [250, 150, 200, 200, 250, 100];

    state.sides[0].pokemon[1] = bench;

    let foe = &mut state.sides[1].pokemon[0];
    *foe = Pokemon::EMPTY;
    foe.species = Species::from_id("tyranitar").unwrap();
    foe.base_species = foe.species;
    foe.level = 79;
    foe.hp = 300;
    foe.max_hp = 300;
    foe.types = [Type::Rock, Type::Dark];
    foe.base_types = foe.types;
    foe.live_types = foe.types;
    foe.stats = [300, 250, 220, 180, 200, foe_speed];
    state
}

fn core_minior(level: u8) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id("minior").unwrap();
    p.base_species = p.species;
    p.level = level;
    p.hp = 200;
    p.max_hp = 220; // above half, so Shields Down picks the Meteor forme
    p.types = [Type::Rock, Type::Flying];
    p.base_types = p.types;
    p.live_types = p.types;
    p.ability = Ability::ShieldsDown;
    p.base_ability = p.ability;
    p.stats = [220, 190, 130, 190, 130, 202];
    p
}

/// The Speed side One ends up with once `slot` has entered and its switch-in effects have run.
fn speed_after_entry(pre: &State, slot: u8) -> i32 {
    let mut post = *pre;
    switch_into(&mut post, SideId::One, slot);
    effective_speed(&post, SideId::One)
}

#[test]
fn shields_down_entry_forme_change_ties_only_the_last_shuffle() {
    // Probe the Meteor forme's Speed, then give the foe exactly that.
    let probe = replacement_state(core_minior(79), 1);
    let meteor_speed = speed_after_entry(&probe, 1);
    assert!(
        meteor_speed != effective_speed(&probe, SideId::One),
        "the core forme's entry Speed must differ from Meteor's, or the test proves nothing"
    );

    let pre = replacement_state(core_minior(79), meteor_speed as i16);
    let mut post = pre;
    switch_into(&mut post, SideId::One, 1);
    assert_eq!(
        post.side(SideId::One).active().species,
        Species::from_id("miniormeteor").unwrap(),
        "Shields Down must have forme-changed on the way in"
    );

    assert!(
        !replacement_bracket_tied(&pre, &[(SideId::One, 1)]),
        "shuffles 1-2 sort on the pre-entry cache (core forme), which does not tie"
    );
    assert!(
        replacement_bracket_tied_after_entry(&pre, &[(SideId::One, 1)]),
        "shuffle 3 sorts on the Meteor forme's stored Speed, which does tie"
    );
}

/// An untied board stays untied at both ends of the bracket.
#[test]
fn no_forme_change_keeps_one_predicate() {
    let mut plain = core_minior(79);
    plain.ability = Ability::Levitate; // no Shields Down: nothing changes on entry
    plain.base_ability = plain.ability;

    let entry = replacement_bracket_tied(&replacement_state(plain, 202), &[(SideId::One, 1)]);
    let after =
        replacement_bracket_tied_after_entry(&replacement_state(plain, 202), &[(SideId::One, 1)]);
    assert_eq!(entry, after, "without a forme change the two predicates are the same");
}

/// Transform is the species change that does NOT rewrite the cache: `transformInto` overwrites
/// `storedStats` after `setSpecies` and never re-reads `.speed`. An Imposter Ditto whose copied
/// Speed ties the foe's must still leave BOTH predicates false (rb1119 d8, rb1241 d21).
#[test]
fn imposter_transform_does_not_retie_the_last_shuffle() {
    let mut ditto = Pokemon::EMPTY;
    ditto.species = Species::from_id("ditto").unwrap();
    ditto.base_species = ditto.species;
    ditto.level = 87;
    ditto.hp = 200;
    ditto.max_hp = 200;
    ditto.types = [Type::Normal, Type::None];
    ditto.base_types = ditto.types;
    ditto.live_types = ditto.types;
    ditto.ability = Ability::Imposter;
    ditto.base_ability = ditto.ability;
    ditto.item = Item::ChoiceScarf;
    ditto.stats = [200, 130, 130, 130, 130, 130];

    let pre = replacement_state(ditto, 147);
    let mut post = pre;
    switch_into(&mut post, SideId::One, 1);
    let copy = post.side(SideId::One).active();
    assert!(copy.transformed, "Imposter must have copied the foe");
    assert_eq!(copy.stats[5], 147, "the copy's Speed stat ties the foe's");

    assert!(!replacement_bracket_tied(&pre, &[(SideId::One, 1)]));
    assert!(
        !replacement_bracket_tied_after_entry(&pre, &[(SideId::One, 1)]),
        "a transform leaves PS's Speed cache untouched, so the copied stat must not retie it"
    );
}
