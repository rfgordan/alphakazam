//! Dry Skin's OTHER half: `onFoeBasePower` ×1.25 for an incoming Fire move.
//!
//! `data/abilities.ts` dryskin has three handlers — `onTryHit` (Water absorb), `onWeather` (the
//! sun/rain residual) and `onFoeBasePower` at `onFoeBasePowerPriority: 17`. The engine modelled
//! the first two. rb1636 d5: a Delphox's Fire Blast into a Toxicroak, 43 HP of the 244 PS took.
//!
//! It is a BASE-POWER modifier, so it chains with the others in one `chainModify` rather than
//! rounding on its own — the same reason Heatproof's ×0.5 sits on the offensive stat and Punk
//! Rock's ×1.3 sits in the base-power chain.

use engine::generate::generate_instructions;
use engine::ids::{Ability, MoveId, Species};
use engine::state::{MoveSlot, Pokemon, State};
use engine::MoveChoice;

fn board(def_ability: &str, mv: &str) -> State {
    let mut s = State::EMPTY;
    for (i, (sp, m)) in [("delphox", mv), ("toxicroak", "splash")].into_iter().enumerate() {
        let p = &mut s.sides[i].pokemon[0];
        p.species = Species::from_id(sp).unwrap();
        p.level = 100;
        p.types = engine::data::species_types(p.species);
        p.base_types = p.types;
        p.hp = 600;
        p.max_hp = 600;
        p.stats = [600, 200, 200, 250, 200, if i == 0 { 300 } else { 1 }];
        p.moves[0] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    }
    s.sides[1].pokemon[0].ability = Ability::from_id(def_ability).unwrap();
    s
}

/// Lowest non-zero damage over all branches (the non-crit, lowest-roll one).
fn min_damage(s: &State) -> i16 {
    generate_instructions(s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .map(|o| {
            let mut r = *s;
            r.apply_instructions(&o.instructions);
            600 - r.sides[1].pokemon[0].hp
        })
        .filter(|&d| d > 0)
        .min()
        .unwrap()
}

#[test]
fn dry_skin_takes_more_from_fire() {
    let plain = min_damage(&board("poisontouch", "flamethrower"));
    let dry = min_damage(&board("dryskin", "flamethrower"));
    assert!(dry > plain, "Dry Skin is +25% base power against Fire: {dry} vs {plain}");
}

#[test]
fn dry_skin_does_not_touch_a_non_fire_move() {
    assert_eq!(
        min_damage(&board("dryskin", "psychic")),
        min_damage(&board("poisontouch", "psychic"))
    );
}

#[test]
fn heatproof_still_halves_in_the_other_direction() {
    let plain = min_damage(&board("poisontouch", "flamethrower"));
    let proof = min_damage(&board("heatproof", "flamethrower"));
    assert!(proof < plain, "the control on the same board");
}
