//! `ignoreDefensive: true` zeroes the target's Def/SpD STAGE in `getDamage`.
//!
//! `sim/battle-actions.ts:1695-1704` — `const ignoreDefensive = !!(move.ignoreDefensive ||
//! (ignorePositiveDefensive && defBoosts > 0)); if (ignoreDefensive) defBoosts = 0;`. The flag
//! form is unconditional, so a NEGATIVE stage is discarded too, and it is a different field from
//! `ignoreEvasion` even though the same four moves (Chip Away, Darkest Lariat, Nihil Light,
//! Sacred Sword) carry both. The engine had only wired the evasion half. rb1781 d5: Sacred Sword
//! into a +1 Def Krookodile — PS deals 87, the engine divided through the 1.5x and dealt 59.

use engine::generate::generate_instructions;
use engine::ids::{Ability, BoostIndex, MoveId, Species};
use engine::state::{MoveSlot, Pokemon, State};
use engine::MoveChoice;

fn board(mv: &str, def_stage: i8) -> State {
    let mut s = State::EMPTY;
    for (i, (sp, m)) in [("chienpao", mv), ("krookodile", "splash")].into_iter().enumerate() {
        let p = &mut s.sides[i].pokemon[0];
        p.species = Species::from_id(sp).unwrap();
        p.level = 100;
        p.types = engine::data::species_types(p.species);
        p.base_types = p.types;
        p.hp = 400;
        p.max_hp = 400;
        p.stats = [400, 250, 200, 200, 200, if i == 0 { 300 } else { 1 }];
        p.ability = Ability::None;
        p.moves[0] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    }
    s.sides[1].boosts[BoostIndex::Defense as usize] = def_stage;
    s
}

/// The SMALLEST non-zero damage over all branches — i.e. the non-crit, lowest-roll one. Using the
/// max would compare crit branches, and a crit already ignores POSITIVE defensive stages
/// (`moveHit.crit -> ignorePositiveDefensive`), which hides the flag under test.
fn min_damage(s: &State) -> i16 {
    generate_instructions(s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .map(|o| {
            let mut r = *s;
            r.apply_instructions(&o.instructions);
            400 - r.sides[1].pokemon[0].hp
        })
        .filter(|&d| d > 0)
        .min()
        .unwrap()
}

#[test]
fn sacred_sword_ignores_a_positive_defense_stage() {
    assert_eq!(min_damage(&board("sacredsword", 2)), min_damage(&board("sacredsword", 0)));
}

#[test]
fn sacred_sword_ignores_a_negative_defense_stage_too() {
    // The flag form is unconditional — unlike the crit rule, which only drops POSITIVE stages.
    assert_eq!(min_damage(&board("sacredsword", -2)), min_damage(&board("sacredsword", 0)));
}

#[test]
fn an_ordinary_move_still_reads_the_stage() {
    assert!(min_damage(&board("crunch", 2)) < min_damage(&board("crunch", 0)));
    assert!(min_damage(&board("crunch", -2)) > min_damage(&board("crunch", 0)));
}

#[test]
fn darkest_lariat_and_chip_away_carry_the_same_flag() {
    for mv in ["darkestlariat", "chipaway"] {
        assert_eq!(min_damage(&board(mv, 2)), min_damage(&board(mv, 0)), "{mv}");
    }
}
