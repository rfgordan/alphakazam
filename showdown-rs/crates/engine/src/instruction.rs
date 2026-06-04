//! Reversible state deltas — the heart of the transition model.
//!
//! An [`Instruction`] is a single atomic change to the [`State`], small and *exactly
//! invertible*. A turn's effect is a `Vec<Instruction>` that the engine applies to one
//! mutable state and can later undo, so tree search / rollouts never clone whole states
//! on the hot path. (Cloning is still cheap here — see `state.rs` — but apply/undo on a
//! single state is cheaper still and is the primary loop.)
//!
//! A stochastic turn produces several [`StateInstructions`]: one weighted instruction
//! list per possible outcome (the crit branch, the miss branch, the secondary-proc
//! branch, ...). This is the representation the RL env samples from and the
//! differential harness drives via injected outcomes.
//!
//! ## Invariant: instructions carry *effective* amounts
//!
//! `Damage { amount }`, `Heal { amount }`, and `Boost { amount }` store the actual
//! change after clamping (HP can't drop below 0 or exceed max; a stat stage can't pass
//! ±6). The *generator* is responsible for computing the clamped amount; `apply`/
//! `reverse` are dumb add/subtract so that reversal is always exact. Where a delta is
//! unnatural (status, weather, types, item, ability, side conditions) the instruction
//! stores both `previous` and `new`.

use crate::ids::{Ability, BoostIndex, Item, Status, Terrain, Type, Weather};
use crate::state::{SideId, State};
use crate::volatile::VolatileStatus;

/// A weighted list of instructions representing one possible outcome of a turn.
/// `percentage` is the probability of this branch, in `[0, 100]`.
#[derive(Debug, Clone, PartialEq)]
pub struct StateInstructions {
    pub percentage: f32,
    pub instructions: Vec<Instruction>,
}

impl Default for StateInstructions {
    fn default() -> Self {
        StateInstructions {
            percentage: 100.0,
            instructions: Vec::with_capacity(8),
        }
    }
}

impl StateInstructions {
    pub fn new(percentage: f32) -> Self {
        StateInstructions {
            percentage,
            instructions: Vec::with_capacity(8),
        }
    }

    /// Scale this branch's probability (used when a decision point splits the tree).
    #[inline]
    pub fn scale(&mut self, factor: f32) {
        self.percentage *= factor;
    }
}

/// Identifies a side-condition field for [`Instruction::SetSideCondition`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SideConditionId {
    StealthRock,
    Spikes,
    ToxicSpikes,
    StickyWeb,
    Reflect,
    LightScreen,
    AuroraVeil,
    Tailwind,
}

/// An atomic, exactly-reversible change to the state.
///
/// Variants are grouped by what they touch. Each carries the minimum needed to undo
/// itself: a delta where natural, otherwise `previous`/`new` pairs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Instruction {
    // --- active Pokémon selection ---
    /// Change a side's active slot. Boost/volatile resets are emitted as their own
    /// instructions, so this is purely the index swap.
    Switch { side: SideId, previous: u8, next: u8 },

    // --- HP ---
    Damage { side: SideId, slot: u8, amount: i16 },
    Heal { side: SideId, slot: u8, amount: i16 },
    DamageSubstitute { side: SideId, amount: i16 },

    // --- status ---
    ChangeStatus { side: SideId, slot: u8, previous: Status, new: Status },
    ChangeStatusCounter { side: SideId, slot: u8, previous: u8, new: u8 },

    // --- boosts (stat stages of the active Pokémon) ---
    Boost { side: SideId, stat: BoostIndex, amount: i8 },

    // --- volatiles ---
    ApplyVolatile { side: SideId, volatile: VolatileStatus },
    RemoveVolatile { side: SideId, volatile: VolatileStatus },
    ChangeSubstituteHp { side: SideId, amount: i16 },

    // --- side conditions (hazards / screens) ---
    SetSideCondition { side: SideId, condition: SideConditionId, previous: u8, new: u8 },

    // --- field ---
    ChangeWeather { previous: Weather, previous_turns: i8, new: Weather, new_turns: i8 },
    DecrementWeatherTurns,
    ChangeTerrain { previous: Terrain, previous_turns: i8, new: Terrain, new_turns: i8 },
    DecrementTerrainTurns,
    ToggleTrickRoom { previous_turns: i8, new_turns: i8 },
    DecrementTrickRoomTurns,

    // --- moves ---
    DecrementPp { side: SideId, slot: u8, move_index: u8, amount: u8 },
    SetMoveDisabled { side: SideId, slot: u8, move_index: u8, disabled: bool },

    // --- transformations ---
    ChangeTypes { side: SideId, slot: u8, previous: [Type; 2], new: [Type; 2] },
    ChangeItem { side: SideId, slot: u8, previous: Item, new: Item },
    ChangeAbility { side: SideId, slot: u8, previous: Ability, new: Ability },
    ToggleTerastallized { side: SideId, slot: u8 },
}

impl State {
    /// Apply every instruction in order.
    pub fn apply_instructions(&mut self, instructions: &[Instruction]) {
        for ins in instructions {
            self.apply_one(*ins);
        }
    }

    /// Undo every instruction, in reverse order. After
    /// `apply_instructions(xs); reverse_instructions(xs);` the state is byte-identical
    /// to before.
    pub fn reverse_instructions(&mut self, instructions: &[Instruction]) {
        for ins in instructions.iter().rev() {
            self.reverse_one(*ins);
        }
    }

    pub fn apply_one(&mut self, ins: Instruction) {
        use Instruction::*;
        match ins {
            Switch { side, next, .. } => {
                self.side_mut(side).active_index = next;
            }
            Damage { side, slot, amount } => {
                self.sides[side.index()].pokemon[slot as usize].hp -= amount;
            }
            Heal { side, slot, amount } => {
                self.sides[side.index()].pokemon[slot as usize].hp += amount;
            }
            DamageSubstitute { side, amount } => {
                self.side_mut(side).substitute_hp -= amount;
            }
            ChangeStatus { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].status = new;
            }
            ChangeStatusCounter { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].status_counter = new;
            }
            Boost { side, stat, amount } => {
                self.side_mut(side).boosts[stat as usize] += amount;
            }
            ApplyVolatile { side, volatile } => {
                self.side_mut(side).volatiles.insert(volatile);
            }
            RemoveVolatile { side, volatile } => {
                self.side_mut(side).volatiles.remove(volatile);
            }
            ChangeSubstituteHp { side, amount } => {
                self.side_mut(side).substitute_hp += amount;
            }
            SetSideCondition { side, condition, new, .. } => {
                set_side_condition(self, side, condition, new);
            }
            ChangeWeather { new, new_turns, .. } => {
                self.weather = new;
                self.weather_turns = new_turns;
            }
            DecrementWeatherTurns => {
                self.weather_turns -= 1;
            }
            ChangeTerrain { new, new_turns, .. } => {
                self.terrain = new;
                self.terrain_turns = new_turns;
            }
            DecrementTerrainTurns => {
                self.terrain_turns -= 1;
            }
            ToggleTrickRoom { new_turns, .. } => {
                self.trick_room = !self.trick_room;
                self.trick_room_turns = new_turns;
            }
            DecrementTrickRoomTurns => {
                self.trick_room_turns -= 1;
            }
            DecrementPp { side, slot, move_index, amount } => {
                self.sides[side.index()].pokemon[slot as usize].moves[move_index as usize].pp -= amount;
            }
            SetMoveDisabled { side, slot, move_index, disabled } => {
                self.sides[side.index()].pokemon[slot as usize].moves[move_index as usize].disabled = disabled;
            }
            ChangeTypes { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].types = new;
            }
            ChangeItem { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].item = new;
            }
            ChangeAbility { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].ability = new;
            }
            ToggleTerastallized { side, slot } => {
                let p = &mut self.sides[side.index()].pokemon[slot as usize];
                p.terastallized = !p.terastallized;
            }
        }
    }

    pub fn reverse_one(&mut self, ins: Instruction) {
        use Instruction::*;
        match ins {
            Switch { side, previous, .. } => {
                self.side_mut(side).active_index = previous;
            }
            Damage { side, slot, amount } => {
                self.sides[side.index()].pokemon[slot as usize].hp += amount;
            }
            Heal { side, slot, amount } => {
                self.sides[side.index()].pokemon[slot as usize].hp -= amount;
            }
            DamageSubstitute { side, amount } => {
                self.side_mut(side).substitute_hp += amount;
            }
            ChangeStatus { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].status = previous;
            }
            ChangeStatusCounter { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].status_counter = previous;
            }
            Boost { side, stat, amount } => {
                self.side_mut(side).boosts[stat as usize] -= amount;
            }
            ApplyVolatile { side, volatile } => {
                self.side_mut(side).volatiles.remove(volatile);
            }
            RemoveVolatile { side, volatile } => {
                self.side_mut(side).volatiles.insert(volatile);
            }
            ChangeSubstituteHp { side, amount } => {
                self.side_mut(side).substitute_hp -= amount;
            }
            SetSideCondition { side, condition, previous, .. } => {
                set_side_condition(self, side, condition, previous);
            }
            ChangeWeather { previous, previous_turns, .. } => {
                self.weather = previous;
                self.weather_turns = previous_turns;
            }
            DecrementWeatherTurns => {
                self.weather_turns += 1;
            }
            ChangeTerrain { previous, previous_turns, .. } => {
                self.terrain = previous;
                self.terrain_turns = previous_turns;
            }
            DecrementTerrainTurns => {
                self.terrain_turns += 1;
            }
            ToggleTrickRoom { previous_turns, .. } => {
                self.trick_room = !self.trick_room;
                self.trick_room_turns = previous_turns;
            }
            DecrementTrickRoomTurns => {
                self.trick_room_turns += 1;
            }
            DecrementPp { side, slot, move_index, amount } => {
                self.sides[side.index()].pokemon[slot as usize].moves[move_index as usize].pp += amount;
            }
            SetMoveDisabled { side, slot, move_index, .. } => {
                // The only forward transition toggles disabled true/false; reversing a
                // disable restores `!disabled`. We store the post-value, so invert it.
                let m = &mut self.sides[side.index()].pokemon[slot as usize].moves[move_index as usize];
                m.disabled = !m.disabled;
            }
            ChangeTypes { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].types = previous;
            }
            ChangeItem { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].item = previous;
            }
            ChangeAbility { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].ability = previous;
            }
            ToggleTerastallized { side, slot } => {
                let p = &mut self.sides[side.index()].pokemon[slot as usize];
                p.terastallized = !p.terastallized;
            }
        }
    }
}

/// Write a `u8`-encoded value into the addressed side-condition field. Booleans map to
/// `0`/non-zero so the single instruction handles both layer counts and on/off flags.
fn set_side_condition(state: &mut State, side: SideId, condition: SideConditionId, value: u8) {
    let sc = &mut state.side_mut(side).side_conditions;
    match condition {
        SideConditionId::StealthRock => sc.stealth_rock = value != 0,
        SideConditionId::Spikes => sc.spikes = value,
        SideConditionId::ToxicSpikes => sc.toxic_spikes = value,
        SideConditionId::StickyWeb => sc.sticky_web = value != 0,
        SideConditionId::Reflect => sc.reflect = value,
        SideConditionId::LightScreen => sc.light_screen = value,
        SideConditionId::AuroraVeil => sc.aurora_veil = value,
        SideConditionId::Tailwind => sc.tailwind = value,
    }
}
