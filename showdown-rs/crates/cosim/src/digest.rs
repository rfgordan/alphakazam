//! Canonical digest of an engine `State` — the slim seed-fixture's state oracle.
//!
//! A full v2 trace carries PS's complete `serializeBattle` after EVERY decision (~31 KB each,
//! ~85% of the trace). The seed gate only ever asks one question of that state: *does
//! `diff_states(engine, convert(ps))` come back empty?* So an extension corpus can ship a
//! 128-bit digest per decision instead of the state, provided the digest answers exactly that
//! question.
//!
//! ## The canonical encoding
//!
//! `state_digest` walks the SAME manifest `diff::diff_states` compares, in a fixed order, and
//! feeds it to FNV-1a/128. Where `diff_states` gates a comparison on a *joint* predicate over
//! both states, the encoder uses the corresponding *self* predicate — which is equivalent
//! because whenever the two states disagree on the predicate itself, some field the encoder
//! DOES cover already differs:
//!
//! | `diff_states` gate | encoder gate | why equivalent |
//! |---|---|---|
//! | `weather_turns` iff weathers equal & non-None | iff own weather non-None | weathers differ ⇒ the weather tag already differs |
//! | active block iff both actives alive & same index | iff own active alive | a fainted/alive disagreement is an `hp` difference; an index disagreement is an `active_index` difference |
//! | `status_counter` iff statuses equal & Sleep/Toxic | iff own status Sleep/Toxic | statuses differ ⇒ the status tag already differs |
//! | `move{i}.pp` iff move ids equal | always | ids differ ⇒ the id already differs |
//! | `partial_trap_*` iff both PartiallyTrapped | iff own PartiallyTrapped | the volatile bitset is itself encoded |
//! | per-mon tail skipped iff either side fainted | skipped iff own hp ≤ 0 | an hp disagreement is already encoded |
//!
//! The one place `diff_states` is *asymmetrically* lenient is the terminal sentinel: a finished
//! battle can leave a side with no serialized active (PS drops `isActive`), so `convert` yields
//! `active_index == u8::MAX` and `diff_states` skips BOTH the index compare and the whole active
//! block for that side. Which side that happens to depends on how the battle ended — information
//! the engine's own carried-forward state does not have. So the mask is DATA, not a guess: the
//! fixture stores PS's `active_index == u8::MAX` bits (`noActive`, false almost everywhere) next
//! to the digest, and the gate digests the engine state under the same mask. That reproduces
//! `diff_states`' leniency exactly — no more, no less. Guessing it instead (e.g. "a side with no
//! living Pokemon means the battle is over, mask both") silently swallows real terminal-state
//! divergences: it turned 9 games with genuine `active_turns` / `encore` / `volatiles` /
//! `pending_move` diffs — one of them a `rust-extra shuffle@disablemove` OVER-EMISSION — green.
//!
//! Consequence: `state_digest(a) == state_digest(b)` ⟺ `diff_states(a, b).is_empty()`, up to a
//! 2⁻¹²⁸ collision. Certified empirically: the slim gate reproduces the full gate's exact-game
//! SET on the whole 111-trace corpus (`FIXTURE_SELFTEST=1`).

use engine::ids::Status;
use engine::state::{PendingMove, Side, State};
use engine::volatile::VolatileStatus;

/// FNV-1a/128 over the canonical encoding.
pub struct Digester {
    h: u128,
}

const FNV_OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
const FNV_PRIME: u128 = 0x0000000001000000000000000000013b;

impl Digester {
    pub fn new() -> Self {
        Digester { h: FNV_OFFSET }
    }
    #[inline]
    fn u8(&mut self, b: u8) {
        self.h ^= b as u128;
        self.h = self.h.wrapping_mul(FNV_PRIME);
    }
    #[inline]
    fn u16(&mut self, v: u16) {
        self.u8(v as u8);
        self.u8((v >> 8) as u8);
    }
    #[inline]
    fn i16(&mut self, v: i16) {
        self.u16(v as u16);
    }
    #[inline]
    fn i8(&mut self, v: i8) {
        self.u8(v as u8);
    }
    #[inline]
    fn b(&mut self, v: bool) {
        self.u8(v as u8);
    }
    #[inline]
    fn u64(&mut self, v: u64) {
        for i in 0..8 {
            self.u8((v >> (8 * i)) as u8);
        }
    }
    pub fn finish(self) -> u128 {
        self.h
    }
}

impl Default for Digester {
    fn default() -> Self {
        Self::new()
    }
}

/// `u8::MAX` in the encoding: "this side has no comparable active slot" (terminal state).
const NO_ACTIVE: u8 = u8::MAX;

/// PS's per-side "no comparable active slot" bits, read off a CONVERTED PS state. Stored in the
/// fixture and replayed onto the engine state at gate time (see the module doc).
pub fn ps_active_mask(ps: &State) -> [bool; 2] {
    [ps.sides[0].active_index == NO_ACTIVE, ps.sides[1].active_index == NO_ACTIVE]
}

pub fn state_digest(s: &State) -> u128 {
    state_digest_masked(s, [false, false])
}

pub fn state_digest_masked(s: &State, no_active: [bool; 2]) -> u128 {
    let mut d = Digester::new();
    d.u8(s.weather as u8);
    if s.weather != engine::ids::Weather::None {
        d.i8(s.weather_turns);
    }
    d.u8(s.terrain as u8);
    if s.terrain != engine::ids::Terrain::None {
        d.i8(s.terrain_turns);
    }
    d.b(s.trick_room);
    for (si, side) in s.sides.iter().enumerate() {
        digest_side(&mut d, side, no_active[si]);
    }
    d.finish()
}

fn digest_side(d: &mut Digester, a: &Side, masked: bool) {
    let ai = if masked || a.active_index == NO_ACTIVE { NO_ACTIVE } else { a.active_index };
    d.u8(ai);

    if ai != NO_ACTIVE && a.pokemon[ai as usize].is_alive() {
        for bi in 0..7 {
            d.i8(a.boosts[bi]);
        }
        let mut v = a.volatiles;
        for single in [
            VolatileStatus::Flinch,
            VolatileStatus::Protect,
            VolatileStatus::Endure,
            VolatileStatus::Roosted,
            VolatileStatus::Roost,
        ] {
            v.remove(single);
        }
        d.u64(v.0);
        d.i16(a.substitute_hp);
        if a.volatiles.contains(VolatileStatus::PartiallyTrapped) {
            d.u8(a.partial_trap_turns);
            d.u8(a.partial_trap_div);
        }
        d.u8(a.taunt_turns);
        d.u8(a.confusion_turns);
        d.u8(a.perish_turns);
        d.u8(a.yawn_turns);
        d.u16(a.encore.0 .0);
        d.u8(a.encore.1);
        d.u16(a.disable.0 .0);
        d.u8(a.disable.1);
        d.u8(a.stall_counter);
        d.u8(a.active_turns);
        match a.pending_move {
            PendingMove::None => d.u8(0),
            PendingMove::Charging(m) => {
                d.u8(1);
                d.u16(m.0);
            }
            PendingMove::Rampaging(m, t) => {
                d.u8(2);
                d.u16(m.0);
                d.u8(t);
            }
            PendingMove::Recharging => d.u8(3),
        }
    }

    let c = &a.side_conditions;
    d.b(c.stealth_rock);
    d.u8(c.spikes);
    d.u8(c.toxic_spikes);
    d.b(c.sticky_web);
    d.u8(c.reflect);
    d.u8(c.light_screen);
    d.u8(c.aurora_veil);
    d.u8(c.tailwind);
    d.b(a.tera_used);
    d.u8(a.wish.0);
    d.i16(a.wish.1);
    d.u8(a.future_sight.0);
    d.u8(a.future_sight.1);

    for p in &a.pokemon {
        d.u16(p.species.0);
        d.i16(p.hp);
        if p.hp <= 0 {
            continue; // fainted: PS scrubs the rest
        }
        d.u8(p.status as u8);
        if matches!(p.status, Status::Sleep | Status::Toxic) {
            d.u8(p.status_counter);
        }
        d.u16(p.item as u16);
        d.u16(p.ability as u16);
        d.u8(p.types[0] as u8);
        d.u8(p.types[1] as u8);
        d.b(p.terastallized);
        d.u8(p.tera_type as u8);
        d.u8(p.times_hit);
        d.b(p.ability_used);
        d.b(p.transformed);
        d.u16(p.last_berry as u16);
        for mi in 0..4 {
            d.u16(p.moves[mi].id.0);
            d.u8(p.moves[mi].pp);
        }
    }
}

pub fn hex(d: u128) -> String {
    format!("{d:032x}")
}

pub fn parse_hex(s: &str) -> Option<u128> {
    u128::from_str_radix(s, 16).ok()
}
