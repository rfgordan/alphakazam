//! Exact comparison of two engine `State`s over the modeled manifest.
//!
//! Unlike the old `relaxed_eq` (hp/status/3-of-8 hazards), this compares *everything the
//! engine models*: items, PP, volatiles, every side condition with durations, counters,
//! pending moves, wish/future-sight, weather/terrain/TR durations. Each difference is
//! reported as (category, detail) so the aggregate report can rank divergence causes.
//!
//! Normalizations (documented, intentional):
//!  - fainted mons compare hp only (PS scrubs status to "fnt" and leaves stale fields),
//!  - single-turn flags (Flinch/Protect/Endure/Roosted) are cleared on both sides — they
//!    cannot survive to a decision boundary; any real effect shows up in other fields,
//!  - `turn` and `last_used_move`/`move_streak` are not compared (engine-internal conventions;
//!    their *consequences* are compared).

use engine::ids::Status;
use engine::state::{Side, State};
use engine::volatile::{VolatileStatus, Volatiles};

pub struct Diff {
    pub category: String,
    pub detail: String,
}

fn d(out: &mut Vec<Diff>, category: impl Into<String>, detail: String) {
    out.push(Diff { category: category.into(), detail });
}

macro_rules! cmp {
    ($out:expr, $cat:expr, $a:expr, $b:expr) => {
        if $a != $b {
            d($out, $cat, format!("{}: engine={:?} ps={:?}", $cat, $a, $b));
        }
    };
}

/// Volatile bits that exist only within a turn and cannot meaningfully persist to a boundary.
fn normalized_volatiles(v: Volatiles) -> Volatiles {
    let mut v = v;
    for single in [
        VolatileStatus::Flinch,
        VolatileStatus::Protect,
        VolatileStatus::Endure,
        VolatileStatus::Roosted,
        VolatileStatus::Roost,
    ] {
        v.remove(single);
    }
    v
}

pub fn diff_states(engine_state: &State, ps: &State) -> Vec<Diff> {
    let mut out = Vec::new();

    cmp!(&mut out, "field.weather", engine_state.weather, ps.weather);
    if engine_state.weather == ps.weather && engine_state.weather != engine::ids::Weather::None {
        cmp!(&mut out, "field.weather_turns", engine_state.weather_turns, ps.weather_turns);
    }
    cmp!(&mut out, "field.terrain", engine_state.terrain, ps.terrain);
    if engine_state.terrain == ps.terrain && engine_state.terrain != engine::ids::Terrain::None {
        cmp!(&mut out, "field.terrain_turns", engine_state.terrain_turns, ps.terrain_turns);
    }
    cmp!(&mut out, "field.trick_room", engine_state.trick_room, ps.trick_room);

    for (si, (a, b)) in engine_state.sides.iter().zip(&ps.sides).enumerate() {
        diff_side(&mut out, si, a, b);
    }
    out
}

fn diff_side(out: &mut Vec<Diff>, si: usize, a: &Side, b: &Side) {
    let s = |field: &str| format!("s{si}.{field}");

    // u8::MAX = terminal sentinel: a finished battle drops the loser's fainted active in PS,
    // so there is no active slot to compare.
    if b.active_index != u8::MAX {
        cmp!(out, s("active_index"), a.active_index, b.active_index);
    }

    let active_alive = a.active_index != u8::MAX
        && b.active_index != u8::MAX
        && a.pokemon[a.active_index as usize].is_alive()
        && b.pokemon[b.active_index as usize].is_alive();
    if active_alive && a.active_index == b.active_index {
        for (bi, name) in ["atk", "def", "spa", "spd", "spe", "acc", "eva"].iter().enumerate() {
            cmp!(out, format!("s{si}.boost.{name}"), a.boosts[bi], b.boosts[bi]);
        }
        cmp!(out, s("volatiles"), normalized_volatiles(a.volatiles), normalized_volatiles(b.volatiles));
        cmp!(out, s("substitute_hp"), a.substitute_hp, b.substitute_hp);
        // Partial-trap countdown + Binding Band divisor (pairs with the PartiallyTrapped bit).
        if a.volatiles.contains(VolatileStatus::PartiallyTrapped)
            && b.volatiles.contains(VolatileStatus::PartiallyTrapped)
        {
            cmp!(out, s("partial_trap_turns"), a.partial_trap_turns, b.partial_trap_turns);
            cmp!(out, s("partial_trap_div"), a.partial_trap_div, b.partial_trap_div);
        }
        cmp!(out, s("taunt_turns"), a.taunt_turns, b.taunt_turns);
        cmp!(out, s("confusion_turns"), a.confusion_turns, b.confusion_turns);
        cmp!(out, s("perish_turns"), a.perish_turns, b.perish_turns);
        cmp!(out, s("yawn_turns"), a.yawn_turns, b.yawn_turns);
        cmp!(out, s("encore"), a.encore, b.encore);
        cmp!(out, s("disable"), a.disable, b.disable);
        cmp!(out, s("stall_counter"), a.stall_counter, b.stall_counter);
        cmp!(out, s("active_turns"), a.active_turns, b.active_turns);
        cmp!(out, s("pending_move"), a.pending_move, b.pending_move);
    }

    let (ca, cb) = (&a.side_conditions, &b.side_conditions);
    cmp!(out, s("sc.stealth_rock"), ca.stealth_rock, cb.stealth_rock);
    cmp!(out, s("sc.spikes"), ca.spikes, cb.spikes);
    cmp!(out, s("sc.toxic_spikes"), ca.toxic_spikes, cb.toxic_spikes);
    cmp!(out, s("sc.sticky_web"), ca.sticky_web, cb.sticky_web);
    cmp!(out, s("sc.reflect"), ca.reflect, cb.reflect);
    cmp!(out, s("sc.light_screen"), ca.light_screen, cb.light_screen);
    cmp!(out, s("sc.aurora_veil"), ca.aurora_veil, cb.aurora_veil);
    cmp!(out, s("sc.tailwind"), ca.tailwind, cb.tailwind);
    cmp!(out, s("tera_used"), a.tera_used, b.tera_used);
    cmp!(out, s("wish"), a.wish, b.wish);
    cmp!(out, s("future_sight"), a.future_sight, b.future_sight);

    for (pi, (pa, pb)) in a.pokemon.iter().zip(&b.pokemon).enumerate() {
        let p = |field: &str| format!("s{si}#{pi}.{field}");
        cmp!(out, p("species"), pa.species, pb.species);
        cmp!(out, p("hp"), pa.hp, pb.hp);
        if pa.hp <= 0 || pb.hp <= 0 {
            continue; // fainted: PS scrubs the rest
        }
        cmp!(out, p("status"), pa.status, pb.status);
        if pa.status == pb.status && matches!(pa.status, Status::Sleep | Status::Toxic) {
            cmp!(out, p("status_counter"), pa.status_counter, pb.status_counter);
        }
        cmp!(out, p("item"), pa.item, pb.item);
        cmp!(out, p("ability"), pa.ability, pb.ability);
        // PS's `pokemon.types` ARRAY (`live_types`), matching `digest.rs`. The engine's resolved
        // `types` adds nothing — it is that array plus `terastallized` / `tera_type` (compared
        // below) and the `Roosted` marker, which is normalized away here like the other
        // single-turn flags.
        cmp!(out, p("types"), pa.live_types, pb.live_types);
        cmp!(out, p("terastallized"), pa.terastallized, pb.terastallized);
        cmp!(out, p("tera_type"), pa.tera_type, pb.tera_type);
        cmp!(out, p("times_hit"), pa.times_hit, pb.times_hit);
        cmp!(out, p("ability_used"), pa.ability_used, pb.ability_used);
        cmp!(out, p("transformed"), pa.transformed, pb.transformed);
        cmp!(out, p("last_berry"), pa.last_berry, pb.last_berry);
        for mi in 0..4 {
            let (ma, mb) = (pa.moves[mi], pb.moves[mi]);
            cmp!(out, p(&format!("move{mi}.id")), ma.id, mb.id);
            if ma.id == mb.id {
                cmp!(out, p(&format!("move{mi}.pp")), ma.pp, mb.pp);
            }
        }
    }
}
