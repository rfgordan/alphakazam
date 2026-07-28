//! `Pivot::Pause` belongs to the move that RUNS, not to the move that was CHOSEN.
//!
//! `Flow::run_turn` decides whether a turn pauses for a landing/revive by looking at the chosen
//! move slot — a self-switch move with an alive bench, or Revival Blessing with a fainted bench —
//! and stamps `Pivot::Pause` on the action. `run_move_action` then substitutes the move under two
//! rules that `run_turn` cannot see: **Struggle** (`no_usable_move`) and **the Encore redirect**
//! (`runEvent('OverrideAction')`, the first thing `runMove` does). The `match pivot` arms at the
//! end of every hit path key on the ACTION's pivot, so the substituted move inherited the pause.
//!
//! The lethal instance: a PP-stalled mon whose bench is entirely FAINTED picks Revival Blessing,
//! which grants `Pivot::Pause` off `has_fainted_bench`; `no_usable_move` then replaces it with
//! Struggle, whose damaging path pushes `PivotPending`; and `Flow::resume_pivot` is handed a
//! `PivotLanding` request for a side with no live bench. That is the tripwire that killed a
//! 4096-env trainer at ~1e-5 games.
//!
//! PS's own gate is `sim/battle.ts:2904` — `if (switches[i] && !this.canSwitch(this.sides[i]))`
//! clears `switchFlag` and drops the side out of `switches`, i.e. with nowhere to go the mon
//! simply stays in and no switch request is issued.

use engine::ids::{MoveId, Species};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::{MoveSlot, Pokemon, SideId, State};

fn mon(species: &str, moves: &[(&str, u8)]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = engine::data::species_types(p.species);
    p.base_types = p.types;
    p.hp = 300;
    p.max_hp = 300;
    p.stats = [300, 150, 150, 150, 150, 150];
    for (i, (m, pp)) in moves.iter().enumerate() {
        p.moves[i] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: *pp, max_pp: 16, disabled: false };
    }
    p
}

/// Side One: a Pawmot with Revival Blessing at 0 PP and nothing else usable, one FAINTED ally and
/// no live one. Side Two: an ordinary attacker.
fn stalled_reviver_board() -> State {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("pawmot", &[("revivalblessing", 0)]);
    s.sides[0].pokemon[1] = mon("kingambit", &[("ironhead", 16)]);
    s.sides[0].pokemon[1].hp = 0; // fainted: a Revival Blessing target, NOT a landing target
    s.sides[1].pokemon[0] = mon("blissey", &[("splash", 16)]);
    s.sides[1].pokemon[0].stats[5] = 1; // slower, so side one moves first
    s
}

#[test]
fn struggle_does_not_inherit_revival_blessings_pause() {
    let mut flow = Flow::new(stalled_reviver_board(), 11);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert!(
        !matches!(req, Request::PivotLanding { .. }),
        "Struggle inherited Revival Blessing's Pivot::Pause and asked for a landing: {req:?}"
    );
    // The Pawmot Struggles and stays in; nothing was revived.
    assert_eq!(flow.state.side(SideId::One).active_index, 0);
    assert!(!flow.state.side(SideId::One).pokemon[1].is_alive());
}

/// The same substitution the other way: a chosen SELF-SWITCH move replaced by Struggle must not
/// pivot either — the pause is legal here (there IS a live bench), so the bug is silent unless the
/// executed move is checked.
#[test]
fn struggle_does_not_inherit_a_pivot_moves_pause() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("dragapult", &[("uturn", 0)]);
    s.sides[0].pokemon[1] = mon("kingambit", &[("ironhead", 16)]);
    s.sides[1].pokemon[0] = mon("blissey", &[("splash", 16)]);
    s.sides[1].pokemon[0].stats[5] = 1;

    let mut flow = Flow::new(s, 11);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert!(
        !matches!(req, Request::PivotLanding { .. }),
        "a 0-PP U-turn Struggles; Struggle is not a self-switch move: {req:?}"
    );
    assert_eq!(flow.state.side(SideId::One).active_index, 0, "the Struggler stays in");
}

/// A real self-switch move whose bench empties is PS's `!canSwitch` case: no request, stay in.
/// (Reached here by giving the pivot user no live ally at all — `run_turn`'s own guard covers the
/// straightforward version, so this pins the emission-site guard.)
#[test]
fn pivot_with_no_live_ally_issues_no_request() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("dragapult", &[("uturn", 16)]);
    s.sides[0].pokemon[1] = mon("kingambit", &[("ironhead", 16)]);
    s.sides[0].pokemon[1].hp = 0;
    s.sides[1].pokemon[0] = mon("blissey", &[("splash", 16)]);
    s.sides[1].pokemon[0].stats[5] = 1;

    let mut flow = Flow::new(s, 11);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert!(!matches!(req, Request::PivotLanding { .. }), "nowhere to go = no request: {req:?}");
    assert_eq!(flow.state.side(SideId::One).active_index, 0);
}

/// Revival Blessing itself still pauses when it actually runs.
#[test]
fn revival_blessing_still_raises_its_request() {
    let mut s = stalled_reviver_board();
    s.sides[0].pokemon[0].moves[0].pp = 16;
    let mut flow = Flow::new(s, 11);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert_eq!(req, Request::Revive { side: SideId::One });
}
