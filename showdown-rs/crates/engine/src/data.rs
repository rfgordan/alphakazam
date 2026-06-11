//! Static game data for the current id slice: move records and species base stats.
//!
//! Move *effects* are data-driven: the engine's turn logic reads `self_boosts`,
//! `secondary_*`, `status`, `side_condition`, `heal`, `self_switch`, and
//! `target_volatile` from `MoveData` rather than matching on individual move ids. This
//! is the architecture that lets coverage grow by adding data rows (eventually
//! generated from PS `data/moves.ts`) instead of code.

use crate::ids::{BoostIndex, MoveCategory, MoveId, Species, Status, Type};
use crate::instruction::SideConditionId;
use crate::volatile::VolatileStatus;

/// The full record for a move: damage fields plus a data description of its effects.
/// Who a move targets (PS's `target` field). Drives Pressure PP, drag/self routing,
/// redirection, and spread logic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MoveTarget {
    Normal = 0,
    User,               // PS "self"
    AdjacentAlly,
    AdjacentAllyOrSelf,
    AdjacentFoe,
    AllAdjacent,
    AllAdjacentFoes,
    All,
    Allies,
    AllySide,
    AllyTeam,
    Any,
    FoeSide,
    RandomNormal,
    Scripted,
}

impl MoveTarget {
    /// Does this move target the opposing side (Pressure-taxed, can be redirected, ...)?
    pub fn targets_foe(self) -> bool {
        !matches!(
            self,
            MoveTarget::User
                | MoveTarget::AdjacentAlly
                | MoveTarget::AdjacentAllyOrSelf
                | MoveTarget::Allies
                | MoveTarget::AllySide
                | MoveTarget::AllyTeam
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveData {
    pub id: MoveId,
    pub typ: Type,
    pub category: MoveCategory,
    pub base_power: u16,
    /// Accuracy in percent; `0` means "never misses" (bypasses the accuracy check).
    pub accuracy: u8,
    pub priority: i8,
    /// Base PP (before PP Ups; PS default sets carry max PP Ups = ×8/5).
    pub pp: u8,
    pub target: MoveTarget,
    /// Crit-stage bonus from the move itself (1 = normal, 2 = high-crit like Slash, ...).
    pub crit_ratio: u8,
    /// Always crits (Wicked Blow, Surging Strikes, Flower Trick, ...).
    pub always_crit: bool,
    /// The user faints on use (Explosion, Self-Destruct, Memento, Misty Explosion, ...).
    pub self_destruct: bool,
    /// Number of hits (1 for most; e.g. Dragon Darts = 2). For a variable multi-hit move
    /// (Bullet Seed, Rock Blast, ...) this is the guaranteed *minimum*; `hits_max` the max.
    pub hits: u8,
    /// Maximum number of hits; equals `hits` for fixed-count moves. `hits_max > hits` marks
    /// a variable multi-hit move ([min, max], almost always [2, 5] in gen9).
    pub hits_max: u8,
    /// Body Press and similar use the user's Defense as the attacking stat.
    pub uses_defense_as_attack: bool,
    /// Pivot move (U-turn): the user switches out after it connects.
    pub self_switch: bool,
    /// Forces the *target* to switch out (Roar, Whirlwind, Dragon Tail, ...).
    pub force_switch: bool,

    /// Stat-stage changes applied to the *user* (Close Combat self-drop, Swords Dance).
    pub self_boosts: [i8; BoostIndex::COUNT],
    /// Stat-stage changes a status move applies to the *target* (Growl, Charm, ...).
    pub target_boosts: [i8; BoostIndex::COUNT],

    /// Chance (percent) the move flinches the target (Iron Head 30%, Fake Out 100%).
    pub flinch_chance: u8,
    /// Chance (percent) of the target secondary; `0` = no secondary.
    pub secondary_chance: u8,
    /// Stat-stage changes applied to the *target* when the secondary procs.
    pub secondary_boosts: [i8; BoostIndex::COUNT],
    /// Status applied to the target when the secondary procs (`None` = no status).
    pub secondary_status: Status,
    /// Volatile applied to the target when the secondary procs (Hurricane confusion, ...).
    pub secondary_volatile: Option<VolatileStatus>,

    /// Primary status a status-move inflicts on the target (Will-O-Wisp = Burn).
    pub status: Status,
    /// Hazard this move sets on the opposing side, if any.
    pub side_condition: Option<SideConditionId>,
    /// Self-heal as a fraction of max HP (numerator, denominator); `(0, _)` = none.
    pub heal: (u8, u8),
    /// Recoil as a fraction of damage dealt (numerator, denominator); `(0, _)` = none.
    pub recoil: (u8, u8),
    /// Drain (heal) as a fraction of damage dealt (numerator, denominator); `(0,_)`=none.
    pub drain: (u8, u8),
    /// Volatile this move applies to the target (Salt Cure, etc.).
    pub target_volatile: Option<VolatileStatus>,
    /// Weather this move sets (Sunny Day, Sandstorm, ...).
    pub weather: crate::ids::Weather,
    // Move flags relevant to abilities/items.
    pub flag_contact: bool,
    pub flag_sound: bool,
    pub flag_punch: bool,
    pub flag_bite: bool,
    pub flag_slicing: bool,
    pub flag_bullet: bool,
    pub flag_pulse: bool,
    /// `heal` flag: the move restores HP (Recover, Roost, drains) — fails under Heal Block.
    pub flag_heal: bool,
    /// Powder move (Spore, Sleep Powder, Stun Spore): no effect on Grass types, Overcoat,
    /// or Safety Goggles holders.
    pub flag_powder: bool,
}

impl MoveData {
    /// A defaulted move record; override fields with struct-update syntax.
    const fn new(id: MoveId, typ: Type, category: MoveCategory, base_power: u16, accuracy: u8, priority: i8) -> Self {
        MoveData {
            id,
            typ,
            category,
            base_power,
            accuracy,
            priority,
            pp: 0,
            target: MoveTarget::Normal,
            crit_ratio: 1,
            always_crit: false,
            self_destruct: false,
            hits: 1,
            hits_max: 1,
            flinch_chance: 0,
            uses_defense_as_attack: false,
            self_switch: false,
            force_switch: false,
            self_boosts: [0; BoostIndex::COUNT],
            target_boosts: [0; BoostIndex::COUNT],
            secondary_chance: 0,
            secondary_boosts: [0; BoostIndex::COUNT],
            secondary_status: Status::None,
            secondary_volatile: None,
            status: Status::None,
            side_condition: None,
            heal: (0, 1),
            recoil: (0, 1),
            drain: (0, 1),
            target_volatile: None,
            weather: crate::ids::Weather::None,
            flag_contact: false,
            flag_sound: false,
            flag_punch: false,
            flag_bite: false,
            flag_slicing: false,
            flag_bullet: false,
            flag_pulse: false,
            flag_heal: false,
            flag_powder: false,
        }
    }
}

/// Build a boost array from (BoostIndex, delta) pairs.
const fn boosts(pairs: &[(BoostIndex, i8)]) -> [i8; BoostIndex::COUNT] {
    let mut out = [0i8; BoostIndex::COUNT];
    let mut i = 0;
    while i < pairs.len() {
        out[pairs[i].0 as usize] = pairs[i].1;
        i += 1;
    }
    out
}

impl MoveData {
    /// The empty/no-op move (id 0), used as the table's index-0 sentinel.
    pub const fn none() -> Self {
        MoveData::new(MoveId::None, Type::Normal, MoveCategory::Status, 0, 0, 0)
    }
}

/// Look up a move's full record from the generated table (`gen::MOVES`).
pub fn move_data(id: MoveId) -> MoveData {
    crate::gen::MOVES.get(id.0 as usize).copied().unwrap_or_else(MoveData::none)
}

/// Species base stats `[hp, atk, def, spa, spd, spe]` from the generated table.
pub fn base_stats(species: Species) -> [u16; 6] {
    crate::gen::SPECIES_BASE_STATS.get(species.0 as usize).copied().unwrap_or([0; 6])
}

/// Species typing from the generated table.
pub fn species_types(species: Species) -> [Type; 2] {
    crate::gen::SPECIES_TYPES.get(species.0 as usize).copied().unwrap_or([Type::None, Type::None])
}

/// Species weight in hectograms (kg × 10), from the generated table.
pub fn species_weight_hg(species: Species) -> u32 {
    crate::gen::SPECIES_WEIGHT_HG.get(species.0 as usize).copied().unwrap_or(0)
}

/// Whether the species is not-fully-evolved (can still evolve) — Eviolite raises the
/// defenses of such Pokémon.
pub fn species_is_nfe(species: Species) -> bool {
    crate::gen::SPECIES_NFE.get(species.0 as usize).copied().unwrap_or(false)
}
