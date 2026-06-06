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
pub mod encode;
pub mod gen;
pub mod generate;
pub mod ids;
pub mod instruction;
pub mod names;
pub mod narrate;
pub mod state;
pub mod team;
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

    #[test]
    fn observe_masks_unrevealed_foe_info_and_keeps_own_side() {
        use ids::{Ability, Item, MoveId, Species, Type};
        use state::Reveal;

        let mut st = State::EMPTY;
        // Give each side one real Pokémon with an item, ability, a move, and a Tera type.
        for s in 0..2 {
            let p = &mut st.sides[s].pokemon[0];
            p.species = Species(1);
            p.hp = 100;
            p.max_hp = 100;
            p.item = Item::Leftovers;
            p.ability = Ability::ClearBody;
            p.base_ability = Ability::ClearBody;
            p.tera_type = Type::Fire;
            p.moves[0].id = MoveId(2);
            p.evs[1] = 252;
        }
        // Side One (the foe of viewer Two) has revealed its move slot 0 and its item; nothing else.
        st.sides[0].pokemon[0].reveal = Reveal { moves: 0b0001, flags: Reveal::ITEM };

        // From Side Two's perspective, the foe is Side One.
        let obs = st.observe(SideId::Two);
        let foe = &obs.sides[0].pokemon[0];
        assert_eq!(foe.item, Item::Leftovers, "revealed item stays visible");
        assert_eq!(foe.moves[0].id, MoveId(2), "revealed move stays visible");
        assert_eq!(foe.ability, Ability::Unknown, "unrevealed ability is masked");
        assert_eq!(foe.tera_type, Type::None, "unrevealed Tera type is masked");
        assert_eq!(foe.moves[1].id, MoveId(0), "unrevealed move slot is cleared");
        assert_eq!(foe.evs, [0; ids::StatIndex::COUNT], "spread is hidden");
        // The viewer's own side is untouched.
        assert_eq!(obs.sides[1].pokemon[0].ability, Ability::ClearBody);
        assert_eq!(obs.sides[1].pokemon[0].evs[1], 252);
    }

    #[test]
    fn reveal_instruction_is_reversible() {
        use state::Reveal;
        let mut st = State::EMPTY;
        st.sides[0].pokemon[2].reveal = Reveal { moves: 0b0010, flags: Reveal::ABILITY };
        let before = st;
        // Set new bits only (move slot 0 + ITEM); move slot 1 / ABILITY already set.
        let ins = Instruction::Reveal { side: SideId::One, slot: 2, moves: 0b0001, flags: Reveal::ITEM };
        st.apply_one(ins);
        let r = st.sides[0].pokemon[2].reveal;
        assert_eq!(r.moves, 0b0011);
        assert_eq!(r.flags, Reveal::ABILITY | Reveal::ITEM);
        st.reverse_one(ins);
        assert_eq!(st, before, "reverse restores the exact prior mask");
    }
}
