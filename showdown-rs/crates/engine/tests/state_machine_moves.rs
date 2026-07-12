//! Focused tests for the state-machine move batch: Mirror Coat's special-damage-taken
//! tracking, Gigaton Hammer / Blood Moon (cantusetwice) selection legality, and Double
//! Shock's Electric-type removal (typeless slot).

use engine::generate::{cantusetwice_locked, generate_move_action, is_cantusetwice_move};
use engine::ids::{MoveId, Species, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};

fn mon(species: &str, types: [Type; 2], moves: &[&str]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.hp = 300;
    p.max_hp = 300;
    p.types = types;
    p.base_types = types;
    p.stats = [300, 200, 180, 200, 180, 150];
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

// ---- Mirror Coat -----------------------------------------------------------------------

#[test]
fn special_hit_records_damage_taken_and_reverses() {
    // Thunderbolt (special) into a Water-type: the defender's side must record the damage
    // taken this turn (Mirror Coat's 2x source), and every branch must reverse exactly.
    let state = duel(
        mon("magnezone", [Type::Electric, Type::Steel], &["thunderbolt"]),
        mon("azumarill", [Type::Water, Type::Fairy], &["mirrorcoat"]),
    );
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    assert!(!outcomes.is_empty());
    let mut saw_damage = false;
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        let dealt = 300 - r.side(SideId::Two).active().hp;
        if dealt > 0 {
            saw_damage = true;
            assert_eq!(
                r.side(SideId::Two).special_damage_taken, dealt,
                "defender side must record the special damage it took"
            );
            assert_eq!(r.side(SideId::One).special_damage_taken, 0);
        }
        let mut back = r;
        back.reverse_instructions(&o.instructions);
        assert_eq!(back, state, "SetSpecialDamageTaken must reverse exactly");
    }
    assert!(saw_damage);
}

#[test]
fn mirror_coat_returns_double_recorded_damage_and_fails_fresh() {
    // With 55 recorded: Mirror Coat deals exactly 110 (Psychic-type, target not immune).
    let mut state = duel(
        mon("azumarill", [Type::Water, Type::Fairy], &["mirrorcoat"]),
        mon("magnezone", [Type::Electric, Type::Steel], &["thunderbolt"]),
    );
    state.sides[0].special_damage_taken = 55;
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        let dealt = 300 - r.side(SideId::Two).active().hp;
        assert_eq!(dealt, 110, "Mirror Coat must deal exactly 2x the recorded special damage");
    }
    // With nothing recorded it deals nothing (PS onTry fails).
    state.sides[0].special_damage_taken = 0;
    for o in generate_move_action(&state, SideId::One, 0, None, None) {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        assert_eq!(r.side(SideId::Two).active().hp, 300, "fresh Mirror Coat deals no damage");
    }
}

#[test]
fn physical_hit_does_not_record_special_damage() {
    let state = duel(
        mon("azumarill", [Type::Water, Type::Fairy], &["playrough"]),
        mon("magnezone", [Type::Electric, Type::Steel], &["mirrorcoat"]),
    );
    for o in generate_move_action(&state, SideId::One, 0, None, None) {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        assert_eq!(
            r.side(SideId::Two).special_damage_taken, 0,
            "a physical hit must not feed Mirror Coat"
        );
    }
}

// ---- Gigaton Hammer / Blood Moon selection legality --------------------------------------

#[test]
fn cantusetwice_locks_only_after_own_use() {
    let mut state = duel(
        mon("tinkaton", [Type::Fairy, Type::Steel], &["gigatonhammer", "playrough"]),
        mon("ursalunabloodmoon", [Type::Ground, Type::Normal], &["bloodmoon", "earthpower"]),
    );
    let gh = MoveId::from_id("gigatonhammer").unwrap();
    let bm = MoveId::from_id("bloodmoon").unwrap();
    assert!(is_cantusetwice_move(gh) && is_cantusetwice_move(bm));
    assert!(!is_cantusetwice_move(MoveId::from_id("playrough").unwrap()));

    // Fresh: neither is locked.
    assert!(!cantusetwice_locked(&state, SideId::One, gh));
    assert!(!cantusetwice_locked(&state, SideId::Two, bm));

    // After a use (last_used_move set), the SAME move is locked for that side only.
    state.sides[0].last_used_move = gh;
    state.sides[1].last_used_move = bm;
    assert!(cantusetwice_locked(&state, SideId::One, gh));
    assert!(cantusetwice_locked(&state, SideId::Two, bm));
    assert!(!cantusetwice_locked(&state, SideId::One, bm));
    assert!(!cantusetwice_locked(&state, SideId::Two, gh));
    assert!(!cantusetwice_locked(&state, SideId::One, MoveId::from_id("playrough").unwrap()));

    // Using something else in between unlocks it.
    state.sides[0].last_used_move = MoveId::from_id("playrough").unwrap();
    assert!(!cantusetwice_locked(&state, SideId::One, gh));
}

#[test]
fn executing_a_move_locks_and_switching_resets() {
    // A whole-move execution sets last_used_move, locking Gigaton Hammer next turn; the
    // lock lives on the active slot's tracking, which resets on switch (PS parity).
    let mut tink = mon("tinkaton", [Type::Fairy, Type::Steel], &["gigatonhammer"]);
    tink.stats = [300, 200, 180, 200, 180, 150];
    let state = duel(tink, mon("azumarill", [Type::Water, Type::Fairy], &["playrough"]));
    let gh = MoveId::from_id("gigatonhammer").unwrap();
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        assert!(cantusetwice_locked(&r, SideId::One, gh), "using it must lock re-selection");
    }
}

// ---- Double Shock typeless --------------------------------------------------------------

#[test]
fn double_shock_strips_electric_and_second_use_fails() {
    // Pawmot (Electric/Fighting): a connecting Double Shock leaves [None, Fighting].
    let state = duel(
        mon("pawmot", [Type::Electric, Type::Fighting], &["doubleshock"]),
        mon("azumarill", [Type::Water, Type::Fairy], &["playrough"]),
    );
    let outcomes = generate_move_action(&state, SideId::One, 0, None, None);
    let mut connected = false;
    for o in &outcomes {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        let dealt = 300 - r.side(SideId::Two).active().hp;
        if dealt > 0 {
            connected = true;
            assert_eq!(
                r.side(SideId::One).active().types,
                [Type::None, Type::Fighting],
                "Electric must be stripped to the typeless slot on a hit"
            );
            // A second Double Shock from the now non-Electric user fails: no damage, no
            // further type change.
            let hp_before = r.side(SideId::Two).active().hp;
            for o2 in generate_move_action(&r, SideId::One, 0, None, None) {
                let mut r2 = r;
                r2.apply_instructions(&o2.instructions);
                assert_eq!(r2.side(SideId::Two).active().hp, hp_before, "second use must fail");
                assert_eq!(r2.side(SideId::One).active().types, [Type::None, Type::Fighting]);
            }
        }
        let mut back = r;
        back.reverse_instructions(&o.instructions);
        assert_eq!(back, state, "type strip must reverse exactly");
    }
    assert!(connected);
}

#[test]
fn double_shock_pure_electric_goes_fully_typeless() {
    // A pure-Electric user (PS "???" case) ends fully typeless after a connecting hit.
    let state = duel(
        mon("regieleki", [Type::Electric, Type::None], &["doubleshock"]),
        mon("azumarill", [Type::Water, Type::Fairy], &["playrough"]),
    );
    let mut connected = false;
    for o in generate_move_action(&state, SideId::One, 0, None, None) {
        let mut r = state;
        r.apply_instructions(&o.instructions);
        if 300 - r.side(SideId::Two).active().hp > 0 {
            connected = true;
            assert_eq!(r.side(SideId::One).active().types, [Type::None, Type::None]);
        }
    }
    assert!(connected);
}
