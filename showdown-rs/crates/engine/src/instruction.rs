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

use crate::ids::{Ability, BoostIndex, Item, MoveId, Status, Terrain, Type, Weather};
use crate::state::{PendingMove, SideId, State};

/// Selects which of the active Pokémon's simple turn-countdowns an instruction touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveCounter {
    Taunt,
    Confusion,
    Perish,
    Yawn,
    ThroatChop,
    HealBlock,
    ActiveTurns,
    MagnetRise,
}
use crate::volatile::VolatileStatus;
use crate::ids::Species;
use crate::state::MoveSlot;

/// Everything Transform overwrites on the user (HP/level/status are untouched). `stats[0]`
/// (HP) is carried through unchanged by convention.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransformData {
    pub species: Species,
    pub stats: [i16; 6],
    pub types: [Type; 2],
    pub ability: Ability,
    pub moves: [MoveSlot; 4],
    pub transformed: bool,
    /// Transform copies `timesAttacked` (Rage Fist counts).
    pub times_hit: u8,
}


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
    /// Partial-trap payload `(turns, divisor)` for the active mon (pairs with `PartiallyTrapped`).
    SetPartialTrap { side: SideId, previous: (u8, u8), new: (u8, u8) },

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

    // --- consecutive-use tracking (active Pokémon; resets on switch) ---
    SetLastMove { side: SideId, previous: MoveId, new: MoveId },
    SetMoveStreak { side: SideId, previous: u8, new: u8 },
    SetStallCounter { side: SideId, previous: u8, new: u8 },
    SetStallTurns { side: SideId, previous: u8, new: u8 },

    // --- multi-turn move state (active Pokémon; resets on switch) ---
    SetPendingMove { side: SideId, previous: PendingMove, new: PendingMove },
    SetEncore { side: SideId, previous: (MoveId, u8), new: (MoveId, u8) },
    SetDisable { side: SideId, previous: (MoveId, u8), new: (MoveId, u8) },
    SetActiveCounter { side: SideId, which: ActiveCounter, previous: u8, new: u8 },
    SetWish { side: SideId, previous: (u8, i16), new: (u8, i16) },
    SetFutureSight { side: SideId, previous: (u8, u8), new: (u8, u8) },
    /// Special damage this side's active took from the foe this turn (Mirror Coat's 2× source).
    SetSpecialDamageTaken { side: SideId, previous: i16, new: i16 },
    /// Physical damage this side's active took from the foe this turn (Focus Punch's fail gate).
    SetPhysicalDamageTaken { side: SideId, previous: i16, new: i16 },
    /// Transform / Imposter: replace the active's battle identity with a copy of the foe's
    /// (or restore its own on switch-out). `previous_base_moves` keeps reversal byte-exact:
    /// entering a transform overwrites `base_moves` with the pre-transform slots.
    Transform { side: SideId, slot: u8, previous: TransformData, new: TransformData, previous_base_moves: [MoveSlot; 4] },
    SetHealingWish { side: SideId, previous: bool, new: bool },
    SetSleptByFoe { side: SideId, slot: u8, previous: bool, new: bool },
    SetLastBerry { side: SideId, slot: u8, previous: Item, new: Item },
    SetCudChew { side: SideId, slot: u8, previous: u8, new: u8 },

    // --- battle-long per-mon history (persists across switches) ---
    SetAbilityUsed { side: SideId, slot: u8, previous: bool, new: bool },
    SetTimesHit { side: SideId, slot: u8, previous: u8, new: u8 },

    // --- hidden-information layer (what the foe has learned; never read by the transition) ---
    /// OR the given *newly-set* bits into a Pokémon's reveal mask. `moves`/`flags` carry only the
    /// bits this instruction sets (delta vs. the current mask), so reversal clears exactly them.
    Reveal { side: SideId, slot: u8, moves: u8, flags: u8 },

    // --- transformations ---
    ChangeTypes { side: SideId, slot: u8, previous: [Type; 2], new: [Type; 2] },
    ChangeItem { side: SideId, slot: u8, previous: Item, new: Item },
    ChangeAbility { side: SideId, slot: u8, previous: Ability, new: Ability },
    ToggleTerastallized { side: SideId, slot: u8 },

    // --- decision-point markers (state no-ops; consumed by the request-flow driver) ---
    /// A pivot move (U-turn/Teleport/…) connected and its user survived, but the landing choice
    /// is deferred to a `PivotLanding` request instead of being applied inline. Emitted only
    /// under `Pivot::Pause` (request-model executor); never on verification paths.
    PivotPending { side: SideId },
    /// Revival Blessing connected and its user has a fainted ally, but the revive target
    /// choice is deferred to a `Revive` request instead of being applied inline. Emitted only
    /// under `Pivot::Pause` (request-model executor); never on verification paths.
    RevivePending { side: SideId },
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
            SetHealingWish { side, new, .. } => {
                self.side_mut(side).healing_wish = new;
            }
            SetSleptByFoe { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].slept_by_foe = new;
            }
            SetLastBerry { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].last_berry = new;
            }
            SetCudChew { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].cudchew_turns = new;
            }
            Transform { side, slot, previous, new, .. } => {
                let p = &mut self.sides[side.index()].pokemon[slot as usize];
                p.species = new.species;
                for i in 1..6 { p.stats[i] = new.stats[i]; }
                // Forme changes (Tera Shift) can change max HP; damage taken is preserved.
                if new.stats[0] != previous.stats[0] {
                    p.stats[0] = new.stats[0];
                    p.max_hp = new.stats[0];
                    p.hp += new.stats[0] - previous.stats[0];
                }
                p.types = new.types;
                p.ability = new.ability;
                p.moves = new.moves;
                p.transformed = new.transformed;
                p.times_hit = new.times_hit;
                if !previous.transformed && new.transformed {
                    // Entering a transform: remember what to restore on switch-out.
                    p.base_species = previous.species;
                    p.base_stats = previous.stats;
                    p.base_moves = previous.moves;
                }
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
            SetPartialTrap { side, new, .. } => {
                let s = self.side_mut(side);
                s.partial_trap_turns = new.0;
                s.partial_trap_div = new.1;
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
            SetLastMove { side, new, .. } => {
                self.side_mut(side).last_used_move = new;
            }
            SetMoveStreak { side, new, .. } => {
                self.side_mut(side).move_streak = new;
            }
            SetStallCounter { side, new, .. } => {
                self.side_mut(side).stall_counter = new;
            }
            SetStallTurns { side, new, .. } => {
                self.side_mut(side).stall_turns = new;
            }
            SetPendingMove { side, new, .. } => {
                self.side_mut(side).pending_move = new;
            }
            SetEncore { side, new, .. } => {
                self.side_mut(side).encore = new;
            }
            SetDisable { side, new, .. } => {
                self.side_mut(side).disable = new;
            }
            SetActiveCounter { side, which, new, .. } => {
                set_active_counter(self, side, which, new);
            }
            SetWish { side, new, .. } => {
                self.side_mut(side).wish = new;
            }
            SetFutureSight { side, new, .. } => {
                self.side_mut(side).future_sight = new;
            }
            SetSpecialDamageTaken { side, new, .. } => {
                self.side_mut(side).special_damage_taken = new;
            }
            SetPhysicalDamageTaken { side, new, .. } => {
                self.side_mut(side).physical_damage_taken = new;
            }
            SetAbilityUsed { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].ability_used = new;
            }
            SetTimesHit { side, slot, new, .. } => {
                self.sides[side.index()].pokemon[slot as usize].times_hit = new;
            }
            Reveal { side, slot, moves, flags } => {
                let r = &mut self.sides[side.index()].pokemon[slot as usize].reveal;
                r.moves |= moves;
                r.flags |= flags;
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
                let s = &mut self.sides[side.index()];
                s.pokemon[slot as usize].terastallized = !s.pokemon[slot as usize].terastallized;
                // Once-per-battle side flag. Recomputation is exact in both directions because
                // the generator gates tera on !tera_used (a side toggles at most once).
                s.tera_used = s.pokemon.iter().any(|p| p.terastallized);
            }
            PivotPending { .. } => {}
            RevivePending { .. } => {}
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
            SetHealingWish { side, previous, .. } => {
                self.side_mut(side).healing_wish = previous;
            }
            SetSleptByFoe { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].slept_by_foe = previous;
            }
            SetLastBerry { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].last_berry = previous;
            }
            SetCudChew { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].cudchew_turns = previous;
            }
            Transform { side, slot, previous, new, previous_base_moves } => {
                let p = &mut self.sides[side.index()].pokemon[slot as usize];
                p.species = previous.species;
                for i in 1..6 { p.stats[i] = previous.stats[i]; }
                if new.stats[0] != previous.stats[0] {
                    p.stats[0] = previous.stats[0];
                    p.max_hp = previous.stats[0];
                    p.hp -= new.stats[0] - previous.stats[0];
                }
                p.types = previous.types;
                p.ability = previous.ability;
                p.moves = previous.moves;
                p.transformed = previous.transformed;
                p.times_hit = previous.times_hit;
                if !previous.transformed && new.transformed {
                    // Undo the base_* overwrite from entering the transform.
                    p.base_species = previous.species;
                    p.base_stats = previous.stats;
                    p.base_moves = previous_base_moves;
                }
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
            SetPartialTrap { side, previous, .. } => {
                let s = self.side_mut(side);
                s.partial_trap_turns = previous.0;
                s.partial_trap_div = previous.1;
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
            SetLastMove { side, previous, .. } => {
                self.side_mut(side).last_used_move = previous;
            }
            SetMoveStreak { side, previous, .. } => {
                self.side_mut(side).move_streak = previous;
            }
            SetStallCounter { side, previous, .. } => {
                self.side_mut(side).stall_counter = previous;
            }
            SetStallTurns { side, previous, .. } => {
                self.side_mut(side).stall_turns = previous;
            }
            SetPendingMove { side, previous, .. } => {
                self.side_mut(side).pending_move = previous;
            }
            SetEncore { side, previous, .. } => {
                self.side_mut(side).encore = previous;
            }
            SetDisable { side, previous, .. } => {
                self.side_mut(side).disable = previous;
            }
            SetActiveCounter { side, which, previous, .. } => {
                set_active_counter(self, side, which, previous);
            }
            SetWish { side, previous, .. } => {
                self.side_mut(side).wish = previous;
            }
            SetFutureSight { side, previous, .. } => {
                self.side_mut(side).future_sight = previous;
            }
            SetSpecialDamageTaken { side, previous, .. } => {
                self.side_mut(side).special_damage_taken = previous;
            }
            SetPhysicalDamageTaken { side, previous, .. } => {
                self.side_mut(side).physical_damage_taken = previous;
            }
            SetAbilityUsed { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].ability_used = previous;
            }
            SetTimesHit { side, slot, previous, .. } => {
                self.sides[side.index()].pokemon[slot as usize].times_hit = previous;
            }
            Reveal { side, slot, moves, flags } => {
                // `moves`/`flags` are the bits this instruction newly set, so clearing exactly
                // them restores the prior mask (reveals are otherwise monotonic).
                let r = &mut self.sides[side.index()].pokemon[slot as usize].reveal;
                r.moves &= !moves;
                r.flags &= !flags;
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
                let s = &mut self.sides[side.index()];
                s.pokemon[slot as usize].terastallized = !s.pokemon[slot as usize].terastallized;
                // Once-per-battle side flag. Recomputation is exact in both directions because
                // the generator gates tera on !tera_used (a side toggles at most once).
                s.tera_used = s.pokemon.iter().any(|p| p.terastallized);
            }
            PivotPending { .. } => {}
            RevivePending { .. } => {}
        }
    }
}

/// Write a value into the addressed active-Pokémon turn-countdown field.
fn set_active_counter(state: &mut State, side: SideId, which: ActiveCounter, value: u8) {
    let s = state.side_mut(side);
    match which {
        ActiveCounter::Taunt => s.taunt_turns = value,
        ActiveCounter::Confusion => s.confusion_turns = value,
        ActiveCounter::Perish => s.perish_turns = value,
        ActiveCounter::Yawn => s.yawn_turns = value,
        ActiveCounter::ThroatChop => s.throat_chop_turns = value,
        ActiveCounter::HealBlock => s.heal_block_turns = value,
        ActiveCounter::ActiveTurns => s.active_turns = value,
        ActiveCounter::MagnetRise => s.magnet_rise_turns = value,
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
