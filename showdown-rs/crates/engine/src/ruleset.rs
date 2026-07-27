//! Format rules, made configurable.
//!
//! Everything the sim does differently between `[Gen 9] Custom Game` (the format the entire
//! draw-exact corpus was recorded under) and `[Gen 9] Random Battle` (the real target) is
//! collected here as one small `Copy` struct that is built once at battle init and never
//! mutated. `showdown-rs/RULESET_SPEC.md` is the derivation, with `file:line` citations into
//! the pinned PS tree (`ps.lock`, `b9dc987d`); the short version is:
//!
//! ```text
//! [Gen 9] Random Battle   ruleset: PotD, Obtainable, Species Clause, HP Percentage Mod,
//!                                  Cancel Mod, Sleep Clause Mod, Illusion Level Mod
//! [Gen 9] Custom Game     ruleset: Team Preview, Cancel Mod, Max Team Size = 24,
//!                                  Max Move Count = 24, Max Level = 9999, Default Level = 100
//!                         plus     debug: true,  battle: { trunc: Math.trunc }
//! ```
//!
//! The two format-level fields (`debug`, `battle.trunc`) are NOT ruleset entries but they are
//! format-driven, and they are the two highest-fidelity-risk deltas: `debug` gives customgame
//! exact HP in the shared protocol stream, and `battle.trunc = Math.trunc` **disables** the
//! 13-bit Speed and 16-bit damage truncations that every real format performs.
//!
//! Which of these can move the PRNG stream (see [`Ruleset::sleep_clause`] and
//! [`Ruleset::bit_truncation`]) and which are observation-only is tabulated in
//! `RULESET_SPEC.md` §11.4. Only those two are core.

/// Format-level rules. Constructed once per battle from the format id; never mutated after init.
///
/// Carried on [`crate::state::State`] so the deep interior of `generate.rs` can read it without
/// threading a parameter through 13k lines. It is *not* part of the state manifest that
/// `cosim::diff` / `cosim::digest` compare — those walk an explicit field list and this is not
/// on it, by construction (it is battle configuration, not battle state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ruleset {
    /// PS format id this preset models (`gen9customgame` / `gen9randombattle`).
    pub format_id: &'static str,

    // ---- core mechanics (affect state transitions and/or the PRNG stream) ----
    /// **Sleep Clause Mod** (`data/rulesets.ts:1378`). Blocks a FOE-inflicted `slp` when a living
    /// party member is already foe-slept — at the `SetStatus` event, i.e. **before** the status
    /// condition's `onStart` rolls `random(2,5)` for the duration (`data/conditions.ts:47`).
    /// A blocked sleep therefore consumes **zero** draws at the status site. That is the single
    /// biggest draw-shape difference between the two presets.
    pub sleep_clause: bool,
    /// Endless Battle Clause proper (staleness → forced win/tie). NOT in either preset —
    /// randbats does not list it and customgame has no ruleset. The format-INDEPENDENT part of
    /// `maybeTriggerEndlessBattleClause` (the turn-1000 auto-tie) is not gated on this.
    pub endless_battle_clause: bool,
    /// `format.battle.trunc = Math.trunc` (customgame ONLY) replaces `Dex#trunc`
    /// (`sim/dex.ts:363`, `(num >>> 0) % 2**bits`) with a function that ignores the `bits`
    /// argument. `true` here = PS's real, bit-honouring truncation:
    ///   * `sim/pokemon.ts:649` — `getActionSpeed` returns `trunc(speed, 13)`, so effective Speed
    ///     wraps mod 8192. Turn-order relevant, hence **draw-order relevant**.
    ///   * `sim/pokemon.ts:638` — `getStat('spe')` caps at 10000, but ONLY when
    ///     `!format.battle?.trunc`; customgame skips the cap as well.
    ///   * `sim/battle-actions.ts:1845`/`:1863` — damage is `trunc(baseDamage, 16)`.
    pub bit_truncation: bool,
    /// `sim/battle.ts:1741`: `if ((ruleTable.has('+hackmons') || !ruleTable.has('obtainableabilities'))
    /// && !this.format.team) continue;` — customgame satisfies both halves and SKIPS the
    /// `FoeMaybeTrapPokemon` sweep over each foe's possible abilities; randbats does not
    /// (`obtainableabilities` present AND `format.team === 'random'`). Request-shape only.
    pub infer_foe_trapping_abilities: bool,

    // ---- observation layer (protocol + request JSON only) ----
    /// `format.debug` → `battle.reportExactHP` (`sim/battle.ts:225`): the SHARED (foe-visible)
    /// stream carries `cur/max` instead of `pct/100`. Your own request JSON always carries exact
    /// HP either way (`getHealth().secret`, `sim/pokemon.ts:1158`).
    pub report_exact_hp: bool,
    /// `format.debug` → `battle.debugMode`: `|debug|…` protocol lines and `checkEVBalance()`.
    pub emit_debug_lines: bool,
    /// Illusion Level Mod (`data/rulesets.ts:2916`): a disguised mon's `|switch|` details use the
    /// COPIED mon's level rather than leaking the disguiser's (`sim/pokemon.ts:545`).
    pub illusion_level_mod: bool,
    /// Cancel Mod → `battle.supportCancel`; omits `noCancel` from the request JSON when both
    /// sides have a live request (`sim/battle.ts:1455`). Both presets have it.
    pub cancel_mod: bool,
    /// Team Preview: emit `|clearpoke|`/`|poke|`/`|teampreview|` and issue a teampreview request
    /// as the FIRST decision. `false` ⇒ the first decision is a `move` request and the leads are
    /// `side.pokemon[0]` in generator order (`RULESET_SPEC.md` §5).
    pub team_preview: bool,
    /// The `|rule|` lines emitted from `start()`, in RuleTable order.
    pub rule_lines: &'static [&'static str],

    // ---- scalars from RuleTable::resolveNumbers ----
    pub max_team_size: u8,
    pub max_move_count: u8,
    pub picked_team_size: Option<u8>,
}

impl Ruleset {
    /// `[Gen 9] Random Battle` — `config/formats.ts:28`. The priority target.
    pub const GEN9_RANDOM_BATTLE: Ruleset = Ruleset {
        format_id: "gen9randombattle",
        sleep_clause: true,
        endless_battle_clause: false,
        bit_truncation: true,
        infer_foe_trapping_abilities: true,
        report_exact_hp: false,
        emit_debug_lines: false,
        illusion_level_mod: true,
        cancel_mod: true,
        team_preview: false,
        // PotD is silent (Config.potd defaults to ''), Cancel Mod has no |rule| line.
        rule_lines: &[
            "Species Clause: Limit one of each Pok\u{e9}mon",
            "HP Percentage Mod: HP is shown in percentages",
            "Sleep Clause Mod: Limit one foe put to sleep",
            "Illusion Level Mod: Illusion disguises the Pok\u{e9}mon's true level",
        ],
        max_team_size: 6,
        max_move_count: 4,
        picked_team_size: None,
    };

    /// `[Gen 9] Custom Game` — `config/formats.ts:148`. **Exactly the behaviour every existing
    /// fixture and gate was calibrated on**; this is the default for anything that does not say
    /// otherwise.
    pub const GEN9_CUSTOM_GAME: Ruleset = Ruleset {
        format_id: "gen9customgame",
        sleep_clause: false,
        endless_battle_clause: false,
        bit_truncation: false, // format.battle = { trunc: Math.trunc }
        infer_foe_trapping_abilities: false,
        report_exact_hp: true,  // format.debug
        emit_debug_lines: true, // format.debug
        illusion_level_mod: false,
        cancel_mod: true,
        team_preview: true,
        rule_lines: &[],
        max_team_size: 24,
        max_move_count: 24,
        picked_team_size: None,
    };

    /// Resolve a PS format id to a preset. `None` for anything we have not derived — callers
    /// must fail loudly rather than silently defaulting (`RULESET_SPEC.md` §11.5 assertion b).
    pub fn from_format(id: &str) -> Option<Ruleset> {
        match id {
            "gen9customgame" => Some(Ruleset::GEN9_CUSTOM_GAME),
            "gen9randombattle" => Some(Ruleset::GEN9_RANDOM_BATTLE),
            _ => None,
        }
    }

    /// PS's `Dex#trunc` (`sim/dex.ts:363`) under this ruleset: `(num >>> 0) % 2**bits` when the
    /// format leaves `battle.trunc` alone, and `Math.trunc` (a no-op on an already-integral
    /// value, `bits` ignored) when the format overrides it.
    #[inline]
    pub fn trunc(&self, num: i64, bits: u32) -> i64 {
        if !self.bit_truncation || bits == 0 {
            return num;
        }
        // PS: `num >>> 0` first (ToUint32), then `% 2**bits`.
        ((num as i64 as u32) % (1u32 << bits)) as i64
    }
}

impl Default for Ruleset {
    /// The corpus default. Every committed fixture, trace and unit test was recorded under
    /// customgame, so an unstamped state gets customgame behaviour.
    fn default() -> Self {
        Ruleset::GEN9_CUSTOM_GAME
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunc_is_a_no_op_under_math_trunc() {
        let cg = Ruleset::GEN9_CUSTOM_GAME;
        assert_eq!(cg.trunc(12096, 13), 12096);
        assert_eq!(cg.trunc(100_000, 16), 100_000);
    }

    #[test]
    fn trunc_wraps_under_the_real_dex_trunc() {
        let rb = Ruleset::GEN9_RANDOM_BATTLE;
        assert_eq!(rb.trunc(8191, 13), 8191);
        assert_eq!(rb.trunc(8192, 13), 0);
        assert_eq!(rb.trunc(12096, 13), 12096 - 8192);
        assert_eq!(rb.trunc(10000, 13), 1808);
        assert_eq!(rb.trunc(65536, 16), 0);
        assert_eq!(rb.trunc(70000, 16), 70000 - 65536);
        assert_eq!(rb.trunc(500, 0), 500); // bits == 0 -> plain ToUint32
    }

    #[test]
    fn from_format_is_total_over_the_corpus_formats() {
        assert_eq!(Ruleset::from_format("gen9customgame"), Some(Ruleset::GEN9_CUSTOM_GAME));
        assert_eq!(Ruleset::from_format("gen9randombattle"), Some(Ruleset::GEN9_RANDOM_BATTLE));
        assert_eq!(Ruleset::from_format("gen9ou"), None);
    }
}
