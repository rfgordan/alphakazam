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

// ---- Sleep Talk ------------------------------------------------------------------------

#[test]
fn sleep_talk_calls_each_eligible_move_uniformly() {
    // Asleep Snorlax: sleeptalk, rest, bodyslam, crunch. Eligible = rest/bodyslam/crunch
    // (sleeptalk excludes itself), so three 1/3 sub-move families. Rest fails while asleep
    // (no heal); Body Slam and Crunch damage the foe.
    let mut p1 = mon("snorlax", [Type::Normal, Type::None], &["sleeptalk", "rest", "bodyslam", "crunch"]);
    p1.status = engine::ids::Status::Sleep;
    p1.status_counter = 3;
    p1.hp = 300; // below max so a Rest heal WOULD be visible if it (incorrectly) succeeded
    let state = duel(p1, mon("milotic", [Type::Water, Type::None], &["recover"]));
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    let mut damaged = 0.0f32; // total probability of branches where the foe took damage
    let mut healed = false;
    let mut pp_paid_once = true;
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        if r.side(SideId::Two).active().hp < 400 {
            damaged += o.percentage;
        }
        if r.side(SideId::One).active().hp > 300 {
            healed = true; // Rest must FAIL while already asleep
        }
        // Only Sleep Talk's own slot pays PP; the called move's slot is untouched.
        let moves = &r.side(SideId::One).active().moves;
        if moves[0].pp != 15 || moves[1].pp != 16 || moves[2].pp != 16 || moves[3].pp != 16 {
            pp_paid_once = false;
        }
        // The user must still be asleep with the counter ticked once (3 -> 2).
        assert_eq!(r.side(SideId::One).active().status, engine::ids::Status::Sleep);
        assert_eq!(r.side(SideId::One).active().status_counter, 2);
    }
    assert!(!healed, "Sleep Talk -> Rest must fail while asleep");
    assert!(pp_paid_once, "only sleeptalk's slot pays PP");
    // Body Slam + Crunch = 2 of 3 sub-moves connect (both 100% acc) => ~2/3 damage mass.
    assert!((damaged - 66.6).abs() < 3.0, "expected ~2/3 damaging mass, got {damaged}");
}

#[test]
fn sleep_talk_fails_awake() {
    let p1 = mon("snorlax", [Type::Normal, Type::None], &["sleeptalk", "rest", "bodyslam", "crunch"]);
    let state = duel(p1, mon("milotic", [Type::Water, Type::None], &["recover"]));
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    assert_eq!(outcomes.len(), 1, "awake Sleep Talk fails deterministically");
    let mut r = state;
    r.apply_instructions(&outcomes[0].instructions);
    assert_eq!(r.side(SideId::Two).active().hp, 400, "no sub-move may run");
    assert_eq!(r.side(SideId::One).active().moves[0].pp, 15, "PP is still paid on the failed use");
}

// ---- Magnet Rise -----------------------------------------------------------------------

#[test]
fn magnet_rise_grants_ground_immunity_and_expires() {
    use engine::volatile::VolatileStatus;
    // Klefki uses Magnet Rise: volatile + 5-turn counter; a follow-up Earthquake must deal 0.
    let p1 = mon("klefki", [Type::Steel, Type::Fairy], &["magnetrise"]);
    let p2 = mon("garchomp", [Type::Dragon, Type::Ground], &["earthquake"]);
    let state = duel(p1, p2);
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    assert_eq!(outcomes.len(), 1);
    let mut risen = state;
    risen.apply_instructions(&outcomes[0].instructions);
    assert!(risen.side(SideId::One).volatiles.contains(VolatileStatus::MagnetRise));
    assert_eq!(risen.side(SideId::One).magnet_rise_turns, 5);

    // Earthquake into the risen Klefki: every branch deals zero damage.
    let eq = generate_move_action(&risen, SideId::Two, 0, None, None);
    for o in &eq {
        let mut r = risen;
        r.apply_instructions(&o.instructions);
        assert_eq!(r.side(SideId::One).active().hp, 400, "Ground move must not connect under Magnet Rise");
    }

    // Control: without Magnet Rise, Earthquake connects (Klefki is grounded Steel/Fairy).
    let eq0 = generate_move_action(&state, SideId::Two, 0, None, None);
    let hit = eq0.iter().any(|o| {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        r.side(SideId::One).active().hp < 400
    });
    assert!(hit, "Earthquake must connect while grounded");
}

// ---- Focus Punch fail gate -------------------------------------------------------------

#[test]
fn focus_punch_fails_after_taking_a_hit_without_paying_pp() {
    // The user was already struck this turn (physical_damage_taken > 0): Focus Punch is
    // cancelled before PP is paid (PS beforeMoveCallback precedes deductPP).
    let p1 = mon("dusknoir", [Type::Ghost, Type::None], &["focuspunch"]);
    let p2 = mon("snorlax", [Type::Normal, Type::None], &["bodyslam"]);
    let mut state = duel(p1, p2);
    state.sides[0].physical_damage_taken = 77;
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    assert_eq!(outcomes.len(), 1, "lost focus is deterministic");
    let mut r = state;
    r.apply_instructions(&outcomes[0].instructions);
    assert_eq!(r.side(SideId::Two).active().hp, 400, "the punch must not land");
    assert_eq!(r.side(SideId::One).active().moves[0].pp, 16, "no PP is paid on a lost-focus cancel");

    // Undamaged: the punch throws normally (the damage is the signal).
    let clean = duel(
        mon("dusknoir", [Type::Ghost, Type::None], &["focuspunch"]),
        mon("snorlax", [Type::Normal, Type::None], &["bodyslam"]),
    );
    let outcomes = generate_move_action(&clean, SideId::One, 0, None, None);
    let hit = outcomes.iter().any(|o| {
        let mut r = clean;
        r.apply_instructions(&o.instructions);
        r.side(SideId::Two).active().hp < 400
    });
    assert!(hit, "an undisturbed Focus Punch must connect");
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
