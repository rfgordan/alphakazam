//! The `PivotLanding with no live bench` tripwire, as a property.
//!
//! `Flow::run_turn` decides whether a self-switch move PAUSES for a landing by asking
//! `alive_bench(side).is_some()` at the TOP of the turn; `Flow::resume_pivot` then has to find a
//! landing target with the same predicate several state transitions later. A path that empties the
//! side's bench in between issues a `PivotLanding` request the player cannot legally answer — it
//! killed a 4096-env trainer at roughly 1e-5 games.
//!
//! The property under test is the one the driver actually needs: **whenever the pending request is
//! `PivotLanding { side }`, that side has an alive non-active party member.** Random play over
//! small, damage-heavy teams built around the mechanics that can move a side's active mid-turn
//! (pivot moves, drag moves, Revival Blessing, entry hazards) is the search.

use engine::ids::{MoveId, Species};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::{SideId, State};
use engine::team::{self, MemberSpec};

fn bench_alive(state: &State, side: SideId) -> Vec<u8> {
    let s = state.side(side);
    (0..6u8)
        .filter(|&i| {
            i != s.active_index
                && s.pokemon[i as usize].species != Species::None
                && s.pokemon[i as usize].is_alive()
        })
        .collect()
}

/// A pool built around everything that can move a side's active in the middle of a turn.
fn pool() -> Vec<MemberSpec> {
    use engine::ids::Nature::*;
    const N: [u8; 6] = [0; 6];
    let m = |species, moves: [&'static str; 4], item| MemberSpec {
        species,
        ability: "noability",
        item,
        tera: "normal",
        nature: Serious,
        evs: N,
        moves,
    };
    vec![
        // pivots
        m("dragapult", ["uturn", "dragondarts", "shadowball", "hex"], "lifeorb"),
        m("barraskewda", ["flipturn", "liquidation", "crunch", "psychicfangs"], "lifeorb"),
        m("cyclizar", ["shedtail", "uturn", "knockoff", "rapidspin"], "leftovers"),
        m("corviknight", ["uturn", "bravebird", "roost", "whirlwind"], "rockyhelmet"),
        m("rotomwash", ["voltswitch", "hydropump", "willowisp", "painsplit"], "lifeorb"),
        m("slowkinggalar", ["teleport", "sludgebomb", "icebeam", "chillyreception"], "leftovers"),
        m("incineroar", ["partingshot", "closecombat", "knockoff", "suckerpunch"], "lifeorb"),
        // draggers / hazards / revive
        m("dragonite", ["dragontail", "earthquake", "roost", "firepunch"], "lifeorb"),
        m("skarmory", ["whirlwind", "spikes", "stealthrock", "bodypress"], "leftovers"),
        m("garchomp", ["dragontail", "stealthrock", "earthquake", "spikes"], "rockyhelmet"),
        m("pawmot", ["revivalblessing", "closecombat", "thunderpunch", "uturn"], "lifeorb"),
        m("hydrapple", ["toxicspikes", "gigadrain", "dragonpulse", "recover"], "leftovers"),
        // pure attackers, to end games quickly
        m("kingambit", ["kowtowcleave", "suckerpunch", "ironhead", "swordsdance"], "lifeorb"),
        m("ironvaliant", ["closecombat", "moonblast", "knockoff", "thunderbolt"], "lifeorb"),
    ]
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        (z ^ (z >> 31)) % n.max(1)
    }
}

fn build_game(r: &mut Rng) -> State {
    let p = pool();
    let mut pick = |r: &mut Rng, n: usize| -> Vec<MemberSpec> {
        (0..n).map(|_| p[r.next(p.len() as u64) as usize]).collect()
    };
    // Short benches make the "bench emptied mid-turn" corner reachable in a handful of turns.
    let (n1, n2) = (2 + r.next(2) as usize, 2 + r.next(2) as usize);
    let (t1, t2) = (pick(r, n1), pick(r, n2));
    let mut s = team::build_state(&t1, &t2, 100);
    // Hazards on both sides: the mechanism that can KILL a mon the instant it becomes active.
    for i in 0..2usize {
        let sd = &mut s.sides[i];
        if r.next(2) == 0 {
            sd.side_conditions.stealth_rock = true;
        }
        sd.side_conditions.spikes = r.next(4) as u8;
        // Chip everyone: a one-hit board keeps whole games inside a few turns.
        for k in 0..6usize {
            if sd.pokemon[k].species != Species::None {
                let mx = sd.pokemon[k].max_hp;
                sd.pokemon[k].hp = 1 + (r.next(mx.max(2) as u64 / 3) as i16);
                // Near-empty PP: `no_usable_move` -> Struggle is one of the two substitutions that
                // can replace the move a `Pivot::Pause` was granted for, and the root the tripwire
                // was actually firing on. Without PP pressure the fuzz never reaches it.
                for slot in 0..4usize {
                    if sd.pokemon[k].moves[slot].id != MoveId::None {
                        sd.pokemon[k].moves[slot].pp = r.next(3) as u8;
                    }
                }
                // ...and a party member that is already down, so a bench can be all-fainted while
                // Revival Blessing is still a legal choice.
                if r.next(4) == 0 && k > 0 {
                    sd.pokemon[k].hp = 0;
                }
            }
        }
    }
    s
}

/// `PIVOT_FUZZ_GAMES` overrides the game budget (the hunt runs it at 1e6; CI runs the default).
#[test]
fn pivot_landing_always_has_a_live_bench() {
    let games: u64 = std::env::var("PIVOT_FUZZ_GAMES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20_000);
    let seed0: u64 = std::env::var("PIVOT_FUZZ_SEED").ok().and_then(|v| v.parse().ok()).unwrap_or(1);
    let mut violations = 0u64;

    for game in 0..games {
        let mut r = Rng(seed0.wrapping_mul(0x9E37_79B9).wrapping_add(game).wrapping_mul(6364136223846793005));
        let state = build_game(&mut r);
        let mut flow = Flow::new(state, game.wrapping_mul(2654435761).wrapping_add(7));

        for _step in 0..600 {
            let req = flow.request();
            if let Request::PivotLanding { side } = req {
                if bench_alive(&flow.state, side).is_empty() {
                    violations += 1;
                    eprintln!(
                        "VIOLATION game={game} turn={} side={side:?} active={} party={:?}",
                        flow.state.turn,
                        flow.state.side(side).active_index,
                        (0..6)
                            .map(|i| (
                                flow.state.side(side).pokemon[i].species,
                                flow.state.side(side).pokemon[i].hp
                            ))
                            .collect::<Vec<_>>()
                    );
                    break;
                }
            }
            let pick_switch = |flow: &Flow, side: SideId, k: u64| -> PlayerChoice {
                let b = bench_alive(&flow.state, side);
                PlayerChoice::Switch { slot: if b.is_empty() { 0 } else { b[(k as usize) % b.len()] } }
            };
            let pick_revive = |flow: &Flow, side: SideId, k: u64| -> PlayerChoice {
                let s = flow.state.side(side);
                let f: Vec<u8> = (0..6u8)
                    .filter(|&i| {
                        i != s.active_index
                            && s.pokemon[i as usize].species != Species::None
                            && !s.pokemon[i as usize].is_alive()
                    })
                    .collect();
                PlayerChoice::Switch { slot: if f.is_empty() { 0 } else { f[(k as usize) % f.len()] } }
            };
            let pick_turn = |flow: &Flow, side: SideId, k: u64| -> PlayerChoice {
                let s = flow.state.side(side);
                let mut opts: Vec<PlayerChoice> = (0..4u8)
                    .filter(|&i| {
                        let mv = s.active().moves[i as usize];
                        mv.id != MoveId::None && mv.pp > 0
                    })
                    .map(|i| PlayerChoice::Move { slot: i, tera: false })
                    .collect();
                for i in bench_alive(&flow.state, side) {
                    opts.push(PlayerChoice::Switch { slot: i });
                }
                if opts.is_empty() {
                    return PlayerChoice::Move { slot: 0, tera: false };
                }
                let n = opts.len() as u64;
                opts[(k % n) as usize]
            };
            match req {
                Request::Terminal { .. } => break,
                Request::Turn => {
                    let (a, b) = (r.next(1 << 30), r.next(1 << 30));
                    flow.submit([
                        Some(pick_turn(&flow, SideId::One, a)),
                        Some(pick_turn(&flow, SideId::Two, b)),
                    ]);
                }
                Request::Replace { sides } => {
                    let (a, b) = (r.next(1 << 30), r.next(1 << 30));
                    let c1 = sides[0].then(|| pick_switch(&flow, SideId::One, a));
                    let c2 = sides[1].then(|| pick_switch(&flow, SideId::Two, b));
                    flow.submit([c1, c2]);
                }
                Request::PivotLanding { side } => {
                    let k = r.next(1 << 30);
                    let c = pick_switch(&flow, side, k);
                    let mut ch = [None, None];
                    ch[side.index()] = Some(c);
                    flow.submit(ch);
                }
                Request::Revive { side } => {
                    let k = r.next(1 << 30);
                    let c = pick_revive(&flow, side, k);
                    let mut ch = [None, None];
                    ch[side.index()] = Some(c);
                    flow.submit(ch);
                }
            }
        }
    }
    assert_eq!(violations, 0, "PivotLanding issued with an empty bench in {violations} game(s)");
}
