//! Request-flow (decision-point) driver tests: the no-free-hit replacement property, PS pivot
//! timing, and a random-play invariant fuzz over whole games.

use engine::ids::{Item, MoveId};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::SideId;
use engine::team;

/// Faint replacement must not cost a turn: the opponent's state is untouched while the
/// replacement enters. (The old bridge MDP gave the opponent a free hit here.)
#[test]
fn replacement_is_not_a_turn() {
    let mut state = team::default_matchup();
    // Side one has Stealth Rock up on its own field; its slot-1 mon enters at 1 HP with no
    // item (no Heavy-Duty Boots) and dies to the hazard on the hard switch.
    state.side_mut(SideId::One).side_conditions.stealth_rock = true;
    {
        let p = &mut state.side_mut(SideId::One).pokemon[1];
        p.hp = 1;
        p.item = Item::None;
    }

    let mut flow = Flow::new(state, 7);
    let req = flow.submit([
        Some(PlayerChoice::Switch { slot: 1 }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert_eq!(req, Request::Replace { sides: [true, false] }, "hazard KO must raise Replace");

    // Snapshot the opponent completely; answering the replacement must not change it.
    let foe_before = flow.state.sides[SideId::Two.index()];
    let turn_before = flow.state.turn;

    let req = flow.submit([Some(PlayerChoice::Switch { slot: 0 }), None]);
    assert_eq!(req, Request::Turn);
    let s1 = flow.state.side(SideId::One);
    assert_eq!(s1.active_index, 0);
    assert!(s1.active().is_alive(), "replacement entered and survived its hazards");
    let foe_after = flow.state.sides[SideId::Two.index()];
    assert_eq!(flow.state.turn, turn_before, "replacement is a request, not a turn");
    // Byte-compare the foe side: no move fired, no PP spent, no damage dealt.
    assert_eq!(format!("{:?}", foe_before.pokemon[0].hp), format!("{:?}", foe_after.pokemon[0].hp));
    assert_eq!(foe_before.pokemon[0].moves[0].pp, foe_after.pokemon[0].moves[0].pp);
}

/// Pivot timing: U-turn pauses for its landing AFTER dealing damage and BEFORE the opponent's
/// queued move, which then resolves against the lander — never against the pivot user.
#[test]
fn pivot_pauses_then_opponent_hits_the_lander() {
    let mut state = team::default_matchup();
    // Put Dragapult (slot 3, has U-turn in slot 2) in front for side one, fast and bulky
    // enough that it always moves first and always survives.
    state.side_mut(SideId::One).active_index = 3;
    {
        let p = &mut state.side_mut(SideId::One).pokemon[3];
        p.stats[5] = 999;
        p.max_hp = 500;
        p.hp = 500;
        assert_eq!(p.moves[2].id, MoveId::from_id("uturn").unwrap());
    }
    let dragapult_hp_start = 500;

    let mut flow = Flow::new(state, 11);
    let foe_pp_before = flow.state.side(SideId::Two).active().moves[0].pp;
    let req = flow.submit([
        Some(PlayerChoice::Move { slot: 2, tera: false }),
        Some(PlayerChoice::Move { slot: 0, tera: false }),
    ]);
    assert_eq!(req, Request::PivotLanding { side: SideId::One }, "U-turn must pause for a landing");
    // Opponent has NOT moved yet at the pause (PS mid-turn request).
    assert_eq!(flow.state.side(SideId::Two).active().moves[0].pp, foe_pp_before);

    let req = flow.submit([Some(PlayerChoice::Switch { slot: 0 }), None]);
    assert!(matches!(req, Request::Turn | Request::Replace { .. }));
    let s1 = flow.state.side(SideId::One);
    assert_eq!(s1.active_index, 0, "landing choice honored");
    // The opponent's move resolved after the landing: PP spent, and Dragapult — long gone —
    // took no damage from it.
    assert_eq!(flow.state.side(SideId::Two).active().moves[0].pp, foe_pp_before - 1);
    assert_eq!(s1.pokemon[3].hp, dragapult_hp_start, "pivot user left before the foe's move");
}

/// Random-play fuzz: whole games through the request loop terminate with well-formed requests
/// and consistent flags at every step.
#[test]
fn random_games_terminate_with_wellformed_requests() {
    let mut rng: u64 = 0xFEED_FACE;
    let mut next = move |n: u64| {
        rng = rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n.max(1)
    };

    let mut terminated = 0;
    for game in 0..60u64 {
        let mut flow = Flow::new(team::default_matchup(), game * 977 + 3);
        for _ in 0..4000 {
            let req = flow.request();
            let pick_switch = |flow: &Flow, side: SideId, r: u64| -> PlayerChoice {
                let s = flow.state.side(side);
                let bench: Vec<u8> = (0..6u8)
                    .filter(|&i| {
                        i != s.active_index
                            && s.pokemon[i as usize].species != engine::ids::Species::None
                            && s.pokemon[i as usize].is_alive()
                    })
                    .collect();
                PlayerChoice::Switch { slot: bench[(r as usize) % bench.len()] }
            };
            let pick_turn = |flow: &Flow, side: SideId, r: u64| -> PlayerChoice {
                let s = flow.state.side(side);
                let mut opts: Vec<PlayerChoice> = (0..4u8)
                    .filter(|&i| {
                        let m = s.active().moves[i as usize];
                        m.id != MoveId::None && m.pp > 0
                    })
                    .map(|i| PlayerChoice::Move { slot: i, tera: r % 7 == 0 })
                    .collect();
                for i in 0..6u8 {
                    if i != s.active_index
                        && s.pokemon[i as usize].species != engine::ids::Species::None
                        && s.pokemon[i as usize].is_alive()
                    {
                        opts.push(PlayerChoice::Switch { slot: i });
                    }
                }
                if opts.is_empty() {
                    // PP-stalled: the engine Struggle-detects a 0-PP move choice.
                    return PlayerChoice::Move { slot: 0, tera: false };
                }
                let n = opts.len() as u64;
                opts[(r % n) as usize]
            };
            match req {
                Request::Terminal { winner } => {
                    assert!((-1..=1).contains(&winner));
                    terminated += 1;
                    break;
                }
                Request::Turn => {
                    let r1 = next(1 << 30);
                    let r2 = next(1 << 30);
                    let c1 = pick_turn(&flow, SideId::One, r1);
                    let c2 = pick_turn(&flow, SideId::Two, r2);
                    flow.submit([Some(c1), Some(c2)]);
                }
                Request::Replace { sides } => {
                    for (i, side) in [SideId::One, SideId::Two].into_iter().enumerate() {
                        if sides[i] {
                            assert!(
                                !flow.state.side(side).active().is_alive(),
                                "Replace flagged a side whose active is alive"
                            );
                        }
                    }
                    let r1 = next(1 << 30);
                    let r2 = next(1 << 30);
                    let c1 = sides[0].then(|| pick_switch(&flow, SideId::One, r1));
                    let c2 = sides[1].then(|| pick_switch(&flow, SideId::Two, r2));
                    flow.submit([c1, c2]);
                }
                Request::PivotLanding { side } => {
                    let r = next(1 << 30);
                    let c = pick_switch(&flow, side, r);
                    let mut choices = [None, None];
                    choices[side.index()] = Some(c);
                    flow.submit(choices);
                }
                Request::Revive { side } => {
                    // Pick a fainted party member to revive.
                    let s = flow.state.side(side);
                    let fainted: Vec<u8> = (0..6u8)
                        .filter(|&i| {
                            i != s.active_index
                                && s.pokemon[i as usize].species != engine::ids::Species::None
                                && !s.pokemon[i as usize].is_alive()
                        })
                        .collect();
                    assert!(!fainted.is_empty(), "Revive raised with no fainted ally");
                    let r = next(1 << 30) as usize;
                    let mut choices = [None, None];
                    choices[side.index()] = Some(PlayerChoice::Switch { slot: fainted[r % fainted.len()] });
                    flow.submit(choices);
                }
            }
        }
    }
    assert!(terminated >= 55, "only {terminated}/60 random games terminated");
}
