//! The battle state.
//!
//! Design priorities, in order:
//!   1. **Cheap to snapshot.** The entire tree is `Copy` — fixed-size arrays, small
//!      integer fields, no heap, no `Rc`/`Arc`. `let snap = *state;` is a memcpy.
//!      This is what makes replay buffers, parallel rollouts, and (eventually) a
//!      vectorized RL env cheap. Contrast with poke-engine, where each `Pokemon`
//!      embeds four cloned move-data structs and each side a `HashSet`.
//!   2. **Flat / index-addressable.** Sides are `[_; 2]`, party slots `[_; 6]`, moves
//!      `[_; 4]`. An `Instruction` addresses any field by `(side, slot, ...)` indices.
//!   3. **Data out of band.** A `MoveSlot` stores only `(id, pp, disabled)`. Static
//!      move/species data is looked up from tables in `data.rs` by id, never copied
//!      into the state.

use crate::ids::{Ability, BoostIndex, Item, MoveId, Nature, Species, StatIndex, Status, Terrain, Type, Weather};
use crate::volatile::Volatiles;

/// One of a Pokémon's four move slots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveSlot {
    pub id: MoveId,
    pub pp: u8,
    pub max_pp: u8,
    pub disabled: bool,
}

impl MoveSlot {
    pub const EMPTY: MoveSlot = MoveSlot {
        id: MoveId::None,
        pp: 0,
        max_pp: 0,
        disabled: false,
    };
}

/// A single Pokémon. Stats are the final computed values (after nature/EV/IV), so the
/// hot path never recomputes them; `nature`/`evs` are retained for reference and for
/// abilities that recompute (e.g. Protosynthesis picks the highest stat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pokemon {
    pub species: Species,
    pub level: u8,

    /// Current effective typing. `base_types` is the original for moves like Roost /
    /// abilities that restore it; `types` reflects type changes (Soak, tera, etc.).
    pub types: [Type; 2],
    pub base_types: [Type; 2],

    pub hp: i16,
    pub max_hp: i16,

    /// Final computed stats, indexed by `StatIndex` (slot 0 = HP, unused here).
    pub stats: [i16; StatIndex::COUNT],

    pub status: Status,
    /// Counter that rides along with `status`: remaining sleep turns, or toxic stage.
    pub status_counter: u8,

    pub ability: Ability,
    pub base_ability: Ability,
    pub item: Item,
    pub nature: Nature,
    pub evs: [u8; StatIndex::COUNT],

    pub moves: [MoveSlot; 4],

    pub tera_type: Type,
    pub terastallized: bool,
}

impl Pokemon {
    pub const EMPTY: Pokemon = Pokemon {
        species: Species::None,
        level: 100,
        types: [Type::None, Type::None],
        base_types: [Type::None, Type::None],
        hp: 0,
        max_hp: 0,
        stats: [0; StatIndex::COUNT],
        status: Status::None,
        status_counter: 0,
        ability: Ability::None,
        base_ability: Ability::None,
        item: Item::None,
        nature: Nature::Serious,
        evs: [0; StatIndex::COUNT],
        moves: [MoveSlot::EMPTY; 4],
        tera_type: Type::None,
        terastallized: false,
    };

    #[inline]
    pub fn is_alive(&self) -> bool {
        self.hp > 0
    }

    #[inline]
    pub fn stat(&self, idx: StatIndex) -> i16 {
        self.stats[idx as usize]
    }
}

/// Entry/field hazards and screens. All small integers so the side stays `Copy`.
/// Counts/turns are stored directly (e.g. `spikes` is layers 0..=3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SideConditions {
    pub stealth_rock: bool,
    pub spikes: u8,        // 0..=3 layers
    pub toxic_spikes: u8,  // 0..=2 layers
    pub sticky_web: bool,
    pub reflect: u8,       // turns remaining
    pub light_screen: u8,  // turns remaining
    pub aurora_veil: u8,   // turns remaining
    pub tailwind: u8,      // turns remaining
}

/// One player's side of the field.
///
/// Boosts and volatiles belong to the *active* Pokémon and reset on switch, so they
/// live here rather than on `Pokemon` — matching how Showdown and poke-engine model it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Side {
    pub pokemon: [Pokemon; 6],
    pub active_index: u8,

    /// Stat stages of the active Pokémon, indexed by `BoostIndex` (-6..=6).
    pub boosts: [i8; BoostIndex::COUNT],

    /// Volatile statuses of the active Pokémon (bitset).
    pub volatiles: Volatiles,
    /// Payloads for volatiles that carry a value.
    pub substitute_hp: i16,
    pub confusion_turns: u8,
    pub encore_turns: u8,
    pub taunt_turns: u8,
    pub disable_turns: u8,
    pub locked_move_turns: u8,

    pub side_conditions: SideConditions,

    /// Wish: (turns remaining, heal amount). turns == 0 means inactive.
    pub wish: (u8, i16),
    /// Future Sight: (turns remaining, source party slot). turns == 0 means inactive.
    pub future_sight: (u8, u8),
}

impl Side {
    pub const EMPTY: Side = Side {
        pokemon: [Pokemon::EMPTY; 6],
        active_index: 0,
        boosts: [0; BoostIndex::COUNT],
        volatiles: Volatiles::empty(),
        substitute_hp: 0,
        confusion_turns: 0,
        encore_turns: 0,
        taunt_turns: 0,
        disable_turns: 0,
        locked_move_turns: 0,
        side_conditions: SideConditions {
            stealth_rock: false,
            spikes: 0,
            toxic_spikes: 0,
            sticky_web: false,
            reflect: 0,
            light_screen: 0,
            aurora_veil: 0,
            tailwind: 0,
        },
        wish: (0, 0),
        future_sight: (0, 0),
    };

    #[inline]
    pub fn active(&self) -> &Pokemon {
        &self.pokemon[self.active_index as usize]
    }

    #[inline]
    pub fn active_mut(&mut self) -> &mut Pokemon {
        &mut self.pokemon[self.active_index as usize]
    }

    #[inline]
    pub fn boost(&self, idx: BoostIndex) -> i8 {
        self.boosts[idx as usize]
    }
}

/// Identifies a side. Index into `State::sides`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum SideId {
    One = 0,
    Two = 1,
}

impl SideId {
    #[inline]
    pub fn other(self) -> SideId {
        match self {
            SideId::One => SideId::Two,
            SideId::Two => SideId::One,
        }
    }

    #[inline]
    pub fn index(self) -> usize {
        self as usize
    }
}

/// The complete battle state. `Copy`: `let snapshot = *state;` is a flat memcpy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct State {
    pub sides: [Side; 2],

    pub weather: Weather,
    pub weather_turns: i8,
    pub terrain: Terrain,
    pub terrain_turns: i8,
    pub trick_room: bool,
    pub trick_room_turns: i8,

    pub turn: u32,
}

impl State {
    pub const EMPTY: State = State {
        sides: [Side::EMPTY; 2],
        weather: Weather::None,
        weather_turns: 0,
        terrain: Terrain::None,
        terrain_turns: 0,
        trick_room: false,
        trick_room_turns: 0,
        turn: 0,
    };

    #[inline]
    pub fn side(&self, id: SideId) -> &Side {
        &self.sides[id.index()]
    }

    #[inline]
    pub fn side_mut(&mut self, id: SideId) -> &mut Side {
        &mut self.sides[id.index()]
    }
}

impl Default for State {
    fn default() -> Self {
        State::EMPTY
    }
}
