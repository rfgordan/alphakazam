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

/// What the *opponent* has learned about this Pokémon — the engine's hidden-information layer.
///
/// Pokémon is a public-information game: a reveal (move used, item triggered, ability fired,
/// Terastallization) is seen by both players, and each player always knows its own side. So a
/// single per-Pokémon mask suffices — it records what the *foe* knows about this mon.
///
/// Crucially this is **never read by the transition** (the simulator always runs on full ground
/// truth — that's what keeps turn speed). It is only consulted by [`State::observe`] to produce a
/// fog-of-war view, and it is written with cheap bitwise ORs at the moment of first reveal via the
/// reversible [`Instruction::Reveal`]. Two bytes; off the hot path entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Reveal {
    /// Bit `i` set ⇒ move slot `i` has been used in front of the foe.
    pub moves: u8,
    /// Disjoint one-shot reveals (`Reveal::SPECIES | ITEM | ABILITY | TERA`).
    pub flags: u8,
}

impl Reveal {
    pub const SPECIES: u8 = 1 << 0;
    pub const ITEM: u8 = 1 << 1;
    pub const ABILITY: u8 = 1 << 2;
    pub const TERA: u8 = 1 << 3;

    #[inline]
    pub fn move_seen(&self, slot: u8) -> bool {
        self.moves & (1 << slot) != 0
    }
    #[inline]
    pub fn has(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }
}

/// A single Pokémon. Stats are the final computed values (after nature/EV/IV), so the
/// hot path never recomputes them; `nature`/`evs` are retained for reference and for
/// abilities that recompute (e.g. Protosynthesis picks the highest stat).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pokemon {
    pub species: Species,
    pub level: u8,

    /// Current EFFECTIVE typing — PS's `pokemon.getTypes()`. Tera is folded in (a
    /// terastallized mon's `types` is `[tera_type]`) and Roost's Flying strip is applied,
    /// because PS resolves both at lookup time and the engine stores the resolved value.
    pub types: [Type; 2],
    /// PS's `pokemon.types` VERBATIM — the live, PRE-TERASTALLIZED type list that Protean /
    /// Soak / Burn Up / Reflect Type / Transform / a forme change rewrite. Tera does NOT touch
    /// it (`getTypes` short-circuits on `terastallized` before reading it) and neither does
    /// Roost (whose `onType` filters `getTypes()`, not the array). It is the state PS's
    /// `isSTAB` reads through `getTypes(false, true)` (`sim/battle-actions.ts:1768`), and it is
    /// the field the digest / state diff compare — `types` is derivable from it plus
    /// `terastallized` / `tera_type` / the `Roosted` marker.
    pub live_types: [Type; 2],
    /// The SPECIES' typing (PS's `pokemon.baseTypes`) — what `clearVolatile`'s
    /// `setSpecies(baseSpecies)` restores `live_types` to on switch-out / faint.
    pub base_types: [Type; 2],
    /// Transform bookkeeping: `transformed` marks an active Transform/Imposter copy; the
    /// `base_*` fields hold what to restore when it switches out (PS reverts transform on
    /// switch). When not transformed they mirror the current values and are never read.
    pub transformed: bool,
    /// This mon's current Sleep was inflicted by the opponent (drives Sleep Clause Mod;
    /// Rest sleep doesn't count). Only meaningful while status == Sleep.
    pub slept_by_foe: bool,
    /// The berry this mon ate (for Harvest regrowth). `Item::None` = none eaten.
    pub last_berry: Item,
    /// Cud Chew (Tauros-Paldea): turns until the eaten berry's effect is re-applied. PS stores
    /// `abilityState.counter` (2 on eat, ticked each end-of-turn); 0 = no pending re-eat.
    pub cudchew_turns: u8,
    /// Gender (0 = genderless, 1 = M, 2 = F). Static — no instruction changes it; drives
    /// Attract / Cute Charm legality (opposite genders only).
    pub gender: u8,
    pub base_species: Species,
    pub base_stats: [i16; StatIndex::COUNT],
    pub base_moves: [MoveSlot; 4],

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

    // --- battle-long, per-mon history (persists across switches) ---
    /// A once-per-battle ability (gen9 Intrepid Sword / Dauntless Shield) has already fired.
    pub ability_used: bool,
    /// How many times this Pokémon has been hit by a damaging move this battle (Rage Fist).
    pub times_hit: u8,

    /// What the opponent has learned about this mon (hidden-information layer). Not read by the
    /// transition; consulted only by [`State::observe`]. See [`Reveal`].
    pub reveal: Reveal,

    /// Illusion (`data/abilities.ts:2010`): the CANONICAL PARTY SLOT of the party member this mon
    /// is currently disguised as, i.e. PS's `pokemon.illusion` pointer. `None` = not disguised.
    ///
    /// Chosen at `onBeforeSwitchIn` as the LAST able member of PS's live `side.pokemon` array
    /// (see [`Side::roster`]) — so it is a function of the array ORDER, not of party slots. It is
    /// per-MON state that survives a switch-out (PS's `onEnd` bails on `beingCalledBack`, which is
    /// always true there) and is cleared by a faint, by re-entry, and by the visible break.
    pub illusion: Option<u8>,
}

impl Pokemon {
    pub const EMPTY: Pokemon = Pokemon {
        species: Species::None,
        level: 100,
        types: [Type::None, Type::None],
        live_types: [Type::None, Type::None],
        base_types: [Type::None, Type::None],
        transformed: false,
        slept_by_foe: false,
        last_berry: Item::None,
        cudchew_turns: 0,
        gender: 0,
        base_species: Species::None,
        base_stats: [0; StatIndex::COUNT],
        base_moves: [MoveSlot::EMPTY; 4],
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
        ability_used: false,
        times_hit: 0,
        reveal: Reveal { moves: 0, flags: 0 },
        illusion: None,
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

/// A multi-turn move the active Pokémon is committed to. Mutually exclusive — a mon is in
/// at most one of these at a time — so a single field captures charge moves, semi-invulnerable
/// moves, rampages, and recharges. Resets on switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PendingMove {
    /// Free to choose and act normally.
    #[default]
    None,
    /// A two-turn move has finished charging; *next* turn it strikes. Covers charge moves
    /// (Solar Beam, Sky Attack, Meteor Beam, …) and semi-invulnerable moves (Fly, Dig, Dive,
    /// Phantom Force, Bounce) — whether the user is untargetable while charging is derived
    /// from the move's data, not stored here.
    Charging(MoveId),
    /// Locked into a rampage move for this many more turns, then self-confuse on expiry
    /// (Outrage, Petal Dance, Thrash).
    Rampaging(MoveId, u8),
    /// Just spent a recharge move (Hyper Beam, Giga Impact, …); the next turn is forfeited.
    Recharging,
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

    /// PS's LIVE `side.pokemon` array, as canonical party slots (`roster[i]` is the canonical slot
    /// of the mon PS keeps at array index `i`). The engine's own slots are fixed; PS's are not —
    /// `switchIn` SWAPS the outgoing and incoming entries (`sim/battle-actions.ts:118-131`), so
    /// after the first switch `roster[0]` is always the active in singles.
    ///
    /// Two mechanics read the array ORDER rather than the party: Beat Up's participant sequence and
    /// **Illusion's disguise target** (the last able entry). Identity at battle start, because the
    /// canonical slot IS the battle-start array index (the recorder's `rosterIndex`).
    pub roster: [u8; 6],

    /// Stat stages of the active Pokémon, indexed by `BoostIndex` (-6..=6).
    pub boosts: [i8; BoostIndex::COUNT],

    /// Volatile statuses of the active Pokémon (bitset).
    pub volatiles: Volatiles,
    pub substitute_hp: i16,

    /// Partial-trap (Fire Spin / Bind / Infestation / …) payload for the active mon, paired with
    /// the `PartiallyTrapped` volatile bit. `partial_trap_turns` is PS's remaining `duration`
    /// (ticked down each end of turn; 0 = inactive). `partial_trap_div` is the snapshotted
    /// `boundDivisor` (8 normally, 6 with the trapper holding Binding Band).
    pub partial_trap_turns: u8,
    pub partial_trap_div: u8,

    // --- multi-turn state of the active Pokémon (all reset on switch) ---
    /// The multi-turn move the active is locked into, if any (see [`PendingMove`]).
    pub pending_move: PendingMove,
    /// Move-choice restrictions: `(move, turns left)`; `(MoveId::None, 0)` = inactive.
    pub encore: (MoveId, u8),
    pub disable: (MoveId, u8),
    /// Turn countdowns (0 = inactive). `taunt`: no status moves; `confusion`: may hit self;
    /// `perish`: faints when it ticks past 0; `yawn`: falls asleep when it reaches 0.
    pub taunt_turns: u8,
    pub confusion_turns: u8,
    pub perish_turns: u8,
    pub yawn_turns: u8,
    /// Throat Chop: sound moves unusable while > 0. Heal Block: all healing blocked while > 0.
    pub throat_chop_turns: u8,
    pub heal_block_turns: u8,
    /// Turns the current active Pokémon has been out (0 the turn it switches in). Drives
    /// first-turn moves (Fake Out, First Impression) and Slow Start. Resets on switch.
    pub active_turns: u8,

    pub side_conditions: SideConditions,

    /// The active Pokémon's last *executed* move, and how many turns in a row it has been
    /// used. Reset on switch. Drives consecutive-use mechanics (Fury Cutter / Echoed Voice
    /// power ramp; Encore/Disable/Torment/Stomping Tantrum read `last_used_move`).
    pub last_used_move: MoveId,
    /// The active mon's move LAST TURN failed (missed/immune/no effect) — PS `moveLastTurnResult
    /// === false`, which doubles Stomping Tantrum. Reconstructed from the serialized `lastMove`.
    pub last_move_failed: bool,
    pub move_streak: u8,
    /// Consecutive successful Protect-family uses (the "stall" counter). Each consecutive
    /// Protect succeeds with probability 1/3^n; reset to 0 by any non-Protect action or a
    /// failed Protect. Reset on switch.
    pub stall_counter: u8,
    /// Remaining duration of PS's `stall` volatile (duration 2, applied by a Protect-family use).
    /// Tracked SEPARATELY from `stall_counter`: PS registers a Residual handler for the `stall`
    /// volatile for as long as the volatile exists (its `getKey:'duration'` entry), which survives
    /// ONE turn past the Protect (duration 2 → present through the next turn's residual even if the
    /// holder used a non-Protect move that turn). `stall_counter` is the `onStallMove` success
    /// denominator (3^n) and resets on any non-Protect action; it is the wrong signal for the
    /// residual-handler-list length. Not compared by `diff_states` (engine-internal; PS carries it
    /// as the volatile's `duration`). Reset on switch.
    pub stall_turns: u8,

    /// This side has used its once-per-battle Terastallization. Derivable from
    /// `pokemon[].terastallized` during engine-only play, but stored explicitly because the
    /// flag is unrecoverable from a PS snapshot once the tera'd mon faints (and the RL agent
    /// needs it as a feature).
    pub tera_used: bool,

    /// Healing Wish / Lunar Dance pending on this side: the next damaged-or-statused mon to
    /// switch in is fully healed and cured (the wish persists until consumed).
    pub healing_wish: bool,
    /// Wish: (turns remaining, heal amount). turns == 0 means inactive.
    pub wish: (u8, i16),
    /// Future Sight: (turns remaining, source party slot). turns == 0 means inactive.
    pub future_sight: (u8, u8),
    /// Damage the active took from the FOE's most recent Special-category damaging hit THIS
    /// turn (Mirror Coat reflects 2×). Transient within a turn — set when a special move damages
    /// this side's active, read by Mirror Coat (priority -5, so it moves after the hit), and
    /// cleared at end of turn. Not part of the diffed manifest (mirrors PS's duration-1 volatile).
    pub special_damage_taken: i16,
    /// Damage the active took from the FOE's most recent Physical-category damaging hit THIS
    /// turn. Same transient lifetime as `special_damage_taken`; read by Focus Punch (fails if the
    /// user was hit by any damaging move this turn) and cleared at end of turn.
    pub physical_damage_taken: i16,
    /// Magnet Rise: turns remaining of Ground immunity / ungrounding (5 on use, ticks each end of
    /// turn; 0 = inactive). The `MagnetRise` volatile marks presence.
    pub magnet_rise_turns: u8,
}

impl Side {
    pub const EMPTY: Side = Side {
        pokemon: [Pokemon::EMPTY; 6],
        active_index: 0,
        roster: [0, 1, 2, 3, 4, 5],
        boosts: [0; BoostIndex::COUNT],
        volatiles: Volatiles::empty(),
        substitute_hp: 0,
        partial_trap_turns: 0,
        partial_trap_div: 0,
        pending_move: PendingMove::None,
        encore: (MoveId::None, 0),
        disable: (MoveId::None, 0),
        taunt_turns: 0,
        confusion_turns: 0,
        perish_turns: 0,
        yawn_turns: 0,
        throat_chop_turns: 0,
        heal_block_turns: 0,
        active_turns: 0,
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
        last_used_move: MoveId::None,
        last_move_failed: false,
        move_streak: 0,
        stall_counter: 0,
        stall_turns: 0,
        tera_used: false,
        healing_wish: false,
        wish: (0, 0),
        future_sight: (0, 0),
        special_damage_taken: 0,
        physical_damage_taken: 0,
        magnet_rise_turns: 0,
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

    /// The party slot the OPPONENT sees in `slot` — the Illusion target when disguised, else
    /// `slot` itself. Every protocol ident/details field and PS's `(source.illusion || source)`
    /// reads go through this; nothing mechanical does.
    #[inline]
    pub fn apparent_slot(&self, slot: u8) -> u8 {
        self.pokemon[slot as usize].illusion.unwrap_or(slot)
    }

    /// The mon the opponent sees in the active slot (the Illusion target while disguised).
    #[inline]
    pub fn apparent_active(&self) -> &Pokemon {
        &self.pokemon[self.apparent_slot(self.active_index) as usize]
    }

    /// PS's Illusion choice for a mon that is ENTERING at array position 0: the last able entry of
    /// the live array strictly after it (`data/abilities.ts:2011-2023`, run after `switchIn`'s
    /// swap, so `pokemon.position` is already 0). `None` when nothing able is behind it, and
    /// `None` for the entry itself (an Illusion mon never disguises as itself).
    ///
    /// `roster_after` is the array as it will be once the swap lands.
    pub fn illusion_target(&self, roster_after: [u8; 6], entering: u8) -> Option<u8> {
        let pos = roster_after.iter().position(|&s| s == entering)?;
        for i in (pos + 1..6).rev() {
            let cand = roster_after[i];
            let p = &self.pokemon[cand as usize];
            if p.species == crate::ids::Species::None {
                continue; // beyond PS's array length — not a party member at all
            }
            // PS's `break` sits INSIDE the `!fainted` arm, so a fainted entry is skipped and the
            // scan continues; the first able entry from the back ends it either way.
            if !p.is_alive() {
                continue;
            }
            // "If Ogerpon is in the last slot while the Illusion Pokemon is Terastallized,
            // Illusion will not disguise as anything" — the assignment is skipped but the `break`
            // still fires, so the scan does NOT fall through to an earlier candidate.
            let base = p.base_species.to_id();
            if self.pokemon[entering as usize].terastallized
                && (base.starts_with("ogerpon") || base.starts_with("terapagos"))
            {
                return None;
            }
            return Some(cand);
        }
        None
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
    /// Format rules — see [`crate::ruleset::Ruleset`]. Set once at battle init, never mutated.
    /// Battle CONFIGURATION, not battle state: it is deliberately absent from the field manifest
    /// `cosim::diff` / `cosim::digest` walk.
    pub ruleset: crate::ruleset::Ruleset,
    pub sides: [Side; 2],

    pub weather: Weather,
    pub weather_turns: i8,
    pub terrain: Terrain,
    pub terrain_turns: i8,
    pub trick_room: bool,
    pub trick_room_turns: i8,

    pub turn: u32,

    /// Battle CONFIGURATION like `ruleset` (not state; absent from the diff/digest manifests):
    /// when set, [`State::observe`] blanks foe party members whose `Reveal::SPECIES` bit is
    /// unset — real fog of war. Off by default because every checkpoint trained before
    /// 2026-07-28 learned on observations that (incorrectly) showed the full enemy roster;
    /// flipping this under a live run would shift its input distribution. New runs opt in.
    pub fog_species: bool,
}

impl State {
    pub const EMPTY: State = State {
        // The corpus default. Every committed trace/fixture and every engine unit test was
        // recorded under `[Gen 9] Custom Game`; a state that never says otherwise gets it.
        // (The old `sleep_clause: true` default was the OPPOSITE of what the corpus needed and
        // was overwritten at every cosim entry point — see RULESET_SPEC.md H2.)
        ruleset: crate::ruleset::Ruleset::GEN9_CUSTOM_GAME,
        sides: [Side::EMPTY; 2],
        weather: Weather::None,
        weather_turns: 0,
        terrain: Terrain::None,
        terrain_turns: 0,
        trick_room: false,
        trick_room_turns: 0,
        turn: 0,
        fog_species: false,
    };

    #[inline]
    pub fn side(&self, id: SideId) -> &Side {
        &self.sides[id.index()]
    }

    #[inline]
    pub fn side_mut(&mut self, id: SideId) -> &mut Side {
        &mut self.sides[id.index()]
    }

    /// Fog-of-war view from `viewer`'s perspective: the viewer's own side verbatim, the foe's
    /// side with everything `viewer` hasn't observed collapsed to sentinels. Returned as a full
    /// `State` (still `Copy`) so it shares the encoder/transition path; an agent acting under
    /// hidden information samples concrete states from a prior consistent with this view
    /// (determinization), then runs the perfect-information engine on each.
    ///
    /// Public fields (boosts, volatiles, hazards, HP, status, active index, terastallized) stay
    /// visible — they're announced in the battle log. Hidden until the relevant [`Reveal`] bit is
    /// set: held item, ability, unused move slots, Tera type, and the spread (EVs/nature). Base
    /// stats are left intact (they follow from the public species); EVs/nature are zeroed because
    /// the exact spread is only ever *inferred* from observed damage, never announced.
    ///
    /// **Illusion is masked here, not by the reveal bits.** A disguised foe announced itself as
    /// somebody else in the protocol (`|switch|p1a: Ninetales|…`), so the viewer's whole picture of
    /// the active slot — species, forme, level, gender, typing, stats — is the DISGUISE's. That
    /// identity block is substituted into the observed active slot; HP / status / boosts /
    /// volatiles are the real mon's, because those are exactly the things the log reports
    /// truthfully about the slot regardless of who is standing in it. The `illusion` pointer itself
    /// is scrubbed: knowing a disguise is up is knowing it is a disguise.
    ///
    /// Note what is deliberately NOT hidden: the disguise target keeps its own party entry, so the
    /// observed foe roster shows that species twice. That matches this model's standing assumption
    /// that the foe's ROSTER is public (randbats team preview / open sheets) while the identity of
    /// the mon on the field is not.
    ///
    /// This is the only place the reveal layer is read, and it is off the hot path — call it when
    /// feeding an agent or logging a position, not inside the per-turn transition.
    /// Mark both starting actives as species-revealed (battle-init bookkeeping — leads are
    /// visible from turn 0; every later entrance reveals via `Instruction::Reveal`).
    pub fn reveal_leads(&mut self) {
        for s in [SideId::One, SideId::Two] {
            let sd = self.side_mut(s);
            let idx = sd.active_index as usize;
            if idx < 6 {
                sd.pokemon[idx].reveal.flags |= Reveal::SPECIES;
            }
        }
    }

    pub fn observe(&self, viewer: SideId) -> State {
        use crate::ids::{Ability, Item, Type};
        let mut obs = *self;
        let foe = obs.side_mut(viewer.other());
        for slot in 0..6usize {
            let Some(shown) = foe.pokemon[slot].illusion else { continue };
            let d = foe.pokemon[shown as usize];
            let p = &mut foe.pokemon[slot];
            p.species = d.species;
            p.base_species = d.base_species;
            p.level = d.level;
            p.gender = d.gender;
            p.types = d.types;
            p.live_types = d.live_types;
            p.base_types = d.base_types;
            p.stats = d.stats;
            p.base_stats = d.base_stats;
            p.illusion = None;
        }
        for p in foe.pokemon.iter_mut() {
            if p.species == crate::ids::Species::None {
                continue;
            }
            let r = p.reveal;
            if self.fog_species && !r.has(Reveal::SPECIES) {
                // Never sent out: the viewer knows only that a healthy, unknown party member
                // exists. hp/max_hp = 1/1 keeps the "healthy" fraction; everything else blanks.
                let mut q = Pokemon::EMPTY;
                q.hp = 1;
                q.max_hp = 1;
                *p = q;
                continue;
            }
            if !r.has(Reveal::ITEM) {
                p.item = Item::Unknown;
            }
            if !r.has(Reveal::ABILITY) {
                p.ability = Ability::Unknown;
                p.base_ability = Ability::Unknown;
            }
            if !r.has(Reveal::TERA) {
                p.tera_type = Type::None;
            }
            for i in 0..4u8 {
                if !r.move_seen(i) {
                    p.moves[i as usize] = MoveSlot::EMPTY;
                }
            }
            // The spread is hidden; an agent samples it during determinization. Base stats stay
            // (they're implied by the species and bound the observed damage rolls).
            p.evs = [0; StatIndex::COUNT];
            p.nature = Nature::Serious;
            if self.fog_species {
                // HONEST MODE (the ladder information set — see the scramble-invariance test):
                //  * computed stats are re-derived from PUBLIC info (base stats + level, the
                //    standard randbats 85 EV / 31 IV / neutral spread) — the true spread's
                //    exact stats must not leak. Randbats spreads are near-uniform, so the
                //    estimate is usually exact anyway; the point is provenance, not accuracy.
                //  * sleep-turns-remaining is hidden in PS (rolled server-side); toxic stage is
                //    public arithmetic. Zero the counter only for sleep.
                for s in [StatIndex::Attack, StatIndex::Defense, StatIndex::SpecialAttack,
                          StatIndex::SpecialDefense, StatIndex::Speed] {
                    let idx = s as usize;
                    p.stats[idx] = crate::damage::compute_stat(
                        p.base_stats[idx] as u16, 31, 85, p.level, Nature::Serious, s);
                }
                if p.status == Status::Sleep {
                    p.status_counter = 0;
                }
                // Raw HP totals are hidden (PS shows the foe's HP as a percentage): re-derive
                // max HP from public info and rescale current HP to preserve the fraction.
                if p.max_hp > 0 {
                    let frac = p.hp.max(0) as f32 / p.max_hp as f32;
                    let est = crate::damage::compute_hp(p.base_stats[0] as u16, 31, 85, p.level);
                    p.max_hp = est;
                    p.hp = (frac * est as f32).round() as i16;
                }
            }
        }
        if self.fog_species {
            // Substitute presence is public; its remaining HP is not.
            let foe_side = obs.side_mut(viewer.other());
            if foe_side.substitute_hp > 0 {
                foe_side.substitute_hp = i16::MAX; // encoder clamps: reads as "sub up, HP unknown"
            }
        }
        obs
    }
}

impl Default for State {
    fn default() -> Self {
        State::EMPTY
    }
}
