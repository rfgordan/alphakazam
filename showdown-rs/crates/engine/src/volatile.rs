//! Volatile statuses, stored as a packed bitset rather than a `HashSet`.
//!
//! Volatiles belong to whichever Pokémon is currently active and are cleared on
//! switch-out, so they live on the `Side`. Representing the active set as a single
//! `u64` keeps `State` `Copy` and makes membership tests/clears branch-free — a
//! meaningful win when snapshotting states for replay buffers or vectorized envs.
//!
//! Statuses that carry a *counter* or *value* (confusion turns left, substitute HP,
//! etc.) keep the flag here for presence and store their payload in dedicated fields
//! on the `Side`.

/// One volatile status. The discriminant doubles as the bit index, so there is room
/// for 64 distinct volatiles before this needs to widen to `u128`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum VolatileStatus {
    Confusion = 0,
    Substitute,
    LeechSeed,
    Taunt,
    Encore,
    Disable,
    Protect,
    Endure,
    Flinch,
    Roost,
    Charge,
    Yawn,
    PerishSong,
    DestinyBond,
    Curse,
    Nightmare,
    Attract,
    Torment,
    SaltCure,
    GlaiveRush,
    LockedMove, // e.g. Outrage/Petal Dance lock
    MustRecharge,
    PartiallyTrapped,
    Roosted,
    ChoiceLock,      // locked into one move by a Choice item
    Protosynthesis,  // Booster Energy / sun stat boost active
    QuarkDrive,      // Booster Energy / electric terrain stat boost active
    FocusEnergy,     // +2 crit stages (Focus Energy / Dragon Cheer)
    Unburden,
    ThroatChop,
    HealBlock,
    TypeShifted,        // lost/consumed its item: Speed ×2 until switch-out
    // --- trapping (switch-out prevention) ---
    Trapped,            // Mean Look / Block / Spider Web / Jaw Lock — foe-sourced, ends when trapper leaves
    Ingrain,            // self-rooted: cannot switch (also grounded + 1/16 heal)
    NoRetreat,          // self-trap after the +1-all-stats boost
    Octolock,           // foe-sourced trap + end-of-turn Def/SpD drop
    FlashFire,          // absorbed a Fire move: ×1.5 own Fire moves until switch-out
    Truant,             // Slaking: loaf on the next move attempt (toggles every attempt)
    StatsRaisedThisTurn, // PS `pokemon.statsRaisedThisTurn` (Burning Jealousy's burn gate)
    StatsLoweredThisTurn, // PS `pokemon.statsLoweredThisTurn` (Lash Out's ×2 gate)
    MagnetRise,          // Magnet Rise: 5-turn Ground immunity / ungrounding
    /// PS's `fromBooster` flag on the `protosynthesis` / `quarkdrive` condition: the boost came
    /// from a consumed Booster Energy rather than from the field. A field-sourced boost is removed
    /// the moment the sun / Electric Terrain lapses (`onWeatherChange` / `onTerrainChange`); a
    /// Booster-sourced one is kept forever (until switch-out). Companion bit to
    /// `Protosynthesis` / `QuarkDrive` — only ever set alongside one of them.
    ProtoBooster,
}

/// A packed set of active-Pokémon volatile statuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Volatiles(pub u64);

impl Volatiles {
    #[inline]
    pub const fn empty() -> Self {
        Volatiles(0)
    }

    #[inline]
    fn mask(status: VolatileStatus) -> u64 {
        1u64 << (status as u8)
    }

    #[inline]
    pub fn contains(self, status: VolatileStatus) -> bool {
        self.0 & Self::mask(status) != 0
    }

    /// Insert `status`; returns true if it was newly added.
    #[inline]
    pub fn insert(&mut self, status: VolatileStatus) -> bool {
        let was = self.contains(status);
        self.0 |= Self::mask(status);
        !was
    }

    /// Remove `status`; returns true if it was present.
    #[inline]
    pub fn remove(&mut self, status: VolatileStatus) -> bool {
        let was = self.contains(status);
        self.0 &= !Self::mask(status);
        was
    }

    #[inline]
    pub fn clear(&mut self) {
        self.0 = 0;
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}
