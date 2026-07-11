//! `is_trapped` legality: every trapping source and every exemption, on fixtures built from
//! `team::default_matchup` with targeted field surgery (same pattern as tests/request_flow.rs).
//!
//! Side One's default active is Great Tusk (Ground/Fighting — grounded, non-Steel, non-Ghost),
//! which makes it a clean victim for each ability trap.

use engine::generate::is_trapped;
use engine::ids::{Ability, Item, Type};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::SideId;
use engine::team;
use engine::volatile::VolatileStatus;

fn base() -> engine::state::State {
    team::default_matchup()
}

#[test]
fn baseline_not_trapped() {
    let state = base();
    assert!(!is_trapped(&state, SideId::One));
    assert!(!is_trapped(&state, SideId::Two));
}

// --- opposing-ability traps -----------------------------------------------------------------

#[test]
fn arena_trap_traps_grounded_foes_only() {
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ArenaTrap;
    assert!(is_trapped(&state, SideId::One), "grounded foe is held");

    // Flying-type: exempt.
    state.side_mut(SideId::One).pokemon[0].types = [Type::Flying, Type::Ground];
    assert!(!is_trapped(&state, SideId::One), "Flying-types are not grounded");
    state.side_mut(SideId::One).pokemon[0].types = [Type::Ground, Type::Fighting];

    // Levitate: exempt.
    state.side_mut(SideId::One).pokemon[0].ability = Ability::Levitate;
    assert!(!is_trapped(&state, SideId::One), "Levitate is not grounded");
    state.side_mut(SideId::One).pokemon[0].ability = Ability::Protosynthesis;

    // Air Balloon: exempt.
    state.side_mut(SideId::One).pokemon[0].item = Item::AirBalloon;
    assert!(!is_trapped(&state, SideId::One), "Air Balloon is not grounded");
}

#[test]
fn shadow_tag_traps_all_but_other_shadow_tag() {
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ShadowTag;
    assert!(is_trapped(&state, SideId::One));
    // Mirror: a Shadow Tag holder is immune to the foe's Shadow Tag.
    state.side_mut(SideId::One).pokemon[0].ability = Ability::ShadowTag;
    assert!(!is_trapped(&state, SideId::One));
    assert!(!is_trapped(&state, SideId::Two));
}

#[test]
fn magnet_pull_traps_steel_only() {
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::MagnetPull;
    assert!(!is_trapped(&state, SideId::One), "Great Tusk is not Steel");
    state.side_mut(SideId::One).pokemon[0].types = [Type::Steel, Type::Fighting];
    assert!(is_trapped(&state, SideId::One));
}

#[test]
fn fainted_trapper_does_not_trap() {
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ArenaTrap;
    state.side_mut(SideId::Two).pokemon[0].hp = 0;
    assert!(!is_trapped(&state, SideId::One));
}

// --- volatile traps ---------------------------------------------------------------------------

#[test]
fn volatile_traps_hold() {
    for v in [
        VolatileStatus::PartiallyTrapped,
        VolatileStatus::Trapped,
        VolatileStatus::Ingrain,
        VolatileStatus::NoRetreat,
        VolatileStatus::Octolock,
    ] {
        let mut state = base();
        state.side_mut(SideId::One).volatiles.insert(v);
        assert!(is_trapped(&state, SideId::One), "{v:?} must trap its holder");
        assert!(!is_trapped(&state, SideId::Two), "{v:?} must not trap the other side");
    }
}

// --- exemptions --------------------------------------------------------------------------------

#[test]
fn ghost_types_always_escape() {
    // Natural Ghost typing beats abilities and volatiles alike.
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ShadowTag;
    state.side_mut(SideId::One).pokemon[0].types = [Type::Ghost, Type::Fighting];
    state.side_mut(SideId::One).volatiles.insert(VolatileStatus::PartiallyTrapped);
    state.side_mut(SideId::One).volatiles.insert(VolatileStatus::Trapped);
    assert!(!is_trapped(&state, SideId::One));

    // Tera-Ghost: `types` collapse to the tera type on Terastallization.
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ArenaTrap;
    {
        let p = &mut state.side_mut(SideId::One).pokemon[0];
        p.terastallized = true;
        p.tera_type = Type::Ghost;
        p.types = [Type::Ghost, Type::None];
    }
    assert!(!is_trapped(&state, SideId::One));
}

#[test]
fn shed_shell_always_escapes() {
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ShadowTag;
    state.side_mut(SideId::One).pokemon[0].item = Item::ShedShell;
    state.side_mut(SideId::One).volatiles.insert(VolatileStatus::PartiallyTrapped);
    state.side_mut(SideId::One).volatiles.insert(VolatileStatus::Ingrain);
    assert!(!is_trapped(&state, SideId::One));
}

#[test]
fn fainted_active_is_never_trapped() {
    // Replacement legality is the Replace phase's business; a fainted active reports untrapped.
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ShadowTag;
    state.side_mut(SideId::One).pokemon[0].hp = 0;
    assert!(!is_trapped(&state, SideId::One));
}

// --- flow integration ---------------------------------------------------------------------------

#[test]
fn flow_does_not_force_a_trapped_mon_out() {
    // Foe holds side one with Shadow Tag; submitting a Switch on a Turn request must be coerced
    // into a move, leaving side one's active in place.
    let mut state = base();
    state.side_mut(SideId::Two).pokemon[0].ability = Ability::ShadowTag;
    let mut flow = Flow::new(state, 11);
    assert_eq!(flow.request(), Request::Turn);
    flow.submit([
        Some(PlayerChoice::Switch { slot: 1 }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    let s1 = flow.state.side(SideId::One);
    assert_eq!(s1.active_index, 0, "trapped mon must not have switched");
}
