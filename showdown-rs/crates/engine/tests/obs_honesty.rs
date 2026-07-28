//! Scramble-invariance: the honest observation must be a function of the viewer's INFORMATION
//! SET only. We take real battle states, scramble everything the viewer cannot know about the
//! foe — unrevealed items/abilities/tera types/move slots, hidden spreads (EVs/nature AND the
//! computed stats they produce), sleep counters, substitute HP, and whole never-seen party
//! members — and assert the encoding (v1 and v2) is bit-identical. Any future feature that
//! reads hidden state fails this test the day it is written.

use engine::ids::{Ability, Item, MoveId, Nature, Species, StatIndex, Status, Type};
use engine::request::{Flow, PlayerChoice, Request};
use engine::state::{MoveSlot, Reveal, SideId, State};

fn splitmix(z: &mut u64) -> u64 {
    *z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = *z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Overwrite everything the viewer can't see about the foe with junk derived from `z`.
fn scramble_hidden(state: &mut State, viewer: SideId, z: &mut u64) {
    let foe = state.side_mut(viewer.other());
    for slot in 0..6 {
        let p = &mut foe.pokemon[slot];
        if p.species == Species::None {
            continue;
        }
        let r = p.reveal;
        if !r.has(Reveal::SPECIES) {
            // Never seen: species identity and everything with it is unknowable.
            p.species = Species((splitmix(z) % 800 + 1) as u16);
            p.level = (splitmix(z) % 50 + 50) as u8;
            p.types = [Type::Fire, Type::None];
            p.stats = [(splitmix(z) % 300) as i16 + 50; StatIndex::COUNT];
            p.base_stats = p.stats;
            p.hp = 77;
            p.max_hp = 154;
        }
        if !r.has(Reveal::ITEM) {
            p.item = if splitmix(z) % 2 == 0 { Item::Leftovers } else { Item::ChoiceScarf };
        }
        if !r.has(Reveal::ABILITY) {
            p.ability = Ability::Levitate;
            p.base_ability = Ability::Levitate;
        }
        if !r.has(Reveal::TERA) {
            p.tera_type = Type::Dragon;
        }
        for m in 0..4u8 {
            if !r.move_seen(m) {
                p.moves[m as usize] = MoveSlot {
                    id: MoveId((splitmix(z) % 700 + 1) as u16),
                    pp: (splitmix(z) % 30) as u8 + 1,
                    max_pp: 32,
                    disabled: false,
                };
            }
        }
        // Hidden spread: junk EVs/nature and a consistently-junk computed-stats vector (keep
        // fractions: hp/max_hp scale together so the PUBLIC fraction is preserved).
        p.evs = [(splitmix(z) % 252) as u8; StatIndex::COUNT];
        p.nature = Nature::Adamant;
        if r.has(Reveal::SPECIES) {
            for s in 1..StatIndex::COUNT {
                p.stats[s] = p.stats[s].saturating_add(13);
            }
            // Double both so the PUBLIC hp fraction is preserved exactly while the raw values
            // (hidden) change — the honest encoder must re-derive from base stats + fraction.
            if p.max_hp > 0 && p.max_hp < 8000 {
                p.hp = p.hp.max(0) * 2;
                p.max_hp *= 2;
            }
        }
        if p.status == Status::Sleep {
            p.status_counter = (splitmix(z) % 3) as u8 + 1;
        }
    }
    if foe.substitute_hp > 0 {
        foe.substitute_hp = (splitmix(z) % 90) as i16 + 1;
    }
}

#[test]
fn honest_obs_is_scramble_invariant() {
    // Drive real battles from the default matchup with pseudo-random legal actions, checking
    // the invariance at every decision point, both viewers, both encoders.
    let mut checked = 0usize;
    for seed in 0..8u64 {
        let mut st = engine::team::default_matchup();
        st.fog_species = true;
        st.reveal_leads();
        let mut flow = Flow::new(st, seed.wrapping_mul(7919).wrapping_add(3));
        let mut z = seed ^ 0xDEAD_BEEF;
        for _step in 0..120 {
            if matches!(flow.request(), Request::Terminal { .. }) {
                break;
            }
            for viewer in [SideId::One, SideId::Two] {
                let clean_v1 = engine::encode::encode(&flow.state, viewer);
                let clean_v2 = engine::encode::encode_v2(&flow.state, viewer);
                let clean_ids = engine::encode::encode_ids(&flow.state, viewer);
                let mut scrambled = flow.state;
                scramble_hidden(&mut scrambled, viewer, &mut z);
                assert_eq!(clean_v1, engine::encode::encode(&scrambled, viewer),
                           "v1 obs read hidden foe state (seed {seed}, step {_step})");
                assert_eq!(clean_v2, engine::encode::encode_v2(&scrambled, viewer),
                           "v2 obs read hidden foe state (seed {seed}, step {_step})");
                assert_eq!(clean_ids, engine::encode::encode_ids(&scrambled, viewer),
                           "obs ids read hidden foe state (seed {seed}, step {_step})");
                checked += 1;
            }
            // Advance with a pseudo-random legal-ish choice per acting side.
            let pick = |st: &State, side: SideId, z: &mut u64| -> PlayerChoice {
                let s = st.side(side);
                if splitmix(z) % 3 == 0 {
                    for i in 0..6u8 {
                        if i != s.active_index && s.pokemon[i as usize].species != Species::None
                            && s.pokemon[i as usize].is_alive() {
                            return PlayerChoice::Switch { slot: i };
                        }
                    }
                }
                let mv = (splitmix(z) % 4) as u8;
                let ok = s.active().moves[mv as usize].id != MoveId::None
                    && s.active().moves[mv as usize].pp > 0;
                PlayerChoice::Move { slot: if ok { mv } else { 0 }, tera: false }
            };
            let req = flow.request();
            let c = |side: SideId, flow: &Flow, z: &mut u64| match req {
                Request::Turn => Some(pick(&flow.state, side, z)),
                Request::Replace { sides } if sides[side.index()] => {
                    Some(pick(&flow.state, side, z))
                }
                Request::PivotLanding { side: s2 } | Request::Revive { side: s2 } if s2 == side => {
                    Some(pick(&flow.state, side, z))
                }
                _ => None,
            };
            let c0 = c(SideId::One, &flow, &mut z);
            let c1 = c(SideId::Two, &flow, &mut z);
            flow.submit([c0, c1]);
        }
    }
    assert!(checked > 300, "too few states checked ({checked})");
}
