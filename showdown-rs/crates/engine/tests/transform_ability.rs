//! Two Transform/ability-timing rules, both found on rb1502.
//!
//! * **`transformInto` ends with `setAbility(target.ability, this, true)`, and `setAbility` runs
//!   `singleEvent('Start', ability, ...)` for every gen > 3** — so the COPIED ability fires. An
//!   Imposter Ditto copying Intimidate Intimidates.
//! * **The switch-out abilities read the LIVE ability**, from `runEvent('BeforeSwitchOut')`
//!   (`sim/battle.ts:2919`), which is before `switchIn` calls `clearVolatile()`. A Ditto that
//!   Impostered into a Regenerator mon still has Regenerator when it leaves.

use engine::generate::generate_instructions;
use engine::ids::{Ability, BoostIndex, Item, MoveId, Species};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::MoveChoice;

fn mon(species: &str, ability: &str, m: &str) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = engine::data::species_types(p.species);
    p.base_types = p.types;
    p.hp = 300;
    p.max_hp = 300;
    p.stats = [300, 200, 200, 200, 200, 200];
    p.ability = Ability::from_id(ability).unwrap();
    p.moves[0] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    p
}

#[test]
fn imposter_into_intimidate_intimidates() {
    let mut s = State::EMPTY;
    // Side one switches its Ditto in against an Intimidate holder.
    s.sides[0].pokemon[0] = mon("blissey", "naturalcure", "splash");
    s.sides[0].pokemon[1] = mon("ditto", "imposter", "splash");
    s.sides[1].pokemon[0] = mon("incineroar", "intimidate", "splash");

    for o in &generate_instructions(&s, MoveChoice::Switch(1), MoveChoice::Move(0)) {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(
            r.side(SideId::Two).boost(BoostIndex::Attack),
            -1,
            "the Ditto copied Intimidate, and setAbility fires its Start event"
        );
    }
}

#[test]
fn imposter_copies_the_targets_boosts() {
    // The control on the same board: Transform copies stat stages.
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("blissey", "naturalcure", "splash");
    s.sides[0].pokemon[1] = mon("ditto", "imposter", "splash");
    s.sides[1].pokemon[0] = mon("kingambit", "supremeoverlord", "splash");
    s.sides[1].boosts[BoostIndex::SpecialAttack as usize] = 2;

    for o in &generate_instructions(&s, MoveChoice::Switch(1), MoveChoice::Move(0)) {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(r.side(SideId::One).boost(BoostIndex::SpecialAttack), 2);
    }
}

#[test]
fn a_transformed_ditto_switches_out_with_the_copied_regenerator() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("ditto", "imposter", "splash");
    s.sides[0].pokemon[0].hp = 60; // 300 max: Regenerator restores 100
    s.sides[0].pokemon[0].item = Item::None;
    s.sides[0].pokemon[0].ability = Ability::Regenerator; // as if already Impostered
    s.sides[0].pokemon[0].transformed = true;
    s.sides[0].pokemon[0].base_ability = Ability::Imposter;
    s.sides[0].pokemon[1] = mon("blissey", "naturalcure", "splash");
    s.sides[1].pokemon[0] = mon("kingambit", "supremeoverlord", "splash");

    for o in &generate_instructions(&s, MoveChoice::Switch(1), MoveChoice::Move(0)) {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(
            r.sides[0].pokemon[0].hp, 160,
            "Regenerator is read before revert_transform puts Imposter back"
        );
        assert_eq!(r.sides[0].pokemon[0].ability, Ability::Imposter, "and the revert still happens");
    }
}
