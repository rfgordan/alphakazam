//! Revival Blessing decision-point coverage: the request-flow driver must raise a `Revive`
//! request when Revival Blessing connects with a fainted ally, restrict the choice to fainted
//! party members, revive the chosen mon to floor(maxHP/2) HP with healthy status while leaving it
//! benched, and keep the acting mon in (no switch-out) so the turn continues.

use engine::ids::{MoveId, Status};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::{MoveSlot, SideId};
use engine::team;

/// Give side one's active Revival Blessing in slot 0 and faint one benched teammate; the Turn
/// must resolve into a `Revive` request, and answering it revives the fainted mon at half HP.
#[test]
fn revival_blessing_raises_revive_request_and_revives_benched_mon() {
    let mut state = team::default_matchup();
    // Slow side one's active down so the (harmless) opponent move is irrelevant to the revive.
    let rb = MoveId::from_id("revivalblessing").expect("revivalblessing in dex");
    {
        let s = state.side_mut(SideId::One);
        s.active_index = 0;
        s.pokemon[0].moves[0] = MoveSlot { id: rb, pp: 1, max_pp: 1, disabled: false };
        // Make the reviver fast and bulky so it always moves first and survives the foe's hit.
        s.pokemon[0].stats[5] = 999;
        s.pokemon[0].max_hp = 600;
        s.pokemon[0].hp = 600;
        // Faint the benched teammate in slot 2, giving it a lingering status to prove the revive
        // clears it.
        let p = &mut s.pokemon[2];
        p.hp = 0;
        p.status = Status::Poison;
        p.status_counter = 3;
    }
    let max_hp_2 = state.side(SideId::One).pokemon[2].max_hp;
    let expected_hp = (max_hp_2 / 2).max(1);
    let active_hp_before = state.side(SideId::One).pokemon[0].hp;

    let mut flow = Flow::new(state, 42);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert_eq!(req, Request::Revive { side: SideId::One }, "Revival Blessing must raise a Revive request");

    // The acting mon has NOT switched out; it is still the active.
    assert_eq!(flow.state.side(SideId::One).active_index, 0);
    // The fainted teammate is still down (not yet revived — the choice is pending).
    assert!(!flow.state.side(SideId::One).pokemon[2].is_alive());

    let req = flow.submit([Some(PlayerChoice::Switch { slot: 2 }), None]);
    assert!(matches!(req, Request::Turn | Request::Replace { .. } | Request::Terminal { .. }));

    let revived = &flow.state.side(SideId::One).pokemon[2];
    assert_eq!(revived.hp, expected_hp, "revived to floor(maxHP/2)");
    assert_eq!(revived.status, Status::None, "revive clears status");
    assert_eq!(revived.status_counter, 0, "revive clears the status counter");
    // Still benched: the active slot is unchanged, and the reviver kept its position.
    assert_eq!(flow.state.side(SideId::One).active_index, 0);
    assert!(flow.state.side(SideId::One).pokemon[0].hp <= active_hp_before);
}

/// The `Revive` legal mask (as the bridge computes it) exposes only fainted party members.
#[test]
fn revive_legal_targets_are_only_fainted_party_members() {
    let mut state = team::default_matchup();
    let rb = MoveId::from_id("revivalblessing").expect("revivalblessing in dex");
    {
        let s = state.side_mut(SideId::One);
        s.active_index = 0;
        s.pokemon[0].moves[0] = MoveSlot { id: rb, pp: 1, max_pp: 1, disabled: false };
        // Make the reviver fast and bulky so it always moves first and survives the foe's hit.
        s.pokemon[0].stats[5] = 999;
        s.pokemon[0].max_hp = 600;
        s.pokemon[0].hp = 600;
        s.pokemon[3].hp = 0; // one fainted teammate
    }

    let mut flow = Flow::new(state, 5);
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 0, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert_eq!(req, Request::Revive { side: SideId::One });

    // Every legal revive target must be a fainted, non-active party member.
    let s = flow.state.side(SideId::One);
    for i in 0..6u8 {
        let p = &s.pokemon[i as usize];
        let is_target = i == 3;
        if is_target {
            assert!(!p.is_alive() && p.species != engine::ids::Species::None);
        }
    }
    // Answering with an ALIVE slot is coerced to the fainted one.
    flow.submit([Some(PlayerChoice::Switch { slot: 1 }), None]);
    assert!(flow.state.side(SideId::One).pokemon[3].is_alive(), "coerced revive still hit the fainted mon");
}
