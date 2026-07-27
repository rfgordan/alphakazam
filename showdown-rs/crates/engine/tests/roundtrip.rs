//! Property test for the apply/reverse engine: for any well-formed instruction list,
//! `apply` followed by `reverse` must restore the state byte-for-byte.
//!
//! "Well-formed" means each instruction's `previous`/`amount` fields are consistent
//! with the state at the moment it is applied — the same contract the generator
//! upholds. We build lists with a small deterministic LCG (no external rng dep) so the
//! test is reproducible.

use engine::ids::{BoostIndex, Species, Status, Type, Weather};
use engine::instruction::{Instruction, SideConditionId};
use engine::state::{Pokemon, SideId, State};
use engine::volatile::VolatileStatus;

/// A tiny deterministic PRNG just for generating test sequences.
struct Lcg(u64);
impl Lcg {
    fn next_u32(&mut self) -> u32 {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    fn below(&mut self, n: u32) -> u32 {
        self.next_u32() % n
    }
}

fn sample_state() -> State {
    let mut s = State::EMPTY;
    for side in 0..2 {
        for slot in 0..6 {
            let p = &mut s.sides[side].pokemon[slot];
            *p = Pokemon::EMPTY;
            p.species = Species::from_id("greattusk").unwrap();
            p.level = 100;
            p.types = [Type::Ground, Type::Fighting];
            p.base_types = p.types;
            p.max_hp = 300 + (slot as i16) * 7;
            p.hp = p.max_hp;
            p.stats = [0, 200, 150, 100, 120, 180];
            p.moves[0] = engine::state::MoveSlot { id: engine::ids::MoveId::from_id("earthquake").unwrap(), pp: 16, max_pp: 16, disabled: false };
        }
    }
    s.sides[0].active_index = 0;
    s.sides[1].active_index = 0;
    s
}

/// Generate one instruction that is well-formed for `state` (reads current values into
/// the `previous` fields), without mutating `state`.
fn gen_instruction(state: &State, rng: &mut Lcg) -> Instruction {
    let side = if rng.below(2) == 0 { SideId::One } else { SideId::Two };
    let s = state.side(side);
    let slot = s.active_index;
    let active = s.active();
    match rng.below(13) {
        12 => Instruction::ClearBoosts { side, previous: s.boosts },
        0 => {
            // Damage that won't underflow current HP.
            let max = active.hp.max(1) as u32;
            let amount = (rng.below(max) + 1) as i16;
            Instruction::Damage { side, slot, amount }
        }
        1 => {
            // Heal that won't overflow max HP.
            let room = (active.max_hp - active.hp).max(0) as u32;
            let amount = if room == 0 { 0 } else { rng.below(room) as i16 };
            Instruction::Heal { side, slot, amount }
        }
        2 => {
            let stat = match rng.below(BoostIndex::COUNT as u32) {
                0 => BoostIndex::Attack,
                1 => BoostIndex::Defense,
                2 => BoostIndex::SpecialAttack,
                3 => BoostIndex::SpecialDefense,
                4 => BoostIndex::Speed,
                5 => BoostIndex::Accuracy,
                _ => BoostIndex::Evasion,
            };
            // Effective (clamped to -6..=6) delta, matching the generator contract.
            let current = s.boost(stat);
            let target = (rng.below(13) as i8) - 6;
            let amount = target - current;
            Instruction::Boost { side, stat, amount }
        }
        3 => {
            let new = match rng.below(4) {
                0 => Status::Burn,
                1 => Status::Paralysis,
                2 => Status::Poison,
                _ => Status::None,
            };
            Instruction::ChangeStatus { side, slot, previous: active.status, new }
        }
        4 => {
            let vol = match rng.below(4) {
                0 => VolatileStatus::Taunt,
                1 => VolatileStatus::LeechSeed,
                2 => VolatileStatus::Substitute,
                _ => VolatileStatus::Confusion,
            };
            if s.volatiles.contains(vol) {
                Instruction::RemoveVolatile { side, volatile: vol }
            } else {
                Instruction::ApplyVolatile { side, volatile: vol }
            }
        }
        5 => {
            let previous = s.side_conditions.spikes;
            let new = rng.below(4) as u8;
            Instruction::SetSideCondition { side, condition: SideConditionId::Spikes, previous, new }
        }
        6 => {
            let previous = s.side_conditions.stealth_rock as u8;
            let new = (rng.below(2)) as u8;
            Instruction::SetSideCondition { side, condition: SideConditionId::StealthRock, previous, new }
        }
        7 => {
            let new = match rng.below(3) {
                0 => Weather::Sand,
                1 => Weather::Rain,
                _ => Weather::None,
            };
            Instruction::ChangeWeather {
                previous: state.weather,
                previous_turns: state.weather_turns,
                new,
                new_turns: 5,
            }
        }
        8 => Instruction::Switch {
            side,
            previous: s.active_index,
            next: rng.below(6) as u8,
        },
        9 => {
            // PP decrement that won't underflow.
            let pp = active.moves[0].pp;
            let amount = if pp == 0 { 0 } else { 1 };
            Instruction::DecrementPp { side, slot, move_index: 0, amount }
        }
        10 => Instruction::ToggleTrickRoom {
            previous_turns: state.trick_room_turns,
            new_turns: if state.trick_room { 0 } else { 5 },
        },
        _ => {
            // A genuine disabled toggle (always flips, per contract).
            Instruction::SetMoveDisabled {
                side,
                slot,
                move_index: 0,
                disabled: !active.moves[0].disabled,
            }
        }
    }
}

#[test]
fn apply_then_reverse_restores_state() {
    for seed in 0..200u64 {
        let original = sample_state();
        let mut state = original;
        let mut rng = Lcg(seed.wrapping_mul(0x9E3779B97F4A7C15) ^ 0xDEADBEEF);

        // Build a list, capturing each instruction against the evolving state so the
        // `previous` fields stay consistent — exactly the generator's contract.
        let len = 1 + rng.below(40) as usize;
        let mut list = Vec::with_capacity(len);
        for _ in 0..len {
            let ins = gen_instruction(&state, &mut rng);
            state.apply_one(ins);
            list.push(ins);
        }

        // Now roll the whole list back.
        state.reverse_instructions(&list);
        assert_eq!(state, original, "round-trip failed for seed {seed}");
    }
}

#[test]
fn apply_reverse_pairwise_identity() {
    // Each instruction individually must be a perfect inverse pair.
    let original = sample_state();
    let mut rng = Lcg(12345);
    for _ in 0..500 {
        let mut state = original;
        let ins = gen_instruction(&state, &mut rng);
        state.apply_one(ins);
        state.reverse_one(ins);
        assert_eq!(state, original, "single-instruction round-trip failed for {ins:?}");
    }
}
