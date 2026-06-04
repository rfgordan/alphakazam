//! A clean-slate Pokémon battle engine, built for fast reinforcement-learning
//! simulation and verified against Pokémon Showdown via differential testing.
//!
//! Module layout:
//!   - [`ids`]      — integer-backed identifier enums (Species, MoveId, Type, ...).
//!   - [`volatile`] — packed bitset of volatile statuses.
//!   - [`state`]    — the flat, `Copy`-able [`state::State`] / `Side` / `Pokemon`.
//!
//! Coming next: `instruction` (reversible state deltas + apply/reverse) and
//! `generate_instructions` (the weighted-outcome transition model).

pub mod damage;
pub mod data;
pub mod gen;
pub mod generate;
pub mod ids;
pub mod instruction;
pub mod names;
pub mod state;
pub mod volatile;

pub use generate::{generate_instructions, MoveChoice};

pub use instruction::{Instruction, StateInstructions};
pub use state::{Pokemon, Side, SideId, State};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_is_copy_and_flat() {
        // A compile-time witness that `State: Copy` (and therefore heap-free).
        fn assert_copy<T: Copy>() {}
        assert_copy::<State>();

        // Snapshotting is a memcpy; mutating the copy must not touch the original.
        let mut a = State::EMPTY;
        let b = a; // Copy, not move
        a.turn = 42;
        assert_eq!(b.turn, 0);
        assert_eq!(a.turn, 42);
    }

    #[test]
    fn state_size_is_reasonable() {
        // Not a hard requirement, just a guard against accidental bloat. A whole
        // battle state in well under a few KB keeps snapshots cache-friendly.
        let bytes = std::mem::size_of::<State>();
        assert!(bytes < 4096, "State grew to {bytes} bytes");
    }
}
