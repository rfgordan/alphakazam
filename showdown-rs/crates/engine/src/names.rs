//! String-id ↔ enum mapping, matching Pokémon Showdown's `toID` form (lowercase,
//! alphanumerics only, e.g. `"greattusk"`, `"boosterenergy"`, `"brn"`).
//!
//! These are the bridge between PS traces and the engine's integer enums. For the
//! current id slice they are written by hand; the intent is to regenerate them from
//! PS's data files as coverage grows. `from_id` returns `None` for ids outside the
//! slice so the differential runner can *report* what's unmapped rather than guess.

use crate::ids::{Ability, Item, MoveId, Species, Status, Terrain, Type, Weather};
use crate::volatile::VolatileStatus;

macro_rules! id_map {
    ($ty:ty { $($variant:ident => $s:literal),+ $(,)? }) => {
        impl $ty {
            /// Map a PS `toID` string to this enum, or `None` if unknown to the slice.
            pub fn from_id(s: &str) -> Option<$ty> {
                match s {
                    $($s => Some(<$ty>::$variant),)+
                    _ => None,
                }
            }
            /// The PS `toID` string for this value.
            pub fn to_id(self) -> &'static str {
                match self {
                    $(<$ty>::$variant => $s,)+
                }
            }
        }
    };
}

id_map!(Type {
    None => "none",
    Normal => "normal",
    Fire => "fire",
    Water => "water",
    Electric => "electric",
    Grass => "grass",
    Ice => "ice",
    Fighting => "fighting",
    Poison => "poison",
    Ground => "ground",
    Flying => "flying",
    Psychic => "psychic",
    Bug => "bug",
    Rock => "rock",
    Ghost => "ghost",
    Dragon => "dragon",
    Dark => "dark",
    Steel => "steel",
    Fairy => "fairy",
    Stellar => "stellar",
});

id_map!(Status {
    None => "none",
    Burn => "brn",
    Paralysis => "par",
    Sleep => "slp",
    Freeze => "frz",
    Poison => "psn",
    Toxic => "tox",
});

id_map!(Weather {
    None => "none",
    Sun => "sunnyday",
    Rain => "raindance",
    Sand => "sandstorm",
    Snow => "snowscape",
    HarshSun => "desolateland",
    HeavyRain => "primordialsea",
    StrongWinds => "deltastream",
});

id_map!(Terrain {
    None => "none",
    Electric => "electricterrain",
    Grassy => "grassyterrain",
    Misty => "mistyterrain",
    Psychic => "psychicterrain",
});

/// Binary-search a sorted `(name, id)` table.
fn lookup(table: &[(&str, u16)], s: &str) -> Option<u16> {
    table.binary_search_by(|(n, _)| n.cmp(&s)).ok().map(|i| table[i].1)
}

impl Species {
    /// Map a PS species id to a dense `Species`, or `None` if unknown.
    pub fn from_id(s: &str) -> Option<Species> {
        lookup(crate::gen::SPECIES_BY_NAME, s).map(Species)
    }
    /// The PS species id for this value.
    pub fn to_id(self) -> &'static str {
        crate::gen::SPECIES_NAMES.get(self.0 as usize).copied().unwrap_or("none")
    }
}

impl MoveId {
    /// Map a PS move id to a dense `MoveId`, or `None` if unknown.
    pub fn from_id(s: &str) -> Option<MoveId> {
        lookup(crate::gen::MOVE_BY_NAME, s).map(MoveId)
    }
    /// The PS move id for this value.
    pub fn to_id(self) -> &'static str {
        crate::gen::MOVE_NAMES.get(self.0 as usize).copied().unwrap_or("none")
    }
}

id_map!(Ability {
    None => "none",
    Protosynthesis => "protosynthesis",
    GoodAsGold => "goodasgold",
    ClearBody => "clearbody",
    PurifyingSalt => "purifyingsalt",
    SupremeOverlord => "supremeoverlord",
    IntimidateAbility => "intimidate",
    ToxicDebris => "toxicdebris",
    Pressure => "pressure",
    Regenerator => "regenerator",
    QuarkDrive => "quarkdrive",
    Drought => "drought",
    Drizzle => "drizzle",
    SandStream => "sandstream",
    SnowWarning => "snowwarning",
    Levitate => "levitate",
    HugePower => "hugepower",
    PurePower => "purepower",
    Guts => "guts",
    Technician => "technician",
    Adaptability => "adaptability",
    ThickFat => "thickfat",
    Multiscale => "multiscale",
    ShadowShield => "shadowshield",
    IceScales => "icescales",
    Filter => "filter",
    SolidRock => "solidrock",
    PrismArmor => "prismarmor",
    Sturdy => "sturdy",
    FlashFire => "flashfire",
    WaterAbsorb => "waterabsorb",
    DrySkin => "dryskin",
    VoltAbsorb => "voltabsorb",
    LightningRod => "lightningrod",
    MotorDrive => "motordrive",
    StormDrain => "stormdrain",
    SapSipper => "sapsipper",
    Overgrow => "overgrow",
    Blaze => "blaze",
    Torrent => "torrent",
    Swarm => "swarm",
    SheerForce => "sheerforce",
    TintedLens => "tintedlens",
    Neuroforce => "neuroforce",
    Reckless => "reckless",
    Defeatist => "defeatist",
    MarvelScale => "marvelscale",
    FurCoat => "furcoat",
    ToughClaws => "toughclaws",
    IronFist => "ironfist",
    StrongJaw => "strongjaw",
    Sharpness => "sharpness",
    MegaLauncher => "megalauncher",
    PunkRock => "punkrock",
    RoughSkin => "roughskin",
    IronBarbs => "ironbarbs",
    Soundproof => "soundproof",
    Bulletproof => "bulletproof",
    Chlorophyll => "chlorophyll",
    SwiftSwim => "swiftswim",
    SandRush => "sandrush",
    SlushRush => "slushrush",
    QuickFeet => "quickfeet",
    Hustle => "hustle",
    Unaware => "unaware",
    SereneGrace => "serenegrace",
    Defiant => "defiant",
    Competitive => "competitive",
    Moxie => "moxie",
    Prankster => "prankster",
    Contrary => "contrary",
    FlameBody => "flamebody",
    Static => "static",
    PoisonPoint => "poisonpoint",
    PoisonTouch => "poisontouch",
    PoisonHeal => "poisonheal",
    MagicGuard => "magicguard",
    Infiltrator => "infiltrator",
});

id_map!(Item {
    None => "none",
    Leftovers => "leftovers",
    ChoiceBand => "choiceband",
    ChoiceScarf => "choicescarf",
    ChoiceSpecs => "choicespecs",
    HeavyDutyBoots => "heavydutyboots",
    AssaultVest => "assaultvest",
    LifeOrb => "lifeorb",
    RockyHelmet => "rockyhelmet",
    BoosterEnergy => "boosterenergy",
    ExpertBelt => "expertbelt",
    MuscleBand => "muscleband",
    WiseGlasses => "wiseglasses",
    FocusSash => "focussash",
    Eviolite => "eviolite",
    SitrusBerry => "sitrusberry",
    WhiteHerb => "whiteherb",
    WeaknessPolicy => "weaknesspolicy",
    ToxicOrb => "toxicorb",
    FlameOrb => "flameorb",
    ChestoBerry => "chestoberry",
    Other => "other",
});

id_map!(VolatileStatus {
    Confusion => "confusion",
    Substitute => "substitute",
    LeechSeed => "leechseed",
    Taunt => "taunt",
    Encore => "encore",
    Disable => "disable",
    Protect => "protect",
    Endure => "endure",
    Flinch => "flinch",
    Roost => "roost",
    Charge => "charge",
    Yawn => "yawn",
    PerishSong => "perishsong",
    DestinyBond => "destinybond",
    Curse => "curse",
    Nightmare => "nightmare",
    Attract => "attract",
    Torment => "torment",
    SaltCure => "saltcure",
    GlaiveRush => "glaiverush",
    LockedMove => "lockedmove",
    MustRecharge => "mustrecharge",
    PartiallyTrapped => "partiallytrapped",
    Roosted => "roosted",
    ChoiceLock => "choicelock",
    Protosynthesis => "protosynthesis",
    QuarkDrive => "quarkdrive",
});
