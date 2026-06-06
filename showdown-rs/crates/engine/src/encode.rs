//! Flat observation encoding for the RL agent.
//!
//! A *read-time* projection of the battle state into a fixed-length `f32` vector — the same
//! "render a view of canonical state" pattern as [`crate::state::State::observe`] and
//! [`crate::narrate`]; it is never part of the transition. The agent sees the world only
//! through this vector.
//!
//! Layout (all values roughly in [0, 1], or small signed for boosts):
//!   for each side in [viewer, foe]:
//!       for each of 6 party slots: per-Pokémon block (PER_MON features)
//!   active boosts: 7 per side  (viewer, foe)
//!   global: weather one-hot (8) + terrain one-hot (5) + trick room (1)
//!           + per-side hazards/screens (8 each)
//!
//! The foe side is encoded from `state.observe(viewer)`, so anything the viewer hasn't seen is
//! already masked (today the encoded fields — HP/status/types/stats — are public anyway, but
//! routing through `observe` keeps the contract honest as the encoding grows).

use crate::ids::{BoostIndex, StatIndex, Status, Type};
use crate::state::{Pokemon, Side, SideId, State};

const STATUS_COUNT: usize = 7; // None,Burn,Paralysis,Sleep,Freeze,Poison,Toxic
const TYPE_SLOTS: usize = 18; // the 18 canonical types (None/Stellar excluded)
const WEATHER_COUNT: usize = 8;
const TERRAIN_COUNT: usize = 5;
const HAZARD_FEATS: usize = 8; // SR, spikes, tspikes, web, reflect, lscreen, aurora, tailwind

/// Per-Pokémon feature count: hp_frac, fainted, active, terastallized, status one-hot,
/// type multi-hot, 5 normalized stats, level.
const PER_MON: usize = 1 + 1 + 1 + 1 + STATUS_COUNT + TYPE_SLOTS + 5 + 1;

/// Length of the vector returned by [`encode`]. The model's input dimension.
pub const OBS_DIM: usize =
    2 * 6 * PER_MON                       // both teams, 6 mons each
    + 2 * BoostIndex::COUNT               // active boosts per side
    + WEATHER_COUNT + TERRAIN_COUNT + 1   // weather, terrain, trick room
    + 2 * HAZARD_FEATS;                   // hazards/screens per side

/// Encode the battle from `viewer`'s perspective into a length-[`OBS_DIM`] vector.
pub fn encode(state: &State, viewer: SideId) -> Vec<f32> {
    let observed = state.observe(viewer);
    let mut v = Vec::with_capacity(OBS_DIM);

    // Per-mon blocks: viewer's side first, then the foe's.
    for side_id in [viewer, viewer.other()] {
        let side = observed.side(side_id);
        for slot in 0..6 {
            encode_mon(&mut v, &side.pokemon[slot], slot as u8 == side.active_index);
        }
    }

    // Active boosts (-6..=6 -> [-1,1]).
    for side_id in [viewer, viewer.other()] {
        for b in observed.side(side_id).boosts {
            v.push(b as f32 / 6.0);
        }
    }

    // Global field state.
    one_hot(&mut v, observed.weather as usize, WEATHER_COUNT);
    one_hot(&mut v, observed.terrain as usize, TERRAIN_COUNT);
    v.push(observed.trick_room as u8 as f32);

    for side_id in [viewer, viewer.other()] {
        encode_hazards(&mut v, observed.side(side_id));
    }

    debug_assert_eq!(v.len(), OBS_DIM, "encode produced {} feats, expected {}", v.len(), OBS_DIM);
    v
}

fn encode_mon(v: &mut Vec<f32>, p: &Pokemon, is_active: bool) {
    let present = p.species != crate::ids::Species::None;
    let max_hp = p.max_hp.max(1) as f32;
    v.push(if present { (p.hp.max(0) as f32) / max_hp } else { 0.0 }); // hp fraction
    v.push((present && p.hp <= 0) as u8 as f32); // fainted
    v.push((present && is_active) as u8 as f32); // active
    v.push(p.terastallized as u8 as f32);

    // Status one-hot (index 0 = None).
    one_hot(v, status_index(p.status), STATUS_COUNT);

    // Type multi-hot over the 18 canonical types (Type::None = 0, Normal = 1, ...).
    let mut types = [0.0f32; TYPE_SLOTS];
    for t in p.types {
        if let Some(i) = type_index(t) {
            types[i] = 1.0;
        }
    }
    v.extend_from_slice(&types);

    // Normalized offensive/defensive stats (skip HP at index 0). ~500 is a generous cap.
    for s in 1..StatIndex::COUNT {
        v.push((p.stats[s] as f32 / 500.0).min(2.0));
    }
    v.push(p.level as f32 / 100.0);
}

fn encode_hazards(v: &mut Vec<f32>, side: &Side) {
    let sc = &side.side_conditions;
    v.push(sc.stealth_rock as u8 as f32);
    v.push(sc.spikes as f32 / 3.0);
    v.push(sc.toxic_spikes as f32 / 2.0);
    v.push(sc.sticky_web as u8 as f32);
    v.push((sc.reflect > 0) as u8 as f32);
    v.push((sc.light_screen > 0) as u8 as f32);
    v.push((sc.aurora_veil > 0) as u8 as f32);
    v.push((sc.tailwind > 0) as u8 as f32);
}

fn one_hot(v: &mut Vec<f32>, index: usize, len: usize) {
    for i in 0..len {
        v.push((i == index) as u8 as f32);
    }
}

fn status_index(s: Status) -> usize {
    s as usize // Status discriminants are 0..=6, matching STATUS_COUNT
}

/// Map a `Type` to a 0..18 slot (the 18 real types), or `None` for `Type::None`/`Stellar`.
fn type_index(t: Type) -> Option<usize> {
    let d = t as usize; // None=0, Normal=1, ..., Fairy=18, Stellar=19
    if (1..=TYPE_SLOTS).contains(&d) {
        Some(d - 1)
    } else {
        None
    }
}
