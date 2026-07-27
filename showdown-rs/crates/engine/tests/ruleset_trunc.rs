//! The `trunc` arm — `RULESET_SPEC.md` §9 / H3.
//!
//! `[Gen 9] Custom Game` sets `battle: { trunc: Math.trunc }` (`config/formats.ts:152`), a
//! function that ignores the `bits` argument. Every other format keeps `Dex#trunc`
//! (`sim/dex.ts:363`), `(num >>> 0) % 2**bits`. Two call sites take `bits`, and the whole
//! draw-exact corpus was calibrated on the arm where neither fires:
//!
//!   * `sim/pokemon.ts:649` — `getActionSpeed()` returns `trunc(speed, 13)`
//!   * `sim/pokemon.ts:638` — `getStat('spe')` caps at 10000 iff `!format.battle?.trunc`
//!   * `sim/battle-actions.ts:1845` — damage is `trunc(baseDamage, 16)`
//!
//! The Speed one is draw-order relevant: it decides turn order, hence which branch of the
//! move-order Speed-tie shuffle PS takes.

use engine::generate::effective_speed;
use engine::ids::{Ability, Item, StatIndex, Weather};
use engine::ruleset::Ruleset;
use engine::state::{SideId, State};
use engine::team;

/// A board whose side-One active is as fast as the game allows to stack it.
fn stacked_speed(rs: Ruleset) -> State {
    let mut st = team::default_matchup();
    st.ruleset = rs;
    let s = st.side_mut(SideId::One);
    // Regieleki-class raw Speed, then +6 (×4), Choice Scarf (×1.5), Tailwind (×2),
    // Swift Swim in rain (×2)  =>  504 * 4 * 1.5 * 2 * 2 = 12096 — the SPEC §9 worked example.
    s.pokemon[0].stats[StatIndex::Speed as usize] = 504;
    s.pokemon[0].item = Item::ChoiceScarf;
    s.pokemon[0].ability = Ability::SwiftSwim;
    s.boosts[engine::ids::BoostIndex::Speed as usize] = 6;
    s.side_conditions.tailwind = 4;
    st.weather = Weather::Rain;
    st
}

#[test]
fn customgame_never_wraps_speed_and_never_caps_it() {
    let st = stacked_speed(Ruleset::GEN9_CUSTOM_GAME);
    // Math.trunc ignores `bits`, and the 10000 cap is gated on `!format.battle?.trunc`.
    assert_eq!(effective_speed(&st, SideId::One), 12096);
}

#[test]
fn randombattle_caps_at_10000_then_wraps_mod_8192() {
    let st = stacked_speed(Ruleset::GEN9_RANDOM_BATTLE);
    // SPEC §9 predicts "12096 -> wraps to 3904". It does NOT: the §9 worked example skips the
    // `stat > 10000` cap at `sim/pokemon.ts:638`, which is gated on the very same
    // `!format.battle?.trunc` and therefore fires in exactly the formats that also truncate.
    // getStat caps 12096 -> 10000; getActionSpeed then truncs 10000 -> 10000 - 8192 = 1808.
    //
    // Consequence worth stating: the cap makes the whole action-speed range reachable only as
    // [0, 8191] for raw <= 8191, [0, 1808] for raw in [8192, 10000], and the single value 1808
    // for every raw > 10000. There is no way to land in (1808, 8191] by wrapping.
    assert_eq!(effective_speed(&st, SideId::One), 1808);
}

/// The wrap is not merely numeric: it INVERTS turn order, which is what makes it draw-relevant.
#[test]
fn the_wrap_inverts_turn_order_against_an_ordinary_foe() {
    let mut st = stacked_speed(Ruleset::GEN9_RANDOM_BATTLE);
    st.side_mut(SideId::Two).pokemon[0].stats[StatIndex::Speed as usize] = 300;
    let fast = effective_speed(&st, SideId::One);
    let slow = effective_speed(&st, SideId::Two);
    assert_eq!((fast, slow), (1808, 300));
    assert!(fast > slow, "still first here — the wrap landed above the foe");

    // Push the stack just past a wrap boundary: raw 8192 truncates to exactly 0, which is
    // SLOWER than a benched Slowpoke and ties the field-effect handlers at speed 0 (SPEC H4).
    let mut st2 = st;
    let s = st2.side_mut(SideId::One);
    s.boosts = [0; 7];
    s.side_conditions.tailwind = 0;
    s.pokemon[0].item = Item::None;
    s.pokemon[0].ability = Ability::SwiftSwim; // ×2 in rain
    s.pokemon[0].stats[StatIndex::Speed as usize] = 4096; // 4096 * 2 = 8192
    assert_eq!(effective_speed(&st2, SideId::One), 0, "8192 truncates to 0");
    assert!(effective_speed(&st2, SideId::One) < effective_speed(&st2, SideId::Two));

    // ...and the same board under customgame keeps the full 8192 and moves first.
    let mut cg = st2;
    cg.ruleset = Ruleset::GEN9_CUSTOM_GAME;
    assert_eq!(effective_speed(&cg, SideId::One), 8192);
}

#[test]
fn damage_16_bit_truncation_is_a_no_op_at_legal_damage() {
    use engine::damage::{damage_rolls, DamageInput};
    use engine::ids::{MoveCategory, Type};
    let mk = |trunc_16| DamageInput {
        level: 100,
        base_power: 100,
        category: MoveCategory::Physical,
        move_type: Type::Ground,
        attacker_types: [Type::Ground, Type::None],
        attacker_base_types: [Type::Ground, Type::None],
        defender_types: [Type::Normal, Type::None],
        attack_stat: 300,
        defense_stat: 200,
        is_crit: false,
        attacker_burned: false,
        weather: Weather::None,
        terastallized: false,
        tera_type: Type::None,
        life_orb: false,
        adaptability: false,
        tera_shell: false,
        freeze_dry: false,
        trunc_16,
        final_num: 1,
        final_den: 1,
    };
    assert_eq!(damage_rolls(&mk(true)), damage_rolls(&mk(false)));
}
