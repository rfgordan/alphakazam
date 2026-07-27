//! Type effectiveness, stat computation, and the damage formula.
//!
//! The damage calc here is the standard gen-9 formula with the common multiplicative
//! modifier chain (STAB, type effectiveness, crit, weather, burn). It is exposed as a
//! pure function over explicit inputs so it can be unit-tested in isolation and, later,
//! cross-checked against PS's exact HP deltas once the roll-emit patch lands.

use crate::ids::{MoveCategory, Nature, StatIndex, Type, Weather};

/// Single-type effectiveness multiplier of an attacking type vs one defending type.
/// Encodes the gen-6+ chart; only non-neutral matchups are listed, default `1.0`.
pub fn type_effectiveness(att: Type, def: Type) -> f32 {
    use Type::*;
    if att == None || def == None || att == Stellar || def == Stellar {
        return 1.0;
    }
    match att {
        Normal => match def { Rock | Steel => 0.5, Ghost => 0.0, _ => 1.0 },
        Fire => match def { Grass | Ice | Bug | Steel => 2.0, Fire | Water | Rock | Dragon => 0.5, _ => 1.0 },
        Water => match def { Fire | Ground | Rock => 2.0, Water | Grass | Dragon => 0.5, _ => 1.0 },
        Electric => match def { Water | Flying => 2.0, Electric | Grass | Dragon => 0.5, Ground => 0.0, _ => 1.0 },
        Grass => match def {
            Water | Ground | Rock => 2.0,
            Fire | Grass | Poison | Flying | Bug | Dragon | Steel => 0.5,
            _ => 1.0,
        },
        Ice => match def { Grass | Ground | Flying | Dragon => 2.0, Fire | Water | Ice | Steel => 0.5, _ => 1.0 },
        Fighting => match def {
            Normal | Ice | Rock | Dark | Steel => 2.0,
            Poison | Flying | Psychic | Bug | Fairy => 0.5,
            Ghost => 0.0,
            _ => 1.0,
        },
        Poison => match def { Grass | Fairy => 2.0, Poison | Ground | Rock | Ghost => 0.5, Steel => 0.0, _ => 1.0 },
        Ground => match def {
            Fire | Electric | Poison | Rock | Steel => 2.0,
            Grass | Bug => 0.5,
            Flying => 0.0,
            _ => 1.0,
        },
        Flying => match def { Grass | Fighting | Bug => 2.0, Electric | Rock | Steel => 0.5, _ => 1.0 },
        Psychic => match def { Fighting | Poison => 2.0, Psychic | Steel => 0.5, Dark => 0.0, _ => 1.0 },
        Bug => match def {
            Grass | Psychic | Dark => 2.0,
            Fire | Fighting | Poison | Flying | Ghost | Steel | Fairy => 0.5,
            _ => 1.0,
        },
        Rock => match def { Fire | Ice | Flying | Bug => 2.0, Fighting | Ground | Steel => 0.5, _ => 1.0 },
        Ghost => match def { Psychic | Ghost => 2.0, Dark => 0.5, Normal => 0.0, _ => 1.0 },
        Dragon => match def { Dragon => 2.0, Steel => 0.5, Fairy => 0.0, _ => 1.0 },
        Dark => match def { Psychic | Ghost => 2.0, Fighting | Dark | Fairy => 0.5, _ => 1.0 },
        Steel => match def { Ice | Rock | Fairy => 2.0, Fire | Water | Electric | Steel => 0.5, _ => 1.0 },
        Fairy => match def { Fighting | Dragon | Dark => 2.0, Fire | Poison | Steel => 0.5, _ => 1.0 },
        None | Stellar => 1.0,
    }
}

/// Combined effectiveness vs a (possibly dual-type) defender: product over its types.
pub fn type_multiplier(att: Type, defender_types: [Type; 2]) -> f32 {
    type_effectiveness(att, defender_types[0]) * type_effectiveness(att, defender_types[1])
}

/// Per-defending-type effectiveness with Freeze-Dry's `onEffectiveness` applied.
/// `data/moves.ts` `freezedry`: `onEffectiveness(typeMod, target, type) { if (type === 'Water')
/// return 1; }`. PS's `runEffectiveness` sums a per-type EXPONENT, so returning 1 REPLACES the
/// Ice-vs-Water -1 with +1 — x2 per Water type on the defender, i.e. x4 relative to the plain
/// chart. It is a per-type override, so Water/Ground takes x2 (Water) x 2 (Ground) = x4.
pub fn effectiveness_of(att: Type, def: Type, freeze_dry: bool) -> f32 {
    if freeze_dry && def == Type::Water { 2.0 } else { type_effectiveness(att, def) }
}

/// `type_multiplier` with the Freeze-Dry override.
pub fn type_multiplier_fd(att: Type, defender_types: [Type; 2], freeze_dry: bool) -> f32 {
    effectiveness_of(att, defender_types[0], freeze_dry)
        * effectiveness_of(att, defender_types[1], freeze_dry)
}

/// Nature multiplier (×1.1 / ×0.9 / ×1.0) for a given stat.
pub fn nature_multiplier(nature: Nature, stat: StatIndex) -> f32 {
    let (boosted, lowered) = nature_stats(nature);
    if Some(stat) == boosted {
        1.1
    } else if Some(stat) == lowered {
        0.9
    } else {
        1.0
    }
}

/// (boosted, lowered) stat for a nature; `(None, None)` for neutral natures.
fn nature_stats(nature: Nature) -> (Option<StatIndex>, Option<StatIndex>) {
    use Nature::*;
    use StatIndex::*;
    let map = |b, l| (Some(b), Some(l));
    match nature {
        Lonely => map(Attack, Defense),
        Brave => map(Attack, Speed),
        Adamant => map(Attack, SpecialAttack),
        Naughty => map(Attack, SpecialDefense),
        Bold => map(Defense, Attack),
        Relaxed => map(Defense, Speed),
        Impish => map(Defense, SpecialAttack),
        Lax => map(Defense, SpecialDefense),
        Timid => map(Speed, Attack),
        Hasty => map(Speed, Defense),
        Jolly => map(Speed, SpecialAttack),
        Naive => map(Speed, SpecialDefense),
        Modest => map(SpecialAttack, Attack),
        Mild => map(SpecialAttack, Defense),
        Quiet => map(SpecialAttack, Speed),
        Rash => map(SpecialAttack, SpecialDefense),
        Calm => map(SpecialDefense, Attack),
        Gentle => map(SpecialDefense, Defense),
        Sassy => map(SpecialDefense, Speed),
        Careful => map(SpecialDefense, SpecialAttack),
        Hardy | Docile | Serious | Bashful | Quirky => (Option::None, Option::None),
    }
}

/// Final non-HP stat: `floor((floor((2*base + iv + ev/4) * level / 100) + 5) * nature)`.
pub fn compute_stat(base: u16, iv: u8, ev: u8, level: u8, nature: Nature, stat: StatIndex) -> i16 {
    let inner = (2 * base as i32 + iv as i32 + (ev as i32 / 4)) * level as i32 / 100;
    let with_base = inner + 5;
    let m = nature_multiplier(nature, stat);
    ((with_base as f32) * m).floor() as i16
}

/// Final HP stat: `floor((2*base + iv + ev/4) * level / 100) + level + 10`.
pub fn compute_hp(base: u16, iv: u8, ev: u8, level: u8) -> i16 {
    let inner = (2 * base as i32 + iv as i32 + (ev as i32 / 4)) * level as i32 / 100;
    (inner + level as i32 + 10) as i16
}

/// Inputs to a single damage computation (one hit, before the random roll).
#[derive(Debug, Clone, Copy)]
pub struct DamageInput {
    pub level: u8,
    pub base_power: u16,
    pub category: MoveCategory,
    pub move_type: Type,
    pub attacker_types: [Type; 2],
    /// The attacker's *original* (pre-Tera) types — needed to detect a Tera into one of its
    /// PS `pokemon.getTypes(false, true)` — the attacker's LIVE, pre-terastallized type array
    /// (`Pokemon::live_types`), which a Protean / Soak / Burn Up rewrite and Tera does not.
    /// Only read when `terastallized`; equals `attacker_types` otherwise.
    pub attacker_base_types: [Type; 2],
    pub defender_types: [Type; 2],
    pub attack_stat: i16,
    pub defense_stat: i16,
    pub is_crit: bool,
    pub attacker_burned: bool,
    pub weather: Weather,
    pub terastallized: bool,
    pub tera_type: Type,
    /// Life Orb: ×1.3 final damage (recoil handled by the caller).
    pub life_orb: bool,
    /// Adaptability: STAB becomes ×2 instead of ×1.5.
    pub adaptability: bool,
    /// Tera Shell (Terapagos-Terastal at full HP): one extra resist step (PS onEffectiveness −1).
    pub tera_shell: bool,
    /// Freeze-Dry: its own `onEffectiveness` makes every Water defending type ×2 (see
    /// `effectiveness_of`).
    pub freeze_dry: bool,
    /// A final defender-side damage modifier as a 4096-based ratio (num/den), applied
    /// after type/burn/Life Orb — e.g. Multiscale (×0.5), Filter (×0.75). `(1, 1)` = none.
    pub final_num: i64,
    pub final_den: i64,
}

/// The pre-roll base damage: `floor(floor((2*level/5+2) * power * atk / def) / 50) + 2`.
pub fn base_damage(level: u8, base_power: u16, attack: i16, defense: i16) -> i32 {
    let level = level as i32;
    let a = (2 * level / 5) + 2;
    let numerator = a * base_power as i32 * attack as i32 / defense.max(1) as i32;
    (numerator / 50) + 2
}

/// The STAB modifier as a 4096-friendly ratio, mirroring `battle-actions.ts:1762-1796`:
///
/// ```text
/// isSTAB = move.forceSTAB || pokemon.hasType(type) || pokemon.getTypes(false, true).includes(type)
/// stab   = isSTAB ? 1.5 : 1
/// if (pokemon.terastallized === type && pokemon.getTypes(false, true).includes(type)) stab = 2
/// stab   = runEvent('ModifySTAB', ...)          // Adaptability
/// ```
///
/// The two type predicates are DIFFERENT for a terastallized Pokemon: `hasType` reads
/// `getTypes()`, which for a Tera'd mon is EXACTLY `[teraType]`, while `getTypes(false, true)`
/// is the pre-Tera list. Base STAB survives Tera through the second predicate — but
/// Adaptability's `onModifySTAB` (`data/abilities.ts:44`) gates on `source.hasType(move.type)`,
/// so after Terastallizing it only fires for moves of the TERA type. A Tera'd Adaptability mon
/// attacking with one of its original (non-Tera) types therefore gets a plain ×1.5.
fn stab_mod(input: &DamageInput) -> (i64, i64) {
    let mt = input.move_type;
    // Typeless moves (Struggle, confusion self-hit) never get STAB — but a mon's `types` array
    // carries `Type::None` in its empty second slot, so guard against `None == None` matching.
    if mt == Type::None {
        return (1, 1);
    }
    // PS `pokemon.hasType(type)`.
    let has_type = if input.terastallized {
        input.tera_type == mt
    } else {
        input.attacker_types.contains(&mt)
    };
    // PS `pokemon.getTypes(false, true)` — the pre-terastallized type list.
    let base_has = if input.terastallized {
        input.attacker_base_types.contains(&mt)
    } else {
        input.attacker_types.contains(&mt)
    };
    if !has_type && !base_has {
        return (1, 1);
    }
    let double = has_type && base_has && input.terastallized; // Tera into an original type
    // Adaptability only applies where PS's `hasType` does.
    match (double, input.adaptability && has_type) {
        (true, true) => (9216, 4096), // ×2.25
        (true, false) => (2, 1),      // ×2
        (false, true) => (2, 1),      // ×2
        (false, false) => (3, 2),     // ×1.5
    }
}

/// PS's `Battle#chainModify` (`sim/battle.ts:2334`): every handler inside ONE `runEvent`
/// accumulates its factor into `this.event.modifier`, and `runEvent` applies the product ONCE
/// at `battle.ts:932` (`relayVar = this.modify(relayVar, this.event.modifier)`). Applying each
/// factor with its own `modify` instead rounds at every step and drifts by 1.
/// `previous` and the result are 4096-scaled; identity is 4096.
pub(crate) fn chain(previous: i64, num: i64, den: i64) -> i64 {
    let next = num * 4096 / den; // trunc, PS `tr(numerator * 4096 / denominator)`
    (previous * next + 2048) >> 12
}

/// PS's `modify` with an already-4096-scaled modifier (the accumulated `event.modifier`).
pub(crate) fn modify_by(value: i64, modifier: i64) -> i64 {
    (value * modifier + 2048 - 1) / 4096
}

/// PS's `modify`: `value * num/den` with 4096-based fixed point and round-half-up.
/// Used for STAB / weather / burn / item modifiers so results match Showdown exactly.
pub(crate) fn modify(value: i64, num: i64, den: i64) -> i64 {
    let modifier = num * 4096 / den; // trunc
    (value * modifier + 2048 - 1) / 4096 // trunc; the +2047 is PS's pokeRound
}

/// All 16 possible damage values (roll 0..=15 → factor (100-roll)/100, i.e. 1.00 down to
/// 0.85), in descending order. Status moves / immunities yield all-zero.
///
/// Mirrors PS's `modifyDamage` order and integer rounding exactly: base → weather → crit
/// → random factor → STAB → type effectiveness (sequential ×2 / floor÷2) → burn → min 1.
pub fn damage_rolls(input: &DamageInput) -> [i16; 16] {
    if input.category == MoveCategory::Status || input.base_power == 0 {
        return [0; 16];
    }
    // Type effectiveness as the two per-type multipliers, applied sequentially below.
    let e0 = effectiveness_of(input.move_type, input.defender_types[0], input.freeze_dry);
    let e1 = effectiveness_of(input.move_type, input.defender_types[1], input.freeze_dry);
    if e0 == 0.0 || e1 == 0.0 {
        return [0; 16];
    }

    let base = base_damage(input.level, input.base_power, input.attack_stat, input.defense_stat) as i64;
    let (stab_num, stab_den) = stab_mod(input);
    let burn = input.attacker_burned && input.category == MoveCategory::Physical;
    let weather = weather_mult(input.move_type, input.weather);

    let mut out = [0i16; 16];
    for roll in 0..16u8 {
        let mut d = base;
        // Weather (×1.5 / ×0.5 via modify).
        if weather > 1.0 {
            d = modify(d, 3, 2);
        } else if weather < 1.0 {
            d = modify(d, 1, 2);
        }
        // Crit (gen6+ ×1.5, truncated).
        if input.is_crit {
            d = d * 3 / 2;
        }
        // Random factor: trunc(d * (100 - roll) / 100).
        d = d * (100 - roll as i64) / 100;
        // STAB (×1.5 base, ×2 Adaptability or Tera-into-own-type, ×2.25 both).
        if (stab_num, stab_den) != (1, 1) {
            d = modify(d, stab_num, stab_den);
        }
        // Type effectiveness: PS applies the *net* exponent — ×2 for each super-effective
        // type, floor(÷2) for each resisted — all the ×2 first, then the ÷2. Applying each
        // type sequentially (halve-then-double) mis-rounds by 1 when they cancel, e.g. Bug
        // (×0.5 vs Poison, ×2 vs Psychic) vs Slowking-Galar = ×1, not ×1 − 1.
        let up = (e0 == 2.0) as i32 + (e1 == 2.0) as i32;
        let down = (e0 == 0.5) as i32 + (e1 == 0.5) as i32 + (input.tera_shell as i32);
        let net = up - down;
        if net > 0 {
            d *= 1 << net;
        } else if net < 0 {
            for _ in 0..(-net) {
                d /= 2;
            }
        }
        // Burn (×0.5 via modify).
        if burn {
            d = modify(d, 1, 2);
        }
        // Life Orb (×1.3 = 5324/4096, a "final" modifier after type/burn).
        if input.life_orb {
            d = modify(d, 5324, 4096);
        }
        // Final defender modifier (Multiscale, Filter, ...).
        d = modify(d, input.final_num, input.final_den);
        out[roll as usize] = d.max(1) as i16;
    }
    out
}

fn weather_mult(move_type: Type, weather: Weather) -> f32 {
    match (move_type, weather) {
        (Type::Fire, Weather::Sun) | (Type::Water, Weather::Rain) => 1.5,
        (Type::Fire, Weather::Rain) | (Type::Water, Weather::Sun) => 0.5,
        _ => 1.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::Type::*;

    #[test]
    fn type_chart_known_matchups() {
        assert_eq!(type_effectiveness(Ground, Steel), 2.0);
        assert_eq!(type_effectiveness(Ground, Flying), 0.0);
        assert_eq!(type_effectiveness(Electric, Ground), 0.0);
        assert_eq!(type_effectiveness(Water, Fire), 2.0);
        assert_eq!(type_effectiveness(Fire, Water), 0.5);
        assert_eq!(type_effectiveness(Normal, Ghost), 0.0);
        assert_eq!(type_effectiveness(Ghost, Normal), 0.0);
        assert_eq!(type_effectiveness(Fighting, Dark), 2.0);
        assert_eq!(type_effectiveness(Dragon, Fairy), 0.0);
        assert_eq!(type_effectiveness(Fairy, Dragon), 2.0);
        assert_eq!(type_effectiveness(Ice, Dragon), 2.0);
        assert_eq!(type_effectiveness(Steel, Fairy), 2.0);
        assert_eq!(type_effectiveness(Normal, Normal), 1.0);
    }

    #[test]
    fn dual_type_multiplier_stacks() {
        // Ice vs Landorus (Ground/Flying) = 2 * 2 = 4x.
        assert_eq!(type_multiplier(Ice, [Ground, Flying]), 4.0);
        // Ground vs a Bug/Steel = 0.5 * 2 = 1x.
        assert_eq!(type_multiplier(Ground, [Bug, Steel]), 1.0);
        // Second type None is neutral.
        assert_eq!(type_multiplier(Water, [Fire, None]), 2.0);
    }

    #[test]
    fn base_damage_formula() {
        // level 100, BP 100, atk 300, def 200:
        // a = 42; num = 42*100*300/200 = 6300; 6300/50 + 2 = 128.
        assert_eq!(base_damage(100, 100, 300, 200), 128);
    }

    #[test]
    fn stab_and_neutral_max_roll() {
        // Ground move from a Ground-typed attacker (STAB 1.5), neutral target.
        let input = DamageInput {
            level: 100,
            base_power: 100,
            category: MoveCategory::Physical,
            move_type: Ground,
            attacker_types: [Ground, Fighting],
            attacker_base_types: [Ground, Fighting],
            defender_types: [Normal, None],
            attack_stat: 300,
            defense_stat: 200,
            is_crit: false,
            attacker_burned: false,
            weather: Weather::None,
            terastallized: false,
            tera_type: None,
            life_orb: false,
            adaptability: false,
            tera_shell: false,
            freeze_dry: false,
            final_num: 1,
            final_den: 1,
        };
        let rolls = damage_rolls(&input);
        // Max roll = floor(128 * 1.0) * 1.5 = 192. Min roll = floor(128*0.85)=108 *1.5=162.
        assert_eq!(rolls[0], 192);
        assert_eq!(rolls[15], 162);
        // Rolls are non-increasing.
        for w in rolls.windows(2) {
            assert!(w[0] >= w[1]);
        }
    }

    #[test]
    fn stat_and_hp_computation() {
        // Great Tusk base 87 Spe, 252 EV, 31 IV, Jolly (+Spe), level 100.
        let spe = compute_stat(87, 31, 252, 100, Nature::Jolly, StatIndex::Speed);
        assert_eq!(spe, 300);
        // Great Tusk base 115 HP, 0 EV, 31 IV, level 100 — matches the maxHp:371 PS trace.
        let hp = compute_hp(115, 31, 0, 100);
        assert_eq!(hp, 371);
        // Cross-check the formula against a well-known value: Garchomp base 102 Spe 252+.
        assert_eq!(compute_stat(102, 31, 252, 100, Nature::Jolly, StatIndex::Speed), 333);
    }
}
