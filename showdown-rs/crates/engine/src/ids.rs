//! Integer-backed identifier enums for the engine.
//!
//! Everything in the hot path is keyed by these small `#[repr]` enums rather than
//! strings, so a `State` stays flat and cheap to copy. The Species / Move / Ability /
//! Item sets below are an intentionally small starter slice — they are meant to be
//! regenerated from Pokémon Showdown's data files (see `data.rs`) as coverage grows.

/// Elemental type. `None` is the "second type" of a mono-type Pokémon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Type {
    None = 0,
    Normal,
    Fire,
    Water,
    Electric,
    Grass,
    Ice,
    Fighting,
    Poison,
    Ground,
    Flying,
    Psychic,
    Bug,
    Rock,
    Ghost,
    Dragon,
    Dark,
    Steel,
    Fairy,
    Stellar,
}

impl Type {
    /// Total number of "real" elemental types (excludes `None`/`Stellar` bookkeeping).
    pub const COUNT: usize = 18;
}

/// Non-volatile status. Stored on the Pokémon; only one at a time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Status {
    None = 0,
    Burn,
    Paralysis,
    Sleep,
    Freeze,
    Poison,
    Toxic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Weather {
    None = 0,
    Sun,
    Rain,
    Sand,
    Snow,
    HarshSun,
    HeavyRain,
    StrongWinds,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Terrain {
    None = 0,
    Electric,
    Grassy,
    Misty,
    Psychic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MoveCategory {
    Physical = 0,
    Special,
    Status,
}

/// Index into the boost / stat-stage array on a `Side`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum BoostIndex {
    Attack = 0,
    Defense,
    SpecialAttack,
    SpecialDefense,
    Speed,
    Accuracy,
    Evasion,
}

impl BoostIndex {
    pub const COUNT: usize = 7;
}

/// Permanent stat slots (no accuracy/evasion — those only exist as boosts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum StatIndex {
    Hp = 0,
    Attack,
    Defense,
    SpecialAttack,
    SpecialDefense,
    Speed,
}

impl StatIndex {
    pub const COUNT: usize = 6;
}

/// A species, addressed by a dense u16 id into the generated tables (`gen.rs`).
/// `Species::None` is the empty sentinel (id 0). Look up data via `data::base_stats`,
/// names via `from_id`/`to_id` (see `names.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Species(pub u16);

impl Species {
    #[allow(non_upper_case_globals)]
    pub const None: Species = Species(0);
}

/// A move, addressed by a dense u16 id into the generated tables (`gen.rs`). Look up the
/// full record via `data::move_data`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MoveId(pub u16);

impl MoveId {
    #[allow(non_upper_case_globals)]
    pub const None: MoveId = MoveId(0);
}

/// Starter ability slice. Regenerate from PS `data/abilities.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Ability {
    None = 0,
    Protosynthesis,
    GoodAsGold,
    ClearBody,
    PurifyingSalt,
    SupremeOverlord,
    IntimidateAbility,
    ToxicDebris,
    Pressure,
    Regenerator,
    QuarkDrive,
    Drought,
    Drizzle,
    SandStream,
    SnowWarning,
    Levitate,
    // Offensive multipliers
    HugePower,
    PurePower,
    Guts,
    Technician,
    Adaptability,
    // Defensive multipliers
    ThickFat,
    Multiscale,
    ShadowShield,
    IceScales,
    Filter,
    SolidRock,
    PrismArmor,
    Sturdy,
    // Type-immunity (absorbing) abilities
    FlashFire,
    WaterAbsorb,
    DrySkin,
    VoltAbsorb,
    LightningRod,
    MotorDrive,
    StormDrain,
    SapSipper,
    // Pinch (low-HP) STAB boosters
    Overgrow,
    Blaze,
    Torrent,
    Swarm,
    // Other offensive
    SheerForce,
    TintedLens,
    Neuroforce,
    Reckless,
    Defeatist,
    // Defensive
    MarvelScale,
    FurCoat,
    // Flag-keyed offensive
    ToughClaws,
    IronFist,
    StrongJaw,
    Sharpness,
    MegaLauncher,
    PunkRock,
    // Contact punishers / move-flag immunities
    RoughSkin,
    IronBarbs,
    Soundproof,
    Bulletproof,
    // Speed (affect turn order)
    Chlorophyll,
    SwiftSwim,
    SandRush,
    SlushRush,
    QuickFeet,
    Hustle,
    Unaware,
    SereneGrace,
    // React to an opponent lowering a stat
    Defiant,
    Competitive,
    // +1 Atk on KO
    Moxie,
    // +1 priority to status moves
    Prankster,
    // Inverts stat-stage changes on the holder
    Contrary,
    // Contact-triggered status (30%): defender burns/paralyzes/poisons the attacker
    FlameBody,
    Static,
    PoisonPoint,
    // Attacker poisons the target on contact (30%)
    PoisonTouch,
    // End-of-turn residual modifiers
    PoisonHeal,
    MagicGuard,
    // Bypasses screens / Substitute when attacking
    Infiltrator,
    // +1 Atk when hit by a Dark move
    Justified,
    // Drain moves damage the holder's attacker instead of healing
    LiquidOoze,
    // Cures status on switch-out
    NaturalCure,
    // Ignores the defender's damage-affecting abilities when attacking
    MoldBreaker,
    // Type-boosting offensive abilities
    WaterBubble,
    Transistor,
    DragonsMaw,
    RockyPayload,
    Steelworker,
    // Mold Breaker variants (ignore defender abilities)
    Teravolt,
    Turboblaze,
    // On-hit reaction abilities
    WeakArmor,
    Aftermath,
    // Sleep-immunity abilities
    Insomnia,
    VitalSpirit,
    SweetVeil,
}

/// Starter item slice. Regenerate from PS `data/items.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Item {
    None = 0,
    Leftovers,
    ChoiceBand,
    ChoiceScarf,
    ChoiceSpecs,
    HeavyDutyBoots,
    AssaultVest,
    LifeOrb,
    RockyHelmet,
    BoosterEnergy,
    ExpertBelt,
    MuscleBand,
    WiseGlasses,
    FocusSash,
    Eviolite,
    SitrusBerry,
    WhiteHerb,
    WeaknessPolicy,
    ToxicOrb,
    FlameOrb,
    ChestoBerry,
    LumBerry,
    ThroatSpray,
    /// Catch-all for a held item the engine doesn't model — preserves "has an item"
    /// (needed by Knock Off, Acrobatics, etc.) without modeling its effect.
    Other,
}

/// All 25 natures. Each nudges one stat +10% and another -10% (Serious-type = neutral).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Nature {
    Hardy = 0,
    Lonely,
    Brave,
    Adamant,
    Naughty,
    Bold,
    Docile,
    Relaxed,
    Impish,
    Lax,
    Timid,
    Hasty,
    Serious,
    Jolly,
    Naive,
    Modest,
    Mild,
    Quiet,
    Bashful,
    Rash,
    Calm,
    Gentle,
    Sassy,
    Careful,
    Quirky,
}
