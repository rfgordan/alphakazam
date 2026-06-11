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

use crate::data::move_data;
use crate::damage::type_multiplier;
use crate::ids::{BoostIndex, MoveCategory, MoveId, Species, StatIndex, Status, Type};
use crate::state::{PendingMove, Pokemon, Side, SideId, State};
use crate::volatile::VolatileStatus;

const STATUS_COUNT: usize = 7; // None,Burn,Paralysis,Sleep,Freeze,Poison,Toxic
const TYPE_SLOTS: usize = 18; // the 18 canonical types (None/Stellar excluded)
const WEATHER_COUNT: usize = 8;
const TERRAIN_COUNT: usize = 5;
const HAZARD_FEATS: usize = 8; // SR, spikes, tspikes, web, reflect, lscreen, aurora, tailwind
const STAT_SCALE: f32 = 600.0; // normalizer for computed stats

/// All volatile statuses, encoded as a binary flag each in the active mon's field block.
const VOLATILES: [VolatileStatus; 29] = {
    use VolatileStatus::*;
    [
        Confusion, Substitute, LeechSeed, Taunt, Encore, Disable, Protect, Endure, Flinch, Roost,
        Charge, Yawn, PerishSong, DestinyBond, Curse, Nightmare, Attract, Torment, SaltCure,
        GlaiveRush, LockedMove, MustRecharge, PartiallyTrapped, Roosted, ChoiceLock, Protosynthesis,
        QuarkDrive, FocusEnergy, Unburden,
    ]
};

/// Per-Pokémon feature count: hp_frac, fainted, active, terastallized, status one-hot,
/// type multi-hot, 5 normalized stats, level.
const PER_MON: usize = 1 + 1 + 1 + 1 + STATUS_COUNT + TYPE_SLOTS + 5 + 1;

/// Active-mon dynamic state ("field block"): substitute HP, volatile flags, status counter,
/// 9 multi-turn counters, pending-move one-hot (4), can-Tera flag.
const FIELD_BLOCK: usize = 1 + VOLATILES.len() + 1 + 9 + 4 + 1;
/// Per active move: present, pp frac, disabled, type-eff vs foe, STAB, base power, category (3),
/// accuracy.
const MOVE_FEATS: usize = 1 + 1 + 1 + 1 + 1 + 1 + 3 + 1;
const ACTIVE_MOVE_BLOCK: usize = 4 * MOVE_FEATS;
/// Matchup: I'm-faster flag + both effective speeds.
const MATCHUP: usize = 3;

/// Length of the vector returned by [`encode`]. The model's input dimension.
pub const OBS_DIM: usize =
    2 * 6 * PER_MON                       // both teams, 6 mons each (roster block)
    + 2 * BoostIndex::COUNT               // active boosts per side
    + WEATHER_COUNT + TERRAIN_COUNT + 1   // weather, terrain, trick room
    + 2 * HAZARD_FEATS                    // hazards/screens per side
    + 2 * FIELD_BLOCK                     // active-mon dynamic state per side
    + 2 * ACTIVE_MOVE_BLOCK              // active mon's 4 moves per side
    + MATCHUP;

// --- categorical IDs for embedding (Level-1 entity features) -------------------------------

/// Categorical IDs per entity: species, ability, item, tera type, move×4, last-used move.
pub const IDS_PER_MON: usize = 9;
/// Entities embedded: 12 roster (both teams' 6) + 2 *active duplicates* (viewer's & foe's active,
/// piped into a fixed input slot so the model can track the current mon directly).
pub const N_MONS: usize = 14;
/// Length of the integer-ID vector from [`encode_ids`].
pub const ID_DIM: usize = N_MONS * IDS_PER_MON;

fn push_mon_ids(v: &mut Vec<i64>, p: &Pokemon, last_move: MoveId) {
    v.push(p.species.0 as i64);
    v.push(p.ability as i64);
    v.push(p.item as i64);
    v.push(p.tera_type as i64);
    for m in 0..4 {
        v.push(p.moves[m].id.0 as i64);
    }
    v.push(last_move.0 as i64); // 9th column: last move (None for benched mons)
}

/// Per-entity integer IDs for the model's embedding tables: 12 roster (viewer's 6, foe's 6) then
/// the two active duplicates (viewer's active, foe's active) at fixed positions 12/13. Read from
/// the fog-of-war `observe(viewer)` state, so hidden info comes back as `Unknown`/`None`.
pub fn encode_ids(state: &State, viewer: SideId) -> Vec<i64> {
    let observed = state.observe(viewer);
    let mut v = Vec::with_capacity(ID_DIM);
    for side_id in [viewer, viewer.other()] {
        let side = observed.side(side_id);
        for slot in 0..6 {
            let last = if slot as u8 == side.active_index { side.last_used_move } else { MoveId::None };
            push_mon_ids(&mut v, &side.pokemon[slot], last);
        }
    }
    for side_id in [viewer, viewer.other()] {
        let side = observed.side(side_id);
        push_mon_ids(&mut v, side.active(), side.last_used_move);
    }
    debug_assert_eq!(v.len(), ID_DIM);
    v
}

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

    // Active-mon dynamic state, then its 4 moves (vs the opposing active), then the speed matchup.
    for side_id in [viewer, viewer.other()] {
        encode_field(&mut v, observed.side(side_id));
    }
    for side_id in [viewer, viewer.other()] {
        let foe_active = observed.side(side_id.other()).active();
        encode_active_moves(&mut v, observed.side(side_id), foe_active);
    }
    encode_matchup(&mut v, &observed, viewer);

    debug_assert_eq!(v.len(), OBS_DIM, "encode produced {} feats, expected {}", v.len(), OBS_DIM);
    v
}

/// Active mon's dynamic ("field") state: substitute, volatiles, status counter, multi-turn
/// counters, pending move, can-Tera. Public for the foe too — these are announced in the log.
fn encode_field(v: &mut Vec<f32>, side: &Side) {
    let p = side.active();
    let present = p.species != Species::None;
    let max_hp = p.max_hp.max(1) as f32;

    v.push(if present { (side.substitute_hp.max(0) as f32) / max_hp } else { 0.0 });
    for vol in VOLATILES {
        v.push(side.volatiles.contains(vol) as u8 as f32);
    }
    v.push((p.status_counter as f32 / 16.0).min(1.0)); // toxic stage / sleep turns
    v.push((side.taunt_turns as f32 / 5.0).min(1.0));
    v.push((side.confusion_turns as f32 / 5.0).min(1.0));
    v.push((side.perish_turns as f32 / 4.0).min(1.0));
    v.push((side.yawn_turns as f32 / 2.0).min(1.0));
    v.push((side.encore.1 as f32 / 8.0).min(1.0));
    v.push((side.disable.1 as f32 / 8.0).min(1.0));
    v.push((side.active_turns as f32 / 6.0).min(1.0));
    v.push((side.stall_counter as f32 / 5.0).min(1.0));
    v.push((side.move_streak as f32 / 5.0).min(1.0));
    let pend = match side.pending_move {
        PendingMove::None => 0,
        PendingMove::Charging(_) => 1,
        PendingMove::Rampaging(..) => 2,
        PendingMove::Recharging => 3,
    };
    one_hot(v, pend, 4);
    let side_terad = side.tera_used || side.pokemon.iter().any(|m| m.terastallized);
    v.push((present && !side_terad && !p.terastallized) as u8 as f32); // can still Terastallize
}

/// The active mon's four moves, scored against the opposing active (type-effectiveness, STAB, ...).
fn encode_active_moves(v: &mut Vec<f32>, side: &Side, foe_active: &Pokemon) {
    let p = side.active();
    for m in 0..4 {
        let mv = p.moves[m];
        let present = mv.id != MoveId::None;
        let md = move_data(mv.id);
        v.push(present as u8 as f32);
        v.push(if present && mv.max_pp > 0 { (mv.pp as f32) / (mv.max_pp as f32) } else { 0.0 });
        v.push(mv.disabled as u8 as f32);
        let te = if present { type_multiplier(md.typ, foe_active.types) } else { 0.0 };
        v.push((te / 4.0).min(1.0)); // 0, 0.25, 0.5, 1, 2, 4 -> /4
        let stab = present && (p.types[0] == md.typ || p.types[1] == md.typ);
        v.push(stab as u8 as f32);
        v.push((md.base_power as f32 / 200.0).min(1.0));
        one_hot(v, if present { md.category as usize } else { MoveCategory::Status as usize }, 3);
        v.push(if !present { 0.0 } else if md.accuracy == 0 { 1.0 } else { md.accuracy as f32 / 100.0 });
    }
}

/// Speed matchup: who moves first (respecting Trick Room) + both normalized effective speeds.
fn encode_matchup(v: &mut Vec<f32>, state: &State, viewer: SideId) {
    let my = crate::generate::effective_speed(state, viewer);
    let foe = crate::generate::effective_speed(state, viewer.other());
    let i_faster = if state.trick_room { my < foe } else { my > foe };
    v.push(i_faster as u8 as f32);
    v.push((my as f32 / STAT_SCALE).min(1.5));
    v.push((foe as f32 / STAT_SCALE).min(1.5));
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

    // Normalized offensive/defensive stats (skip HP at index 0).
    for s in 1..StatIndex::COUNT {
        v.push((p.stats[s] as f32 / STAT_SCALE).min(2.0));
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
