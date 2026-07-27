//! Focused tests for the c6 strict-gap move tranche: Weather Ball weather-typing + ×2 BP,
//! Lash Out's ×2 after a stat drop this turn, Moongeist Beam's ability-ignoring damage, and the
//! Focus Punch physical-damage-taken bookkeeping (set + exact reversal).

use engine::generate::generate_move_action;
use engine::ids::{Ability, MoveId, Species, Type, Weather};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::volatile::VolatileStatus;

fn mon(species: &str, types: [Type; 2], moves: &[&str]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.hp = 400;
    p.max_hp = 400;
    p.types = types;
    p.base_types = types;
    p.stats = [400, 250, 220, 250, 220, 200];
    for (i, m) in moves.iter().enumerate() {
        p.moves[i] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    }
    p
}

fn duel(p1: Pokemon, p2: Pokemon) -> State {
    let mut state = State::EMPTY;
    state.sides[0].pokemon[0] = p1;
    state.sides[1].pokemon[0] = p2;
    state
}

/// Max damage the p1 move deals to p2 across all enumerated branches.
fn max_damage(state: &State) -> i16 {
    let outcomes = generate_move_action(state, SideId::One, 0, None, None);
    let mut worst = 0;
    for o in &outcomes {
        let mut r = *state;
        r.apply_instructions(&o.instructions);
        worst = worst.max(400 - r.side(SideId::Two).active().hp);
    }
    worst
}

// ---- Weather Ball ----------------------------------------------------------------------

#[test]
fn weather_ball_becomes_water_in_rain() {
    // Vs a Rock/Ground target (4x weak to Water, but only 0.5x to a Normal-type Weather Ball).
    // Rain turns it Water AND doubles its base power (50 -> 100), so it must vastly out-damage
    // the no-weather Normal version.
    let build = |weather: Weather| {
        let mut s = duel(
            mon("pelipper", [Type::Water, Type::Flying], &["weatherball"]),
            mon("golem", [Type::Rock, Type::Ground], &["tackle"]),
        );
        s.weather = weather;
        s.weather_turns = 5;
        s
    };
    let no_weather = max_damage(&build(Weather::None));
    let rain = max_damage(&build(Weather::Rain));
    assert!(no_weather > 0);
    // 4x type + 2x BP vs 0.5x type, 1x BP => rain is many times larger.
    assert!(rain > no_weather * 4, "rain Weather Ball ({rain}) must dwarf Normal ({no_weather})");
}

#[test]
fn weather_ball_becomes_fire_in_sun() {
    // Vs a Steel/Grass target: Fire (sun) is 4x; Water (rain) is 0.5x. Sun must out-damage rain.
    let build = |weather: Weather| {
        let mut s = duel(
            mon("torkoal", [Type::Fire, Type::Fire], &["weatherball"]),
            mon("abomasnow", [Type::Grass, Type::Ice], &["tackle"]),
        );
        s.weather = weather;
        s.weather_turns = 5;
        s
    };
    let sun = max_damage(&build(Weather::Sun));
    let rain = max_damage(&build(Weather::Rain));
    // Sun (Fire, 4x + STAB) frequently KOs — its damage is clamped to the target's HP — so the
    // ratio understates the true gap; ×3 is still comfortably clear of rain (Water, 0.5x).
    assert!(sun > rain * 3, "sun (Fire, {sun}) must dwarf rain (Water, {rain}) here");
}

// ---- Lash Out --------------------------------------------------------------------------

#[test]
fn lash_out_doubles_after_a_stat_drop_this_turn() {
    let base = duel(
        mon("honchkrow", [Type::Dark, Type::Flying], &["lashout"]),
        mon("snorlax", [Type::Normal, Type::None], &["tackle"]),
    );
    let plain = max_damage(&base);
    let mut lowered = base;
    lowered.sides[0].volatiles.insert(VolatileStatus::StatsLoweredThisTurn);
    let doubled = max_damage(&lowered);
    assert!(plain > 0);
    // BP 75 -> 150; damage roughly doubles (allow rounding slack).
    assert!(doubled >= plain * 19 / 10, "Lash Out should ~2x: plain {plain} doubled {doubled}");
}

// ---- Moongeist Beam --------------------------------------------------------------------

#[test]
fn moongeist_beam_ignores_purifying_salt() {
    // Purifying Salt halves incoming Ghost damage; Moongeist Beam's ignoreAbility bypasses it, so
    // the damage is identical whether the target has Purifying Salt or an inert ability.
    let build = |ab: Ability| {
        let mut p2 = mon("garganacl", [Type::Rock, Type::Rock], &["tackle"]);
        p2.ability = ab;
        duel(mon("lunala", [Type::Psychic, Type::Ghost], &["moongeistbeam"]), p2)
    };
    let vs_salt = max_damage(&build(Ability::PurifyingSalt));
    let vs_inert = max_damage(&build(Ability::Pressure));
    assert!(vs_inert > 0);
    assert_eq!(vs_salt, vs_inert, "Moongeist Beam must ignore Purifying Salt (equal damage)");
}

#[test]
fn shadow_ball_is_halved_by_purifying_salt() {
    // Control: a non-ignoring Ghost move IS reduced by Purifying Salt, proving the test above
    // is measuring the ability and not a no-op.
    let build = |ab: Ability| {
        let mut p2 = mon("garganacl", [Type::Rock, Type::Rock], &["shadowball"]);
        p2.ability = ab;
        duel(mon("lunala", [Type::Psychic, Type::Ghost], &["shadowball"]), p2)
    };
    let vs_salt = max_damage(&build(Ability::PurifyingSalt));
    let vs_inert = max_damage(&build(Ability::Pressure));
    assert!(vs_salt < vs_inert, "Purifying Salt should reduce Shadow Ball: {vs_salt} < {vs_inert}");
}

// ---- Focus Punch bookkeeping -----------------------------------------------------------

#[test]
fn physical_hit_records_damage_taken_and_reverses() {
    // A physical hit records the defender side's physical_damage_taken (Focus Punch's fail gate)
    // and every branch reverses byte-exact.
    let state = duel(
        mon("tauros", [Type::Normal, Type::None], &["bodyslam"]),
        mon("blissey", [Type::Normal, Type::None], &["softboiled"]),
    );
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    let mut saw = false;
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        let dealt = 400 - r.side(SideId::Two).active().hp;
        if dealt > 0 {
            saw = true;
            assert_eq!(r.side(SideId::Two).physical_damage_taken, dealt);
            assert_eq!(r.side(SideId::One).physical_damage_taken, 0);
        }
        let mut back = r;
        back.reverse_instructions(&o.instructions);
        assert_eq!(back, state, "SetPhysicalDamageTaken must reverse exactly");
    }
    assert!(saw);
}
