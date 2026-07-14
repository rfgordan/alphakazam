//! Unit tests for the audited ability/item batch (proactive implementation of the
//! gen9randombattle vocabulary entries that had no behavioral reference).
//!
//! Fixture style follows tests/ice_face.rs: hand-built `State`s, `generate_move_action`
//! for single actions (no end-of-turn), `generate_instructions` for full turns.

use engine::generate::{effective_speed, generate_instructions_ex, generate_move_action};
use engine::ids::{Ability, BoostIndex, Item, MoveId, Species, Status, Terrain, Type, Weather};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::volatile::VolatileStatus;
use engine::MoveChoice;

fn mon(species: &str, types: [Type; 2], stats: [i16; 6]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = types;
    p.base_types = types;
    p.hp = stats[0];
    p.max_hp = stats[0];
    p.stats = stats;
    p
}

fn slot(m: &str) -> MoveSlot {
    MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 10, max_pp: 10, disabled: false }
}

/// A plain 1v1 state: side one "ambipom" (Normal), side two "eiscue" (Ice) — species only
/// matter where species-specific logic reads them; stats/types are set explicitly.
fn duel(attacker_move: &str, defender_move: &str) -> State {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("ambipom", [Type::Normal, Type::None], [300, 250, 180, 180, 180, 250]);
    s.sides[0].pokemon[0].moves[0] = slot(attacker_move);
    s.sides[1].pokemon[0] = mon("eiscue", [Type::Ice, Type::None], [400, 180, 250, 180, 220, 130]);
    s.sides[1].pokemon[0].moves[0] = slot(defender_move);
    s
}

fn outcomes(state: &State) -> Vec<(f32, State)> {
    generate_move_action(state, SideId::One, 0, None, None)
        .into_iter()
        .map(|o| {
            let mut r = *state;
            r.apply_instructions(&o.instructions);
            (o.percentage, r)
        })
        .collect()
}

fn full_turn(state: &State) -> Vec<(f32, State)> {
    generate_instructions_ex(state, MoveChoice::Move(0), MoveChoice::Move(0), [None, None], [false, false])
        .into_iter()
        .map(|o| {
            let mut r = *state;
            r.apply_instructions(&o.instructions);
            (o.percentage, r)
        })
        .collect()
}

/// The smallest post-move HP of side `sid`'s active over all outcome branches.
fn min_hp(results: &[(f32, State)], sid: SideId) -> i16 {
    results.iter().map(|(_, r)| r.side(sid).active().hp).min().unwrap()
}
fn max_hp_seen(results: &[(f32, State)], sid: SideId) -> i16 {
    results.iter().map(|(_, r)| r.side(sid).active().hp).max().unwrap()
}

// ---------- damage / stat modifiers ----------

#[test]
fn stakeout_doubles_offense_against_fresh_switch_in() {
    let mut fresh = duel("tackle", "tackle");
    fresh.sides[0].pokemon[0].ability = Ability::Stakeout;
    fresh.sides[1].active_turns = 0;
    let mut settled = fresh;
    settled.sides[1].active_turns = 1;
    let d_fresh = 400 - min_hp(&outcomes(&fresh), SideId::Two) as i32;
    let d_settled = 400 - min_hp(&outcomes(&settled), SideId::Two) as i32;
    assert!(d_fresh >= d_settled * 19 / 10, "Stakeout ~2x: fresh {d_fresh} vs settled {d_settled}");
}

#[test]
fn surge_surfer_doubles_speed_in_electric_terrain() {
    let mut s = duel("tackle", "tackle");
    s.sides[0].pokemon[0].ability = Ability::SurgeSurfer;
    let base = effective_speed(&s, SideId::One);
    s.terrain = Terrain::Electric;
    s.terrain_turns = 5;
    assert_eq!(effective_speed(&s, SideId::One), base * 2);
}

#[test]
fn liquid_voice_makes_sound_moves_water() {
    let mut s = duel("hypervoice", "tackle");
    s.sides[0].pokemon[0].ability = Ability::LiquidVoice;
    s.sides[1].pokemon[0].ability = Ability::WaterAbsorb; // absorbs only if the move is Water
    let res = outcomes(&s);
    assert_eq!(min_hp(&res, SideId::Two), 400, "Water Absorb must nullify the Water-typed Hyper Voice");
}

#[test]
fn lustrous_orb_boosts_palkia_dragon_moves() {
    let mut with = duel("dracometeor", "tackle");
    with.sides[0].pokemon[0] = mon("palkia", [Type::Water, Type::Dragon], [300, 250, 180, 300, 180, 250]);
    with.sides[0].pokemon[0].moves[0] = slot("dracometeor");
    let mut without = with;
    with.sides[0].pokemon[0].item = Item::LustrousOrb;
    without.sides[0].pokemon[0].item = Item::None;
    let d_with = 400 - min_hp(&outcomes(&with), SideId::Two) as i32;
    let d_without = 400 - min_hp(&outcomes(&without), SideId::Two) as i32;
    assert!(d_with > d_without, "Lustrous Orb x1.2: {d_with} vs {d_without}");
    // Non-signature holder gets nothing.
    let mut wrong = with;
    wrong.sides[0].pokemon[0].species = Species::from_id("ambipom").unwrap();
    let d_wrong = 400 - min_hp(&outcomes(&wrong), SideId::Two) as i32;
    assert_eq!(d_wrong, d_without, "orb must be inert for non-Palkia");
}

#[test]
fn analytic_boosts_when_target_will_not_move() {
    let mut s = duel("tackle", "tackle");
    s.sides[0].pokemon[0].ability = Ability::Analytic;
    let tackle = MoveId::from_id("tackle").unwrap();
    let boosted: Vec<(f32, State)> = generate_move_action(&s, SideId::One, 0, None, None)
        .into_iter().map(|o| { let mut r = s; r.apply_instructions(&o.instructions); (o.percentage, r) }).collect();
    let unboosted: Vec<(f32, State)> = generate_move_action(&s, SideId::One, 0, None, Some(tackle))
        .into_iter().map(|o| { let mut r = s; r.apply_instructions(&o.instructions); (o.percentage, r) }).collect();
    let d_boosted = 400 - min_hp(&boosted, SideId::Two) as i32;
    let d_unboosted = 400 - min_hp(&unboosted, SideId::Two) as i32;
    assert!(d_boosted > d_unboosted, "Analytic x1.3: {d_boosted} vs {d_unboosted}");
}

// ---------- immunities / absorbs ----------

#[test]
fn well_baked_body_absorbs_fire_and_boosts_def() {
    let mut s = duel("flamethrower", "tackle");
    s.sides[1].pokemon[0].ability = Ability::WellBakedBody;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::Two).active().hp, 400);
        assert_eq!(r.side(SideId::Two).boost(BoostIndex::Defense), 2);
    }
}

#[test]
fn wind_rider_absorbs_wind_and_boosts_atk() {
    let mut s = duel("gust", "tackle");
    s.sides[1].pokemon[0].ability = Ability::WindRider;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::Two).active().hp, 400);
        assert_eq!(r.side(SideId::Two).boost(BoostIndex::Attack), 1);
    }
}

#[test]
fn queenly_majesty_blocks_priority_moves() {
    let mut s = duel("quickattack", "tackle");
    s.sides[1].pokemon[0].ability = Ability::QueenlyMajesty;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::Two).active().hp, 400, "Quick Attack must fail vs Queenly Majesty");
    }
    // Priority 0 still connects.
    let mut s0 = duel("tackle", "tackle");
    s0.sides[1].pokemon[0].ability = Ability::QueenlyMajesty;
    assert!(min_hp(&outcomes(&s0), SideId::Two) < 400);
}

// ---------- status legality ----------

#[test]
fn corrosion_poisons_steel_types() {
    let mut s = duel("toxic", "tackle");
    s.sides[0].pokemon[0].ability = Ability::Corrosion;
    s.sides[1].pokemon[0].types = [Type::Steel, Type::None];
    s.sides[1].pokemon[0].base_types = s.sides[1].pokemon[0].types;
    let res = outcomes(&s);
    assert!(res.iter().any(|(_, r)| r.side(SideId::Two).active().status == Status::Toxic),
        "Corrosion Toxic must land on a Steel type");
    // Without Corrosion it must fail.
    let mut s2 = s;
    s2.sides[0].pokemon[0].ability = Ability::None;
    for (_, r) in outcomes(&s2) {
        assert_eq!(r.side(SideId::Two).active().status, Status::None);
    }
}

#[test]
fn leaf_guard_blocks_status_in_sun() {
    let mut s = duel("willowisp", "tackle");
    s.sides[1].pokemon[0].ability = Ability::LeafGuard;
    s.weather = Weather::Sun;
    s.weather_turns = 5;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::Two).active().status, Status::None);
    }
    let mut no_sun = s;
    no_sun.weather = Weather::None;
    no_sun.weather_turns = 0;
    assert!(outcomes(&no_sun).iter().any(|(_, r)| r.side(SideId::Two).active().status == Status::Burn),
        "without sun Leaf Guard must not block");
}

#[test]
fn early_bird_halves_sleep() {
    // Counter 4: a normal sleeper stays asleep at 3; Early Bird drops to 2 and stays.
    let mut s = duel("tackle", "tackle");
    s.sides[0].pokemon[0].ability = Ability::EarlyBird;
    s.sides[0].pokemon[0].status = Status::Sleep;
    s.sides[0].pokemon[0].status_counter = 4;
    for (_, r) in outcomes(&s) {
        let p = r.side(SideId::One).active();
        assert_eq!(p.status, Status::Sleep);
        assert_eq!(p.status_counter, 2, "Early Bird ticks sleep twice");
        assert_eq!(r.side(SideId::Two).active().hp, 400);
    }
    // Counter 2: Early Bird wakes AND acts this turn.
    let mut s2 = s;
    s2.sides[0].pokemon[0].status_counter = 2;
    for (_, r) in outcomes(&s2) {
        assert_eq!(r.side(SideId::One).active().status, Status::None);
        assert!(r.side(SideId::Two).active().hp < 400, "woken mon must move");
    }
}

// ---------- boost-drop blockers ----------

#[test]
fn boost_drop_blockers_and_mirror_armor() {
    for (ability, stat_move, blocked_stat) in [
        (Ability::BigPecks, "leer", BoostIndex::Defense), // Big Pecks blocks only Defense drops
        (Ability::FullMetalBody, "growl", BoostIndex::Attack),
        (Ability::KeenEye, "sandattack", BoostIndex::Accuracy),
        (Ability::MindsEye, "sandattack", BoostIndex::Accuracy),
    ] {
        let mut s = duel(stat_move, "tackle");
        s.sides[1].pokemon[0].ability = ability;
        for (_, r) in outcomes(&s) {
            assert_eq!(r.side(SideId::Two).boost(blocked_stat), 0, "{ability:?} must block the drop");
        }
    }
    // Mirror Armor bounces the drop onto the source.
    let mut s = duel("growl", "tackle");
    s.sides[1].pokemon[0].ability = Ability::MirrorArmor;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::Two).boost(BoostIndex::Attack), 0);
        assert_eq!(r.side(SideId::One).boost(BoostIndex::Attack), -1);
    }
}

// ---------- on-hit reactions ----------

#[test]
fn gooey_drops_attacker_speed_on_contact() {
    let mut s = duel("tackle", "tackle");
    s.sides[1].pokemon[0].ability = Ability::Gooey;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::One).boost(BoostIndex::Speed), -1);
    }
    // Non-contact: no drop.
    let mut nc = duel("swift", "tackle");
    nc.sides[1].pokemon[0].ability = Ability::TanglingHair;
    for (_, r) in outcomes(&nc) {
        assert_eq!(r.side(SideId::One).boost(BoostIndex::Speed), 0);
    }
}

#[test]
fn anger_shell_fires_on_half_hp_crossing() {
    let mut s = duel("tackle", "tackle");
    s.sides[1].pokemon[0].hp = 205; // just above half of 400
    s.sides[1].pokemon[0].ability = Ability::AngerShell;
    for (_, r) in outcomes(&s) {
        let hp = r.side(SideId::Two).active().hp;
        if hp * 2 <= 400 && hp > 0 {
            assert_eq!(r.side(SideId::Two).boost(BoostIndex::Attack), 1);
            assert_eq!(r.side(SideId::Two).boost(BoostIndex::SpecialAttack), 1);
            assert_eq!(r.side(SideId::Two).boost(BoostIndex::Speed), 1);
            assert_eq!(r.side(SideId::Two).boost(BoostIndex::Defense), -1);
            assert_eq!(r.side(SideId::Two).boost(BoostIndex::SpecialDefense), -1);
        }
    }
}

#[test]
fn seed_sower_plants_grassy_terrain() {
    let mut s = duel("tackle", "tackle");
    s.sides[1].pokemon[0].ability = Ability::SeedSower;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.terrain, Terrain::Grassy);
        assert_eq!(r.terrain_turns, 5);
    }
}

#[test]
fn effect_spore_splits_11_10_9() {
    let mut s = duel("tackle", "tackle");
    s.sides[1].pokemon[0].ability = Ability::EffectSpore;
    let res = outcomes(&s);
    let mass = |st: Status| -> f32 {
        res.iter().filter(|(_, r)| r.side(SideId::One).active().status == st).map(|(p, _)| *p).sum()
    };
    assert!((mass(Status::Sleep) - 11.0).abs() < 0.01, "slp {}", mass(Status::Sleep));
    assert!((mass(Status::Paralysis) - 10.0).abs() < 0.01, "par {}", mass(Status::Paralysis));
    assert!((mass(Status::Poison) - 9.0).abs() < 0.01, "psn {}", mass(Status::Poison));
    // Grass types are powder-immune: no procs at all.
    let mut grass = s;
    grass.sides[0].pokemon[0].types = [Type::Grass, Type::None];
    grass.sides[0].pokemon[0].base_types = grass.sides[0].pokemon[0].types;
    for (_, r) in outcomes(&grass) {
        assert_eq!(r.side(SideId::One).active().status, Status::None);
    }
}

#[test]
fn cute_charm_infatuates_opposite_gender_only() {
    let mut s = duel("tackle", "tackle");
    s.sides[0].pokemon[0].gender = 1; // M
    s.sides[1].pokemon[0].gender = 2; // F
    s.sides[1].pokemon[0].ability = Ability::CuteCharm;
    let res = outcomes(&s);
    let attract_mass: f32 = res.iter()
        .filter(|(_, r)| r.side(SideId::One).volatiles.contains(VolatileStatus::Attract))
        .map(|(p, _)| *p).sum();
    assert!((attract_mass - 30.0).abs() < 0.01, "attract mass {attract_mass}");
    // Same gender: never.
    let mut same = s;
    same.sides[0].pokemon[0].gender = 2;
    for (_, r) in outcomes(&same) {
        assert!(!r.side(SideId::One).volatiles.contains(VolatileStatus::Attract));
    }
}

#[test]
fn attract_immobilizes_half_the_time() {
    let mut s = duel("tackle", "tackle");
    s.sides[0].volatiles.insert(VolatileStatus::Attract);
    let res = outcomes(&s);
    let no_move_mass: f32 = res.iter()
        .filter(|(_, r)| r.side(SideId::Two).active().hp == 400)
        .map(|(p, _)| *p).sum();
    assert!((no_move_mass - 50.0).abs() < 0.01, "immobilized mass {no_move_mass}");
}

// ---------- item theft / removal ----------

#[test]
fn pickpocket_steals_on_contact() {
    let mut s = duel("tackle", "tackle");
    s.sides[0].pokemon[0].item = Item::Leftovers;
    s.sides[1].pokemon[0].ability = Ability::Pickpocket;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::One).active().item, Item::None);
        assert_eq!(r.side(SideId::Two).active().item, Item::Leftovers);
    }
}

#[test]
fn magician_steals_target_item() {
    let mut s = duel("swift", "tackle"); // non-contact still steals (any damaging hit)
    s.sides[0].pokemon[0].ability = Ability::Magician;
    s.sides[1].pokemon[0].item = Item::Leftovers;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::One).active().item, Item::Leftovers);
        assert_eq!(r.side(SideId::Two).active().item, Item::None);
    }
}

#[test]
fn knock_off_respects_species_locked_items() {
    let mut locked = duel("knockoff", "tackle");
    locked.sides[1].pokemon[0] = mon("zaciancrowned", [Type::Fairy, Type::Steel], [400, 180, 250, 180, 220, 130]);
    locked.sides[1].pokemon[0].item = Item::RustedSword;
    locked.sides[1].pokemon[0].moves[0] = slot("tackle");
    let mut bare = locked;
    bare.sides[1].pokemon[0].item = Item::None;
    let res_locked = outcomes(&locked);
    for (_, r) in &res_locked {
        assert_eq!(r.side(SideId::Two).active().item, Item::RustedSword, "Rusted Sword can't be knocked off");
    }
    // No removable item -> no 1.5x: damage distribution must match the itemless case.
    assert_eq!(min_hp(&res_locked, SideId::Two), min_hp(&outcomes(&bare), SideId::Two));
    assert_eq!(max_hp_seen(&res_locked, SideId::Two), max_hp_seen(&outcomes(&bare), SideId::Two));
}

// ---------- volatile blockers ----------

#[test]
fn aroma_veil_blocks_taunt() {
    let mut s = duel("taunt", "tackle");
    s.sides[1].pokemon[0].ability = Ability::AromaVeil;
    for (_, r) in outcomes(&s) {
        assert!(!r.side(SideId::Two).volatiles.contains(VolatileStatus::Taunt));
        assert_eq!(r.side(SideId::Two).taunt_turns, 0);
    }
}

// ---------- Magic Bounce ----------

#[test]
fn magic_bounce_reflects_status_and_hazards() {
    // Thunder Wave bounces back onto the user (90% accuracy re-rolled from the bouncer).
    let mut s = duel("thunderwave", "tackle");
    s.sides[1].pokemon[0].ability = Ability::MagicBounce;
    let res = outcomes(&s);
    let user_par: f32 = res.iter()
        .filter(|(_, r)| r.side(SideId::One).active().status == Status::Paralysis)
        .map(|(p, _)| *p).sum();
    for (_, r) in &res {
        assert_eq!(r.side(SideId::Two).active().status, Status::None, "the holder is never paralyzed");
    }
    assert!((user_par - 90.0).abs() < 0.01, "bounced T-Wave lands on the user at its 90%: {user_par}");
    // Stealth Rock lands on the USER's side.
    let mut sr = duel("stealthrock", "tackle");
    sr.sides[1].pokemon[0].ability = Ability::MagicBounce;
    for (_, r) in outcomes(&sr) {
        assert!(!r.side(SideId::Two).side_conditions.stealth_rock);
        assert!(r.side(SideId::One).side_conditions.stealth_rock, "bounced rocks go to the user's side");
    }
    // Mold Breaker pierces the bounce (holder gets paralyzed on the 90% hit branch).
    let mut mb = s;
    mb.sides[0].pokemon[0].ability = Ability::MoldBreaker;
    let res_mb = outcomes(&mb);
    assert!(res_mb.iter().any(|(_, r)| r.side(SideId::Two).active().status == Status::Paralysis));
    for (_, r) in &res_mb {
        assert_eq!(r.side(SideId::One).active().status, Status::None);
    }
}

// ---------- Dancer ----------

#[test]
fn dancer_copies_swords_dance() {
    let mut s = duel("swordsdance", "tackle");
    s.sides[1].pokemon[0].ability = Ability::Dancer;
    for (_, r) in outcomes(&s) {
        assert_eq!(r.side(SideId::One).boost(BoostIndex::Attack), 2, "original user boosts");
        assert_eq!(r.side(SideId::Two).boost(BoostIndex::Attack), 2, "Dancer copies the boost");
    }
    // The copy costs the dancer no PP.
    for o in generate_move_action(&s, SideId::One, 0, None, None) {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(r.side(SideId::Two).active().moves[0].pp, 10);
    }
}

#[test]
fn dancer_copies_fiery_dance_back_at_the_user() {
    let mut s = duel("fierydance", "tackle");
    s.sides[1].pokemon[0].ability = Ability::Dancer;
    let res = outcomes(&s);
    assert!(min_hp(&res, SideId::Two) < 400, "original Fiery Dance damages the dancer");
    assert!(min_hp(&res, SideId::One) < 300, "copied Fiery Dance damages the original user");
}

// ---------- ordering: Custap / Mycelium Might ----------

#[test]
fn custap_berry_grants_first_strike_in_bracket() {
    // Slow holder at <=1/4 HP with Custap KOs the much faster foe before it can act.
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("ambipom", [Type::Normal, Type::None], [300, 400, 180, 180, 180, 50]);
    s.sides[0].pokemon[0].hp = 60; // 60/300 <= 1/4
    s.sides[0].pokemon[0].item = Item::CustapBerry;
    s.sides[0].pokemon[0].moves[0] = slot("tackle");
    s.sides[1].pokemon[0] = mon("eiscue", [Type::Ice, Type::None], [80, 400, 60, 180, 60, 250]);
    s.sides[1].pokemon[0].moves[0] = slot("tackle");
    for (_, r) in full_turn(&s) {
        assert!(r.side(SideId::Two).active().hp <= 0, "custap holder must strike first and KO");
        assert_eq!(r.side(SideId::One).active().hp, 60, "the faster foe never moved");
        assert_eq!(r.side(SideId::One).active().item, Item::None, "berry consumed");
        assert_eq!(r.side(SideId::One).active().last_berry, Item::CustapBerry);
    }
    // Above 1/4 HP the berry does nothing: the faster foe moves first.
    let mut healthy = s;
    healthy.sides[0].pokemon[0].hp = 300;
    assert!(full_turn(&healthy).iter().any(|(_, r)| r.side(SideId::One).active().hp < 300),
        "without the pinch the fast foe hits first");
}

#[test]
fn mycelium_might_status_moves_act_last_and_ignore_abilities() {
    // Faster MM holder uses Spore; the slower foe still gets its hit in first.
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("toedscruel", [Type::Ground, Type::Grass], [300, 180, 180, 180, 180, 300]);
    s.sides[0].pokemon[0].ability = Ability::MyceliumMight;
    s.sides[0].pokemon[0].moves[0] = slot("spore");
    s.sides[1].pokemon[0] = mon("eiscue", [Type::Ice, Type::None], [400, 180, 250, 180, 220, 100]);
    s.sides[1].pokemon[0].ability = Ability::Insomnia; // ignored by Mycelium Might
    s.sides[1].pokemon[0].moves[0] = slot("tackle");
    for (_, r) in full_turn(&s) {
        assert!(r.side(SideId::One).active().hp < 300, "the slower foe attacked before the -0.1 Spore");
        assert_eq!(r.side(SideId::Two).active().status, Status::Sleep, "Spore pierces Insomnia via ignoreAbility");
    }
}

// ---------- residuals ----------

#[test]
fn bad_dreams_damages_sleeping_foe_each_turn() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("darkrai", [Type::Dark, Type::None], [300, 180, 180, 300, 180, 350]);
    s.sides[0].pokemon[0].ability = Ability::BadDreams;
    s.sides[0].pokemon[0].moves[0] = slot("calmmind");
    s.sides[1].pokemon[0] = mon("eiscue", [Type::Ice, Type::None], [400, 180, 250, 180, 220, 130]);
    s.sides[1].pokemon[0].status = Status::Sleep;
    s.sides[1].pokemon[0].status_counter = 4;
    s.sides[1].pokemon[0].moves[0] = slot("tackle");
    for (_, r) in full_turn(&s) {
        assert_eq!(r.side(SideId::Two).active().hp, 350, "sleeper loses 1/8 max HP (50) at end of turn");
    }
    // Awake foe: untouched.
    let mut awake = s;
    awake.sides[1].pokemon[0].status = Status::None;
    awake.sides[1].pokemon[0].status_counter = 0;
    for (_, r) in full_turn(&awake) {
        assert!(r.side(SideId::Two).active().hp > 350, "no Bad Dreams chip while awake");
    }
}

#[test]
fn cud_chew_reeats_the_berry_at_the_end_of_next_turn() {
    // Turn 1: a hit drops the holder to <=1/2, Sitrus is eaten, counter is set (2 -> ticks
    // to 1 at that same end of turn).
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("tauros", [Type::Normal, Type::None], [400, 180, 180, 180, 180, 100]);
    s.sides[0].pokemon[0].ability = Ability::CudChew;
    s.sides[0].pokemon[0].item = Item::SitrusBerry;
    s.sides[0].pokemon[0].hp = 210;
    s.sides[0].pokemon[0].moves[0] = slot("calmmind");
    s.sides[1].pokemon[0] = mon("eiscue", [Type::Ice, Type::None], [400, 250, 250, 180, 220, 130]);
    s.sides[1].pokemon[0].moves[0] = slot("tackle");
    let turn1 = full_turn(&s);
    let ate: Vec<&(f32, State)> = turn1.iter()
        .filter(|(_, r)| r.side(SideId::One).active().last_berry == Item::SitrusBerry)
        .collect();
    assert!(!ate.is_empty(), "some branch must eat the Sitrus");
    for (_, r) in &ate {
        assert_eq!(r.side(SideId::One).active().cudchew_turns, 1,
            "counter set to 2 on eat, ticked once at this end of turn");
    }
    // Turn 2 (constructed): counter 1 -> the residual re-eats the stored berry (+25% max HP).
    let mut s2 = s;
    s2.sides[0].pokemon[0].item = Item::None;
    s2.sides[0].pokemon[0].last_berry = Item::SitrusBerry;
    s2.sides[0].pokemon[0].cudchew_turns = 1;
    s2.sides[0].pokemon[0].hp = 100;
    s2.sides[1].pokemon[0].stats[1] = 10; // make the foe's tackle negligible
    s2.sides[1].pokemon[0].moves[0] = slot("calmmind");
    for (_, r) in full_turn(&s2) {
        assert_eq!(r.side(SideId::One).active().cudchew_turns, 0);
        assert_eq!(r.side(SideId::One).active().hp, 200, "re-eaten Sitrus heals 1/4 max HP");
    }
}
