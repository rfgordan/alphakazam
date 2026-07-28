//! Turn resolution: `generate_instructions` produces the set of weighted outcome
//! branches for a pair of chosen actions.
//!
//! Because `State` is `Copy`, each branch simply carries its own state — enumerating
//! the probability tree is a fold over `Branch`es, no mutate/undo bookkeeping needed
//! (the apply/reverse engine in `instruction.rs` is the separate, allocation-free path
//! intended for search/RL rollouts). For *verification* we want the full enumeration so
//! that PS's actual result is guaranteed to be one of the branches we produce.
//!
//! Coverage is the current slice: switching with entry hazards, damaging moves (with
//! accuracy / crit / damage-roll branching and 100%-chance self-stat secondaries),
//! a few status moves, and common end-of-turn residuals. Unmodeled mechanics are listed
//! at each site; the differential runner measures what fraction of real turns this
//! already reproduces.

use crate::damage::{damage_rolls, type_multiplier, DamageInput};
use crate::data::move_data;
use crate::ids::{BoostIndex, Item, MoveCategory, Status, Type, Weather};
use crate::instruction::{Instruction, SideConditionId, StateInstructions};
use crate::state::{SideId, State};
use crate::volatile::VolatileStatus;

/// A chosen action for one side this turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoveChoice {
    Move(u8),   // move slot index 0..=3
    Switch(u8), // party index 0..=5
}

/// A single PRNG draw the engine *would* make, in Pokémon Showdown's call form. Emitted at
/// each stochastic site when draw annotation is enabled (Replicate/DRAW_DIFF). `kind`/`args`
/// mirror PS's `random`/`randomChance`/`sample`/`shuffle`; `result` is the realized outcome
/// under PS's interpretation (bool as 0/1, roll/sample index, etc.; shuffle result is -1 since
/// PS logs shuffle order as null — the ordering is verified via the resulting state instead).
///
/// The draw stream carried by a branch is the ordered list of draws that produced *that*
/// branch's outcome. The consumption differ picks the branch reproducing PS's recorded
/// `stateAfter` and compares this stream against PS's recorded draw log.
#[derive(Clone, Debug, PartialEq)]
pub struct DrawEvent {
    /// "random" | "randomChance" | "sample" | "shuffle".
    pub kind: &'static str,
    /// PS call args: random(n)->[n]; random(m,n)->[m,n]; randomChance(a,d)->[a,d];
    /// sample(len)->[len]; shuffle(len,start,end)->[len,start,end].
    pub args: Vec<i32>,
    /// Outcome under PS's interpretation. randomChance: 0/1. random(n)/random(m,n): the drawn
    /// integer. sample: chosen index. shuffle: -1 (order not in PS's log).
    pub result: i64,
    /// Engine-side site tag, for triage when the recorded label is unavailable.
    pub site: &'static str,
}

thread_local! {
    /// When set, stochastic sites append their [`DrawEvent`] to each branch they produce. Off
    /// by default so `Enumerate`/`Sample` and every existing caller keep identical behavior and
    /// pay zero annotation cost. Set/cleared around annotated entry points only.
    static ANNOTATE_DRAWS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
pub(crate) fn annotating() -> bool {
    ANNOTATE_DRAWS.with(|c| c.get())
}

thread_local! {
    /// Replicate mode: when set, an equal-priority/equal-speed move-order tie resolves to a
    /// SINGLE ordering (`Some(true)` = side One moves first) instead of the 50/50 enumerate fork.
    /// The seed-driven gate reads PS's `commitChoices` shuffle bit and forces the realized order
    /// so single-path replay is unambiguous. Default `None` — Enumerate/Sample are untouched.
    static FORCED_TIE_ORDER: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Force (or clear) the move-order tie resolution for the current thread. See [`FORCED_TIE_ORDER`].
pub fn set_forced_tie_order(order: Option<bool>) {
    FORCED_TIE_ORDER.with(|c| c.set(order));
}

#[inline]
fn forced_tie_order() -> Option<bool> {
    FORCED_TIE_ORDER.with(|c| c.get())
}

thread_local! {
    /// Cached both-side effective Speeds frozen at the start of the CURRENT move action, mirroring
    /// PS's `pokemon.speed` — a stat refreshed by `updateSpeed()` only at turn start and once before
    /// each move action (battle.ts:2942), NOT continuously. So a Speed change APPLIED DURING a move
    /// (paralysis from Thunder Wave, a Speed-dropping secondary, a self-boost) does NOT affect that
    /// same move's internal `eachEvent('Update')` speed-sorts (970/1024/2882) — they still sort on
    /// the pre-move cached Speed. `run_move_action` sets this to `Some([spe1, spe2])` while a move
    /// resolves; `actives_update_tie` uses it for the Speed comparison (liveness stays live). `None`
    /// everywhere else (turn-start bracket, switch brackets, residual, replacement bracket) → the
    /// live `effective_speed` is used, which is correct there (those are between-action Updates that
    /// PS evaluates after an `updateSpeed`).
    static MOVE_TIE_SPEEDS: std::cell::Cell<Option<[i32; 2]>> = const { std::cell::Cell::new(None) };
}

// ── Realized single-path multi-hit executor ────────────────────────────────────────────────
//
// A variable multi-hit move ([2,5] bulletseed/iciclespear/…, Population Bomb's 10 hits, Beat Up's
// per-member hits) draws a hit COUNT (`sample`/`random`) then rolls crit+damage PER hit — the full
// per-hit product is 32^hits, which is why the Enumerate/Sample verification path folds the count
// into a sumset-DP (`apply_multihit_dp`) that emits NO per-hit draw stream. That is exact for STATE
// but under-consumes the PRNG, so the Replicate (seed gate) and differ paths desync from PS's stream
// on these moves. When a realized source is installed (only by those two callers) the multi-hit
// dispatch instead REALIZES a single branch: it consumes the count draw and each hit's crit+damage
// from the source in PS's exact order (`battle-actions.ts:864` hit loop), producing the one branch
// PS's stream dictates with its exact draw log. Enumerate/Sample never install a source → DP path.

/// Where the realized multi-hit executor reads its outcomes.
pub enum RealizedSource {
    /// Seed gate: the `PsPrng` state at the START of the decision. The executor positions a clone
    /// by replaying the branch's draws-so-far (shape-consumed — draw COUNT, not values, matters for
    /// positioning), then peeks the count + per-hit rolls off the clone.
    Prng(crate::psprng::PsPrng),
    /// Differ: the unit's recorded draw RESULTS in order (sample → chosen index). The executor
    /// indexes by the branch's draws-so-far length, which equals PS's draw position when aligned.
    Recorded(std::rc::Rc<Vec<i64>>),
    /// `Exec::Sample` (training): draw outcomes from a splitmix64 stream held in [`SPLITMIX`].
    ///
    /// Sample mode follows ONE path, so it has no use for the branch set a multi-hit move's
    /// per-hit product generates — but it was still paying for it, because pruning happens at
    /// stage seams and the product is built *inside* a stage. Triple Axel is the extreme case:
    /// (16 damage rolls x 2 crit)^k for k=1..3 = 33,825 branches, each cloning a `State`, all
    /// but one discarded. Installing this source routes the same moves down the realized
    /// executor the seed gate already uses, which draws per hit and emits a single branch.
    ///
    /// Unlike the other two this carries no state of its own: the stream lives in a separate
    /// cell so `realized_cursor` (which only has `&`) can advance it.
    Splitmix,
}

thread_local! {
    static REALIZED_SOURCE: std::cell::RefCell<Option<RealizedSource>> =
        const { std::cell::RefCell::new(None) };
    /// The `RealizedSource::Splitmix` stream. Seeded per call from the sampled executor's own RNG
    /// (see `generate_instructions_sampled`) so a run stays reproducible from its seed.
    static SPLITMIX: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// One splitmix64 step of the [`SPLITMIX`] stream.
fn splitmix_next() -> u64 {
    SPLITMIX.with(|c| {
        let s = c.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
        c.set(s);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    })
}

/// Install (or clear) the realized multi-hit source for the current thread. See [`RealizedSource`].
pub fn set_realized_source(src: Option<RealizedSource>) {
    REALIZED_SOURCE.with(|c| *c.borrow_mut() = src);
}

/// gen≥5 [2,5] hit-count table: `sample([2×7, 3×7, 4×3, 5×3])` (battle-actions.ts:864). Index → count.
const MULTIHIT_COUNT_TABLE: [u8; 20] = [2, 2, 2, 2, 2, 2, 2, 3, 3, 3, 3, 3, 3, 3, 4, 4, 4, 5, 5, 5];

/// Advance a `PsPrng` by one draw of the given PS call shape (used to position the peek clone from
/// a branch's recorded draw stream — only the number of consumed sub-draws matters).
fn consume_shape(p: &mut crate::psprng::PsPrng, kind: &str, args: &[i32]) {
    match kind {
        "randomChance" => { p.random_chance(args[0] as u32, args[1] as u32); }
        "random" if args.len() == 2 => { p.random_range(args[0] as u32, args[1] as u32); }
        "random" => { p.random_n(args[0] as u32); }
        "sample" => { p.sample_index(args[0] as u32); }
        "shuffle" => {
            let (start, end) = (args[1] as usize, args[2] as usize);
            let mut s = start;
            while s < end.saturating_sub(1) { p.random_range(s as u32, end as u32); s += 1; }
        }
        _ => {}
    }
}

/// A positioned reader over the realized source: peeks successive draws in PS call form and returns
/// the realized result under PS's interpretation (randomChance → 0/1, random/sample → value/index).
#[derive(Clone)]
enum RealizedCursor {
    Prng(crate::psprng::PsPrng),
    Recorded { results: std::rc::Rc<Vec<i64>>, idx: usize },
    /// Sample mode. Stateless here — each `peek` advances the thread-local [`SPLITMIX`] stream,
    /// so several cursors taken within one decision continue the same stream instead of
    /// replaying it (two multi-hit moves in the same turn must not roll identically).
    Splitmix,
}

impl RealizedCursor {
    fn peek(&mut self, kind: &str, args: &[i32]) -> i64 {
        match self {
            RealizedCursor::Prng(p) => match kind {
                "randomChance" => p.random_chance(args[0] as u32, args[1] as u32) as i64,
                "random" if args.len() == 2 => p.random_range(args[0] as u32, args[1] as u32) as i64,
                "random" => p.random_n(args[0] as u32) as i64,
                "sample" => p.sample_index(args[0] as u32) as i64,
                _ => 0,
            },
            RealizedCursor::Recorded { results, idx } => {
                let r = results.get(*idx).copied().unwrap_or(0);
                *idx += 1;
                r
            }
            // Same call forms as the `Prng` arm and the same interpretation of each, so the
            // realized executor cannot tell them apart; only the bit source differs.
            RealizedCursor::Splitmix => {
                let r = splitmix_next();
                match kind {
                    "randomChance" => ((r % args[1].max(1) as u64) < args[0] as u64) as i64,
                    "random" if args.len() == 2 => {
                        let (from, to) = (args[0] as u64, args[1].max(args[0] + 1) as u64);
                        (from + r % (to - from)) as i64
                    }
                    "random" => (r % args[0].max(1) as u64) as i64,
                    "sample" => (r % args[0].max(1) as u64) as i64,
                    _ => 0,
                }
            }
        }
    }
    /// Consume the inter-hit `ModifyDamage` screen-tie `shuffle[k,0,k]` that `apply_damage_hit`
    /// emits after each damaging hit's roll (`emit_modifydamage_shuffle`, fired when `k >= 2`
    /// screens are on the field). The peek loop reads all hits' crit+damage up front, so it must
    /// step the cursor past this shuffle between hits or every hit past the first desyncs (a
    /// screened multi-hit move — e.g. Bullet Seed into Reflect+Light Screen). The `Prng` cursor
    /// consumes the raw `random(0,k)` draws PS's `speedSort` makes; the `Recorded` cursor skips
    /// the single logged shuffle entry.
    fn consume_shuffle(&mut self, k: i32) {
        if k < 2 {
            return;
        }
        match self {
            RealizedCursor::Prng(p) => consume_shape(p, "shuffle", &[k, 0, k]),
            RealizedCursor::Recorded { idx, .. } => *idx += 1,
            // Nothing to stay in step with: the splitmix stream exists only to pick outcomes, and
            // a speed-tie shuffle does not decide one. Skipping it keeps the stream shorter; it
            // cannot desync anything because no other reader shares this stream.
            RealizedCursor::Splitmix => {}
        }
    }
}

/// Number of screens (Reflect / Light Screen / Aurora Veil) present across BOTH sides — the
/// tie-group length `k` of the `ModifyDamage` screen shuffle. `emit_modifydamage_shuffle` fires
/// exactly when this is `>= 2`.
fn modifydamage_screen_count(b: &Branch) -> i32 {
    let mut k = 0i32;
    for side in [SideId::One, SideId::Two] {
        let sc = &b.state.side(side).side_conditions;
        k += (sc.reflect > 0) as i32 + (sc.light_screen > 0) as i32 + (sc.aurora_veil > 0) as i32;
    }
    k
}

/// Build a [`RealizedCursor`] positioned at the branch's current draw point, or `None` when no
/// realized source is installed (Enumerate/Sample — the DP path stays in effect).
fn realized_cursor(b: &Branch) -> Option<RealizedCursor> {
    REALIZED_SOURCE.with(|c| match &*c.borrow() {
        None => None,
        Some(RealizedSource::Prng(start)) => {
            let mut clone = *start;
            for d in &b.draws {
                consume_shape(&mut clone, d.kind, &d.args);
            }
            Some(RealizedCursor::Prng(clone))
        }
        Some(RealizedSource::Recorded(results)) => {
            Some(RealizedCursor::Recorded { results: std::rc::Rc::clone(results), idx: b.draws.len() })
        }
        // Sample mode deliberately does NOT realize here — see `realized_cursor_per_hit`.
        Some(RealizedSource::Splitmix) => None,
    })
}

/// Like [`realized_cursor`], but also hands `Exec::Sample` a cursor.
///
/// Only valid for moves whose ENUMERATE path also emits one `Damage` per hit. The variable
/// [2,5] moves, Population Bomb and Beat Up are compressed by the sumset DP in Enumerate: that
/// path emits a SINGLE aggregated `Damage { amount: total }`, while the realized executor emits
/// `Damage` per hit. Final HP agrees, the transcript does not — and the transcript is what the
/// protocol emitter, the narrator, the apply/reverse roundtrip and `sampled_distribution`'s
/// support check all read. Letting Sample realize those moves silently desynchronised the
/// sampled transcript from the enumerated one (caught by the multi-hit support check).
///
/// `tripleaxel`/`triplekick` enumerate via `HitCombos`, which already emits per-hit `Damage`, so
/// realizing them in Sample is transcript-identical — and they are the moves that actually cost:
/// (16 rolls x 2 crit)^k for k=1..3 = 33,825 branches, ~408 ms per decision, all but one thrown
/// away.
fn realized_cursor_per_hit(b: &Branch) -> Option<RealizedCursor> {
    if REALIZED_SOURCE.with(|c| matches!(&*c.borrow(), Some(RealizedSource::Splitmix))) {
        return Some(RealizedCursor::Splitmix);
    }
    realized_cursor(b)
}

/// True if `md` is a multi-hit move the realized executor handles (variable [2,5] or a fixed count
/// above the exact-enumeration cap — Population Bomb's 10). Beat Up routes separately.
fn realized_multihit_move(md: &crate::data::MoveData) -> bool {
    let (lo, hi) = (md.hits as usize, md.hits_max as usize);
    lo != hi || (lo == hi && lo > MAX_EXACT_HITS)
}

/// True for the per-hit-accuracy multi-hit moves (`multiaccuracy`): each hit past the first rolls
/// its own accuracy `randomChance(acc,100)` and a miss ends the move.
fn is_multiaccuracy_move(md: &crate::data::MoveData) -> bool {
    matches!(md.id.to_id(), "populationbomb" | "tripleaxel" | "triplekick")
}

/// Would this pair of move choices resolve as an equal-priority/equal-speed order TIE (the case
/// PS breaks with a `commitChoices` `shuffle[2,0,2]`)? Both sides must be attacking; custap
/// fractional priority is accounted for exactly as the turn resolver does. Used by the seed gate
/// to know when to consume PS's order-deciding shuffle bit.
pub fn move_order_tie(state: &State, s1: MoveChoice, s2: MoveChoice) -> bool {
    let (MoveChoice::Move(i1), MoveChoice::Move(i2)) = (s1, s2) else { return false };
    let mut branches = vec![Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false }];
    let custap = custap_stage(&mut branches, state, s1, s2);
    let mk = |side: SideId, idx: u8, cu: bool| Action {
        side, move_idx: idx, pivot: Pivot::Stay, shell_phys: None,
        foe_pending_move: None, custap: cu, struggling: no_usable_move(state, side),
        external_move: None, called: false,
    };
    let a = mk(SideId::One, i1, custap[0]);
    let c = mk(SideId::Two, i2, custap[1]);
    move_order(state, &a, &c) == Order::Tie
}

/// Append a draw to a branch's stream (no-op unless annotation is enabled).
#[inline]
pub(crate) fn draw(b: &mut Branch, kind: &'static str, args: &[i32], result: i64, site: &'static str) {
    if annotating() {
        b.draws.push(DrawEvent { kind, args: args.to_vec(), result, site });
    }
}

/// RAII guard: enable draw annotation for the current thread, restore prior state on drop.
struct AnnotateGuard(bool);
impl AnnotateGuard {
    fn enable() -> Self {
        let prev = ANNOTATE_DRAWS.with(|c| c.replace(true));
        AnnotateGuard(prev)
    }
}
impl Drop for AnnotateGuard {
    fn drop(&mut self) {
        ANNOTATE_DRAWS.with(|c| c.set(self.0));
    }
}

/// One node of the outcome tree: a probability, the resulting state, and the
/// instructions that produced it (relative to the input state).
#[derive(Clone)]
pub(crate) struct Branch {
    pub(crate) prob: f32,
    pub(crate) state: State,
    pub(crate) ins: Vec<Instruction>,
    /// Ordered PRNG draws that produced this branch (populated only under annotation).
    pub(crate) draws: Vec<DrawEvent>,
    /// Transient (NOT part of State): did the move being resolved on this branch FAIL to connect
    /// — immune / missed / no-target / blocked (PS `moveThisTurnResult === false`)? Set at the
    /// damaging-move failure sites; committed to the side's `last_move_failed` once per move action
    /// in `run_move_action`. Defaults false ("succeeded"); reset implicitly each move.
    pub(crate) move_failed: bool,
    /// Transient (NOT part of State): a self-switch (pivot) move already emitted its move action's
    /// trailing runAction Update (2882) on the PRE-switch board (PS fires it before processing the
    /// `switchFlag`), so `run_move_action` must NOT emit it again on the post-switch board. Set by
    /// `emit_pivot_trailing_update` at the pivot apply site; reset each move in `run_move_action`.
    pub(crate) pivot_update_done: bool,
    /// Transient (NOT part of State): the realized multi-hit executor already fired this move's
    /// per-hit `DamagingHit` ability rolls (Cursed Body / Toxic Chain / the contact-status set)
    /// INSIDE its hit loop, exactly where PS's `spreadMoveHit` runs `runEvent('DamagingHit')`.
    /// The post-hit-loop block must then not fire them a second time. Only ever set on the
    /// realized (seed-gate / differ) path; the DP path leaves it false and keeps the once-per-move
    /// application, which is exact for a single-hit move and is what Enumerate/Sample verify.
    pub(crate) per_hit_procs_done: bool,
    /// Transient (NOT part of State): the *deferred* `runEvent('DamagingHit')` payload for the last
    /// connecting hit — `(any_damage, def_item, def_ability)`. `spreadMoveHit` runs step 5
    /// (`secondaries()`) BEFORE step 7 (`runEvent('DamagingHit')`), so the hit loops stash the final
    /// hit's event here instead of firing it inline and `apply_damaging_hit_step7` flushes it after
    /// the secondary split. Hits that are followed by another executed hit flush at the top of the
    /// next iteration (PS's damage calc for hit n+1 must see hit n's event), so at most one is ever
    /// pending. See `apply_damaging_hit_step7`.
    pub(crate) pending_damaging_hit: Option<(bool, Item, crate::ids::Ability)>,
    /// Transient (NOT part of State): a DRAG (Dragon Tail / Circle Throw / Roar / Whirlwind)
    /// happened during this move action, so every remaining `getAllActive()` speed-sort in the
    /// action must use THESE cached Speeds instead of the move-start snapshot in
    /// `MOVE_TIE_SPEEDS`. Set per-branch by `apply_drag` (the drawn target differs per branch, so
    /// a thread-local cannot carry it); read by `run_move_action`'s trailing 2882 emit; reset at
    /// the top of every `run_move_action`. See `apply_drag` for the PS derivation.
    pub(crate) drag_tie_speeds: Option<[i32; 2]>,
    /// Transient (NOT part of State): was the ATTACKER still standing at the moment PS runs a
    /// move's `onAfterHit`?
    ///
    /// `onAfterHit` lives INSIDE `spreadMoveHit` (`sim/battle-actions.ts:1144`), guarded by
    /// `pokemon.hp`, immediately after `runEvent('DamagingHit')`. Everything that can kill the
    /// attacker afterwards — `move.recoil`, and Life Orb's `onAfterMoveSecondarySelf` at
    /// `useMoveInner:533` — happens LATER. The engine applies both inside `apply_post_damage`,
    /// which runs at the end of the hit loop and therefore BEFORE the `onAfterHit` payloads
    /// (Ceaseless Edge's Spikes, Stone Axe's Stealth Rock, Glaive Rush's volatile). Testing
    /// `is_alive()` at those sites asks the question one self-KO too late, so `apply_post_damage`
    /// snapshots the answer here first. `true` when no damaging move has resolved.
    ///
    /// **The snapshot alone is one PS line too EARLY when the attacker dies at step 7.** The guard
    /// has to be false for a user the `runEvent('DamagingHit')` chip killed (Rocky Helmet / Rough
    /// Skin / Iron Barbs), because step 7 genuinely precedes step 8 — and true for a user its own
    /// Life Orb killed, because that is `onAfterMoveSecondarySelf`, later. The engine applies the
    /// LATE self-damage early and flushes step 7 LATE, so neither `is_alive()` nor the raw snapshot
    /// answers both. `step8_user_alive` composes them: `hp + late_self_damage > 0`.
    pub(crate) after_hit_user_alive: bool,
    /// Transient (NOT part of State): how much self-damage `apply_post_damage` has already dealt
    /// the ATTACKER that PS deals only AFTER step 8 — `move.recoil`
    /// (`battle-actions.ts:984`, after the whole hit loop) and Life Orb's
    /// `onAfterMoveSecondarySelf` (`useMoveInner:533`). Reset at the top of every
    /// `apply_post_damage`; read only by `step8_user_alive`.
    pub(crate) late_self_damage: i16,
    /// Transient (NOT part of State): did the move `apply_post_damage` just closed out deal damage
    /// to the TARGET ITSELF (PS's `damage` entry being a number, not a Substitute's `true`)?
    /// Written at the top of every `apply_post_damage`; read by the realized executors' caller,
    /// which fires `apply_after_hit_item_moves` at the step-7/8 boundary and has no other way to
    /// see the hit loop's `any_damage`.
    pub(crate) move_any_damage: bool,
}

/// PS's step-8 guard `if (moveData.onAfterHit && pokemon.hp)` (`battle-actions.ts:1144`),
/// evaluated where the ENGINE stands rather than where PS does.
///
/// The engine's move pipeline runs `apply_post_damage` (drain, `move.recoil`, Life Orb) at the end
/// of the hit loop and DEFERS `runEvent('DamagingHit')` past the caller's step-5 secondary split
/// (see `apply_damaging_hit_step7`). PS's order is the reverse on both counts: step 7 chips the
/// attacker BEFORE step 8 reads its HP, and the recoil / orb land after. So at the step-7/8
/// boundary the engine's `hp` is short by exactly the late self-damage it front-loaded, and
/// crediting it back reproduces PS's `pokemon.hp` at that line.
///
/// * rb5164 d44 t35 — a 9-HP Assault Vest Okidogi Knock Offs a Rocky Helmet Amoonguss and dies to
///   the 1/6 chip. `late_self_damage` is 0, so the guard is FALSE and PS keeps the helmet; the
///   raw snapshot said the Okidogi was alive and the engine knocked it off.
/// * rb1765 d6 t5 — a 16-HP Life Orb Samurott-Hisui lands Ceaseless Edge and dies to the ORB.
///   `late_self_damage` is 16, so the guard stays TRUE and the Spikes are laid, as PS lays them.
fn step8_user_alive(b: &Branch, side: SideId) -> bool {
    b.state.side(side).active().hp as i32 + b.late_self_damage as i32 > 0
}

/// Integer divide with round-half-up (matches PS's `Math.round` for positive values).
fn round_div(n: i32, d: i32) -> i32 {
    (n + d / 2) / d
}

/// True if the defender's ability grants immunity to `move_type` (absorbing abilities).
fn ability_immune(move_type: Type, ability: crate::ids::Ability) -> bool {
    use crate::ids::Ability::*;
    match (move_type, ability) {
        (Type::Ground, Levitate) => true,
        (Type::Fire, FlashFire) => true,
        (Type::Water, WaterAbsorb | DrySkin | StormDrain) => true,
        (Type::Electric, VoltAbsorb | LightningRod | MotorDrive) => true,
        (Type::Grass, SapSipper) => true,
        // Well-Baked Body: Fire immunity (+2 Def applied on the absorb branch).
        (Type::Fire, WellBakedBody) => true,
        _ => false,
    }
}

/// A species-locked item's guard: the `baseSpecies` id prefix it is locked to, and whether the
/// lock is SYMMETRIC (PS's `num ===` guards fire when EITHER the holder or the move's source is
/// that species; the `baseSpecies.baseSpecies ===` guards only look at the holder).
///
/// Read off the pinned gen9 dex (`Dex.forGen(9).items` with an `onTakeItem`); the groups are
/// exactly: Arceus plates (num 493), Silvally memories (773), Genesect drives (649), the ORIGIN
/// forms of the creation-trio items (Adamant Crystal 483 / Lustrous Globe 484 / Griseous Core
/// 487), Zacian's Rusted Sword (888) and Zamazenta's Rusted Shield (889), the Ogerpon masks, and
/// Kyogre's Blue Orb / Groudon's Red Orb.
///
/// NOT locked in gen9: the plain **Adamant / Lustrous / Griseous Orb** — those lost their
/// `onTakeItem` when the Origin items were introduced, so Knock Off both boosts on and removes a
/// Palkia's Lustrous Orb. (The engine used to block them, which cost rb1090 and friends.)
fn item_lock(item: Item) -> Option<(&'static str, bool)> {
    Some(match item {
        Item::DracoPlate | Item::DreadPlate | Item::EarthPlate | Item::FistPlate
        | Item::FlamePlate | Item::IciclePlate | Item::InsectPlate | Item::IronPlate
        | Item::MeadowPlate | Item::MindPlate | Item::PixiePlate | Item::SkyPlate
        | Item::SplashPlate | Item::SpookyPlate | Item::StonePlate | Item::ToxicPlate
        | Item::ZapPlate => ("arceus", true),
        Item::BugMemory | Item::DarkMemory | Item::DragonMemory | Item::ElectricMemory
        | Item::FairyMemory | Item::FightingMemory | Item::FireMemory | Item::FlyingMemory
        | Item::GhostMemory | Item::GrassMemory | Item::GroundMemory | Item::IceMemory
        | Item::PoisonMemory | Item::PsychicMemory | Item::RockMemory | Item::SteelMemory
        | Item::WaterMemory => ("silvally", true),
        Item::BurnDrive | Item::ChillDrive | Item::DouseDrive | Item::ShockDrive => ("genesect", true),
        Item::AdamantCrystal => ("dialga", true),
        Item::LustrousGlobe => ("palkia", true),
        Item::GriseousCore => ("giratina", true),
        Item::RustedSword => ("zacian", true),
        Item::RustedShield => ("zamazenta", true),
        Item::HearthflameMask | Item::WellspringMask | Item::CornerstoneMask => ("ogerpon", false),
        Item::BlueOrb => ("kyogre", false),
        Item::RedOrb => ("groudon", false),
        _ => return None,
    })
}

/// Whether `item` can be removed from a mon of `holder` by Knock Off / Magician / Pickpocket /
/// Thief (PS's `onTakeItem` returning false blocks it). `source` is the species on the OTHER end
/// of the transfer, which the symmetric (`num ===`) guards also test.
fn item_removable_from(
    holder: crate::ids::Species,
    item: Item,
    source: Option<crate::ids::Species>,
) -> bool {
    if item == Item::None {
        return true; // "removable" is meaningless without an item
    }
    let Some((prefix, symmetric)) = item_lock(item) else { return true };
    if holder.to_id().starts_with(prefix) {
        return false;
    }
    if symmetric {
        if let Some(src) = source {
            if src.to_id().starts_with(prefix) {
                return false;
            }
        }
    }
    true
}

/// Moves with the `defrost` flag at the pin: their frozen user thaws with no `randomChance` roll.
fn is_defrost_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "burnup" | "flamewheel" | "flareblitz" | "fusionflare" | "hydrosteam" | "matchagotcha"
            | "pyroball" | "sacredfire" | "scald" | "scorchingsands" | "sizzlyslide"
            | "steameruption" | "polarflare"
    )
}

/// Moves with the `dance` flag at the pin (Dancer copies them).
fn is_dance_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "aquastep" | "clangoroussoul" | "dragondance" | "featherdance" | "fierydance"
            | "lunardance" | "petaldance" | "quiverdance" | "revelationdance" | "swordsdance"
            | "teeterdance" | "victorydance"
    )
}

/// Moves with the `reflectable` flag at the pin (Magic Bounce / Magic Coat targets).
fn is_reflectable_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "attract" | "babydolleyes" | "block" | "captivate" | "charm" | "confide"
            | "confuseray" | "corrosivegas" | "cottonspore" | "darkvoid" | "defog"
            | "disable" | "eerieimpulse" | "embargo" | "encore" | "entrainment"
            | "faketears" | "featherdance" | "flash" | "flatter" | "floralhealing"
            | "foresight" | "forestscurse" | "gastroacid" | "glare" | "grasswhistle"
            | "growl" | "healblock" | "healpulse" | "hypnosis" | "kinesis" | "leechseed"
            | "leer" | "lovelykiss" | "magicpowder" | "meanlook" | "metalsound"
            | "miracleeye" | "nobleroar" | "odorsleuth" | "partingshot" | "playnice"
            | "poisongas" | "poisonpowder" | "powder" | "purify" | "roar" | "sandattack"
            | "sappyseed" | "scaryface" | "screech" | "simplebeam" | "sing" | "sleeppowder"
            | "smokescreen" | "soak" | "spicyextract" | "spiderweb" | "spikes" | "spite"
            | "spore" | "spotlight" | "stealthrock" | "stickyweb" | "strengthsap"
            | "stringshot" | "stunspore" | "supersonic" | "swagger" | "sweetkiss"
            | "sweetscent" | "tailwhip" | "tarshot" | "taunt" | "tearfullook"
            | "telekinesis" | "thunderwave" | "tickle" | "topsyturvy" | "torment" | "toxic"
            | "toxicspikes" | "toxicthread" | "trickortreat" | "venomdrench" | "whirlwind"
            | "willowisp" | "worryseed" | "yawn"
    )
}

/// Dancer: after `side` successfully used a dance move, the opposing active with Dancer
/// immediately uses the same move (PS runMove with `externalMove: true`, targeting the
/// original user; self-targeting dances boost the Dancer itself). The copy goes through
/// the full before-move gauntlet (sleep tick, attract, confusion, paralysis) like PS's
/// BeforeMove event, but pays no PP and does none of the move-use bookkeeping.
fn apply_dancer_copies(out: Vec<Branch>, side: SideId, move_id: crate::ids::MoveId) -> Vec<Branch> {
    let foe = side.other();
    out.into_iter()
        .flat_map(|b| {
            let d = b.state.side(foe).active();
            let ok = d.is_alive()
                && d.ability == crate::ids::Ability::Dancer
                && !b.state.side(foe).volatiles.contains(VolatileStatus::Flinch)
                && !matches!(b.state.side(foe).pending_move, crate::state::PendingMove::Charging(m) if is_semi_invuln_move(m));
            if !ok {
                return vec![b];
            }
            execute_move(b, Action {
                side: foe,
                move_idx: 0,
                pivot: Pivot::Stay,
                shell_phys: None,
                foe_pending_move: None,
                custap: false,
                struggling: false, // external (Dancer) copies never Struggle
                external_move: Some(move_id),
                called: false,
            })
        })
        .collect()
}

/// Stage 0 of turn resolution — Custap Berry fires at QUEUE time (PS onFractionalPriority runs
/// when the turn's actions are resolved, before anything executes): a holder at ≤1/4 HP (≤1/2
/// with Gluttony) whose chosen move has effective priority ≤ 0 eats the berry immediately and
/// gains +0.1 priority for this turn's ordering (the `custap` flag on its Action). Shared by the
/// whole-turn resolver and the request-flow's decomposed pivot path.
pub(crate) fn custap_stage(branches: &mut [Branch], state: &State, s1: MoveChoice, s2: MoveChoice) -> [bool; 2] {
    let mut custap = [false; 2];
    for (i, (side, choice)) in [(SideId::One, s1), (SideId::Two, s2)].into_iter().enumerate() {
        let MoveChoice::Move(idx) = choice else { continue };
        let p = state.side(side).active();
        let pinch = p.hp * 4 <= p.max_hp
            || (p.ability == crate::ids::Ability::Gluttony && p.hp * 2 <= p.max_hp);
        if p.item == Item::CustapBerry
            && p.is_alive()
            && pinch
            && effective_priority(state, side, idx) <= 0
        {
            custap[i] = true;
            for b in branches.iter_mut() {
                let slot = b.state.side(side).active_index;
                push(b, Instruction::ChangeItem { side, slot, previous: Item::CustapBerry, new: Item::None });
                on_berry_eaten_id(b, side, Item::CustapBerry);
            }
        }
    }
    custap
}

/// Damaging moves with the `wind` flag (Wind Rider / Wind Power triggers). Non-damaging wind
/// moves (Whirlwind, Tailwind, Sandstorm) route through the status path and are not covered.
/// `flags: { wind: 1 }` — ENUMERATED off the pinned dex (`Dex.forGen(9).moves.all()`), all 17,
/// not recalled. The hand-written version of this list had 14 and was missing `whirlwind`,
/// `sandstorm` and `tailwind`.
///
/// **`whirlwind` is the one that cost a game.** rb5343 d48 t41: a Skarmory Whirlwinds a Shiftry
/// with WIND RIDER. `windrider.onTryHit` (`data/abilities.ts:5490`) tests `move.flags['wind']`,
/// gives the target +1 Atk and returns `null` at moveStep 2 — so the drag never happens and PS
/// draws nothing. The engine dragged and emitted a `rust extra` `sample[1]@drag`.
///
/// `sandstorm` (target `all`) and `tailwind` (target `allySide`) never reach a foe's `TryHit`, so
/// they are inert on this path; they are here because the LIST is the dex's, not a curated subset.
fn is_wind_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "aeroblast" | "aircutter" | "bleakwindstorm" | "blizzard" | "fairywind" | "gust"
            | "heatwave" | "hurricane" | "icywind" | "petalblizzard" | "sandsearstorm"
            | "sandstorm" | "springtidestorm" | "tailwind" | "twister" | "whirlwind"
            | "wildboltstorm"
    )
}

/// Multiplier for a stat stage (-6..=6), the standard gen formula.
pub fn boost_multiplier(stage: i8) -> f32 {
    if stage >= 0 {
        (2 + stage as i32) as f32 / 2.0
    } else {
        2.0 / (2 - stage as i32) as f32
    }
}

/// Apply a stat-stage boost to a stat with PS-exact integer math: `floor(stat · num / den)`,
/// where a positive stage is `(2+n)/2` and a negative stage is `2/(2−n)`. Avoids the ±1
/// errors a float multiply-then-truncate can introduce at negative stages.
pub fn boosted_stat(stat: i64, stage: i8) -> i64 {
    let (num, den) = if stage >= 0 { (2 + stage as i64, 2) } else { (2, 2 - stage as i64) };
    stat * num / den
}

/// Effective speed including boost, paralysis, Choice Scarf, Tailwind and a Speed-based
/// Protosynthesis / Quark Drive boost.
pub fn effective_speed(state: &State, side: SideId) -> i32 {
    effective_speed_opts(state, side, false)
}

/// `effective_speed`, optionally evaluated as PS caches it at SWITCH-IN — see
/// `switch_entry_speed`. `at_entry` drops every effect whose PS handler runs strictly after
/// `queue.insertChoice({choice:'runSwitch'})`: the Speed boosts entry hazards apply, Slow Start's
/// halving and the Protosynthesis / Quark Drive boost (all three are `onStart`, fired by
/// `runSwitch`'s `fieldEvent('SwitchIn')`).
fn effective_speed_opts(state: &State, side: SideId, at_entry: bool) -> i32 {
    let s = state.side(side);
    let p = s.active();
    let mut spe = p.stat(crate::ids::StatIndex::Speed) as f32;
    spe *= boost_multiplier(if at_entry { 0 } else { s.boost(BoostIndex::Speed) });
    if p.status == Status::Paralysis {
        spe *= 0.5;
    }
    if p.item == Item::ChoiceScarf {
        spe *= 1.5;
    }
    if s.side_conditions.tailwind > 0 {
        spe *= 2.0;
    }
    if s.volatiles.contains(VolatileStatus::Unburden) {
        spe *= 2.0;
    }
    if !at_entry && has_proto(s) && proto_stat(p) == crate::ids::StatIndex::Speed {
        spe *= 1.5;
    }
    // Speed abilities (affect turn order).
    use crate::ids::Ability::*;
    let weather_double = matches!(
        (p.ability, state.weather),
        (Chlorophyll, crate::ids::Weather::Sun)
            | (SwiftSwim, crate::ids::Weather::Rain)
            | (SandRush, crate::ids::Weather::Sand)
            | (SlushRush, crate::ids::Weather::Snow)
    );
    if weather_double {
        spe *= 2.0;
    }
    if p.ability == QuickFeet && p.status != Status::None {
        spe *= 1.5;
    }
    // Surge Surfer: ×2 Speed in Electric Terrain (PS onModifySpe chainModify(2)).
    if p.ability == SurgeSurfer && state.terrain == crate::ids::Terrain::Electric {
        spe *= 2.0;
    }
    // Slow Start halves Speed for the first five active turns after each switch-in.
    if !at_entry && p.ability == SlowStart && s.active_turns <= 5 {
        spe *= 0.5;
    }
    action_speed_trunc(state, spe as i32)
}

/// The tail of PS's `getStat('spe')` + `getActionSpeed()` — the two steps our whole corpus never
/// saw, because `[Gen 9] Custom Game` sets `battle: { trunc: Math.trunc }` and both are gated on
/// the format NOT doing that.
///
/// 1. `sim/pokemon.ts:638` — `if (statName === 'spe' && stat > 10000 && !this.battle.format.battle?.trunc)
///    stat = 10000;` The Speed cap is SKIPPED in customgame (its `format.battle.trunc` is truthy)
///    and applied everywhere else.
/// 2. `sim/pokemon.ts:649` — `return this.battle.trunc(speed, 13);` With the real `Dex#trunc`
///    (`sim/dex.ts:363`) that is `(speed >>> 0) % 8192`; with `Math.trunc` the `bits` argument is
///    ignored outright and nothing happens.
///
/// This is the ONE place either step belongs: every engine read of "the Speed an action sorts on"
/// goes through `effective_speed`, which models `pokemon.speed` = `updateSpeed()` =
/// `getActionSpeed()` (`sim/pokemon.ts:557`).
///
/// **Known residual — Trick Room.** PS interposes `speed = 10000 - speed` BETWEEN the cap and the
/// truncation, so under Trick Room it truncates `10000 - s`, while the engine models Trick Room by
/// inverting the comparison instead. The two agree exactly wherever `s <= 1808`: there
/// `trunc(10000 - s, 13) == 1808 - s`, which is strictly decreasing in `s`, so "descending
/// truncated 10000-s" and "ascending s" induce the same order AND the same tie set (both are
/// `s1 == s2`). 1808 is far above any Speed reachable without a +6/Scarf/Tailwind stack, so the
/// disagreement needs Trick Room AND a >1808 effective Speed simultaneously. Flagged, not fixed:
/// fixing it means making the ~20 comparison sites read a signed action speed rather than
/// flipping, which is not worth the regression surface here.
#[inline]
fn action_speed_trunc(state: &State, spe: i32) -> i32 {
    if !state.ruleset.bit_truncation {
        return spe; // format.battle.trunc = Math.trunc: no cap, no wrap.
    }
    let capped = spe.min(10000).max(0) as i64;
    state.ruleset.trunc(capped, 13) as i32
}

/// The Speed PS caches for a mon that JUST switched in, which every shuffle of the switch bracket
/// sorts on.
///
/// `switchIn` (sim/battle-actions.ts:135-155) does, in this order: swap the slot, reset
/// `abilityState`/`itemState` with `initEffectState`, `runEvent('BeforeSwitchIn')`, then
/// `queue.insertChoice({choice: 'runSwitch', pokemon})` — and `insertChoice` calls
/// `choice.pokemon.updateSpeed()` (sim/battle-queue.ts:373-375). That is the LAST `updateSpeed`
/// before the bracket runs, and it happens BEFORE `runSwitch`, i.e. before entry hazards and before
/// every switch-in ability's `onStart`. So the cache excludes:
///   * the Speed −1 Sticky Web applies (c3c2s82 d49: Iron Crown enters a webbed side, cached 324 vs
///     live 216; rb1021 d58: Magnezone cached **151**, live 100 — the sidecar records PS's
///     `pokemon.speed` verbatim, and 151 ties the foe Sylveon exactly),
///   * Slow Start's ×0.5 (`onStart` sets `effectState.counter`, which `onModifySpe` reads, and
///     `switchIn` has just cleared `abilityState` — rb1369 d44, two Regigigas),
///   * the Protosynthesis / Quark Drive Speed boost (both `onStart`).
/// A weather/terrain Speed ability (Chlorophyll, Swift Swim, Surge Surfer) has no such gate and
/// DOES apply, as do paralysis, Choice Scarf and Tailwind.
/// It is computed on the PRE-switch board with the incoming party mon in the slot: `insertChoice`
/// runs after `switchIn`'s slot swap but before `runSwitch`, so the mon is `clearVolatile`-fresh
/// (no boosts, no volatiles, `abilityState` re-initialised) and NOTHING the entry does has
/// happened — no hazards, no `onStart`, no Imposter transform (rb1057 d24: a Ditto replaces into a
/// +1 Venomoth; PS caches Ditto's own pre-transform Speed, the engine read the copied one), and no
/// weather the switch-in's OWN ability is about to set.
fn switch_entry_speed(pre: &State, side: SideId, slot: u8) -> i32 {
    let mut st = *pre;
    let s = st.side_mut(side);
    s.active_index = slot;
    s.boosts = [0; 7];
    s.volatiles = Default::default();
    s.active_turns = 0;
    effective_speed_opts(&st, side, true)
}

/// Run `f` with the switch bracket's cached Speeds installed as the Update tie speeds: the mon that
/// just entered on `entered` uses `switch_entry_speed` off the PRE-switch board `pre`, the other
/// side its live (already-cached) Speed. See `switch_entry_speed` for why the two differ.
/// The three-shuffle bracket EVERY `switch` action fires after the swap: the switch action's
/// runAction `eachEvent('Update')` (sim/battle.ts:2882), `runSwitch`'s `getAllActive()` speedSort
/// (sim/battle-actions.ts:182), and `runSwitch`'s own runAction Update. Each is tie-gated on the
/// Speed PS cached at `insertChoice` — see `switch_entry_speed`. `pre` is the board captured
/// BEFORE `apply_switch`.
///
/// A PIVOT's mid-turn switch is a `switch` action too: `runAction` (sim/battle.ts:2897-2932) turns
/// a `switchFlag` into a `switch` REQUEST whose choice is inserted and resolved exactly like a
/// turn-action switch. It does NOT fire the pre-swap switch-out Update, though — that same block
/// runs `BeforeSwitchOut` itself and sets `skipBeforeSwitchOutEventFlag`, and `switchIn`'s
/// `eachEvent('Update')` (sim/battle-actions.ts:80-84) is gated on that flag being clear.
/// Witness rb1029 d18/d19: Grafaiai (238) U-turns out to Cramorant (195) against a Meganium (195);
/// PS records three `shuffle[2,0,2]` between the U-turn's draws and the foe's Swords Dance, the
/// engine recorded none and ran three draws behind from turn 15 on.
fn emit_switch_bracket(b: &mut Branch, pre: &State, side: SideId, target: u8) {
    with_switch_bracket_speeds(b, pre, side, target, |b| {
        emit_update(b); // switch action runAction Update (2882) — plain getAllActive()
        // `runSwitch` sorts `this.battle.getAllActive(TRUE)` (`sim/battle-actions.ts:181-182`) —
        // the ONLY speed sort in the bracket that passes `includeFainted`. A slot still holding a
        // mon that fainted earlier this turn therefore COUNTS, so the sort can tie (and consume a
        // shuffle) on a board where both Updates around it see a single active and consume none.
        // `emit_update_hit` is exactly that predicate ("the slot is occupied" rather than "alive").
        //
        // rb1710 d5 t4 / rb1706 d8 t6: a pivot lands while the FOE's slot holds a mon the pivot
        // move just KO'd and whose replacement is a separate, later decision. PS records exactly
        // ONE `shuffle[2,0,2]` for the landing — this one — and the engine recorded none, running
        // a draw behind PS for the rest of the game.
        emit_update_hit(b); // runSwitch getAllActive(true) speedSort (battle-actions.ts:182)
    });
    // **The THIRD shuffle sits on the far side of `fieldEvent('SwitchIn')`**, so the entrant's
    // switch-in ability has already run — and an ability that calls `formeChange` REFRESHES the
    // Speed cache the first two sorted on (`setSpecies` ends in `this.speed = this.storedStats.spe`,
    // `sim/pokemon.ts:1419`). `runSwitch` (`battle-actions.ts:180-193`) does the `getAllActive(true)`
    // speedSort, THEN `fieldEvent('SwitchIn')`, and only then returns to `runAction`'s trailing
    // Update (`battle.ts:2882`).
    //
    // Only a species change moves it: a Sticky Web drop or an Intimidate is a BOOST, and boosts
    // never touch `pokemon.speed` (rb1021 d58 is that case and still wants the cached entry Speed).
    // rb1751 d24 t19: a Minior-Green (Speed 235) switches into a Scream Tail (235); the first two
    // shuffles tie and fire, Shields Down then makes it Minior-Meteor at a RAW 140, and PS's third
    // Update does not tie. PS records two shuffles for the unit; the engine recorded three and ran
    // a draw ahead — the corpus's second `rust extra`.
    with_switch_bracket_speeds_post(b, pre, side, target, |b| {
        emit_update(b); // runSwitch runAction Update (2882) — plain getAllActive()
    });
}

/// The bracket's post-`SwitchIn` speeds: as `with_switch_bracket_speeds`, except that a switch-in
/// ability which `formeChange`d the entrant has overwritten its cached Speed with the new forme's
/// RAW `storedStats.spe` (`setSpecies`, `sim/pokemon.ts:1419`).
///
/// **A TRANSFORM is excluded even though it is a species change**, because `transformInto`
/// (`sim/pokemon.ts:1290-1305`) runs `setSpecies(species, effect, true)` FIRST — which caches the
/// Speed `setSpecies` computed for the copied species out of the TRANSFORMER's own level / IVs /
/// EVs / nature — and only then overwrites `storedStats` with the target's. The engine's post-state
/// carries the copied stat, not that intermediate, so an Imposter Ditto's cached Speed is not
/// derivable here; keeping the entry Speed is what the corpus was already exact on (rb1303 d6,
/// rb1060, rb1241, rb1591 and four more all regressed on a Ditto when this case was folded in).
/// NAMED GAP: an Imposter entrant whose `setSpecies` Speed differs from both its entry Speed and
/// the copied one would still mis-gate this shuffle; no corpus witness.
fn with_switch_bracket_speeds_post(
    b: &mut Branch, pre: &State, entered: SideId, slot: u8, f: impl FnOnce(&mut Branch),
) {
    let entry_species = pre.side(entered).pokemon[slot as usize].species;
    let now = b.state.side(entered).active();
    if now.species == entry_species || now.transformed {
        return with_switch_bracket_speeds(b, pre, entered, slot, f);
    }
    let prev = MOVE_TIE_SPEEDS.with(|c| c.get());
    let sort_speed = |s: SideId| {
        let p = b.state.side(s).active();
        if p.is_alive() { effective_speed(&b.state, s) } else { p.stat(crate::ids::StatIndex::Speed) as i32 }
    };
    let mut sp = [sort_speed(SideId::One), sort_speed(SideId::Two)];
    sp[entered as usize] = now.stat(crate::ids::StatIndex::Speed) as i32;
    MOVE_TIE_SPEEDS.with(|c| c.set(Some(sp)));
    f(b);
    MOVE_TIE_SPEEDS.with(|c| c.set(prev));
}

fn with_switch_bracket_speeds(
    b: &mut Branch, pre: &State, entered: SideId, slot: u8, f: impl FnOnce(&mut Branch),
) {
    let prev = MOVE_TIE_SPEEDS.with(|c| c.get());
    // A slot holding a mon that FAINTED earlier this turn is in `getAllActive(true)`, and it sorts
    // on the cached `pokemon.speed` that `faintMessages`' `clearVolatile` left behind:
    // `clearVolatile` ends in `setSpecies(baseSpecies)`, which ends in
    // `this.speed = this.storedStats.spe` (`sim/pokemon.ts:1419`). That is the RAW stat — no
    // boosts, no paralysis, no Choice Scarf, no Tailwind. Reading `effective_speed` there sees
    // boosts PS has already thrown away (rb1706 d7: Excadrill's Rapid Spin +1 Spe, on top of the
    // +3 it was already carrying, all of it gone the moment it faints — 185, not 277).
    let sort_speed = |s: SideId| {
        let p = b.state.side(s).active();
        if p.is_alive() { effective_speed(&b.state, s) } else { p.stat(crate::ids::StatIndex::Speed) as i32 }
    };
    let mut sp = [sort_speed(SideId::One), sort_speed(SideId::Two)];
    sp[entered as usize] = switch_entry_speed(pre, entered, slot);
    MOVE_TIE_SPEEDS.with(|c| c.set(Some(sp)));
    f(b);
    MOVE_TIE_SPEEDS.with(|c| c.set(prev));
}

/// PS `eachEvent('Update')` / `runAction` post-action Update speed-sorts `getAllActive()` with
/// `(a,b)=>b.speed-a.speed`; in singles the two actives tie — consuming one `prng.shuffle` —
/// iff they are both on the field and share `effective_speed`. `prefaint`: at the per-hit
/// `eachEvent('Update')` (battle-actions.ts:970) a just-KO'd target is STILL in `getAllActive`
/// (its `.fainted` flag isn't set until `faintMessages` at :979), so liveness there is "the slot
/// is occupied" (species present) rather than HP>0. Every other Update site (turn-start,
/// post-hit-loop 1024, runAction 2882, post-residual) evaluates AFTER `faintMessages`, so a
/// fainted mon is gone and the tie needs both actives alive.
fn actives_update_tie(state: &State, prefaint: bool) -> bool {
    let a = state.side(SideId::One).active();
    let d = state.side(SideId::Two).active();
    let live_ok = if prefaint {
        a.species != crate::ids::Species::None && d.species != crate::ids::Species::None
    } else {
        a.is_alive() && d.is_alive()
    };
    // Mid-move Updates sort on the Speed cached at this move's start (PS's `pokemon.speed`), so a
    // Speed change the move itself just applied (e.g. Thunder Wave's paralysis on a Speed-tied foe)
    // does not break the tie for the SAME move's 970/1024/2882. Outside a move (`None`) → live Speed.
    let (s1, s2) = match MOVE_TIE_SPEEDS.with(|c| c.get()) {
        Some([a, b]) => (a, b),
        None => (effective_speed(state, SideId::One), effective_speed(state, SideId::Two)),
    };
    live_ok && s1 == s2
}

/// Both actives alive and equal `effective_speed` (turn-order / commitChoices tie predicate).
fn actives_speed_tied(state: &State) -> bool {
    actives_update_tie(state, false)
}

/// Public tie predicate for the seed gate's post-KO replacement-switch bracket: PS resolves a
/// forced replacement as a `switch` action whose `runAction` fires the switch bracket (switch-action
/// runAction Update + `runSwitch` getAllActive speedSort + runSwitch runAction Update = 3
/// `shuffle[2,0,2]`), each a `getAllActive()` speed-tie shuffle on the POST-swap board. The gate
/// applies replacements via `switch_into` (state only), so it consumes this bracket separately —
/// gated on exactly this predicate (both actives alive and equal `effective_speed`).
///
/// Called on the PRE-swap board with the replacement list; a side that IS replacing sorts on
/// `switch_entry_speed` — the Speed PS cached at `queue.insertChoice({choice:'runSwitch'})`, before
/// entry hazards and before any switch-in `onStart`/transform — and a side that is not sorts on its
/// live (already-cached) Speed. Ground-truthed on c3c2s82 d49 (Iron Crown replaces a fainted
/// Grimmsnarl into Sticky Web: stored Spe 324, live 216 — exactly the foe Deoxys-Defense's 216 —
/// yet PS consumes ZERO draws, because its cache still reads 324), on rb1369 d44 / rb1310 d11 (the
/// Slow Start half of the same rule, one in each direction) and on rb1057 d24 (an Imposter Ditto
/// replaces into a +1 Venomoth: PS caches Ditto's own Speed, not the copied one).
///
/// Replacements are applied in order, so a second one sees the first's board.
pub fn replacement_bracket_tied(pre: &State, replacements: &[(SideId, u8)]) -> bool {
    let mut st = *pre;
    let mut sp = [effective_speed(pre, SideId::One), effective_speed(pre, SideId::Two)];
    for &(side, slot) in replacements {
        sp[side as usize] = switch_entry_speed(&st, side, slot);
        let _ = switch_into(&mut st, side, slot);
    }
    let alive = st.side(SideId::One).active().is_alive() && st.side(SideId::Two).active().is_alive();
    alive && sp[0] == sp[1]
}

/// Whether `commitChoices`' `queue.sort()` over a SIMULTANEOUS both-sides forced replacement ties.
///
/// A forced replacement is answered through the normal choice flow, so `commitChoices`
/// (sim/battle.ts) clears the queue, commits both sides' choices and calls `this.queue.sort()`
/// BEFORE `turnLoop` — one `speedSort` over the two `instaswitch` actions (order 3). They share
/// order and priority, so they tie ⟺ their `action.speed` values are equal, and
/// `action.speed = action.pokemon.getActionSpeed()` (sim/battle.ts:2681) is recomputed LIVE from
/// the OUTGOING mon — the one that just fainted. `faintMessages` ran `clearVolatile(false)` on it
/// (sim/battle.ts:2576), so that Speed carries no boosts and no volatiles; its status, item and the
/// side conditions survive. One replacement alone cannot tie: `speedSort` returns immediately on a
/// list of length 1.
///
/// This is a draw the gate never consumed. It precedes both the `insertChoice` `random(0, 2)` and
/// the replacement BRACKET — which sort the INCOMING mons — and is independent of them: rb1271 d10
/// fires this one alone, rb1329 d23 fires the other two alone.
///
/// Witness rb1271 d10 t8: Brambleghast's Rapid Spin and Tauros's Flare Blitz KO each other at t7,
/// both sides replace at t8, and PS's only draw for the whole unit is one `shuffle[2, 0, 2]` whose
/// group is `[{choice:'instaswitch', p1: Brambleghast, order 3, speed 209}, {choice:'instaswitch',
/// p2: Tauros, order 3, speed 209}]`. The incoming pair (Torkoal / Iron Bundle) is NOT tied, so the
/// bracket contributes nothing and the engine consumed zero draws for the unit.
pub fn replacement_queue_sort_tied(pre: &State) -> bool {
    let spe = |side: SideId| {
        let mut st = *pre;
        let s = st.side_mut(side);
        s.boosts = [0; 7];
        s.volatiles = Default::default();
        effective_speed(&st, side)
    };
    spe(SideId::One) == spe(SideId::Two)
}

/// Whether `ability` can be copied by Trace (PS `onUpdate` skips a `notrace`/self-referential
/// ability — the copy never fires and no `sample` is drawn). Shared by the switch-in Trace copy
/// and the seed gate's forced-replacement trace-draw accounting.
pub(crate) fn ability_is_traceable(ability: crate::ids::Ability) -> bool {
    use crate::ids::Ability::*;
    !matches!(
        ability,
        None | Trace | AsOneGlastrier | AsOneSpectrier | Comatose | Disguise | FlowerGift
            | Forecast | GulpMissile | HungerSwitch | IceFace | Illusion | Imposter
            | Multitype | NeutralizingGas | PowerConstruct | PowerOfAlchemy | Receiver
            | RKSSystem | Schooling | ShieldsDown | StanceChange | WonderGuard | ZenMode
            | ZeroToHero | Commander
            // PS `notrace` flag additions:
            | BattleBond | EmbodyAspectCornerstone | EmbodyAspectHearthflame
            | EmbodyAspectTeal | EmbodyAspectWellspring | Hospitality
            | PoisonPuppeteer | Protosynthesis | QuarkDrive
            | TeraShell | TeraShift | TeraformZero
    )
}

/// Whether a mon at party `slot` about to switch into `side` will fire Trace's `sample(1)` draw:
/// its stored ability is Trace and the foe it will face is alive with a traceable ability. Used
/// by the seed gate — a forced/post-turn replacement applied via `switch_into` (state only) skips
/// this switch-in `onUpdate` draw, which PS still consumes (c3c2s82/s83: a Trace Gardevoir replaces
/// a fainted mon and copies the foe's ability).
///
/// Trace is an `onUpdate` handler, so it fires at the first `eachEvent('Update')` at which a valid
/// foe exists. In a SIMULTANEOUS both-sides replacement the foe slot is still fainted before the
/// swaps are applied, so the foe must be resolved on the POST-swap board: `foe_replacement` is the
/// party slot the OTHER side is replacing with (if any), else the foe's current active.
pub fn trace_replacement_sample(state: &State, side: SideId, slot: u8, foe_replacement: Option<u8>) -> bool {
    let mon = &state.side(side).pokemon[slot as usize];
    if mon.ability != crate::ids::Ability::Trace {
        return false;
    }
    let foe = match foe_replacement {
        Some(fs) => &state.side(side.other()).pokemon[fs as usize],
        None => state.side(side.other()).active(),
    };
    foe.is_alive() && ability_is_traceable(foe.ability)
}

/// Emit the post-action `eachEvent('Update')` shuffle (battle.ts:2882 runAction, post-residual,
/// switch/tera brackets, and post-hit-loop 1024) — fires iff both actives are alive and tied.
/// Annotation-only; state-neutral (PS logs the shuffle order as null, validated via `stateAfter`).
fn emit_update(b: &mut Branch) {
    if annotating() && actives_update_tie(&b.state, false) {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
    }
}

/// PS `Field.setWeather` / `clearWeather` / `setTerrain` / `clearTerrain` each END with
/// `eachEvent('WeatherChange')` / `eachEvent('TerrainChange')` (field.ts:87 / :97 / :155 / :165) —
/// a `getAllActive()` speedSort, so exactly ONE `shuffle[2,0,2]` on a Speed-tied board. `eachEvent`
/// sorts on the CACHED `pokemon.speed` (only `updateSpeed` refreshes it), so a weather-gated Speed
/// ability (Swift Swim / Chlorophyll / Sand Rush / Slush Rush) does not change the tie at the
/// instant the field flips — emit BEFORE the state change. Ground-truthed in the corpus:
/// d6 d64 `shuffle[2,0,2]@drizzle` (Pelipper's Drizzle sets rain on a mid-turn switch-in) and
/// r10 d32 `shuffle[2,0,2]@grassysurge`. Annotation-only; state-neutral.
fn emit_field_change_shuffle(b: &mut Branch) {
    if annotating() && actives_update_tie(&b.state, false) {
        draw(b, "shuffle", &[2, 0, 2], -1, "fieldchange");
    }
}

/// Would the weather's own per-mon `onWeather` handlers KO an active this upkeep? Only the
/// damaging ones exist in gen9: the Sandstorm chip (1/16, skipped for Rock/Ground/Steel and the
/// sand abilities) and Dry Skin's 1/8 sun burn. Mirrors the residual loop below; used to decide
/// whether the recursive `eachEvent('Update')` inside `eachEvent('Weather')` still sees two
/// actives in `getAllActive()`.
fn weather_upkeep_faints(state: &State) -> bool {
    use crate::ids::Ability as Ab;
    [SideId::One, SideId::Two].into_iter().any(|side| {
        let p = state.side(side).active();
        if !p.is_alive() || p.ability == Ab::MagicGuard {
            return false;
        }
        let dmg = if effective_weather(state) == Weather::Sand {
            let immune = p.types.contains(&Type::Rock)
                || p.types.contains(&Type::Ground)
                || p.types.contains(&Type::Steel)
                || matches!(p.ability, Ab::SandVeil | Ab::SandRush | Ab::SandForce | Ab::Overcoat);
            if immune { 0 } else { (p.max_hp / 16).max(1) }
        } else if p.ability == Ab::DrySkin && matches!(state.weather, Weather::Sun | Weather::HarshSun) {
            (p.max_hp / 8).max(1)
        } else {
            0
        };
        dmg >= p.hp
    })
}

/// The weather's own `onFieldResidual` (data/conditions.ts — EVERY weather: rain/sun/sand/snow and
/// the primal trio) ends with `this.eachEvent('Weather')`, a `getAllActive()` speedSort; and
/// `eachEvent` RECURSES into `eachEvent('Update')` for gen >= 7 (battle.ts:474), a second speedSort.
/// So an active weather contributes TWO tie-gated `shuffle[2,0,2]` per end of turn, at residual
/// order 1 (`onFieldResidualOrder: 1`) — immediately after the residual handler-list speedSort and
/// before every other residual handler. PS skips them on the weather's FINAL turn: the residual
/// loop (battle.ts:515) decrements the duration first and, at 0, calls `field.clearWeather()`
/// instead of the handler (which fires one `WeatherChange` shuffle — see
/// `emit_field_change_shuffle`). Ground-truthed: d6 d64 two `shuffle[2,0,2]@raindance`, r10
/// d34/d35 two `shuffle[2,0,2]@snowscape` each, all directly after the `@generic` residual sort.
fn emit_weather_upkeep_shuffles(b: &mut Branch) {
    if !annotating() || !actives_update_tie(&b.state, false) {
        return;
    }
    draw(b, "shuffle", &[2, 0, 2], -1, "weather"); // eachEvent('Weather')
    if !weather_upkeep_faints(&b.state) {
        draw(b, "shuffle", &[2, 0, 2], -1, "weather"); // recursive eachEvent('Update')
    }
}

/// A turn-action `switch` runs its own `runAction` → post-action `eachEvent('Update')`
/// (battle.ts:2881) on the PRE-swap board — the outgoing mon is still on the field when this
/// Update speed-sorts, so the tie is evaluated BEFORE the incoming mon changes the Speed. Emit
/// this shuffle (state-neutral) immediately before applying the switch, so a tied board contributes
/// its extra `shuffle[2,0,2]` exactly where PS makes it (a Move+Switch turn: BeforeTurn + Update +
/// this = 3 draws, vs the engine's turn-start bracket alone = 2). Annotation-only.
fn emit_switch_pre_update(b: &mut Branch) {
    if annotating() && actives_update_tie(&b.state, false) {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
    }
}

/// A self-switch (pivot) move's runAction Update (battle.ts:2882): PS fires it when the move
/// action ends — BEFORE the `switchFlag` is processed as its own `switch`/`runSwitch` action — so
/// it speed-sorts the board with the pivot USER STILL on the field. The engine applies the pivot
/// switch inside `execute_move`, so this must be emitted (on the pre-switch board) at the pivot
/// site and `run_move_action` told not to re-emit it on the post-switch board. Ground-truthed on
/// d6 i25 (U-turn on a Garchomp==Pelipper speed tie: the trailing 2882 shuffles on `[user | foe]`).
fn emit_pivot_trailing_update(b: &mut Branch) {
    emit_update(b);
    b.pivot_update_done = true;
}

/// Emit the per-hit `eachEvent('Update')` shuffle (battle-actions.ts:970) — fires once per
/// connecting hit, on the PRE-faint-message board (a target at 0 HP still counts as on-field).
fn emit_update_hit(b: &mut Branch) -> bool {
    if annotating() && actives_update_tie(&b.state, true) {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
        return true;
    }
    false
}

/// The 970 Update fires ONCE PER ITERATION of `hitStepMoveHitLoop`, not once per move — it is the
/// last statement of the loop body (`battle-actions.ts:965`), so a five-hit Scale Shot fires FIVE.
/// Every hit loop in the engine emits the previous iteration's Update at the TOP of the next one
/// (after that iteration's KO break, which is PS's `targets.every(!hp)` — a hit that faints the
/// target still fires its own Update, and the iteration that then breaks fires none), leaving the
/// LAST executed hit's Update to `execute_move_inner`'s trailing `emit_update_hit`. That placement
/// is also PS's for the one thing it interleaves with: hit n's Update precedes hit n+1's
/// `multiaccuracy` accuracy roll (`:907`) and hit n+1's crit roll.
///
/// On a realized path the emitted `draw` advances the branch log and the real prng but NOT the peek
/// clone, so the cursor has to be stepped over it exactly like the `ModifyDamage` screen shuffle.
/// rb1661 d55 is the witness: Scale Shot + Loaded Dice into a Substitute, PS
/// `crit, damage, shuffle[2,0,2]` five times over; the engine emitted the five crit/damage pairs
/// back to back and read hit n+1's crit roll out of hit n's missing shuffle.
fn emit_prev_hit_update(b: &mut Branch, cur: Option<&mut RealizedCursor>) {
    if emit_update_hit(b) {
        if let Some(c) = cur {
            c.consume_shuffle(2);
        }
    }
}

/// Emit the `ModifyDamage` screen-tie shuffle PS makes inside `getDamage` (`runEvent('ModifyDamage')`,
/// battle-actions.ts:1830), fired per damaging hit AFTER the damage roll and before the secondary.
/// The Reflect / Light Screen / Aurora Veil side-conditions each register an `onAnyModifyDamage`
/// handler whose `effectHolder` is the SIDE (Side has no `getStat`), so comparePriority sees `speed`
/// 0 and `subOrder` 4 (side condition) — every present screen ties on (order false, priority 0,
/// speed 0, subOrder 4), independent of any active's Speed. `speedSort` shuffles the tie-group once
/// when ≥2 screens are on the field. Every OTHER ModifyDamage handler (resist berries, Multiscale,
/// Life Orb, Expert Belt, …) has speed>0 / subOrder 7-8 and sorts strictly BEFORE the speed-0
/// screens; corpus-wide EVERY mid-move ModifyDamage shuffle is `[K,0,K]` (no such handler precedes
/// the screens on a tied hit), so the tie-group starts at 0. Annotation-only; state-neutral.
fn emit_modifydamage_shuffle(b: &mut Branch) {
    if !annotating() {
        return;
    }
    let k = modifydamage_screen_count(b);
    if k >= 2 {
        draw(b, "shuffle", &[k, 0, k], -1, "modifydamage");
    }
}

/// Emit the `TrapPokemon` shuffles PS makes while building the next move request. In `getRequests`
/// PS runs `runEvent('TrapPokemon', pokemon)` for each active in `getAllActive()` order (p1 then
/// p2, battle.ts:1640/1724). The trapping volatiles — `trapped` (Mean Look / Block / Spider Web /
/// Jaw Lock), `partiallytrapped` (Bind / Fire Spin / …), `noretreat`, `octolock` — each register an
/// `onTrapPokemon` handler at DEFAULT priority (no `onTrapPokemonPriority`), so their comparePriority
/// keys are identical: order false, priority 0, subOrder 2 (Condition), holder = the same mon (same
/// speed). A mon trapped by ≥2 of them therefore has all handlers tie → one `shuffle[N,0,N]`.
/// (Commander's `commanded`/`commanding` use `onTrapPokemonPriority:-11`, a different priority — they
/// never tie with these and don't occur in the corpus.) Fired at the end of the turn, after the
/// post-residual Update, since `getRequests` runs after the turn completes. Annotation-only.
fn emit_trap_pokemon_shuffles(b: &mut Branch) {
    if !annotating() {
        return;
    }
    // PS's `endTurn` per-active loop (battle.ts:1664) runs `runEvent('DisableMove', pokemon)`
    // (:1688) and then `runEvent('TrapPokemon', pokemon)` (:1724) for the SAME mon before moving
    // on to the next side's active — so the two shuffles interleave per side, not in two passes.
    for side in [SideId::One, SideId::Two] {
        emit_disable_move_shuffle(b, side);
        let s = b.state.side(side);
        if !s.active().is_alive() {
            continue;
        }
        let v = s.volatiles;
        let n = (s.partial_trap_turns > 0) as i32
            + v.contains(VolatileStatus::Trapped) as i32
            + v.contains(VolatileStatus::NoRetreat) as i32
            + v.contains(VolatileStatus::Octolock) as i32;
        if n >= 2 {
            draw(b, "shuffle", &[n, 0, n], -1, "trappokemon");
        }
    }
}

/// Emit the `DisableMove` handler-tie shuffle PS makes in `endTurn` (battle.ts:1688,
/// `runEvent('DisableMove', pokemon)` per active). The `onDisableMove` handlers in gen9 are:
/// the CONDITIONS `choicelock` / `disable` / `encore` / `healblock` / `taunt` / `throatchop` /
/// `torment` (subOrder 2), the ability `gorillatactics` (7) and the item `assaultvest` (8).
/// `comparePriority` keys are (order false, priority 0, speed = the HOLDER's speed, subOrder) —
/// all of a single mon's condition handlers therefore tie exactly, so a mon carrying ≥2 of them
/// consumes one `shuffle[N, 0, k]` (N = its handler count, k = the tying condition count; the
/// ability/item handlers sort strictly after at subOrder 7/8 and never tie). Ground-truthed with a
/// PS shuffle-call-site probe on r2 d28 (Entei holds `choicelock` + `healblock`, both
/// `o=false,p=0,s=201,so=2` → `shuffle[2,0,2]`, from `Battle.endTurn` → `runEvent`). The moves'
/// own `onDisableMove` (Belch / Stuff Cheeks) go through `singleEvent` in the following moveSlot
/// loop — no handler list, no sort. `gravity`'s field-level handler is not modeled (the engine has
/// no Gravity state); it would sort at subOrder 5, speed 0, i.e. never inside the condition group.
/// Annotation-only; state-neutral.
fn emit_disable_move_shuffle(b: &mut Branch, side: SideId) {
    let s = b.state.side(side);
    if !s.active().is_alive() {
        return;
    }
    let v = s.volatiles;
    // Condition handlers (subOrder 2) — all tie on the holder's own speed.
    let k = v.contains(VolatileStatus::ChoiceLock) as i32
        + (s.disable.1 > 0) as i32
        + (s.encore.1 > 0) as i32
        + (s.heal_block_turns > 0) as i32
        + (s.taunt_turns > 0) as i32
        + (s.throat_chop_turns > 0) as i32
        + v.contains(VolatileStatus::Torment) as i32;
    if k < 2 {
        return;
    }
    let p = s.active();
    let n = k
        + (p.ability == crate::ids::Ability::GorillaTactics) as i32
        + (p.item == Item::AssaultVest) as i32;
    draw(b, "shuffle", &[n, 0, k], -1, "disablemove");
}

/// Emit the turn-start Update bracket PS produces before any move executes, in queue order:
///   1. `commitChoices` `queue.sort()` (battle.ts:3039): the two committed actions tie →
///      `shuffle[2,0,2]`. Two moves tie iff a full `move_order` tie (equal order/priority/
///      fractional-priority/speed); two switches tie iff equal OUTGOING (current-active) speed;
///      a move+switch never ties (orders 200 vs 103 differ).
///   2. `beforeTurn` action → `eachEvent('BeforeTurn')` (battle.ts:2830): `shuffle[2,0,2]` on a
///      speed tie (independent of priority/action kind).
///   3. runAction Update after the beforeTurn action (battle.ts:2882): `shuffle[2,0,2]` on a
///      speed tie.
///   4. gen8 dynamic-speed re-sort before the first move (battle.ts:2938), only when the next
///      queued action is a move (i.e. NEITHER side switches) and the two moves tie: the remaining
///      queue is `[move,move,residual]` (length 3) → `shuffle[3,0,2]`.
/// All four are state-neutral annotation draws emitted on the pre-switch board.
fn emit_turn_start_bracket(b: &mut Branch, s1: MoveChoice, s2: MoveChoice, custap: [bool; 2], tera: [bool; 2]) {
    if !annotating() {
        return;
    }
    let st = &b.state;
    let mk = |side: SideId, idx: u8, cu: bool| Action {
        side,
        move_idx: idx,
        pivot: Pivot::Stay,
        shell_phys: None,
        foe_pending_move: None,
        custap: cu,
        struggling: no_usable_move(st, side),
        external_move: None,
        called: false,
    };
    let both_move = matches!(s1, MoveChoice::Move(_)) && matches!(s2, MoveChoice::Move(_));
    let commit_tie = match (s1, s2) {
        (MoveChoice::Move(i1), MoveChoice::Move(i2)) => {
            let a = mk(SideId::One, i1, custap[0]);
            let c = mk(SideId::Two, i2, custap[1]);
            move_order(st, &a, &c) == Order::Tie
        }
        // Two switch actions sort on the outgoing (current) active's speed at order 103.
        (MoveChoice::Switch(_), MoveChoice::Switch(_)) => actives_speed_tied(st),
        _ => false,
    };
    let speed_tie = actives_speed_tied(st);
    // A `terastallize` action (gen9, order 106) is queued for each side that teras AND moves. It
    // precedes the two move actions (order 200) in the commit `queue.sort()`, so it lengthens the
    // sorted list and shifts the move-tie group: for `k` tera actions the moves tie at [k, k+2) of
    // a length-(k+2) list → `shuffle[k+2, k, k+2]`. **k=2 with two equal-Speed teras ALSO ties the
    // teras themselves at [0,2)**, and `speedSort` shuffles every tie group it finds, so that is a
    // SECOND commit draw ahead of the move one — `shuffle[4,0,2]` then `shuffle[4,2,4]`. It was
    // written off as vanishingly rare; rb1464 d5 t5 is the witness (both sides terastallize at
    // equal Speed, and the engine was one draw behind for the rest of the game). A `terastallize`
    // action's speed is `getActionSpeed(action)` — the same `pokemon.getActionSpeed()` a move
    // action gets (`battle-queue.ts:270`) — so the tera pair ties exactly when the actives do, and
    // `subOrder`/`effectOrder` are undefined on both, i.e. 0 in `comparePriority`.
    // Each tera action ALSO runs its
    // own `runAction` → an `eachEvent('Update')` shuffle (battle.ts:2882), but a `switch` action
    // (order 103) sorts BEFORE `terastallize` (106), so that Update speed-sorts the POST-switch
    // board — it is emitted at the tera application site (step 1.5 of `generate_branches_ctx`),
    // after any switch has been applied, not in this pre-switch bracket. (t2 d7: p1 switches
    // Gothitelle→Skarmory while p2 teras; pre-swap Gothitelle-vs-Gothitelle ties, post-swap
    // Skarmory-vs-Gothitelle does not — PS fires 3 shuffles, the pre-switch model fired 4.)
    let k = (tera[0] && matches!(s1, MoveChoice::Move(_))) as i32
        + (tera[1] && matches!(s2, MoveChoice::Move(_))) as i32;
    if k == 2 && speed_tie {
        draw(b, "shuffle", &[4, 0, 2], -1, "update"); // 1a. commitChoices sort: the two tera actions tie at [0,2)
    }
    if commit_tie {
        draw(b, "shuffle", &[2 + k, k, k + 2], -1, "update"); // 1b. commitChoices sort (tera-shifted move group)
    }
    if speed_tie {
        draw(b, "shuffle", &[2, 0, 2], -1, "update"); // 2. eachEvent('BeforeTurn')
        draw(b, "shuffle", &[2, 0, 2], -1, "update"); // 3. runAction Update (after beforeTurn)
    }
    if both_move && commit_tie {
        draw(b, "shuffle", &[3, 0, 2], -1, "update"); // 4. dynamic-speed re-sort (len-3 queue [move,move,residual])
    }
}

/// Would this pair of choices resolve as a Speed TIE between two `switch` ACTIONS?
///
/// Both sides switching queues two `switch` actions at order 103, speed-sorted on the OUTGOING
/// (current) active's Speed. On a tie `commitChoices`' `queue.sort()` breaks it with one
/// `shuffle[2,0,2]` (`sim/battle.ts:3038`), and unlike the `eachEvent` Update shuffles that tie
/// is NOT state-neutral: the faster side's `switch` queues its `runSwitch` at order 101, which
/// preempts the slower side's pending `switch` (103), so the winner's switch-in ability fires
/// while the loser's OLD mon is still on the field. rb1250 d32: Heatran and Malamar are both at
/// 167 Speed and PS's shuffle put p2 first, so Salamence's Intimidate landed on the outgoing
/// Heatran and Rabsca came in clean.
///
/// Unlike a both-move tie there is NO second queue sort to compose with: `runAction`'s gen8
/// dynamic re-sort (`sim/battle.ts:2940`) is gated on `queue.peek()?.choice === 'move'`, and the
/// next queued action here is a `switch`. So side One goes first iff the shuffle's single
/// `random(2)` bit is 0.
pub fn switch_order_tie(state: &State, s1: MoveChoice, s2: MoveChoice) -> bool {
    matches!((s1, s2), (MoveChoice::Switch(_), MoveChoice::Switch(_))) && actives_speed_tied(state)
}

/// Recompute a mon's stats for a NEW base-stat spread, preserving its own EV/IV/nature.
///
/// PS's forme changes go through `Pokemon.setSpecies` -> `spreadModify(species.baseStats, this.set)`,
/// i.e. the mon's ACTUAL set is re-applied to the new base stats. The engine's `State` bakes the
/// spread into `stats` (convert.rs stores `nature: Serious, evs: 0` — "spreads are baked into
/// storedStats"), so the spread is recovered by inverting each stat against the CURRENT species'
/// base stat: `stat = floor(floor((2*base + d) * level/100 + 5) * nature)` with
/// `d = IV + floor(EV/4)`. Corpus sets (custom + randombattle) all use 31 IVs, so `d ∈ [31, 94]`,
/// and the nature multiplier is tried neutral-first (1.0, then 1.1, then 0.9) — the first (n, d)
/// pair reproducing the current stat wins.
///
/// Verified against PS on the c6a2 Palafin (Jolly, 252 Atk / 4 SpD / 252 Spe):
/// Zero -> Hero gives atk 239->419, def 180->230, spa 127->223, spd 161->211, spe 328->328,
/// exactly PS's `storedStats`. The previous hard-coded random-battle spread (31 IV / 85 EV /
/// neutral) produced def 251 instead of 230 — a 4-point U-turn damage error at c6a2s114 d47.
/// HP (index 0) is never re-derived: every battle-only forme shares its base forme's HP base, and
/// PS keeps `maxhp` across the change.
fn respread_stats(old_base: [u16; 6], new_base: [u16; 6], old: [i16; 6], level: u8) -> [i16; 6] {
    let mut out = old;
    let stat_of = [
        crate::ids::StatIndex::Hp, crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
        crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
        crate::ids::StatIndex::Speed,
    ];
    for i in 1..6usize {
        let apply = |base: u16, d: i32, n: f32| -> i16 {
            let inner = (2 * base as i32 + d) * level as i32 / 100;
            (((inner + 5) as f32) * n).floor() as i16
        };
        'search: for &n in &[1.0f32, 1.1, 0.9] {
            // Every `d` reproducing the current stat under this nature multiplier. Below level 100
            // the `* level / 100` truncation makes several `d` collide, so accept the inversion
            // only when they all agree on the NEW stat; otherwise fall back to the random-battle
            // spread (31 IV / 85 EV / neutral), which is exact for every randombattle set.
            let mut vals: Vec<i16> = (31..=94i32)
                .filter(|&d| apply(old_base[i], d, n) == old[i])
                .map(|d| apply(new_base[i], d, n))
                .collect();
            if vals.is_empty() {
                continue;
            }
            vals.dedup();
            out[i] = if vals.len() == 1 {
                vals[0]
            } else {
                crate::damage::compute_stat(new_base[i], 31, 85, level, crate::ids::Nature::Serious, stat_of[i])
            };
            break 'search;
        }
    }
    out
}

/// Whether the active Pokémon has a Protosynthesis / Quark Drive boost active.
fn has_proto(s: &crate::state::Side) -> bool {
    s.volatiles.contains(VolatileStatus::Protosynthesis) || s.volatiles.contains(VolatileStatus::QuarkDrive)
}

/// The stat Protosynthesis / Quark Drive boosts: the highest of atk/def/spa/spd/spe,
/// matching PS's `bestStat` (first max in that order).
pub fn proto_stat(p: &crate::state::Pokemon) -> crate::ids::StatIndex {
    use crate::ids::StatIndex::*;
    let candidates = [Attack, Defense, SpecialAttack, SpecialDefense, Speed];
    let mut best = Attack;
    let mut best_val = i16::MIN;
    for c in candidates {
        let v = p.stat(c);
        if v > best_val {
            best_val = v;
            best = c;
        }
    }
    best
}

/// The battle is over once either side has no living Pokémon; PS then stops the turn
/// before end-of-turn residuals.
pub(crate) fn battle_over(state: &State) -> bool {
    [SideId::One, SideId::Two].into_iter().any(|side| {
        !state.side(side).pokemon.iter().any(|p| p.species != crate::ids::Species::None && p.is_alive())
    })
}

/// True if `side` has at least one living Pokémon (active or benched). PS's `AfterFaint` event —
/// which drives Moxie/Beast Boost/Neigh KO boosts — is skipped when the faint ends the battle
/// (`checkWin` returns before `AfterFaint`), so those boosts only apply while the KO'd mon's side
/// still has a Pokémon left.
fn side_has_living_mon(state: &State, side: SideId) -> bool {
    state.side(side).pokemon.iter().any(|p| p.species != crate::ids::Species::None && p.is_alive())
}

/// Is the active Pokémon grounded (subject to Spikes / Toxic Spikes / Sticky Web)?
/// Moves with PS `ignoreAbility`: they suppress the TARGET's ability for the move's damage and
/// immunity checks, exactly like a Mold Breaker user (Sunsteel Strike / Moongeist Beam / Photon
/// Geyser and the Ultra Necrozma Z-moves).
fn move_ignores_ability(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "sunsteelstrike" | "moongeistbeam" | "photongeyser"
            | "searingsunrazesmash" | "menacingmoonrazemaelstrom"
    )
}

/// The weather as mechanics see it: Air Lock / Cloud Nine on either active suppresses all
/// weather effects (the weather itself keeps ticking).
fn effective_weather(state: &State) -> Weather {
    use crate::ids::Ability as Ab;
    for side in [SideId::One, SideId::Two] {
        let p = state.side(side).active();
        if p.is_alive() && matches!(p.ability, Ab::AirLock | Ab::CloudNine) {
            return Weather::None;
        }
    }
    state.weather
}

/// PS Shields Down (abilities.ts:4194): Minior swaps between its Meteor shell and its coloured
/// core. The check is NOT continuous — it runs at `onStart` (switch-in, `onSwitchInPriority: -1`)
/// and at `onResidual` (`onResidualOrder: 29`), and only there: above half HP the mon is
/// `Minior-Meteor`, at or below half it reverts to `pokemon.set.species` (the coloured core the
/// team was packed with — `Minior`, `Minior-Blue`, `Minior-Orange`, …, which the engine keeps in
/// `base_species` because `Instruction::Transform` does not touch that field).
///
/// The base stats differ (Meteor 60/60/100/60/100/60 vs core 60/100/60/100/60/120) so the change
/// respreads through `respread_stats`, which recovers the mon's own EV/IV/nature spread; HP base is
/// 60 either way, so current/max HP are untouched. Types are Rock/Flying in both formes.
/// Makes no PRNG draw.
fn shields_down_forme(b: &mut Branch, side: SideId) {
    let p = b.state.side(side).active();
    if p.ability != crate::ids::Ability::ShieldsDown || !p.is_alive() || p.transformed {
        return;
    }
    let Some(meteor) = crate::ids::Species::from_id("miniormeteor") else { return };
    let core = p.base_species;
    if core == crate::ids::Species::None || !core.to_id().starts_with("minior") {
        return;
    }
    let want = if (p.hp as i32) * 2 > p.max_hp as i32 { meteor } else { core };
    if p.species == want {
        return;
    }
    let level = p.level;
    let old_base = crate::data::base_stats(p.species);
    let new_base = crate::data::base_stats(want);
    let stats = respread_stats(old_base, new_base, p.stats, level);
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = want;
    new.stats = stats;
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
}

/// PS's Protosynthesis `onWeatherChange` / Quark Drive `onTerrainChange` (abilities.ts:3473 and
/// :3563): whenever the field condition changes, EVERY holder already on the field re-evaluates —
/// `isWeather('sunnyday')` (resp. `isTerrain('electricterrain')`) ADDS the volatile, and anything
/// else REMOVES it unless it carries `fromBooster`, which PS keeps for the rest of the mon's stay.
/// The engine only re-derived the volatile at switch-in, so a mon already in neither gained the
/// boost when the sun came out (rb1018 t4: Walking Wake, Sunny Day goes up on the turn it is
/// already active) nor lost it when the sun lapsed (rb1167 t6 / rb1244 t6 / rb1024 t6: Gouging Fire
/// and Sandy Shocks keep a stale bit 25 once the weather runs out).
///
/// State-only — PS's handlers make no PRNG draw, so the draw stream is untouched. Note the sun test
/// is exactly `'sunnyday'`: `Field.isWeather` compares the effective weather id, so Desolate Land
/// does NOT activate Protosynthesis, and Air Lock / Cloud Nine (which zero `effectiveWeather`) DO
/// switch it off — both of which `effective_weather` already models.
fn refresh_proto_quark(b: &mut Branch) {
    for side in [SideId::One, SideId::Two] {
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let (ability, on) = match p.ability {
            crate::ids::Ability::Protosynthesis => {
                (VolatileStatus::Protosynthesis, effective_weather(&b.state) == Weather::Sun)
            }
            crate::ids::Ability::QuarkDrive => {
                (VolatileStatus::QuarkDrive, b.state.terrain == crate::ids::Terrain::Electric)
            }
            _ => continue,
        };
        let has = b.state.side(side).volatiles.contains(ability);
        if on {
            if !has {
                push(b, Instruction::ApplyVolatile { side, volatile: ability });
            }
        } else if has && !b.state.side(side).volatiles.contains(VolatileStatus::ProtoBooster) {
            push(b, Instruction::RemoveVolatile { side, volatile: ability });
        }
    }
}

/// A mon's weight after ability modifiers (Light Metal ×0.5, Heavy Metal ×2).
fn modified_weight_hg(p: &crate::state::Pokemon) -> u32 {
    let w = crate::data::species_weight_hg(p.species);
    match p.ability {
        crate::ids::Ability::LightMetal => w / 2,
        crate::ids::Ability::HeavyMetal => w * 2,
        _ => w,
    }
}

/// Snapshot the fields Transform touches on a side's active mon.
fn transform_data_of(state: &State, side: SideId) -> crate::instruction::TransformData {
    let p = state.side(side).active();
    crate::instruction::TransformData {
        species: p.species,
        stats: p.stats,
        types: p.types,
        live_types: p.live_types,
        ability: p.ability,
        moves: p.moves,
        transformed: p.transformed,
        times_hit: p.times_hit,
    }
}

/// Transform / Imposter: copy the foe's battle identity onto `side`'s active. Mirrors PS
/// `transformInto`: species/types/stats(except HP)/ability/boosts copied; each copied move
/// gets PP = min(5, base PP); crit volatiles (Focus Energy) copied. Fails against a
/// substitute, a transformed target, or when the user is already transformed — and, per
/// `sim/pokemon.ts:1274`, whenever EITHER mon is currently running an Illusion.
fn apply_transform(b: &mut Branch, side: SideId) -> bool {
    let foe = side.other();
    let user_ok = b.state.side(side).active().is_alive()
        && !b.state.side(side).active().transformed
        && b.state.side(side).active().illusion.is_none();
    let target = b.state.side(foe).active();
    let target_ok = target.is_alive()
        && !target.transformed
        && target.illusion.is_none()
        && !b.state.side(foe).volatiles.contains(VolatileStatus::Substitute);
    if !user_ok || !target_ok {
        return false;
    }
    let previous = transform_data_of(&b.state, side);
    let mut new = transform_data_of(&b.state, foe);
    new.stats[0] = previous.stats[0]; // HP is never copied
    // PS `transformInto` (`sim/pokemon.ts:1295`) copies `pokemon.getTypes(true, true)` — the
    // target's PRE-TERASTALLIZED live types, and explicitly `roost.typeWas` when the target is
    // roosting, i.e. the target's `types` array UNFILTERED. That is `live_types`, not the
    // target's effective typing. `setType(..., /*enforce*/ true)` writes it even when the USER
    // is terastallized, but the user's own effective typing stays its Tera type.
    new.live_types = b.state.side(foe).active().live_types;
    new.types = if b.state.side(side).active().terastallized {
        previous.types
    } else {
        new.live_types
    };
    for m in new.moves.iter_mut() {
        if m.id != crate::ids::MoveId::None {
            let pp = crate::data::move_data(m.id).pp.min(5);
            *m = crate::state::MoveSlot { id: m.id, pp, max_pp: pp, disabled: false };
        }
    }
    new.transformed = true;
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
    if b.state.side(side).volatiles.contains(VolatileStatus::ChoiceLock) {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::ChoiceLock });
    }
    // Copy the foe's stat stages.
    for stat in [
        BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
        BoostIndex::SpecialDefense, BoostIndex::Speed, BoostIndex::Accuracy, BoostIndex::Evasion,
    ] {
        let delta = b.state.side(foe).boost(stat) - b.state.side(side).boost(stat);
        if delta != 0 {
            push(b, Instruction::Boost { side, stat, amount: delta });
        }
    }
    // Crit-stage volatiles transfer.
    let foe_fe = b.state.side(foe).volatiles.contains(VolatileStatus::FocusEnergy);
    let my_fe = b.state.side(side).volatiles.contains(VolatileStatus::FocusEnergy);
    if foe_fe && !my_fe {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::FocusEnergy });
    } else if !foe_fe && my_fe {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::FocusEnergy });
    }
    // **The copied ability FIRES.** `transformInto` ends with `this.setAbility(pokemon.ability,
    // this, true)` (`sim/pokemon.ts`), and `setAbility` runs `singleEvent('Start', ability, ...)`
    // on the new ability for every gen > 3 — so an Imposter Ditto copying an Intimidate mon
    // Intimidates. rb1502 d20: a Ditto Impostered into a +2/+1 Incineroar and PS dropped the real
    // Incineroar's Attack to +1. The recursion terminates on its own: the copied ability can only
    // be `Imposter` if the target was an untransformed Ditto, and the re-entry then fails
    // `user_ok` because this mon is now `transformed`.
    apply_switch_in_ability(b, side);
    true
}

/// Faint the user of a self-sacrificing status move (Healing Wish, Memento, ...).
fn apply_post_status_self_destruct(b: &mut Branch, side: SideId, _md: &crate::data::MoveData) {
    let p = b.state.side(side).active();
    if p.is_alive() {
        let slot = b.state.side(side).active_index;
        push(b, Instruction::Damage { side, slot, amount: p.hp });
    }
}

/// A status move that affects the FOE (Thunder Wave, Taunt, Parting Shot, ...), as opposed
/// to self/field moves — used for Prankster's Dark-type immunity.
fn targets_foe_status(md: &crate::data::MoveData) -> bool {
    // PS's Prankster immunity is `!targets[i].isAlly(pokemon)` (battle-actions.ts:671-673): it
    // keys on the move's resolved TARGET, so a self-targeting Prankster move is never blocked.
    // `md.target_volatile` alone is not that test — the codegen folds PS `move.volatileStatus`
    // in, and self-targeting moves (Substitute, Magnet Rise, Destiny Bond, …) carry one.
    md.target.targets_foe()
        && (md.status != Status::None
            || md.target_boosts.iter().any(|&x| x != 0)
            || md.target_volatile.is_some()
            || md.force_switch
            || matches!(md.id.to_id(), "partingshot" | "trick" | "switcheroo" | "encore" | "disable" | "taunt" | "whirlwind" | "roar" | "defog"))
}

/// Sleep Clause Mod: an induced (non-Rest) sleep fails while any other Pokémon on the
/// target's side is already asleep.
fn sleep_clause_blocks(state: &State, side: SideId) -> bool {
    if !state.ruleset.sleep_clause {
        return false;
    }
    let s = state.side(side);
    s.pokemon.iter().any(|p| {
        p.species != crate::ids::Species::None
            && p.is_alive()
            && p.status == Status::Sleep
            && p.slept_by_foe
    })
}

/// Mark the active of `side` as foe-slept (for Sleep Clause).
fn mark_slept_by_foe(b: &mut Branch, side: SideId) {
    let slot = b.state.side(side).active_index;
    if !b.state.side(side).active().slept_by_foe {
        push(b, Instruction::SetSleptByFoe { side, slot, previous: false, new: true });
    }
}

/// PS `mustrecharge.onBeforeMove` (data/conditions.ts:364-373): the recharge turn removes BOTH
/// the `mustrecharge` volatile and — explicitly — the holder's `truant` volatile, then returns
/// null. Truant's own `onBeforeMove` (priority 9) never runs, because `runEvent` breaks on
/// mustrecharge's falsy return at priority 11; PS clears it by hand so a Slaking that spent its
/// recharge turn is NOT also loafing on the turn after.
fn clear_recharge_volatiles(b: &mut Branch, side: SideId) {
    for v in [VolatileStatus::MustRecharge, VolatileStatus::Truant] {
        if b.state.side(side).volatiles.contains(v) {
            push(b, Instruction::RemoveVolatile { side, volatile: v });
        }
    }
}

/// Rampage moves lock the user in for 2-3 turns total, then confuse it.
fn is_rampage_move(id: crate::ids::MoveId) -> bool {
    matches!(id.to_id(), "outrage" | "petaldance" | "thrash" | "ragingfury")
}

/// A rampage use that FAILED to connect — missed (Hustle Outrage), was Protect-blocked, had no
/// living target, or hit an immune target. PS never removes `lockedmove` on a move failure:
/// the volatile's `duration` is 1 at the start of every locked turn and is re-armed to 2 only
/// by `onRestart`, which fires from the move's ON-HIT `self: {volatileStatus: 'lockedmove'}`.
/// A use that does not connect therefore leaves `duration === 1`, and `lockedmove.onAfterMove`
/// — which `runMove` fires unconditionally after `useMove` (sim/battle-actions.ts:311-312) —
/// calls `removeVolatile`, i.e. `onEnd`. `onEnd` confuses when `trueDuration <= 1`
/// (data/conditions.ts:277-279). So a failed rampage on the FINAL locked turn still confuses,
/// with the `random(2, 6)` duration draw at the move's stream position; only a failure with
/// rampage turns still to run drops the lock silently. A FIRST use never locks at all.
///
/// Returns branches because the confusion duration splits.
fn end_rampage_on_fail(mut b: Branch, side: SideId, move_id: crate::ids::MoveId) -> Vec<Branch> {
    use crate::state::PendingMove;
    if !is_rampage_move(move_id) {
        return vec![b];
    }
    let pending = b.state.side(side).pending_move;
    let final_turn = matches!(pending, PendingMove::Rampaging(m, n) if m == move_id && n <= 1);
    if matches!(pending, PendingMove::Rampaging(m, _) if m == move_id) {
        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
    }
    if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
    }
    if final_turn
        && b.state.side(side).active().is_alive()
        && !b.state.side(side).volatiles.contains(VolatileStatus::Confusion)
        && b.state.side(side).active().ability != crate::ids::Ability::OwnTempo
    {
        push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Confusion });
        let mut branches = branch_confusion_counter(b, side);
        for nb in &mut branches {
            consume_lum_if_statused(nb, side);
        }
        return branches;
    }
    vec![b]
}

/// PS applies a rampage move's `self: { volatileStatus: 'lockedmove' }` in `selfDrops`
/// (sim/battle-actions.ts:1117 — step 4 of `spreadMoveHit`, after `onHit` and BEFORE the
/// target's secondaries at step 5 and the `DamagingHit` contact abilities that follow). The
/// `lockedmove` `onStart` rolls `this.random(2, 4)` (data/conditions.ts:264-267), so that draw
/// sits at the SELF-DROP position in the stream, not at the end of the move.
///
/// Only a CONNECTING hit gets here — an immune/missed/blocked use never runs `selfDrops`, so it
/// never starts a lock. A continuation instead hits `onRestart`, which rolls nothing.
fn start_rampage_lock(b: Branch, side: SideId, move_id: crate::ids::MoveId) -> Vec<Branch> {
    use crate::state::PendingMove;
    if !is_rampage_move(move_id)
        || b.state.side(side).pending_move != PendingMove::None
        || !b.state.side(side).active().is_alive()
    {
        return vec![b];
    }
    // `trueDuration` is the mid-turn (kernel) value {2,3}; the end-of-turn `onResidual`
    // decrements it, so the terminal snapshot is {1,2}.
    [2u8, 3]
        .into_iter()
        .map(|rem| {
            let mut nb = scaled(&b, 0.5);
            draw(&mut nb, "random", &[2, 4], rem as i64, "lockedmove");
            push(&mut nb, Instruction::SetPendingMove {
                side,
                previous: PendingMove::None,
                new: PendingMove::Rampaging(move_id, rem),
            });
            if !nb.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                push(&mut nb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::LockedMove });
            }
            nb
        })
        .collect()
}

/// Apply rampage lock state transitions to every outcome branch after the move resolves.
/// (Approximation: a miss mid-rampage should end the lock without confusion; this treats
/// all branches alike.)
fn apply_rampage_state(out: Vec<Branch>, side: SideId, move_id: crate::ids::MoveId) -> Vec<Branch> {
    use crate::state::PendingMove;
    if !is_rampage_move(move_id) {
        return out;
    }
    // A failed rampage (immune target) doesn't lock; mid-rampage it ends without confusion.
    let failed = {
        let probe = out.first();
        probe.is_some_and(|b| {
            let foe = side.other();
            let t = b.state.side(foe).active();
            t.is_alive() && crate::damage::type_multiplier(move_data(move_id).typ, t.types) == 0.0
        })
    };
    if failed {
        // Same rule as every other non-connecting use: the lock ends here, and on the FINAL
        // locked turn `lockedmove.onEnd` still confuses (see `end_rampage_on_fail`).
        return out
            .into_iter()
            .flat_map(|b| {
                if matches!(b.state.side(side).pending_move, PendingMove::Rampaging(m, _) if m == move_id) {
                    end_rampage_on_fail(b, side, move_id)
                } else {
                    vec![b]
                }
            })
            .collect();
    }
    out.into_iter()
        .flat_map(|mut b| {
            if !b.state.side(side).active().is_alive() {
                return vec![b];
            }
            let pending = b.state.side(side).pending_move;
            match pending {
                PendingMove::Rampaging(m, n) if m == move_id => {
                    if n >= 2 {
                        // Continuation with turns still to come: this is a mid-turn (kernel)
                        // snapshot, so `trueDuration` keeps its start-of-turn value here — the
                        // per-use decrement is PS's end-of-turn `onResidual` (applied in
                        // `apply_end_of_turn`), so the move action leaves it unchanged.
                        vec![b]
                    } else {
                        // n == 1: the final locked turn. PS removes the volatile in `onAfterMove`
                        // (duration hit 1) and `onEnd` confuses — both move-time (kernel) effects.
                        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
                        if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                            push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
                        }
                        if !b.state.side(side).volatiles.contains(VolatileStatus::Confusion)
                            && b.state.side(side).active().ability != crate::ids::Ability::OwnTempo
                        {
                            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Confusion });
                            // PS `confusion` `onStart` rolls the duration (`random(2,6)`) the instant
                            // confusion is applied — BEFORE any berry's `onUpdate` can cure it. So a
                            // Lum/Persim holder that snaps out immediately STILL consumes the duration
                            // draw (rd292 t3, r19 t7: Outrage's final turn confuses a Lum-holding
                            // Regidrago → PS logs `random[2,6]@confusion` then the berry cures it).
                            // Emit the duration draw + counter first, then apply the cure per branch
                            // (which resets the counter to 0 — all four branches converge to cured).
                            let mut branches = branch_confusion_counter(b, side);
                            for nb in &mut branches {
                                consume_lum_if_statused(nb, side);
                            }
                            return branches;
                        }
                        vec![b]
                    }
                }
                // Starting a rampage is NOT here: PS applies `self: {volatileStatus:'lockedmove'}`
                // in `selfDrops` (battle-actions.ts:1117, step 4 of `spreadMoveHit`), i.e. BEFORE
                // the target's secondaries and before the `DamagingHit` contact abilities — see
                // `start_rampage_lock`, called at the self-drop fork.
                _ => vec![b],
            }
        })
        .collect()
}

/// Heal Block (Psychic Noise): all HP restoration for this side's active is prevented.
fn heal_blocked(b: &Branch, side: SideId) -> bool {
    b.state.side(side).volatiles.contains(VolatileStatus::HealBlock)
}

fn is_grounded(state: &State, side: SideId) -> bool {
    let p = state.side(side).active();
    !(p.types.contains(&Type::Flying)
        || p.ability == crate::ids::Ability::Levitate
        || state.side(side).volatiles.contains(VolatileStatus::MagnetRise)
        || p.item == Item::AirBalloon)
}

/// Whether `side`'s active Pokémon is prevented from switching out (gen9 trapping). Fainted
/// actives are never trapped — they resolve through the always-legal faint-replacement phase.
///
/// Escape hatches checked first (they beat every trap, PS `onTrapPokemon` priority -10 Shed Shell
/// and the Ghost `trapped` type immunity that fails `tryTrap`): Ghost-types (including tera-Ghost)
/// and Shed Shell holders can always switch. Then self/foe trap volatiles, then the opposing
/// active's trapping abilities. Mirrors `Pokemon.tryTrap` + the ability `onFoeTrapPokemon` guards.
pub fn is_trapped(state: &State, side: SideId) -> bool {
    use crate::ids::Ability as Ab;
    let me = state.side(side).active();
    if !me.is_alive() {
        return false;
    }
    if me.types.contains(&Type::Ghost) || me.item == Item::ShedShell {
        return false;
    }
    let vols = state.side(side).volatiles;
    if vols.contains(VolatileStatus::PartiallyTrapped)
        || vols.contains(VolatileStatus::Trapped)
        || vols.contains(VolatileStatus::Octolock)
        || vols.contains(VolatileStatus::Ingrain)
        || vols.contains(VolatileStatus::NoRetreat)
    {
        return true;
    }
    let foe = state.side(side.other()).active();
    if foe.is_alive() {
        match foe.ability {
            // Arena Trap holds only grounded foes.
            Ab::ArenaTrap => return is_grounded(state, side),
            // Magnet Pull holds only Steel-types.
            Ab::MagnetPull => return me.types.contains(&Type::Steel),
            // Shadow Tag holds everyone except other Shadow Tag holders.
            Ab::ShadowTag => return me.ability != Ab::ShadowTag,
            _ => {}
        }
    }
    false
}

/// PS `pokemon.maybeTrapped` — the request-JSON flag that says "a switch here MIGHT be refused,
/// so the client must not let you take it back".
///
/// `nextTurn` (`sim/battle.ts:1723-1755`) resets `trapped = maybeTrapped = false`, runs
/// `TrapPokemon` and `MaybeTrapPokemon` (which pick up the foe's REAL ability through
/// `onFoeTrapPokemon` / `onFoeMaybeTrapPokemon`), and then sweeps **every ability the foe's
/// apparent species could legally have** with `singleEvent('FoeMaybeTrapPokemon', ability, …)`.
/// That sweep is skipped entirely by:
///
/// ```ts
/// if ((ruleTable.has('+hackmons') || !ruleTable.has('obtainableabilities')) && !this.format.team) continue;
/// ```
///
/// `[Gen 9] Custom Game` satisfies BOTH halves — no `obtainableabilities`, no `format.team` — so
/// our entire recorded corpus has never seen it. `[Gen 9] Random Battle` fails both (Obtainable
/// is in its ruleset and `team: 'random'`), so it runs. This is a genuine customgame->randbats
/// delta in the request layer, hence `Ruleset::infer_foe_trapping_abilities`.
///
/// No PRNG (`singleEvent` has no handler list and never speed-sorts) — request shape only. And
/// `maybeTrapped` does NOT reject a switch; only `trapped` does. Conflating the two is wrong the
/// moment this flag can be set by an ability the foe does not actually have.
///
/// The three `onFoeMaybeTrapPokemon` handlers (`data/abilities.ts:203, 2477, 4117`) each add
/// `isAdjacent` (always true in singles) to the same condition their real trap uses, with
/// `!pokemon.knownType` disjuncts that are dead here — types are public in our model.
pub fn maybe_trapped(state: &State, side: SideId) -> bool {
    use crate::ids::Ability as Ab;
    if is_trapped(state, side) {
        return true;
    }
    if !state.ruleset.infer_foe_trapping_abilities {
        return false;
    }
    let me = state.side(side).active();
    if !me.is_alive() || me.types.contains(&Type::Ghost) || me.item == Item::ShedShell {
        return false;
    }
    let foe = state.side(side.other()).active();
    if !foe.is_alive() {
        return false;
    }
    // **The sweep reads the foe's APPARENT species** — `const species = (source.illusion ||
    // source).species` (`sim/battle.ts:1732`). A disguised Zoroark therefore offers the DISGUISE's
    // ability list to the inference, which is the one place Illusion changes a gate-visible field.
    let foe_apparent = state.side(side.other()).apparent_active();
    // PS skips the ability the foe ACTUALLY has ("pokemon event was already run above"), which is
    // exactly the `is_trapped` case handled at the top.
    species_possible_trap_abilities(foe_apparent.species)
        .iter()
        .filter(|&&ab| ab != foe.ability)
        .any(|&ab| match ab {
            Ab::ArenaTrap => is_grounded(state, side),
            Ab::MagnetPull => me.types.contains(&Type::Steel),
            Ab::ShadowTag => me.ability != Ab::ShadowTag,
            _ => false,
        })
}

/// Every gen-9 species carrying Arena Trap / Shadow Tag / Magnet Pull in ANY ability slot,
/// enumerated from the pinned dex (`b9dc987d`) with PS's own two skips applied:
/// `abilitySlot === 'H' && species.unreleasedHidden` (none of these), and
/// `ruleTable.has('-ability:…')` (randbats bans no abilities).
///
/// Hardcoded rather than generated because the whole table is 17 species and the alternative is
/// widening the 9k-line generated `gen.rs` for three abilities. The `*Past` entries
/// (Gengar-Mega, Wobbuffet, Wynaut, Meltan) cannot be produced by the gen-9 randbats generator;
/// they are listed so the function is a property of the DEX, not of one generator's pool.
fn species_possible_trap_abilities(sp: crate::ids::Species) -> &'static [crate::ids::Ability] {
    use crate::ids::Ability as Ab;
    const ARENA: &[Ab] = &[Ab::ArenaTrap];
    const SHADOW: &[Ab] = &[Ab::ShadowTag];
    const MAGNET: &[Ab] = &[Ab::MagnetPull];
    match sp.to_id() {
        "diglett" | "dugtrio" | "trapinch" => ARENA,
        "gothita" | "gothorita" | "gothitelle" | "wobbuffet" | "wynaut" | "gengarmega" => SHADOW,
        "geodudealola" | "graveleralola" | "golemalola" | "magnemite" | "magneton" | "nosepass"
        | "magnezone" | "probopass" | "meltan" => MAGNET,
        _ => &[],
    }
}

/// Clear the foe-sourced traps (partial trap / Mean Look-family / Octolock) on `victim`'s active
/// when the trapper leaves the field. In singles the trapper is always the current opposing
/// active, so any switch-out of it ends these; self-traps (Ingrain / No Retreat) are untouched.
fn clear_foe_sourced_traps(b: &mut Branch, victim: SideId) {
    for v in [VolatileStatus::PartiallyTrapped, VolatileStatus::Trapped, VolatileStatus::Octolock] {
        if b.state.side(victim).volatiles.contains(v) {
            push(b, Instruction::RemoveVolatile { side: victim, volatile: v });
        }
    }
    let pt = (b.state.side(victim).partial_trap_turns, b.state.side(victim).partial_trap_div);
    if pt != (0, 0) {
        push(b, Instruction::SetPartialTrap { side: victim, previous: pt, new: (0, 0) });
    }
}

/// Move accuracy as a hit probability, with the modifiers that change it: No Guard (either
/// side) and weather-perfect moves -> always hit; Compound Eyes ×1.3, Wide Lens ×1.1,
/// Hustle ×0.8 on physical.
fn accuracy_of(b: &Branch, side: SideId, md: &crate::data::MoveData) -> f32 {
    use crate::ids::Ability as Ab;
    if md.accuracy == 0 {
        return 1.0;
    }
    let atk = b.state.side(side).active();
    let def = b.state.side(side.other()).active();
    if atk.ability == Ab::NoGuard || def.ability == Ab::NoGuard {
        return 1.0;
    }
    // A target still carrying its Glaive Rush drawback cannot avoid the attack
    // (PS condition `onAccuracy` returns true).
    if b.state.side(side.other()).volatiles.contains(VolatileStatus::GlaiveRush) {
        return 1.0;
    }
    let id = md.id.to_id();
    // gen >= 8: Toxic used by a Poison-type NEVER misses (`accuracy = true`,
    // battle-actions.ts:726) — a No Guard-shaped override, not a numeric 100.
    if id == "toxic" && atk.types.contains(&Type::Poison) {
        return 1.0;
    }
    // Weather-perfect (forced-`true`) accuracy — always hits. (Sun-halved Thunder/Hurricane is a
    // NUMERIC 50 and flows through `accuracy_numerator` below.)
    match (id, effective_weather(&b.state)) {
        ("blizzard", Weather::Snow) => return 1.0,
        ("thunder" | "hurricane" | "bleakwindstorm" | "wildboltstorm" | "sandsearstorm", Weather::Rain | Weather::HeavyRain) => return 1.0,
        _ => {}
    }
    (accuracy_numerator(b, side, md) as f32 / 100.0).min(1.0)
}

/// PS `chainModify` step: fold a `next`/4096 factor into a running `prev`/4096 modifier with
/// PS's round-half-up (`(prev*next + 2048) >> 12`). Both args are 4096-fixed-point (4096 = ×1).
fn chain_mod(prev: i64, next: i64) -> i64 {
    (prev * next + 2048) >> 12
}

/// Whether a move ignores the target's evasion boost (`ignoreEvasion: true`, data/moves.ts) —
/// its accuracy stage combines only the attacker's accuracy boost, not the target's evasion.
fn move_ignores_evasion(id: crate::ids::MoveId) -> bool {
    matches!(id.to_id(), "chipaway" | "darkestlariat" | "nihillight" | "sacredsword")
}

/// Whether a move ignores the target's DEFENSIVE stage (`ignoreDefensive: true`, data/moves.ts).
/// The same four moves carry both flags, but they are two different PS fields and only the
/// evasion one was wired: `getDamage` sets `defBoosts = 0` outright (`battle-actions.ts:1701`),
/// for a negative stage as well as a positive one. rb1781 d5: Sacred Sword into a +1 Def
/// Krookodile — PS deals 87, the engine divided through the 1.5x and dealt 59.
fn move_ignores_defensive(id: crate::ids::MoveId) -> bool {
    matches!(id.to_id(), "chipaway" | "darkestlariat" | "nihillight" | "sacredsword")
}

/// The exact integer numerator PS passes to `randomChance(accuracy, 100)` at `hitStepAccuracy`:
/// `move.accuracy` after `onModifyMove` (Hustle physical ×0.8, sun Thunder/Hurricane = 50), the
/// `ModifyAccuracy` ×4096 chain (Compound Eyes 5325/4096, Wide Lens 4505/4096 — applied to the
/// raw accuracy BEFORE the stage boosts), and the accuracy/evasion STAGE boosts
/// (`trunc(acc*(3+b)/3)` up / `trunc(acc*3/(3-b))` down, with `b = clamp(acc_stage) then
/// clamp(b - eva_stage)`). May exceed 100 (the caller caps the probability). Only meaningful for
/// a numeric-accuracy move that isn't forced-`true`. PS ref: battle-actions.ts:685 hitStepAccuracy.
fn accuracy_numerator(b: &Branch, side: SideId, md: &crate::data::MoveData) -> i32 {
    use crate::ids::Ability as Ab;
    let atk = b.state.side(side).active();
    let foe = side.other();
    // --- onModifyMove (runs before hitStepAccuracy) ---
    let mut acc: i64 = if matches!(
        (md.id.to_id(), effective_weather(&b.state)),
        ("thunder" | "hurricane", Weather::Sun | Weather::HarshSun)
    ) {
        50
    } else if atk.ability == Ab::Hustle && md.category == MoveCategory::Physical {
        // Move accuracies are multiples of 5 → ×4/5 is exact and integer.
        md.accuracy as i64 * 4 / 5
    } else {
        md.accuracy as i64
    };
    // --- runEvent('ModifyAccuracy'): attacker item/ability ×4096 chain ---
    let mut modf: i64 = 4096; // running event.modifier (4096-fixed; 4096 = ×1)
    if atk.ability == Ab::CompoundEyes {
        modf = chain_mod(modf, 5325); // 5325/4096 ≈ ×1.3
    }
    if atk.item == Item::WideLens {
        modf = chain_mod(modf, 4505); // 4505/4096 ≈ ×1.1
    }
    if modf != 4096 {
        acc = crate::damage::modify(acc, modf, 4096);
    }
    // --- accuracy / evasion stage boosts (combined, clamped -6..6) ---
    let mut boost: i64 = (b.state.side(side).boost(BoostIndex::Accuracy) as i64).clamp(-6, 6);
    if !move_ignores_evasion(md.id) {
        boost = (boost - b.state.side(foe).boost(BoostIndex::Evasion) as i64).clamp(-6, 6);
    }
    if boost > 0 {
        acc = acc * (3 + boost) / 3; // trunc
    } else if boost < 0 {
        acc = acc * 3 / (3 - boost); // trunc
    }
    acc as i32
}

/// The integer numerator PS passes to `randomChance(accuracy, 100)` at `hitStepAccuracy`. Only
/// ever called when a numeric-accuracy draw is actually emitted (`md.accuracy != 0`, not
/// forced-true), so it delegates straight to the exact `accuracy_numerator`.
fn accuracy_arg(b: &Branch, side: SideId, md: &crate::data::MoveData) -> i32 {
    accuracy_numerator(b, side, md)
}

/// Counter-family damage-return moves (Counter / Mirror Coat / Metal Burst / Comeuppance) fail at
/// PS `onTry` — BEFORE `hitStepAccuracy` — when no qualifying damage was taken this turn, so PS
/// makes NO accuracy draw (nor crit/damage; the fixed-damage path emits none of those anyway). The
/// engine otherwise reaches the accuracy branch and rolls a phantom `randomChance(100,100)`. Gate
/// the accuracy annotation on this so the failing move emits nothing (annotation-only; the failing
/// move already deals 0 damage, so state is unchanged).
fn counter_family_ontry_fails(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    let s = b.state.side(side);
    match md.id.to_id() {
        "counter" => s.physical_damage_taken <= 0,
        "mirrorcoat" => s.special_damage_taken <= 0,
        "metalburst" | "comeuppance" => s.physical_damage_taken <= 0 && s.special_damage_taken <= 0,
        _ => false,
    }
}

/// Whether PS overrides a move's accuracy to `true` (bypassing the `hitStepAccuracy` roll
/// entirely) via an `Accuracy`/`ModifyMove` event, as opposed to a numeric accuracy that merely
/// evaluates to 100. A `true` override means PS makes NO accuracy draw — but a later crit /
/// damage roll still happens, so the engine must not emit an accuracy draw here. Cases:
///   * No Guard on either side (`onAnyAccuracy` returns true),
///   * a Glaive Rush target (its volatile's `onAccuracy` returns true),
///   * **gen >= 8 Toxic used by a Poison-type** (`battle-actions.ts:726`, hard-coded into
///     `hitStepAccuracy` alongside `move.alwaysHit`),
///   * weather-perfect accuracy: Blizzard in snow, and Thunder/Hurricane/Bleakwind Storm/
///     Wildbolt Storm/Sandsear Storm in rain (`onModifyMove` sets `move.accuracy = true`).
/// The sun case for Thunder/Hurricane sets a NUMERIC 50 (still rolls) and is deliberately absent.
/// A plain 100-accuracy move (Close Combat, Poltergeist) is NOT forced true — it rolls
/// `randomChance(100, 100)`.
fn accuracy_forced_true(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    use crate::ids::Ability as Ab;
    let atk = b.state.side(side).active();
    let def = b.state.side(side.other()).active();
    if atk.ability == Ab::NoGuard || def.ability == Ab::NoGuard {
        return true;
    }
    if b.state.side(side.other()).volatiles.contains(VolatileStatus::GlaiveRush) {
        return true;
    }
    if md.id.to_id() == "toxic" && atk.types.contains(&Type::Poison) {
        return true;
    }
    matches!(
        (md.id.to_id(), effective_weather(&b.state)),
        ("blizzard", Weather::Snow)
            | (
                "thunder" | "hurricane" | "bleakwindstorm" | "wildboltstorm" | "sandsearstorm",
                Weather::Rain | Weather::HeavyRain
            )
    )
}

/// Public entry point. `s1`/`s2` are side one's and side two's chosen actions.
pub fn generate_instructions(state: &State, s1: MoveChoice, s2: MoveChoice) -> Vec<StateInstructions> {
    generate_instructions_ex(state, s1, s2, [None, None], [false, false])
}

/// Execution mode for the turn resolver.
///
/// `Enumerate` expands every stochastic fork with exact probabilities (verification, search).
/// `Sample` follows ONE weighted path: at each stage seam the branch set is pruned to a single
/// survivor drawn ∝ probability, carrying the stage's total incoming mass. This is *exact
/// ancestral sampling* over the same probability tree — certified against `Enumerate` by
/// `tests/sampled_distribution.rs` — and is the training-throughput path: it avoids the
/// cross-product of both movers' damage/crit/secondary branches (the dominant step cost).
pub enum Exec {
    Enumerate,
    /// splitmix64 state; advanced on every draw.
    Sample(u64),
}

impl Exec {
    fn next_unit(&mut self) -> Option<f32> {
        match self {
            Exec::Enumerate => None,
            Exec::Sample(s) => {
                *s = s.wrapping_add(0x9E37_79B9_7F4A_7C15);
                let mut z = *s;
                z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
                z ^= z >> 31;
                Some((z >> 40) as f32 / (1u64 << 24) as f32)
            }
        }
    }

    /// 50/50 coin for symmetric forks (speed ties). None when enumerating.
    pub(crate) fn coin(&mut self) -> Option<bool> {
        self.next_unit().map(|u| u < 0.5)
    }

    /// In `Sample` mode, reduce `branches` to one survivor drawn ∝ `prob`, re-weighted to the
    /// stage's total incoming mass (so overall mass is conserved and the final sampled branch
    /// comes out at the parent's weight). Identity in `Enumerate` mode.
    pub(crate) fn prune(&mut self, mut branches: Vec<Branch>) -> Vec<Branch> {
        if branches.len() <= 1 {
            return branches;
        }
        let Some(u) = self.next_unit() else { return branches };
        let total: f32 = branches.iter().map(|b| b.prob).sum();
        let mut r = u * total;
        let mut idx = branches.len() - 1;
        for (i, b) in branches.iter().enumerate() {
            r -= b.prob;
            if r <= 0.0 {
                idx = i;
                break;
            }
        }
        let mut survivor = branches.swap_remove(idx);
        survivor.prob = total;
        vec![survivor]
    }
}

/// Sampled single-path turn resolution: same rules, same probability tree as
/// [`generate_instructions_ex`], but follows one weighted trajectory and returns it alone.
/// `rng` is splitmix64 state, advanced in place. ~10-30× cheaper than full enumeration.
pub fn generate_instructions_sampled(
    state: &State,
    s1: MoveChoice,
    s2: MoveChoice,
    pivot: [Pivot; 2],
    tera: [bool; 2],
    rng: &mut u64,
) -> StateInstructions {
    // Realize multi-hit moves instead of enumerating their per-hit product. Sample follows one
    // path, so the product is pure waste — see `RealizedSource::Splitmix`. Seeded from `rng` so
    // the run stays reproducible from its seed, and folded back below so the stream advances.
    //
    // Save/restore rather than clear: the seed gate installs a `Prng` source around its own
    // generation, and must get it back if anything ever calls this from inside that scope.
    let prev = REALIZED_SOURCE.with(|c| c.borrow_mut().take());
    SPLITMIX.with(|c| c.set(rng.wrapping_mul(0xD6E8_FEB8_6659_FD93).rotate_left(17)));
    set_realized_source(Some(RealizedSource::Splitmix));

    let mut exec = Exec::Sample(*rng);
    let mut out = generate_instructions_ctx(state, s1, s2, pivot, tera, &mut exec);
    if let Exec::Sample(s) = exec {
        *rng = s ^ SPLITMIX.with(|c| c.get());
    }
    set_realized_source(prev);
    debug_assert_eq!(out.len(), 1);
    out.pop().unwrap_or(StateInstructions { percentage: 100.0, instructions: Vec::new() })
}

/// Generate the conditional stochastic kernel for one queued move action only.
///
/// Unlike [`generate_instructions_ex`], this does not resolve ordering, execute the other
/// side's action, or run end-of-turn residuals. It exists both for the request-model state
/// machine and for factorized co-simulation: equality of each queued action's conditional
/// distribution proves equality of their composition without materializing the Cartesian
/// product of both moves' damage/crit/secondary branches.
pub fn generate_move_action(
    state: &State,
    side: SideId,
    move_idx: u8,
    pivot: Option<u8>,
    foe_pending_move: Option<crate::ids::MoveId>,
) -> Vec<StateInstructions> {
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
    // In the full turn resolver the queue suppresses a flinched action before calling the move
    // executor. The factorized/request-model entry point must preserve that same boundary.
    if state.side(side).volatiles.contains(VolatileStatus::Flinch) {
        return vec![StateInstructions { percentage: 100.0, instructions: Vec::new() }];
    }
    execute_move(start, Action {
        shell_phys: None,
        side,
        move_idx,
        pivot: pivot.map_or(Pivot::Stay, Pivot::Target),
        foe_pending_move,
        custap: false,
        struggling: no_usable_move(state, side),
        external_move: None,
        called: false,
    })
    .into_iter()
    .map(|b| StateInstructions { percentage: b.prob, instructions: b.ins })
    .collect()
}

/// Apply Terastallization to a side's active at turn start: its types become its tera type
/// (Stellar keeps the original types) and the terastallized flag flips. Done before moves so
/// the new typing affects both its own STAB and the damage it takes this turn.
pub(crate) fn apply_tera(b: &mut Branch, side: SideId) {
    let (already, tera_type, prev, slot) = {
        let s = b.state.side(side);
        let p = s.active();
        (p.terastallized, p.tera_type, p.types, s.active_index)
    };
    if already || b.state.side(side).tera_used {
        return;
    }
    if tera_type != Type::None && tera_type != Type::Stellar {
        // EFFECTIVE typing only. PS never rewrites `pokemon.types` on Terastallization —
        // `getTypes()` short-circuits on `terastallized` (`sim/pokemon.ts:2139`) — and the
        // pre-tera live list is exactly what `isSTAB`'s `getTypes(false, true)` reads, so it
        // must SURVIVE this.
        push(b, Instruction::ChangeTypes { side, slot, previous: prev, new: [tera_type, Type::None] });
    }
    push(b, Instruction::ToggleTerastallized { side, slot });
    // Hidden-info: Terastallizing reveals the Tera type to the foe.
    reveal(b, side, 0, crate::state::Reveal::TERA);
    apply_tera_forme(b, side);
}

/// The two species whose Terastallization is also a PERMANENT forme change
/// (`battle-actions.ts:1935` `terastallize`, right after `pokemon.terastallized = type`):
///
/// * every Ogerpon mask becomes `<forme>tera` (plain Ogerpon becomes `ogerpontealtera`), gaining
///   `Embody Aspect (...)`. That ability's `onStart` fires here because `formeChange` calls
///   `setAbility(ability, null, null, /* isTransform */ true)` and `singleEvent('Start')` runs
///   for `!isTransform || ability.flags['notransform']` — EmbodyAspect carries `notransform`.
///   The boost is +1 Spe / SpD / Atk / Def for Teal / Wellspring / Hearthflame / Cornerstone,
///   once (`effectState.embodied`), and the four Ogerpon formes share base stats so no respread
///   is observable.
/// * `Terapagos-Terastal` becomes `Terapagos-Stellar`, which is a real stat change (base HP
///   90 -> 160, and every other base stat rises). `formeChange`'s `updateMaxHp` keeps the damage
///   taken: `hp = max(1, newMaxHP - (maxhp - hp))` — exactly what `Instruction::Transform` does
///   with a changed `stats[0]`. Its ability becomes Teraform Zero, whose `onAfterTerastallization`
///   (`data/abilities.ts`) clears weather AND terrain when either is up — PS fires it via
///   `runEvent('AfterTerastallization')` at the very end of `terastallize`.
/// The stat `Embody Aspect (...)`'s `onStart` raises, or `None` for any other ability.
fn embody_aspect_stat(ability: crate::ids::Ability) -> Option<BoostIndex> {
    use crate::ids::Ability as A;
    match ability {
        A::EmbodyAspectTeal => Some(BoostIndex::Speed),
        A::EmbodyAspectWellspring => Some(BoostIndex::SpecialDefense),
        A::EmbodyAspectHearthflame => Some(BoostIndex::Attack),
        A::EmbodyAspectCornerstone => Some(BoostIndex::Defense),
        _ => None,
    }
}

/// One of the four `Ogerpon-*-Tera` formes.
fn species_is_ogerpon_tera(species: crate::ids::Species) -> bool {
    [
        "ogerpontealtera", "ogerponwellspringtera", "ogerponhearthflametera", "ogerponcornerstonetera",
        // Terapagos-Stellar carries `formeRegression` for the same reason (`formeChange` with no
        // `source`, `sim/pokemon.ts:1449-1452`) and regresses all the way to the SET species —
        // Terapagos, not Terapagos-Terastal — because PS restores from `set.species`.
        "terapagosstellar",
    ]
        .iter()
        .any(|n| crate::ids::Species::from_id(n) == Some(species))
}

/// A `formeRegression` forme (set by the Tera forme change) reverts when its holder FAINTS:
/// `battle.ts:2571` restores `baseSpecies`/`baseAbility` from the SET before `clearVolatile`,
/// whose `setSpecies(this.baseSpecies)` then puts the mask Ogerpon back on the bench. rb1135 t7
/// and rb1276 t10 both show PS's fainted Ogerpon back in its non-Tera forme while the engine
/// kept `-Tera`. Ogerpon's four formes share base stats, so this is a pure species/ability swap
/// with no max-HP move (Terapagos-Stellar's regression WOULD move max HP and is NOT handled
/// here — `Instruction::Transform`'s hp carry-over is not defined for a fainted mon).
fn regress_fainted_tera_formes(b: &mut Branch) {
    for side in [SideId::One, SideId::Two] {
        for slot in 0..6u8 {
            let p = &b.state.side(side).pokemon[slot as usize];
            if p.is_alive() || p.transformed || !species_is_ogerpon_tera(p.species) {
                continue;
            }
            let previous = crate::instruction::TransformData {
                species: p.species,
                stats: p.stats,
                types: p.types,
                live_types: p.live_types,
                ability: p.ability,
                moves: p.moves,
                transformed: p.transformed,
                times_hit: p.times_hit,
            };
            let mut new = previous;
            new.species = p.base_species;
            new.ability = p.base_ability;
            // PS restores from the SET (`battle.ts:2573`: `dex.species.get(pokemon.set.species ||
            // pokemon.set.name)`), not from `baseSpecies`. For Ogerpon the two agree. For Terapagos
            // they do NOT: Tera Shift's own permanent forme change already moved `baseSpecies` to
            // Terapagos-Terastal, so a fainted Terapagos-Stellar goes back to plain **Terapagos**
            // with **Tera Shift**, two steps down. rb1040 d10: the engine stopped at
            // Terapagos-Terastal (791) where PS has Terapagos (789).
            if Some(previous.species) == crate::ids::Species::from_id("terapagosstellar") {
                if let Some(base) = crate::ids::Species::from_id("terapagos") {
                    new.species = base;
                    new.ability = crate::ids::Ability::TeraShift;
                }
            }
            if new.species == previous.species && new.ability == previous.ability {
                continue;
            }
            // Ogerpon's four Tera formes share a base-stat line; **Terapagos-Stellar does not** —
            // base HP 160 back down to Terapagos's 90 — and `formeChange` ends in `updateMaxHp`.
            // Randbats spread (31 IV / 85 EV / neutral), the same assumption the Tera Shift entry
            // forme change makes.
            let (ob, nb) = (crate::data::base_stats(previous.species), crate::data::base_stats(new.species));
            if ob != nb {
                new.stats = respread_stats(ob, nb, p.stats, p.level);
                if ob[0] != nb[0] {
                    new.stats[0] = crate::damage::compute_hp(nb[0], 31, 85, p.level);
                }
            }
            let previous_base_moves = p.base_moves;
            push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
        }
    }
}

fn apply_tera_forme(b: &mut Branch, side: SideId) {
    use crate::ids::Ability as A;
    use crate::ids::Species as Sp;
    let (species, level, stats) = {
        let p = b.state.side(side).active();
        (p.species, p.level, p.stats)
    };
    let sid = |name: &str| Sp::from_id(name).unwrap_or(Sp::None);
    let ogerpon: Option<(Sp, A, BoostIndex)> = if species == sid("ogerpon") {
        Some((sid("ogerpontealtera"), A::EmbodyAspectTeal, BoostIndex::Speed))
    } else if species == sid("ogerponwellspring") {
        Some((sid("ogerponwellspringtera"), A::EmbodyAspectWellspring, BoostIndex::SpecialDefense))
    } else if species == sid("ogerponhearthflame") {
        Some((sid("ogerponhearthflametera"), A::EmbodyAspectHearthflame, BoostIndex::Attack))
    } else if species == sid("ogerponcornerstone") {
        Some((sid("ogerponcornerstonetera"), A::EmbodyAspectCornerstone, BoostIndex::Defense))
    } else {
        None
    };
    let (new_species, new_ability, boost) = match ogerpon {
        Some(x) => x,
        None if species == sid("terapagosterastal") => {
            (sid("terapagosstellar"), A::TeraformZero, BoostIndex::Attack)
        }
        None => return,
    };
    if new_species == Sp::None {
        return;
    }
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = new_species;
    let (old_base, new_base) = (crate::data::base_stats(species), crate::data::base_stats(new_species));
    new.stats = respread_stats(old_base, new_base, stats, level);
    // `respread_stats` never re-derives HP (every battle-only forme shares its base forme's HP
    // base) — but Terapagos-Stellar does NOT: base HP 90 -> 160. PS's `formeChange` calls
    // `updateMaxHp`, which recomputes maxhp and preserves the damage taken; the engine's
    // `Instruction::Transform` does the same when `stats[0]` moves. Random-battle spread
    // (31 IV / 85 EV / neutral), exactly as the Tera Shift entry forme change assumes.
    if new_base[0] != old_base[0] {
        new.stats[0] = crate::damage::compute_hp(new_base[0], 31, 85, level);
    }
    new.ability = new_ability;
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
    if new_ability == A::TeraformZero {
        // Teraform Zero clears BOTH field effects, and only when one is up.
        if b.state.weather != Weather::None {
            set_weather(b, Weather::None, 0);
        }
        if b.state.terrain != crate::ids::Terrain::None {
            emit_field_change_shuffle(b);
            push(b, Instruction::ChangeTerrain {
                previous: b.state.terrain,
                previous_turns: b.state.terrain_turns,
                new: crate::ids::Terrain::None,
                new_turns: 0,
            });
            refresh_proto_quark(b);
        }
    } else {
        raise_boost(b, side, boost, 1);
    }
}

/// Like [`generate_instructions`], but `pivot` gives each side's switch-in target for a
/// pivot move (U-turn): when that move connects and the user survives, it switches out
/// mid-turn — so a faster pivot's switch happens *before* the opponent's move. Used by
/// the differential harness, which knows the recorded replacement target.
pub fn generate_instructions_ex(state: &State, s1: MoveChoice, s2: MoveChoice, pivot: [Option<u8>; 2], tera: [bool; 2]) -> Vec<StateInstructions> {
    let pv = [pivot[0].map_or(Pivot::Stay, Pivot::Target), pivot[1].map_or(Pivot::Stay, Pivot::Target)];
    generate_instructions_ctx(state, s1, s2, pv, tera, &mut Exec::Enumerate)
}

pub(crate) fn generate_instructions_ctx(state: &State, s1: MoveChoice, s2: MoveChoice, pivot: [Pivot; 2], tera: [bool; 2], exec: &mut Exec) -> Vec<StateInstructions> {
    generate_branches_ctx(state, s1, s2, pivot, tera, exec)
        .into_iter()
        .map(|b| StateInstructions { percentage: b.prob, instructions: b.ins })
        .collect()
}

/// One fully-resolved outcome with its probability, the instructions that produced it, and the
/// ordered PRNG draw stream the engine would consume to realize it (PS call form). Returned by
/// [`generate_instructions_annotated`] for the draw-consumption differ.
pub struct AnnotatedOutcome {
    pub percentage: f32,
    pub instructions: Vec<Instruction>,
    pub draws: Vec<DrawEvent>,
}

/// Like [`generate_instructions_ex`] but additionally returns, per outcome branch, the ordered
/// PRNG draws (PS call form) that produce it. Runs the full enumeration with draw annotation
/// enabled; the differ selects the branch reproducing PS's recorded `stateAfter` and compares
/// its `draws` against PS's recorded draw log. Enumerate/Sample behavior is unchanged: the
/// annotation guard is thread-local and scoped to this call only.
pub fn generate_instructions_annotated(
    state: &State,
    s1: MoveChoice,
    s2: MoveChoice,
    pivot: [Option<u8>; 2],
    tera: [bool; 2],
) -> Vec<AnnotatedOutcome> {
    let _guard = AnnotateGuard::enable();
    let pv = [pivot[0].map_or(Pivot::Stay, Pivot::Target), pivot[1].map_or(Pivot::Stay, Pivot::Target)];
    generate_branches_ctx(state, s1, s2, pv, tera, &mut Exec::Enumerate)
        .into_iter()
        .map(|b| AnnotatedOutcome { percentage: b.prob, instructions: b.ins, draws: b.draws })
        .collect()
}

fn generate_branches_ctx(state: &State, s1: MoveChoice, s2: MoveChoice, pivot: [Pivot; 2], tera: [bool; 2], exec: &mut Exec) -> Vec<Branch> {
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new(), move_failed: false , pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
    let mut branches = vec![start];

    let custap = custap_stage(&mut branches, state, s1, s2);

    // 0) Turn-start Update bracket (PS `commitChoices` sort + `beforeTurn` action + gen8 dynamic
    //    re-sort), emitted on the pre-switch board before any action runs — the leading
    //    `shuffle` draws of an equal-Speed turn. Annotation-only (no-op without draw annotation).
    for b in &mut branches {
        emit_turn_start_bracket(b, s1, s2, custap, tera);
    }

    // 1) Switches resolve before moves, in speed order when both sides switch (the slower
    //    side's switch-in ability resolves last and e.g. its weather wins).
    let switch_actions: Vec<(SideId, u8)> = [(SideId::One, s1), (SideId::Two, s2)]
        .into_iter()
        .filter_map(|(side, c)| match c {
            MoveChoice::Switch(t) => Some((side, t)),
            _ => None,
        })
        .collect();
    if switch_actions.len() == 2 {
        // A turn-action double switch resolves SEQUENTIALLY in speed order (PS: the `switch`
        // action queues a `runSwitch` at order 101, which preempts the slower side's pending
        // `switch` at order 103). So the faster side completes its full switch — including its
        // switch-in ability — while the slower side's OLD mon is still on the field; that
        // Intimidate/etc. lands on the outgoing mon (and is wiped when it leaves). Only the
        // slower side's switch-in ability sees a freshly-entered foe. This differs from a
        // double REPLACEMENT (both fainted), where `switch_into_pair` enters both then fires
        // abilities so each sees the other fresh mon.
        let pairs = [switch_actions[0], switch_actions[1]];
        for b in &mut branches {
            let mut order = pairs;
            let (sp0, sp1) = (effective_speed(&b.state, order[0].0), effective_speed(&b.state, order[1].0));
            if sp1 > sp0 {
                order.swap(0, 1);
            } else if sp1 == sp0 {
                // A double-switch Speed TIE is decided by `commitChoices`' `queue.sort()` shuffle
                // and is NOT state-neutral — it picks whose switch-in ability sees whose mon (see
                // `switch_order_tie`). Replicate forces the realized side; Enumerate/Sample keep
                // the deterministic side-One-first reading.
                if forced_tie_order() == Some(false) {
                    order.swap(0, 1);
                }
            }
            // A turn-action double switch is NOT batched: the `switch` action (order 103)
            // queues its `runSwitch` (order 101), which preempts the other side's pending
            // `switch` (103), so PS runs `switch(A), runSwitch(A), switch(B), runSwitch(B)`
            // interleaved. Each switch therefore fires the SAME full bracket as a single
            // switch (battle.ts:2881 switch-out :83, switch runAction 2882, runSwitch
            // getAllActive speedSort battle-actions.ts:182, runSwitch runAction 2882), and
            // each shuffle is gated on the CURRENT (incrementally-swapped) board's tie —
            // switch(B)'s switch-out Update sees A already swapped in. (c1 d45.)
            for &(side, target) in &order {
                emit_switch_pre_update(b); // switch-out :83 (pre-swap board)
                let pre = b.state;
                apply_switch(b, side, target);
                // All three sort on the Speed cached at `insertChoice({runSwitch})`, i.e. before
                // this switch's hazards and switch-in ability — see `switch_entry_speed`.
                emit_switch_bracket(b, &pre, side, target);
            }
        }
    } else {
        for (side, target) in switch_actions {
            for b in &mut branches {
                // Switch-out `eachEvent('Update')` (battle-actions.ts:83) — PRE-swap board.
                emit_switch_pre_update(b);
                let pre = b.state;
                apply_switch(b, side, target);
                // POST-swap switch bracket (each a `shuffle[2,0,2]` iff both actives alive and
                // equal effective_speed on the POST-swap board): the `switch` action's runAction
                // Update (battle.ts:2881), the `runSwitch` `getAllActive()` speedSort
                // (battle-actions.ts:182), and the `runSwitch` runAction Update (2881). PS runs
                // `switch` and `runSwitch` as two queue actions, each ending in a runAction Update.
                // (c2 d16: p2 switches Iron Valiant→Toxapex; pre-swap Garganacl vs QuarkDrive-boosted
                // Iron Valiant is untied so switch-out is skipped, post-swap Garganacl==Toxapex==106
                // ties → [Update, null, Update]. A pre-tied/post-untied switch gives the mirror
                // [BeforeTurn, Update, Update] from the turn-start bracket + switch-out Update.)
                // All three sort on the Speed cached at `insertChoice({runSwitch})`, i.e. before
                // this switch's hazards and switch-in ability — see `switch_entry_speed`.
                // (rb1021 d58: Magnezone switches into Sticky Web on a 151==151 tie; PS's cached
                // 151 ties and fires all three, the post-hazard live 100 does not.)
                emit_switch_bracket(b, &pre, side, target);
            }
        }
    }

    // 1.5) Terastallization happens at turn start (gen9), before moves, for staying mons. The
    //      `terastallize` action (order 106) sorts AFTER any `switch` (103), so its trailing
    //      `runAction` `eachEvent('Update')` (battle.ts:2882) speed-sorts the POST-switch board.
    for (i, side) in [SideId::One, SideId::Two].into_iter().enumerate() {
        if tera[i] && matches!([s1, s2][i], MoveChoice::Move(_)) {
            for b in &mut branches {
                apply_tera(b, side);
                emit_update(b); // tera action runAction Update (2882)
            }
        }
    }

    // 2) Moves, ordered by priority then effective speed (speed ties branch 50/50).
    let move_actions: Vec<Action> = [(SideId::One, s1, pivot[0], custap[0]), (SideId::Two, s2, pivot[1], custap[1])]
        .into_iter()
        .filter_map(|(side, c, pv, cu)| match c {
            MoveChoice::Move(idx) => Some(Action {
                side, move_idx: idx, pivot: pv, foe_pending_move: None, shell_phys: None, custap: cu,
                // Evaluated on the TURN-START board, which is when PS builds the request.
                struggling: no_usable_move(state, side),
                external_move: None, called: false,
            }),
            MoveChoice::Switch(_) => None,
        })
        .collect();
    debug_assert!(
        !(matches!(exec, Exec::Enumerate) && pivot.contains(&Pivot::Pause)),
        "Pivot::Pause is a request-flow (sampled) construct; enumeration paths must pass Stay/Target"
    );

    branches = resolve_moves(branches, &move_actions, exec);

    // A side's active was switched in this turn (chose to switch, or used a pivot move) — it
    // hasn't earned an end-of-turn Speed Boost yet.
    let switched = [
        matches!(s1, MoveChoice::Switch(_)) || pivot[0] != Pivot::Stay,
        matches!(s2, MoveChoice::Switch(_)) || pivot[1] != Pivot::Stay,
    ];

    // 3) End-of-turn residuals (deterministic) — skipped if the battle has ended (a side
    //    has no living Pokémon), matching PS, which stops the turn on a win.
    branches = branches
        .into_iter()
        .flat_map(|b| {
            if battle_over(&b.state) {
                vec![b]
            } else {
                apply_end_of_turn(b, switched)
                    .into_iter()
                    .map(|mut nb| {
                        // PS `nextTurn` resets `statsRaisedThisTurn` / `statsLoweredThisTurn` on
                        // every active after the residuals run (battle.ts:1675-1676) — drop the
                        // volatiles at the same boundary, but ONLY when PS actually reaches
                        // `nextTurn`. See `next_turn_reached`: an empty active slot means a
                        // replacement bracket follows and `clear_stats_raised_markers` makes the
                        // call there instead. Clearing here would fire BEFORE those replacements,
                        // which is the wrong side of PS's order (rb1529: Reuniclus's Intimidate
                        // marker was dropped at a residual PS never ran, three replacement
                        // switch-ins before the battle ended).
                        if !next_turn_reached(&nb.state) {
                            return nb;
                        }
                        for s in [SideId::One, SideId::Two] {
                            if nb.state.side(s).volatiles.contains(VolatileStatus::StatsRaisedThisTurn) {
                                push(&mut nb, Instruction::RemoveVolatile {
                                    side: s,
                                    volatile: VolatileStatus::StatsRaisedThisTurn,
                                });
                            }
                            if nb.state.side(s).volatiles.contains(VolatileStatus::StatsLoweredThisTurn) {
                                push(&mut nb, Instruction::RemoveVolatile {
                                    side: s,
                                    volatile: VolatileStatus::StatsLoweredThisTurn,
                                });
                            }
                        }
                        nb
                    })
                    .collect()
            }
        })
        .collect();
    branches = exec.prune(branches);

    branches
}

/// How many EXTRA `getAllActive()` speedSorts a forced replacement's switch-in abilities fire by
/// CHANGING the field.
///
/// PS's `runSwitch` (sim/battle-actions.ts:175-190) is speedSort → `fieldEvent('SwitchIn')` →
/// the switch action's trailing runAction Update — the 3-draw bracket `replacement_bracket_tied`
/// already accounts for. But a switch-in ability that sets weather or terrain runs
/// `Field.setWeather` / `setTerrain`, and each of those ENDS with
/// `eachEvent('WeatherChange')` / `eachEvent('TerrainChange')` (sim/field.ts:87 / :155) — one more
/// `getAllActive()` speedSort, i.e. one more tie-gated `shuffle[2,0,2]` INSIDE the bracket,
/// between its speedSort and its trailing Update. The move path emits these via
/// `emit_field_change_shuffle`; the gate's forced-replacement path applies the switch with
/// `switch_into` (state only) and so never saw them.
///
/// Witness rb1362 d21: a fainted p1 is replaced by a Drizzle Politoed on a Speed-tied board; PS
/// records FOUR `shuffle[2,0,2]`, the third tagged `drizzle`/`SwitchIn`. The engine consumed three
/// and ran one draw behind for the rest of the game (surfacing at d24 as a `randomChance@par`
/// class label — an OFFSET symptom, not a paralysis bug).
///
/// Counted by replaying the real `switch_into` on a clone and diffing the field, so the ability
/// table stays in one place (`apply_switch_in_ability`). Replacements are applied in order, so a
/// second switch-in that re-sets the SAME weather correctly counts nothing.
pub fn replacement_field_change_draws(state: &State, replacements: &[(SideId, u8)]) -> usize {
    let mut st = *state;
    let mut n = 0;
    for &(side, slot) in replacements {
        let (w, t) = (st.weather, st.terrain);
        let _ = switch_into(&mut st, side, slot);
        n += (st.weather != w) as usize + (st.terrain != t) as usize;
    }
    n
}

/// Apply a (forced) switch-in directly to `state`: reset the outgoing active's boosts
/// and volatiles, change the active slot, and apply entry hazards. Used by the
/// differential harness to apply post-faint replacement switches.
/// Switch `target` in as `side`'s active (faint replacement / landing). Returns the reversible
/// instruction list applied — entry-hazard damage, switch-in ability/item effects, move-tracking
/// resets — so a display layer (e.g. `protocol.rs`) can render the switch-in events. Callers that
/// only want the state mutation may ignore the return value.
pub fn switch_into(state: &mut State, side: SideId, target: u8) -> Vec<Instruction> {
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new(), move_failed: false, pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
    apply_switch(&mut b, side, target);
    clear_stats_raised_markers(&mut b.state);
    *state = b.state;
    b.ins
}

/// Does PS reach `nextTurn()` from this board — i.e. is the turn actually going to END here?
///
/// `go()` (`sim/battle.ts`) drains its action queue and only then calls `nextTurn()`, and it
/// returns early on `this.ended` OR `this.requestState`. Both early exits show up in state as the
/// same thing: an active slot with a fainted mon in it. If a side is wiped the battle ended; if it
/// still has a bench, `checkFainted` raised a `switch` request and the turn is suspended until the
/// replacement lands. Either way the per-turn bookkeeping `nextTurn` does — the
/// `statsRaisedThisTurn` / `statsLoweredThisTurn` reset — has NOT happened yet.
fn next_turn_reached(state: &State) -> bool {
    [SideId::One, SideId::Two].iter().all(|&s| state.side(s).active().is_alive())
}

/// `statsRaisedThisTurn` / `statsLoweredThisTurn` are cleared in exactly two PS places: on the
/// OUTGOING mon inside `switchIn` (`sim/battle-actions.ts:123-124`, already covered by the
/// switch-out volatile reset) and on every ACTIVE mon inside `nextTurn`
/// (`sim/battle.ts:1675-1676`). A faint replacement is followed by the rest of the turn and then
/// that `nextTurn`, so the replacement entry APIs stand in for it here.
///
/// **Unless the turn never gets there.** `go()` runs its action queue and only then calls
/// `nextTurn()`, and it returns early on BOTH `this.ended` and `this.requestState` — so the
/// markers freeze into the recorded state whenever the replacement leaves an active slot
/// EMPTY. Two shapes, one predicate:
///
/// - the replacement kills itself on entry and its side still has a live bench, so PS issues
///   ANOTHER switch request and returns from `go()` before `nextTurn` (rb1529: Suicune and
///   then Scovillain each enter into Spikes + Stealth Rock and die on the way in, while
///   Reuniclus keeps the `statsLoweredThisTurn` that the foe's Intimidate gave it);
/// - the replacement kills itself on entry and the side is now EMPTY, so the battle ends
///   (rb1433: Sticky Web sits on both sides, both replacements take −1 Spe, and Stealth Rock
///   finishes the 20-HP Houndstone).
///
/// A fainted active covers both — `battle_over` implies one. `apply_end_of_turn`'s own clear
/// carried only the `this.ended` half; both now go through `next_turn_reached`.
fn clear_stats_raised_markers(state: &mut State) {
    if !next_turn_reached(state) {
        return;
    }
    for side in [SideId::One, SideId::Two] {
        state.side_mut(side).volatiles.remove(VolatileStatus::StatsRaisedThisTurn);
        state.side_mut(side).volatiles.remove(VolatileStatus::StatsLoweredThisTurn);
    }
}

/// Push an instruction onto a branch and apply it to that branch's state.
fn push(b: &mut Branch, ins: Instruction) {
    b.state.apply_one(ins);
    b.ins.push(ins);
}

/// Mark hidden-information bits as now seen by the foe (the active mon of `side`). Pushes a
/// reversible `Reveal` carrying only the *newly*-set bits, so it's a no-op when nothing is new.
/// Off the critical path of damage/stat math — purely bookkeeping for `State::observe`.
fn reveal(b: &mut Branch, side: SideId, moves: u8, flags: u8) {
    let slot = b.state.side(side).active_index;
    let cur = b.state.side(side).active().reveal;
    let new_moves = moves & !cur.moves;
    let new_flags = flags & !cur.flags;
    if new_moves != 0 || new_flags != 0 {
        push(b, Instruction::Reveal { side, slot, moves: new_moves, flags: new_flags });
    }
}

// --- switching ---------------------------------------------------------------

/// Does `side` have a non-active party member that is still alive (a legal switch target)?
pub(crate) fn has_alive_bench(state: &State, side: SideId) -> bool {
    let s = state.side(side);
    (0..6u8).any(|i| {
        i != s.active_index
            && s.pokemon[i as usize].species != crate::ids::Species::None
            && s.pokemon[i as usize].is_alive()
    })
}

/// Does `side` have a non-active party member that has fainted (a Revival Blessing target)?
pub(crate) fn has_fainted_bench(state: &State, side: SideId) -> bool {
    let s = state.side(side);
    (0..6u8).any(|i| {
        i != s.active_index
            && s.pokemon[i as usize].species != crate::ids::Species::None
            && !s.pokemon[i as usize].is_alive()
    })
}

/// Revival Blessing: restore a fainted party member (`slot`) to floor(maxHP/2) HP with healthy
/// status, keeping it benched. PP is untouched. Reversible (Heal + status clears).
pub(crate) fn apply_revive(b: &mut Branch, side: SideId, slot: u8) {
    let (species, alive, max_hp, status, counter, was_tera, cur_types, base_types) = {
        let p = &b.state.side(side).pokemon[slot as usize];
        (p.species, p.is_alive(), p.max_hp, p.status, p.status_counter, p.terastallized, p.types, p.base_types)
    };
    // Only a genuinely fainted party member is a legal target.
    if species == crate::ids::Species::None || alive || slot == b.state.side(side).active_index {
        return;
    }
    // PS `delete pokemon.terastallized` runs in faintMessages (battle.ts:2581), so a mon that
    // fainted while Terastallized comes back to its BASE typing — a revived mon is never tera'd.
    // The forward seed gate carries the pre-faint tera on the (hp=0) mon (a fainted mon's types
    // aren't otherwise compared); revert it here so the revived mon matches PS's base form.
    if was_tera {
        if cur_types != base_types {
            push(b, Instruction::ChangeTypes { side, slot, previous: cur_types, new: base_types });
        }
        push(b, Instruction::ToggleTerastallized { side, slot });
    }
    // Same reasoning for PS's `types` ARRAY: `faintMessages` runs `clearVolatile`, whose
    // `setSpecies(this.baseSpecies)` calls `setType(species.types, /*enforce*/ true)` — so a mon
    // that fainted after a Protean / Double Shock / forme change is back on the species typing.
    // The engine leaves the stale array on the (hp = 0) mon, where nothing compares it; undo it
    // here, where the revived mon starts being compared again.
    {
        let live = b.state.side(side).pokemon[slot as usize].live_types;
        if live != base_types {
            push(b, Instruction::ChangeLiveTypes { side, slot, previous: live, new: base_types });
        }
    }
    let heal = (max_hp / 2).max(1);
    push(b, Instruction::Heal { side, slot, amount: heal });
    // PS sets `status = ''` on the revived mon (a mon that fainted while statused keeps that
    // status in the serialized pre-state).
    if status != Status::None {
        push(b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
    }
    if counter != 0 {
        push(b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: 0 });
    }
    // Illusion's `onFaint` nulled the disguise when this mon went down (`sim/battle.ts:2578`).
    // The engine leaves it on the corpse — nothing compares a fainted mon — so the ONE path that
    // brings a corpse back into comparison has to clear it here.
    if let Some(previous) = b.state.side(side).pokemon[slot as usize].illusion {
        push(b, Instruction::SetIllusion { side, slot, previous: Some(previous), new: None });
    }
}

pub(crate) fn apply_switch(b: &mut Branch, side: SideId, target: u8) {
    apply_switch_inner(b, side, target, true, false);
}

/// Shed Tail's switch: identical to `apply_switch` but the outgoing mon's Substitute (volatile
/// + HP) is preserved and passed to the incoming mon (PS `copyVolatileFrom(_, 'shedtail')`,
/// which copies only the substitute — boosts and every other volatile are still cleared).
pub(crate) fn apply_switch_pass_sub(b: &mut Branch, side: SideId, target: u8) {
    apply_switch_inner(b, side, target, true, true);
}

/// `fire_ability: false` defers the incoming mon's switch-in ability (simultaneous entries
/// run all abilities after every replacement is on the field, in speed order — PS event
/// semantics; Intimidate must see the other fresh switch-in). `pass_sub` keeps the outgoing
/// mon's Substitute alive across the switch (Shed Tail).
fn apply_switch_inner(b: &mut Branch, side: SideId, target: u8, fire_ability: bool, pass_sub: bool) {
    let s = b.state.side(side);
    let previous = s.active_index;
    let replacing_fainted = !s.active().is_alive();
    if previous == target {
        return;
    }
    // **The switch-out abilities read the LIVE ability, before any of this function's reverts.**
    // PS fires them from `runEvent('BeforeSwitchOut')` (`sim/battle.ts:2919`), which happens before
    // `switchIn` calls `clearVolatile()` — so a Ditto that Impostered into a Regenerator mon still
    // has Regenerator when it leaves. The engine read the ability ~120 lines down, AFTER
    // `revert_transform` had already put Imposter back. rb1502 d17: a Ditto-as-Klawf switches out
    // at 19 HP; PS heals it to 94 (baseMaxhp 225 / 3), the engine healed nothing.
    let switch_out_ability = s.active().ability;
    // The outgoing mon is the current opposing active from the foe's perspective, so its exit ends
    // any foe-sourced trap (partial trap / Mean Look / Octolock / Jaw Lock) it was holding the foe
    // in — PS clears the linked `trapped`/`partiallytrapped` when the trapper's `clearVolatile`
    // runs. The leaving mon's own trapping volatiles are cleared below via `ALL_VOLATILES`.
    clear_foe_sourced_traps(b, side.other());
    // `moveLastTurnResult` — the flag Stomping Tantrum's doubler reads — is a PER-MON field that
    // `clearVolatile` wipes on switch-out (sim/pokemon.ts:1546-1547), and the incoming mon's own
    // copy was wiped the same way when IT last left. The engine keeps one flag per SIDE and only
    // ever writes it from a move action, so a failed move left it set across the switch.
    // rb1243 d11: a Walking Wake MISSES Hydro Pump on turn 8, Amoonguss comes in on turn 9 and uses
    // Stomping Tantrum on turn 10 — PS deals 64, the engine doubled the base power and dealt 126.
    if b.state.side(side).last_move_failed {
        push(b, Instruction::SetLastMoveFailed { side, previous: true, new: false });
    }
    // A traced / copied ability reverts on switch-out (Transform handles its own below).
    {
        let p = b.state.side(side).active();
        // Tera Shift's forme change (Terapagos-Normal -> Terapagos-Terastal, ability -> Tera Shell)
        // is PERMANENT (PS `formeChange`, not a copied ability): the mon stays Terastal with Tera
        // Shell when it switches out, so this revert must skip it (base_ability is still the stale
        // Tera Shift). Every OTHER non-base ability on a non-transformed mon is a Trace/Role
        // Play/Skill Swap copy that PS's clearVolatile reverts.
        // Same for the Terastallization formes: `formeChange(..., isPermanent)` writes
        // `baseAbility = ability`, so Embody Aspect / Teraform Zero survive the switch-out that
        // `clearVolatile`'s `ability = baseAbility` would otherwise revert.
        let terastal_forme = crate::ids::Species::from_id("terapagosterastal");
        let is_forme_ability = (Some(p.species) == terastal_forme && p.ability == crate::ids::Ability::TeraShell)
            || matches!(
                p.ability,
                crate::ids::Ability::EmbodyAspectTeal
                    | crate::ids::Ability::EmbodyAspectWellspring
                    | crate::ids::Ability::EmbodyAspectHearthflame
                    | crate::ids::Ability::EmbodyAspectCornerstone
                    | crate::ids::Ability::TeraformZero
            );
        if !p.transformed && p.ability != p.base_ability && !is_forme_ability {
            let slot = previous;
            push(b, Instruction::ChangeAbility { side, slot, previous: p.ability, new: p.base_ability });
        }
    }
    // Attract ends when its source leaves the field (PS condition onUpdate: source not
    // active). In singles the source of the OPPONENT's infatuation is necessarily this
    // outgoing active, so the foe's Attract volatile is cleared here.
    if b.state.side(side.other()).volatiles.contains(VolatileStatus::Attract) {
        push(b, Instruction::RemoveVolatile { side: side.other(), volatile: VolatileStatus::Attract });
    }
    // A transformed mon reverts to its own identity as it leaves the field; battle-only
    // formes (Pirouette / Morpeko-Hangry) regress to their base forme.
    revert_transform(b, side);
    revert_battle_only_forme(b, side);
    // Zero to Hero (Palafin): on switch-out, the base forme transforms into Palafin-Hero
    // (higher offensive stats; HP base is unchanged so max HP carries). One-way — once Hero it
    // stays Hero. PS's `setSpecies` re-runs `spreadModify(species.baseStats, this.set)`, so the
    // mon's OWN EV/IV/nature spread carries — recovered by `respread_stats`.
    {
        let p = b.state.side(side).active();
        let palafin = crate::ids::Species::from_id("palafin");
        // PS's `onSwitchOut` forme change does NOT run for a fainted mon — a Palafin that faints
        // stays in its base forme.
        if !replacing_fainted && p.ability == crate::ids::Ability::ZeroToHero && Some(p.species) == palafin {
            if let Some(hero) = crate::ids::Species::from_id("palafinhero") {
                let stats = respread_stats(
                    crate::data::base_stats(p.species),
                    crate::data::base_stats(hero),
                    p.stats,
                    p.level,
                );
                let prev_data = transform_data_of(&b.state, side);
                let mut new = prev_data;
                new.species = hero;
                new.stats = stats;
                let slot = previous;
                let previous_base_moves = b.state.side(side).active().base_moves;
                push(b, Instruction::Transform { side, slot, previous: prev_data, new, previous_base_moves });
            }
        }
    }
    // Type changes (Protean/Libero, Conversion, Reflect Type, …) revert as the mon leaves the
    // field: PS's `clearVolatile` ends with `setSpecies(this.baseSpecies)`, whose
    // `setType(species.types, /*enforce*/ true)` bypasses the terastallized guard — so the
    // `types` ARRAY always goes back to the species typing. The EFFECTIVE typing follows it
    // only when the mon is not terastallized (Tera survives a switch).
    {
        let (tera, cur, live, base) = {
            let p = b.state.side(side).active();
            (p.terastallized, p.types, p.live_types, p.base_types)
        };
        let slot = previous;
        if !tera && cur != base {
            push(b, Instruction::ChangeTypes { side, slot, previous: cur, new: base });
        }
        if live != base {
            push(b, Instruction::ChangeLiveTypes { side, slot, previous: live, new: base });
        }
    }
    // Reset the outgoing active's boosts and volatiles (emit explicit deltas so the
    // instruction list stays exactly reversible).
    for stat in [
        BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
        BoostIndex::SpecialDefense, BoostIndex::Speed, BoostIndex::Accuracy, BoostIndex::Evasion,
    ] {
        let cur = b.state.side(side).boost(stat);
        if cur != 0 {
            push(b, Instruction::Boost { side, stat, amount: -cur });
        }
    }
    let vols = b.state.side(side).volatiles;
    for v in ALL_VOLATILES {
        if pass_sub && *v == VolatileStatus::Substitute {
            continue; // Shed Tail passes the Substitute to the incoming mon.
        }
        if vols.contains(*v) {
            push(b, Instruction::RemoveVolatile { side, volatile: *v });
        }
    }
    let sub = b.state.side(side).substitute_hp;
    if sub != 0 && !pass_sub {
        push(b, Instruction::ChangeSubstituteHp { side, amount: -sub });
    }
    // Natural Cure heals the outgoing Pokémon's non-volatile status as it switches out;
    // Regenerator restores 1/3 of its max HP. Both act on the mon before it leaves.
    let (out_ability, out_status, out_hp, out_max) = {
        let o = b.state.side(side).active();
        (switch_out_ability, o.status, o.hp, o.max_hp)
    };
    if out_ability == crate::ids::Ability::NaturalCure && out_status != Status::None {
        push(b, Instruction::ChangeStatus { side, slot: previous, previous: out_status, new: Status::None });
        clear_status_counter(b, side, previous);
    }
    if out_ability == crate::ids::Ability::Regenerator && out_hp > 0 && out_hp < out_max {
        let heal = (out_max / 3).min(out_max - out_hp);
        if heal > 0 {
            push(b, Instruction::Heal { side, slot: previous, amount: heal });
        }
    }
    // Consecutive-use tracking belongs to the active slot — reset it as the mon leaves.
    reset_move_tracking(b, side);
    // Illusion's `onBeforeSwitchIn` fires AFTER `switchIn` has swapped the array entries but
    // BEFORE the `|switch|` line is added (`sim/battle-actions.ts:128-145`), so the disguise has to
    // be installed ahead of the Switch instruction the protocol layer renders. The choice reads the
    // POST-swap array, computed here because `Switch` is what performs the swap.
    apply_illusion_choice(b, side, previous, target);
    push(b, Instruction::Switch { side, previous, next: target });

    apply_entry_hazards(b, side);
    // Toxic's damage stage resets whenever the badly-poisoned mon re-enters.
    {
        let p = b.state.side(side).active();
        if p.status == Status::Toxic && p.status_counter != 0 {
            let (slot, prev) = (b.state.side(side).active_index, p.status_counter);
            push(b, Instruction::ChangeStatusCounter { side, slot, previous: prev, new: 0 });
        }
    }
    // Healing Wish / Lunar Dance: heal the incoming mon if it needs it (wish persists
    // until a damaged-or-statused mon enters).
    if b.state.side(side).healing_wish {
        let (hp, max_hp, status, slot) = {
            let p = b.state.side(side).active();
            (p.hp, p.max_hp, p.status, b.state.side(side).active_index)
        };
        if hp > 0 && (hp < max_hp || status != Status::None) {
            if hp < max_hp {
                push(b, Instruction::Heal { side, slot, amount: max_hp - hp });
            }
            if status != Status::None {
                let counter = b.state.side(side).active().status_counter;
                push(b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
                if counter != 0 {
                    push(b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: 0 });
                }
            }
            push(b, Instruction::SetHealingWish { side, previous: true, new: false });
        }
    }
    if fire_ability && b.state.side(side).active().is_alive() {
        apply_switch_in_ability(b, side);
        run_update_event(b);
    }
}

/// The crit + damage rolls PS makes and then throws away, for the `onDamage`-returns-0 abilities
/// (Ice Face, Disguise): `getDamage` still rolls `randomChance(1, critMult)` and `random(16)`
/// before `onDamage` zeroes the result, so the stream has to advance by exactly those two draws
/// plus the ModifyDamage screen-tie shuffle.
///
/// The RESULTS are recorded from the realized source when there is one. They change nothing about
/// the branch, but the seed gate's branch selector filters on recorded-result equality with the
/// live PRNG, so a hardcoded value desyncs the unit.
fn emit_discarded_damage_rolls(hb: &mut Branch, crit_den: i32) {
    let mut cur = realized_cursor(hb);
    if crit_den > 0 {
        let v = cur.as_mut().map_or(0, |c| c.peek("randomChance", &[1, crit_den]));
        draw(hb, "randomChance", &[1, crit_den], v, "crit");
    }
    let v = cur.as_mut().map_or(0, |c| c.peek("random", &[16]));
    draw(hb, "random", &[16], v, "damage-roll");
    emit_modifydamage_shuffle(hb);
}

/// Illusion `onBeforeSwitchIn` (`data/abilities.ts:2011-2023`): the entering mon's disguise is
/// nulled and then re-chosen as the LAST able entry of PS's live `side.pokemon` array behind it.
///
/// Runs for the entering mon only, and only if it actually HAS the ability — the handler is the
/// ability's, so a mon that lost Illusion (or never had it) neither picks nor clears. Consumes no
/// PRNG: the scan is a plain loop and `singleEvent` never speed-sorts.
fn apply_illusion_choice(b: &mut Branch, side: SideId, previous: u8, target: u8) {
    if b.state.side(side).pokemon[target as usize].ability != crate::ids::Ability::Illusion {
        return;
    }
    // The array as it will look once `Switch` performs the swap.
    let mut roster_after = b.state.side(side).roster;
    if let (Some(i), Some(j)) = (
        roster_after.iter().position(|&x| x == previous),
        roster_after.iter().position(|&x| x == target),
    ) {
        roster_after.swap(i, j);
    }
    let s = b.state.side(side);
    let new = s.illusion_target(roster_after, target);
    let prev = s.pokemon[target as usize].illusion;
    if prev != new {
        push(b, Instruction::SetIllusion { side, slot: target, previous: prev, new });
    }
}

/// The PAYLOAD of PS's `this.eachEvent('Update')` — the event `runAction` fires at the end of
/// EVERY action, move or switch alike (`sim/battle.ts:2882`). `emit_update` and friends only
/// account for the SHUFFLE that event's speedSort consumes; this runs what the event actually
/// does. The handlers that matter are the `onUpdate` items: the Sitrus Berry's
/// `onUpdate(pokemon) { if (pokemon.hp <= pokemon.maxhp / 2) pokemon.eatItem(); }` and Lum /
/// Chesto's cure (`data/items.ts`).
///
/// Two witnesses, one on each kind of action:
/// - **switch** — rb1003 d34: Dedenne (Cheek Pouch, Sitrus, 70/261) replaces a faint on 2 layers
///   of Spikes, takes 43 and lands at 27. PS eats the berry on the spot — 65 from the berry plus
///   Cheek Pouch's extra 1/3 max — and ends the unit at 179/261; the engine left it at 27 holding
///   an uneaten berry.
/// - **move** — rb1227 d39: Arboliva (Harvest, Sitrus) takes a 100 Scald and then Substitutes for
///   another 288/4 = 72, landing at 116/288, under half. PS eats the Sitrus at the move action's
///   2882 Update (+72), Leech Seed heals it 36 to 224, and the order-28 Harvest residual regrows
///   the berry. The engine only ran the berry check at the per-HIT Update inside `spreadMoveHit`
///   (`battle-actions.ts:970`), which no status move ever reaches and which sits AHEAD of Life
///   Orb / recoil self-damage even on a damaging one — so it stayed at 152 with the berry unlit.
///
/// `eachEvent` speed-sorts the actives, so run the faster one's handlers first. Idempotent: a
/// berry already eaten is no longer in hand, so firing this after the 970 Update changes nothing.
fn run_update_event(b: &mut Branch) {
    let mut order = [SideId::One, SideId::Two];
    if effective_speed(&b.state, order[1]) > effective_speed(&b.state, order[0]) {
        order.swap(0, 1);
    }
    for side in order {
        apply_pinch_berry(b, side);
        consume_lum_if_statused(b, side);
        retry_trace(b, side);
    }
}

/// Trace is an `onUpdate` handler, not a one-shot switch-in effect.
///
/// `data/abilities.ts:5075-5103`: `onStart` only sets `effectState.seek` and then hand-fires the
/// SAME `onUpdate` once; `seek` is never cleared afterwards (only No Ability / Ability Shield
/// suppress it up front), and the copy itself is what ends the retry — `setAbility` replaces
/// Trace, so the holder stops carrying the handler. A Trace holder that switched in against an
/// untraceable foe therefore keeps trying at EVERY `eachEvent('Update')` until a traceable foe
/// appears.
///
/// rb1244 d10 t7: a Gardevoir switches in against a Protosynthesis Sandy Shocks (`notrace`), so
/// its copy fails. p1's Volt Switch then pivots a Water Absorb mon in, and PS's very next Update
/// fires `sample[1]@trace` and Gardevoir ends the unit on Water Absorb. The engine had modelled
/// Trace only at the holder's own switch-in, so it kept Trace and swallowed no draw.
fn retry_trace(b: &mut Branch, side: SideId) {
    use crate::ids::Ability::Trace;
    if !b.state.side(side).active().is_alive() || b.state.side(side).active().ability != Trace {
        return;
    }
    let foe = side.other();
    let fa = b.state.side(foe).active().ability;
    if !b.state.side(foe).active().is_alive() || !ability_is_traceable(fa) {
        return;
    }
    // `this.sample(possibleTargets)` runs even on a one-element list in singles.
    draw(b, "sample", &[1], 0, "trace");
    let slot = b.state.side(side).active_index;
    push(b, Instruction::ChangeAbility { side, slot, previous: Trace, new: fa });
    // The copied ability activates as if the holder just switched in (PS `setAbility` fires the
    // new ability's `onStart`).
    apply_switch_in_ability(b, side);
}

/// Switch both sides simultaneously: entries (and hazards) in speed order of the OUTGOING
/// actives, then switch-in abilities in speed order of the INCOMING actives.
/// Double faint-replacement: both mons enter, hazards, then switch-in abilities in speed order.
/// Returns the applied reversible instruction list (see [`switch_into`]).
pub fn switch_into_pair(state: &mut State, pairs: [(SideId, u8); 2]) -> Vec<Instruction> {
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new(), move_failed: false, pivot_update_done: false, per_hit_procs_done: false, pending_damaging_hit: None, drag_tie_speeds: None, after_hit_user_alive: true, late_self_damage: 0, move_any_damage: false };
    let mut order = pairs;
    if effective_speed(&b.state, order[1].0) > effective_speed(&b.state, order[0].0) {
        order.swap(0, 1);
    }
    for &(side, target) in &order {
        apply_switch_inner(&mut b, side, target, false, false);
    }
    let mut ab_order = [order[0].0, order[1].0];
    if effective_speed(&b.state, ab_order[1]) > effective_speed(&b.state, ab_order[0]) {
        ab_order.swap(0, 1);
    }
    for side in ab_order {
        if b.state.side(side).active().is_alive() {
            apply_switch_in_ability(&mut b, side);
        }
    }
    run_update_event(&mut b);
    clear_stats_raised_markers(&mut b.state);
    *state = b.state;
    b.ins
}

/// Zero all of a side's active-only state that resets on switch: consecutive-use tracking
/// plus the multi-turn move / restriction / countdown fields. Emitted as explicit reversible
/// deltas so the instruction list stays exactly invertible.
fn reset_move_tracking(b: &mut Branch, side: SideId) {
    use crate::ids::MoveId;
    use crate::instruction::ActiveCounter::{ActiveTurns, Confusion, HealBlock, Perish, Taunt, ThroatChop, Yawn};
    let s = b.state.side(side);
    let (lm, streak, stall) = (s.last_used_move, s.move_streak, s.stall_counter);
    let (pending, encore, disable) = (s.pending_move, s.encore, s.disable);
    let (taunt, conf, perish, yawn, active) =
        (s.taunt_turns, s.confusion_turns, s.perish_turns, s.yawn_turns, s.active_turns);
    let (tc, hb) = (s.throat_chop_turns, s.heal_block_turns);
    if tc != 0 {
        push(b, Instruction::SetActiveCounter { side, which: ThroatChop, previous: tc, new: 0 });
    }
    if hb != 0 {
        push(b, Instruction::SetActiveCounter { side, which: HealBlock, previous: hb, new: 0 });
    }
    if lm != MoveId::None {
        push(b, Instruction::SetLastMove { side, previous: lm, new: MoveId::None });
    }
    if streak != 0 {
        push(b, Instruction::SetMoveStreak { side, previous: streak, new: 0 });
    }
    if stall != 0 {
        push(b, Instruction::SetStallCounter { side, previous: stall, new: 0 });
    }
    let stall_t = b.state.side(side).stall_turns;
    if stall_t != 0 {
        push(b, Instruction::SetStallTurns { side, previous: stall_t, new: 0 });
    }
    if pending != crate::state::PendingMove::None {
        push(b, Instruction::SetPendingMove { side, previous: pending, new: crate::state::PendingMove::None });
    }
    if encore.1 != 0 {
        push(b, Instruction::SetEncore { side, previous: encore, new: (MoveId::None, 0) });
    }
    if disable.1 != 0 {
        push(b, Instruction::SetDisable { side, previous: disable, new: (MoveId::None, 0) });
    }
    let pt = (b.state.side(side).partial_trap_turns, b.state.side(side).partial_trap_div);
    if pt != (0, 0) {
        push(b, Instruction::SetPartialTrap { side, previous: pt, new: (0, 0) });
    }
    for (which, cur) in [(Taunt, taunt), (Confusion, conf), (Perish, perish), (Yawn, yawn), (ActiveTurns, active)] {
        if cur != 0 {
            push(b, Instruction::SetActiveCounter { side, which, previous: cur, new: 0 });
        }
    }
}

/// Record that `side`'s active executed `move_id` this turn: advance `move_streak` if it's
/// the same move as last turn (else restart at 1), and reset the Protect `stall_counter`
/// unless this is itself a Protect-family move. Called once the mon actually acts.
fn record_move_use(b: &mut Branch, side: SideId, move_id: crate::ids::MoveId) {
    let s = b.state.side(side);
    let (prev_move, prev_streak, prev_stall) = (s.last_used_move, s.move_streak, s.stall_counter);
    let new_streak = if move_id == prev_move { prev_streak.saturating_add(1).min(250) } else { 1 };
    if move_id != prev_move {
        push(b, Instruction::SetLastMove { side, previous: prev_move, new: move_id });
    }
    if new_streak != prev_streak {
        push(b, Instruction::SetMoveStreak { side, previous: prev_streak, new: new_streak });
    }
    // NOTE: a non-Protect action does NOT clear the chain here. PS's `stall` volatile carries the
    // `onStallMove` denominator in `effectState.counter` and is only deleted by a FAILED stall
    // roll or by its own `duration: 2` running out — using something else never touches it, so
    // the counter survives to the end of the turn and is cleared with the volatile in the
    // residual pass (`apply_end_of_turn_inner`, gated on the this-turn Protect marker), which is
    // exactly PS's lifetime. rb1239 d64 t51 catches the difference at a mid-turn boundary: the
    // Toxapex used Baneful Bunker on t50 and Toxic on t51, and PS's snapshot after the Toxic
    // still holds `stall` with `counter: 3`.
    let _ = prev_stall;
    // Hidden-info: using a move reveals that slot to the foe. (Struggle has no slot; skip it.)
    let slot_bit = b.state.side(side).active().moves.iter()
        .position(|m| m.id == move_id)
        .map(|i| 1u8 << i)
        .unwrap_or(0);
    if slot_bit != 0 {
        reveal(b, side, slot_bit, 0);
    }
    // Using a move while holding a Choice item locks the user into it (PS `choicelock`
    // volatile, cleared on switch-out).
    let item = b.state.side(side).active().item;
    if matches!(item, Item::ChoiceBand | Item::ChoiceScarf | Item::ChoiceSpecs)
        && !b.state.side(side).volatiles.contains(VolatileStatus::ChoiceLock)
    {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ChoiceLock });
    }
}

/// Does Pressure tax this move's PP?
///
/// PS resolves the answer in `Pokemon#getMoveTargets` (sim/pokemon.ts:853-860): the pressure
/// targets are the move's targets, EXCEPT that a `foeSide` move gets NONE
/// (`if (move.target === 'foeSide') pressureTargets = []`) and a `mustpressure` move always gets
/// the foes. So the hazards SPLIT: Spikes / Stealth Rock / Toxic Spikes carry `mustpressure` and
/// are taxed, while Sticky Web — the one `foeSide` hazard without the flag — is NOT.
/// rb1377 d4 / rb1326 d2: PS leaves a Sticky Web at 31 PP, the engine took it to 30.
///
/// `singleEvent('ModifyMove')` runs at battle-actions.ts:429, BEFORE the `getMoveTargets` at
/// :467, so Curse has already swapped in its `nonGhostTarget` (`self`) for a non-Ghost user and
/// is likewise untaxed. rb1152 d48.
fn pressure_affected(md: &crate::data::MoveData, user_is_ghost: bool) -> bool {
    if md.flag_mustpressure {
        return true;
    }
    let target = if user_is_ghost { md.target } else { md.non_ghost_target };
    target != crate::data::MoveTarget::FoeSide && target.targets_foe()
}

/// On-switch-in ability effects (weather setters and Intimidate).
fn apply_switch_in_ability(b: &mut Branch, side: SideId) {
    use crate::ids::Ability::*;
    let ability = b.state.side(side).active().ability;
    // Ice Face's `onStart` is the same restore as its `onWeatherChange` (`data/abilities.ts:1926`):
    // a Noice Eiscue entering hail / snowscape is Eiscue again immediately. State-only.
    restore_ice_face(b, side);
    let weather = match ability {
        Drought | OrichalcumPulse => Weather::Sun,
        Drizzle => Weather::Rain,
        SandStream => Weather::Sand,
        SnowWarning => Weather::Snow,
        _ => Weather::None,
    };
    if weather != Weather::None && b.state.weather != weather {
        let turns = weather_set_turns(b.state.side(side).active().item, weather);
        set_weather(b, weather, turns);
    }
    // Terrain surges set their terrain for 5 turns (8 with Terrain Extender).
    let terrain = match ability {
        ElectricSurge | HadronEngine => crate::ids::Terrain::Electric,
        GrassySurge => crate::ids::Terrain::Grassy,
        PsychicSurge => crate::ids::Terrain::Psychic,
        MistySurge => crate::ids::Terrain::Misty,
        _ => crate::ids::Terrain::None,
    };
    if terrain != crate::ids::Terrain::None && b.state.terrain != terrain {
        let turns = if b.state.side(side).active().item == Item::TerrainExtender { 8 } else { 5 };
        emit_field_change_shuffle(b); // setTerrain -> eachEvent('TerrainChange') (r10 d32 @grassysurge)
        push(b, Instruction::ChangeTerrain {
            previous: b.state.terrain,
            previous_turns: b.state.terrain_turns,
            new: terrain,
            new_turns: turns,
        });
        refresh_proto_quark(b); // PS Quark Drive `onTerrainChange`
    }
    // Tera Shift: Terapagos becomes its Terastal forme on entry (new base stats incl. max
    // HP — damage taken carries over — and the Tera Shell ability). Random-battle spread
    // (31 IV / 85 EV / neutral) is assumed for the stat recompute.
    if ability == TeraShift
        && b.state.side(side).active().species == crate::ids::Species::from_id("terapagos").unwrap_or(crate::ids::Species::None)
    {
        if let Some(terastal) = crate::ids::Species::from_id("terapagosterastal") {
            let p = b.state.side(side).active();
            let level = p.level;
            let base = crate::data::base_stats(terastal);
            let mut stats = [0i16; 6];
            stats[0] = crate::damage::compute_hp(base[0], 31, 85, level);
            for (si, stat) in [
                crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
                crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
                crate::ids::StatIndex::Speed,
            ].into_iter().enumerate() {
                stats[si + 1] = crate::damage::compute_stat(base[si + 1], 31, 85, level, crate::ids::Nature::Serious, stat);
            }
            let previous = transform_data_of(&b.state, side);
            let mut new = previous;
            new.species = terastal;
            new.stats = stats;
            new.ability = crate::ids::Ability::TeraShell;
            let slot = b.state.side(side).active_index;
            let previous_base_moves = b.state.side(side).active().base_moves;
            push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
        }
    }
    // Shields Down (Minior): PS `onStart` at `onSwitchInPriority: -1` picks the forme from the
    // entering mon's HP — Meteor above half, the packed core at or below.
    shields_down_forme(b, side);
    // Dauntless Shield / Intrepid Sword: +1 Def / +1 Atk, once per battle.
    if matches!(ability, DauntlessShield | IntrepidSword) && !b.state.side(side).active().ability_used {
        let stat = if ability == DauntlessShield { BoostIndex::Defense } else { BoostIndex::Attack };
        raise_boost(b, side, stat, 1);
        let slot = b.state.side(side).active_index;
        push(b, Instruction::SetAbilityUsed { side, slot, previous: false, new: true });
    }
    // Imposter (Ditto): transform into the foe's active immediately on switch-in.
    if ability == Imposter {
        apply_transform(b, side);
    }
    // Embody Aspect re-fires on EVERY switch-in, not just at Terastallization: its once-only
    // guard is `this.effectState.embodied`, and `battle-actions.ts:142` re-inits
    // `pokemon.abilityState` on each `switchIn`. The other guards are `baseSpecies.name` being
    // the `-Tera` forme and `pokemon.terastallized`. rb1142 t20: a Wellspring Ogerpon that
    // pivots out and back takes a second +1 SpD in PS.
    if let Some(stat) = embody_aspect_stat(ability) {
        let p = b.state.side(side).active();
        if p.terastallized && species_is_ogerpon_tera(p.species) {
            raise_boost(b, side, stat, 1);
        }
    }
    // Wind Rider: +1 Atk on switch-in while the holder's own Tailwind is active (PS onStart).
    if ability == WindRider && b.state.side(side).side_conditions.tailwind > 0 {
        raise_boost(b, side, BoostIndex::Attack, 1);
    }
    // Frisk reveals the opponent's held item on switch-in (information only).
    if ability == Frisk {
        let foe = side.other();
        if b.state.side(foe).active().is_alive() {
            reveal(b, foe, 0, crate::state::Reveal::ITEM);
        }
    }
    // Trace copies the opponent's ability on switch-in (a few abilities are untraceable).
    if ability == Trace {
        let foe = side.other();
        let fa = b.state.side(foe).active().ability;
        let untraceable = !ability_is_traceable(fa);
        if b.state.side(foe).active().is_alive() && !untraceable {
            // PS Trace `onUpdate` picks its target via `this.sample(possibleTargets)` — the
            // traceable adjacent foes (battle abilities.ts). In singles that list is length 1, so
            // one `sample[1]` draw fires before the copy (draw-and-discard; state validates).
            draw(b, "sample", &[1], 0, "trace");
            let slot = b.state.side(side).active_index;
            push(b, Instruction::ChangeAbility { side, slot, previous: Trace, new: fa });
            // The copied ability activates as if the holder just switched in.
            apply_switch_in_ability(b, side);
        }
    }
    // Intimidate: lower the opposing active's Attack by 1 on switch-in. Inner Focus /
    // Oblivious / Own Tempo / Scrappy and a Substitute block it in gen 8+.
    if ability == Intimidate {
        let foe = side.other();
        let blocked = matches!(
            b.state.side(foe).active().ability,
            InnerFocus | Oblivious | OwnTempo | Scrappy
        ) || b.state.side(foe).volatiles.contains(VolatileStatus::Substitute);
        if b.state.side(foe).active().is_alive() && !blocked {
            if apply_boost_clamped(b, foe, BoostIndex::Attack, -1) < 0 {
                react_to_stat_drop(b, foe);
            }
            // Rattled: +1 Spe when Intimidated (PS `onAfterBoost` keyed on the Intimidate
            // effect). AfterBoost receives the *attempted* boost object, so this fires even
            // when the Atk drop was clamped at −6 — any unblocked Intimidate suffices.
            if b.state.side(foe).active().ability == crate::ids::Ability::Rattled {
                raise_boost(b, foe, BoostIndex::Speed, 1);
            }
        }
    }
    // Intrepid Sword (Zacian) / Dauntless Shield (Zamazenta): +1 Atk / +1 Def — but only
    // ONCE per battle in gen9 (not on every switch-in like gen8).
    if matches!(ability, IntrepidSword | DauntlessShield) && !b.state.side(side).active().ability_used {
        let stat = if ability == IntrepidSword { BoostIndex::Attack } else { BoostIndex::Defense };
        raise_boost(b, side, stat, 1);
        let slot = b.state.side(side).active_index;
        push(b, Instruction::SetAbilityUsed { side, slot, previous: false, new: true });
    }
    // Download (`data/abilities.ts`):
    //   if (totaldef && totaldef >= totalspd) boost({spa: 1}); else if (totalspd) boost({atk: 1});
    // — the TIE goes to Special Attack, and the stats are `getStat(x, false, true)`: boosts
    // applied, ability/item modifiers not. The engine had the comparison inverted (`def <= spd
    // -> Atk`), which is only observable on the tie — and a tie is exactly what an equal-defence
    // species gives. rb1063 t2: Porygon-Z enters on a Scrafty (base 115/115) and PS takes +1 SpA.
    if ability == Download {
        let foe = side.other();
        if b.state.side(foe).active().is_alive() {
            let fs = b.state.side(foe);
            let f = fs.active();
            let def = boosted_stat(f.stat(crate::ids::StatIndex::Defense) as i64, fs.boost(BoostIndex::Defense));
            let spd = boosted_stat(f.stat(crate::ids::StatIndex::SpecialDefense) as i64, fs.boost(BoostIndex::SpecialDefense));
            let stat = if def > 0 && def >= spd {
                BoostIndex::SpecialAttack
            } else {
                BoostIndex::Attack
            };
            raise_boost(b, side, stat, 1);
        }
    }
    // Protosynthesis / Quark Drive re-derive on switch-in (PS `onSwitchIn` priority -2, plus the
    // Booster Energy item's own `onSwitchIn`): the volatile activates under Sun / Electric Terrain,
    // otherwise the held Booster Energy is consumed once to grant it. The switch already cleared any
    // stale volatile (see ALL_VOLATILES), so this is the sole re-application on entry.
    if ability == Protosynthesis {
        let slot = b.state.side(side).active_index;
        if effective_weather(&b.state) == Weather::Sun {
            if !b.state.side(side).volatiles.contains(VolatileStatus::Protosynthesis) {
                push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Protosynthesis });
            }
        } else if b.state.side(side).active().item == Item::BoosterEnergy {
            push(b, Instruction::ChangeItem { side, slot, previous: Item::BoosterEnergy, new: Item::None });
            on_item_lost(b, side);
            push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Protosynthesis });
            // PS `fromBooster`: a Booster-Energy boost survives every later field change.
            push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ProtoBooster });
        }
    }
    if ability == QuarkDrive {
        let slot = b.state.side(side).active_index;
        if b.state.terrain == crate::ids::Terrain::Electric {
            if !b.state.side(side).volatiles.contains(VolatileStatus::QuarkDrive) {
                push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::QuarkDrive });
            }
        } else if b.state.side(side).active().item == Item::BoosterEnergy {
            push(b, Instruction::ChangeItem { side, slot, previous: Item::BoosterEnergy, new: Item::None });
            on_item_lost(b, side);
            push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::QuarkDrive });
            push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ProtoBooster });
        }
    }
}

/// Apply the switching side's own hazards to the incoming Pokémon.
fn apply_entry_hazards(b: &mut Branch, side: SideId) {
    let s = b.state.side(side);
    let p = s.active();
    let slot = s.active_index;
    let maxhp = p.max_hp;
    let grounded = is_grounded(&b.state, side);

    // Heavy-Duty Boots are NOT one blanket early-return: each hazard's `onSwitchIn` carries its
    // own `pokemon.hasItem('heavydutyboots')` check, and Toxic Spikes' sits BELOW the Poison-type
    // absorb (`data/moves.ts:19780-19791`) — so a grounded Poison type in boots still SOAKS the
    // layers even though it takes nothing from them. rb1336 t6: Grafaiai (Poison/Normal,
    // Heavy-Duty Boots) switches in and PS's `toxicspikes` is gone.
    let boots = p.item == Item::HeavyDutyBoots;
    let magic_guard = p.ability == crate::ids::Ability::MagicGuard;

    // Stealth Rock — hits everything, scaled by Rock effectiveness (Magic Guard blocks it).
    if s.side_conditions.stealth_rock && !magic_guard && !boots {
        let mult = type_multiplier(Type::Rock, p.types);
        let dmg = ((maxhp as f32 / 8.0) * mult).floor() as i16;
        let dmg = dmg.max(1).min(p.hp);
        if dmg > 0 {
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }
    }
    // Spikes — grounded only (Magic Guard blocks it).
    let layers = b.state.side(side).side_conditions.spikes;
    if grounded && layers > 0 && !magic_guard && !boots {
        let frac = match layers { 1 => 8, 2 => 6, _ => 4 };
        let p = b.state.side(side).active();
        let dmg = (p.max_hp / frac).max(1).min(p.hp);
        if dmg > 0 {
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }
    }
    // Toxic Spikes — grounded; 1 layer poisons, 2 layers badly-poisons; Poison types absorb
    // them (handled as immunity here: a grounded Poison type would remove them, but we only
    // model the status). Steel/immune types and non-grounded skip.
    let tspikes = b.state.side(side).side_conditions.toxic_spikes;
    if grounded && tspikes > 0 {
        let p = b.state.side(side).active();
        if p.types.contains(&Type::Poison) {
            // A grounded Poison type absorbs the spikes, clearing them.
            push(b, Instruction::SetSideCondition {
                side,
                condition: SideConditionId::ToxicSpikes,
                previous: tspikes,
                new: 0,
            });
        } else {
            let status = if tspikes >= 2 { Status::Toxic } else { Status::Poison };
            if !boots
                && !p.types.contains(&Type::Steel)
                && status_applies(p, status)
                && !status_blocked_by_field(&b.state, side, status)
            {
                push(b, Instruction::ChangeStatus { side, slot, previous: Status::None, new: status });
                consume_lum_if_statused(b, side);
            }
        }
    }
    // Sticky Web — grounded: −1 Speed on entry.
    if grounded && !boots && b.state.side(side).side_conditions.sticky_web {
        if apply_boost_clamped(b, side, BoostIndex::Speed, -1) < 0 {
            react_to_stat_drop(b, side);
        }
    }
}

// --- moves -------------------------------------------------------------------

/// What to do when a self-switch move (U-turn/Teleport/…) connects and its user survives.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Pivot {
    /// Stay in (legacy bridge behavior; also what a pivot with an empty bench does).
    Stay,
    /// Switch to this party slot inline (verification paths — the recorded target is known).
    Target(u8),
    /// Emit `Instruction::PivotPending` and leave the user in: the request-flow driver pauses
    /// for a `PivotLanding` choice. Never used on verification/enumeration paths.
    ///
    /// Two invariants the emission sites enforce, because the caller cannot:
    /// * the EXECUTED move must be the self-switch move the pause was granted for — Struggle and
    ///   the Encore `OverrideAction` redirect replace it inside `run_move_action` (see the
    ///   re-derivation there);
    /// * the side must still have somewhere to go. PS `sim/battle.ts:2904`:
    ///   `if (switches[i] && !this.canSwitch(this.sides[i]))` clears `switchFlag` and drops the
    ///   side out of `switches` — with an empty bench the mon simply stays in and no switch
    ///   request is issued.
    Pause,
}

/// A move action: which side, which move slot, and (for a pivot move) the switch-in
/// target to use after it connects.
#[derive(Clone, Copy)]
pub(crate) struct Action {
    pub(crate) side: SideId,
    pub(crate) move_idx: u8,
    pub(crate) pivot: Pivot,
    /// Shell Side Arm's category pick, resolved by `dispatch_move_inner` (PS `onModifyMove`
    /// compares floor-chain damage estimates; an exact tie flips `randomChance(1, 2)`).
    pub(crate) shell_phys: Option<bool>,
    /// The move the foe will use *after* this action this turn (None if the foe already
    /// moved, switched, or there is no second move). Lets Sucker Punch / Thunderclap know
    /// whether the target is about to attack.
    pub(crate) foe_pending_move: Option<crate::ids::MoveId>,
    /// Custap Berry fired at queue time (+0.1 fractional priority for this action). The
    /// berry was already consumed at turn start, so the flag must ride on the action.
    pub(crate) custap: bool,
    /// PS decided at REQUEST time that this side has no usable move and must Struggle
    /// (`no_usable_move`). It rides on the action because the decision is made on the
    /// turn-START board: an Encore or a Disable that the foe lands EARLIER IN THIS TURN
    /// cannot retroactively force Struggle — PS's `onOverrideAction` just redirects the
    /// already-chosen action and never re-consults `disabled`.
    pub(crate) struggling: bool,
    /// A Dancer-invoked copy of `Some(move)` (PS `externalMove`): the move executes from
    /// this side without a move slot — no PP cost, no Encore/rampage override, no move-use
    /// bookkeeping, no rampage lock (the "Dancer Petal Dance hack"), and no re-trigger of
    /// Dancer. The full BeforeMove gauntlet (sleep/attract/confusion/paralysis) still runs.
    pub(crate) external_move: Option<crate::ids::MoveId>,
    /// PS `actions.useMove` re-entry (Sleep Talk's called move): unlike `runMove` (Dancer), the
    /// `useMove` path fires NO `BeforeMove` event — the sleep/freeze/recharge/Truant/confusion/
    /// attract/paralysis gauntlet is skipped entirely and the called move resolves with its own
    /// complete draw stream while the user stays asleep. Always paired with `external_move`.
    pub(crate) called: bool,
}

/// Run one move action and append its trailing runAction Update (battle.ts:2882): after EVERY
/// move action completes — hit, miss, immunity, or a fully-cancelled attempt — PS fires
/// `eachEvent('Update')`, which shuffles on a surviving equal-Speed pair. The in-kernel per-hit
/// (970) and post-hit-loop (1024) Updates are emitted inside `execute_move`; this adds the 2882.
fn run_move_action(mut b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    // Fresh per-move outcome flags: this branch may carry a `move_failed` / `pivot_update_done` set
    // by an earlier action this turn (sequence_two_moves reuses the branch for the second mover).
    b.move_failed = false;
    b.pivot_update_done = false;
    b.per_hit_procs_done = false;
    b.drag_tie_speeds = None;
    // Freeze both actives' Speed at this move's start (PS's `updateSpeed()` before the move action)
    // so the move's own internal Updates sort on the pre-move Speed — a paralysis/secondary Speed
    // change the move applies does not retroactively break its own tie. Save/restore for called-move
    // reentrancy (Dancer / Magic Bounce / Instruct re-execute a move within this one).
    let prev_tie_speeds = MOVE_TIE_SPEEDS.with(|c| c.replace(Some([
        effective_speed(&b.state, SideId::One),
        effective_speed(&b.state, SideId::Two),
    ])));
    let mut out = execute_move(b, action);
    // PS's `runAction` ends a MOVE action with `this.eachEvent('Update')` (sim/battle.ts:2882)
    // just as it ends a switch action with one. Run that event's payload here — the per-hit 970
    // Update inside `spreadMoveHit` is a different, earlier event that no status move reaches and
    // that sits ahead of the self-damage in `apply_post_damage` (Substitute's HP cost, Life Orb,
    // recoil) even on a damaging move. See `run_update_event` for the two witnesses.
    for nb in &mut out {
        run_update_event(nb);
    }
    if annotating() {
        for nb in &mut out {
            // `runMove` fires `runEvent('AfterMove')` (sim/battle-actions.ts:312) between `useMove`
            // returning and the action's trailing 2882 — its handler list is speed-sorted, so two
            // `onAnyAfterMove` holders at equal Speed consume one shuffle here.
            emit_after_move_shuffles(nb);
            // Commit PS's `moveLastTurnResult` for the acting side: a move that failed to connect
            // (immune / miss / no-target / blocked) sets it `false` — the signal Stomping Tantrum's
            // base-power doubler reads next turn. The read (BP calc) already happened inside
            // execute_move against the PRIOR value, so committing here is PS's nextTurn semantics.
            // Not diffed (engine-internal); annotation-gated so Enumerate/Sample stay byte-identical.
            let cur = nb.state.side(side).last_move_failed;
            if cur != nb.move_failed {
                push(nb, Instruction::SetLastMoveFailed { side, previous: cur, new: nb.move_failed });
            }
            // A pivot move already emitted its trailing 2882 on the PRE-switch board at the pivot
            // site (PS fires it before processing `switchFlag`); don't re-emit on the post-switch board.
            if !nb.pivot_update_done {
                // Move action's trailing 2882. Normally it sorts on the frozen pre-move Speed; if
                // this action DRAGGED a mon in, PS's `getAllActive()` now contains the REPLACEMENT
                // and its (stale, never-refreshed) cache — see `apply_drag`.
                match nb.drag_tie_speeds {
                    Some(sp) => {
                        let prev = MOVE_TIE_SPEEDS.with(|c| c.replace(Some(sp)));
                        emit_update(nb);
                        MOVE_TIE_SPEEDS.with(|c| c.set(prev));
                    }
                    None => emit_update(nb),
                }
            }
        }
    }
    // Restore AFTER the trailing 2882 emit: PS's `updateSpeed` runs at the END of runAction
    // (battle.ts:2942), so this move's 2882 still sorts on the pre-move Speed; the NEXT action
    // snapshots afresh.
    MOVE_TIE_SPEEDS.with(|c| c.set(prev_tie_speeds));
    out
}

fn resolve_moves(branches: Vec<Branch>, actions: &[Action], exec: &mut Exec) -> Vec<Branch> {
    let mut out = Vec::new();
    for b in branches {
        out.extend(resolve_moves_for_branch(b, actions, exec));
    }
    out
}

fn resolve_moves_for_branch(b: Branch, actions: &[Action], exec: &mut Exec) -> Vec<Branch> {
    match actions.len() {
        0 => vec![b],
        1 => {
            let out = run_move_action(b, actions[0]);
            exec.prune(out)
        }
        _ => {
            let (a, b_act) = (actions[0], actions[1]);
            let order = move_order(&b.state, &a, &b_act);
            match order {
                Order::First(first) => {
                    let (f, s) = if first == a.side { (a, b_act) } else { (b_act, a) };
                    sequence_two_moves(b, f, s, exec)
                }
                // Replicate: a forced order (from PS's commitChoices shuffle bit) collapses the
                // tie to a single realized path — no 50/50 fork, no ambiguity for the differ.
                Order::Tie if forced_tie_order().is_some() => {
                    let a_first = forced_tie_order().unwrap();
                    let (f, s) = if a_first { (a, b_act) } else { (b_act, a) };
                    sequence_two_moves(b, f, s, exec)
                }
                Order::Tie => match exec.coin() {
                    // Sampled: resolve the tie with one coin flip; the branch keeps its full
                    // mass (the flip is the sampled event, not a reweighting).
                    Some(a_first) => {
                        let (f, s) = if a_first { (a, b_act) } else { (b_act, a) };
                        sequence_two_moves(b, f, s, exec)
                    }
                    // Enumerated: 50/50 over the two orderings. PS resolves the equal-speed action
                    // tie with `prng.shuffle` over the committed actions — already emitted as the
                    // commitChoices `shuffle[2,0,2]` in `emit_turn_start_bracket` (item 1), which
                    // both order-branches inherited here. The ordering is validated by `stateAfter`.
                    None => {
                        let ba = scaled(&b, 0.5);
                        let bb = scaled(&b, 0.5);
                        let mut res = sequence_two_moves(ba, a, b_act, exec);
                        res.extend(sequence_two_moves(bb, b_act, a, exec));
                        res
                    }
                },
            }
        }
    }
}

fn scaled(b: &Branch, f: f32) -> Branch {
    Branch { prob: b.prob * f, state: b.state, ins: b.ins.clone(), draws: b.draws.clone(), move_failed: b.move_failed , pivot_update_done: b.pivot_update_done, per_hit_procs_done: b.per_hit_procs_done, pending_damaging_hit: b.pending_damaging_hit, drag_tie_speeds: b.drag_tie_speeds, after_hit_user_alive: b.after_hit_user_alive, late_self_damage: b.late_self_damage, move_any_damage: b.move_any_damage }
}

/// Truant's BeforeMove toggle (PS abilities.ts): if the loaf marker is present the holder
/// removes it and loafs (returns `true` — no move this attempt); otherwise the marker is set
/// for the next attempt and the move proceeds. Callers are responsible for only invoking this
/// where PS's handler would run (after mustrecharge/slp/frz cancels, before flinch/confusion/
/// paralysis ones).
fn truant_gate(b: &mut Branch, side: SideId) -> bool {
    if b.state.side(side).active().ability != crate::ids::Ability::Truant {
        return false;
    }
    if b.state.side(side).volatiles.contains(VolatileStatus::Truant) {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Truant });
        return true;
    }
    push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Truant });
    false
}

/// A flinched mon still runs the BeforeMove handlers ABOVE flinch's priority 8 before the
/// flinch cancels its move: mustrecharge (11) consumes the recharge turn, slp/frz (10) tick
/// the sleep counter / roll the 20% thaw, and Truant (9) toggles its loaf marker. The engine
/// skips move execution for flinched mons wholesale, so those higher-priority effects are
/// replayed here (PS runs handlers in descending priority until one returns false).
/// Discard a cured status's counter.
///
/// PS's `cureStatus()` goes through `setStatus('')`, which REPLACES `pokemon.statusState` wholesale
/// (`sim/pokemon.ts`) — so curing a status always throws its state away: the `slp` timer, the `tox`
/// stage, everything. And `setStatus` of a NEW status re-initialises `statusState` before running
/// the condition's `onStart`, so nothing a previous status left behind can ever be read by the next
/// one.
///
/// The engine cured `status` at several sites and left `status_counter` standing, and the next
/// status to land on that mon inherited it. The load-bearing case is WAKING UP: the sleep cancel
/// wakes the mon at `counter == 1` and only cleared `status`, so the mon carried a phantom counter
/// of 1 — and a Toxic applied later started at stage 1, making its FIRST residual deal 2·maxhp/16
/// instead of 1·maxhp/16 (`tox`'s `onStart` sets `stage = 0` and `onResidual` increments, so the
/// first tick is always stage 1). rb1030 d53 is the witness: p2's Indeedee sleeps at t30, wakes at
/// t36, is Toxic'd by Trevenant at t46, and takes 34 instead of 17 at t47 — engine 61, PS 78, with
/// `status_counter` engine 2 / PS 1. rb1300 d52 is the second.
///
/// Safe at every cure site: it is a no-op when the counter is already 0, which is every status
/// except `slp` and `tox`.
fn clear_status_counter(b: &mut Branch, side: SideId, slot: u8) {
    let previous = b.state.side(side).pokemon[slot as usize].status_counter;
    if previous != 0 {
        push(b, Instruction::ChangeStatusCounter { side, slot, previous, new: 0 });
    }
}

fn flinch_cancel_chain(mut b: Branch, side: SideId) -> Vec<Branch> {
    // Glaive Rush's drawback removal (BeforeMove priority 100) precedes every cancel.
    if b.state.side(side).volatiles.contains(VolatileStatus::GlaiveRush) {
        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::GlaiveRush });
    }
    let (status, counter) = {
        let p = b.state.side(side).active();
        (p.status, p.status_counter)
    };
    let slot = b.state.side(side).active_index;
    let pending = b.state.side(side).pending_move;
    if matches!(pending, crate::state::PendingMove::Recharging) {
        // mustrecharge (11) removes itself and cancels before slp/frz/Truant/flinch run.
        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: crate::state::PendingMove::None });
        clear_recharge_volatiles(&mut b, side);
        return vec![b];
    }
    match status {
        Status::Sleep => {
            if counter > 1 {
                // Still asleep: slp (10) cancels first; flinch never runs, Truant untouched.
                push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: counter - 1 });
                return vec![b];
            }
            // Wakes, Truant (9) toggles, then flinch (8) cancels the move.
            push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
            clear_status_counter(&mut b, side, slot);
            truant_gate(&mut b, side);
            vec![b]
        }
        Status::Freeze => {
            // frz (10): 80% stay frozen (cancelled before flinch); 20% thaw, then Truant
            // toggles, then the flinch cancels the move anyway.
            let mut out = vec![scaled(&b, 0.80)];
            let mut thawed = scaled(&b, 0.20);
            push(&mut thawed, Instruction::ChangeStatus { side, slot, previous: Status::Freeze, new: Status::None });
            truant_gate(&mut thawed, side);
            out.push(thawed);
            out
        }
        _ => {
            truant_gate(&mut b, side);
            vec![b]
        }
    }
}

fn sequence_two_moves(b: Branch, mut first: Action, second: Action, exec: &mut Exec) -> Vec<Branch> {
    // Tell the first mover what the (not-yet-moved) second mover is about to do, so Sucker
    // Punch / Thunderclap can tell whether the target is attacking. The second mover's foe
    // (the first) has already acted, so it stays None.
    first.foe_pending_move = Some(b.state.side(second.side).active().moves[second.move_idx as usize].id);
    // PS's queued action names a POKEMON, and `runAction`'s `case 'move'` opens with
    // `if (!action.pokemon.isActive) return false` — so a second mover the FIRST mover forced off
    // the field (Dragon Tail / Whirlwind / Roar / Circle Throw, Red Card, Eject Button) never
    // acts, and the replacement that took its slot does not inherit the action. The engine's
    // action names only a SIDE, so pin the party slot here and compare after the first move.
    // rb1360 d6 t6: both sides pick Dragon Tail, p1's is faster, PS's draw stream ENDS at the
    // drag `sample[4]` — the engine went on to spend the incoming Empoleon's Roost PP and emit
    // two more Update shuffles (PS's `return false` also skips the trailing 2882 Update).
    let second_slot = b.state.side(second.side).active_index;
    let mut out = Vec::new();
    // The prune between the movers is what kills the branch cross-product in Sample mode:
    // the second mover executes on one sampled first-move outcome instead of all of them.
    // `run_move_action` appends the first mover's runAction Update (2882) to each outcome.
    for fb in exec.prune(run_move_action(b, first)) {
        // The second mover acts only if its active is alive and wasn't flinched by the first.
        let flinched = fb.state.side(second.side).volatiles.contains(VolatileStatus::Flinch);
        // Once the first action ends the battle (for example Life Orb recoil KOs that side's
        // final Pokémon), PS never runs the queued slower action and therefore pays no PP for
        // it.  Do not use merely `first mover is alive` here: if it has a replacement available
        // PS can continue the queue, and that broader condition regresses valid Memento/status
        // cases.
        if fb.state.side(second.side).active_index == second_slot
            && fb.state.side(second.side).active().is_alive()
            && !battle_over(&fb.state)
        {
            if flinched {
                // The move is cancelled, but the BeforeMove handlers above flinch's priority
                // still run (sleep tick / thaw roll / Truant toggle / recharge consumption). The
                // flinched mon's move action still completes, so its runAction Update (2882) fires.
                let mut fc = flinch_cancel_chain(fb, second.side);
                if annotating() {
                    for nb in &mut fc {
                        emit_update(nb);
                    }
                }
                out.extend(fc);
            } else {
                // `run_move_action` appends the second mover's runAction Update (2882).
                out.extend(run_move_action(fb, second));
            }
        } else {
            // The second action never runs (its user fainted or the battle ended) — no 2882.
            out.push(fb);
        }
    }
    exec.prune(out)
}

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Order {
    First(SideId),
    Tie,
}

/// The defender's types as the type chart should see them: a Scrappy attacker's Normal /
/// Fighting moves treat the target's Ghost type as neutral (immunity removed).
/// Freeze-Dry is the only gen9-legal move with an `onEffectiveness` handler that randbats can
/// roll (Flying Press / Thousand Arrows are Past, Tar Shot is a volatile on the target).
fn is_freeze_dry(md: &crate::data::MoveData) -> bool {
    md.id.to_id() == "freezedry"
}

fn effective_def_types(scrappy: bool, move_type: Type, types: [Type; 2]) -> [Type; 2] {
    if scrappy && matches!(move_type, Type::Normal | Type::Fighting) {
        let unghost = |t: Type| if t == Type::Ghost { Type::None } else { t };
        [unghost(types[0]), unghost(types[1])]
    } else {
        types
    }
}

/// Fixed-damage moves bypass the damage formula (PS implements them via a `damage` field or
/// callback). Returns the HP to remove, or `None` if `md` isn't one of them. Type immunity
/// is handled separately by the caller's `connects` check.
fn fixed_damage_amount(md: &crate::data::MoveData, state: &State, side: SideId) -> Option<i16> {
    let attacker = state.side(side).active();
    let defender = state.side(side.other()).active();
    Some(match md.id.to_id() {
        "seismictoss" | "nightshade" => attacker.level as i16,
        "dragonrage" => 40,
        "sonicboom" => 20,
        // Super Fang halves the target's *current* HP (min 1); Endeavor brings it down to the
        // attacker's current HP (0 if the target is already at or below it).
        "superfang" | "ruination" => (defender.hp / 2).max(1),
        "endeavor" => (defender.hp - attacker.hp).max(0),
        // Mirror Coat returns 2× the special damage its user took from the foe this turn (PS
        // `damageCallback`). With no such hit recorded it deals nothing (PS `onTry` fails).
        "mirrorcoat" => (state.side(side).special_damage_taken * 2).max(0),
        _ => return None,
    })
}

/// PS's `getActionSpeed` priority: the move's base priority run through the FULL
/// `ModifyPriority` handler set, which in gen 9 is exactly four handlers — Prankster
/// (`data/abilities.ts:3380`, +1 to the user's status moves), Gale Wings (`:1543`, +1 to a Flying
/// move at full HP), Triage (`:5129`, **+3 to any move with `flags.heal`**) and Grassy Glide's own
/// `onModifyPriority` (`data/moves.ts:7664`, +1 on Grassy Terrain while grounded).
/// (`Dex.forGen(9).abilities.all().filter(a => a.onModifyPriority)` -> galewings, prankster,
/// triage; `data/moves.ts` -> grassyglide. Mycelium Might's −0.1 is FRACTIONAL and never crosses
/// the `> 0.1` tests below.)
///
/// This is the value every `priority > 0.1` predicate reads: `getActionSpeed` writes it back onto
/// the ActiveMove (`if (this.gen > 5) action.move.priority = priority`, `sim/battle.ts:2670`), so
/// the priority BLOCKERS — Psychic Terrain, Dazzling / Queenly Majesty / Armor Tail — see the
/// boosted number, not the dex one. Keeping one function for all of them is the point: the two
/// blockers used to carry their own hand-copied three-handler list and both missed Triage
/// (rb1348 d12 t11 — a Triage Comfey's Draining Kiss is +3 and Psychic Terrain blocks it dead,
/// zero draws in PS's whole unit; the engine rolled the move and dealt 69).
fn modified_priority(state: &State, side: SideId, md: &crate::data::MoveData) -> i8 {
    let p = state.side(side).active();
    let mut pri = md.priority;
    if md.category == MoveCategory::Status && p.ability == crate::ids::Ability::Prankster {
        pri += 1;
    }
    if md.id.to_id() == "grassyglide"
        && state.terrain == crate::ids::Terrain::Grassy
        && is_grounded(state, side)
    {
        pri += 1;
    }
    if p.ability == crate::ids::Ability::GaleWings && md.typ == Type::Flying && p.hp >= p.max_hp {
        pri += 1;
    }
    if md.flag_heal && p.ability == crate::ids::Ability::Triage {
        pri += 3;
    }
    pri
}

/// A move's effective priority (turn order), by move slot. See [`modified_priority`].
fn effective_priority(state: &State, side: SideId, move_idx: u8) -> i8 {
    let md = move_data(state.side(side).active().moves[move_idx as usize].id);
    modified_priority(state, side, &md)
}

/// The move data an ACTION will actually resolve with — which is not always the chosen slot's.
///
/// `runMove` (`sim/battle-actions.ts:255-275`) replaces the chosen move with Struggle when the
/// mon has nothing usable (`if (!moveSlot?.pp) { move = dex.moves.get('struggle') }`), and the
/// queue's `getActionSpeed` reads `action.move.priority` — i.e. **Struggle's priority, 0** — not
/// the empty slot's. A called/external move (Sleep Talk's pick, a Dancer copy) likewise resolves
/// as itself.
///
/// The engine keyed priority off `active.moves[move_idx]`, so a mon Struggling because its
/// Choice-locked slot ran out of PP inherited THAT slot's priority. rb5081 d49 t39: a
/// choice-locked Ditto, transformed into Glaceon, is locked onto a 0-PP Protect and Struggles.
/// The engine gave the Struggle Protect's **+4**, moved it before the foe's real Protect, and
/// the recoil KO'd its last mon and ended the battle. PS runs the foe's Protect first, blocks
/// the Struggle outright, and its only draw for the turn is the residual protect/stall shuffle.
fn action_move_data(state: &State, act: &Action) -> crate::data::MoveData {
    if let Some(ext) = act.external_move {
        return move_data(ext);
    }
    let p = state.side(act.side).active();
    let slot = p.moves[act.move_idx as usize];
    if act.struggling || slot.pp == 0 {
        if let Some(id) = crate::ids::MoveId::from_id("struggle") {
            return move_data(id);
        }
    }
    move_data(slot.id)
}

pub(crate) fn move_order(state: &State, a: &Action, b: &Action) -> Order {
    let (sa, sb) = (a.side, b.side);
    let (mda, mdb) = (action_move_data(state, a), action_move_data(state, b));
    let pa = modified_priority(state, sa, &mda);
    let pb = modified_priority(state, sb, &mdb);
    if pa != pb {
        return Order::First(if pa > pb { sa } else { sb });
    }
    // Fractional priority inside an equal bracket: Custap Berry +0.1 (already consumed at
    // queue time — the flag rides on the Action) and Mycelium Might −0.1 on status moves.
    let frac = |act: &Action| -> i8 {
        let mut f = 0i8;
        if act.custap {
            f += 1;
        }
        let p = state.side(act.side).active();
        if p.ability == crate::ids::Ability::MyceliumMight
            && action_move_data(state, act).category == MoveCategory::Status
        {
            f -= 1;
        }
        f
    };
    let (fa, fb) = (frac(a), frac(b));
    if fa != fb {
        return Order::First(if fa > fb { sa } else { sb });
    }
    let va = effective_speed(state, sa);
    let vb = effective_speed(state, sb);
    let faster_is_higher = !state.trick_room;
    if va == vb {
        Order::Tie
    } else if (va > vb) == faster_is_higher {
        Order::First(sa)
    } else {
        Order::First(sb)
    }
}

const ALL_VOLATILES: &[VolatileStatus] = &[
    VolatileStatus::Confusion, VolatileStatus::Substitute, VolatileStatus::LeechSeed,
    VolatileStatus::Taunt, VolatileStatus::Encore, VolatileStatus::Disable,
    VolatileStatus::Protect, VolatileStatus::Endure, VolatileStatus::Flinch,
    VolatileStatus::Roost, VolatileStatus::Charge, VolatileStatus::Yawn,
    VolatileStatus::PerishSong, VolatileStatus::DestinyBond, VolatileStatus::Curse,
    VolatileStatus::Nightmare, VolatileStatus::Attract, VolatileStatus::Torment,
    VolatileStatus::SaltCure, VolatileStatus::GlaiveRush, VolatileStatus::LockedMove,
    VolatileStatus::MustRecharge, VolatileStatus::PartiallyTrapped, VolatileStatus::Roosted,
    // PS clears `choicelock` on switch-out (the lock re-picks on re-entry) — cosim caught the
    // engine retaining it across switches.
    VolatileStatus::ChoiceLock,
    VolatileStatus::ThroatChop, VolatileStatus::HealBlock, VolatileStatus::TypeShifted,
    // Magnet Rise is a plain volatile with no `onSwitchOut`, so `clearVolatile` drops it
    // (rb1173 t3: PS's Klefki pivots out and its `magnetrise` is gone).
    VolatileStatus::MagnetRise,
    // Trapping volatiles clear when their holder leaves the field (self-traps and the trapped
    // mon's own copy). Foe-sourced traps additionally end when the TRAPPER leaves — handled by
    // `clear_foe_sourced_traps` in `apply_switch_inner`.
    VolatileStatus::Trapped, VolatileStatus::Ingrain, VolatileStatus::NoRetreat, VolatileStatus::Octolock,
    // Protosynthesis / Quark Drive end on switch-out (PS ability `onEnd` deletes the volatile);
    // they are re-derived on switch-in in `apply_switch_in_ability` (weather/terrain, else the
    // one-shot Booster Energy). A mon that stays in never reaches this path, so its boost persists.
    VolatileStatus::Protosynthesis, VolatileStatus::QuarkDrive, VolatileStatus::ProtoBooster,
    // Flash Fire's activation ends on switch-out (PS ability `onEnd` removes the volatile).
    VolatileStatus::FlashFire,
    // Unburden likewise: the ability's `onEnd` removes the volatile when its holder leaves the
    // field, and nothing re-adds it on entry (PS only adds it from `onAfterUseItem`/`onTakeItem`,
    // so a mon that re-enters already itemless does NOT get the Speed doubling back). The engine
    // kept it across the switch and then read a doubled Speed for whatever entered next — 10
    // extension games, e.g. rb1062: Hawlucha's White Herb is eaten by the turn-1 Intimidate drop,
    // granting `unburden`; it pivots to Politoed and PS's volatiles go empty while the engine's
    // stayed at bit 28.
    VolatileStatus::Unburden,
    // Truant's loaf marker is a PS volatile — cleared on switch-out; the mon acts on its
    // first attempt after re-entering. `statsRaisedThisTurn` is a per-Pokémon PS field, but
    // only the active can have raised a stat this turn, so the volatile model is exact.
    VolatileStatus::Truant, VolatileStatus::StatsRaisedThisTurn,
];

/// Crit chance for this attack. Stages: base 0 (1/24), +(crit_ratio-1) from the move (Slash
/// family), +2 Focus Energy / Dragon Cheer, +1 Scope Lens / Razor Claw, +1 Super Luck;
/// stages 0/1/2/3+ -> 1/24, 1/8, 1/2, always. Always-crit moves crit unconditionally;
/// Battle Armor / Shell Armor (unless ignored) never crit.
fn crit_chance(b: &Branch, side: SideId, md: &crate::data::MoveData) -> f32 {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let def = b.state.side(foe).active();
    let mb = matches!(b.state.side(side).active().ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze);
    if !mb && matches!(def.ability, Ab::BattleArmor | Ab::ShellArmor) {
        return 0.0;
    }
    if md.always_crit {
        return 1.0;
    }
    let mut stage = (md.crit_ratio.max(1) - 1) as u32;
    if b.state.side(side).volatiles.contains(VolatileStatus::FocusEnergy) {
        stage += 2;
    }
    if matches!(b.state.side(side).active().item, Item::ScopeLens | Item::RazorClaw) {
        stage += 1;
    }
    if b.state.side(side).active().ability == Ab::SuperLuck {
        stage += 1;
    }
    match stage {
        0 => 1.0 / 24.0,
        1 => 1.0 / 8.0,
        2 => 0.5,
        _ => 1.0,
    }
}

/// The denominator PS passes to `randomChance(1, critMult[critRatio])` in the crit step
/// (battle-actions.ts:1645), or 0 when PS makes no roll. PS rolls whenever `willCrit === undefined`
/// and the crit stage ≥ 1 — INDEPENDENT of target crit-immunity: Battle Armor / Shell Armor /
/// Lucky Chant hook `CriticalHit`, which only downgrades the *result* AFTER the roll, not
/// `ModifyCritRatio`. Always-crit moves set `willCrit = true` and skip the roll. gen9 crit
/// denominators by stage (critRatio-1): 0→24, 1→8, 2→2, 3+→1 (`critMult = [0,24,8,2,1]`).
/// Mirrors `crit_chance`'s stage math but ignores the immunity short-circuit and returns the
/// exact PS call denominator (so crit-immune and always-via-ratio hits still roll).
fn ps_crit_den(b: &Branch, side: SideId, md: &crate::data::MoveData) -> i32 {
    use crate::ids::Ability as Ab;
    if md.always_crit {
        return 0; // willCrit === true → PS skips the roll
    }
    let mut stage = (md.crit_ratio.max(1) - 1) as u32;
    if b.state.side(side).volatiles.contains(VolatileStatus::FocusEnergy) {
        stage += 2;
    }
    if matches!(b.state.side(side).active().item, Item::ScopeLens | Item::RazorClaw) {
        stage += 1;
    }
    if b.state.side(side).active().ability == Ab::SuperLuck {
        stage += 1;
    }
    match stage {
        0 => 24,
        1 => 8,
        2 => 2,
        _ => 1,
    }
}

/// Maximum hit count for which we enumerate the *full* per-hit (roll × crit) product.
/// At 3 hits that's 32³ = 32,768 branches (~28 MB of states) — fine. Above this the
/// product explodes (Population Bomb's 10 hits → 32¹⁰ ≈ 1.1e15 branches, ~1 EB, which
/// hard-crashes the machine), so high-hit moves take the sumset-DP path instead.
const MAX_EXACT_HITS: usize = 3;

/// The damage computation for one move: the 16 damage rolls (non-crit and crit) plus the
/// defender-side fields needed *after* damage is applied (Sturdy/Sash, contact punishers,
/// Life Orb). Computed once per move; both the exact and the DP hit paths consume it.
struct DamageCalc {
    rolls_nocrit: [i16; 16],
    rolls_crit: [i16; 16],
    def_ability: crate::ids::Ability,
    def_item: Item,
    def_maxhp: i16,
    life_orb: bool,
}

/// PS's `lockedmove` volatile carries `duration: 2` and is re-armed ONLY by its `onRestart`,
/// which fires when the user ACTUALLY re-uses the locked move (`addVolatile` from the move's
/// `self` effect). A turn whose attempt is cancelled by a BeforeMove handler — confusion
/// self-hit, attract, full paralysis, freeze, Truant — never re-arms it, so the residual loop
/// takes `duration` 1 -> 0 and ENDS the volatile (`if (eventid === 'Residual' && handler.end &&
/// handler.state?.duration)`, sim/battle.ts:515-522). `lockedmove.onEnd` then confuses only
/// `if (this.effectState.trueDuration <= 1)` (data/conditions.ts:277-279) — so with rampage
/// turns still to run the lock simply DROPS, with no confusion and no duration draw.
/// (`trueDuration` is the engine's `Rampaging(_, n)`, still holding its start-of-turn value at
/// move time because the residual's `onResidual` decrement is skipped on the ending turn.)
fn unarm_rampage_on_cancel(b: &mut Branch, side: SideId) {
    use crate::state::PendingMove;
    let pending = b.state.side(side).pending_move;
    let PendingMove::Rampaging(_, n) = pending else { return };
    // At n == 1 `onEnd` DOES reach `target.addVolatile('confusion')`. That is a no-op — and
    // rolls NO `random(2, 6)` duration draw — when the user is already confused (`addVolatile`
    // on a present volatile only calls `onRestart`, and `confusion` has none) or has Own Tempo;
    // a confusion self-hit cancel is by construction already confused. The one case still open
    // is n == 1 with a NON-confused user (attract / full paralysis / freeze cancel): that needs
    // the fresh `random(2, 6)` at the RESIDUAL stream position, so it is left untouched here.
    if n < 2
        && !b.state.side(side).volatiles.contains(VolatileStatus::Confusion)
        && b.state.side(side).active().ability != crate::ids::Ability::OwnTempo
    {
        return;
    }
    push(b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
    if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
    }
}

/// The `BeforeMove` handlers at priorities 7 / 6 / 5 — Disable (`onBeforeMovePriority: 7`),
/// Throat Chop / Heal Block / Gravity (6) and Taunt (5). Each returns `false`, and PS's
/// `runEvent` short-circuits there, so NOTHING below them runs: not confusion's countdown and
/// 1/3 roll (3), not Attract's 1/2 (2), not paralysis' 1/4 (1), and not `deductPP`.
///
/// Enumerated from the pin rather than recalled — the full descending ladder is
/// `100` Glaive Rush / Grudge / Rage / Chilly Reception (marker removal, never a cancel),
/// `11` mustrecharge, `10` slp + frz, `9` Truant, `8` flinch, `7` Disable, `6` Gravity +
/// Heal Block + Throat Chop, `5` Taunt, `3` confusion, `2` Attract, `1` par, `0` choicelock +
/// Gorilla Tactics, `-1` Destiny Bond. Everything at 8 and above is handled by `execute_move`'s
/// earlier gates; this is the 7/6/5 rung, and it belongs ABOVE the 3/2/1 draw ladder.
fn before_move_blocked_7_6_5(state: &State, side: SideId, md: &crate::data::MoveData) -> bool {
    let s = state.side(side);
    // Disable: only the named move.
    (s.disable.0 != crate::ids::MoveId::None && s.disable.0 == md.id)
        // Throat Chop: sound moves. Heal Block: heal-flag moves (incl. drains).
        || (s.volatiles.contains(VolatileStatus::ThroatChop) && md.flag_sound)
        || (s.volatiles.contains(VolatileStatus::HealBlock) && md.flag_heal)
        // Taunt: every status move.
        || (s.taunt_turns > 0 && md.category == MoveCategory::Status)
}

/// Execute one move, first splitting on confusion: a confused, awake mon has a 1/3 chance to
/// hit itself instead of acting. The 2/3 "acts normally" branch is identical to no-confusion
/// behavior, so this only *adds* the self-hit outcomes (no regression on the common path).
pub(crate) fn execute_move(b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    let mut b = b;
    let (alive, status, _confused) = {
        let p = b.state.side(side).active();
        (p.is_alive(), p.status, b.state.side(side).volatiles.contains(VolatileStatus::Confusion))
    };
    // Glaive Rush's drawback ends the moment its user next attempts a move: PS removes the
    // volatile at BeforeMove priority 100 — ABOVE every cancel handler (recharge, sleep,
    // freeze, Truant, flinch, paralysis), so even a fully-cancelled attempt clears it.
    if alive && b.state.side(side).volatiles.contains(VolatileStatus::GlaiveRush) {
        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::GlaiveRush });
    }
    // Sleep/freeze are handled inside (the mon can't act anyway). For an awake mon, split off
    // confusion self-hit (1/3) and full paralysis (1/4 of the remainder) — both branches where
    // the move doesn't execute. The remaining "acts normally" branch equals prior behavior, so
    // these only *add* outcomes (no regression on the common path).
    if !alive {
        return dispatch_move_inner(b, action);
    }
    if status == Status::Sleep {
        // slp's `onBeforeMove` (priority 10) ticks the counter and returns `false` — cancelling
        // the attempt and short-circuiting `runEvent` — ONLY while the mon stays asleep. Gen-9
        // sleep is a deterministic countdown, so whether this attempt wakes it is decidable here.
        let (counter, early) = {
            let p = b.state.side(side).active();
            (p.status_counter, p.ability == crate::ids::Ability::EarlyBird)
        };
        let tick = if early { 2 } else { 1 };
        if counter > tick {
            // Still asleep: `execute_move_inner` owns the tick and the `sleepUsable`
            // (Sleep Talk / Snore) exception, and nothing below slp runs.
            return dispatch_move_inner(b, action);
        }
        // WAKES. The handler returns `undefined`, so the ladder CONTINUES — Truant (9),
        // Disable/Taunt (7/6/5), confusion (3), Attract (2). `execute_move_inner`'s own wake
        // block is a no-op after this (it re-reads the status, which is now None).
        let slot = b.state.side(side).active_index;
        push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
        clear_status_counter(&mut b, side, slot);
    }
    // Freeze: 20% chance to thaw and act this turn, otherwise stay frozen (no move).
    // PS frz `onBeforeMove` (priority 10) rolls `randomChance(1, 5)` to thaw.
    if status == Status::Freeze {
        // A `defrost`-flagged move (Flame Wheel, Scald, Sacred Fire, …) thaws the user
        // deterministically with NO `randomChance` roll (PS frz `onBeforeMove` returns early on
        // `move.flags['defrost']`). The engine had rolled 80/20 for every frozen mover — the
        // no-thaw branch and its draw were both spurious for these moves.
        let move_id = b.state.side(side).active().moves[action.move_idx as usize].id;
        if is_defrost_move(move_id) {
            let slot = b.state.side(side).active_index;
            push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Freeze, new: Status::None });
            clear_status_counter(&mut b, side, slot);
            // Thawed: frz's handler returned `undefined`, so the rest of the ladder runs
            // (Truant included — `before_move_lower_ladder` owns the gate now).
            return before_move_lower_ladder(b, action);
        }
        let mut frozen = scaled(&b, 0.80);
        draw(&mut frozen, "randomChance", &[1, 5], 0, "frz");
        let mut out = vec![frozen];
        let mut thawed = scaled(&b, 0.20);
        draw(&mut thawed, "randomChance", &[1, 5], 1, "frz");
        let slot = thawed.state.side(side).active_index;
        push(&mut thawed, Instruction::ChangeStatus { side, slot, previous: Status::Freeze, new: Status::None });
        clear_status_counter(&mut thawed, side, slot);
        // frz (priority 10) returned `undefined` on the thaw; Truant (9) and everything below
        // it still run.
        out.extend(before_move_lower_ladder(thawed, action));
        return out;
    }
    before_move_lower_ladder(b, action)
}

/// The tail of PS's `BeforeMove` ladder, everything BELOW slp / frz (priority 10): Truant (9),
/// Disable (7) / Throat Chop / Heal Block / Gravity (6) / Taunt (5), confusion (3), Attract (2),
/// paralysis (1).
///
/// Three callers, and that is the point. `runEvent` short-circuits only on a handler that returns
/// `false`, and **slp's and frz's handlers return `undefined` when the mon WAKES or THAWS** —
/// they return `false` only when it stays asleep/frozen. So a mon that woke this turn, or thawed
/// this turn, still runs every handler below, exactly like an awake one.
///
/// rb5043 d45 t37 is the witness for the sleep half: a confused, sleeping Staraptor is hit by
/// Hurricane, wakes on the expiry turn, and PS immediately rolls its confusion
/// `randomChance(33, 100)` — it hits itself, `random(16)` for the damage, and faints. The engine
/// returned straight into the move machinery on `status == Sleep`, skipped the confusion / Attract
/// / paralysis handlers entirely, and let the Roost through with 183 HP where PS has 0.
///
/// Status is re-read from the CURRENT board rather than captured before the wake, so the
/// paralysis branch is skipped for a just-woken mon without a special case.
fn before_move_lower_ladder(b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    let (status, confused) = {
        let p = b.state.side(side).active();
        (p.status, b.state.side(side).volatiles.contains(VolatileStatus::Confusion))
    };
    // Truant: PS onBeforeMove priority 9 — below mustrecharge (11) and slp/frz (10), above
    // flinch (8), Disable, confusion (3) and paralysis (1). The volatile marks "loaf on this
    // attempt"; the toggle fires on every attempt that reaches this point, including ones a
    // later handler (confusion self-hit, full paralysis) cancels. Recharge turns are resolved
    // in `execute_move_inner` and leave the toggle untouched (mustrecharge preempts Truant).
    let mut b = b;
    if !matches!(b.state.side(side).pending_move, crate::state::PendingMove::Recharging)
        && truant_gate(&mut b, side)
    {
        return vec![b];
    }
    // Disable (7), Throat Chop / Heal Block / Gravity (6) and Taunt (5) come NEXT in PS's
    // `BeforeMove` ladder — above confusion (3), Attract (2) and paralysis (1). `runEvent`
    // short-circuits on the first handler that returns false, so a mon whose move one of these
    // cancels never reaches the lower handlers: **no paralysis roll, no Attract roll, no
    // confusion countdown, and no PP.** The engine ran them the other way round, inside
    // `execute_move_inner`, below this whole ladder.
    //
    // rb1493 d46 t39 is the witness: a paralyzed Chansey picks Heal Bell, Ursaring's Throat Chop
    // lands first, and PS's unit records the four Throat Chop draws and NOTHING else — no
    // `randomChance[1, 4]@par`. The engine rolled it, went one draw ahead of PS for the rest of
    // the game, and the offset first showed up a unit later as a missing Seismic Toss accuracy.
    // (rb1649 is the same shape.)
    {
        let attacker = b.state.side(side).active();
        let mid = action.external_move.unwrap_or(attacker.moves[action.move_idx as usize].id);
        let struggling = action.external_move.is_none()
            && (attacker.moves[action.move_idx as usize].pp == 0 || action.struggling);
        if !struggling && before_move_blocked_7_6_5(&b.state, side, &move_data(mid)) {
            return vec![b];
        }
    }
    // Confusion counts down on each move attempt and ends ("snapped out") at 0, in which
    // case the mon acts normally this turn (PS decrements before the 1/3 roll).
    let mut b = b;
    let mut confused = confused;
    if confused {
        let t = b.state.side(side).confusion_turns;
        let new_t = t.saturating_sub(1);
        push(&mut b, Instruction::SetActiveCounter {
            side,
            which: crate::instruction::ActiveCounter::Confusion,
            previous: t,
            new: new_t,
        });
        if new_t == 0 {
            push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Confusion });
            confused = false;
        }
    }
    // PS runs the BeforeMove cancel handlers in descending priority, each rolling its own draw:
    // confusion (priority 3) `randomChance(33, 100)`, attract (2) `randomChance(1, 2)`,
    // paralysis (1) `randomChance(1, 4)`. A handler that fires cancels the move (its later
    // handlers don't roll). Annotate the roll on the cancel branch AND on the pass-through `b`.
    let mut out = Vec::new();
    let mut act = 1.0f32;
    if confused {
        let mut hit = scaled(&b, act * 0.33);
        draw(&mut hit, "randomChance", &[33, 100], 1, "confusion");
        out.extend(confusion_self_hit(hit, side));
        draw(&mut b, "randomChance", &[33, 100], 0, "confusion");
        act *= 0.67;
    }
    if b.state.side(side).volatiles.contains(VolatileStatus::Attract) {
        let mut imm = scaled(&b, act * 0.5); // immobilized: no move
        draw(&mut imm, "randomChance", &[1, 2], 1, "attract");
        out.push(imm);
        draw(&mut b, "randomChance", &[1, 2], 0, "attract");
        act *= 0.5;
    }
    if status == Status::Paralysis {
        let mut fp = scaled(&b, act * 0.25); // fully paralyzed: no move
        draw(&mut fp, "randomChance", &[1, 4], 1, "par");
        out.push(fp);
        draw(&mut b, "randomChance", &[1, 4], 0, "par");
        act *= 0.75;
    }
    if out.is_empty() {
        dispatch_move_inner(b, action)
    } else {
        // Every branch in `out` is a CANCELLED attempt (confusion self-hit / attract / full
        // paralysis): the locked move was not re-used, so its `lockedmove` volatile is not
        // re-armed and expires at this turn's residual.
        for nb in &mut out {
            unarm_rampage_on_cancel(nb, side);
        }
        out.extend(dispatch_move_inner(scaled(&b, act), action));
        out
    }
}

/// Resolve Shell Side Arm's category before entering the move machinery. PS `onModifyMove`
/// compares two floor-chain damage estimates built from boosted-but-UNMODIFIED stats
/// (`getStat(x, false, true)` — stages included, items/abilities/burn excluded):
///   est = floor(floor(floor(floor(2·L/5 + 2) · 90 · off) / def) / 50)
/// Strictly greater → Physical (and the move gains contact); an exact tie flips
/// `randomChance(1, 2)` → enumerate both categories at ½ each. Everything else routes
/// straight through.
fn dispatch_move_inner(b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    let is_ssa = action.shell_phys.is_none() && {
        let p = b.state.side(side).active();
        p.is_alive()
            && p.moves[action.move_idx as usize].id.to_id() == "shellsidearm"
            && p.moves[action.move_idx as usize].pp > 0
    };
    if !is_ssa || !b.state.side(side.other()).active().is_alive() {
        return execute_move_inner(b, action);
    }
    let (phys, spec) = {
        let atk_p = b.state.side(side).active();
        let def_p = b.state.side(side.other()).active();
        let level = atk_p.level as i64;
        let atk = boosted_stat(atk_p.stat(crate::ids::StatIndex::Attack) as i64, b.state.side(side).boost(BoostIndex::Attack));
        let spa = boosted_stat(atk_p.stat(crate::ids::StatIndex::SpecialAttack) as i64, b.state.side(side).boost(BoostIndex::SpecialAttack));
        let def = boosted_stat(def_p.stat(crate::ids::StatIndex::Defense) as i64, b.state.side(side.other()).boost(BoostIndex::Defense)).max(1);
        let spd = boosted_stat(def_p.stat(crate::ids::StatIndex::SpecialDefense) as i64, b.state.side(side.other()).boost(BoostIndex::SpecialDefense)).max(1);
        let est = |off: i64, dfn: i64| ((2 * level / 5 + 2) * 90 * off / dfn) / 50;
        (est(atk, def), est(spa, spd))
    };
    if phys > spec {
        return execute_move_inner(b, Action { shell_phys: Some(true), ..action });
    }
    if phys < spec {
        return execute_move_inner(b, Action { shell_phys: Some(false), ..action });
    }
    // The tie is a REAL `randomChance(1, 2)` (`data/moves.ts:16242`), and it has to be EMITTED, not
    // just enumerated: the engine forked the two categories at ½ each but drew nothing, so every
    // later draw in the unit read one slot early. `onModifyMove` runs in `useMoveInner`, i.e. after
    // the whole `BeforeMove` ladder (which `execute_move` has already rolled above this call) and
    // before `hitStepAccuracy` — exactly this position. rb1347 d78 t71 is the witness: a
    // Slowbro-Galar with atk == spa == 224 into a Salamence, PS
    // `randomChance[1,2] = true @ shellsidearm ModifyMove` and then the accuracy roll; the engine
    // went straight to accuracy and was one draw behind for the rest of the game (d80's Psychic
    // read roll 11 where PS read 9 — a state diff on an OFFSET stream, two decisions downstream).
    let mut phys_b = scaled(&b, 0.5);
    draw(&mut phys_b, "randomChance", &[1, 2], 1, "shellsidearm");
    let mut spec_b = scaled(&b, 0.5);
    draw(&mut spec_b, "randomChance", &[1, 2], 0, "shellsidearm");
    let mut out = execute_move_inner(phys_b, Action { shell_phys: Some(true), ..action });
    out.extend(execute_move_inner(spec_b, Action { shell_phys: Some(false), ..action }));
    out
}

/// Freshly-applied Sleep: branch the duration (PS `statusState.time = random(2, 5)` — the
/// mon misses `time - 1` turns; uniform over 2/3/4).
fn branch_sleep_counter(b: Branch, side: SideId) -> Vec<Branch> {
    let slot = b.state.side(side).active_index;
    let prev = b.state.side(side).active().status_counter;
    [2u8, 3, 4]
        .into_iter()
        .map(|t| {
            let mut nb = scaled(&b, 1.0 / 3.0);
            // PS `slp` condition `onStart`: `statusState.time = this.random(2, 5)` (2/3/4). The
            // duration draw is consumed the moment sleep is applied.
            draw(&mut nb, "random", &[2, 5], t as i64, "slp");
            push(&mut nb, Instruction::ChangeStatusCounter { side, slot, previous: prev, new: t });
            nb
        })
        .collect()
}

/// PS's `slp` condition rolls its `random(2, 5)` duration in `onStart` — the instant the status is
/// SET, and therefore BEFORE the holder's Lum/Chesto Berry `onUpdate` gets to cure it. rb1297 t17:
/// Sleep Powder lands on a Lum Berry Roaring Moon; PS rolls the duration, the berry then wipes the
/// status, and the mon Outrages normally that same turn — but the draw was still made.
///
/// Call with `applied` = "the sleep was pushed", AFTER the cure attempt. Returns whether the sleep
/// survived (the caller then forks the real duration via `branch_sleep_counter`); when it did not,
/// emits the duration roll here as a draw-and-discard so the stream stays aligned without a
/// pointless 3-way fork over a counter that was just cleared.
fn sleep_survived_or_discard_duration(b: &mut Branch, side: SideId, applied: bool) -> bool {
    if !applied {
        return false;
    }
    if b.state.side(side).active().status == Status::Sleep {
        return true;
    }
    draw(b, "random", &[2, 5], 2, "slp");
    false
}

/// Freshly-applied Confusion: branch the duration (PS `time = random(2, 6)`; uniform 2-5,
/// decremented on each move attempt, snaps out at 0).
fn branch_confusion_counter(b: Branch, side: SideId) -> Vec<Branch> {
    let prev = b.state.side(side).confusion_turns;
    [2u8, 3, 4, 5]
        .into_iter()
        .map(|t| {
            let mut nb = scaled(&b, 0.25);
            // PS `confusion` `onStart`: `effectState.time = this.random(2, 6)` (2/3/4/5).
            draw(&mut nb, "random", &[2, 6], t as i64, "confusion");
            push(&mut nb, Instruction::SetActiveCounter {
                side,
                which: crate::instruction::ActiveCounter::Confusion,
                previous: prev,
                new: t,
            });
            nb
        })
        .collect()
}

/// Apply a status move's target volatile with full PS counter semantics. May split the branch
/// (confusion durations). `foe_moves_later`: the target still has a pending move this turn —
/// drives the Taunt/Encore +1 and Disable -1 duration adjustments (PS `queue.willMove`).
fn apply_status_target_volatile(mut b: Branch, side: SideId, md: &crate::data::MoveData, foe_moves_later: bool) -> Vec<Branch> {
    use crate::instruction::ActiveCounter;
    let Some(v) = md.target_volatile else { return vec![b] };
    // Self-targeting moves (Focus Energy, Substitute-likes, Aqua Ring) put the volatile on
    // the USER; everything else on the foe.
    let foe = if md.target == crate::data::MoveTarget::User { side } else { side.other() };
    if !b.state.side(foe).active().is_alive() || b.state.side(foe).volatiles.contains(v) {
        // Destiny Bond's own `onPrepareHit` is `return !pokemon.removeVolatile('destinybond')`
        // — an already-active bond is REMOVED and the move FAILS, so it can never be stacked
        // two turns running. r3 t19: the faster Froslass re-uses Destiny Bond, its bond is gone
        // before Koraidon's Scale Shot KOs it, and Koraidon survives.
        if v == VolatileStatus::DestinyBond
            && b.state.side(foe).active().is_alive()
            && b.state.side(foe).volatiles.contains(v)
        {
            push(&mut b, Instruction::RemoveVolatile { side: foe, volatile: v });
        }
        return vec![b];
    }
    let mold_breaker = matches!(
        b.state.side(side).active().ability,
        crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
    );
    // Aroma Veil (breakable): blocks attract / disable / encore / heal block / taunt /
    // torment inflicted on the holder('s side) by a move.
    if foe != side
        && !mold_breaker
        && b.state.side(foe).active().ability == crate::ids::Ability::AromaVeil
        && matches!(
            v,
            VolatileStatus::Attract | VolatileStatus::Disable | VolatileStatus::Encore
                | VolatileStatus::HealBlock | VolatileStatus::Taunt | VolatileStatus::Torment
        )
    {
        return vec![b];
    }
    // Attract: only lands between opposite genders; Oblivious (breakable) is immune to
    // both Attract and Taunt (PS oblivious onTryAddVolatile).
    if v == VolatileStatus::Attract {
        let a = b.state.side(side).active();
        let t = b.state.side(foe).active();
        let genders_ok = (a.gender == 1 && t.gender == 2) || (a.gender == 2 && t.gender == 1);
        if !genders_ok || (!mold_breaker && t.ability == crate::ids::Ability::Oblivious) {
            return vec![b];
        }
    }
    if v == VolatileStatus::Taunt
        && foe != side
        && !mold_breaker
        && b.state.side(foe).active().ability == crate::ids::Ability::Oblivious
    {
        return vec![b];
    }
    match v {
        VolatileStatus::MagnetRise => {
            // `duration: 5` (`data/moves.ts` magnetrise `condition`). Self-targeting, so `foe`
            // resolves to the user's own side.
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
            let prev = b.state.side(foe).magnet_rise_turns;
            push(&mut b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::MagnetRise, previous: prev, new: 5 });
        }
        VolatileStatus::Taunt => {
            // PS: base 3, +1 only when the target has already moved this turn
            // (activeTurns truthy and not in the queue) — a fresh switch-in stays at 3.
            let dur = if foe_moves_later || b.state.side(foe).active_turns == 0 { 3 } else { 4 };
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
            let prev = b.state.side(foe).taunt_turns;
            push(&mut b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::Taunt, previous: prev, new: dur });
        }
        VolatileStatus::Encore => {
            // Fails unless the target's last move is encorable and still on its set with PP.
            // PS `encore`'s `onStart` (`data/moves.ts:4737`) bails on
            // `move.isZ || move.isMax || move.flags['failencore'] || !moveSlot || moveSlot.pp <= 0`.
            // The `failencore` set below is the COMPLETE gen-9 flag list at the pin, enumerated
            // with `Dex.forGen(9).moves.all().filter(m => m.flags.failencore)` — the engine used
            // to carry six of these eighteen. rb1387 d36 t32 is the witness for the one that
            // matters in randbats: a Lapras whose last move is **Sleep Talk** (`data/moves.ts:617`
            // carries `failencore: 1`) cannot be Encored, because `lastMove` is the CALLER — PS's
            // `actions.useMove` path never overwrites it with the called move. The engine held an
            // Encore PS did not, locked the Lapras out of Freeze-Dry, and swallowed its draw.
            let last = b.state.side(foe).last_used_move;
            let encorable = last != crate::ids::MoveId::None
                && !matches!(
                    last.to_id(),
                    "assist" | "blazingtorque" | "combattorque" | "copycat" | "dynamaxcannon"
                        | "encore" | "magicaltorque" | "mefirst" | "metronome" | "mimic"
                        | "mirrormove" | "naturepower" | "noxioustorque" | "sketch" | "sleeptalk"
                        | "struggle" | "transform" | "wickedtorque"
                )
                && b.state.side(foe).active().moves.iter().any(|m| m.id == last && m.pp > 0);
            if encorable {
                let dur = if foe_moves_later { 3 } else { 4 };
                push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
                let prev = b.state.side(foe).encore;
                push(&mut b, Instruction::SetEncore { side: foe, previous: prev, new: (last, dur) });
            }
        }
        VolatileStatus::Disable => {
            let last = b.state.side(foe).last_used_move;
            let ok = last != crate::ids::MoveId::None
                && last.to_id() != "struggle"
                && b.state.side(foe).active().moves.iter().any(|m| m.id == last);
            if ok {
                // PS: duration 5, -1 if the target will still move this turn.
                let dur = if foe_moves_later { 4 } else { 5 };
                push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
                let prev = b.state.side(foe).disable;
                push(&mut b, Instruction::SetDisable { side: foe, previous: prev, new: (last, dur) });
            }
        }
        VolatileStatus::Yawn => {
            // The drowse is a VOLATILE, so it is refused by `onTryAddVolatile`, not by
            // `onSetStatus` — a different (and shorter) list than "can this mon be put to sleep".
            // Enumerated from the pin (`…filter(x => x.onTryAddVolatile)`), the handlers that
            // return null for `yawn` are: insomnia, vitalspirit, purifyingsalt, shieldsdown
            // (Meteor Minior), leafguard in sun, safeguard, and **electricterrain for a GROUNDED
            // target**. `status_applies` already carries the ability half; the terrain half was
            // missing, and it is NOT interchangeable with `status_blocked_by_field` — Misty
            // Terrain blocks `confusion`, never `yawn`, so a Yawn under Misty still lands its
            // volatile and only fails later at `onSetStatus`. (Safeguard is a side condition the
            // engine does not model at all; noted, not fixed here.)
            //
            // rb1778 d36 t32: Pincurchin's Electric Surge terrain is still up when it pivots out
            // to Copperajah and Meowstic's Prankster Yawn resolves. PS refuses the volatile; the
            // engine drowsed a mon that cannot sleep.
            let leaf_guard_sun = b.state.side(foe).active().ability == crate::ids::Ability::LeafGuard
                && matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun);
            let electric_ground = b.state.terrain == crate::ids::Terrain::Electric
                && is_grounded(&b.state, foe);
            let t = b.state.side(foe).active();
            if t.status == Status::None
                && status_applies(t, Status::Sleep)
                && !leaf_guard_sun
                && !electric_ground
            {
                push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
                let prev = b.state.side(foe).yawn_turns;
                push(&mut b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::Yawn, previous: prev, new: 2 });
            }
        }
        VolatileStatus::PerishSong => {
            // Perish Song hits every active (both sides). Counter 4: ticks at each end of
            // turn; the holder faints when it reaches 0 (PS shows perish3 after the first tick).
            for ps_side in [side, foe] {
                if b.state.side(ps_side).active().is_alive()
                    && !b.state.side(ps_side).volatiles.contains(VolatileStatus::PerishSong)
                {
                    push(&mut b, Instruction::ApplyVolatile { side: ps_side, volatile: VolatileStatus::PerishSong });
                    let prev = b.state.side(ps_side).perish_turns;
                    push(&mut b, Instruction::SetActiveCounter { side: ps_side, which: ActiveCounter::Perish, previous: prev, new: 4 });
                }
            }
        }
        VolatileStatus::Confusion => {
            // Own Tempo is confusion-immune (PS `onTryAddVolatile`): the move just fails.
            if b.state.side(foe).active().ability == crate::ids::Ability::OwnTempo {
                return vec![b];
            }
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
            return branch_confusion_counter(b, foe);
        }
        VolatileStatus::HealBlock => {
            // PS's heal block `durationCallback` (moves.ts): Psychic Noise → 2, else 5 (Persistent
            // ability → 7, absent from the corpus). The counter drives the end-of-turn duration
            // decrement + expiry; the catch-all below applies only the bit, leaving turns=0 so the
            // volatile never expires (r2 t8: Psychic Noise heal block stuck forever).
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
            let dur = if md.id.to_id() == "psychicnoise" { 2 } else { 5 };
            let prev = b.state.side(foe).heal_block_turns;
            push(&mut b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::HealBlock, previous: prev, new: dur });
        }
        _ => {
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
        }
    }
    vec![b]
}

/// The confusion self-hit: a 40-BP typeless physical attack the mon lands on itself, using
/// its own (boosted) Attack and Defense. Enumerates the 16 damage rolls.
///
/// **`getConfusionDamage` (`sim/battle-actions.ts:1854-1866`) is a STANDALONE formula, not a trip
/// through `getDamage`/`modifyDamage`.** It is four truncated divisions, a 16-bit truncation, and
/// `randomizer` — and then it is done. Every modifier that lives in `modifyDamage` is therefore
/// ABSENT: no STAB, no type effectiveness, no weather, no crit, no items/abilities — and **no burn
/// halving**, because the `pokemon.status === 'brn'` block is `modifyDamage`'s (`:1845`), and
/// `conditions.ts brn` carries only `onModifyAtk() {} // hardcoded in BattleActions#modifyDamage()`.
/// A burned mon's confusion self-hit deals FULL damage. (rb1448 d8: a burned, +1-Atk Roaring Moon,
/// bd 62, roll 14 → PS 53; the engine halved to 26.)
fn confusion_self_hit(b: Branch, side: SideId) -> Vec<Branch> {
    let (level, atk, def, hp) = {
        let s = b.state.side(side);
        let p = s.active();
        let atk = boosted_stat(p.stat(crate::ids::StatIndex::Attack) as i64, s.boost(BoostIndex::Attack));
        let def = boosted_stat(p.stat(crate::ids::StatIndex::Defense) as i64, s.boost(BoostIndex::Defense)).max(1);
        (p.level as i64, atk, def, p.hp)
    };
    let lvl_factor = 2 * level / 5 + 2;
    // `tr(baseDamage, 16)` (`:1863`) — "Damage is 16-bit context in self-hit confusion damage".
    // Unreachable at legal stats (the /50 caps bd in the hundreds), modelled because PS writes it.
    let bd = ((lvl_factor * 40 * atk) / def / 50 + 2) % 65536;
    let mut out = Vec::with_capacity(16);
    for i in 0..16i64 {
        // PS `randomizer`: tr(tr(bd * (100 - random(16))) / 100). Roll `i` maps to factor
        // (100 - i)/100 — the SAME orientation as the main damage path (branch result == roll,
        // higher roll → less damage). The old `85 + i` inverted this: the branch the differ/gate
        // selected for a recorded roll R computed (85+R)/100 instead of (100-R)/100, over-dealing.
        let dmg = bd * (100 - i) / 100;
        let dmg = (dmg.max(1) as i16).min(hp);
        let mut sb = scaled(&b, 1.0 / 16.0);
        // PS's confusion self-hit rolls `random(16)` for its damage (no crit, typeless 40 BP).
        draw(&mut sb, "random", &[16], i, "confusion-damage");
        if dmg > 0 {
            let slot = sb.state.side(side).active_index;
            push(&mut sb, Instruction::Damage { side, slot, amount: dmg });
        }
        out.push(sb);
    }
    out
}

/// Revert a transformed active to its own identity (species/stats/types/ability/moves). PS's
/// `clearVolatile` does this both on switch-out and on faint, so the engine calls it from both.
/// Battle-only, non-permanent formes regress to their base forme when the holder leaves the
/// field or faints (PS clears them with the volatiles): Meloetta-Pirouette -> Meloetta (full
/// stat swap, randbats 31/85/neutral spread assumed) and Morpeko-Hangry -> Morpeko (species
/// id only; the formes share stats and typing). Palafin-Hero / Mimikyu-Busted / Eiscue-Noice
/// are permanent and stay.
fn revert_battle_only_forme(b: &mut Branch, side: SideId) {
    let p = b.state.side(side).active();
    if p.transformed {
        return; // Transform reversion handles its own identity
    }
    let pirouette = crate::ids::Species::from_id("meloettapirouette").unwrap_or(crate::ids::Species::None);
    let hangry = crate::ids::Species::from_id("morpekohangry").unwrap_or(crate::ids::Species::None);
    let meteor = crate::ids::Species::from_id("miniormeteor").unwrap_or(crate::ids::Species::None);
    // Cramorant-Gulping / Cramorant-Gorging revert on switch-out and on faint. Gulp Missile's
    // `onSourceTryPrimaryHit` calls `source.formeChange(forme, effect)` with NO `isPermanent`
    // (`data/abilities.ts` gulpmissile), so `baseSpecies` stays `cramorant` and `clearVolatile`'s
    // `setSpecies(this.baseSpecies)` puts it back. That is the DIFFERENCE from Mimikyu-Busted and
    // Palafin-Hero, whose `formeChange(..., true)` rewrites `baseSpecies` and therefore sticks.
    // All three Cramorant formes share stats and typing, so no restat.
    let gulping = crate::ids::Species::from_id("cramorantgulping").unwrap_or(crate::ids::Species::None);
    let gorging = crate::ids::Species::from_id("cramorantgorging").unwrap_or(crate::ids::Species::None);
    let (base, restat) = if p.species == gulping || p.species == gorging {
        (crate::ids::Species::from_id("cramorant").unwrap_or(crate::ids::Species::None), false)
    } else if p.species == pirouette {
        (crate::ids::Species::from_id("meloetta").unwrap_or(crate::ids::Species::None), true)
    } else if p.species == hangry {
        (crate::ids::Species::from_id("morpeko").unwrap_or(crate::ids::Species::None), false)
    } else if p.species == meteor && p.ability == crate::ids::Ability::ShieldsDown {
        // PS `clearVolatile` ends every switch-out with `setSpecies(this.baseSpecies)`, and a
        // Minior's `baseSpecies` is the SET's coloured core — so a Meteor that leaves the field
        // goes back to its core on the BENCH, whatever its HP was (rb1328: a Minior that shelled up
        // at full HP is `minior` again in every later `stateAfter`). Shields Down re-picks the
        // forme from HP the moment it re-enters.
        (p.base_species, true)
    } else {
        return;
    };
    if base == crate::ids::Species::None {
        return;
    }
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = base;
    if restat {
        let level = p.level;
        let bs = crate::data::base_stats(base);
        let mut stats = p.stats;
        for (si, stat) in [
            crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
            crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
            crate::ids::StatIndex::Speed,
        ].into_iter().enumerate() {
            stats[si + 1] = crate::damage::compute_stat(
                bs[si + 1], 31, 85, level, crate::ids::Nature::Serious, stat,
            );
        }
        new.stats = stats;
        // `setSpecies` -> `setType(species.types, /*enforce*/ true)`: PS's `types` array moves
        // even under Tera; only the EFFECTIVE typing keeps the Tera type.
        new.live_types = crate::data::species_types(base);
        if !p.terastallized {
            new.types = new.live_types;
        }
    }
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
}

fn revert_transform(b: &mut Branch, side: SideId) {
    if !b.state.side(side).active().transformed {
        return;
    }
    let prev_data = transform_data_of(&b.state, side);
    let p = b.state.side(side).active();
    let new = crate::instruction::TransformData {
        species: p.base_species,
        stats: { let mut st = p.base_stats; st[0] = p.stats[0]; st },
        // `clearVolatile`'s `setSpecies(baseSpecies)` calls `setType(species.types, true)`, so
        // PS's `types` array reverts to the SPECIES typing even for a terastallized reverter.
        types: if p.terastallized { p.types } else { p.base_types },
        live_types: p.base_types,
        ability: p.base_ability,
        moves: p.base_moves,
        transformed: false,
        times_hit: p.times_hit,
    };
    let slot = b.state.side(side).active_index;
    let previous_base_moves = p.base_moves;
    push(b, Instruction::Transform { side, slot, previous: prev_data, new, previous_base_moves });
}

/// Struggle recoils the user 1/4 of its max HP after it connects.
fn apply_struggle_recoil(mut out: Vec<Branch>, side: SideId, struggling: bool) -> Vec<Branch> {
    if struggling {
        for b in &mut out {
            let p = b.state.side(side).active();
            if p.is_alive() {
                // Struggle recoil is round(maxhp/4) (PS rounds half up), not a floor.
                let rec = (round_div(p.max_hp as i32, 4) as i16).max(1).min(p.hp);
                let slot = b.state.side(side).active_index;
                push(b, Instruction::Damage { side, slot, amount: rec });
                // A transformed mon that faints to its own recoil reverts (PS clearVolatile).
                if !b.state.side(side).active().is_alive() {
                    revert_transform(b, side);
                    revert_battle_only_forme(b, side);
                }
            }
        }
    }
    out
}

/// After a recharge move (Hyper Beam, …) resolves, mark the still-alive user as needing to
/// recharge next turn. Applied to every outcome branch (hit, miss, or no-target alike).
fn apply_recharge(mut out: Vec<Branch>, side: SideId, move_id: crate::ids::MoveId) -> Vec<Branch> {
    if is_recharge_move(move_id) {
        for b in &mut out {
            if b.state.side(side).active().is_alive()
                && b.state.side(side).pending_move != crate::state::PendingMove::Recharging
            {
                let prev = b.state.side(side).pending_move;
                push(b, Instruction::SetPendingMove { side, previous: prev, new: crate::state::PendingMove::Recharging });
                // Keep the `mustrecharge` VOLATILE bit in lockstep with `pending_move`:
                // `convert.rs` sets both when it reads PS's `mustrecharge` volatile
                // (crates/cosim/src/convert.rs:536-539), and the digest hashes the raw bitset,
                // so an engine state that only moved `pending_move` mismatches every PS state
                // captured while the recharge is pending (rb1092 t14, rb1157 t15 — Giga Impact:
                // PS's volatiles carry bit 21, the engine's do not).
                if !b.state.side(side).volatiles.contains(VolatileStatus::MustRecharge) {
                    push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::MustRecharge });
                }
            }
        }
    }
    out
}

/// Execute one move from `action.side`, returning the resulting branches.
fn execute_move_inner(b: Branch, action: Action) -> Vec<Branch> {
    let Action { side, move_idx, pivot, foe_pending_move, external_move, called, .. } = action;
    let external = external_move.is_some();
    let attacker = b.state.side(side).active();
    if !attacker.is_alive() {
        return vec![b];
    }
    // A Dancer-invoked copy carries its move directly (it need not be on the user's set).
    let mut move_id = external_move.unwrap_or(attacker.moves[move_idx as usize].id);
    // Struggle: a mon forced to act with no usable moves uses Struggle instead — a typeless
    // 50-BP physical hit that connects on everything and recoils 1/4 of the user's max HP.
    // The chosen slot being out of PP is the common case; `no_usable_move` is PS's actual rule
    // (`getMoves` returns `[]` when EVERY slot is disabled, which the request turns into
    // Struggle) and covers the disabled-but-not-empty mons the pp test misses.
    let struggling = !external && (attacker.moves[move_idx as usize].pp == 0 || action.struggling);
    if struggling {
        // The move USED is Struggle, not whatever slot the choice nominally pointed at — PS
        // sets `lastMove` to the struggle Move object. It matters: rb1024 d82's Ursaluna
        // Struggles on t72 while Encored into Blood Moon, and if the engine records Blood Moon
        // as the last move then `cantusetwice` locks it out on t73 and the mon Struggles a
        // SECOND time, where PS goes back to using Blood Moon.
        move_id = crate::ids::MoveId::from_id("struggle").unwrap_or(crate::ids::MoveId::None);
    }
    // **`runEvent('OverrideAction')` is the FIRST thing `runMove` does** (`battle-actions.ts:228`),
    // before `getActiveMove` builds the object every `onModifyType` / `onModifyMove` then edits.
    // Encore's handler (`data/moves.ts:4754`) returns the encored move id whenever the chosen move
    // is not it, so an Encore that lands EARLIER IN THE SAME TURN redirects an action that was
    // already chosen. PS's exclusions are exactly `struggle`, Z/Max, and an external (Dancer) move;
    // the multi-turn guard is the engine's own and is kept.
    //
    // This used to sit ~130 lines DOWN, after the whole move-modifier chain, and re-assigned `md =
    // move_data(enc.0)` — throwing away every modifier and leaving `move_id` pointing at the move
    // the player picked. rb1734 d30 t24: a Prankster Encore locks an Arceus-Electric that chose
    // Recover into Judgment; the engine substituted RAW Judgment, i.e. **Normal**-type, because the
    // Zap Plate's `onModifyType` had been applied to Recover's `MoveData` and then discarded — and
    // Normal is IMMUNE to the Ghost-type Sableye in front of it. So the move failed at moveStep 3
    // with no accuracy roll (`PS-unconsumed randomChance[100, 100]@judgment`) where PS KO'd the
    // Sableye. The redirect itself was never the bug; its POSITION was.
    let mut move_idx = move_idx;
    let mut move_id = move_id;
    if !struggling && !external {
        let enc = b.state.side(side).encore;
        if enc.0 != crate::ids::MoveId::None
            && b.state.side(side).pending_move == crate::state::PendingMove::None
        {
            if let Some(enc_slot) =
                b.state.side(side).active().moves.iter().position(|m| m.id == enc.0 && m.pp > 0)
            {
                if enc_slot as u8 != move_idx {
                    move_idx = enc_slot as u8;
                    move_id = enc.0;
                }
            }
        }
    }
    let move_idx = move_idx;
    let move_id = move_id;
    let mut md = if struggling {
        let mut m = crate::data::MoveData::none();
        m.typ = Type::None;
        m.category = MoveCategory::Physical;
        m.base_power = 50;
        m.accuracy = 0;
        m
    } else {
        move_data(move_id)
    };
    // **`Pivot::Pause` belongs to the move that RUNS, not the move that was CHOSEN.**
    // `Flow::run_turn` stamps it on the action from the chosen slot — a `self_switch` move with an
    // alive bench, or Revival Blessing with a fainted bench — but the two substitutions above
    // (Struggle via `no_usable_move`, and the Encore `OverrideAction` redirect) replace the move
    // afterwards, and every `match pivot` arm below keys on the ACTION. So the substitute inherited
    // the pause: a PP-stalled mon with an entirely FAINTED bench picks Revival Blessing, Struggles,
    // and its damaging path pushes `PivotPending` — a `PivotLanding` request for a side with
    // nowhere to go (`request.rs:resume_pivot`'s tripwire; it killed a 4096-env trainer at ~1e-5
    // games). Re-derive the pause from the executed move; `Pivot::Target` is left alone because the
    // verification paths supply it from the RECORDED choice, which is already the executed move.
    let pivot = match pivot {
        Pivot::Pause
            if struggling
                || !(md.self_switch
                    || matches!(md.id.to_id(), "revivalblessing" | "shedtail")) =>
        {
            Pivot::Stay
        }
        p => p,
    };
    // Shell Side Arm: category resolved by `dispatch_move_inner`; the physical variant
    // additionally becomes a contact move (PS sets `move.flags.contact = 1`).
    if md.id.to_id() == "shellsidearm" {
        match action.shell_phys {
            Some(true) => {
                md.category = MoveCategory::Physical;
                md.flag_contact = true;
            }
            Some(false) | None => {}
        }
    }
    // Aura Wheel: only Morpeko (either forme) may use it; its type follows the forme
    // (PS `onModifyType`: Dark for Morpeko-Hangry, Electric otherwise).
    if md.id.to_id() == "aurawheel" {
        let sp = attacker.species;
        if sp == crate::ids::Species::from_id("morpekohangry").unwrap_or(crate::ids::Species::None) {
            md.typ = Type::Dark;
        } else if sp != crate::ids::Species::from_id("morpeko").unwrap_or(crate::ids::Species::None) {
            // PS `onTry` fails the move outright for non-Morpeko users.
            return vec![b];
        }
    }
    // Focus Punch: `beforeMoveCallback` (data/moves.ts:6015-6020) aborts the move when the
    // `focuspunch` volatile's `lostFocus` is set — its `onHit` sets that for any non-Status move
    // that hits the user this turn (data/moves.ts:6026-6030). PS runs `beforeMoveCallback` in
    // `runMove` (sim/battle-actions.ts:270-276), i.e. AFTER the BeforeMove cancel handlers and
    // BEFORE `deductPP` (:281) and `useMove` — so a de-focused Focus Punch pays no PP, deals no
    // damage, and makes NO accuracy/crit/damage draws. The engine rolled a phantom
    // `randomChance(100,100)@accuracy` for it (rb1397 t27: Mach Punch into a queued Focus Punch,
    // PS records only Mach Punch's three draws). `physical_damage_taken`/`special_damage_taken`
    // are the side's this-turn damage record (reset at the top of the residual phase) — exactly
    // "was hit by a damaging move this turn".
    if md.id.to_id() == "focuspunch" {
        let s = b.state.side(side);
        if s.physical_damage_taken > 0 || s.special_damage_taken > 0 {
            let mut b = b;
            b.move_failed = true; // PS sets `moveThisTurnResult = false`
            return vec![b];
        }
    }
    // Judgment: takes the type of the user's held Plate (PS `onModifyType` via
    // `item.onPlate`; randbats Arceus formes always hold their matching Plate).
    if md.id.to_id() == "judgment" {
        if let Some(t) = plate_type(attacker.item) {
            md.typ = t;
        }
    }
    // Tera Starstorm: `onModifyType` (`data/moves.ts:19259`) makes it **Stellar** — and flips it
    // physical on the same Atk-vs-SpA test Tera Blast uses — as soon as the user is
    // Terapagos-Stellar, which is the forme Tera Shift's Terastallization produces. Without this
    // the move stayed NORMAL, i.e. immune into any Ghost and resisted by Rock/Steel, and it took
    // ordinary Normal STAB instead of the Stellar rule in `damage::stab_mod`.
    if md.id.to_id() == "terastarstorm"
        && Some(attacker.species) == crate::ids::Species::from_id("terapagosstellar")
    {
        md.typ = Type::Stellar;
        if attacker.terastallized
            && attacker.stat(crate::ids::StatIndex::Attack) > attacker.stat(crate::ids::StatIndex::SpecialAttack)
        {
            md.category = MoveCategory::Physical;
        }
    }
    // Tera Blast: when the user is Terastallized it becomes the tera type and uses whichever
    // of Atk/SpA is higher (so the category can flip to physical).
    if md.id.to_id() == "terablast" && attacker.terastallized {
        md.typ = attacker.tera_type;
        if attacker.stat(crate::ids::StatIndex::Attack) > attacker.stat(crate::ids::StatIndex::SpecialAttack) {
            md.category = MoveCategory::Physical;
        }
    }
    // Payback doubles only after the target has already acted. A queued opposing move means
    // it has not; a freshly switched target (`active_turns == 0`) is also explicitly exempt.
    if md.id.to_id() == "payback"
        && foe_pending_move.is_none()
        && b.state.side(side.other()).active_turns > 0
    {
        md.base_power = md.base_power.saturating_mul(2);
    }
    // -ate abilities (Pixilate/Refrigerate/Aerilate/Galvanize): a Normal-type move becomes the
    // ability's type and gains ×1.2 base power (PS onModifyType + onBasePower). Excludes moves
    // whose type is determined dynamically, and Tera Blast while terastallized.
    if md.typ == Type::Normal
        && !matches!(md.id.to_id(),
            "judgment" | "multiattack" | "naturalgift" | "revelationdance"
            | "technoblast" | "terrainpulse" | "weatherball")
        && !(md.id.to_id() == "terablast" && attacker.terastallized)
    {
        let ate_type = match attacker.ability {
            crate::ids::Ability::Pixilate => Some(Type::Fairy),
            crate::ids::Ability::Refrigerate => Some(Type::Ice),
            crate::ids::Ability::Aerilate => Some(Type::Flying),
            crate::ids::Ability::Galvanize => Some(Type::Electric),
            _ => None,
        };
        if let Some(t) = ate_type {
            md.typ = t;
            // PS sets `move.typeChangerBoosted = this.effect` in `onModifyType`; the ×1.2 is a
            // SEPARATE `onBasePower` handler (priority 23) that chains with every other
            // onBasePower modifier instead of rounding on its own. Stamp the flag and let
            // `compute_damage` fold it into `bp_chain`.
            md.type_changer_boosted = true;
        }
    }
    // Liquid Voice: sound moves become Water-type (PS onModifyType, no power change).
    if attacker.ability == crate::ids::Ability::LiquidVoice && md.flag_sound {
        md.typ = Type::Water;
    }
    // Revelation Dance takes the user's primary type (PS onModifyType; a terastallized
    // user's current first type is its tera type, matching PS).
    if md.id.to_id() == "revelationdance" && attacker.types[0] != Type::None {
        md.typ = attacker.types[0];
    }
    // Raging Bull's type follows the Paldean Tauros forme (PS onModifyType by species).
    if md.id.to_id() == "ragingbull" {
        let sid = attacker.species.to_id();
        if sid == "taurospaldeacombat" {
            md.typ = Type::Fighting;
        } else if sid == "taurospaldeablaze" {
            md.typ = Type::Fire;
        } else if sid == "taurospaldeaaqua" {
            md.typ = Type::Water;
        }
    }
    // Ivy Cudgel's type follows the Ogerpon mask (PS `onModifyType` switching on
    // `pokemon.species.name`, listing BOTH the plain and the `-Tera` forme of each mask; plain
    // Ogerpon / Ogerpon-Teal-Tera stay Grass). rb1230 t7: a tera-Rock Ogerpon-Cornerstone-Tera
    // Ivy Cudgel into Abomasnow — 164 in PS, 30 in the engine, which is exactly the Grass-vs-
    // Grass/Ice x0.25 and the 1.5 STAB instead of Rock's x1 and the double-Tera x2.
    if md.id.to_id() == "ivycudgel" {
        let sid = attacker.species.to_id();
        if sid.starts_with("ogerponwellspring") {
            md.typ = Type::Water;
        } else if sid.starts_with("ogerponhearthflame") {
            md.typ = Type::Fire;
        } else if sid.starts_with("ogerponcornerstone") {
            md.typ = Type::Rock;
        }
    }
    // Weather Ball takes the type of the active weather (PS onModifyType); its base-power
    // doubling under any weather is applied in compute_damage's base_power match.
    if md.id.to_id() == "weatherball" {
        md.typ = match effective_weather(&b.state) {
            Weather::Sun | Weather::HarshSun => Type::Fire,
            Weather::Rain | Weather::HeavyRain => Type::Water,
            Weather::Sand => Type::Rock,
            Weather::Snow => Type::Ice,
            _ => Type::Normal,
        };
    }
    // Analytic: ×1.3 base power when no other active will still move this turn (PS
    // onBasePower chainModify([5325, 4096]); queue.willMove is falsy for a foe that already
    // moved, chose a switch, or is fainted — exactly foe_pending_move == None here).
    // The condition is evaluated HERE (it needs the action queue) but the ×1.3 is applied inside
    // `compute_damage`'s single onBasePower chain — which also makes it apply to the
    // dynamic-BP moves whose base power is recomputed there.
    if attacker.ability == crate::ids::Ability::Analytic
        && foe_pending_move.is_none()
        && md.category != MoveCategory::Status
    {
        md.analytic_boosted = true;
    }
    let foe = side.other();

    let mut b = b;
    let mut move_idx = move_idx;
    let slot = b.state.side(side).active_index;

    // A rampaging mon (Outrage / Thrash / ...) is locked into its move and pays no PP on
    // continuation turns.
    let rampaging_now = matches!(b.state.side(side).pending_move, crate::state::PendingMove::Rampaging(..));
    if let crate::state::PendingMove::Rampaging(m, _) = b.state.side(side).pending_move {
        if !struggling && !external {
            if let Some(slot_i) = b.state.side(side).active().moves.iter().position(|ms| ms.id == m) {
                if slot_i as u8 != move_idx {
                    move_idx = slot_i as u8;
                    md = move_data(m);
                }
            }
        }
    }
    let move_id = if struggling || external { move_id } else { b.state.side(side).active().moves[move_idx as usize].id };

    // Destiny Bond lasts until the user's next move: moving again drops it. PS's
    // `onBeforeMove` explicitly EXEMPTS Destiny Bond itself (`if (move.id === 'destinybond')
    // return;`) — re-using it is handled by the move's own `onPrepareHit`, in
    // `apply_status_target_volatile`.
    if md.target_volatile != Some(VolatileStatus::DestinyBond)
        && b.state.side(side).volatiles.contains(VolatileStatus::DestinyBond)
    {
        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::DestinyBond });
    }

    // Protean / Libero: the user becomes the move's type before it resolves (gen9: once
    // per switch-in; never while terastallized; Struggle excluded).
    {
        let p = b.state.side(side).active();
        if matches!(p.ability, crate::ids::Ability::Protean | crate::ids::Ability::Libero)
            && !struggling
            && !p.terastallized
            && !b.state.side(side).volatiles.contains(VolatileStatus::TypeShifted)
            && md.typ != Type::None
            && p.types != [md.typ, Type::None]
        {
            let slot = b.state.side(side).active_index;
            let prev = p.types;
            let prev_live = p.live_types;
            // A real `setType` — PS's `types` array moves with the effective typing (the
            // guard above already excludes a terastallized user, whom `setType` refuses).
            push(&mut b, Instruction::ChangeTypes { side, slot, previous: prev, new: [md.typ, Type::None] });
            if prev_live != [md.typ, Type::None] {
                push(&mut b, Instruction::ChangeLiveTypes { side, slot, previous: prev_live, new: [md.typ, Type::None] });
            }
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::TypeShifted });
        }
    }

    // Tera Blast: when the user is terastallized it becomes the tera type and uses the
    // higher of the user's (boosted) Atk / SpA.
    if md.id.to_id() == "terablast" && b.state.side(side).active().terastallized {
        let p = b.state.side(side).active();
        if p.tera_type != Type::None {
            md.typ = p.tera_type;
        }
        let atk = boosted_stat(p.stat(crate::ids::StatIndex::Attack) as i64, b.state.side(side).boost(BoostIndex::Attack));
        let spa = boosted_stat(p.stat(crate::ids::StatIndex::SpecialAttack) as i64, b.state.side(side).boost(BoostIndex::SpecialAttack));
        md.category = if atk > spa { MoveCategory::Physical } else { MoveCategory::Special };
    }

    // Photon Geyser (and Ultra Necrozma's Light That Burns the Sky) is declared Special but its
    // `onModifyMove` flips it to Physical when the user's Attack beats its Special Attack:
    // `if (pokemon.getStat('atk', false, true) > pokemon.getStat('spa', false, true))
    // move.category = 'Physical'` (`data/moves.ts:13351-13353`). `getStat(stat, false, true)` is
    // BOOSTED but UNMODIFIED — boosts count, ability/item modifiers do not — the same comparison
    // Tera Blast makes above. Strictly `>`, so a tie stays Special.
    //
    // rb1280 d13 is the witness: a Necrozma-Dusk-Mane (base Atk 157 vs SpA 113) Photon Geysers a
    // switching-in Arboliva, whose Defense is far below its Special Defense. PS deals 102, the
    // engine ran the special side and dealt 67.
    if matches!(md.id.to_id(), "photongeyser" | "lightthatburnsthesky") {
        let p = b.state.side(side).active();
        let atk = boosted_stat(p.stat(crate::ids::StatIndex::Attack) as i64, b.state.side(side).boost(BoostIndex::Attack));
        let spa = boosted_stat(p.stat(crate::ids::StatIndex::SpecialAttack) as i64, b.state.side(side).boost(BoostIndex::SpecialAttack));
        if atk > spa {
            md.category = MoveCategory::Physical;
        }
    }

    // Charge (including the volatile granted by Electromorphosis) doubles the next Electric
    // attack and is consumed by any non-Charge Electric move. PS also consumes it when that
    // move subsequently misses or is aborted, so remove it before the move's early exits.
    if md.typ == Type::Electric
        && md.id.to_id() != "charge"
        && b.state.side(side).volatiles.contains(VolatileStatus::Charge)
    {
        if md.category != MoveCategory::Status {
            md.base_power = md.base_power.saturating_mul(2);
        }
        push(&mut b, Instruction::RemoveVolatile {
            side,
            volatile: VolatileStatus::Charge,
        });
    }

    // Damp shuts down self-destructing moves entirely (either active).
    if md.self_destruct {
        let damp = b.state.side(side).active().ability == crate::ids::Ability::Damp
            || b.state.side(foe).active().ability == crate::ids::Ability::Damp;
        if damp {
            return vec![b];
        }
    }

    // Sleep: can't move while the counter is > 1; on the expiry turn the mon wakes
    // (status cleared) and then moves normally. (gen9 sleep is a fixed countdown.)
    let (status, counter) = { let p = b.state.side(side).active(); (p.status, p.status_counter) };
    if status == Status::Sleep && !called {
        // Early Bird burns sleep turns twice as fast (PS slp onBeforeMove: an extra time--).
        let tick = if b.state.side(side).active().ability == crate::ids::Ability::EarlyBird { 2 } else { 1 };
        if counter > tick {
            push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: counter - tick });
            // PS slp `onBeforeMove` (priority 10) ticks the counter and then returns `false` —
            // EXCEPT for a `sleepUsable` move (Sleep Talk, Snore), where it returns undefined and
            // the mon acts while still asleep. `false` short-circuits `runEvent`, so the lower
            // handlers (Truant 9, flinch 8, confusion 3, paralysis 1) never run in the cancel case;
            // in the sleepUsable case Truant is next in line and still gets its toggle.
            if !md.sleep_usable {
                return vec![b];
            }
            if truant_gate(&mut b, side) {
                return vec![b];
            }
        } else {
            push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
            clear_status_counter(&mut b, side, slot);
            // The wake attempt reaches Truant's BeforeMove handler (priority 9, right after slp's
            // 10): the toggle fires now, and a due loaf consumes the freshly-woken turn.
            if truant_gate(&mut b, side) {
                return vec![b];
            }
        }
    }
    // Freeze: a frozen mon can't move (the 20% thaw + act is left unmodeled for now).
    if b.state.side(side).active().status == Status::Freeze && !called {
        return vec![b];
    }

    // Disable (`onBeforeMovePriority: 7`), Throat Chop / Heal Block / Gravity (6) and Taunt (5)
    // — all `data/moves.ts` — sit BELOW mustrecharge (11), slp / frz (10), Truant (9) and flinch
    // (8) in PS's `BeforeMove` ladder, and `runEvent` short-circuits on the first handler that
    // returns `false`. So a sleeping, frozen, loafing or flinched mon never reaches these cancels,
    // and the higher-priority handler's SIDE EFFECTS still land — above all slp's `time--`
    // (`data/conditions.ts` slp `onBeforeMove`). The engine ran this block first, so a mon that
    // was asleep AND taunted had its move cancelled by Taunt with the sleep counter untouched:
    // rb1009 d4 (a Dondozo asleep on 3 selects Rest, Froslass Taunts it first — PS ticks 3 -> 2)
    // and rb1356 d58 (the same shape with Coil).
    // `execute_move` already ran this ladder for a real move action, ahead of confusion /
    // attract / paralysis. It stays here for the CALLED entries that reach
    // `dispatch_move_inner` directly (Sleep Talk, Dancer, a bounced move), which never pass
    // through `execute_move`. One predicate, `before_move_blocked_7_6_5`, for both.
    if !struggling && before_move_blocked_7_6_5(&b.state, side, &md) {
        return vec![b];
    }

    // --- multi-turn move commitment (charge / semi-invulnerable / recharge) ---
    use crate::state::PendingMove;
    let pending = b.state.side(side).pending_move;
    // Recharge: the mon spent a recharge move last turn and forfeits this one.
    if matches!(pending, PendingMove::Recharging) && !called {
        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
        clear_recharge_volatiles(&mut b, side);
        return vec![b];
    }
    // Are we cashing in a two-turn move that finished charging last turn?
    let executing_charge = matches!(pending, PendingMove::Charging(m) if m == move_id);

    // PP is paid on the charge turn, not the strike turn. Pressure on the opposing active
    // costs one extra PP for any move that targets it (PS onDeductPP; cosim caught this).
    // Struggle is not a move slot (PS `dex.moves.get('struggle')` with no `moveSlot`), so it
    // deducts nothing — the guard used to be implicit because `struggling` meant "the chosen
    // slot is at 0 PP", which is no longer the only way in (`no_usable_move`).
    if !executing_charge && !rampaging_now && !external && !struggling {
        let pp = b.state.side(side).active().moves[move_idx as usize].pp;
        if pp > 0 {
            let foe_active = b.state.side(side.other()).active();
            let user_is_ghost = b.state.side(side).active().types.contains(&Type::Ghost);
            let pressured = foe_active.is_alive()
                && foe_active.ability == crate::ids::Ability::Pressure
                && pressure_affected(&md, user_is_ghost);
            let amount = if pressured { 2u8.min(pp) } else { 1 };
            push(&mut b, Instruction::DecrementPp { side, slot, move_index: move_idx, amount });
            maybe_eat_leppa(&mut b, side);
        }
    }

    // Record the move use for consecutive-use mechanics (streak / Protect stall chain). The
    // mon has passed sleep/freeze, so it is actually acting this turn. A Dancer copy is
    // `isExternal` in PS: no lastMove/streak bookkeeping and no Choice lock.
    if !external {
        record_move_use(&mut b, side, move_id);
    }

    // ── `runEvent('TryMove')` (`sim/battle-actions.ts:485-492`) ──────────────────────────────
    // The FIRST thing `useMoveInner` runs after the Pressure PP deduction, and it is one event
    // for status and damaging moves alike. Queenly Majesty / Dazzling / Armor Tail register
    // `onFoeTryMove` here; the whole hit-step chain (invulnerability, TryHit — which carries
    // Protect at priority 3, Psychic Terrain at 4 and the absorbing abilities at 1/0 — type
    // immunity, the Prankster-vs-Dark check, accuracy) comes after.
    //
    // These two blocks used to sit BELOW the `md.category == Status` dispatch, so they only ever
    // guarded damaging moves and a Prankster-boosted status move sailed straight through. rb1061
    // d34 is the witness: Klefki's Prankster Thunder Wave (+1 priority) into a Queenly Majesty
    // Tsareena — PS's whole unit is Tsareena's own move (the block makes no draw at all), while
    // the engine rolled Thunder Wave's `randomChance(90,100)` and paralysed her.
    //
    // Queenly Majesty (breakable): the foe's increased-priority moves fail against the holder's
    // side (`data/abilities.ts:3671`, `move.priority > 0.1` — the EFFECTIVE priority, so
    // Prankster / Gale Wings / Grassy Glide boosts count; foeSide-targeting moves exempt).
    {
        let holder = b.state.side(foe).active();
        // All THREE of PS's `onFoeTryMove` abilities carry the identical block
        // (`Dex.forGen(9).abilities.all().filter(a => a.onFoeTryMove)` -> armortail, dazzling,
        // queenlymajesty; `data/abilities.ts:215`, `:1290`, `:3671`). The engine listed one.
        if holder.is_alive()
            && matches!(
                holder.ability,
                crate::ids::Ability::QueenlyMajesty
                    | crate::ids::Ability::Dazzling
                    | crate::ids::Ability::ArmorTail
            )
        {
            let atk = b.state.side(side).active();
            let mb = matches!(atk.ability, crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze);
            let pri = modified_priority(&b.state, side, &md);
            let side_targeting = md.side_condition.is_some() && md.target != crate::data::MoveTarget::User;
            // PS's test is `source.isAlly(dazzlingHolder) || move.target === 'all'`
            // (`data/abilities.ts:3679`), and in `onFoeTryMove(target, source, move)` the args are
            // (move USER, move TARGET, move) — `runEvent('TryMove', pokemon, target, move)`. So it
            // reads "the move's resolved TARGET is the ability holder (or its ally)". **A
            // SELF-TARGETING priority move is therefore never blocked**: its target is its own
            // user, which is the holder's foe.
            //
            // The engine blocked on priority alone. rb5051 d35 t31: a Regigigas Protects (+4,
            // target `self`) across from a Queenly Majesty holder; the engine failed the Protect,
            // let a High Jump Kick through for 196, and PS's only draw for the whole turn is the
            // residual protect/stall shuffle. Psychic Terrain — the same predicate, 15 lines
            // below — already carried the `target != User` exemption. Two copies, one right.
            let self_targeting = md.target == crate::data::MoveTarget::User;
            if pri > 0 && !mb && !side_targeting && !self_targeting {
                b.move_failed = true; // blocked → moveThisTurnResult false
                return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
            }
        }
    }
    // Psychic Terrain blocks priority moves aimed at grounded targets (`data/moves.ts:14120`,
    // `onTryHitPriority: 4` — above Protect's 3 and the absorbing abilities' 1/0, and it exempts
    // `move.target === 'self'`). Prankster's boost counts here too: PS compares `effect.priority`,
    // which `getActionSpeed` has already raised.
    if b.state.terrain == crate::ids::Terrain::Psychic
        && md.target != crate::data::MoveTarget::User
        && b.state.side(foe).active().is_alive()
        && is_grounded(&b.state, foe)
    {
        let pri = modified_priority(&b.state, side, &md);
        if pri > 0 {
            b.move_failed = true; // blocked → moveThisTurnResult false
            return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
        }
    }

    // Prankster-boosted status moves fail against Dark-type targets (after PP is paid).
    if md.category == MoveCategory::Status
        && b.state.side(side).active().ability == crate::ids::Ability::Prankster
        && md.target != crate::data::MoveTarget::User
        && b.state.side(side.other()).active().types.contains(&Type::Dark)
        && targets_foe_status(&md)
    {
        return vec![b];
    }

    // A two-turn move spends this turn charging (no attack) unless it strikes instantly.
    // Power Herb is consumed to skip the charge turn entirely.
    if !executing_charge && is_two_turn_move(move_id) && !charges_instantly(move_id, effective_weather(&b.state)) {
        // Meteor Beam / Electro Shot raise the user's SpA on the charge turn (PS `onTryMove`
        // `this.boost`), before committing to (or, with Power Herb, skipping) the charge.
        if let Some((stat, amt)) = charge_self_boost(move_id) {
            apply_self_boost(&mut b, side, stat, amt);
        }
        if b.state.side(side).active().item == Item::PowerHerb {
            push(&mut b, Instruction::ChangeItem { side, slot, previous: Item::PowerHerb, new: Item::None });
            on_item_lost(&mut b, side);
        } else {
            push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::Charging(move_id) });
            return vec![b];
        }
    }
    if executing_charge {
        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
    }

    // Fake Out / First Impression only work on the user's first turn out (active_turns ≤ 1);
    // after that they fail outright.
    if matches!(move_id.to_id(), "fakeout" | "firstimpression") && b.state.side(side).active_turns > 1 {
        return vec![b];
    }

    // Double Shock fails outright (PS `onTryMove` `-fail`) unless the user is currently an
    // Electric type — so a second use after the first stripped the Electric type fails.
    if move_id.to_id() == "doubleshock" && !b.state.side(side).active().types.contains(&Type::Electric) {
        return vec![b];
    }

    // Sucker Punch / Thunderclap fail unless the target is about to use a damaging move this
    // turn (it must not have moved already); Upper Hand additionally needs a priority move.
    if matches!(move_id.to_id(), "suckerpunch" | "thunderclap" | "upperhand") {
        let ok = action.foe_pending_move.map_or(false, |m| {
            let fmd = move_data(m);
            fmd.category != MoveCategory::Status
                && (md.id.to_id() != "upperhand" || fmd.priority > 0)
        });
        if !ok {
            return vec![b];
        }
    }

    // Self-destructing "always" moves (Explosion / Self-Destruct / Misty Explosion) faint the
    // user BEFORE the hit is attempted: `useMoveInner` calls `battle.faint(pokemon, pokemon, move)`
    // at `sim/battle-actions.ts:501`, and `trySpreadMoveHit` — which owns EVERY hit step, including
    // `hitStepTryHitEvent` and the absorbing abilities below — is not reached until :519. So the
    // user faints against a type-immune target, against an ABSORBING ability, through Protect, and
    // on a miss alike. (Damp already cancelled the move above, so no faint there.) The hit-branch
    // self-destruct in `apply_post_damage` is a no-op once the user is already down.
    //
    // This block used to sit BELOW the absorb section, whose early `return vec![b]` skipped it.
    // rb1774 d10 t7: Golem-Alola (Galvanize) Explodes into a Volt Absorb Minun. PS's whole unit is
    // one `randomChance[100,100]@encore` — no accuracy roll, no damage — and Golem is at 0 HP with
    // a replacement queued. The engine's Golem walked away untouched.
    if matches!(move_id.to_id(), "explosion" | "selfdestruct" | "mistyexplosion") {
        let (alive, hp, aslot) = {
            let p = b.state.side(side).active();
            (p.is_alive(), p.hp, b.state.side(side).active_index)
        };
        if alive {
            push(&mut b, Instruction::Damage { side, slot: aslot, amount: hp });
        }
    }

    // Absorbing abilities (Volt Absorb / Water Absorb / Dry Skin / Earth Eater) nullify a move
    // of their type that targets the holder AND heal it 1/4 max HP (PS onTryHit). This fires for
    // damaging AND status moves alike — e.g. Thunder Wave vs Volt Absorb heals and prevents the
    // paralysis. Mold Breaker bypasses. Side/field moves (hazards) don't target the mon.
    {
        use crate::ids::Ability as A;
        // `target_volatile` is the codegen's fold of PS `move.volatileStatus`, which is set on
        // SELF-targeting moves too (Protect, Substitute, Magnet Rise, Aqua Ring, …). PS keys
        // every one of these blocks on `move.target` / `target === source`, so a self-targeting
        // move is never aimed at the foe's mon — gate on `MoveTarget::targets_foe()`.
        let foe_status_target = md.target.targets_foe()
            && (md.status != Status::None
                || md.target_boosts.iter().any(|&x| x != 0)
                || md.target_volatile.is_some()
                || md.force_switch
                // Strength Sap's foe-facing effect is `onHit`-only (invisible to the codegen),
                // but it targets the mon — Sap Sipper absorbs it (cosim caught the miss).
                || md.id.to_id() == "strengthsap");
        // Protect outranks every one of these abilities. Protect's condition carries
        // `onTryHitPriority: 3` (`data/moves.ts:13989`) while Sap Sipper / Lightning Rod /
        // Storm Drain / Motor Drive carry `onTryHitPriority: 1` and Volt Absorb / Water Absorb /
        // Dry Skin / Earth Eater / Flash Fire the default 0 — and all of them are handlers of the
        // SAME `runEvent('TryHit')` inside `hitStepTryHitEvent`, which short-circuits on the
        // first `false`/`null`. So a protected target never reaches its own absorbing ability: no
        // heal, no redirect boost, no Flash Fire activation. Same protect-bypass carve-outs as the
        // damaging-move check further down (`checkMoveBypassesProtect` needs the `protect` flag;
        // Mighty Cleave has none; Unseen Fist ignores protection on contact moves).
        // rb1299 d35: Farigiraf Protects, Toucannon's Bullet Seed is Grass, and the engine still
        // handed the protector Sap Sipper's +1 Attack.
        let protect_blocks = md.flag_protect
            && b.state.side(foe).volatiles.contains(VolatileStatus::Protect)
            && md.id.to_id() != "mightycleave"
            && !(md.flag_contact
                && b.state.side(side).active().ability == crate::ids::Ability::UnseenFist);
        let affects_foe_mon =
            (md.category != MoveCategory::Status || foe_status_target) && !protect_blocks;
        let mb = matches!(b.state.side(side).active().ability, A::MoldBreaker | A::Teravolt | A::Turboblaze);
        let fa = b.state.side(foe).active().ability;
        let absorbs = matches!((md.typ, fa),
            (Type::Water, A::WaterAbsorb | A::DrySkin)
            | (Type::Electric, A::VoltAbsorb)
            | (Type::Ground, A::EarthEater));
        if affects_foe_mon && absorbs && !mb && b.state.side(foe).active().is_alive() {
            let p = b.state.side(foe).active();
            if p.hp < p.max_hp {
                let heal = (p.max_hp / 4).max(1).min(p.max_hp - p.hp);
                let fslot = b.state.side(foe).active_index;
                push(&mut b, Instruction::Heal { side: foe, slot: fslot, amount: heal });
            }
            // `onTryHit` returning null empties `targets`, so `useMoveInner` reaches MoveFail
            // and a crash-damage move crashes (rb1100 t12: Supercell Slam into Volt Absorb).
            apply_crash_damage(&mut b, side, &md);
            return vec![b];
        }
        // Lightning Rod / Storm Drain / Motor Drive: like the absorbing abilities above these are
        // PS `onTryHit` handlers that `return null` — they run in `hitStepTryHitEvent`, which
        // precedes `hitStepAccuracy` in `trySpreadMoveHit`'s step list, so a move they block makes
        // NO accuracy roll at all. The engine knew the type immunity (`ability_immune`) but not the
        // stat boost, and for a STATUS move of that type it fell through to the accuracy draw —
        // a `rust-extra randomChance@accuracy` over-emission (rb1211 t18 and rb1350 t30: Thunder
        // Wave into a Lightning Rod Rhydon / Raichu, where PS's whole unit draws NOTHING).
        // Mold Breaker bypasses (all three are `breakable`).
        let redirect_boost = match (md.typ, fa) {
            (Type::Electric, A::LightningRod) | (Type::Water, A::StormDrain) => Some(BoostIndex::SpecialAttack),
            (Type::Electric, A::MotorDrive) => Some(BoostIndex::Speed),
            _ => None,
        };
        if affects_foe_mon && !mb && b.state.side(foe).active().is_alive() {
            if let Some(stat) = redirect_boost {
                raise_boost(&mut b, foe, stat, 1);
                apply_crash_damage(&mut b, side, &md);
                return vec![b];
            }
        }
        // Sap Sipper: a Grass move targeting the holder is nullified and raises its Attack
        // one stage (PS `onTryHitPriority 1` -> `boost({atk: 1})`).
        if affects_foe_mon
            && md.typ == Type::Grass
            && fa == A::SapSipper
            && !mb
            && b.state.side(foe).active().is_alive()
        {
            raise_boost(&mut b, foe, BoostIndex::Attack, 1);
            apply_crash_damage(&mut b, side, &md);
            return vec![b];
        }
        // Flash Fire: a Fire move targeting the holder is nullified (PS `onTryHit` `return null`)
        // and activates the `flashfire` volatile, which grants ×1.5 to the holder's own Fire moves
        // until it switches out. Applies to damaging and status Fire moves alike; Mold Breaker
        // bypasses.
        if affects_foe_mon
            && md.typ == Type::Fire
            && fa == A::FlashFire
            && !mb
            && b.state.side(foe).active().is_alive()
        {
            if !b.state.side(foe).volatiles.contains(VolatileStatus::FlashFire) {
                push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::FlashFire });
            }
            apply_crash_damage(&mut b, side, &md);
            return vec![b];
        }
        // Well-Baked Body (+2 Def vs a Fire move) and Wind Rider (+1 Atk vs a `wind`-flagged
        // move) are the same `onTryHit` -> `return null` shape as every ability above, and the
        // damaging path already models both — but only there, inside the `connects` test. A
        // STATUS move of the matching type fell through this whole block to the accuracy draw.
        //
        // Enumerated from the pin rather than recalled: the gen9 abilities whose `onTryHit`
        // blocks on a move TYPE or FLAG are exactly dryskin / earthEater / flashfire /
        // lightningrod / motordrive / sapsipper / soundproof / stormdrain / voltabsorb /
        // waterabsorb / wellbakedbody / windrider / bulletproof (plus goodasgold, magicbounce,
        // oblivious, overcoat, sturdy, telepathy, wonderguard, which are handled elsewhere or
        // are singles-irrelevant). Well-Baked Body and Wind Rider were the two missing here.
        //
        // rb1432 t49 / rb1650 t11: Will-O-Wisp (Fire, 85% accurate) into a Dachsbun. PS runs
        // `hitStepTryHitEvent` at step 2 and `hitStepAccuracy` at step 5
        // (`sim/battle-actions.ts:551-563`), so the whole unit draws NOTHING and Dachsbun ends
        // at +2 Def, unburned. The engine rolled `randomChance(85,100)`, burned it, and ran a
        // draw ahead from there.
        //
        // Soundproof / Bulletproof block with no side effect at all; the `flag_immune` test on
        // the damaging path had them, this path did not (Roar into a Soundproof holder).
        let flag_blocked = (md.flag_sound && fa == A::Soundproof)
            || (md.flag_bullet && fa == A::Bulletproof)
            || (is_wind_move(md.id) && fa == A::WindRider);
        if affects_foe_mon
            && (flag_blocked || (md.typ == Type::Fire && fa == A::WellBakedBody))
            && !mb
            && b.state.side(foe).active().is_alive()
        {
            if fa == A::WellBakedBody {
                raise_boost(&mut b, foe, BoostIndex::Defense, 2);
            } else if fa == A::WindRider {
                raise_boost(&mut b, foe, BoostIndex::Attack, 1);
            }
            apply_crash_damage(&mut b, side, &md);
            return vec![b];
        }
    }

    // Future Sight / Doom Desire are category Special, so they don't reach the status-move path —
    // but they deal NO damage this turn. They schedule a delayed strike (2 turns out) on the target
    // side, computed at land time from the caster's stats. PS `onTry` adds a slot condition and
    // fails outright while one is already pending. (`ignoreImmunity`, so Protect/typing don't gate.)
    if matches!(move_id.to_id(), "futuresight" | "doomdesire") {
        let target = side.other();
        if b.state.side(target).future_sight.0 == 0 {
            let caster_slot = b.state.side(side).active_index;
            let prev = b.state.side(target).future_sight;
            push(&mut b, Instruction::SetFutureSight { side: target, previous: prev, new: (3, caster_slot) });
        }
        return vec![b];
    }

    // Status moves handled specially.
    if md.category == MoveCategory::Status {
        // PS `-notarget`: a status move that targets the foe POKEMON fails outright — no accuracy
        // roll, no effect — when the foe active has fainted mid-turn (the second mover's Encore /
        // Thunder Wave / Taunt into a foe that just self-KO'd, e.g. c2a3 t7: Regieleki Explosion
        // faints itself, then Tinkaton's Encore has no target). `getMoveTargets` returns an empty
        // list, so `useMoveInner` bails BEFORE `hitStepAccuracy` — the engine must emit no draw
        // (an emitted always-true accuracy would silently offset the PRNG stream). Gated on the
        // MoveTarget being a single foe (not User / FoeSide / All / AllySide): self moves
        // (Substitute, Swords Dance), hazards (Spikes → FoeSide), and field moves (weather,
        // screens) still resolve because their target — the user or a side — always persists.
        // Annotation-gated: in the DP (Enumerate/Sample) state path no draws are emitted and the
        // move is already state-neutral against a fainted foe, so this is purely a draw-suppression
        // fix for the Replicate/differ streams — leaving the DP sweep byte-identical.
        //
        // CURSE is the one move whose static `target` lies: PS's `onModifyMove` retargets it to its
        // `nonGhostTarget: 'self'` for a non-Ghost user (moves.ts), and `onModifyMove` runs BEFORE
        // `getMoveTargets`. So a non-Ghost Curse into a foe that fainted earlier this turn still
        // resolves fully — self-boosts plus the `random(100)` self-drop discard. (c6a2s114 d36:
        // Victreebel faints to its own Life Orb recoil after Sludge Bomb, then Snorlax's Curse
        // still fires; the engine bailed here, dropping PS's `random[100]@curse` AND the boosts.)
        let retargets_self = md.id.to_id() == "curse"
            && !b.state.side(side).active().types.contains(&Type::Ghost);
        if annotating()
            && !retargets_self
            && matches!(md.target,
                crate::data::MoveTarget::Normal | crate::data::MoveTarget::AdjacentFoe
                    | crate::data::MoveTarget::Any | crate::data::MoveTarget::RandomNormal
                    | crate::data::MoveTarget::Scripted)
            && !b.state.side(foe).active().is_alive()
        {
            return vec![b];
        }
        // Shed Tail: put up a Substitute (floor(maxHP/4) HP) at a cost of ceil(maxHP/2) HP, then
        // pivot out PASSING the Substitute to the incoming mon. Fails (no sub, no pivot) with no
        // switch target, an existing Substitute, or HP at/below the cost. Self-targeting — the
        // foe's Protect/Substitute do not apply.
        if md.id.to_id() == "shedtail" {
            let mut b = b;
            let (hp, max_hp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
            let cost = (max_hp + 1) / 2; // ceil(maxHP/2)
            let sub_hp = max_hp / 4; // floor(maxHP/4)
            let has_sub = b.state.side(side).volatiles.contains(VolatileStatus::Substitute);
            if hp <= cost || has_sub || !has_alive_bench(&b.state, side) {
                return vec![b]; // fails; PP already paid, no pivot
            }
            let slot = b.state.side(side).active_index;
            push(&mut b, Instruction::Damage { side, slot, amount: cost });
            push(&mut b, Instruction::ChangeSubstituteHp { side, amount: sub_hp });
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Substitute });
            match pivot {
                Pivot::Target(t) => {
                    emit_pivot_trailing_update(&mut b);
                    let pre = b.state;
                    apply_switch_pass_sub(&mut b, side, t);
                    emit_switch_bracket(&mut b, &pre, side, t);
                }
                Pivot::Pause => push(&mut b, Instruction::PivotPending { side }),
                Pivot::Stay => {}
            }
            return vec![b];
        }
        // Revival Blessing: revive a fainted party member at floor(maxHP/2) HP, kept benched (NOT
        // switched in). Fails (no revive) when the user has no fainted ally. The revive target is
        // supplied like a pivot landing: `Pivot::Target` on verification paths (recorded choice),
        // `Pivot::Pause` -> `Revive` request on the sampled request-flow path.
        if md.id.to_id() == "revivalblessing" {
            let mut b = b;
            if !has_fainted_bench(&b.state, side) {
                return vec![b]; // fails; no target, PP already paid
            }
            match pivot {
                Pivot::Target(t) => apply_revive(&mut b, side, t),
                Pivot::Pause => push(&mut b, Instruction::RevivePending { side }),
                Pivot::Stay => {}
            }
            return vec![b];
        }
        // Protect blocks a status move that targets the foe (Thunder Wave, Will-O-Wisp,
        // Toxic, Taunt, Parting Shot, Roar, ...) — but not self/field moves (Swords Dance,
        // recovery, weather, hazards). `md.target_volatile` is the codegen's fold of PS
        // `move.volatileStatus`, which SELF-targeting moves carry too (Protect itself,
        // Substitute, Magnet Rise, Aqua Ring, Destiny Bond, Imprison, Laser Focus, …); PS's
        // Protect `onTryHit` and Substitute `onTryPrimaryHit` (`if (target === source) return`,
        // data/moves.ts:20857 / :16512) never see such a move, so gate on the move's TARGET.
        // Whether the move is AIMED AT the foe's mon (not its side, not the field, not the
        // user) — PS registers Protect's `onTryHit` and Substitute's `onTryPrimaryHit` on the
        // target Pokemon, so both fire on exactly this set. Guessing from the payload
        // (`status`/`target_boosts`/`target_volatile`/`force_switch`) missed the moves whose
        // foe-facing effect lives in an `onHit` callback the codegen cannot see — Strength Sap,
        // Trick/Switcheroo, Topsy-Turvy, Pain Split, …
        let targets_foe_mon = matches!(md.target,
            crate::data::MoveTarget::Normal | crate::data::MoveTarget::AdjacentFoe
                | crate::data::MoveTarget::Any | crate::data::MoveTarget::AllAdjacent
                | crate::data::MoveTarget::AllAdjacentFoes | crate::data::MoveTarget::RandomNormal
                | crate::data::MoveTarget::Scripted);
        // PS `checkMoveBypassesProtect` (sim/battle.ts:1300-1308): Protect stops the move iff it
        // carries the `protect` FLAG. Roar / Whirlwind / Perish Song and the field moves do not.
        if targets_foe_mon && md.flag_protect
            && b.state.side(foe).volatiles.contains(VolatileStatus::Protect)
        {
            return vec![b];
        }
        let targets_foe = targets_foe_mon;
        // Magic Bounce (breakable): a reflectable status move aimed at the holder (or its
        // side — hazards) is used BY the holder against the original user instead. Runs
        // after Protect (onTryHitPriority 3 vs 1) and before the Substitute check (the
        // sub's block is a later event step in PS). The bounced move re-resolves fully
        // from the bouncer: its own accuracy, the original user's immunities, hazards
        // onto the original user's side, Roar dragging the original user, etc. Calling
        // execute_status_move directly means a bounced move can never re-bounce.
        if is_reflectable_move(md.id)
            && b.state.side(foe).active().is_alive()
            && b.state.side(foe).active().ability == crate::ids::Ability::MagicBounce
            && !matches!(
                b.state.side(side).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
            )
            && !matches!(b.state.side(foe).pending_move, PendingMove::Charging(m) if is_semi_invuln_move(m))
        {
            return execute_status_move(b, foe, &md, None);
        }
        // Good as Gold (breakable): `onTryHit(target, source, move) { if (move.category ===
        // 'Status' && target !== source) { -immune; return null; } }` (data/abilities.ts:1585-1596).
        // `onTryHit` fires in `hitStepTryHitEvent`, which `trySpreadMoveHit` runs as step 1 —
        // BEFORE `hitStepAccuracy` at step 4 (sim/battle-actions.ts:551-563). So a status move
        // aimed at the holder makes **no accuracy draw at all**; the engine rolled one and then
        // blocked only the move's PAYLOAD further down (the `foe_immune` gate in
        // `execute_status_move`), leaving the PRNG stream one draw long.
        //
        // rb1277 d10 t8 is the witness: Lilligant's Sleep Powder into a Tera Dark Gholdengo. PS
        // records ZERO draws for the unit; the engine rolled `randomChance(75,100)` and ran +1
        // ahead from turn 8 on, first surfacing as `result random[16]@gigadrain` two units later.
        //
        // It returns `null`, not `false`, so `trySpreadMoveHit`'s `atLeastOneFailure` stays false
        // and `pokemon.moveThisTurnResult = null` (:610) — NOT `false`. Stomping Tantrum doubles
        // only on an explicit `false`, so `move_failed` must stay clear here.
        //
        // Placed above the Substitute block because `onTryHit` (priority 0) precedes Substitute's
        // `onTryPrimaryHit`, and below Magic Bounce (`onTryHitPriority: 1`). Mold Breaker /
        // Teravolt / Turboblaze pierce it as a `breakable` ability, and Mycelium Might sets
        // `move.ignoreAbility` for the user's status moves.
        if targets_foe
            && b.state.side(foe).active().is_alive()
            && b.state.side(foe).active().ability == crate::ids::Ability::GoodAsGold
            && !matches!(
                b.state.side(side).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt
                    | crate::ids::Ability::Turboblaze | crate::ids::Ability::MyceliumMight
            )
        {
            return vec![b];
        }
        // A Substitute blocks foe-targeting status moves unless they bypass it (sound
        // moves, Taunt, Encore, ...) or the user has Infiltrator. PS blocks the move at
        // `Substitute.onTryPrimaryHit` (inside `spreadMoveHit`, AFTER `hitStepAccuracy`), so a
        // numeric-accuracy status move still consumes `randomChance(accuracy,100)` here — unless it
        // was already stopped by an earlier hit step (powder / Thunder-Wave type immunity). The
        // outcome is state-neutral (the sub blocks the effect; a miss does nothing) → draw-only.
        if targets_foe
            && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute)
            && !md.flag_bypass_sub
            && b.state.side(side).active().ability != crate::ids::Ability::Infiltrator
        {
            // An ALREADY-STATUSED target does NOT suppress the roll. `setStatus` fails inside
            // `moveHit`, long after `hitStepAccuracy`, and `hitStepTryImmunity` (battle-actions.ts:
            // 661-684) has no status check at all. This site used to carry a `target_already_statused`
            // gate justified by d6 t58-62 ("Toxic on an already-paralyzed, subbed Garchomp draws
            // nothing") — but d6's Toxic user is **Toxtricity, Electric/POISON**, so the real reason
            // was the gen-8 Poison-type Toxic override, which now lives in `accuracy_forced_true`.
            // rb5039 d46 (Toxic into an already-badly-poisoned, subbed Keldeo) and rb1642 d35
            // (Will-O-Wisp into a statused, subbed Keldeo) are the counter-witnesses: PS rolls.
            if annotating()
                && md.accuracy != 0
                && !accuracy_forced_true(&b, side, &md)
                && status_move_reaches_accuracy(&b, side, &md)
            {
                let acc = accuracy_arg(&b, side, &md);
                let hp = accuracy_of(&b, side, &md);
                draw(&mut b, "randomChance", &[acc, 100], (hp > 0.0) as i64, "accuracy");
            }
            return vec![b];
        }
        let ins_before = b.ins.len();
        let mut branches = execute_status_move(b, side, &md, foe_pending_move);
        // PS runs a status move through `hitStepMoveHitLoop` exactly like a damaging move: a
        // `moveHit` that applies its effect fires the per-hit `eachEvent('Update')`
        // (battle-actions.ts:970) and the post-hit-loop `eachEvent('Update')` (:1024). A move that
        // fails at `tryHit` / `spreadMoveHit` (immune / already-statused / missed / redundant boost)
        // breaks BEFORE 970 and fires neither. Both are actives-Speed-tie shuffles (no-op off a
        // tie), so this only affects tied boards. Detect a successful moveHit per-branch as "the
        // status resolution added ≥1 effect instruction" — a failed move applies none. Pure
        // protect-fail bookkeeping (only a `SetStallCounter` reset) is NOT a moveHit success, and
        // neither is a **`SleepClauseBlocked`**: Sleep Clause Mod is an `onSetStatus` that returns
        // FALSE, so `trySetStatus` fails, `moveHit` returns false, `hitStepMoveHitLoop` breaks at
        // `hit === 1` and PS fires NEITHER Update. The instruction records that the clause SPOKE,
        // not that the move landed.
        //
        // rb5021 d20 t18: a Spore into a side that already has a sleeper, on a 96-vs-96 board. The
        // engine emitted 970 + 1024, ran TWO draws ahead from turn 18, and the damage surfaced at
        // d21 dressed as a move-order-tie bug — the `b0 == b3` composition was never wrong. The
        // recorded shuffle GROUPS prove it: d21's commit sort reads `[p1 Amoonguss, p2 Snorlax]`
        // and the dynamic re-sort reads `[p2 Snorlax, p1 Amoonguss]`, so b0 = 1 (swapped), and PS
        // then executes Amoonguss, so b3 = 1 — `b0 == b3` gives PS's answer exactly.
        //
        // The 970/1024 Updates fire only when PS enters the per-POKEMON `hitStepMoveHitLoop`
        // (`spreadMoveHit`, battle-actions.ts). A status move that targets a SIDE or the FIELD
        // (Reflect/Light Screen/Tailwind = allySide; Spikes/Stealth Rock = foeSide; weather/terrain/
        // Trick Room = all; Heal Bell/Aromatherapy = allyTeam) resolves via the side/field `onHit`
        // path and never enters that loop, so it fires NEITHER Update — only its runAction 2882
        // (appended by `run_move_action`). Ground-truthed on c5a1 t12: Grimmsnarl's Prankster Reflect
        // (allySide) ran first and produced zero move-internal Updates in the pinned PS trace, while
        // the engine over-emitted 970+1024 → +2 leading shuffles → PRNG desync at the psychic
        // accuracy roll. Self-targeting pokemon moves (Calm Mind = self) DO enter the loop and fire.
        let hits_pokemon = !matches!(
            md.target,
            crate::data::MoveTarget::AllySide
                | crate::data::MoveTarget::FoeSide
                | crate::data::MoveTarget::All
                | crate::data::MoveTarget::AllyTeam
        );
        if annotating() && hits_pokemon {
            for sb in &mut branches {
                let did_something = sb.ins.len() > ins_before
                    && sb.ins[ins_before..]
                        .iter()
                        .any(|i| {
                            !matches!(
                                i,
                                Instruction::SetStallCounter { .. }
                                    | Instruction::SetStallTurns { .. }
                                    | Instruction::SleepClauseBlocked { .. }
                            )
                        });
                if did_something {
                    emit_update_hit(sb); // 970 (per-hit, pre-faint board)
                    emit_update(sb); // 1024 (post-hit-loop, alive-gated)
                }
            }
        }
        // Self-switch status moves (Teleport, Chilly Reception, Parting Shot) pivot out.
        match pivot {
            Pivot::Target(t) => {
                for sb in &mut branches {
                    if sb.state.side(side).active().is_alive() {
                        emit_pivot_trailing_update(sb); // move action's 2882 (pre-switch board)
                        let pre = sb.state;
                        apply_switch(sb, side, t);
                        emit_switch_bracket(sb, &pre, side, t);
                    }
                }
            }
            Pivot::Pause => {
                for sb in &mut branches {
                    if sb.state.side(side).active().is_alive() && has_alive_bench(&sb.state, side) {
                        push(sb, Instruction::PivotPending { side });
                    }
                }
            }
            Pivot::Stay => {}
        }
        // Dancer: the foe immediately copies a successfully used dance move (see
        // apply_dancer_copies). Only from a real (non-external) use.
        if !external && is_dance_move(move_id) {
            branches = apply_dancer_copies(branches, side, move_id);
        }
        return branches;
    }

    // Poltergeist: `onTry(source, target) { return !!target.item; }` (data/moves.ts:13610-13612).
    // PS runs `singleEvent('Try', …)` at the top of `hitStepTryHitEvent`/`tryMoveHit`
    // (sim/battle-actions.ts:821) — after `deductPP`, but BEFORE `hitStepAccuracy` — so against an
    // itemless target the move fails outright with NO accuracy roll and no damage (rb1327 t30: a
    // Poltergeist aimed at a switching-in itemless mon; PS records zero draws for the unit).
    if md.id.to_id() == "poltergeist" && b.state.side(foe).active().item == Item::None {
        b.move_failed = true; // PS `moveThisTurnResult = false`
        return vec![b];
    }

    // `hitStepTryImmunity` (sim/battle-actions.ts:560) runs its `singleEvent('TryImmunity')`
    // BEFORE `hitStepAccuracy` (:563), so a move whose `onTryImmunity` returns false fails with
    // NO accuracy roll at all. The damaging members of that set (data/moves.ts):
    //   endeavor      :4796  `return pokemon.hp < target.hp`
    //   dreameater    :4260  `return target.status === 'slp' || target.hasAbility('comatose')`
    //   synchronoise  :18663 `return target.hasType(source.getTypes())`
    // rb1282 d13: p1's Luvdisc Endeavors a target that is NOT above it in HP and PS records ZERO
    // draws for the unit; the engine rolled the 100% accuracy check, offsetting the prng for the
    // rest of the game (its next damage roll came out 11 where PS drew a different value).
    {
        let (atk, def) = (b.state.side(side).active(), b.state.side(foe).active());
        let immune = match md.id.to_id() {
            "endeavor" => !(atk.hp < def.hp),
            "dreameater" => {
                def.status != Status::Sleep && def.ability != crate::ids::Ability::Comatose
            }
            "synchronoise" => !def
                .types
                .iter()
                .any(|t| *t != Type::None && atk.types.contains(t)),
            _ => false,
        };
        if immune {
            b.move_failed = true; // PS `moveThisTurnResult = false`
            return vec![b];
        }
    }

    // Protect: a protected target blocks the incoming damaging move (Protect moves +4
    // priority, so the protector has already set the volatile this turn).
    if b.state.side(foe).volatiles.contains(VolatileStatus::Protect)
        // Mighty Cleave's flags carry no `protect` entry — it strikes straight through.
        && md.id.to_id() != "mightycleave"
        // Unseen Fist (Urshifu): contact moves ignore protection.
        && !(md.flag_contact && b.state.side(side).active().ability == crate::ids::Ability::UnseenFist)
    {
        // High Jump Kick / Jump Kick / Supercell Slam still crash into the protector (1/2
        // max HP — PS `onMoveFail` fires on a protected target too).
        apply_crash_damage(&mut b, side, &md);
        b.move_failed = true; // Protect-blocked → PS moveThisTurnResult false (doubles ST)
        // A blocked rampage ends its lock (Protect's own `onTryHit` only deletes it outright
        // when `duration === 2`, which a non-connecting turn never reaches).
        return end_rampage_on_fail(b, side, move_id);
    }

    // (Queenly Majesty's `onFoeTryMove` and Psychic Terrain's `onTryHitPriority: 4` are checked
    // ABOVE, before the status/damaging split — both outrank everything on this path.)

    // Air Balloon / Magnet Rise: the holder is off the ground, so Ground moves miss it. Both
    // are `onImmunity('Ground') -> false` (`data/items.ts` airballoon, `data/moves.ts`
    // magnetrise `condition`), which `runImmunity` consults in `hitStepTryImmunity` — before
    // the accuracy roll, so no draw is made.
    if md.typ == Type::Ground
        && (b.state.side(foe).active().item == Item::AirBalloon
            || b.state.side(foe).volatiles.contains(VolatileStatus::MagnetRise))
        && b.state.side(foe).active().is_alive()
    {
        b.move_failed = true; // blocked → moveThisTurnResult false
        return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
    }

    // Brick Break / Psychic Fangs / Raging Bull shatter the target's screens BEFORE the
    // accuracy roll (PS `onTryHit` precedes the accuracy step — the break sticks even on a
    // miss), through a Substitute, but not when Protect already blocked the move above.
    if matches!(md.id.to_id(), "brickbreak" | "psychicfangs" | "ragingbull") {
        for sc in [SideConditionId::Reflect, SideConditionId::LightScreen, SideConditionId::AuroraVeil] {
            let cur = match sc {
                SideConditionId::Reflect => b.state.side(foe).side_conditions.reflect,
                SideConditionId::LightScreen => b.state.side(foe).side_conditions.light_screen,
                _ => b.state.side(foe).side_conditions.aurora_veil,
            };
            if cur != 0 {
                push(&mut b, Instruction::SetSideCondition { side: foe, condition: sc, previous: cur, new: 0 });
            }
        }
    }

    // Damaging move: branch on accuracy (hit/miss), then crit, then the 16 rolls.
    let mut out = Vec::new();
    // Branches where the move failed to connect. Kept apart from `out` so the rampage lock
    // transition (`apply_rampage_state`) and the recharge commitment (`apply_recharge`) only
    // see connecting branches — PS applies both through on-hit self effects, so a miss
    // neither locks/extends a rampage nor forces the recharge turn.
    let mut miss_out = Vec::new();
    let hit_prob = accuracy_of(&b, side, &md);
    let miss_prob = 1.0 - hit_prob;

    // PS `hitStepAccuracy`: a move with a numeric accuracy rolls `randomChance(accuracy, 100)`
    // once (before the hit loop); accuracy `true` (engine `md.accuracy == 0`) skips the roll.
    // The roll runs only AFTER the earlier hit-steps pass: a move against a FAINTED foe (no
    // valid target), a SEMI-INVULNERABLE dodger (`hitStepInvulnerabilityEvent`), or a
    // TYPE/ABILITY/FLAG-immune target (`hitStepTypeImmunity`/`hitStepTryHitEvent` — Levitate,
    // Water Absorb, Soundproof, a 0× type chart, …) fails earlier and never reaches
    // `hitStepAccuracy`, so PS makes no accuracy draw. (Protect blocks earlier still — already
    // returned above.) Gate the annotation on the move actually reaching the accuracy step.
    // Annotate on `b` so both the hit and miss branches inherit this draw. (Accuracy/evasion
    // stages and modifiers shift the recorded arg; unmodeled here — the differ flags them.)
    let mut acc_draw_pushed = false;
    if annotating() && md.accuracy != 0 && !accuracy_forced_true(&b, side, &md)
        && !counter_family_ontry_fails(&b, side, &md)
    {
        let reaches_accuracy = {
            let foe_alive = b.state.side(foe).active().is_alive();
            let semi_invuln = matches!(b.state.side(foe).pending_move, PendingMove::Charging(m) if is_semi_invuln_move(m));
            let connects = {
                let defender = b.state.side(foe).active();
                let def_ab = if matches!(
                    b.state.side(side).active().ability,
                    crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
                ) || move_ignores_ability(md.id) {
                    crate::ids::Ability::None
                } else {
                    defender.ability
                };
                let wind_immune = def_ab == crate::ids::Ability::WindRider && is_wind_move(md.id);
                let flag_immune = (md.flag_sound && def_ab == crate::ids::Ability::Soundproof)
                    || (md.flag_bullet && def_ab == crate::ids::Ability::Bulletproof)
                    || wind_immune;
                let scrappy = matches!(b.state.side(side).active().ability, crate::ids::Ability::Scrappy | crate::ids::Ability::MindsEye);
                let def_types_eff = effective_def_types(scrappy, md.typ, defender.types);
                crate::damage::type_multiplier(md.typ, def_types_eff) != 0.0
                    && !ability_immune(md.typ, def_ab)
                    && !flag_immune
            };
            foe_alive && !semi_invuln && connects
        };
        if reaches_accuracy {
            let acc = accuracy_arg(&b, side, &md);
            // Result is the HIT value (1): the accuracy `randomChance(acc,100)` is true iff the
            // move connects, and every branch that survives this point is a hit branch. The MISS
            // branch (built below) overrides its copy of this draw to 0 so the Replicate filter
            // selects hit vs miss by the drawn value — not by a shared "can-hit" flag (which made
            // both branches carry 1 and the filter fall through to the crit roll, mis-selecting a
            // hit branch on a real miss).
            draw(&mut b, "randomChance", &[acc, 100], (hit_prob > 0.0) as i64, "accuracy");
            acc_draw_pushed = true;
        }
    }

    if miss_prob > 0.0 {
        let mut mb = scaled(&b, miss_prob);
        // The accuracy roll came up a miss on this branch: flip the inherited hit-result to 0.
        if acc_draw_pushed {
            if let Some(d) = mb.draws.last_mut() {
                d.result = 0;
            }
        }
        // High Jump Kick / Jump Kick / Supercell Slam: missing costs the user 1/2 of its
        // max HP (crash; PS `onMoveFail` → `damage(source.baseMaxhp / 2)`).
        apply_crash_damage(&mut mb, side, &md);
        mb.move_failed = true; // missed → PS moveThisTurnResult false (doubles ST next turn)
        miss_out.extend(end_rampage_on_fail(mb, side, move_id));
    }

    let foe_alive = b.state.side(foe).active().is_alive();
    if !foe_alive {
        // No living target: the move fails outright — no rampage lock, no recharge.
        let mut hb = scaled(&b, hit_prob);
        hb.move_failed = true; // no target → moveThisTurnResult false
        out.extend(end_rampage_on_fail(hb, side, move_id));
        out.extend(miss_out);
        return out;
    }
    // A target mid-Fly/Dig/etc. (semi-invulnerable) dodges the move entirely.
    if matches!(b.state.side(foe).pending_move, PendingMove::Charging(m) if is_semi_invuln_move(m)) {
        let mut hb = scaled(&b, hit_prob);
        // `hitStepInvulnerability` empties `targets`, so `trySpreadMoveHit` returns false and
        // `useMoveInner` fires MoveFail — the crash-damage moves crash.
        apply_crash_damage(&mut hb, side, &md);
        hb.move_failed = true; // dodged (semi-invulnerable) → moveThisTurnResult false
        out.extend(end_rampage_on_fail(hb, side, move_id));
        out.extend(miss_out);
        return out;
    }

    // A type-immune move (e.g. Close Combat vs a Ghost, or Ground vs Levitate) deals no
    // damage and skips its self-stat secondary — PS only applies `self` boosts on a hit.
    let defender = b.state.side(foe).active();
    // Mold Breaker also bypasses the defender's immunity abilities (Levitate, absorbs,
    // Soundproof, Bulletproof) — treat the ability as None for the immunity check.
    let def_ab = if matches!(
        b.state.side(side).active().ability,
        crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
    ) || move_ignores_ability(md.id) {
        crate::ids::Ability::None
    } else {
        defender.ability
    };
    // Wind Rider: immunity to damaging wind moves (+1 Atk applied on the absorb branch).
    let wind_immune = def_ab == crate::ids::Ability::WindRider && is_wind_move(md.id);
    let flag_immune = (md.flag_sound && def_ab == crate::ids::Ability::Soundproof)
        || (md.flag_bullet && def_ab == crate::ids::Ability::Bulletproof)
        || wind_immune;
    let scrappy = matches!(b.state.side(side).active().ability, crate::ids::Ability::Scrappy | crate::ids::Ability::MindsEye);
    let def_types_eff = effective_def_types(scrappy, md.typ, defender.types);
    let connects = crate::damage::type_multiplier(md.typ, def_types_eff) != 0.0
        && !ability_immune(md.typ, def_ab)
        && !flag_immune;
    if !connects {
        let mut ib = scaled(&b, hit_prob);
        // Absorbing abilities that boost the holder on the negated hit: Well-Baked Body (+2 Def
        // vs Fire), Wind Rider (+1 Atk vs a wind move). Only when this ability caused the block.
        let well_baked = def_ab == crate::ids::Ability::WellBakedBody && md.typ == Type::Fire;
        if well_baked {
            raise_boost(&mut ib, foe, BoostIndex::Defense, 2);
        }
        if wind_immune {
            raise_boost(&mut ib, foe, BoostIndex::Attack, 1);
        }
        // A blocked target is filtered out of `targets` by `hitStepTryImmunity` /
        // `hitStepTryHitEvent`, so `trySpreadMoveHit` returns false and `useMoveInner` reaches
        // `singleEvent('MoveFail')` (`battle-actions.ts:526`) — the crash-damage moves crash on
        // an immunity or an absorbing ability exactly as they do on a miss. rb1100 t12: Supercell
        // Slam into a Volt Absorb Thundurus-Therian costs Eelektross 145 HP in PS.
        apply_crash_damage(&mut ib, side, &md);
        // Type-chart / Levitate immunity fails the move (PS `runImmunity` → hitResult false →
        // moveThisTurnResult false → Stomping Tantrum doubles next turn). A boosting absorb
        // (Well-Baked Body / Wind Rider) instead returns null → NOT a failure (no ST double).
        ib.move_failed = !well_baked && !wind_immune;
        out.push(ib);
        // A rampage move (Outrage/Thrash) that hits an immune target ENDS the lock (without
        // confusion) — route through the rampage/recoil tail rather than returning bare.
        // No recharge either: PS's `mustrecharge` is an on-hit self volatile. A Dancer copy
        // (external) never engages the rampage lock.
        let mut out = if external { out } else { apply_rampage_state(out, side, move_id) };
        out.extend(miss_out);
        return apply_struggle_recoil(out, side, struggling);
    }

    // Ice Face nullifies one physical hit and changes Eiscue into its Noice forme.
    if def_ab == crate::ids::Ability::IceFace
        && md.category == MoveCategory::Physical
        && defender.species == crate::ids::Species::from_id("eiscue").unwrap_or(crate::ids::Species::None)
        && md.hits_max == 1
    {
        let mut hb = scaled(&b, hit_prob);
        // PS's Ice Face is an `onDamage`/`onCriticalHit`/`onEffectiveness` block: getDamage still
        // rolls the crit `randomChance(1, critMult)` and the damage `random(16)` (onCriticalHit
        // forces no-crit, onEffectiveness forces typeMod 0), then onDamage zeroes the result. Emit
        // those two draw-and-discards so the from-seed stream advances exactly as PS's does.
        let crit_den = ps_crit_den(&b, side, &md);
        // **A draw-and-discard still has to record the REAL value.** The result is irrelevant to
        // the outcome — `onDamage` zeroes the damage whatever the crit and roll were — but the seed
        // gate does not "pick the closest branch": it draws from the real PRNG and keeps only the
        // branches whose RECORDED result equals what it drew (`seedgate.rs:330-352`). A hardcoded
        // 0 therefore kills the only live branch the moment the PRNG hands back anything else.
        // rb1710 d18: U-turn into an intact Ice Face Eiscue, PS's roll is 13, the engine's branch
        // claimed 0, and the unit desynced on a draw whose value cannot matter.
        emit_discarded_damage_rolls(&mut hb, crit_den);
        break_ice_face(&mut hb, foe);
        // A nullified hit is still a hit for Life Orb — see `apply_life_orb_recoil`. Placed here,
        // where the main path's `apply_post_damage` sits: after the damage step and ahead of the
        // self-drops / secondaries, so a secondary reading HP reads the post-orb HP as PS's does.
        // The `source.hp` snapshot for step 8 is taken AHEAD of the orb, exactly as
        // `apply_post_damage` takes it (PS runs `onAfterHit` before `onAfterMoveSecondarySelf`).
        hb.after_hit_user_alive = hb.state.side(side).active().is_alive();
        apply_life_orb_recoil(&mut hb, side, &md);
        // Step 8, `onAfterHit`, still runs on a nullified hit — see `apply_knock_off_take_item`.
        if md.id.to_id() == "knockoff" {
            let alive = hb.after_hit_user_alive;
            apply_knock_off_take_item(&mut hb, side, def_ab, alive);
        }
        // PS's Ice Face is `onDamage` returning 0 — a NUMBER, not `false` — so `spreadMoveHit`
        // keeps the target live (`if (!damage[i] && damage[i] !== 0) targets[i] = false`,
        // battle-actions.ts:1127-1129) and still runs step 5, `secondaries()`. rb1038 t5: Throat
        // Chop into an intact Eiscue — PS rolls `random[100]` and the target ends the turn with
        // the `throatchop` volatile even though the hit dealt nothing.
        // `spreadMoveHit` step 4, `selfDrops`, runs BEFORE step 5's `secondaries()` and this
        // branch skipped it — see the Disguise arm below for the witness and the reasoning.
        for mut hb in apply_self_drop(hb, side, &md)
            .into_iter()
            .flat_map(|x| if external { vec![x] } else { start_rampage_lock(x, side, move_id) })
        {
            apply_damage_secondaries(&mut hb, side, &md, false);
            for mut sb in apply_target_secondary(hb, side, &md)
                .into_iter()
                .flat_map(|sb| apply_flinch_split(sb, side, &md))
                .flat_map(|sb| apply_cursed_body(sb, side, &md))
                .flat_map(|sb| apply_contact_secondaries(sb, side, &md))
                .flat_map(|sb| apply_source_damaging_hit(sb, side, &md))
            {
                // A nullified hit is still a connecting hit, so the per-hit `eachEvent('Update')`
                // (970) and the post-hit-loop one (1024) both fire.
                emit_update_hit(&mut sb);
                emit_update(&mut sb);
                // **A nullified hit still CONNECTED, so a pivot user still leaves.** `selfSwitch`
                // is set in `hitStepMoveHitLoop` on `move.totalDamage !== false`, and Ice Face's
                // `onDamage` returns the NUMBER 0 — the target stays in `targets` and the move
                // counts as a hit. The Disguise arm below already did this; the Ice Face arm
                // returned without ever looking at `pivot`, so a U-turn into an intact Eiscue
                // simply stayed in. rb1710 d18 / rb1410 d33 / rb1629 d31.
                match pivot {
                    Pivot::Target(t) => if sb.state.side(side).active().is_alive() {
                        emit_pivot_trailing_update(&mut sb);
                        let pre = sb.state;
                        apply_switch(&mut sb, side, t);
                        emit_switch_bracket(&mut sb, &pre, side, t);
                    },
                    Pivot::Pause => if sb.state.side(side).active().is_alive() && has_alive_bench(&sb.state, side) { push(&mut sb, Instruction::PivotPending { side }); },
                    Pivot::Stay => {}
                }
                out.push(sb);
            }
        }
        // The accuracy split's MISS branch belongs to this arm's return too — a nullifying
        // ability does not make the move stop rolling accuracy, and dropping the branch leaves the
        // seed gate with nothing to select when PS's roll was a miss.
        out.extend(miss_out);
        return apply_struggle_recoil(apply_recharge(out, side, move_id), side, struggling);
    }

    // Disguise: the first damaging hit on an un-busted Mimikyu is nullified (0 damage), busts it
    // into Mimikyu-Busted, and chips 1/8 of its max HP (PS `onDamage`+`onUpdate`). The move still
    // "connected", so a pivot user (U-turn) leaves and the move's PP was paid — but no damage,
    // secondary, or contact ability fires. Single-hit only (multi-hit busts on the first hit then
    // damages, unmodeled — no such matchup in the corpus).
    if def_ab == crate::ids::Ability::Disguise
        && matches!(md.category, MoveCategory::Physical | MoveCategory::Special)
        && defender.species == crate::ids::Species::from_id("mimikyu").unwrap_or(crate::ids::Species::None)
        && !defender.transformed
        && md.hits_max == 1
    {
        let mut hb = scaled(&b, hit_prob);
        // PS's Disguise is an `onDamage`/`onCriticalHit`/`onEffectiveness` block (identical shape to
        // Ice Face): getDamage still rolls the crit `randomChance(1, critMult)` and the damage
        // `random(16)` before onDamage zeroes it. Emit those two draw-and-discards so the from-seed
        // stream advances exactly as PS's does (a bare bust under-emitted 2 draws — e.g. U-turn into
        // an intact Mimikyu — desyncing every later damage roll).
        let crit_den = ps_crit_den(&b, side, &md);
        // **A draw-and-discard still has to record the REAL value.** The result is irrelevant to
        // the outcome — `onDamage` zeroes the damage whatever the crit and roll were — but the seed
        // gate does not "pick the closest branch": it draws from the real PRNG and keeps only the
        // branches whose RECORDED result equals what it drew (`seedgate.rs:330-352`). A hardcoded
        // 0 therefore kills the only live branch the moment the PRNG hands back anything else.
        // rb1710 d18: U-turn into an intact Ice Face Eiscue, PS's roll is 13, the engine's branch
        // claimed 0, and the unit desynced on a draw whose value cannot matter.
        emit_discarded_damage_rolls(&mut hb, crit_den);
        bust_disguise(&mut hb, foe);
        // A nullified hit is still a hit for Life Orb — see `apply_life_orb_recoil`.
        hb.after_hit_user_alive = hb.state.side(side).active().is_alive();
        apply_life_orb_recoil(&mut hb, side, &md);
        // Step 8, `onAfterHit`, still runs on a nullified hit — see `apply_knock_off_take_item`.
        if md.id.to_id() == "knockoff" {
            let alive = hb.after_hit_user_alive;
            apply_knock_off_take_item(&mut hb, side, def_ab, alive);
        }
        // `onDamage` returns 0, a NUMBER — the target stays live, so `spreadMoveHit` runs the
        // REST of its numbered steps, not just the secondaries. Step 4 is `selfDrops` — the
        // `move.self.boosts` payload with its `random(100)` — and it sits AHEAD of step 5's
        // `secondaries()`. Both this arm and the Ice Face arm above went straight to step 5.
        // rb1093 d22 t17: Ice Hammer into an intact Mimikyu — PS rolls `random[100]@icehammer`
        // and takes the user's Spe to −3; the engine made no roll and stopped at −2.
        let dropped: Vec<Branch> = apply_self_drop(hb, side, &md)
            .into_iter()
            .flat_map(|x| if external { vec![x] } else { start_rampage_lock(x, side, move_id) })
            .collect();
        for mut hb in dropped {
            apply_damage_secondaries(&mut hb, side, &md, false);
            for mut sb in apply_target_secondary(hb, side, &md)
                .into_iter()
                .flat_map(|x| apply_flinch_split(x, side, &md))
                .flat_map(|x| apply_cursed_body(x, side, &md))
                .flat_map(|x| apply_contact_secondaries(x, side, &md))
                .flat_map(|x| apply_source_damaging_hit(x, side, &md))
            {
                // Same two Updates as the Ice Face arm: `onDamage` returned the NUMBER 0, so the
                // target stayed in `targets` and PS ran the rest of `hitStepMoveHitLoop`.
                // rb1191 d17: a Thunderbolt busts an intact Mimikyu on a Speed-TIED board and PS
                // logs two `shuffle[2,0,2]@thunderbolt`; the engine logged one.
                emit_update_hit(&mut sb);
                emit_update(&mut sb);
                match pivot {
                    Pivot::Target(t) => if sb.state.side(side).active().is_alive() {
                        emit_pivot_trailing_update(&mut sb);
                        let pre = sb.state;
                        apply_switch(&mut sb, side, t);
                        emit_switch_bracket(&mut sb, &pre, side, t);
                    },
                    Pivot::Pause => if sb.state.side(side).active().is_alive() && has_alive_bench(&sb.state, side) { push(&mut sb, Instruction::PivotPending { side }); },
                    Pivot::Stay => {}
                }
                out.push(sb);
            }
        }
        // Same as the Ice Face arm above: keep the miss branch. rb1421 d26 — a 70-accuracy
        // Hurricane into an intact Mimikyu, where PS MISSED and the engine had generated no miss
        // outcome at all, so `replicate_select` fell through to the only branch there was and
        // busted the Disguise.
        out.extend(miss_out);
        return apply_struggle_recoil(apply_recharge(out, side, move_id), side, struggling);
    }

    // Each hit rolls damage (16) and crit independently. For small hit counts we enumerate
    // the full per-hit product (exact, and preserves Substitute/Sturdy interleaving). For
    // large counts (Population Bomb's 10 hits → 32¹⁰ branches) that explodes the allocator
    // and crashes the machine, so we instead enumerate the distinct *total* damage via a
    // sumset DP — same set of observable result states, bounded memory.
    let (hits_min, hits_max) = multihit_bounds(&b, side, &md);
    let hits_min = hits_min.max(1);
    let hits_max = hits_max.max(hits_min);
    // Beak Blast's `onHit` is PER HIT (`spreadMoveHit` step 3), so the realized multi-hit loops
    // need the charging flag inside the loop, not just at the post-hit-loop site below.
    let beak_blast_charging = foe_pending_move.is_some_and(|m| m.to_id() == "beakblast");
    // A fixed, small hit count keeps the exact per-hit product (preserves Substitute/Sturdy
    // interleaving). Variable hit counts ([2,5]) and large fixed counts (Population Bomb)
    // take the sumset-DP path, which also folds in the distribution over the hit count.
    //
    // Realized single-path executor: when a realized source is installed (seed gate / differ) and
    // this is a variable multi-hit move the DP can't stream (non-multiaccuracy [2,5], incl. Scale
    // Shot; multiaccuracy Population Bomb / Triple Axel route separately), draw the count + per-hit
    // rolls off the source and produce the one branch PS realized. Enumerate/Sample: no source → DP.
    let realized = if realized_multihit_move(&md) && !is_multiaccuracy_move(&md) && md.id.to_id() != "beatup" {
        realized_cursor(&b)
    } else {
        None
    };
    let damaged: Vec<(Branch, bool)> = if let Some(cur) = realized {
        apply_multihit_realized(&b, side, &md, hit_prob, cur, beak_blast_charging)
    } else if md.id.to_id() == "beatup" {
        // Beat Up: one hit per eligible party member with per-member base power. Realized single
        // path (seed gate / differ) draws each member's crit + damage off the source; Enumerate/
        // Sample stay on the sumset-DP convolution (no per-hit stream needed).
        if let Some(cur) = realized_cursor(&b) {
            let mds = beatup_mds(&b, side, &md);
            let calcs: Vec<DamageCalc> = mds.iter().map(|m| compute_damage(&b, side, m)).collect();
            apply_multihit_realized_ma(&b, side, &md, hit_prob, &calcs, &mds, cur, beak_blast_charging)
        } else {
            apply_beatup(&b, side, &md, hit_prob)
        }
    } else if let Some(fixed) = fixed_damage_amount(&md, &b.state, side) {
        // Fixed-damage moves (Night Shade / Seismic Toss = level, Dragon Rage = 40, ...) skip
        // the damage formula entirely: one deterministic outcome, no rolls or crit.
        let mut hb = scaled(&b, hit_prob);
        let calc = compute_damage(&hb, side, &md);
        let target_hp = hb.state.side(foe).active().hp;
        // A Substitute absorbs a FIXED-damage move exactly like any other. PS has no separate
        // path for these: the `substitute` volatile's `onTryPrimaryHit` (`data/moves.ts`) calls
        // `this.actions.getDamage(source, target, move)`, which is what runs the move's
        // `damageCallback` — so Super Fang still reads the TARGET's HP for its half, and the
        // resulting number is then subtracted from `substitute.hp`. This branch skipped the
        // routing every damage-formula path already had, so a fixed-damage move punched straight
        // through the Substitute and hit the mon.
        // rb1326 d50 t40: a Super Fang for 18 into a 66-HP Substitute — PS leaves the sub at 48
        // and the mon untouched at 37; the engine left the sub at 66, dropped the mon to 19 and
        // bumped its `times_hit` (the Rage Fist counter) to 2.
        let bypass_sub = md.flag_sound
            || hb.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
        let sub_hp = hb.state.side(foe).substitute_hp;
        if sub_hp > 0 && !bypass_sub && hb.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
            let sub_dmg = fixed.min(sub_hp);
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_dmg });
            if fixed >= sub_hp {
                push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
            }
            // `hits_landed` (the PS `timesAttacked` increment) is ZERO: `spreadMoveHit` rewrites a
            // sub-absorbed entry to `damage[i] = true; targets[i] = null`
            // (`sim/battle-actions.ts:1082-1085`), and the `timesAttacked` block at `:1011-1019`
            // is gated on BOTH a non-null target and `typeof moveDamage[i] === 'number'`.
            // Recoil and drain still apply — the Substitute's own `onTryPrimaryHit` runs them off
            // the sub damage (`data/moves.ts`, substitute lines 59-64) — so `any_damage` stays true.
            apply_post_damage(&mut hb, side, &md, sub_dmg as i32, true, true, 0, calc.life_orb, calc.def_item, calc.def_ability, false);
            vec![(hb, false)]
        } else {
            let mut dealt = fixed.min(target_hp);
            if (calc.def_ability == crate::ids::Ability::Sturdy || calc.def_item == Item::FocusSash)
                && target_hp == calc.def_maxhp && dealt >= target_hp
            {
                dealt = target_hp - 1;
                if calc.def_item == Item::FocusSash {
                    let slot = hb.state.side(foe).active_index;
                    push(&mut hb, Instruction::ChangeItem { side: foe, slot, previous: Item::FocusSash, new: Item::None });
                }
            }
            if dealt > 0 {
                let slot = hb.state.side(foe).active_index;
                push(&mut hb, Instruction::Damage { side: foe, slot, amount: dealt });
            }
            apply_post_damage(&mut hb, side, &md, dealt as i32, dealt > 0, false, (dealt > 0) as u8, calc.life_orb, calc.def_item, calc.def_ability, false);
            vec![(hb, false)]
        }
    } else if matches!(md.id.to_id(), "populationbomb")
        && realized_cursor(&b).is_some()
    {
        // Population Bomb (10 hits, multiaccuracy) can't enumerate its per-hit product — realize the
        // single branch off the source (per-hit accuracy + crit + damage, Loaded Dice count).
        let cur = realized_cursor(&b).unwrap();
        let calcs = vec![compute_damage(&b, side, &md)];
        apply_multihit_realized_ma(&b, side, &md, hit_prob, &calcs, std::slice::from_ref(&md), cur, beak_blast_charging)
    } else if matches!(md.id.to_id(), "tripleaxel" | "triplekick") {
        // Ascending power (20/40/60 or 10/20/30) with a fresh 90% accuracy check per hit;
        // a miss ends the move. hit_prob here is the single-hit accuracy.
        let step = md.base_power;
        let mds: Vec<crate::data::MoveData> = (1..=3u16)
            .map(|i| { let mut m = md; m.base_power = step * i; m })
            .collect();
        let calcs: Vec<DamageCalc> = mds.iter().map(|m| compute_damage(&b, side, m)).collect();
        if let Some(cur) = realized_cursor_per_hit(&b) {
            // Realized single path (seed gate / differ / SAMPLE): per-hit accuracy + crit + damage
            // off the source, KO-truncated. The enumerated branch below serves Enumerate only.
            apply_multihit_realized_ma(&b, side, &md, hit_prob, &calcs, &mds, cur, beak_blast_charging)
        } else {
            let crit_p = crit_chance(&b, side, &md);
            let acc = hit_prob;
            // The k=0 (full miss) case is the standard miss branch pushed earlier.
            let mut v = Vec::new();
            for k in 1..=3usize {
                let count_p = acc.powi(k as i32) * if k < 3 { 1.0 - acc } else { 1.0 };
                if count_p <= 0.0 {
                    continue;
                }
                for combo in HitCombos::new(k) {
                    let mut prob = count_p;
                    for &(_, crit) in &combo {
                        prob *= (1.0 / 16.0) * if crit { crit_p } else { 1.0 - crit_p };
                    }
                    if prob <= 0.0 {
                        continue;
                    }
                    let mut hb = scaled(&b, prob);
                    annotate_hits(&mut hb, &combo, ps_crit_den(&b, side, &md));
                    let hit_sub = apply_damage_hit_indexed(&mut hb, side, &md, &calcs, &combo);
                    v.push((hb, hit_sub));
                }
            }
            v
        }
    } else if hits_min == hits_max && hits_min <= MAX_EXACT_HITS {
        let mut v = Vec::new();
        let crit_p = crit_chance(&b, side, &md);
        let crit_den = ps_crit_den(&b, side, &md);
        // PS runs `runEvent('BasePower')` BETWEEN the crit roll and the damage roll
        // (battle-actions.ts:1653, "happens after crit calculation"). Fickle Beam's `onBasePower`
        // rolls `randomChance(3, 10)` there and `chainModify(2)` on a proc, so its draw sits INSIDE
        // the per-hit crit/damage pair — not before it and not after it — and the doubling feeds
        // that same hit's damage. Enumerate both outcomes; `apply_damage_hit` emits the draw in
        // position. (Fickle Beam is the only gen9 move with a random `onBasePower`.)
        let bp_outcomes: [(f32, Option<bool>); 2] = if md.id.to_id() == "ficklebeam" {
            [(0.70, Some(false)), (0.30, Some(true))]
        } else {
            [(1.0, None), (0.0, None)]
        };
        for (bp_p, bp_roll) in bp_outcomes {
            if bp_p <= 0.0 {
                continue;
            }
            let mut md_bp = md;
            if bp_roll == Some(true) {
                md_bp.base_power = md.base_power.saturating_mul(2);
            }
            for combo in HitCombos::new(hits_min) {
                let mut prob = hit_prob * bp_p;
                for &(_, crit) in &combo {
                    prob *= (1.0 / 16.0) * if crit { crit_p } else { 1.0 - crit_p };
                }
                if prob <= 0.0 {
                    continue;
                }
                let mut hb = scaled(&b, prob);
                // Per-hit crit+damage draws are emitted INSIDE the hit loop (KO-terminating), so a
                // multi-hit move that faints the target early stops rolling exactly where PS does
                // (`hitStepMoveHitLoop`: the top-of-loop `targets.every(!hp)` break precedes the
                // next hit's crit/damage rolls). See `apply_damage_hit`.
                let hit_sub = apply_damage_hit(&mut hb, side, &md_bp, &combo, crit_den, bp_roll);
                v.push((hb, hit_sub));
            }
        }
        v
    } else if ice_face_is_intact(&b, foe, &md) {
        apply_multihit_dp_ice_face(&b, side, &md, hits_min, hits_max, hit_prob)
    } else {
        apply_multihit_dp(&b, side, &md, hits_min, hits_max, hit_prob)
    };
    // PS `selfDrops` rolls one `random(100)` for a `move.self.boosts` payload and applies it only
    // when `self.chance` is undefined or the roll came in under it. Diamond Storm is the one gen9
    // move with a chance (50, +2 Def), so the drop has to FORK — hence the split out of
    // `apply_damage_secondaries` (which cannot branch): the roll and the boost live here, and
    // everything downstream sees the post-drop branch.
    let damaged: Vec<(Branch, bool)> = damaged
        .into_iter()
        .flat_map(|(hb, hit_sub)| {
            apply_self_drop(hb, side, &md)
                .into_iter()
                // Same `selfDrops` step: a rampage move's `self: {volatileStatus:'lockedmove'}`
                // (a Dancer copy — `external` — engages no lock).
                .flat_map(|x| if external { vec![x] } else { start_rampage_lock(x, side, move_id) })
                .map(move |x| (x, hit_sub))
                .collect::<Vec<_>>()
        })
        .collect();
    for (mut hb, hit_sub) in damaged {
        apply_damage_secondaries(&mut hb, side, &md, hit_sub);
        // Double Shock: the connecting hit strips the user's Electric type (PS `self.onHit`
        // `setType`, mapping Electric -> "???" typeless). A pure-Electric user becomes fully
        // typeless; Pawmot (Electric/Fighting) keeps only Fighting. Modeled as Type::None in the
        // stripped slot (the engine's typeless).
        // **`setType` REFUSES a terastallized user** — `sim/pokemon.ts`: "Terastallized Pokemon
        // cannot have their base type changed except via forme change", `if (this.terastallized)
        // return false`. The move itself still runs: its `onTryMove` gate is
        // `pokemon.hasType('Electric')`, and `hasType` goes through `getTypes()`, which
        // short-circuits on `terastallized` — so a Pawmot that Teras to ELECTRIC passes the gate,
        // deals its damage, and changes nothing. rb5189 d10 / rb5258 d19 are the same witness
        // twice: the engine's tera writes `types = [Electric, None]`, Double Shock then stripped
        // that to `[None, None]`, and because `types` reverts on switch-out only for a
        // NON-terastallized mon (see the `clearVolatile` note), the typeless pair survived on the
        // bench where PS had the species pair back.
        if md.id.to_id() == "doubleshock"
            && hb.state.side(side).active().is_alive()
            && !hb.state.side(side).active().terastallized
        {
            let p = hb.state.side(side).active();
            if p.types.contains(&Type::Electric) {
                let prev = p.types;
                let new = [
                    if prev[0] == Type::Electric { Type::None } else { prev[0] },
                    if prev[1] == Type::Electric { Type::None } else { prev[1] },
                ];
                let slot = hb.state.side(side).active_index;
                let prev_live = p.live_types;
                // Double Shock's `onHit` is `pokemon.setType(...)` — a real type change, so
                // PS's `types` array moves too. The terastallized case is excluded at the guard
                // above, NOT by the `hasType('Electric')` test, which a mon terastallized to
                // Electric passes.

                push(&mut hb, Instruction::ChangeTypes { side, slot, previous: prev, new });
                if prev_live != new {
                    push(&mut hb, Instruction::ChangeLiveTypes { side, slot, previous: prev_live, new });
                }
            }
        }
        // Weakness Policy on the target (super-effective hit), then White Herb if the user's
        // own self-drops (Leaf Storm, Close Combat, ...) left a negative stage.
        // Justified / Rattled / Thermal Exchange (onDamagingHit), Bug Bite's berry steal and the
        // frozen-target thaw (move onHit / frz onHit) don't fire when a Substitute took the hit:
        // the sub's `onTryPrimaryHit` eats the damage, `spreadMoveHit`'s `damage[i]` is 0, and
        // `runEvent('DamagingHit')` (`sim/battle-actions.ts:1142`) is gated on it. `apply_justified`
        // was outside the guard — rb1147 d38: a Keldeo-Resolute behind a fresh Substitute takes a
        // Knock Off; PS leaves its Attack at 0, the engine gave it Justified's +1.
        if !hit_sub {
            apply_bug_bite(&mut hb, side, &md);
            apply_thaw_on_hit(&mut hb, foe, &md);
            apply_spirit_shackle(&mut hb, side, &md);
            apply_sparkling_aria(&mut hb, side, &md);
        }
        // Stone Axe sets Stealth Rock on the target's side whether the hit landed on the mon
        // OR its Substitute (PS has both `onAfterHit` and `onAfterSubDamage`), as long as the
        // user is still standing. Glaive Rush's self-drawback likewise applies on any hit.
        //
        // "Still standing" means **at PS's `onAfterHit`**, which is inside `spreadMoveHit` and
        // therefore ahead of `move.recoil` and Life Orb — both of which the engine has already
        // applied by this point. rb1765 d6 t5: a 16-HP Life Orb Samurott-Hisui lands Ceaseless
        // Edge and dies to the orb; PS lays the Spikes first and the engine, reading a corpse,
        // laid none. See `Branch::after_hit_user_alive`.
        if md.id.to_id() == "stoneaxe" && hb.after_hit_user_alive {
            apply_hazard(&mut hb, foe, SideConditionId::StealthRock);
        }
        // Ceaseless Edge lays a layer of Spikes on the target's side on any hit (PS
        // `onAfterHit`/`onAfterSubDamage`), as long as the user is still standing.
        if md.id.to_id() == "ceaselessedge" && hb.after_hit_user_alive {
            apply_hazard(&mut hb, foe, SideConditionId::Spikes);
        }
        if md.id.to_id() == "glaiverush" && hb.after_hit_user_alive
            && !hb.state.side(side).volatiles.contains(VolatileStatus::GlaiveRush)
        {
            push(&mut hb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::GlaiveRush });
        }
        apply_relic_song_forme(&mut hb, side, &md);
        apply_throat_spray(&mut hb, side, &md);
        // `apply_spin_clear` USED to sit here. It is an `onAfterHit` payload (step 8) and it
        // removes the SPINNER's OWN side conditions — which is the one onAfterHit effect that can
        // be undone by a step-7 `onDamagingHit` handler laying a hazard on that same side.
        // rb1591 d19 t17: a Ditto transformed into Glimmora uses Mortal Spin into the real
        // Glimmora; PS's Toxic Debris (step 7) scatters a Toxic Spikes layer on the SPINNER's side
        // and the spin (step 8) then wipes it — net zero. The engine ran the spin first and kept
        // the layer. Moved to the step-7 boundary below, in both arms.
        apply_white_herb(&mut hb, side);
        // A Substitute blocks the target's own secondaries (boosts/status) and contact
        // abilities; otherwise split on the move's secondary, then the contact-status ability.
        // CONSUME the realized per-hit flag here: it is a per-MOVE transient (the branch is reused
        // for the turn's second mover, so a leftover `true` would silence that mon's own
        // once-per-move DamagingHit roll — rb1341 t13: Triple Axel, then Hyper Voice into the
        // Froslass whose Cursed Body PS still rolls).
        let per_hit_done = std::mem::replace(&mut hb.per_hit_procs_done, false);
        let branches = if hit_sub {
            // PS `spreadMoveHit` sets `targets[i] = null` on a Substitute hit (battle-actions.ts:1085),
            // and `secondaries()` skips only `target === false` (`null !== false`), so it STILL rolls
            // `this.random(100)` per secondary — the effect merely no-ops (moveHit on a null target).
            // `ModifySecondaries` runs on the (null) target, so Shield Dust / Covert Cloak do NOT
            // strip it here; only Sheer Force (which removed the secondaries upfront) suppresses the
            // roll. Emit those draw-and-discard rolls so the sub hit consumes the same stream PS does.
            emit_sub_secondary_rolls(&mut hb, side, &md);
            // Step 7 still closes the hit — the deferred no-draw event, gated on the sub.
            apply_damaging_hit_step7(&mut hb, side, &md, true);
            apply_spin_clear(&mut hb, side, &md);
            vec![hb]
        } else {
            apply_beak_blast_burn(&mut hb, side, &md, beak_blast_charging);
            apply_burning_jealousy(&mut hb, side, &md);
            // A realized multi-hit branch already fired the `DamagingHit` ability rolls per hit
            // (PS runs the event inside `spreadMoveHit`, once per connecting hit) — don't fire them
            // a second time here. The DP path never sets the flag and keeps the once-per-move
            // application, which is exact for the single-hit moves Enumerate/Sample verify.
            apply_target_secondary(hb, side, &md)
                .into_iter()
                .flat_map(|sb| apply_alluringvoice_confusion(sb, side, &md))
                .flat_map(|sb| apply_triattack_secondary(sb, side, &md))
                .flat_map(|sb| apply_direclaw_secondary(sb, side, &md))
                .flat_map(|sb| apply_partial_trap(sb, side, &md))
                // FLINCH IS A SECONDARY (`secondaries: [{volatileStatus:'flinch'}]`), so PS rolls
                // it in `secondaries()` — step 5 of `spreadMoveHit` — BEFORE the `DamagingHit` /
                // `SourceDamagingHit` ability rolls (Static / Flame Body / Poison Point / Poison
                // Touch / Toxic Chain / Cursed Body) that follow at step 7. rb1392 t2: Fake Out
                // into a Toxic Chain user — PS logs `random[100]@fakeout` then
                // `randomChance[3,10]` with `event: DamagingHit`.
                .flat_map(|sb| apply_flinch_split(sb, side, &md))
                // ---- step 5 ends, step 7 begins ----
                // The last connecting hit's deferred `runEvent('DamagingHit')`: the no-draw
                // handlers (Rough Skin / Rocky Helmet chip, Stamina, Water Compaction, Seed Sower,
                // Toxic Debris, Gooey, Justified, Rattled, Thermal Exchange, Weak Armor) land HERE,
                // on the post-secondary board, and ahead of the drawing handlers below because PS
                // orders Rough Skin / Iron Barbs (`onDamagingHitOrder: 1`) and Rocky Helmet (2)
                // ahead of the unordered contact-status set — a chip that faints the attacker must
                // suppress its paralysis/burn. See `apply_damaging_hit_step7`.
                // ---- step 7 ends, step 8 (`onAfterHit`) begins ----
                .map(|mut sb| {
                    apply_damaging_hit_step7(&mut sb, side, &md, false);
                    apply_spin_clear(&mut sb, side, &md);
                    // `hitStepMoveHitLoop`'s trailing `afterMoveSecondaryEvent` — the frz thaw of
                    // a `thawsTarget` move lives HERE, after the secondary that would have burned.
                    apply_thaw_after_secondary(&mut sb, side, foe, &md);
                    sb
                })
                .flat_map(|sb| if per_hit_done { vec![sb] } else { apply_cursed_body(sb, side, &md) })
                .flat_map(|sb| if per_hit_done { vec![sb] } else { apply_contact_secondaries(sb, side, &md) })
                .flat_map(|sb| if per_hit_done { vec![sb] } else { apply_source_damaging_hit(sb, side, &md) })
                .collect::<Vec<_>>()
        };
        for mut sb in branches {
            // Weakness Policy is `onDamagingHit` (data/items.ts:7591-7605) — step 7 of
            // `spreadMoveHit`, AFTER `secondaries()` at step 5. Applying it earlier made its
            // +2 Atk/+2 SpA visible to a secondary that reads `target.statsRaisedThisTurn`
            // (Alluring Voice, Burning Jealousy): rb1178 t11 — PS does NOT confuse the
            // Weakness-Policy Tyranitar that Alluring Voice just boosted.
            apply_weakness_policy(&mut sb, foe, &md);
            // ---- step 7 ends, step 8 (`onAfterHit`) begins ----
            // Knock Off's `takeItem` is step 8 and its guard is `pokemon.hp` — the ATTACKER's,
            // read AFTER `runEvent('DamagingHit')` has already chipped it. The realized executors
            // defer that event to the boundary just above, so this is the only line in the move
            // pipeline where the guard can be asked at PS's position; `apply_post_damage` skips
            // the take for them (`!per_hit_done`) and `step8_user_alive` credits back the Life Orb
            // / recoil it front-loaded. rb5164 d44 t35: a 9-HP Okidogi Knock Offs a Rocky Helmet
            // Amoonguss and the 1/6 chip kills it — PS's guard is false and the helmet stays.
            //
            // It sits AFTER `apply_weakness_policy` (step 7 consumes the policy before step 8 can
            // knock it away — rb1544 t14 / rb1447 t25) and BEFORE `apply_pinch_berry` (step 8
            // beats the 970 Update, so a Sitrus holder knocked below half loses the berry rather
            // than eating it — rb1383 t15).
            let alive = step8_user_alive(&sb, side);
            let dealt = sb.move_any_damage;
            apply_after_hit_item_moves(&mut sb, side, &md, def_ab, dealt, hit_sub, alive);
            // In-kernel Update shuffles for this connecting hit, in PS order (after `spreadMoveHit`
            // = self-drops + target secondaries + DamagingHit contact abilities have all rolled):
            //   970  per-hit `eachEvent('Update')` — fires on the PRE-faint board (a target
            //        reduced to 0 HP this hit is still in getAllActive), so a KO still shuffles.
            //   1024 post-hit-loop `eachEvent('Update')` — fires once for the move but AFTER
            //        faintMessages, so a KO'd (now-fainted) target breaks the tie and it doesn't.
            // Both emitted before the pivot/drag switch changes the on-field mon (and its Speed).
            //
            // The HP berries ARE that 970 Update's payload: `eachEvent('Update')` runs each
            // active's item `onUpdate`, and it sits after the WHOLE `spreadMoveHit` — damage,
            // `onHit`, self-drops, secondaries, `DamagingHit`, `onAfterHit` — so a berry decision
            // reads the post-secondary state. Psychic Noise that heal-blocks the target in the
            // same hit that drops it under half therefore keeps the berry (`sitrusberry
            // .onTryEatItem` is gated by Heal Block, data/items.ts:5752). The pinch-berry check
            // used to sit ahead of the secondary split, which ate it.
            apply_pinch_berry(&mut sb, foe);
            apply_pinch_berry(&mut sb, side);
            // Lum / Chesto are `onUpdate` handlers too (`data/items.ts` lumberry / chestoberry),
            // so the SAME `eachEvent('Update')` cures a status this hit inflicted — on EITHER
            // active. The engine cured only at the individual status-application sites, which
            // between them miss the ones that status the ATTACKER: rb1204 d2, a Vileplume's
            // Effect Spore paralyses the Outraging Flygon at step 7 and PS's Lum Berry wipes
            // it here; the engine kept both the paralysis and the berry.
            consume_lum_if_statused(&mut sb, foe);
            consume_lum_if_statused(&mut sb, side);
            emit_update_hit(&mut sb);
            emit_update(&mut sb);
            // Pivot move (U-turn): switch the user out now that it connected. PS fires the move
            // action's runAction Update (2882) BEFORE processing the self-switch, so emit it here on
            // the PRE-switch board (user still in) and mark it done so run_move_action doesn't re-emit.
            match pivot {
                Pivot::Target(t) => {
                    if sb.state.side(side).active().is_alive() {
                        emit_pivot_trailing_update(&mut sb);
                        let pre = sb.state;
                        apply_switch(&mut sb, side, t);
                        emit_switch_bracket(&mut sb, &pre, side, t);
                    }
                }
                Pivot::Pause => {
                    if sb.state.side(side).active().is_alive() && has_alive_bench(&sb.state, side) {
                        push(&mut sb, Instruction::PivotPending { side });
                    }
                }
                Pivot::Stay => {}
            }
            // Dragon Tail / Circle Throw: the survivor is dragged out (uniform over the bench).
            // Only a connecting hit drags — a MISS (which sits in `out` from the accuracy split
            // above) must leave the target in place, so the drag lives on the hit branches here.
            //
            // **A SUBSTITUTE-absorbed hit never phazes.** `spreadMoveHit`'s step 0 sets
            // `targets[i] = null` on `HIT_SUBSTITUTE` (`battle-actions.ts:1083-1085`), and step 6's
            // `forceSwitch` (`:1125`, `:1377`) iterates `targets` and skips the null — the same
            // nulling that already suppresses `onHit`, the self-drops and the secondaries here.
            // The engine dragged anyway and, worse, EMITTED the `sample[5]@drag` for it: rb1760 d7
            // t8, a Dragon Tail into a Substitute the target made the same turn, was the corpus's
            // only `rust extra` on the seed rail.
            if md.force_switch && !hit_sub {
                out.extend(apply_drag(sb, foe));
            } else {
                out.push(sb);
            }
        }
    }
    // (Drag routing for force-switch moves happens per-connecting-hit above — misses never
    // phaze.) A Dancer copy (external) engages neither the rampage lock nor Dancer again.
    let out = if external { out } else { apply_rampage_state(out, side, move_id) };
    let mut out = apply_recharge(out, side, move_id);
    out.extend(miss_out);
    let out = apply_struggle_recoil(out, side, struggling);
    // Dancer: the foe immediately copies a successfully used dance move (miss branches for
    // dance moves don't exist — all are 100/-- accuracy — and failure paths return earlier).
    if !external && is_dance_move(move_id) {
        apply_dancer_copies(out, side, move_id)
    } else {
        out
    }
}

/// Annotate the per-hit PRNG draws PS makes inside the hit loop, in order: for each hit, the
/// crit roll `randomChance(1, critDen)` (only when the crit chance is a genuine coin — a
/// guaranteed crit via `willCrit`/high-crit-stage and a crit-immune target both skip PS's
/// roll) followed by the damage `random(16)`. The engine's roll index equals PS's `random(16)`
/// value by construction (`damage_rolls[roll]` uses factor `(100-roll)/100`), and results are
/// validated against `stateAfter`, so only kind/args/order/count are load-bearing.
fn annotate_hits(hb: &mut Branch, combo: &[(u8, bool)], crit_den: i32) {
    if !annotating() {
        return;
    }
    for (i, &(roll, crit)) in combo.iter().enumerate() {
        // The PREVIOUS hit's `eachEvent('Update')` (970) — one per loop iteration, and the last
        // one is `execute_move_inner`'s trailing `emit_update_hit`. See `emit_prev_hit_update`.
        if i >= 1 {
            emit_prev_hit_update(hb, None);
        }
        if crit_den > 0 {
            // PS rolls once per hit whenever `willCrit` is undefined (crit_den > 0), even against a
            // crit-immune target (the realized `crit` is already false on that branch — the roll is
            // draw-and-discard). `result` isn't compared by the differ; the state validates it.
            draw(hb, "randomChance", &[1, crit_den], crit as i64, "crit");
        }
        draw(hb, "random", &[16], roll as i64, "damage-roll");
        // ModifyDamage screen-tie shuffle (per getDamage, after the damage roll).
        emit_modifydamage_shuffle(hb);
    }
}

/// Iterator over all per-hit (roll 0..16, crit bool) combinations for `hits` hits.
struct HitCombos {
    hits: usize,
    idx: u64,
    total: u64,
}
impl HitCombos {
    fn new(hits: usize) -> Self {
        HitCombos { hits, idx: 0, total: 32u64.pow(hits as u32) }
    }
}
impl Iterator for HitCombos {
    type Item = Vec<(u8, bool)>;
    fn next(&mut self) -> Option<Vec<(u8, bool)>> {
        if self.idx >= self.total {
            return None;
        }
        let mut n = self.idx;
        let mut combo = Vec::with_capacity(self.hits);
        for _ in 0..self.hits {
            let per = (n % 32) as u8; // 0..32: low 4 bits = roll, bit 5 = crit
            combo.push((per & 0x0F, per & 0x10 != 0));
            n /= 32;
        }
        self.idx += 1;
        Some(combo)
    }
}

/// Compute a move's damage rolls (non-crit and crit) and the defender-side fields needed
/// after application. Pure with respect to `b` (reads state, mutates nothing) so both the
/// exact per-hit path and the sumset-DP path can call it once and share the result.
/// Does Mold Breaker / Teravolt / Turboblaze (and `move.ignoreAbility`) suppress this ability?
///
/// **`flags: { breakable: 1 }` in `data/abilities.ts` is the whole rule.** PS `sim/battle.ts:836`:
/// `if (effect.effectType === 'Ability' && effect.flags['breakable'] &&
/// this.suppressingAbility(effectHolder)) continue;`, and `suppressingAbility` (`:365`) is just
/// "there is an active move and it has `ignoreAbility`". Everything else is untouched — the engine
/// used to blank the defender's ability wholesale, which silently deleted the abilities PS
/// deliberately left OUT of the flag:
///
/// * **Shadow Shield** (`flags: {}`) is not Multiscale (`breakable: 1`) — rb1612, a Mold Breaker
///   Haxorus's Iron Head into a full-HP Lunala, where PS still halved the damage.
/// * **the four Ruin abilities** (`flags: {}`) — rb1588, a Mold Breaker Excadrill's Iron Head into
///   Wo-Chien, where PS still applied Tablets of Ruin's ×0.75 to Excadrill's Attack.
/// * **Prism Armor** (`flags: {}`) is not Filter / Solid Rock (`breakable: 1`); no corpus witness
///   yet, same class.
///
/// The list is the 83 `breakable: 1` abilities of the pinned dex, minus `mountaineer` and
/// `rebound` (CAP-only, absent from the engine's `Ability` enum).
fn ability_breakable(a: crate::ids::Ability) -> bool {
    use crate::ids::Ability as Ab;
    matches!(
        a,
        Ab::ArmorTail | Ab::AromaVeil | Ab::AuraBreak | Ab::BattleArmor | Ab::BigPecks |
        Ab::Bulletproof | Ab::ClearBody | Ab::Contrary | Ab::Damp | Ab::Dazzling | Ab::Disguise |
        Ab::DrySkin | Ab::EarthEater | Ab::Filter | Ab::FlashFire | Ab::FlowerGift | Ab::FlowerVeil |
        Ab::Fluffy | Ab::FriendGuard | Ab::FurCoat | Ab::GoodAsGold | Ab::GrassPelt | Ab::GuardDog |
        Ab::Heatproof | Ab::HeavyMetal | Ab::HyperCutter | Ab::IceFace | Ab::IceScales |
        Ab::Illuminate | Ab::Immunity | Ab::InnerFocus | Ab::Insomnia | Ab::KeenEye | Ab::LeafGuard |
        Ab::Levitate | Ab::LightMetal | Ab::LightningRod | Ab::Limber | Ab::MagicBounce |
        Ab::MagmaArmor | Ab::MarvelScale | Ab::MindsEye | Ab::MirrorArmor | Ab::MotorDrive |
        Ab::Multiscale | Ab::Oblivious | Ab::Overcoat | Ab::OwnTempo | Ab::PastelVeil | Ab::PunkRock |
        Ab::PurifyingSalt | Ab::QueenlyMajesty | Ab::SandVeil | Ab::SapSipper | Ab::ShellArmor |
        Ab::ShieldDust | Ab::Simple | Ab::SnowCloak | Ab::SolidRock | Ab::Soundproof | Ab::StickyHold |
        Ab::StormDrain | Ab::Sturdy | Ab::SuctionCups | Ab::SweetVeil | Ab::TangledFeet |
        Ab::Telepathy | Ab::TeraShell | Ab::ThermalExchange | Ab::ThickFat | Ab::Unaware |
        Ab::VitalSpirit | Ab::VoltAbsorb | Ab::WaterAbsorb | Ab::WaterBubble | Ab::WaterVeil |
        Ab::WellBakedBody | Ab::WhiteSmoke | Ab::WindRider | Ab::WonderGuard | Ab::WonderSkin
    )
}

/// Does `side`'s move set PS's `move.ignoreAbility` — i.e. does it suppress the TARGET's
/// `breakable` abilities while it resolves? Mold Breaker / Teravolt / Turboblaze on the user, or a
/// move with its own `ignoreAbility` (Sunsteel Strike / Moongeist Beam / Photon Geyser). Mycelium
/// Might is deliberately absent: it sets the flag only for the user's STATUS moves, and the one
/// status site that needs it already folds it into its own `status_breaker`.
fn move_breaks_abilities(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    use crate::ids::Ability as Ab;
    matches!(b.state.side(side).active().ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze)
        || move_ignores_ability(md.id)
}

fn compute_damage(b: &Branch, side: SideId, md: &crate::data::MoveData) -> DamageCalc {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let attacker = b.state.side(side).active();
    let defender = b.state.side(foe).active();
    // Mold Breaker suppresses the defender's damage-affecting ability for this move — but **only
    // if it is `breakable`**. `def_ab` is the defender's ability as the damage calc should see it.
    let mb = matches!(attacker.ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze)
        || move_ignores_ability(md.id);
    let def_ab = if mb && ability_breakable(defender.ability) { Ab::None } else { defender.ability };

    // Foul Play uses the defender's Attack stat and Attack boost (attacker's burn still
    // applies; Unaware on the defender ignores... the defender's own boost is used as-is).
    let foul_play = md.id.to_id() == "foulplay";
    // Choose offensive / defensive stats (Body Press uses Defense to attack).
    let atk_idx = if md.uses_defense_as_attack {
        crate::ids::StatIndex::Defense
    } else if md.category == MoveCategory::Physical {
        crate::ids::StatIndex::Attack
    } else {
        crate::ids::StatIndex::SpecialAttack
    };
    let atk_boost_idx = match atk_idx {
        crate::ids::StatIndex::Defense => BoostIndex::Defense,
        crate::ids::StatIndex::SpecialAttack => BoostIndex::SpecialAttack,
        _ => BoostIndex::Attack,
    };
    // A few special moves have `overrideDefensiveStat: 'def'` (Psyshock, Psystrike, Secret
    // Sword): they are category Special (so Light Screen, not Reflect, halves them and the
    // attacker's SpA is used) but they hit the target's physical Defense stat and its Def boost.
    let overrides_def = matches!(md.id.to_id(), "psyshock" | "psystrike" | "secretsword");
    let (def_idx, def_boost_idx) = if md.category == MoveCategory::Physical || overrides_def {
        (crate::ids::StatIndex::Defense, BoostIndex::Defense)
    } else {
        (crate::ids::StatIndex::SpecialDefense, BoostIndex::SpecialDefense)
    };

    // Ruin abilities (gen9): each lowers one stat of all OTHER active mons by 25%.
    let tablets = def_ab == Ab::TabletsOfRuin && attacker.ability != Ab::TabletsOfRuin;
    let vessel = def_ab == Ab::VesselOfRuin && attacker.ability != Ab::VesselOfRuin;
    let sword = attacker.ability == Ab::SwordOfRuin && def_ab != Ab::SwordOfRuin;
    let beads = attacker.ability == Ab::BeadsOfRuin && def_ab != Ab::BeadsOfRuin;

    // Unaware: the attacker ignores the defender's defensive boosts, and a defender with
    // Unaware ignores the attacker's offensive boosts.
    let atk_boost = if def_ab == Ab::Unaware {
        0
    } else if foul_play {
        b.state.side(foe).boost(atk_boost_idx)
    } else {
        b.state.side(side).boost(atk_boost_idx)
    };
    let def_boost = if attacker.ability == crate::ids::Ability::Unaware || move_ignores_defensive(md.id) {
        0
    } else {
        b.state.side(foe).boost(def_boost_idx)
    };

    // Protosynthesis / Quark Drive on the boosted offensive / defensive stat. PS uses
    // chainModify([5325, 4096]) — modifier 5325, NOT 13/10 (which rounds to 5324).
    // Offensive modifiers run on the *category* stat event (ModifyAtk for physical,
    // ModifySpA for special) regardless of `overrideOffensiveStat` — so Body Press
    // (physical, reads Defense) is boosted by an 'atk' best-stat. We therefore compare
    // proto_stat to the category offensive stat, not the stat actually read.
    let category_off_stat = if md.category == MoveCategory::Physical {
        crate::ids::StatIndex::Attack
    } else {
        crate::ids::StatIndex::SpecialAttack
    };
    let proto_atk = has_proto(b.state.side(side)) && proto_stat(attacker) == category_off_stat;
    let proto_def = has_proto(b.state.side(foe)) && proto_stat(defender) == def_idx;
    let pinch = (attacker.hp as i32) * 3 <= attacker.max_hp as i32; // HP ≤ 1/3

    // Both stats are computed via closures so the crit branch can re-derive them with the
    // boost clamped (a crit ignores the attacker's *negative* offensive boost and the
    // defender's *positive* defensive boost) while reusing the identical modifier chain.
    let finalize_atk = |boost: i8| -> i64 {
        let atk_owner = if foul_play { defender } else { attacker };
        let mut amod = 4096i64;
        let atk_stat = boosted_stat(atk_owner.stat(atk_idx) as i64, boost);
        // Heatproof (defender): incoming Fire damage halved via the offensive stat
        // (PS `onSourceModifyAtk`/`onSourceModifySpA` chainModify(0.5)).
        if def_ab == Ab::Heatproof && md.typ == Type::Fire {
            amod = crate::damage::chain(amod, 1, 2);
        }
        if tablets && md.category == MoveCategory::Physical {
            amod = crate::damage::chain(amod, 3, 4);
        }
        if vessel && md.category == MoveCategory::Special {
            amod = crate::damage::chain(amod, 3, 4);
        }
        // Item stat modifiers (PS applies these via `modify`, round-half-up).
        match (attacker.item, md.category) {
            (Item::ChoiceBand, MoveCategory::Physical) => amod = crate::damage::chain(amod, 3, 2),
            (Item::ChoiceSpecs, MoveCategory::Special) => amod = crate::damage::chain(amod, 3, 2),
            _ => {}
        }
        // Light Ball doubles both offensive stats for every Pikachu forme. PS keys this on
        // `baseSpecies.baseSpecies === Pikachu`; all generated forme ids share the prefix.
        if attacker.item == Item::LightBall && attacker.species.to_id().starts_with("pikachu") {
            amod = crate::damage::chain(amod, 2, 1);
        }
        // Purifying Salt halves the attacker's offensive stat vs Ghost moves (onSourceModify
        // Atk/SpA chainModify(0.5)) — NOT the final damage, so the rounding point matters.
        if def_ab == Ab::PurifyingSalt && md.typ == Type::Ghost {
            amod = crate::damage::chain(amod, 1, 2);
        }
        if proto_atk {
            amod = crate::damage::chain(amod, 5325, 4096);
        }
        // Orichalcum Pulse (physical Atk in sun) / Hadron Engine (special SpA in Electric
        // Terrain): ×5461/4096 — PS onModifyAtk / onModifySpA.
        if attacker.ability == Ab::OrichalcumPulse
            && md.category == MoveCategory::Physical
            && matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun)
        {
            amod = crate::damage::chain(amod, 5461, 4096);
        }
        if attacker.ability == Ab::HadronEngine
            && md.category == MoveCategory::Special
            && b.state.terrain == crate::ids::Terrain::Electric
        {
            amod = crate::damage::chain(amod, 5461, 4096);
        }
        // Offensive ability multipliers.
        match attacker.ability {
            Ab::HugePower | Ab::PurePower => amod = crate::damage::chain(amod, 2, 1),
            Ab::Guts if attacker.status != Status::None => amod = crate::damage::chain(amod, 3, 2),
            Ab::SlowStart if md.category == MoveCategory::Physical && b.state.side(side).active_turns <= 5 => {
                amod = crate::damage::chain(amod, 1, 2)
            }
            Ab::Overgrow if md.typ == Type::Grass && pinch => amod = crate::damage::chain(amod, 3, 2),
            Ab::Blaze if md.typ == Type::Fire && pinch => amod = crate::damage::chain(amod, 3, 2),
            Ab::Torrent if md.typ == Type::Water && pinch => amod = crate::damage::chain(amod, 3, 2),
            Ab::Swarm if md.typ == Type::Bug && pinch => amod = crate::damage::chain(amod, 3, 2),
            // Sheer Force: ×1.3 when the move has a secondary (the secondary is then removed).

            Ab::Defeatist if (attacker.hp as i32) * 2 <= attacker.max_hp as i32 => amod = crate::damage::chain(amod, 1, 2),
            // Sharpness is handled as a base-power modifier below (PS `onBasePower`), not here —
            // its ×1.5 placement matters once a ×0.5 type multiplier is in play (cosim caught a
            // Ceaseless Edge unit whose rounding only matched with the base-power floor).
            // Punk Rock is handled as a base-power modifier below (PS `onBasePower`), not here.
            Ab::Hustle if md.category == MoveCategory::Physical => amod = crate::damage::chain(amod, 3, 2),
            // Type-boosting abilities (applied to the offensive stat like the others above).
            // Stakeout: ×2 offensive stat vs a target that switched in this turn (activeTurns==0).
            // PS onModifyAtk / onModifySpA chainModify(2).
            Ab::Stakeout if b.state.side(foe).active_turns == 0 => amod = crate::damage::chain(amod, 2, 1),
            Ab::WaterBubble if md.typ == Type::Water => amod = crate::damage::chain(amod, 2, 1),
            Ab::Transistor if md.typ == Type::Electric => amod = crate::damage::chain(amod, 5325, 4096), // ×1.3
            Ab::DragonsMaw if md.typ == Type::Dragon => amod = crate::damage::chain(amod, 3, 2),
            Ab::RockyPayload if md.typ == Type::Rock => amod = crate::damage::chain(amod, 3, 2),
            Ab::Steelworker if md.typ == Type::Steel => amod = crate::damage::chain(amod, 3, 2),
            // Flash Fire: ×1.5 to the holder's Fire moves once activated (the `flashfire`
            // volatile is present). PS `onModifyAtk`/`onModifySpA` in the ability's condition.
            Ab::FlashFire
                if md.typ == Type::Fire
                    && b.state.side(side).volatiles.contains(VolatileStatus::FlashFire) =>
            {
                amod = crate::damage::chain(amod, 3, 2)
            }
            _ => {}
        }
        // Thick Fat (defender) halves the attack of Fire/Ice moves; Water Bubble (defender)
        // halves Fire-move attack.
        if def_ab == Ab::ThickFat && (md.typ == Type::Fire || md.typ == Type::Ice) {
            amod = crate::damage::chain(amod, 1, 2);
        }
        if def_ab == Ab::WaterBubble && md.typ == Type::Fire {
            amod = crate::damage::chain(amod, 1, 2);
        }
        crate::damage::modify_by(atk_stat, amod)
    };
    let finalize_def = |boost: i8| -> i64 {
        let mut dmod = 4096i64;
        let mut def_stat = boosted_stat(defender.stat(def_idx) as i64, boost);
        // Ruin abilities and Assault Vest key on the defensive STAT actually used (Def vs SpD),
        // not the move category — so an overrideDefensiveStat move (Psyshock) is treated by its
        // physical Defense: Sword of Ruin / Fur Coat / Snow / Marvel Scale apply, while Beads of
        // Ruin / Assault Vest (SpD modifiers) do not.
        if sword && def_idx == crate::ids::StatIndex::Defense {
            dmod = crate::damage::chain(dmod, 3, 4);
        }
        if beads && def_idx == crate::ids::StatIndex::SpecialDefense {
            dmod = crate::damage::chain(dmod, 3, 4);
        }
        if defender.item == Item::AssaultVest && def_idx == crate::ids::StatIndex::SpecialDefense {
            dmod = crate::damage::chain(dmod, 3, 2);
        }
        // Eviolite: ×1.5 to the defensive stat (Def and SpD) of a not-fully-evolved Pokémon.
        if defender.item == Item::Eviolite && crate::data::species_is_nfe(defender.species) {
            dmod = crate::damage::chain(dmod, 3, 2);
        }
        if proto_def {
            dmod = crate::damage::chain(dmod, 5325, 4096);
        }
        // Weather defensive boosts: Sandstorm ×1.5 SpD for Rock types, Snow ×1.5 Def for Ice.
        // The two weather defensive boosts are the odd ones out: `data/conditions.ts`
        // `sandstorm.onModifySpD` / `snow.onModifyDef` RETURN `this.modify(spd, 1.5)` instead of
        // calling `chainModify`, so they round on their own, in handler-priority order, against
        // the relayVar — they are NOT part of the accumulated `event.modifier`.
        if b.state.weather == Weather::Sand
            && def_idx == crate::ids::StatIndex::SpecialDefense
            && defender.types.contains(&Type::Rock)
        {
            def_stat = crate::damage::modify(def_stat, 3, 2);
        }
        if b.state.weather == Weather::Snow
            && def_idx == crate::ids::StatIndex::Defense
            && defender.types.contains(&Type::Ice)
        {
            def_stat = crate::damage::modify(def_stat, 3, 2);
        }
        // Marvel Scale / Fur Coat (defender) raise physical Defense. Both are `onModifyDef`
        // (data/abilities.ts `furcoat` / `marvelscale`), and `Pokemon#calculateStat` runs
        // `Modify<Stat>` for whichever stat the move actually reads — so an
        // `overrideDefensiveStat` special move (Secret Sword, Psyshock, Psystrike) gets them
        // too. Key on the STAT, never on the move category (rb1123 t2: Secret Sword into a
        // Fur Coat Persian-Alola — 47 in PS, 94 in the engine, exactly the missing ×2).
        if def_idx == crate::ids::StatIndex::Defense {
            if def_ab == Ab::FurCoat {
                dmod = crate::damage::chain(dmod, 2, 1);
            } else if def_ab == Ab::MarvelScale && defender.status != Status::None {
                dmod = crate::damage::chain(dmod, 3, 2);
            }
        }
        crate::damage::modify_by(def_stat, dmod)
    };
    // Supreme Overlord is applied to base power below (PS uses an exact lookup table).
    let atk_stat = finalize_atk(atk_boost);
    let def_stat = finalize_def(def_boost);
    // Crit-clamped stats: ignore the attacker's negative offensive boost and the defender's
    // positive defensive boost (PS `getDamage` with crit/ignore flags).
    let atk_stat_crit = finalize_atk(atk_boost.max(0));
    let def_stat_crit = finalize_def(def_boost.min(0));
    // Guts ignores the burn attack drop.
    let burned = attacker.status == Status::Burn && attacker.ability != Ab::Guts;

    // Final damage modifiers use PS's 4096-based `chainModify`: combine each modifier with
    // round-half-up, then apply the resulting modifier to damage once. Sequentially modifying
    // damage (or multiplying rational numerators) gives different support when effects stack.
    let chain_final = |previous: i64, next: i64| (previous * next + 2048) >> 12;
    let mut fmod = 4096i64;
    if matches!(def_ab, Ab::Multiscale | Ab::ShadowShield) && defender.hp == defender.max_hp {
        fmod = chain_final(fmod, 2048);
    }
    // Glaive Rush drawback: the user takes double damage until its next move attempt
    // (PS condition `onSourceModifyDamage` chainModify(2)).
    if b.state.side(foe).volatiles.contains(VolatileStatus::GlaiveRush) {
        fmod = chain_final(fmod, 8192);
    }
    let scrappy = matches!(attacker.ability, Ab::Scrappy | Ab::MindsEye);
    let def_types_eff = effective_def_types(scrappy, md.typ, defender.types);
    let type_mult = crate::damage::type_multiplier_fd(md.typ, def_types_eff, is_freeze_dry(md));
    if matches!(def_ab, Ab::Filter | Ab::SolidRock | Ab::PrismArmor) && type_mult > 1.0 {
        fmod = chain_final(fmod, 3072);
    }
    if def_ab == Ab::IceScales && md.category == MoveCategory::Special {
        fmod = chain_final(fmod, 2048);
    }
    // Punk Rock halves sound-move damage taken.
    if def_ab == Ab::PunkRock && md.flag_sound {
        fmod = chain_final(fmod, 2048);
    }
    // Fluffy (defender, breakable): ×2 damage from Fire moves, ÷2 from contact moves. Both
    // may apply at once (a Fire contact move nets ×1). PS `onSourceModifyDamage`.
    if def_ab == Ab::Fluffy {
        if md.typ == Type::Fire {
            fmod = chain_final(fmod, 8192);
        }
        if md.flag_contact {
            fmod = chain_final(fmod, 2048);
        }
    }
    // Attacker final-damage modifiers keyed on effectiveness / item.
    if attacker.ability == Ab::TintedLens && type_mult < 1.0 {
        fmod = chain_final(fmod, 8192);
    }
    if attacker.ability == Ab::Neuroforce && type_mult > 1.0 {
        fmod = chain_final(fmod, 5120);
    }
    if attacker.item == Item::ExpertBelt && type_mult > 1.0 {
        fmod = chain_final(fmod, 4915);
    }
    if attacker.item == Item::MuscleBand && md.category == MoveCategory::Physical {
        fmod = chain_final(fmod, 4505);
    }
    if attacker.item == Item::WiseGlasses && md.category == MoveCategory::Special {
        fmod = chain_final(fmod, 4505);
    }
    let adaptability = attacker.ability == Ab::Adaptability;
    // Tera Shell: Terapagos-Terastal at full HP resists every hit by one extra step (breakable,
    // so `def_ab` already reflects Mold Breaker suppressing it).
    // PS's guard set (`sim/pokemon.ts:2224`) is `category === 'Status' || id === 'struggle' ||
    // !runImmunity(move) || totalTypeMod < 0 || hp < maxhp`. The first two are excluded here, the
    // immunity one is moot (an immune move deals nothing anyway) and the `totalTypeMod < 0` one
    // lives in `damage.rs` where the chart result is known.
    let tera_shell = def_ab == Ab::TeraShell
        && defender.hp == defender.max_hp
        && md.category != MoveCategory::Status
        && md.id.to_id() != "struggle"
        && crate::ids::Species::from_id("terapagosterastal") == Some(defender.species);
    // Returned for post-damage (contact punishers); also suppressed under Mold Breaker.
    let def_ability = def_ab;
    let def_item = defender.item;
    let def_maxhp = defender.max_hp;
    let sheer_force_active = attacker.ability == Ab::SheerForce
        && (md.secondary_chance > 0 || md.flinch_chance > 0
            || md.secondary_self_boosts.iter().any(|&x| x != 0)
            // Tri Attack's secondary is a sample-based onHit that the move table can't encode,
            // so it isn't reflected in `secondary_chance`. Throat Chop's is the same shape —
            // `secondary: {chance: 100, onHit(target) { target.addVolatile('throatchop') }}` —
            // so it too earns Sheer Force's ×1.3 (rb1072 d27: PS's Sheer Force Tauros hits Iron
            // Thorns for 73, the engine for 56, exactly the missing 5325/4096).
            || matches!(md.id.to_id(), "triattack" | "throatchop"));
    // Life Orb's ×1.3 DAMAGE (onModifyDamage) always applies while held; Sheer Force only
    // suppresses the RECOIL (onAfterMoveSecondarySelf). Keep the two flags separate.
    let life_orb = attacker.item == Item::LifeOrb;
    let life_orb_recoil = life_orb && !sheer_force_active;
    if life_orb {
        fmod = chain_final(fmod, 5324);
    }

    // Knock Off: ×1.5 base power when the target is holding a REMOVABLE item — no boost when
    // the item is species-locked (Rusted Sword/Shield, Ogerpon masks, Origin orbs) or the
    // holder has Sticky Hold (PS's handler runs the `TakeItem` event first). This is a MOVE
    // `onBasePower` doing `this.chainModify(1.5)` (data/moves.ts:9970-9975), so it belongs in
    // the shared BasePower chain below (no `onBasePowerPriority` ⇒ 0 ⇒ it chains LAST), NOT as
    // its own `modify()` — a separate rounding step loses a base power point against an
    // ability that also sits in the chain (rb1008: Tough Claws + Knock Off, PS 127 BP, engine
    // 126).
    // Sticky Hold is deliberately NOT consulted here: the boost's gate is
    // `singleEvent('TakeItem', item, target.itemState, target, target, move, item)` — a
    // single event on the ITEM effect, so only the item's OWN `onTakeItem` runs (the
    // species-lock ones). Sticky Hold is an ABILITY `onTakeItem`, reached only by
    // `pokemon.takeItem()` -> `runEvent('TakeItem')` in Knock Off's `onAfterHit`. A Sticky Hold
    // holder therefore KEEPS its item and still eats the x1.5 (rb1104 t21: Knock Off into a
    // Sticky Hold Eviolite holder — PS 41 damage, the engine 27).
    let knock_off_boost = md.id.to_id() == "knockoff"
        && defender.item != Item::None
        && item_removable_from(defender.species, defender.item, Some(attacker.species));
    let mut base_power = md.base_power;
    // Weight-based moves compute their base power dynamically.
    match md.id.to_id() {
        "grassknot" | "lowkick" => {
            let w = modified_weight_hg(defender);
            base_power = if w >= 2000 { 120 } else if w >= 1000 { 100 } else if w >= 500 { 80 }
                else if w >= 250 { 60 } else if w >= 100 { 40 } else { 20 };
        }
        "heavyslam" | "heatcrash" => {
            let wu = modified_weight_hg(attacker).max(1);
            let wt = modified_weight_hg(defender).max(1);
            let ratio = wu / wt;
            base_power = if ratio >= 5 { 120 } else if ratio >= 4 { 100 } else if ratio >= 3 { 80 }
                else if ratio >= 2 { 60 } else { 40 };
        }
        // Status / HP / item-conditional doublers.
        "stompingtantrum" if b.state.side(side).last_move_failed => base_power = base_power.saturating_mul(2),
        "facade" if attacker.status != Status::None => base_power = base_power.saturating_mul(2),
        "hex" | "infernalparade" if defender.status != Status::None => base_power = base_power.saturating_mul(2),
        "venoshock" | "barbbarrage" if matches!(defender.status, Status::Poison | Status::Toxic) => base_power = base_power.saturating_mul(2),
        "brine" if defender.hp * 2 <= defender.max_hp => base_power = base_power.saturating_mul(2),
        "acrobatics" if attacker.item == Item::None => base_power = base_power.saturating_mul(2),
        // Weather Ball doubles (50 -> 100) under any active weather (PS onModifyMove); its type
        // was already resolved in execute_move.
        "weatherball" if effective_weather(&b.state) != Weather::None => base_power = base_power.saturating_mul(2),
        // Lash Out doubles if the user had a stat lowered this turn (PS onBasePower).
        "lashout" if b.state.side(side).volatiles.contains(VolatileStatus::StatsLoweredThisTurn) =>
            base_power = base_power.saturating_mul(2),
        // HP-proportional spread moves: BP = floor(150 · userHP / userMaxHP), min 1.
        "eruption" | "waterspout" | "dragonenergy" => {
            let hp = attacker.hp.max(0) as u32;
            let max = attacker.max_hp.max(1) as u32;
            base_power = ((150 * hp / max).max(1)) as u16;
        }
        // Flail / Reversal: power rises as the user's HP falls (PS `basePowerCallback`, using the
        // 48ths ratio). Without this the move keeps its table BP of 0 → deals no damage and never
        // registers the hit for Rage Fist's `times_hit`.
        "flail" | "reversal" => {
            let ratio = (attacker.hp as i64 * 48 / (attacker.max_hp.max(1) as i64)).max(1);
            base_power = if ratio < 2 { 200 } else if ratio < 5 { 150 } else if ratio < 10 { 100 }
                else if ratio < 17 { 80 } else if ratio < 33 { 40 } else { 20 };
        }
        // Fury Cutter: doubles each consecutive use (40 → 80 → 160 cap). move_streak is the
        // count including this use, already advanced by record_move_use.
        "furycutter" => {
            let streak = b.state.side(side).move_streak.max(1);
            base_power = (40u16) << (streak - 1).min(2);
        }
        // Rage Fist: 50 + 50 per time the user has been hit this battle, capped at 350.
        "ragefist" => {
            base_power = (50u16.saturating_mul(1 + attacker.times_hit as u16)).min(350);
        }
        // Stored Power / Power Trip: 20 + 20 per positive stat stage on the user.
        "storedpower" | "powertrip" => {
            let stages: i16 = b.state.side(side).boosts.iter().map(|&x| (x.max(0)) as i16).sum();
            base_power = (20 + 20 * stages as u16).max(20);
        }
        // Avalanche / Revenge: x2 base power when the TARGET already damaged the user this
        // turn. PS `basePowerCallback`: `pokemon.attackedBy.some(p => p.source === target &&
        // p.damage > 0 && p.thisTurn)`. The engine's per-side `physical_damage_taken` /
        // `special_damage_taken` are exactly that record (set by the foe's connecting move, and
        // cleared in the residual block), so their union is the flag. rb1067 t3: Arceus-Poison's
        // Extreme Speed hits first and Avalugg-Hisui's Avalanche should be 120 BP (PS 183
        // damage, engine 93 — exactly half).
        "avalanche" | "revenge"
            if b.state.side(side).physical_damage_taken > 0
                || b.state.side(side).special_damage_taken > 0 =>
        {
            base_power = base_power.saturating_mul(2);
        }
        _ => {}
    }
    // Collision Course / Electro Drift (`chainModify([5461, 4096])` when super effective,
    // data/moves.ts:2633-2637 / :4619-4623), Psyblade (×1.5 in Electric Terrain, :14038-14042)
    // and Expanding Force (×1.5 for a grounded user in Psychic Terrain, :4952-4956) are the
    // MOVE's own `onBasePower` handlers — no `onBasePowerPriority`, so they chain LAST, in the
    // same accumulated `event.modifier` as every ability/item handler (see `bp_chain` below).
    let move_own_bp: Option<i64> = match md.id.to_id() {
        "collisioncourse" | "electrodrift"
            if crate::damage::type_multiplier(md.typ, b.state.side(side.other()).active().types) > 1.0 =>
            Some(5461),
        "psyblade" if b.state.terrain == crate::ids::Terrain::Electric => Some(6144),
        "expandingforce"
            if b.state.terrain == crate::ids::Terrain::Psychic && is_grounded(&b.state, side) =>
            Some(6144),
        _ => None,
    };
    // Type-boosting held items: ×1.2 base power for the matching move type (PS onBasePower
    // chainModify([4915, 4096])). Plates boost their type too (all 17, for Arceus formes).
    let type_item_boost = plate_type(attacker.item) == Some(md.typ) && plate_type(attacker.item).is_some()
        || matches!(
        (attacker.item, md.typ),
        (Item::Charcoal, Type::Fire)
            | (Item::MysticWater, Type::Water)
            | (Item::Magnet, Type::Electric)
            | (Item::MiracleSeed, Type::Grass)
            | (Item::NeverMeltIce, Type::Ice)
            | (Item::BlackBelt, Type::Fighting)
            | (Item::PoisonBarb, Type::Poison)
            | (Item::SoftSand, Type::Ground)
            | (Item::SharpBeak, Type::Flying)
            | (Item::TwistedSpoon, Type::Psychic)
            | (Item::SilverPowder, Type::Bug)
            | (Item::HardStone, Type::Rock)
            | (Item::SpellTag, Type::Ghost)
            | (Item::DragonFang, Type::Dragon)
            | (Item::BlackGlasses, Type::Dark)
            | (Item::MetalCoat, Type::Steel)
            | (Item::SilkScarf, Type::Normal)
            | (Item::FairyFeather, Type::Fairy)
    );
    // (The 17 Arceus plates' ×1.2 for any holder + Judgment typing landed on main — tranche
    // c3c, plate_type() — and is deliberately not duplicated here; the merge takes main's.)
    // Species-locked orbs/crystals: ×1.2 to two specific types, only for the signature species
    // (PS keys on baseSpecies.num; forme ids share the species prefix). Dialga=Steel/Dragon,
    // Palkia=Water/Dragon, Giratina=Ghost/Dragon.
    let orb_boost = match attacker.item {
        Item::AdamantOrb | Item::AdamantCrystal =>
            attacker.species.to_id().starts_with("dialga") && matches!(md.typ, Type::Steel | Type::Dragon),
        Item::LustrousOrb | Item::LustrousGlobe =>
            attacker.species.to_id().starts_with("palkia") && matches!(md.typ, Type::Water | Type::Dragon),
        Item::GriseousOrb | Item::GriseousCore =>
            attacker.species.to_id().starts_with("giratina") && matches!(md.typ, Type::Ghost | Type::Dragon),
        _ => false,
    };
    // Every MULTIPLICATIVE onBasePower handler in PS accumulates into a single `chainModify`
    // modifier (in DESCENDING onBasePowerPriority), applied to the base power exactly ONCE at the
    // end of the BasePower event (`relayVar = this.modify(relayVar, this.event.modifier)`). The
    // engine used to apply each as its own `modify`, which re-rounds at every step and diverges
    // once two stack — e.g. Technician ×1.5 (prio 30) + Black Glasses ×1.2 (prio 15) on a base
    // power 14: sequential 14→17→26, PS's single chain 14→25. `bp_step` replicates PS's chainModify
    // accumulation `((prev * next + 2048) >> 12)` (den is 4096 for every modifier here, so
    // `next == num`); the final `modify` applies the accumulated modifier once. Technician's ≤60
    // gate reads the base power with only HIGHER-priority mods applied — it is the highest (30), so
    // it reads the raw base power. Same-priority handlers never co-occur (one ability + one item
    // per holder). Reckless / Mega Launcher / Tough Claws stay on the attack stat (unchanged).
    use crate::ids::Terrain;
    let atk_grounded = !attacker.types.contains(&Type::Flying) && attacker.ability != Ab::Levitate;
    let def_grounded = !defender.types.contains(&Type::Flying) && defender.ability != Ab::Levitate;
    let terrain_boost = atk_grounded
        && matches!(
            (b.state.terrain, md.typ),
            (Terrain::Electric, Type::Electric) | (Terrain::Grassy, Type::Grass) | (Terrain::Psychic, Type::Psychic)
        );
    let terrain_halve = (b.state.terrain == Terrain::Grassy
        && def_grounded
        && matches!(md.id.to_id(), "earthquake" | "bulldoze" | "magnitude"))
        || (b.state.terrain == Terrain::Misty && def_grounded && md.typ == Type::Dragon);
    let soul_dew = attacker.item == Item::SoulDew
        && matches!(attacker.species, sp if sp == crate::ids::Species::from_id("latias").unwrap_or(crate::ids::Species::None)
            || sp == crate::ids::Species::from_id("latios").unwrap_or(crate::ids::Species::None))
        && matches!(md.typ, Type::Psychic | Type::Dragon);
    let bp_step = |acc: i64, num: i64| (acc * num + 2048) >> 12;
    let mut bp_chain = 4096i64;
    // 30: Technician (×1.5 for base power ≤ 60, gated on the pre-chain base power).
    if attacker.ability == Ab::Technician && base_power <= 60 {
        bp_chain = bp_step(bp_chain, 6144);
    }
    // 23: Iron Fist (punch ×1.2) / Reckless (recoil or crash-damage move ×1.2) / the `-ate`
    // abilities' ×1.2 for the move they retyped (abilities.ts pixilate:3263-3266,
    // refrigerate/aerilate/galvanize identical). One ability per holder, so no tie.
    if md.type_changer_boosted {
        bp_chain = bp_step(bp_chain, 4915);
    }
    if attacker.ability == Ab::IronFist && md.flag_punch {
        bp_chain = bp_step(bp_chain, 4915);
    }
    if attacker.ability == Ab::Reckless
        && (md.recoil.0 > 0 || matches!(md.id.to_id(), "highjumpkick" | "jumpkick" | "supercellslam"))
    {
        bp_chain = bp_step(bp_chain, 4915);
    }
    // 21: Sheer Force (×1.3) / Supreme Overlord (×table, +10% per fallen ally) / Tough Claws
    // (contact ×1.3) / Analytic (×1.3 when no other active still `willMove`,
    // abilities.ts:110-125) — one ability.
    if md.analytic_boosted {
        bp_chain = bp_step(bp_chain, 5325);
    }
    if sheer_force_active {
        bp_chain = bp_step(bp_chain, 5325);
    }
    if attacker.ability == Ab::ToughClaws && md.flag_contact {
        bp_chain = bp_step(bp_chain, 5325);
    }
    if attacker.ability == Ab::SupremeOverlord {
        let fallen = b.state.side(side).pokemon.iter()
            .filter(|p| p.species != crate::ids::Species::None && p.hp <= 0)
            .count()
            .min(5);
        if fallen > 0 {
            const POW: [i64; 6] = [4096, 4506, 4915, 5325, 5734, 6144];
            bp_chain = bp_step(bp_chain, POW[fallen]);
        }
    }
    // 17: Dry Skin on the DEFENDER — `onFoeBasePower` ×1.25 for an incoming Fire move
    // (`data/abilities.ts` dryskin, `onFoeBasePowerPriority: 17`). The engine modelled Dry Skin's
    // Water absorb and its weather residual and not this half. rb1636 d5: a Delphox's Fire Blast
    // into a Toxicroak — 43 HP of the 244 PS took off it.
    if def_ab == Ab::DrySkin && md.typ == Type::Fire {
        bp_chain = bp_step(bp_chain, 5120); // 1.25 = 5120/4096
    }
    // 19: Strong Jaw (bite ×1.5) / Sharpness (slicing ×1.5).
    if attacker.ability == Ab::StrongJaw && md.flag_bite {
        bp_chain = bp_step(bp_chain, 6144);
    }
    if attacker.ability == Ab::Sharpness && md.flag_slicing {
        bp_chain = bp_step(bp_chain, 6144);
    }
    // 19 also: Mega Launcher (pulse ×1.5), Toxic Boost (poisoned physical ×1.5), Flare Boost
    // (burned special ×1.5) — all three are `onBasePower`, not stat modifiers.
    if attacker.ability == Ab::MegaLauncher && md.flag_pulse {
        bp_chain = bp_step(bp_chain, 6144);
    }
    if attacker.ability == Ab::ToxicBoost
        && matches!(attacker.status, Status::Poison | Status::Toxic)
        && md.category == MoveCategory::Physical
    {
        bp_chain = bp_step(bp_chain, 6144);
    }
    if attacker.ability == Ab::FlareBoost
        && attacker.status == Status::Burn
        && md.category == MoveCategory::Special
    {
        bp_chain = bp_step(bp_chain, 6144);
    }
    // 15: type-boosting items / species orbs / Soul Dew / Ogerpon masks — all ×1.2, one item.
    if type_item_boost || orb_boost || soul_dew
        || matches!(attacker.item, Item::HearthflameMask | Item::WellspringMask | Item::CornerstoneMask)
    {
        bp_chain = bp_step(bp_chain, 4915);
    }
    // 7: Punk Rock (sound ×1.3).
    if attacker.ability == Ab::PunkRock && md.flag_sound {
        bp_chain = bp_step(bp_chain, 5325);
    }
    // 6: terrain boost (×1.3) then terrain halve (×0.5) — grounded user/target gated above
    // (`onBasePowerPriority: 6` on each terrain condition, data/moves.ts).
    if terrain_boost {
        bp_chain = bp_step(bp_chain, 5325);
    }
    if terrain_halve {
        bp_chain = bp_step(bp_chain, 2048);
    }
    // 0 (no `onBasePowerPriority` ⇒ chains last): the MOVE's own `onBasePower` — Knock Off's
    // `chainModify(1.5)` against a removable-item holder, and Collision Course / Electro Drift /
    // Psyblade / Expanding Force (resolved into `move_own_bp` above).
    if knock_off_boost {
        bp_chain = bp_step(bp_chain, 6144);
    }
    if let Some(num) = move_own_bp {
        bp_chain = bp_step(bp_chain, num);
    }
    if bp_chain != 4096 {
        base_power = crate::damage::modify(base_power as i64, bp_chain, 4096) as u16;
    }
    // Terastallization STAB floor: a terastallized mon's move matching its (post-Tera) type with
    // base power < 60 is raised to 60 — applied AFTER every onBasePower modifier. Excludes
    // priority moves, multi-hit moves, and variable-power moves whose dex base power is 0 or 150
    // (Dragon Energy / Eruption / Water Spout, …).
    //
    // **A Stellar Tera gets the floor too, under a different predicate** (`battle-actions.ts:1664`):
    //
    // ```ts
    // source.terastallized && (source.terastallized === 'Stellar'
    //     ? !source.stellarBoostedTypes.includes(move.type)
    //     : source.hasType(move.type)) && basePower < 60 && …
    // ```
    //
    // The engine used to skip Stellar entirely. It is the SAME `stellarBoostedTypes` memory the
    // Stellar STAB rule uses (`damage::stab_mod`), and for the same reason it is not modelled:
    // `:1785` never pushes a type for **Terapagos-Stellar**, the only Stellar user the gen-9
    // randbats generator produces, so the list is permanently empty and the floor applies to
    // EVERY move of every type. Witness rb1795 d3 t4: Terapagos-Stellar's Rapid Spin (dex BP 50)
    // — PS 17 damage off BP 60, the engine 14 off BP 50.
    let tera_floor_type_ok = if attacker.tera_type == Type::Stellar {
        true // `!stellarBoostedTypes.includes(move.type)` — always true for Terapagos-Stellar
    } else {
        attacker.types.contains(&md.typ) // `source.hasType(move.type)` — the post-Tera list
    };
    if attacker.terastallized
        && tera_floor_type_ok
        && base_power < 60
        && md.priority <= 0
        && md.hits_max <= 1
    {
        let dex_bp = crate::data::move_data(md.id).base_power;
        if dex_bp != 0 && dex_bp != 150 {
            base_power = 60;
        }
    }

    let input = DamageInput {
        level: attacker.level,
        base_power,
        category: md.category,
        move_type: md.typ,
        attacker_types: attacker.types,
        // PS's `isSTAB` second term is `pokemon.getTypes(false, true).includes(type)`
        // (`sim/battle-actions.ts:1768`), and `getTypes(false, true)` returns `this.types` — the
        // LIVE, pre-tera array a Protean / Soak / Burn Up already rewrote. NOT the species table
        // (which is what this used to read: a Meowscarada turned Poison by Protean and then
        // Terastallized into Dark still got Grass STAB on Flower Trick — rb1125 d2).
        attacker_base_types: attacker.live_types,
        defender_types: def_types_eff,
        attack_stat: atk_stat as i16,
        defense_stat: def_stat.max(1) as i16,
        is_crit: false,
        attacker_burned: burned,
        // Hydro Steam is BOOSTED ×1.5 in sun instead of halved (PS sunnyday
        // `onWeatherModifyDamage`). Water-in-Rain applies the identical ×1.5 at the identical
        // chain position, so mapping the weather input to Rain reproduces PS's arithmetic
        // exactly without widening `DamageInput`.
        weather: if md.id.to_id() == "hydrosteam"
            && matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun)
        {
            Weather::Rain
        } else {
            effective_weather(&b.state)
        },
        terastallized: attacker.terastallized,
        defender_terastallized: defender.terastallized,
        tera_type: attacker.tera_type,
        life_orb: false,
        adaptability,
        tera_shell,
        freeze_dry: is_freeze_dry(md),
        trunc_16: b.state.ruleset.bit_truncation,
        final_num: fmod,
        final_den: 4096,
    };
    // Crit rolls are computed from the screen-free modifiers (a crit ignores screens) and with
    // the boost-clamped stats (a crit ignores the attacker's negative offensive boost and the
    // defender's positive defensive boost).
    let mut input = input;
    let mut input_crit = input;
    input_crit.is_crit = true;
    input_crit.attack_stat = atk_stat_crit as i16;
    input_crit.defense_stat = (def_stat_crit.max(1)) as i16;
    // Sniper: ×1.5 damage on a critical hit (stacks with the 1.5 crit → 2.25× overall). PS
    // `onModifyDamage` runs in the same final chain, so fold it into the crit final modifier.
    if attacker.ability == Ab::Sniper {
        input_crit.final_num = chain_final(input_crit.final_num, 6144);
    }
    let rolls_crit = damage_rolls(&input_crit);
    // Screens halve non-crit damage in singles (Reflect: physical, Light Screen: special,
    // Aurora Veil: both), unless the attacker has Infiltrator. ×0.5 = modifier 2048/4096.
    let sc = &b.state.side(foe).side_conditions;
    let screened = attacker.ability != Ab::Infiltrator
        && match md.category {
            MoveCategory::Physical => sc.reflect > 0 || sc.aurora_veil > 0,
            MoveCategory::Special => sc.light_screen > 0 || sc.aurora_veil > 0,
            MoveCategory::Status => false,
        };
    if screened {
        input.final_num = chain_final(input.final_num, 2048);
    }
    let rolls_nocrit = damage_rolls(&input);

    DamageCalc { rolls_nocrit, rolls_crit, def_ability, def_item, def_maxhp, life_orb: life_orb_recoil }
}

/// Applies a damaging move's hits sequentially (each its own roll and crit), clamped to HP
/// and routed through any Substitute; returns true if a Substitute absorbed the damage (so
/// the target's own secondaries/volatiles are blocked).
/// PS Life Orb recoil (`data/items.ts:3408`), as a standalone step for the paths that do not go
/// through `apply_post_damage`. `onAfterMoveSecondarySelf` tests only
/// `source && source !== target && move.category !== 'Status' && !source.forceSwitchFlag`, and
/// `useMoveInner` reaches it on any truthy `moveResult` — so a hit NULLIFIED to 0 damage by Ice
/// Face or Disguise still pays the orb. Magic Guard blocks it (the effect is the ITEM, not a Move).
/// `DamageCalc::life_orb` already folds in the Sheer Force suppression.
fn apply_life_orb_recoil(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if b.state.side(side).active().ability == crate::ids::Ability::MagicGuard {
        return;
    }
    if !compute_damage(b, side, md).life_orb {
        return;
    }
    let slot = b.state.side(side).active_index;
    let atk = b.state.side(side).active();
    if !atk.is_alive() {
        return;
    }
    let recoil = (atk.max_hp / 10).max(1).min(atk.hp);
    push(b, Instruction::Damage { side, slot, amount: recoil });
}

fn ice_face_is_intact(b: &Branch, foe: SideId, md: &crate::data::MoveData) -> bool {
    let p = b.state.side(foe).active();
    md.category == MoveCategory::Physical
        && p.ability == crate::ids::Ability::IceFace
        && p.species == crate::ids::Species::from_id("eiscue").unwrap_or(crate::ids::Species::None)
}

/// Is Disguise still up on the target — i.e. will THIS hit be the one it eats?
///
/// The predicate PS uses is `['mimikyu','mimikyutotem'].includes(target.species.id)` inside
/// `onDamage` (`data/abilities.ts:963`), so it is re-evaluated PER HIT and stops being true the
/// moment `onUpdate` forme-changes the mon into Mimikyu-Busted. Which is exactly why a MULTI-HIT
/// move busts the disguise on hit 1 and then damages normally with hits 2..n — the whole-move
/// `hits_max == 1` gate on the single-hit arm was never a statement about the mechanic, only about
/// which arm owns it.
fn disguise_is_intact(b: &Branch, foe: SideId, md: &crate::data::MoveData) -> bool {
    let p = b.state.side(foe).active();
    matches!(md.category, MoveCategory::Physical | MoveCategory::Special)
        && p.ability == crate::ids::Ability::Disguise
        && !p.transformed
        && p.species == crate::ids::Species::from_id("mimikyu").unwrap_or(crate::ids::Species::None)
}

/// Breaks an intact Ice Face and records the blocked hit. Transform instructions carry the
/// complete previous forme data, so reversing a generated branch restores Eiscue exactly.
/// Disguise busts: Mimikyu forme-changes to Mimikyu-Busted (identical stats/types/ability, so
/// only the species id changes) and takes 1/8-max-HP recoil (PS `formeChange` + `damage`).
fn bust_disguise(b: &mut Branch, foe: SideId) {
    let Some(busted) = crate::ids::Species::from_id("mimikyubusted") else { return };
    let previous = transform_data_of(&b.state, foe);
    let mut new = previous;
    new.species = busted;
    let slot = b.state.side(foe).active_index;
    let previous_base_moves = b.state.side(foe).active().base_moves;
    push(b, Instruction::Transform { side: foe, slot, previous, new, previous_base_moves });
    let (hp, maxhp) = { let p = b.state.side(foe).active(); (p.hp, p.max_hp) };
    let chip = (maxhp / 8).min(hp);
    if chip > 0 {
        push(b, Instruction::Damage { side: foe, slot, amount: chip });
    }
    // PS records the disguise-blocked strike as a hit (Rage Fist's `timesAttacked`).
    let cur = b.state.side(foe).active().times_hit;
    push(b, Instruction::SetTimesHit {
        side: foe, slot, previous: cur, new: cur.saturating_add(1).min(250),
    });
}

/// Gulp Missile's SWALLOW half: `onSourceTryPrimaryHit` (`data/abilities.ts` gulpmissile) fires
/// on the Cramorant that just landed a Surf and swaps it into `cramorantgulping`, or into
/// `cramorantgorging` when it is at or below half HP:
///
/// ```text
/// if (effect?.id === 'surf' && source.hasAbility('gulpmissile') && source.species.name === 'Cramorant') {
///   const forme = source.hp <= source.maxhp / 2 ? 'cramorantgorging' : 'cramorantgulping';
///   source.formeChange(forme, effect);
/// }
/// ```
///
/// (Dive's half lives in Dive's own `onTryMove` and is not modelled — no witness in the corpus.)
/// All three formes share one base-stat line, so unlike Ice Face / Shields Down this is a pure
/// species swap with no stat respread. `formeChange` is not permanent, so `clearVolatile` on
/// switch-out puts the base forme back — the engine's `Transform` is reverted by its switch reset
/// for the same reason.
fn gulp_missile_swallow(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.id.to_id() != "surf" {
        return;
    }
    let p = b.state.side(side).active();
    if p.ability != crate::ids::Ability::GulpMissile || !p.is_alive() || p.transformed {
        return;
    }
    let Some(base) = crate::ids::Species::from_id("cramorant") else { return };
    if p.species != base {
        return;
    }
    let want_id = if (p.hp as i32) * 2 <= p.max_hp as i32 { "cramorantgorging" } else { "cramorantgulping" };
    let Some(want) = crate::ids::Species::from_id(want_id) else { return };
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = want;
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
}

/// Gulp Missile's SPIT half — an `onDamagingHit` on the loaded Cramorant. `side` is the ATTACKER
/// that just landed the hit; `foe` holds the ability.
///
/// ```text
/// onDamagingHit(damage, target, source, move) {
///   if (!source.hp || !source.isActive || target.isSemiInvulnerable()) return;
///   if (['cramorantgulping','cramorantgorging'].includes(target.species.id)) {
///     this.damage(source.baseMaxhp / 4, source, target);
///     if (target.species.id === 'cramorantgulping') this.boost({def: -1}, source, target, null, true);
///     else source.trySetStatus('par', target, move);
///     target.formeChange('cramorant', move);
///   }
/// }
/// ```
///
/// Note the early return on a fainted attacker: it suppresses the REVERT too, so a Cramorant that
/// KO's its attacker with the recoil-free hit keeps its loaded forme. The 1/4 is off the
/// attacker's `baseMaxhp` and is ability-sourced, so Magic Guard blocks it.
fn apply_gulp_missile(b: &mut Branch, side: SideId, foe: SideId) {
    let Some(gulping) = crate::ids::Species::from_id("cramorantgulping") else { return };
    let Some(gorging) = crate::ids::Species::from_id("cramorantgorging") else { return };
    let Some(base) = crate::ids::Species::from_id("cramorant") else { return };
    let holder = b.state.side(foe).active();
    if holder.ability != crate::ids::Ability::GulpMissile {
        return;
    }
    let loaded = holder.species;
    if loaded != gulping && loaded != gorging {
        return;
    }
    if !b.state.side(side).active().is_alive() {
        return;
    }
    let aslot = b.state.side(side).active_index;
    if b.state.side(side).active().ability != crate::ids::Ability::MagicGuard {
        let atk = b.state.side(side).active();
        let dmg = (atk.max_hp / 4).max(1).min(atk.hp);
        push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
    }
    if b.state.side(side).active().is_alive() {
        if loaded == gulping {
            apply_boost_clamped(b, side, BoostIndex::Defense, -1);
        } else {
            let breaker = matches!(
                b.state.side(foe).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
            );
            let can = status_applies_src(b.state.side(side).active(), Status::Paralysis, false, breaker)
                && !status_blocked_by_field(&b.state, side, Status::Paralysis);
            if can {
                push(b, Instruction::ChangeStatus {
                    side,
                    slot: aslot,
                    previous: Status::None,
                    new: Status::Paralysis,
                });
                apply_synchronize(b, side, Status::Paralysis);
                consume_lum_if_statused(b, side);
            }
        }
    }
    let previous = transform_data_of(&b.state, foe);
    let mut new = previous;
    new.species = base;
    let slot = b.state.side(foe).active_index;
    let previous_base_moves = b.state.side(foe).active().base_moves;
    push(b, Instruction::Transform { side: foe, slot, previous, new, previous_base_moves });
}

fn break_ice_face(b: &mut Branch, foe: SideId) {
    let Some(noice) = crate::ids::Species::from_id("eiscuenoice") else { return };
    let p = b.state.side(foe).active();
    let level = p.level;
    let base = crate::data::base_stats(noice);
    let mut stats = p.stats;
    for (si, stat) in [
        crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
        crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
        crate::ids::StatIndex::Speed,
    ].into_iter().enumerate() {
        stats[si + 1] = crate::damage::compute_stat(
            base[si + 1], 31, 85, level, crate::ids::Nature::Serious, stat,
        );
    }
    let previous = transform_data_of(&b.state, foe);
    let mut new = previous;
    new.species = noice;
    new.stats = stats;
    let slot = b.state.side(foe).active_index;
    let previous_base_moves = b.state.side(foe).active().base_moves;
    push(b, Instruction::Transform { side: foe, slot, previous, new, previous_base_moves });
    let cur = b.state.side(foe).active().times_hit;
    push(b, Instruction::SetTimesHit {
        side: foe, slot, previous: cur, new: cur.saturating_add(1).min(250),
    });
}

/// The inverse of [`break_ice_face`] — PS Ice Face's `onStart` / `onWeatherChange`
/// (`data/abilities.ts:1926` and `:1963`): a LIVING, untransformed Eiscue-Noice whose holder is
/// standing in hail or snowscape forme-changes straight back to Eiscue. It is not a "next
/// switch-in" restore; it fires the moment the weather turns.
///
/// rb1253 d12 t10: the foe uses Snowscape and PS's Eiscue-Noice is `eiscue` again in the very
/// next serialized state. State-only on both sides (`formeChange` makes no draw), and the two
/// formes differ only in Def/Spe, so this is a species + stats swap.
fn restore_ice_face(b: &mut Branch, side: SideId) {
    if !matches!(b.state.weather, Weather::Snow) {
        return;
    }
    let Some(noice) = crate::ids::Species::from_id("eiscuenoice") else { return };
    let Some(eiscue) = crate::ids::Species::from_id("eiscue") else { return };
    let p = b.state.side(side).active();
    if p.ability != crate::ids::Ability::IceFace || p.species != noice || p.transformed || !p.is_alive() {
        return;
    }
    let level = p.level;
    let base = crate::data::base_stats(eiscue);
    let mut stats = p.stats;
    for (si, stat) in [
        crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
        crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
        crate::ids::StatIndex::Speed,
    ].into_iter().enumerate() {
        stats[si + 1] = crate::damage::compute_stat(
            base[si + 1], 31, 85, level, crate::ids::Nature::Serious, stat,
        );
    }
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = eiscue;
    new.stats = stats;
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
}

/// Where `apply_damage_hit_rolls` gets each hit's crit + damage roll.
enum HitRolls<'a> {
    /// Enumerate / Sample (DP) path: the caller already chose the `(roll, crit)` pair per hit.
    Fixed(&'a [(u8, bool)]),
    /// Realized path (seed gate / differ): peel each hit's crit + damage roll off the cursor
    /// INSIDE the loop, so the per-hit `DamagingHit` ability roll PS fires after each connecting
    /// hit (Cursed Body / Toxic Chain / the contact-status set) interleaves into the stream at
    /// exactly the position `spreadMoveHit` puts it, instead of being appended once after the
    /// whole hit loop.
    Realized { count: usize, cur: &'a mut RealizedCursor },
}

fn apply_damage_hit(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hits: &[(u8, bool)], crit_den: i32, bp_roll: Option<bool>) -> bool {
    apply_damage_hit_rolls(b, side, md, HitRolls::Fixed(hits), crit_den, bp_roll, false)
}

/// `bp_roll`: the enumerated outcome of a random `onBasePower` handler (Fickle Beam), emitted
/// between this hit's crit roll and its damage roll — PS's `runEvent('BasePower')` position.
fn apply_damage_hit_rolls(b: &mut Branch, side: SideId, md: &crate::data::MoveData, mut rolls: HitRolls, crit_den: i32, bp_roll: Option<bool>, beak_blast: bool) -> bool {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let initial_calc = compute_damage(b, side, md);
    let (def_ability, def_item, def_maxhp, life_orb) =
        (initial_calc.def_ability, initial_calc.def_item, initial_calc.def_maxhp, initial_calc.life_orb);
    let mut calc = initial_calc;
    // Apply each hit's damage independently (own roll and crit), clamped to current HP
    // (a hit that faints the target ends the sequence; remaining hits add nothing).
    let mut any_damage = false;
    let mut hit_sub = false;
    let mut total_dealt: i32 = 0;
    let mut hits_landed: u8 = 0;
    let mut hits_executed: u8 = 0;
    let n_hits = match &rolls {
        HitRolls::Fixed(h) => h.len(),
        HitRolls::Realized { count, .. } => *count,
    };
    for hit_i in 0..n_hits {
        // The PREVIOUS hit's deferred step-7 `runEvent('DamagingHit')` (see
        // `apply_damaging_hit_step7`): `spreadMoveHit` completes hit n — including step 7 — before
        // `hitStepMoveHitLoop` begins hit n+1, so it must land before this hit's KO check and before
        // its damage is re-derived. Only the LAST hit's event survives the loop and gets flushed
        // after the secondaries.
        if b.pending_damaging_hit.is_some() {
            let pre_inputs = damage_inputs(b, side);
            apply_damaging_hit_step7(b, side, md, false);
            if damage_inputs(b, side) != pre_inputs {
                calc = compute_damage(b, side, md);
            }
        }
        // PS's `hitStepMoveHitLoop` checks `targets.every(!hp)` at the TOP of each iteration,
        // BEFORE that hit's crit/damage rolls (which happen inside `spreadMoveHit` → `getDamage`).
        // So once the target has fainted, no further crit/damage draws are rolled. Emit the
        // per-hit draws here — after this KO check, before applying the hit — so a multi-hit move
        // that KOs early stops the draw stream exactly where PS does (fixes phantom-hit over-roll).
        if b.state.side(foe).active().hp <= 0 {
            break;
        }
        // The PREVIOUS iteration's `eachEvent('Update')` (970) — see `emit_prev_hit_update`.
        if hit_i >= 1 {
            match &mut rolls {
                HitRolls::Realized { cur, .. } => emit_prev_hit_update(b, Some(cur)),
                HitRolls::Fixed(_) => emit_prev_hit_update(b, None),
            }
            // ...and PS's OWN last statement: `if (!pokemon.hp && targets.length === 1) { hit++;
            // break; }` (`battle-actions.ts:971-974`). A USER that died to the previous hit's
            // step-7 reaction — Rocky Helmet / Rough Skin / Iron Barbs — ends the multi-hit move.
            // rb5280 d9 t7: a 40-HP Meowscarada (Choice Band) Triple Axels a Pecharunt holding a
            // Rocky Helmet, dies to the hit-1 chip, and PS's stream stops after ONE crit+damage
            // pair. The engine rolled all three and emitted a `rust extra`
            // `randomChance[90,100]@accuracy` — an over-emission, which is the campaign's one
            // hard invariant.
            if b.state.side(side).active().hp <= 0 {
                break;
            }
        }
        let (roll, crit) = match &mut rolls {
            HitRolls::Fixed(h) => h[hit_i],
            HitRolls::Realized { cur, .. } => {
                let c = crit_den > 0 && cur.peek("randomChance", &[1, crit_den]) != 0;
                let r = (cur.peek("random", &[16]) as u8) & 0x0F;
                (r, c)
            }
        };
        if crit_den > 0 {
            draw(b, "randomChance", &[1, crit_den], crit as i64, "crit");
        }
        if let Some(v) = bp_roll {
            draw(b, "randomChance", &[3, 10], v as i64, "ficklebeam");
        }
        draw(b, "random", &[16], roll as i64, "damage-roll");
        // ModifyDamage screen-tie shuffle (per getDamage, after the damage roll).
        emit_modifydamage_shuffle(b);
        if let HitRolls::Realized { cur, .. } = &mut rolls {
            // The `draw`/`emit_*` calls above advance the branch's log and (for the seed gate) the
            // real prng, not this peek clone — step it over the shuffle just emitted.
            let k = modifydamage_screen_count(b);
            cur.consume_shuffle(k);
        }
        let dmg_rolls = if crit { &calc.rolls_crit } else { &calc.rolls_nocrit };
        let raw = dmg_rolls[roll as usize];
        // Route to the Substitute if the target has one up (it absorbs the whole hit).
        // Sound moves and Infiltrator users go straight through it.
        let bypass_sub = md.flag_sound
            || b.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
        let sub_hp = b.state.side(foe).substitute_hp;
        if sub_hp > 0 && !bypass_sub && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
            let sub_dmg = raw.min(sub_hp);
            push(b, Instruction::DamageSubstitute { side: foe, amount: sub_dmg });
            if raw >= sub_hp {
                push(b, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
            }
            total_dealt += sub_dmg as i32;
            any_damage = true;
            hit_sub = true;
            hits_executed += 1;
            continue;
        }
        if ice_face_is_intact(b, foe, md) {
            break_ice_face(b, foe);
            // Forme change is immediate: later hits use Noice Form's Defense.
            calc = compute_damage(b, side, md);
            continue;
        }
        // Disguise eats THIS hit and nothing more — `onUpdate`'s forme change into Mimikyu-Busted
        // runs at the per-hit `eachEvent('Update')` (`battle-actions.ts:970`), so hit 2 onward
        // finds `species.id === 'mimikyubusted'` and `onDamage` no longer fires. rb1621 d18:
        // Triple Axel into an intact Mimikyu — PS's three hits are 0 (+ the 1/8 chip), then two
        // real ones; the engine's `hits_max == 1` gate made all three real, and the mon died in
        // both, so the ONLY surviving symptom was `species` 495 vs 496 on a corpse.
        if disguise_is_intact(b, foe, md) {
            bust_disguise(b, foe);
            calc = compute_damage(b, side, md);
            continue;
        }
        let target_hp = b.state.side(foe).active().hp;
        if target_hp <= 0 {
            break;
        }
        let mut dmg = raw.min(target_hp);
        // Sturdy / Focus Sash: survive a would-be KO from full HP at 1 HP.
        if (def_ability == Ab::Sturdy || def_item == Item::FocusSash)
            && target_hp == def_maxhp && dmg >= target_hp
        {
            dmg = target_hp - 1;
            if def_item == Item::FocusSash {
                let slot = b.state.side(foe).active_index;
                push(b, Instruction::ChangeItem { side: foe, slot, previous: Item::FocusSash, new: Item::None });
            }
        }
        if dmg > 0 {
            let slot = b.state.side(foe).active_index;
            push(b, Instruction::Damage { side: foe, slot, amount: dmg });
            any_damage = true;
            total_dealt += dmg as i32;
            hits_landed += 1;
            hits_executed += 1;
        }
        // PS `spreadMoveHit` runs `runEvent('DamagingHit', damagedTargets, ...)` at the END of
        // EVERY hit (battle-actions.ts:1142) — inside the loop, not after it. On the realized path
        // fire its DRAWING handlers here so Cursed Body / Toxic Chain / the contact-status abilities
        // interleave into the stream between this hit and the next hit's crit roll.
        let pre_inputs = damage_inputs(b, side);
        // Step 3 (`onHit`) precedes step 7: Beak Blast burns the attacker on THIS hit, and the
        // re-derivation below hands the halved Attack to hit n+1.
        apply_beak_blast_burn(b, side, md, beak_blast);
        if let HitRolls::Realized { cur, .. } = &mut rolls {
            realized_per_hit_damaging_hit(b, side, md, cur);
        }
        // A status those handlers inflicted (Flame Body's burn halving the attacker's Atk) is an
        // input to the NEXT hit's `getDamage` — PS re-derives it every loop iteration.
        if damage_inputs(b, side) != pre_inputs {
            calc = compute_damage(b, side, md);
        }
        // The no-draw `onDamagingHit` handlers are part of that SAME per-hit event: Stamina's
        // +1 Def, Water Compaction's +2, Rough Skin / Iron Barbs / Rocky Helmet's chip, Gooey's
        // -1 Spe, Justified / Rattled / Thermal Exchange / Weak Armor. They are DEFERRED: step 7
        // runs after step 5's `secondaries()`, and the secondaries are composed by the caller.
        // The top of the next iteration flushes it (anything the event moved that the damage
        // formula reads is visible to the NEXT hit); the last hit's flush happens after the
        // secondary split.
        b.pending_damaging_hit = Some((dmg > 0, def_item, def_ability));
    }
    // PS's `timesAttacked += hit - 1` counts every EXECUTED hit — including ones a Substitute
    // absorbed and the KO-ing hit (nothing after a faint executes) — but only when at least
    // one hit connected with the Pokémon itself (a fully sub-absorbed move records nothing:
    // its per-target damage entry stays `false`). Verified against the pin empirically.
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, times_count, life_orb, def_item, def_ability, true);
    hit_sub
}

/// Like `apply_damage_hit`, but each hit i uses its own precomputed `DamageCalc`
/// (Triple Axel / Triple Kick ascending power).
fn apply_damage_hit_indexed(b: &mut Branch, side: SideId, md: &crate::data::MoveData, calcs: &[DamageCalc], hits: &[(u8, bool)]) -> bool {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let (def_ability, def_item, def_maxhp, life_orb) =
        (calcs[0].def_ability, calcs[0].def_item, calcs[0].def_maxhp, calcs[0].life_orb);
    let mut any_damage = false;
    let mut hit_sub = false;
    let mut total_dealt: i32 = 0;
    let mut hits_landed: u8 = 0;
    let mut hits_executed: u8 = 0;
    let mut restat_dirty = false;
    for (i, &(roll, crit)) in hits.iter().enumerate() {
        // Previous hit's deferred step-7 event (see `apply_damaging_hit_step7`).
        if b.pending_damaging_hit.is_some() {
            let pre_inputs = damage_inputs(b, side);
            apply_damaging_hit_step7(b, side, md, false);
            restat_dirty |= damage_inputs(b, side) != pre_inputs;
        }
        let indexed_calc = &calcs[i.min(calcs.len() - 1)];
        let initial_rolls = if crit { &indexed_calc.rolls_crit } else { &indexed_calc.rolls_nocrit };
        let initial_raw = initial_rolls[roll as usize];
        let bypass_sub = md.flag_sound
            || b.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
        let sub_hp = b.state.side(foe).substitute_hp;
        if sub_hp > 0 && !bypass_sub && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
            let sub_dmg = initial_raw.min(sub_hp);
            push(b, Instruction::DamageSubstitute { side: foe, amount: sub_dmg });
            if initial_raw >= sub_hp {
                push(b, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
            }
            total_dealt += sub_dmg as i32;
            any_damage = true;
            hit_sub = true;
            hits_executed += 1;
            continue;
        }
        if ice_face_is_intact(b, foe, md) {
            break_ice_face(b, foe);
            continue;
        }
        // Disguise eats ONE hit; `onUpdate`'s forme change lands at the per-hit
        // `eachEvent('Update')` and hit 2 onward sees Mimikyu-Busted. See `disguise_is_intact`.
        // This is the Triple Axel arm — rb1621 d18's witness.
        if disguise_is_intact(b, foe, md) {
            bust_disguise(b, foe);
            restat_dirty = true;
            continue;
        }
        // Indexed moves change power each hit, and Ice Face — or a per-hit `onDamagingHit`
        // reaction (Stamina's +1 Def) — can change Defense between hits.
        let noice_calc = if b.state.side(foe).active().species == crate::ids::Species::from_id("eiscuenoice").unwrap_or(crate::ids::Species::None)
            || restat_dirty
        {
            Some(compute_damage(b, side, &{ let mut m = *md; m.base_power *= (i + 1) as u16; m }))
        } else {
            None
        };
        let calc = noice_calc.as_ref().unwrap_or(&calcs[i.min(calcs.len() - 1)]);
        let rolls = if crit { &calc.rolls_crit } else { &calc.rolls_nocrit };
        let raw = rolls[roll as usize];
        let target_hp = b.state.side(foe).active().hp;
        if target_hp <= 0 {
            break;
        }
        let mut dmg = raw.min(target_hp);
        if (def_ability == Ab::Sturdy || def_item == Item::FocusSash)
            && target_hp == def_maxhp && dmg >= target_hp
        {
            dmg = target_hp - 1;
            if def_item == Item::FocusSash {
                let slot = b.state.side(foe).active_index;
                push(b, Instruction::ChangeItem { side: foe, slot, previous: Item::FocusSash, new: Item::None });
            }
        }
        if dmg > 0 {
            let slot = b.state.side(foe).active_index;
            push(b, Instruction::Damage { side: foe, slot, amount: dmg });
            any_damage = true;
            total_dealt += dmg as i32;
            hits_landed += 1;
            hits_executed += 1;
        }
        // `runEvent('DamagingHit')` fires at the end of EVERY hit (battle-actions.ts:1142), but
        // AFTER step 5's `secondaries()` — deferred, flushed at the top of the next iteration or
        // (for the last hit) after the caller's secondary split.
        b.pending_damaging_hit = Some((dmg > 0, def_item, def_ability));
    }
    // PS's `timesAttacked += hit - 1` counts every EXECUTED hit — including ones a Substitute
    // absorbed and the KO-ing hit (nothing after a faint executes) — but only when at least
    // one hit connected with the Pokémon itself (a fully sub-absorbed move records nothing:
    // its per-target damage entry stays `false`). Verified against the pin empirically.
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, times_count, life_orb, def_item, def_ability, true);
    hit_sub
}

/// PS's `runEvent('DamagingHit')` (`sim/battle-actions.ts:1142`) sits at the END of
/// `spreadMoveHit`, which `hitStepMoveHitLoop` calls ONCE PER HIT — so every `onDamagingHit`
/// handler fires once per hit of a multi-hit move, not once per move. A 3-hit Triple Axel
/// raises a Stamina holder's Defense three times (`data/abilities.ts:4471-4474`, an
/// unconditional `this.boost({def: 1})`) and each later hit is computed against the raised
/// stat; Rough Skin / Iron Barbs (`:3893` / `:2179`, `onDamagingHitOrder: 1`) and Rocky Helmet
/// (`data/items.ts:5296`, `onDamagingHitOrder: 2`) likewise bite once per hit.
///
/// rb1202 d2 is the witness: p1 U-turns into a Stamina Archaludon and p2's Triple Axel lands
/// all three hits — PS ends at `def: +3` / 218 HP, the engine at `def: +1` / 189.
///
/// Berserk and Anger Shell are NOT here: they are `onDamage` + `onAfterMoveSecondary`
/// (`data/abilities.ts:404` / `:143`), keyed on the move's total, and stay once per move.
/// `spreadMoveHit`'s numbered steps at the pin (`sim/battle-actions.ts:1044-1155`), and where each
/// one lives in the engine. `hitStepMoveHitLoop` calls the WHOLE of this once per hit.
///
/// ```text
///   0. tryPrimaryHitEvent          Substitute routing            in the hit loops
///   1. getSpreadDamage (getDamage) crit + damage roll            in the hit loops
///   2. spreadDamage                Instruction::Damage           in the hit loops
///   3. runMoveEffects (onHit)      apply_bug_bite / apply_thaw_on_hit / apply_spirit_shackle /
///                                  apply_sparkling_aria / apply_relic_song_forme
///   4. selfDrops                   apply_self_drop, start_rampage_lock
///   5. secondaries()               apply_damage_secondaries, apply_burning_jealousy,
///                                  apply_target_secondary, apply_alluringvoice_confusion,
///                                  apply_triattack_secondary, apply_direclaw_secondary,
///                                  apply_partial_trap, apply_flinch_split
///   6. forceSwitch                 apply_drag
///   7. runEvent('DamagingHit')     THIS FUNCTION: apply_damaging_hit_reactions (Rough Skin /
///                                  Iron Barbs / Rocky Helmet / Gulp Missile / Electromorphosis /
///                                  Stamina / Water Compaction / Seed Sower / Toxic Debris /
///                                  Gooey / Tangling Hair) + Justified / Rattled / Thermal
///                                  Exchange / Weak Armor, then the DRAWING handlers
///                                  apply_contact_secondaries / apply_cursed_body (realized paths
///                                  fire those inside the loop) and apply_weakness_policy
///   8. onAfterHit                  Stone Axe / Ceaseless Edge hazards, apply_spin_clear
///   9. eachEvent('Update')         apply_pinch_berry, consume_lum_if_statused, emit_update_hit
/// ```
///
/// The engine used to run step 7 INSIDE the hit loop and compose step 5 in the caller, i.e. 7
/// before 5. rb1122 d5 is the witness: Palossand sits at Def +5, Azumarill's Liquidation lands and
/// its 20% Def drop procs. PS takes 5 -> 4 (secondary) -> 6 (Water Compaction +2). The engine took
/// 5 -> 6 (clamped) -> 5. So the hit loops now DEFER the event onto `Branch::pending_damaging_hit`
/// and this flushes it — at the top of the next hit (PS finishes hit n before starting hit n+1) or,
/// for the last hit, after the caller's secondary split.
///
/// The once-per-move fallback in `apply_post_damage` (`!per_hit_done`: fixed-damage moves and the
/// enumeration DP paths) is deliberately NOT deferred — none of those moves has a `secondary`, so
/// the reorder is a no-op for them, and leaving it in place keeps the event ahead of the
/// Moxie / `onSourceAfterFaint` block that follows it.
fn apply_damaging_hit_step7(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hit_sub: bool) {
    let foe = side.other();
    if let Some((any_damage, def_item, def_ability)) = b.pending_damaging_hit.take() {
        apply_damaging_hit_reactions(b, side, md, any_damage, false, def_item, def_ability);
    }
    // `runEvent('DamagingHit')` is gated on `damagedDamage.length` — a Substitute hit records
    // `damage[i] === true`, not a number, so no `onDamagingHit` handler runs behind a sub.
    if !hit_sub {
        apply_illusion_break(b, foe);
        apply_justified(b, foe, md);
        apply_rattled(b, foe, md);
        apply_thermal_exchange(b, foe, md);
        apply_weak_armor(b, foe, md);
    }
}

/// Illusion's `onDamagingHit` → `singleEvent('End', Illusion)` → `onEnd`
/// (`data/abilities.ts:2026-2042`). `beingCalledBack` is false on this path (it is only ever set
/// by `switchIn`), so the disguise drops VISIBLY: `|replace|` with the real details, `|-end|` and,
/// under Illusion Level Mod, the level hint. Draw-free; the state change is protocol-visible only.
///
/// Unordered among the `DamagingHit` handlers (no `onDamagingHitOrder`), and gated on
/// `damagedDamage.length` exactly like the rest of step 7 — a Substitute hit does not break it.
/// A hit that KOs still breaks it: `pokemon.fainted` is not set until `faintMessages`.
fn apply_illusion_break(b: &mut Branch, victim: SideId) {
    let slot = b.state.side(victim).active_index;
    let p = &b.state.side(victim).pokemon[slot as usize];
    if p.ability != crate::ids::Ability::Illusion {
        return;
    }
    if let Some(previous) = p.illusion {
        push(b, Instruction::BreakIllusion { side: victim, slot, previous });
    }
}

fn apply_damaging_hit_reactions(
    b: &mut Branch,
    side: SideId,
    md: &crate::data::MoveData,
    any_damage: bool,
    hit_sub: bool,
    def_item: Item,
    def_ability: crate::ids::Ability,
) {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    if !any_damage {
        return;
    }
    let aslot = b.state.side(side).active_index;
    // Magic Guard blocks EVERY non-Move damage source: Rough Skin / Iron Barbs pass the
    // ABILITY and Rocky Helmet the ITEM as the effect, so all three are blocked.
    let magic_guard = b.state.side(side).active().ability == Ab::MagicGuard;
    // Contact punishers: Rough Skin / Iron Barbs (1/8, ability onDamagingHit) AND Rocky
    // Helmet (1/6, item) — PS runs BOTH when the holder has ability + item (the c5
    // directed traces caught the engine applying only one).
    if md.flag_contact && !hit_sub && !magic_guard {
        if matches!(def_ability, Ab::RoughSkin | Ab::IronBarbs) {
            let atk = b.state.side(side).active();
            if atk.is_alive() {
                let dmg = (atk.max_hp / 8).max(1).min(atk.hp);
                push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
            }
        }
        if def_item == Item::RockyHelmet {
            let atk = b.state.side(side).active();
            if atk.is_alive() {
                let dmg = (atk.max_hp / 6).max(1).min(atk.hp);
                push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
            }
        }
    }
    // Gulp Missile's spit — a loaded Cramorant punishes the hit and reverts.
    if !hit_sub {
        apply_gulp_missile(b, side, foe);
    }
    // Electromorphosis charges the holder after any damaging hit. The Charge volatile
    // doubles its next Electric move and is consumed by that move.
    if !hit_sub
        && def_ability == Ab::Electromorphosis
        && b.state.side(foe).active().is_alive()
        && !b.state.side(foe).volatiles.contains(VolatileStatus::Charge)
    {
        push(b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Charge });
    }
    if hit_sub {
        return;
    }
    let f = b.state.side(foe).active();
    if f.is_alive() {
        match f.ability {
            Ab::Stamina => {
                raise_boost(b, foe, BoostIndex::Defense, 1);
            }
            Ab::WaterCompaction if md.typ == Type::Water => {
                raise_boost(b, foe, BoostIndex::Defense, 2);
            }
            _ => {}
        }
    }
    // onDamagingHit reactions that fire even while the holder is FAINTING (PS runs the
    // event before faint processing; the c5 directed traces caught the is_alive gate):
    // Seed Sower's terrain is field-level, and Gooey/Tangling Hair boost the ATTACKER.
    let f = b.state.side(foe).active();
    match f.ability {
        Ab::SeedSower => {
            // Being hit by a damaging move plants Grassy Terrain (PS onDamagingHit
            // this.field.setTerrain — fails silently if already Grassy).
            if b.state.terrain != crate::ids::Terrain::Grassy {
                let turns = if f.item == Item::TerrainExtender { 8 } else { 5 };
                emit_field_change_shuffle(b); // setTerrain -> eachEvent('TerrainChange')
                push(b, Instruction::ChangeTerrain {
                    previous: b.state.terrain,
                    previous_turns: b.state.terrain_turns,
                    new: crate::ids::Terrain::Grassy,
                    new_turns: turns,
                });
                refresh_proto_quark(b); // PS Quark Drive `onTerrainChange`
            }
        }
        // Toxic Debris is an `onDamagingHit` too (`data/abilities.ts:5061`), so a physical
        // MULTI-HIT move scatters one layer PER HIT until the cap: `side.addSideCondition
        // ('toxicspikes')` guarded by `move.category === 'Physical' && (!toxicSpikes ||
        // toxicSpikes.layers < 2)`, with `side = source.side` (the attacker's). It is inside
        // this event, so it needs damage to have landed and it does NOT need the holder alive.
        Ab::ToxicDebris if md.category == MoveCategory::Physical => {
            let cur = b.state.side(side).side_conditions.toxic_spikes;
            if cur < 2 {
                push(b, Instruction::SetSideCondition {
                    side,
                    condition: SideConditionId::ToxicSpikes,
                    previous: cur,
                    new: cur + 1,
                });
            }
        }
        Ab::Gooey | Ab::TanglingHair if md.flag_contact => {
            // Contact drops the attacker's Speed by 1 (PS onDamagingHit boost(spe:-1)
            // with source=holder — a foe-inflicted drop, so blockers/Mirror Armor and
            // Defiant/Competitive apply on the attacker's side).
            if b.state.side(side).active().is_alive() {
                if apply_boost_clamped(b, side, BoostIndex::Speed, -1) < 0 {
                    react_to_stat_drop(b, side);
                    apply_white_herb(b, side);
                }
            }
        }
        _ => {}
    }
}

/// Everything `compute_damage` reads that a per-hit `runEvent('DamagingHit')` reaction can move.
///
/// PS re-derives `getDamage` from LIVE state on every hit of the hit loop, so the engine's cached
/// `DamageCalc` is only valid while none of these inputs has changed. Comparing a snapshot taken
/// before the event with one taken after is strictly more faithful than enumerating which
/// abilities move a stat: it catches the boost handlers (Stamina, Water Compaction, Gooey /
/// Tangling Hair), the field handler (Seed Sower's terrain) AND the STATUS handlers — Flame Body
/// burning the attacker halves its Attack for every remaining hit.
///
/// rb1198 d29 is the status witness: p2's 3-hit Triple Axel into a Flame Body holder burns the
/// user on hit 1 (`randomChance[3,10]@contact-status` = 1). PS's hits 2 and 3 are then computed
/// at half Attack (43 / 43 / 64); the engine kept the unburned calc and scaled it by base power
/// (43 / 90 / 134), 114 HP too much.
#[derive(PartialEq, Eq, Clone, Copy)]
struct DamageInputs {
    atk: (crate::ids::Status, [i8; BoostIndex::COUNT], crate::ids::Ability, Item, [Type; 2]),
    def: (crate::ids::Status, [i8; BoostIndex::COUNT], crate::ids::Ability, Item, [Type; 2]),
    field: (Weather, crate::ids::Terrain),
    screens: (u8, u8, u8),
}
fn damage_inputs(b: &Branch, side: SideId) -> DamageInputs {
    let foe = side.other();
    let (asd, dsd) = (b.state.side(side), b.state.side(foe));
    let (a, d) = (asd.active(), dsd.active());
    DamageInputs {
        atk: (a.status, asd.boosts, a.ability, a.item, a.types),
        def: (d.status, dsd.boosts, d.ability, d.item, d.types),
        field: (b.state.weather, b.state.terrain),
        screens: (dsd.side_conditions.reflect, dsd.side_conditions.light_screen, dsd.side_conditions.aurora_veil),
    }
}

/// Effects keyed on the *total* damage a move dealt: drain, move recoil, Life Orb recoil,
/// and Toxic Debris. Shared by the exact per-hit path and the multi-hit sumset-DP path so both
/// stay in lockstep. `per_hit_done` suppresses the `runEvent('DamagingHit')` reactions when the
/// caller already fired them inside its hit loop (the realized executors).
/// Knock Off's `onAfterHit` (`data/moves.ts` knockoff) — **step 8** of `spreadMoveHit`:
/// `if (source.hp) { const item = target.takeItem(); ... }`.
///
/// Factored out because step 8 has TWO callers, and the second one had nothing at all: a hit
/// NULLIFIED by Disguise or Ice Face. `onDamage` returns the NUMBER 0, so the target stays in
/// `targets` and PS runs the rest of `hitStepMoveHitLoop` — this file's own Disguise arm already
/// records that for step 4 (`selfDrops`, rb1093) and for the two Updates (rb1191). Step 8 is the
/// same fact one step further on.
///
/// * rb5453 d17 t14: a Deoxys-Speed Knock Offs an Eiscue with an intact ICE FACE holding a Sitrus
///   Berry. PS ends the turn with the berry gone; the engine kept it.
/// * rb5463 d27 t24: a Blaziken Knock Offs a Mimikyu with an intact DISGUISE holding a Life Orb.
///   Same shape, other ability.
///
/// The `source.hp` guard is passed IN, not read off `is_alive()` — see `step8_user_alive` and
/// `Branch::after_hit_user_alive` for why the engine has three different answers to "is the
/// attacker standing" and which one belongs at which call site.
fn apply_knock_off_take_item(b: &mut Branch, side: SideId, def_ability: crate::ids::Ability, user_alive: bool) {
    let foe = side.other();
    let f = b.state.side(foe).active();
    if f.species != crate::ids::Species::None
        && user_alive
        && f.item != Item::None
        && item_removable_from(f.species, f.item, Some(b.state.side(side).active().species))
        && def_ability != crate::ids::Ability::StickyHold
    {
        let (prev, fslot) = (f.item, b.state.side(foe).active_index);
        push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: prev, new: Item::None });
        on_item_lost(b, foe);
        // Knocking the item off reveals what it was.
        reveal(b, foe, 0, crate::state::Reveal::ITEM);
    }
}

/// PS's step-8 `onAfterHit` item removal and the two `AfterMoveSecondary(Self)` item steals that
/// follow it, as ONE ordered sequence — Knock Off's `takeItem`, then Magician, then Pickpocket.
///
/// **The order between them is load-bearing and it is not the order the engine used to run.**
/// Knock Off strips the target at step 8; Pickpocket then finds an ITEMLESS holder and steals the
/// attacker's item in its place. rb5267 d1 t2 is the witness: a Choice Band Pickpocket Weavile is
/// Knocked Off by a Leftovers Wo-Chien, and PS ends with the Weavile holding the LEFTOVERS. Split
/// the two across the step-5 secondary composition and the steal reads a Weavile that still has
/// its Choice Band, so nothing moves at all (rb5217 d20, rb5335 d7 are the same shape).
#[allow(clippy::too_many_arguments)]
fn apply_after_hit_item_moves(
    b: &mut Branch,
    side: SideId,
    md: &crate::data::MoveData,
    def_ability: crate::ids::Ability,
    any_damage: bool,
    hit_sub: bool,
    user_alive: bool,
) {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    if md.id.to_id() == "knockoff" && !hit_sub {
        // Knock Off's removal is `onAfterHit` — step 8 — and `runEvent('DamagingHit')` is step 7,
        // so an `onDamagingHit` ITEM consumes itself before Knock Off can take it. The engine runs
        // `apply_post_damage` ahead of the deferred `apply_damaging_hit_step7` (see the Gulp
        // Missile note below for the same hazard), so the removal would erase the item first.
        // Fire the policy here, ahead of the take; the caller's later call re-reads the item and
        // finds nothing, exactly like the Gulp Missile precedent.
        //
        // Safe to hoist it above step 5 for THIS move only: Knock Off has no `secondaries`, so the
        // reason the caller defers the policy — keeping the +2/+2 invisible to a secondary that
        // reads `statsRaisedThisTurn` (rb1178: Alluring Voice) — has nothing to act on here.
        //
        // rb1544 t14 (Knock Off into a Weakness Policy Solgaleo) and rb1447 t25 (into a Necrozma-
        // Dusk-Mane): Dark is 2x on Psychic/Steel, PS ends the turn at +2 Atk / +2 SpA with the
        // policy consumed, the engine at +0 / +0 with it merely knocked away.
        apply_weakness_policy(b, foe, md);
        // **A target the Knock Off just KO'd still loses its item.** `onAfterHit` is step 8, and
        // `faintMessages` has not run — `takeItem` (`sim/pokemon.ts:1851-1866`) checks only
        // `this.item` and a `TakeItem` event; `pokemon.isActive` stays true until a replacement
        // switches in and `pokemon.hp` is never consulted. Testing `is_alive()` here asked the
        // question one faint too early, exactly as `after_hit_user_alive` does for the ATTACKER
        // (which is where PS's real guard sits: `useMoveInner` never reaches step 8 with a dead
        // user). rb1314 d5 t5: p1's Knock Off takes a Light Clay off an Abomasnow and KOs it in
        // the same hit; the engine kept the item, and it stayed invisible for 40 turns until a
        // Revival Blessing at d45 put the corpse back on the field holding it.
        apply_knock_off_take_item(b, side, def_ability, user_alive);
    }

    // The defender's pinch/HP berry is an `onUpdate`, and PS's `eachEvent('Update')` sits at
    // `battle-actions.ts:970` — at the BOTTOM of `hitStepMoveHitLoop`, after `spreadMoveHit`
    // has already run `runEvent('DamagingHit')` (:1142) and the move's own `onAfterHit`
    // (:1144). Knock Off's item removal is that `onAfterHit`, so it beats the berry: a Sitrus
    // holder knocked below half by Knock Off loses the berry instead of eating it (rb1383 t15:
    // Muk Knock Off into a switching-in Veluza — PS 116, engine 189 with `last_berry` set).
    // It still precedes Magician / Pickpocket, which are `AfterMoveSecondary(Self)` events
    // fired by `useMoveInner` after the whole hit loop.
    //
    // The EAT itself is NOT here: `battle-actions.ts:970` also sits after the move's
    // SECONDARIES (`spreadMoveHit` order: damage -> onHit -> selfDrops -> secondaries ->
    // DamagingHit -> onAfterHit -> :970), so the berry decision must read the post-secondary
    // state. It is applied at the 970/1024 emission site in the hit path.

    // Magician: after landing a damaging move, an itemless attacker steals the target's item
    // (PS onAfterMoveSecondarySelf; excluded for pivot moves — source.switchFlag — and Fling).
    if any_damage
        && !hit_sub
        && b.state.side(side).active().ability == Ab::Magician
        && b.state.side(side).active().is_alive()
        && b.state.side(side).active().item == Item::None
        && !md.self_switch
        && md.category != MoveCategory::Status
    {
        let f = b.state.side(foe).active();
        if f.item != Item::None
            && item_removable_from(f.species, f.item, Some(b.state.side(side).active().species))
            && def_ability != Ab::StickyHold
        {
            let stolen = f.item;
            let fslot = b.state.side(foe).active_index;
            let aslot = b.state.side(side).active_index;
            push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: stolen, new: Item::None });
            on_item_lost(b, foe);
            push(b, Instruction::ChangeItem { side, slot: aslot, previous: Item::None, new: stolen });
            // Gaining an item ends an active Unburden speed boost.
            if b.state.side(side).volatiles.contains(VolatileStatus::Unburden) {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Unburden });
            }
            reveal(b, foe, 0, crate::state::Reveal::ITEM);
        }
    }

    // Pickpocket: when hit by a contact move, an itemless holder steals the attacker's item
    // (PS onAfterMoveSecondary; not on pivot moves — source.switchFlag).
    if any_damage
        && !hit_sub
        && md.flag_contact
        && !md.self_switch
        && def_ability == Ab::Pickpocket
        && b.state.side(foe).active().is_alive()
        && b.state.side(foe).active().item == Item::None
    {
        let a = b.state.side(side).active();
        if a.item != Item::None
            && item_removable_from(a.species, a.item, Some(b.state.side(foe).active().species))
            && a.ability != Ab::StickyHold
        {
            let stolen = a.item;
            let aslot = b.state.side(side).active_index;
            let fslot = b.state.side(foe).active_index;
            push(b, Instruction::ChangeItem { side, slot: aslot, previous: stolen, new: Item::None });
            on_item_lost(b, side);
            push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: Item::None, new: stolen });
            if b.state.side(foe).volatiles.contains(VolatileStatus::Unburden) {
                push(b, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Unburden });
            }
            reveal(b, side, 0, crate::state::Reveal::ITEM);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_post_damage(
    b: &mut Branch,
    side: SideId,
    md: &crate::data::MoveData,
    total_dealt: i32,
    any_damage: bool,
    hit_sub: bool,
    hits_landed: u8,
    life_orb: bool,
    def_item: Item,
    def_ability: crate::ids::Ability,
    per_hit_done: bool,
) {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    // PS's `onAfterHit` fired back inside `spreadMoveHit`, BEFORE any of the self-damage below.
    // Snapshot its `pokemon.hp` guard now — see `Branch::after_hit_user_alive`.
    b.after_hit_user_alive = b.state.side(side).active().is_alive();
    // Everything PS defers past step 8 that this function applies now — `move.recoil` and Life
    // Orb — is accumulated so `step8_user_alive` can credit it back at the step-7/8 boundary.
    b.late_self_damage = 0;
    b.move_any_damage = any_damage;
    if any_damage {
        let aslot = b.state.side(side).active_index;
        // Drain (Giga Drain, Drain Punch): heal a fraction of the damage dealt — unless the
        // target has Liquid Ooze, which damages the drainer for that amount instead.
        if md.drain.0 > 0 {
            let atk = b.state.side(side).active();
            let amount = round_div(total_dealt * md.drain.0 as i32, md.drain.1 as i32) as i16;
            if def_ability == Ab::LiquidOoze {
                let dmg = amount.min(atk.hp);
                if dmg > 0 && atk.is_alive() {
                    push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
                }
            } else {
                let heal = amount.min(atk.max_hp - atk.hp);
                if heal > 0 && atk.is_alive() {
                    push(b, Instruction::Heal { side, slot: aslot, amount: heal });
                }
            }
        }
        // Move recoil (Brave Bird, Flare Blitz): self-damage a fraction of damage dealt.
        // Gulp Missile loads the user the moment its Surf connects (`onSourceTryPrimaryHit`).
        gulp_missile_swallow(b, side, md);
        // Rock Head and Magic Guard prevent recoil.
        let recoil_immune = matches!(b.state.side(side).active().ability, Ab::RockHead | Ab::MagicGuard);
        if md.recoil.0 > 0 && !recoil_immune {
            let atk = b.state.side(side).active();
            if atk.is_alive() {
                let rec = (round_div(total_dealt * md.recoil.0 as i32, md.recoil.1 as i32) as i16).max(1).min(atk.hp);
                push(b, Instruction::Damage { side, slot: aslot, amount: rec });
                b.late_self_damage += rec;
            }
        }
    }
    // **Life Orb's recoil is NOT gated on damage dealt.** `lifeorb.onAfterMoveSecondarySelf`
    // (`data/items.ts:3408`) tests only `source && source !== target && move.category !== 'Status'
    // && !source.forceSwitchFlag`, and `useMoveInner` reaches it whenever `moveResult` is truthy
    // (`sim/battle-actions.ts:525-534`) — which a hit nullified to 0 damage by Ice Face or Disguise
    // IS. It sat inside the `any_damage` block with drain and recoil, both of which PS really does
    // gate (`if (move.drain && damage)`, `if (move.recoil && move.totalDamage)`).
    // rb1629 d31 t24: a Life Orb Shiftry's Leaf Blade is eaten by an intact Eiscue's Ice Face; PS
    // still chips the Shiftry 30, which is exactly the margin by which Eiscue's Ice Spinner then
    // KOs it (PS 0 HP, engine 19).
    //
    // Magic Guard blocks EVERY non-Move damage source, not just move recoil:
    // `onDamage(damage, target, source, effect) { if (effect.effectType !== 'Move') return
    // false; }`. Life Orb's recoil is `onAfterMoveSecondarySelf` with the ITEM as the
    // effect, Rough Skin / Iron Barbs / Aftermath pass the ABILITY and Rocky Helmet the
    // ITEM — all four are blocked. rb1318 t2 / rb1174 t3: a Life Orb Magic Guard Reuniclus
    // takes the engine's 33 HP of orb recoil and none of PS's.
    if life_orb && b.state.side(side).active().ability != Ab::MagicGuard {
        let aslot = b.state.side(side).active_index;
        let atk = b.state.side(side).active();
        if atk.is_alive() {
            let recoil = (atk.max_hp / 10).max(1).min(atk.hp);
            push(b, Instruction::Damage { side, slot: aslot, amount: recoil });
            b.late_self_damage += recoil;
        }
    }
    // The `runEvent('DamagingHit')` reactions (contact punishers, Stamina, Gooey, …). PS fires
    // that event at the END of EVERY hit; the realized executors therefore call
    // `apply_damaging_hit_reactions` inside their hit loop and pass `per_hit_done`, so this
    // once-per-move fallback only serves the enumeration paths that have no per-hit stream.
    if !per_hit_done {
        apply_damaging_hit_reactions(b, side, md, any_damage, hit_sub, def_item, def_ability);
    }

    // Moxie / Chilling Neigh (+ As One Glastrier): +1 Atk on a KO; Grim Neigh (+ As One
    // Spectrier): +1 SpA on a KO.
    if any_damage
        && !b.state.side(foe).active().is_alive()
        && b.state.side(side).active().is_alive()
        && side_has_living_mon(&b.state, foe)
    {
        match b.state.side(side).active().ability {
            Ab::Moxie | Ab::ChillingNeigh | Ab::AsOneGlastrier => {
                raise_boost(b, side, BoostIndex::Attack, 1);
            }
            Ab::GrimNeigh | Ab::AsOneSpectrier => {
                raise_boost(b, side, BoostIndex::SpecialAttack, 1);
            }
            // Battle Bond (gen9): `onSourceAfterFaint` gives Greninja-Bond +1 Atk / SpA / Spe
            // when its own MOVE knocks a foe out, provided it is untransformed and the foe still
            // has Pokemon left. rb1299 t6.
            //
            // NOT MODELLED: PS's once-only guard `source.abilityState.battleBondTriggered`.
            // `abilityState` is re-inited on every `switchIn` (`battle-actions.ts:142`), so it is
            // once per STINT, not once per battle — and the engine has no per-stint slot for it.
            // `ability_used` is NOT that slot: `convert.rs` derives it from PS's
            // `swordBoost || shieldBoost` and `diff_states` compares it, so writing it here
            // produces a false `ability_used` divergence (measured on rb1299). A faithful fix
            // needs the same treatment `ProtoBooster` got — explicit engine state read by
            // `convert` from `abilityState.battleBondTriggered` and written back by `export`.
            Ab::BattleBond
                if !b.state.side(side).active().transformed
                    && Some(b.state.side(side).active().species)
                        == crate::ids::Species::from_id("greninjabond") =>
            {
                raise_boost(b, side, BoostIndex::Attack, 1);
                raise_boost(b, side, BoostIndex::SpecialAttack, 1);
                raise_boost(b, side, BoostIndex::Speed, 1);
            }
            _ => {}
        }
    }

    // Beast Boost: a KO raises the attacker's highest stat by 1.
    if any_damage
        && !b.state.side(foe).active().is_alive()
        && b.state.side(side).active().ability == Ab::BeastBoost
        && b.state.side(side).active().is_alive()
        && side_has_living_mon(&b.state, foe)
    {
        let stat = match proto_stat(b.state.side(side).active()) {
            crate::ids::StatIndex::Attack => BoostIndex::Attack,
            crate::ids::StatIndex::Defense => BoostIndex::Defense,
            crate::ids::StatIndex::SpecialAttack => BoostIndex::SpecialAttack,
            crate::ids::StatIndex::SpecialDefense => BoostIndex::SpecialDefense,
            _ => BoostIndex::Speed,
        };
        raise_boost(b, side, stat, 1);
    }

    // Aftermath: if a contact move knocks out the holder, the attacker loses 1/4 max HP.
    if any_damage
        && !hit_sub
        && md.flag_contact
        && def_ability == Ab::Aftermath
        && !b.state.side(foe).active().is_alive()
        && b.state.side(side).active().is_alive()
        && b.state.side(side).active().ability != Ab::MagicGuard
    {
        let aslot = b.state.side(side).active_index;
        let atk = b.state.side(side).active();
        let dmg = (atk.max_hp / 4).max(1).min(atk.hp);
        push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
    }

    // A Tera forme that just fainted regresses to its set forme (PS `faintMessages`).
    regress_fainted_tera_formes(b);

    // Destiny Bond: a foe that faints to this move takes the attacker with it. PS
    // `data/moves.ts` `destinybond.condition.onFaint(target, source, effect)` — it fires for
    // `effect.effectType === 'Move'` (non-futuremove) from a non-ally source and calls
    // `source.faint()`. The volatile is only dropped when the HOLDER moves, so it is still up
    // here. rb1369 t8: Weavile Knock Off KOs a Destiny Bonded Froslass and faints with it.
    if any_damage
        && !hit_sub
        && !b.state.side(foe).active().is_alive()
        && b.state.side(foe).volatiles.contains(VolatileStatus::DestinyBond)
        && b.state.side(side).active().is_alive()
    {
        let aslot = b.state.side(side).active_index;
        let hp = b.state.side(side).active().hp;
        push(b, Instruction::Damage { side, slot: aslot, amount: hp });
    }

    // Air Balloon pops on the first hit that lands (the holder becomes grounded).
    if any_damage && !hit_sub {
        let f = b.state.side(foe).active();
        if f.is_alive() && f.item == Item::AirBalloon {
            let fslot = b.state.side(foe).active_index;
            push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: Item::AirBalloon, new: Item::None });
            on_item_lost(b, foe);
        }
    }

    // Self-destructing moves faint the user (even on a miss in PS — handled here for hits;
    // the miss/immune branches go through apply_self_destruct at the call sites).
    if md.self_destruct {
        let p = b.state.side(side).active();
        if p.is_alive() {
            let aslot = b.state.side(side).active_index;
            let hp = p.hp;
            push(b, Instruction::Damage { side, slot: aslot, amount: hp });
        }
    }

    // Track times the target has been hit (Rage Fist). `hits_landed` here is the PS
    // `timesAttacked` increment the caller computed: every EXECUTED hit counts, including
    // hits a Substitute absorbed, but ONLY if at least one hit connected with the Pokémon
    // itself (fully sub-absorbed moves record nothing — PS keeps their per-target damage
    // entry `false`), and nothing executes after a KO. Verified against the pin empirically
    // (skill-link Icicle Spear through a Substitute counts all 5 hits).
    if any_damage && hits_landed > 0 {
        let fslot = b.state.side(foe).active_index;
        let cur = b.state.side(foe).active().times_hit;
        let new = cur.saturating_add(hits_landed).min(250);
        if new != cur {
            push(b, Instruction::SetTimesHit { side: foe, slot: fslot, previous: cur, new });
        }
    }

    // Record the special-move damage the target (foe) just took, so a later Mirror Coat from that
    // side can reflect 2× it this turn (PS `mirrorcoat` volatile `onDamagingHit`). Substitute hits
    // don't touch the mon, so they don't count. Overwrites within the turn (PS keeps the last hit).
    if any_damage && !hit_sub && md.category == MoveCategory::Special {
        let prev = b.state.side(foe).special_damage_taken;
        let new = total_dealt as i16;
        if prev != new {
            push(b, Instruction::SetSpecialDamageTaken { side: foe, previous: prev, new });
        }
    }
    // Symmetric record for physical damage the target just took, so its own Focus Punch (which
    // fails if hit by any damaging move this turn) can detect the hit. Sub hits don't count.
    if any_damage && !hit_sub && md.category == MoveCategory::Physical {
        let prev = b.state.side(foe).physical_damage_taken;
        let new = total_dealt as i16;
        if prev != new {
            push(b, Instruction::SetPhysicalDamageTaken { side: foe, previous: prev, new });
        }
    }

    // Defender reactions keyed on the move's TOTAL damage — `onAfterMoveSecondary`, not
    // `onDamagingHit`, so they stay once per move even for a multi-hit move.
    if any_damage && !hit_sub {
        let f = b.state.side(foe).active();
        if f.is_alive() {
            use crate::ids::Ability as Ab;
            match f.ability {
                Ab::Berserk => {
                    // Fires when the hit drops the holder below half.
                    let hp = f.hp as i32;
                    let pre = hp + total_dealt;
                    if pre * 2 > f.max_hp as i32 && hp * 2 <= f.max_hp as i32 {
                        raise_boost(b, foe, BoostIndex::SpecialAttack, 1);
                    }
                }
                Ab::AngerShell => {
                    // Same half-HP crossing as Berserk: +1 Atk/SpA/Spe, -1 Def/SpD (PS
                    // onAfterMoveSecondary with the checkedAngerShell threshold test).
                    let hp = f.hp as i32;
                    let pre = hp + total_dealt;
                    if pre * 2 > f.max_hp as i32 && hp * 2 <= f.max_hp as i32 {
                        raise_boost(b, foe, BoostIndex::Attack, 1);
                        raise_boost(b, foe, BoostIndex::SpecialAttack, 1);
                        raise_boost(b, foe, BoostIndex::Speed, 1);
                        // Self-inflicted drops: no Clear-Body-family blocking, no Mirror
                        // Armor bounce, no Defiant — just the ±6 clamp.
                        for stat in [BoostIndex::Defense, BoostIndex::SpecialDefense] {
                            let cur = b.state.side(foe).boost(stat);
                            let eff = (cur - 1).clamp(-6, 6) - cur;
                            if eff != 0 {
                                push(b, Instruction::Boost { side: foe, stat, amount: eff });
                            }
                        }
                        apply_white_herb(b, foe);
                    }
                }
                _ => {}
            }
        }
        // Soul-Heart: +1 SpA whenever a Pokémon faints from this hit — and, like every other
        // KO-boost ability, NOT when that faint was the foe's LAST mon. `Battle#boost` opens with
        // `if (this.gen > 5 && !target.side.foePokemonLeft()) return false;` (`sim/battle.ts:2028`),
        // and `faintMessages` decrements `pokemon.side.pokemonLeft` BEFORE `runEvent('Faint')`, so
        // the `onAnyFaint` handler that reaches `boost()` finds the counter already at zero.
        // Moxie / the Neighs / Beast Boost all carry `side_has_living_mon` here; Soul-Heart was
        // the copy that drifted. rb1573 d36 t28: Magearna-Original's Flash Cannon KOs the last
        // Darkrai — PS ends at spa +2 with `statsRaisedThisTurn` false, the engine at +3.
        if !b.state.side(foe).active().is_alive()
            && b.state.side(side).active().ability == crate::ids::Ability::SoulHeart
            && b.state.side(side).active().is_alive()
            && side_has_living_mon(&b.state, foe)
        {
            raise_boost(b, side, BoostIndex::SpecialAttack, 1);
        }
    }

    // Knock Off removes the target's held item (so it no longer triggers Leftovers heals
    // etc.) — unless the item is species-locked to the holder (PS onTakeItem false) or the
    // holder has Sticky Hold (suppressed by Mold Breaker, but def_ability reflects that).
    // Knock Off's `takeItem` (step 8) and the Magician / Pickpocket steals that follow it are
    // ONE ordered sequence and they have been moved into `apply_after_hit_item_moves`. Only the
    // paths that do NOT defer step 7 fire it here (`!per_hit_done`: fixed-damage and the
    // enumeration DP paths); a deferring path cannot answer Knock Off's `pokemon.hp` guard at
    // this line, so its caller fires the whole sequence at the step-7/8 boundary instead.
    if !per_hit_done {
        apply_after_hit_item_moves(b, side, md, def_ability, any_damage, hit_sub, b.after_hit_user_alive);
    }

    // A transformed mon that fainted this hit (the target, or the attacker via a contact
    // punisher / recoil) reverts to its own identity — PS runs clearVolatile on faint.
    // A fainting mon also releases the Mean-Look-family `trapped` it was holding the OTHER
    // side in — PS's `trapped` is linked to the trapper's `trapper` volatile, and linked
    // volatiles are removed the moment their partner clears on faint (before residuals).
    // Likewise a faint releases the OPPONENT's infatuation (Attract's source is gone).
    if !b.state.side(foe).active().is_alive() {
        // Gulp Missile fires from a Cramorant that FAINTED to the hit: PS's `onDamagingHit` guard
        // is `if (!source.hp || !source.isActive || target.isSemiInvulnerable()) return` — it tests
        // the ATTACKER's HP, never the holder's, and step 7 precedes `faintMessages`. The engine
        // runs `apply_post_damage` ahead of the deferred `apply_damaging_hit_step7`, so the
        // forme-revert below would erase the loaded forme before the missile ever fired. Firing it
        // here first is idempotent — step 7's call re-reads the species and finds plain Cramorant.
        apply_gulp_missile(b, foe.other(), foe);
        revert_transform(b, foe);
        revert_battle_only_forme(b, foe);
        if b.state.side(side).volatiles.contains(VolatileStatus::Trapped) {
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Trapped });
        }
        if b.state.side(side).volatiles.contains(VolatileStatus::Attract) {
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Attract });
        }
    }
    if !b.state.side(side).active().is_alive() {
        revert_transform(b, side);
        revert_battle_only_forme(b, side);
        if b.state.side(foe).volatiles.contains(VolatileStatus::Trapped) {
            push(b, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Trapped });
        }
        if b.state.side(foe).volatiles.contains(VolatileStatus::Attract) {
            push(b, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Attract });
        }
    }
}

/// Sitrus Berry: when the holder's HP is at or below 1/2, it eats the berry and heals 1/4
/// of max HP. The berry's *consumption* isn't compared (item is excluded from `relaxed_eq`,
/// and the harness re-projects PS's pre-turn item each turn), so we only emit the heal.
/// Leppa Berry — `onUpdate(pokemon) { const moveSlot = pokemon.moveSlots.find(m => m.pp === 0);
/// if (moveSlot) pokemon.eatItem(); }`, with `onEat` restoring `moveSlot.pp += 10` capped at
/// `maxpp` (data/items.ts leppaberry). Like every item `onUpdate` it fires at the next
/// `eachEvent('Update')`, which for the move's own PP is the acting side's runAction Update — so
/// it is applied at the PP-deduction site, the same decision-boundary state. rb1130 t10 / rb1389
/// t6: a Revival Blessing / 1-PP slot hits 0 and PS eats the berry back to full.
fn maybe_eat_leppa(b: &mut Branch, side: SideId) {
    if matches!(
        b.state.side(side.other()).active().ability,
        crate::ids::Ability::Unnerve | crate::ids::Ability::AsOneGlastrier | crate::ids::Ability::AsOneSpectrier
    ) && b.state.side(side.other()).active().is_alive() {
        return;
    }
    let p = b.state.side(side).active();
    if p.item != Item::LeppaBerry || !p.is_alive() {
        return;
    }
    // PS's `onUpdate` finds the FIRST 0-PP slot; `onEat` then prefers that same slot.
    let Some(mi) = p.moves.iter().position(|m| m.max_pp > 0 && m.pp == 0) else { return };
    let amount = 10u8.min(p.moves[mi].max_pp - p.moves[mi].pp);
    let slot = b.state.side(side).active_index;
    if amount > 0 {
        push(b, Instruction::RestorePp { side, slot, move_index: mi as u8, amount });
    }
    push(b, Instruction::ChangeItem { side, slot, previous: Item::LeppaBerry, new: Item::None });
    on_berry_eaten_id(b, side, Item::LeppaBerry);
}

fn apply_pinch_berry(b: &mut Branch, side: SideId) {
    // (Historical name.) Routed through the consuming implementation — the old version
    // healed without eating the berry, double-healing alongside maybe_eat_sitrus.
    maybe_eat_sitrus(b, side);
}

/// White Herb: once any of the holder's stats is below 0, it restores every negative stage
/// to 0. Consumption isn't compared (item excluded + re-projected), so we only emit the
/// restoring boosts. Triggers regardless of who caused the drop (self-drops included).
fn apply_white_herb(b: &mut Branch, side: SideId) {
    if b.state.side(side).active().item != Item::WhiteHerb {
        return;
    }
    let mut restored = false;
    for stat in BOOST_ORDER {
        let cur = b.state.side(side).boost(stat);
        if cur < 0 {
            push(b, Instruction::Boost { side, stat, amount: -cur });
            restored = true;
        }
    }
    if restored {
        let slot = b.state.side(side).active_index;
        push(b, Instruction::ChangeItem { side, slot, previous: Item::WhiteHerb, new: Item::None });
        on_item_lost(b, side);
    }
}

/// Synchronize: when the holder is given a burn/paralysis/poison/toxic by the opponent, the
/// opponent receives the same status (if it can be statused). `statused` is the side that
/// just got the status; the source is its opponent.
fn apply_synchronize(b: &mut Branch, statused: SideId, status: Status) {
    if b.state.side(statused).active().ability != crate::ids::Ability::Synchronize {
        return;
    }
    if !matches!(status, Status::Burn | Status::Paralysis | Status::Poison | Status::Toxic) {
        return;
    }
    let source = statused.other();
    if status_applies(b.state.side(source).active(), status) {
        let slot = b.state.side(source).active_index;
        push(b, Instruction::ChangeStatus { side: source, slot, previous: Status::None, new: status });
    }
}

/// Justified: +1 Atk when the holder is hit by a damaging Dark-type move.
fn apply_justified(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.ability == crate::ids::Ability::Justified
        && md.typ == Type::Dark
        && md.category != MoveCategory::Status
    {
        raise_boost(b, foe, BoostIndex::Attack, 1);
    }
}

/// Rattled: +1 Spe when the holder is hit by a damaging Bug/Dark/Ghost-type move
/// (PS `onDamagingHit`). The Intimidate reaction lives in `apply_switch_in_ability`.
fn apply_rattled(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.ability == crate::ids::Ability::Rattled
        && matches!(md.typ, Type::Bug | Type::Dark | Type::Ghost)
        && md.category != MoveCategory::Status
    {
        raise_boost(b, foe, BoostIndex::Speed, 1);
    }
}

/// Bug Bite / Pluck: after the hit connects (not through a Substitute), the attacker steals
/// and immediately eats the target's berry — BEFORE the target's own pinch/Sitrus check runs
/// (verified against the pin: the -enditem [from] stealeat lands even when the damage left
/// the holder in Sitrus range). The eat effect applies to the ATTACKER; neither side's
/// `lastItem`/`last_berry` records it (PS `takeItem` sets no `lastItem`, and the attacker
/// never held the berry — only its `ateBerry` flag is set, which the engine doesn't track
/// separately). Sticky Hold on a living target blocks the steal.
fn apply_bug_bite(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if !matches!(md.id.to_id(), "bugbite" | "pluck") {
        return;
    }
    let foe = side.other();
    if !b.state.side(side).active().is_alive() {
        return; // PS: `source.hp &&` — a rocky-helmet-KO'd attacker eats nothing
    }
    let target = b.state.side(foe).active();
    let berry = target.item;
    if !is_berry(berry) {
        return;
    }
    if target.is_alive() && target.ability == crate::ids::Ability::StickyHold {
        return;
    }
    let tslot = b.state.side(foe).active_index;
    push(b, Instruction::ChangeItem { side: foe, slot: tslot, previous: berry, new: Item::None });
    on_item_lost(b, foe);
    apply_berry_eat_effect(b, side, berry);
}

/// The berries the engine models an on-eat effect for, plus the pinch berries randbats
/// carries (whose effects Bug Bite also consumes on steal — a pinch berry eaten by the
/// thief has no effect at high HP, but the item is gone either way).
fn is_berry(item: Item) -> bool {
    matches!(
        item,
        Item::SitrusBerry | Item::LumBerry | Item::ChestoBerry | Item::PechaBerry
            | Item::RawstBerry | Item::CheriBerry | Item::AspearBerry | Item::PersimBerry
            | Item::LeppaBerry | Item::OranBerry
    )
}

/// Thermal Exchange (Baxcalibur): +1 Atk when the holder is hit by a damaging Fire-type
/// move (PS `onDamagingHit`; its burn immunity lives in `status_applies`).
fn apply_thermal_exchange(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.ability == crate::ids::Ability::ThermalExchange
        && md.typ == Type::Fire
        && md.category != MoveCategory::Status
    {
        raise_boost(b, foe, BoostIndex::Attack, 1);
    }
}

/// `frz.onDamagingHit` (`data/conditions.ts:116`): a damaging FIRE-type move (Polar Flare
/// excepted) cures the freeze as part of the hit.
fn apply_thaw_on_hit(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    if md.typ == Type::Fire && md.category != MoveCategory::Status && md.id.to_id() != "polarflare" {
        cure_freeze(b, foe);
    }
}

/// `frz.onAfterMoveSecondary` (`data/conditions.ts:111-115`): a `thawsTarget` move (Scald /
/// Steam Eruption / Matcha Gotcha / Hydro Steam) cures the freeze **after the secondaries**, at
/// `hitStepMoveHitLoop`'s trailing `afterMoveSecondaryEvent` (`battle-actions.ts:1026`).
///
/// The position is the whole mechanic. A frozen target hit by Scald is STILL FROZEN when
/// `secondaries()` tries the 30% burn, so `setStatus` fails on the already-statused mon and the
/// thaw then leaves it with NO status at all. The engine cured at `onHit` — before the
/// secondaries — and the same roll produced a burn. rb1711 d14: a frozen Bellibolt switches into
/// a Scald, PS ends the turn statusless at 170 HP and the engine burned it, then took the 20-HP
/// burn residual, landing at 150.
///
/// PS skips the whole event under Sheer Force (`afterMoveSecondaryEvent`'s guard) — and Sheer
/// Force also strips the secondary, so a Sheer Force Scald neither burns nor thaws.
fn apply_thaw_after_secondary(b: &mut Branch, side: SideId, foe: SideId, md: &crate::data::MoveData) {
    let sheer_force = b.state.side(side).active().ability == crate::ids::Ability::SheerForce
        && md.secondary_chance > 0;
    if !sheer_force && matches!(md.id.to_id(), "scald" | "steameruption" | "matchagotcha" | "hydrosteam") {
        cure_freeze(b, foe);
    }
}

fn cure_freeze(b: &mut Branch, foe: SideId) {
    let d = b.state.side(foe).active();
    if d.is_alive() && d.status == Status::Freeze {
        let slot = b.state.side(foe).active_index;
        push(b, Instruction::ChangeStatus { side: foe, slot, previous: Status::Freeze, new: Status::None });
        clear_status_counter(b, foe, slot);
    }
}

/// The 17 Arceus Plates: the boosted/granted type (PS `item.onPlate`).
fn plate_type(item: Item) -> Option<Type> {
    Some(match item {
        Item::DracoPlate => Type::Dragon,
        Item::DreadPlate => Type::Dark,
        Item::EarthPlate => Type::Ground,
        Item::FistPlate => Type::Fighting,
        Item::FlamePlate => Type::Fire,
        Item::IciclePlate => Type::Ice,
        Item::InsectPlate => Type::Bug,
        Item::IronPlate => Type::Steel,
        Item::MeadowPlate => Type::Grass,
        Item::MindPlate => Type::Psychic,
        Item::PixiePlate => Type::Fairy,
        Item::SkyPlate => Type::Flying,
        Item::SplashPlate => Type::Water,
        Item::SpookyPlate => Type::Ghost,
        Item::StonePlate => Type::Rock,
        Item::ToxicPlate => Type::Poison,
        Item::ZapPlate => Type::Electric,
        _ => return None,
    })
}

/// Sparkling Aria cures the (single) target's burn after the hit: its 100% secondary plants
/// a marker volatile whose removal in `onAfterMove` triggers the cure — so in singles the
/// cure is blocked by Shield Dust / Covert Cloak (the secondary never lands) and removed by
/// Sheer Force. The caller gates on !hit_sub (sound moves bypass subs anyway).
fn apply_sparkling_aria(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.id.to_id() != "sparklingaria" {
        return;
    }
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return;
    }
    let foe = side.other();
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.status == Status::Burn
        && d.ability != crate::ids::Ability::ShieldDust
        && d.item != Item::CovertCloak
    {
        let slot = b.state.side(foe).active_index;
        push(b, Instruction::ChangeStatus { side: foe, slot, previous: Status::Burn, new: Status::None });
        clear_status_counter(b, foe, slot);
    }
}

/// Spirit Shackle: a 100%-chance secondary whose `onHit` traps the target (the Mean-Look
/// `trapped` volatile, released when the trapper leaves). Ghost-types are immune to
/// `trapped`; Shield Dust / Covert Cloak block it and Sheer Force trades it away.
fn apply_spirit_shackle(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.id.to_id() != "spiritshackle" {
        return;
    }
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce
        || !b.state.side(side).active().is_alive()
    {
        return;
    }
    let foe = side.other();
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.ability != crate::ids::Ability::ShieldDust
        && d.item != Item::CovertCloak
        && !d.types.contains(&Type::Ghost)
        && !b.state.side(foe).volatiles.contains(VolatileStatus::Trapped)
    {
        push(b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Trapped });
    }
}

/// Relic Song: after a successful hit, an untransformed Meloetta swaps between its Aria and
/// Pirouette formes (PS `onAfterMoveSecondarySelf` + `formeChange`); the formes share HP but
/// differ in every other base stat. The recompute re-applies the mon's OWN spread via
/// `respread_stats` (PS `setSpecies` -> `spreadModify(baseStats, this.set)`).
fn apply_relic_song_forme(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.id.to_id() != "relicsong" {
        return;
    }
    let p = b.state.side(side).active();
    if !p.is_alive() || p.transformed {
        return;
    }
    let aria = crate::ids::Species::from_id("meloetta").unwrap_or(crate::ids::Species::None);
    let pirouette = crate::ids::Species::from_id("meloettapirouette").unwrap_or(crate::ids::Species::None);
    let target_forme = if p.species == aria {
        pirouette
    } else if p.species == pirouette {
        aria
    } else {
        return;
    };
    let stats = respread_stats(
        crate::data::base_stats(p.species),
        crate::data::base_stats(target_forme),
        p.stats,
        p.level,
    );
    let previous = transform_data_of(&b.state, side);
    let mut new = previous;
    new.species = target_forme;
    new.stats = stats;
    // The formes differ in typing too (Aria Normal/Psychic, Pirouette Normal/Fighting); a
    // terastallized Meloetta keeps its tera typing, but `setSpecies`' enforced `setType` still
    // rewrites PS's `types` array underneath it.
    new.live_types = crate::data::species_types(target_forme);
    if !p.terastallized {
        new.types = new.live_types;
    }
    let slot = b.state.side(side).active_index;
    let previous_base_moves = b.state.side(side).active().base_moves;
    push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
}

/// Weak Armor: when the holder is hit by a physical move, −1 Def and +2 Spe.
fn apply_weak_armor(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    if d.is_alive() && d.ability == crate::ids::Ability::WeakArmor && md.category == MoveCategory::Physical {
        raise_boost(b, foe, BoostIndex::Defense, -1);
        raise_boost(b, foe, BoostIndex::Speed, 2);
    }
}

/// Throat Spray: the user gains +1 SpA after using a sound move.
fn apply_throat_spray(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.flag_sound && b.state.side(side).active().item == Item::ThroatSpray {
        raise_boost(b, side, BoostIndex::SpecialAttack, 1);
        let slot = b.state.side(side).active_index;
        push(b, Instruction::ChangeItem { side, slot, previous: Item::ThroatSpray, new: Item::None });
        on_item_lost(b, side);
    }
}

/// Weakness Policy: when the holder survives a super-effective damaging hit, +2 Atk / +2 SpA.
fn apply_weakness_policy(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    if d.is_alive()
        && d.item == Item::WeaknessPolicy
        && md.category != MoveCategory::Status
        && crate::damage::type_multiplier_fd(md.typ, d.types, is_freeze_dry(md)) > 1.0
    {
        raise_boost(b, foe, BoostIndex::Attack, 2);
        raise_boost(b, foe, BoostIndex::SpecialAttack, 2);
        let slot = b.state.side(foe).active_index;
        push(b, Instruction::ChangeItem { side: foe, slot, previous: Item::WeaknessPolicy, new: Item::None });
        on_item_lost(b, foe);
    }
}

/// The per-hit-count probabilities for a multi-hit move spanning `min..=max` hits. Fixed
/// moves return a single certain count; the variable [2,5] case uses gen5+'s weighted
/// sample [2,2,3,3,4,5]; any other range is treated as uniform.
fn hit_count_probs(min: usize, max: usize) -> Vec<(usize, f32)> {
    if min == max {
        vec![(min, 1.0)]
    } else if (min, max) == (2, 5) {
        vec![(2, 2.0 / 6.0), (3, 2.0 / 6.0), (4, 1.0 / 6.0), (5, 1.0 / 6.0)]
    } else {
        let n = (max - min + 1) as f32;
        (min..=max).map(|k| (k, 1.0 / n)).collect()
    }
}

/// Variable multi-hit bounds after the attacker's modifiers: Skill Link always hits the max;
/// Loaded Dice rolls 4-5 (uniform).
fn multihit_bounds(b: &Branch, side: SideId, md: &crate::data::MoveData) -> (usize, usize) {
    let (mut min, max) = (md.hits as usize, md.hits_max as usize);
    if min != max {
        let p = b.state.side(side).active();
        if p.ability == crate::ids::Ability::SkillLink {
            return (max, max);
        }
        if p.item == Item::LoadedDice {
            min = min.max(4);
        }
    }
    (min, max)
}

/// Bounded multi-hit convolution for an intact Ice Face. Hit one has deterministic zero
/// damage and transforms the target; hits 2..k use Noice Form's Defense and normal rolls.
fn apply_multihit_dp_ice_face(
    b: &Branch, side: SideId, md: &crate::data::MoveData,
    min: usize, max: usize, hit_prob: f32,
) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    let bypass_sub = md.flag_sound
        || b.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
    let sub_hp = b.state.side(foe).substitute_hp;
    if sub_hp > 0
        && !bypass_sub
        && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute)
    {
        return apply_multihit_dp_ice_face_sub(b, side, md, min, max, hit_prob, sub_hp);
    }
    let mut template = b.clone();
    break_ice_face(&mut template, foe);
    let calc = compute_damage(&template, side, md);
    let counts = hit_count_probs(min, max);
    let crit_p = crit_chance(&template, side, md);
    let mut per_hit = Vec::with_capacity(32);
    for i in 0..16 {
        if crit_p < 1.0 {
            per_hit.push((calc.rolls_nocrit[i] as i32, (1.0 / 16.0) * (1.0 - crit_p)));
        }
        if crit_p > 0.0 {
            per_hit.push((calc.rolls_crit[i] as i32, (1.0 / 16.0) * crit_p));
        }
    }

    let cap = template.state.side(foe).active().hp.max(0) as i32;
    let mut conv: HashMap<i32, f32> = HashMap::new();
    conv.insert(0, 1.0);
    let mut dist: HashMap<(i32, usize), f32> = HashMap::new();
    for k in 1..=max {
        // k == 1 is the blocked hit. Each later hit adds one Noice-form damage roll.
        if k > 1 {
            let mut next = HashMap::with_capacity(conv.len() + 32);
            for (&total, &pt) in &conv {
                for &(damage, pd) in &per_hit {
                    *next.entry((total + damage).min(cap)).or_insert(0.0) += pt * pd;
                }
            }
            conv = next;
        }
        if let Some(&(_, pk)) = counts.iter().find(|(count, _)| *count == k) {
            for (&total, &p) in &conv {
                *dist.entry((total, k)).or_insert(0.0) += pk * p;
            }
        }
    }

    let mut out = Vec::with_capacity(dist.len());
    for ((total, hits), p) in dist {
        let mut hb = scaled(&template, hit_prob * p);
        if total > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: total as i16 });
        }
        apply_post_damage(
            &mut hb, side, md, total, total > 0, false, hits.saturating_sub(1) as u8,
            calc.life_orb, calc.def_item, calc.def_ability, false,
        );
        out.push((hb, false));
    }
    out
}

/// Ice Face + Substitute convolution. State stays bounded by
/// `(sub HP + 1) * 2 face states * (target HP + 1) * (max hits + 1)` and is aggressively
/// merged after every hit. A hit first damages Substitute; once it is gone the next hit
/// breaks Ice Face; only later hits damage Noice Form.
fn apply_multihit_dp_ice_face_sub(
    b: &Branch, side: SideId, md: &crate::data::MoveData,
    min: usize, max: usize, hit_prob: f32, sub_hp0: i16,
) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    let intact_calc = compute_damage(b, side, md);
    let mut noice_template = b.clone();
    break_ice_face(&mut noice_template, foe);
    let noice_calc = compute_damage(&noice_template, side, md);
    let crit_p = crit_chance(b, side, md);
    let roll_dist = |calc: &DamageCalc| {
        let mut values = Vec::with_capacity(32);
        for i in 0..16 {
            if crit_p < 1.0 {
                values.push((calc.rolls_nocrit[i] as i32, (1.0 / 16.0) * (1.0 - crit_p)));
            }
            if crit_p > 0.0 {
                values.push((calc.rolls_crit[i] as i32, (1.0 / 16.0) * crit_p));
            }
        }
        values
    };
    let intact_rolls = roll_dist(&intact_calc);
    let noice_rolls = roll_dist(&noice_calc);
    let counts = hit_count_probs(min, max);
    let hp_cap = b.state.side(foe).active().hp.max(0) as i32;

    // (sub remaining, face broken, mon damage, hits that connected with the Pokémon).
    let mut conv: HashMap<(i32, bool, i32, u8), f32> = HashMap::new();
    conv.insert((sub_hp0 as i32, false, 0, 0), 1.0);
    let mut dist: HashMap<(i32, bool, i32, u8, usize), f32> = HashMap::new();
    for k in 1..=max {
        let mut next = HashMap::new();
        for (&(sub, broken, mon, mon_hits), &state_p) in &conv {
            // Once fainted, later nominal hits do not connect or increment Rage Fist.
            if mon >= hp_cap {
                *next.entry((sub, broken, mon, mon_hits)).or_insert(0.0) += state_p;
                continue;
            }
            let rolls = if broken { &noice_rolls } else { &intact_rolls };
            for &(damage, roll_p) in rolls {
                let state = if sub > 0 {
                    // Overflow from the hit that breaks Substitute is discarded.
                    ((sub - damage).max(0), broken, mon, mon_hits)
                } else if !broken {
                    // This hit is consumed by Ice Face.
                    (0, true, mon, mon_hits.saturating_add(1))
                } else {
                    (0, true, (mon + damage).min(hp_cap), mon_hits.saturating_add(1))
                };
                *next.entry(state).or_insert(0.0) += state_p * roll_p;
            }
        }
        conv = next;
        if let Some(&(_, count_p)) = counts.iter().find(|(count, _)| *count == k) {
            for (&(sub, broken, mon, mon_hits), &p) in &conv {
                *dist.entry((sub, broken, mon, mon_hits, k)).or_insert(0.0) += count_p * p;
            }
        }
    }

    let mut out = Vec::with_capacity(dist.len());
    for ((sub, broken, mon, mon_hits, _), p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        let sub_damage = sub_hp0 as i32 - sub;
        if sub_damage > 0 {
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_damage as i16 });
        }
        if sub == 0 {
            push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
        }
        if broken {
            break_ice_face(&mut hb, foe);
        }
        if mon > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: mon as i16 });
        }
        let damaging_hits = mon_hits.saturating_sub(u8::from(broken));
        // A Substitute hit does not suppress later hits that connect with the Pokémon.
        let only_hit_substitute = mon_hits == 0;
        apply_post_damage(
            &mut hb, side, md, sub_damage + mon, sub_damage + mon > 0, only_hit_substitute,
            damaging_hits, noice_calc.life_orb, noice_calc.def_item, noice_calc.def_ability, false,
        );
        out.push((hb, only_hit_substitute));
    }
    out
}

/// The sumset-DP multi-hit path: enumerate the distinct *total* damage a multi-hit move can
/// deal (instead of the 32ʰⁱᵗˢ per-hit product) and emit one branch per total.
///
/// Each hit independently draws from the 16 non-crit rolls (each w.p. (1/16)·(1−CRIT)) or
/// the 16 crit rolls (each w.p. (1/16)·CRIT). We convolve that per-hit distribution up to
/// `max` times, clamping every running total at the target's current HP (overkill collapses
/// to a single "faint" outcome), and after the kᵗʰ convolution fold in `P(hit count = k)` —
/// so a variable [2,5] move's full range of totals is covered. Runs in O(max · HP · 32) time
/// and O(HP) branches. The convolution remains sequential so Sturdy/Focus Sash activation,
/// early fainting, and the number of hits that actually reach the Pokémon stay observable.
fn apply_multihit_dp(b: &Branch, side: SideId, md: &crate::data::MoveData, min: usize, max: usize, hit_prob: f32) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    let calc = compute_damage(b, side, md);
    let counts = hit_count_probs(min, max);

    // Per-hit damage value → probability (32 entries: 16 non-crit + 16 crit).
    let mut per_hit: Vec<(i32, f32)> = Vec::with_capacity(32);
    let crit_p = crit_chance(b, side, md);
    for i in 0..16 {
        if crit_p < 1.0 {
            per_hit.push((calc.rolls_nocrit[i] as i32, (1.0 / 16.0) * (1.0 - crit_p)));
        }
        if crit_p > 0.0 {
            per_hit.push((calc.rolls_crit[i] as i32, (1.0 / 16.0) * crit_p));
        }
    }

    // If the target has a Substitute up (and the move doesn't bypass it), route the convolution
    // through it: PS caps each hit at the sub's remaining HP (overflow is lost), removes the sub
    // when cumulative hits reach its HP, and only hits AFTER the break damage the Pokémon. State
    // is (sub_remaining, mon_damage): while the sub stands mon_damage stays 0, and once it breaks
    // sub_remaining stays 0 — so the support is bounded by sub_hp + target HP, not their product.
    let bypass_sub = md.flag_sound
        || b.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
    let sub_hp0 = b.state.side(foe).substitute_hp as i32;
    if sub_hp0 > 0 && !bypass_sub && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
        return apply_multihit_dp_sub(b, side, md, &per_hit, &counts, &calc, max, hit_prob, sub_hp0);
    }

    // Convolve the per-hit distribution up to `max` times, clamping cumulative damage at the
    // target's HP (all overkill collapses to one faint outcome, bounding the support size).
    // After the kᵗʰ convolution, mix in the branch for "exactly k hits" weighted by P(k).
    // State is (total damage, hits that actually reached the Pokémon, Focus Sash activated).
    // Nominal hits after a faint do not connect and therefore do not increment Rage Fist.
    let cap = b.state.side(foe).active().hp.max(0) as i32;
    let survival = calc.def_ability == crate::ids::Ability::Sturdy || calc.def_item == Item::FocusSash;
    let mut conv: HashMap<(i32, u8, bool), f32> = HashMap::new();
    conv.insert((0, 0, false), 1.0);
    let mut dist: HashMap<(i32, u8, bool), f32> = HashMap::new();
    for k in 1..=max {
        let mut next: HashMap<(i32, u8, bool), f32> = HashMap::with_capacity(conv.len() + 32);
        for (&(t, landed, sash_used), &pt) in &conv {
            for &(v, pv) in &per_hit {
                let key = if t >= cap {
                    (t, landed, sash_used)
                } else {
                    let hp = cap - t;
                    let mut dealt = v.min(hp);
                    let activates = t == 0 && survival && dealt >= hp;
                    if activates { dealt = hp - 1; }
                    (t + dealt, landed.saturating_add(1),
                        sash_used || (activates && calc.def_item == Item::FocusSash))
                };
                *next.entry(key).or_insert(0.0) += pt * pv;
            }
        }
        conv = next;
        if let Some(&(_, pk)) = counts.iter().find(|(c, _)| *c == k) {
            for (&key, &p) in &conv {
                *dist.entry(key).or_insert(0.0) += pk * p;
            }
        }
    }

    // One branch per distinct observable state.
    let mut out = Vec::with_capacity(dist.len());
    for ((total, hits, sash_used), p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        let slot = hb.state.side(foe).active_index;
        if sash_used {
            push(&mut hb, Instruction::ChangeItem {
                side: foe, slot, previous: Item::FocusSash, new: Item::None,
            });
        }
        if total > 0 {
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: total as i16 });
        }
        apply_post_damage(&mut hb, side, md, total, total > 0, false, hits, calc.life_orb, calc.def_item, calc.def_ability, false);
        out.push((hb, false));
    }
    out
}

/// Sumset-DP multi-hit against a Substitute. The convolution state is `(sub_remaining, mon_dmg)`:
/// each hit caps at the sub's remaining HP (overflow lost) until it breaks, after which hits land
/// on the Pokémon. One branch per distinct `(sub_remaining, mon_dmg, hit count)`.
#[allow(clippy::too_many_arguments)]
fn apply_multihit_dp_sub(
    b: &Branch, side: SideId, md: &crate::data::MoveData,
    per_hit: &[(i32, f32)], counts: &[(usize, f32)], calc: &DamageCalc,
    max: usize, hit_prob: f32, sub_hp0: i32,
) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    let cap = b.state.side(foe).active().hp.max(0) as i32;
    // Key: (sub remaining, mon damage, mon-connected hits, executed hits, Focus Sash used).
    // `executed` counts every hit that ran — sub-absorbed included — and freezes once the
    // Pokémon fainted (PS breaks the hit loop); it feeds Rage Fist's `timesAttacked`, which
    // PS bumps by the executed-hit count whenever at least one hit connected with the mon.
    let survival = calc.def_ability == crate::ids::Ability::Sturdy || calc.def_item == Item::FocusSash;
    let mut conv: HashMap<(i32, i32, u8, u8, bool), f32> = HashMap::new();
    conv.insert((sub_hp0, 0, 0, 0, false), 1.0);
    let mut dist: HashMap<(i32, i32, u8, u8, bool), f32> = HashMap::new();
    for k in 1..=max {
        let mut next: HashMap<(i32, i32, u8, u8, bool), f32> = HashMap::with_capacity(conv.len() + 32);
        for (&(sub_rem, mon, mon_hits, executed, sash_used), &pt) in &conv {
            for &(v, pv) in per_hit {
                let key = if sub_rem > 0 {
                    if v < sub_rem {
                        (sub_rem - v, mon, mon_hits, executed.saturating_add(1), sash_used)
                    } else {
                        (0, mon, mon_hits, executed.saturating_add(1), sash_used)
                    }
                } else if mon >= cap {
                    (0, mon, mon_hits, executed, sash_used)
                } else {
                    let hp = cap - mon;
                    let mut dealt = v.min(hp);
                    let activates = mon == 0 && survival && dealt >= hp;
                    if activates { dealt = hp - 1; }
                    (0, mon + dealt, mon_hits.saturating_add(1), executed.saturating_add(1),
                        sash_used || (activates && calc.def_item == Item::FocusSash))
                };
                *next.entry(key).or_insert(0.0) += pt * pv;
            }
        }
        conv = next;
        if let Some(&(_, pk)) = counts.iter().find(|(c, _)| *c == k) {
            for (&key, &p) in &conv {
                *dist.entry(key).or_insert(0.0) += pk * p;
            }
        }
    }

    let mut out = Vec::with_capacity(dist.len());
    for ((sub_rem, mon, mon_hits, executed, sash_used), p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        let sub_dmg = sub_hp0 - sub_rem;
        if sub_dmg > 0 {
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_dmg as i16 });
        }
        if sub_rem == 0 {
            push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
        }
        let slot = hb.state.side(foe).active_index;
        if sash_used {
            push(&mut hb, Instruction::ChangeItem {
                side: foe, slot, previous: Item::FocusSash, new: Item::None,
            });
        }
        if mon > 0 {
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: mon as i16 });
        }
        let only_hit_substitute = mon_hits == 0;
        let times_count = if mon_hits > 0 { executed } else { 0 };
        apply_post_damage(&mut hb, side, md, sub_dmg + mon, sub_dmg + mon > 0,
            only_hit_substitute, times_count, calc.life_orb, calc.def_item, calc.def_ability, false);
        out.push((hb, only_hit_substitute));
    }
    out
}

/// Realized single-path multi-hit executor for a non-multiaccuracy variable-count move (the [2,5]
/// family: bulletseed / iciclespear / rockblast / tailslap / bonerush / pinmissile / scaleshot).
/// Consumes PS's exact draw stream off `cur`: the count `sample([2..5])` (plus the Loaded Dice
/// `random(2)` re-roll when the sample landed below 4), then each hit's crit `randomChance(1,den)`
/// and damage `random(16)`. The per-hit application, KO / Substitute-break termination, and the
/// per-hit crit/damage/ModifyDamage draw emission all reuse [`apply_damage_hit`], so a KO mid-move
/// truncates the stream exactly where PS's `hitStepMoveHitLoop` breaks. Returns the ONE realized
/// branch. Only ever reached with a realized source installed (seed gate / differ).
fn apply_multihit_realized(
    b: &Branch, side: SideId, md: &crate::data::MoveData, hit_prob: f32, mut cur: RealizedCursor,
    beak_blast: bool,
) -> Vec<(Branch, bool)> {
    let mut hb = scaled(b, hit_prob);
    // Hit count. Variable [2,5]: `sample(20)` → count table; a Loaded Dice holder that sampled
    // 2 or 3 re-rolls `5 - random(2)` (battle-actions.ts:867). Skill Link's `onModifyMove` rewrites
    // `multihit` from the [2,5] ARRAY to the plain number 5 BEFORE the hit loop, so PS never reaches
    // the `Array.isArray` sample — a fixed max count with NO draw. Fixed count (non-multiaccuracy)
    // likewise draws nothing here.
    let (lo, hi) = (md.hits as usize, md.hits_max as usize);
    let skill_link = hb.state.side(side).active().ability == crate::ids::Ability::SkillLink;
    let loaded = hb.state.side(side).active().item == Item::LoadedDice;
    let count = if lo != hi && !skill_link {
        let idx = cur.peek("sample", &[20]);
        draw(&mut hb, "sample", &[20], idx, "multihit-count");
        let mut c = MULTIHIT_COUNT_TABLE[(idx as usize).min(19)] as usize;
        if c < 4 && loaded {
            let r = cur.peek("random", &[2]);
            draw(&mut hb, "random", &[2], r, "loadeddice");
            c = 5 - r as usize;
        }
        c
    } else if lo != hi {
        hi // Skill Link: fixed at the max, no count draw
    } else {
        lo
    };
    // Each hit's crit + damage roll is peeled off the cursor INSIDE `apply_damage_hit_rolls`'s
    // loop, not up front: PS's `spreadMoveHit` fires `runEvent('DamagingHit')` after every hit, so
    // a Cursed Body / Toxic Chain / contact-status roll sits between hit n's damage roll and hit
    // n+1's crit roll, and an up-front peek would read that ability slot as the next hit's crit.
    // The loop also steps the cursor over the inter-hit `ModifyDamage` screen shuffle it emits.
    let crit_den = ps_crit_den(&hb, side, md);
    let hit_sub = apply_damage_hit_rolls(
        &mut hb, side, md, HitRolls::Realized { count, cur: &mut cur }, crit_den, None, beak_blast,
    );
    // The per-hit hook already ran every DamagingHit ability roll for this move; suppress the
    // post-hit-loop once-per-move application in `execute_move_inner`.
    hb.per_hit_procs_done = true;
    vec![(hb, hit_sub)]
}

/// Realized single-path executor for a `multiaccuracy` multi-hit move (Population Bomb's 10 hits,
/// Triple Axel / Triple Kick's 3 ascending-power hits). PS rolls each hit past the first its OWN
/// accuracy `randomChance(acc,100)` (battle-actions.ts:907) and a miss ends the move — UNLESS the
/// holder has Loaded Dice, whose `onModifyMove` deletes `multiaccuracy` (items.ts) so every hit
/// lands with no per-hit roll (and Population Bomb's count becomes `10 - random(7)`). `calcs` is the
/// per-hit `DamageCalc` (broadcast if length 1; ascending for Triple Axel). Emits count/accuracy/
/// crit/damage in PS's exact order and applies with KO / Substitute-break termination — a hit past a
/// faint never executes, so the draw stream truncates exactly where PS's `hitStepMoveHitLoop` breaks.
fn apply_multihit_realized_ma(
    b: &Branch, side: SideId, md: &crate::data::MoveData, hit_prob: f32,
    calcs: &[DamageCalc], mds: &[crate::data::MoveData], mut cur: RealizedCursor, beak_blast: bool,
) -> Vec<(Branch, bool)> {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let mut hb = scaled(b, hit_prob);
    let loaded = hb.state.side(side).active().item == Item::LoadedDice;
    // Per-hit accuracy only for the actual multiaccuracy moves, and only when Loaded Dice hasn't
    // deleted `multiaccuracy` (Beat Up is not multiaccuracy → no per-hit accuracy roll).
    let multiacc = is_multiaccuracy_move(md) && !loaded;
    // Hit count: Beat Up = one per participating member (calcs); Triple Axel/Kick 3; Population Bomb
    // 10, and a Loaded Dice holder rolls `10 - random(7)` (battle-actions.ts:877).
    let mut count = if md.id.to_id() == "beatup" { calcs.len() } else { md.hits_max as usize };
    if md.id.to_id() == "populationbomb" && loaded {
        let r = cur.peek("random", &[7]);
        draw(&mut hb, "random", &[7], r, "loadeddice");
        count = count.saturating_sub(r as usize);
    }
    let crit_den = ps_crit_den(&hb, side, md);
    let acc_arg = accuracy_arg(&hb, side, md);
    let (def_ability, def_item, def_maxhp, life_orb) =
        (calcs[0].def_ability, calcs[0].def_item, calcs[0].def_maxhp, calcs[0].life_orb);
    let mut any_damage = false;
    let mut hit_sub = false;
    let mut total: i32 = 0;
    let mut hits_landed: u8 = 0;
    let mut hits_executed: u8 = 0;
    // Set once a per-hit `onDamagingHit` reaction has moved a stat the damage formula reads,
    // after which each remaining hit re-derives its `DamageCalc` from that hit's `MoveData`.
    let mut restat_dirty = false;
    for i in 0..count {
        // Previous hit's deferred step-7 event (see `apply_damaging_hit_step7`) — it lands before
        // this hit's KO check and before its damage is re-derived.
        if hb.pending_damaging_hit.is_some() {
            let pre_inputs = damage_inputs(&hb, side);
            apply_damaging_hit_step7(&mut hb, side, md, false);
            restat_dirty |= damage_inputs(&hb, side) != pre_inputs;
        }
        // PS breaks the loop at the TOP once the target has fainted (before any hit draw).
        if hb.state.side(foe).active().hp <= 0 {
            break;
        }
        // The PREVIOUS iteration's `eachEvent('Update')` (970) — before this hit's accuracy roll,
        // which is where PS puts it. See `emit_prev_hit_update`.
        if i >= 1 {
            emit_prev_hit_update(&mut hb, Some(&mut cur));
            // PS's own last statement: a USER killed by the previous hit's step-7 reaction ends
            // the move (`battle-actions.ts:971-974`) — same rule as in `apply_damage_hit_rolls`,
            // and this is the arm rb5280's Triple Axel takes.
            if hb.state.side(side).active().hp <= 0 {
                break;
            }
        }
        // Per-hit accuracy (hit>1) unless Loaded Dice removed multiaccuracy; a miss ends the move.
        if i >= 1 && multiacc {
            let hit = cur.peek("randomChance", &[acc_arg, 100]) != 0;
            draw(&mut hb, "randomChance", &[acc_arg, 100], hit as i64, "accuracy");
            if !hit {
                break;
            }
        }
        let crit = crit_den > 0 && cur.peek("randomChance", &[1, crit_den]) != 0;
        if crit_den > 0 {
            draw(&mut hb, "randomChance", &[1, crit_den], crit as i64, "crit");
        }
        let roll = (cur.peek("random", &[16]) as usize) & 0x0F;
        draw(&mut hb, "random", &[16], roll as i64, "damage-roll");
        emit_modifydamage_shuffle(&mut hb);
        // Step the peek cursor past the inter-hit ModifyDamage screen shuffle just emitted (the
        // `draw` above advances the real prng / draw log but not this cursor clone), so the next
        // hit's crit/accuracy/damage peek stays aligned on a screened target.
        cur.consume_shuffle(modifydamage_screen_count(&hb));
        let fresh_calc = if restat_dirty {
            Some(compute_damage(&hb, side, &mds[i.min(mds.len() - 1)]))
        } else {
            None
        };
        let calc = fresh_calc.as_ref().unwrap_or(&calcs[i.min(calcs.len() - 1)]);
        let raw = if crit { calc.rolls_crit[roll] } else { calc.rolls_nocrit[roll] };
        let bypass_sub = md.flag_sound
            || hb.state.side(side).active().ability == Ab::Infiltrator;
        let sub_hp = hb.state.side(foe).substitute_hp;
        if sub_hp > 0 && !bypass_sub && hb.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
            let sub_dmg = raw.min(sub_hp);
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_dmg });
            if raw >= sub_hp {
                push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
            }
            total += sub_dmg as i32;
            any_damage = true;
            hit_sub = true;
            hits_executed += 1;
            continue;
        }
        // Ice Face / Disguise are per-hit `onDamage` blocks whose guard is the target's CURRENT
        // `species.id`, so they eat exactly ONE hit and are gone for the rest of the loop — the
        // forme change lands at the per-hit `eachEvent('Update')` (`battle-actions.ts:970`). The
        // other two hit loops (`apply_damage_hit_rolls`, `apply_damage_hit_indexed`) already did
        // this for Ice Face; the multiaccuracy loop is the THIRD copy and had neither. rb1621 d18
        // is the witness: Triple Axel into an intact Mimikyu.
        if ice_face_is_intact(&hb, foe, md) {
            break_ice_face(&mut hb, foe);
            restat_dirty = true;
            continue;
        }
        if disguise_is_intact(&hb, foe, md) {
            bust_disguise(&mut hb, foe);
            restat_dirty = true;
            continue;
        }
        let target_hp = hb.state.side(foe).active().hp;
        if target_hp <= 0 {
            break;
        }
        let mut dmg = raw.min(target_hp);
        if (def_ability == Ab::Sturdy || def_item == Item::FocusSash)
            && target_hp == def_maxhp && dmg >= target_hp
        {
            dmg = target_hp - 1;
            if def_item == Item::FocusSash {
                let slot = hb.state.side(foe).active_index;
                push(&mut hb, Instruction::ChangeItem { side: foe, slot, previous: Item::FocusSash, new: Item::None });
            }
        }
        if dmg > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: dmg });
            any_damage = true;
            total += dmg as i32;
            hits_landed += 1;
            hits_executed += 1;
        }
        // PS `spreadMoveHit` runs `runEvent('DamagingHit')` after EVERY hit — Fezandipiti's Beat Up
        // (rb1049) records `[crit, dmg, randomChance[3,10]@toxicchain] x 6`. The drawing handlers
        // stay here; the no-draw ones are deferred behind step 5's `secondaries()`.
        let pre_inputs = damage_inputs(&hb, side);
        // Step 3 (`onHit`) precedes step 7: Beak Blast burns the attacker on THIS hit, so hits 2+
        // of a Triple Axel / Population Bomb are computed against the halved Attack.
        apply_beak_blast_burn(&mut hb, side, md, beak_blast);
        realized_per_hit_damaging_hit(&mut hb, side, md, &mut cur);
        // rb1198 d29: a Flame Body burn on hit 1 halves the attacker's Atk for hits 2-3.
        restat_dirty |= damage_inputs(&hb, side) != pre_inputs;
        hb.pending_damaging_hit = Some((dmg > 0, def_item, def_ability));
    }
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(&mut hb, side, md, total, any_damage, hit_sub, times_count, life_orb, def_item, def_ability, true);
    hb.per_hit_procs_done = true;
    vec![(hb, hit_sub)]
}

/// Beat Up: one hit per eligible party member (PS `onModifyMove` filter: the user always, plus
/// any ally that is neither fainted nor statused), in party order. Each hit's base power is
/// `5 + floor(species base Atk / 10)` of that member, but the damage otherwise uses the USER's
/// Attack vs the target's Defense (Dark, physical, no contact). We convolve the per-hit damage
/// distributions (each 16 rolls × crit) in order, tracking `(sub_remaining, mon_damage, landed
/// hits, sash_used)` so Substitute break, early faint, and Sturdy/Focus Sash stay exact.
/// Beat Up's per-hit `DamageCalc` list: one entry per participating party member (party order — the
/// user always, plus each ally that is alive and status-free), each with base power
/// `5 + floor(species base Atk / 10)` but the USER's Attack vs the target's Defense.
fn beatup_calcs(b: &Branch, side: SideId, md: &crate::data::MoveData) -> Vec<DamageCalc> {
    beatup_mds(b, side, md).iter().map(|m| compute_damage(b, side, m)).collect()
}

/// Beat Up's per-participant `MoveData` (party order, each with that member's base power), the
/// input `beatup_calcs` folds into `DamageCalc`s. Kept separate so the realized executor can
/// re-derive a hit's calc after a per-hit `onDamagingHit` reaction moved the target's Defense.
fn beatup_mds(b: &Branch, side: SideId, md: &crate::data::MoveData) -> Vec<crate::data::MoveData> {
    let mut mds: Vec<crate::data::MoveData> = Vec::new();
    let s = b.state.side(side);
    // PS iterates `pokemon.side.pokemon` in its CURRENT array order (active-first, swap-tracked),
    // NOT the fixed canonical/teampreview slot order the engine stores. Since each participant's
    // base power is paired with a distinct per-hit roll, the order changes the realized total, so
    // the seed gate installs PS's array order; without it we fall back to canonical slot order (the
    // sumset-DP and differ don't observe the pairing).
    let order: Vec<usize> = s.roster.iter().map(|&x| x as usize).collect();
    for i in order {
        let p = &s.pokemon[i];
        if p.species == crate::ids::Species::None {
            continue;
        }
        let included = i as u8 == s.active_index || (p.is_alive() && p.status == Status::None);
        if !included {
            continue;
        }
        let base_atk = crate::data::base_stats(p.species)[1];
        let bp = (5 + (base_atk / 10)).max(1) as u16;
        let mut m = *md;
        m.base_power = bp;
        mds.push(m);
    }
    mds
}

fn apply_beatup(b: &Branch, side: SideId, md: &crate::data::MoveData, hit_prob: f32) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    // Participating party members (party order): the user always, plus alive, status-free allies.
    let calcs: Vec<DamageCalc> = beatup_calcs(b, side, md);
    if calcs.is_empty() {
        // No participants is impossible (the user is always eligible), but stay total.
        return vec![(scaled(b, hit_prob), false)];
    }
    let crit_p = crit_chance(b, side, md);
    // Per-hit (damage, prob) for a given calc (16 non-crit + 16 crit rolls).
    let per_hit_of = |calc: &DamageCalc| -> Vec<(i32, f32)> {
        let mut v = Vec::with_capacity(32);
        for i in 0..16 {
            if crit_p < 1.0 {
                v.push((calc.rolls_nocrit[i] as i32, (1.0 / 16.0) * (1.0 - crit_p)));
            }
            if crit_p > 0.0 {
                v.push((calc.rolls_crit[i] as i32, (1.0 / 16.0) * crit_p));
            }
        }
        v
    };

    let bypass_sub = md.flag_sound
        || b.state.side(side).active().ability == crate::ids::Ability::Infiltrator;
    let sub_hp0 = if b.state.side(foe).volatiles.contains(VolatileStatus::Substitute) && !bypass_sub {
        b.state.side(foe).substitute_hp as i32
    } else {
        0
    };
    let cap = b.state.side(foe).active().hp.max(0) as i32;
    let survival = calcs[0].def_ability == crate::ids::Ability::Sturdy || calcs[0].def_item == Item::FocusSash;

    // Key: (sub remaining, mon damage, mon-connected hits, executed hits, Focus Sash used).
    // `executed` mirrors PS's timesAttacked bump: every hit that ran counts (sub-absorbed
    // included), frozen once the mon fainted, credited only if some hit connected.
    let mut conv: HashMap<(i32, i32, u8, u8, bool), f32> = HashMap::new();
    conv.insert((sub_hp0, 0, 0, 0, false), 1.0);
    for calc in &calcs {
        let per_hit = per_hit_of(calc);
        let mut next: HashMap<(i32, i32, u8, u8, bool), f32> = HashMap::with_capacity(conv.len() + 32);
        for (&(sub_rem, mon, mon_hits, executed, sash_used), &pt) in &conv {
            for &(v, pv) in &per_hit {
                let key = if sub_rem > 0 {
                    ((sub_rem - v).max(0), mon, mon_hits, executed.saturating_add(1), sash_used)
                } else if mon >= cap {
                    (0, mon, mon_hits, executed, sash_used)
                } else {
                    let hp = cap - mon;
                    let mut dealt = v.min(hp);
                    let activates = mon == 0 && survival && dealt >= hp;
                    if activates {
                        dealt = hp - 1;
                    }
                    (0, mon + dealt, mon_hits.saturating_add(1), executed.saturating_add(1),
                        sash_used || (activates && calcs[0].def_item == Item::FocusSash))
                };
                *next.entry(key).or_insert(0.0) += pt * pv;
            }
        }
        conv = next;
    }

    let mut out = Vec::with_capacity(conv.len());
    for ((sub_rem, mon, mon_hits, executed, sash_used), p) in conv {
        let mut hb = scaled(b, hit_prob * p);
        let sub_dmg = sub_hp0 - sub_rem;
        if sub_dmg > 0 {
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_dmg as i16 });
        }
        if sub_hp0 > 0 && sub_rem == 0 {
            push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
        }
        let slot = hb.state.side(foe).active_index;
        if sash_used {
            push(&mut hb, Instruction::ChangeItem { side: foe, slot, previous: Item::FocusSash, new: Item::None });
        }
        if mon > 0 {
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: mon as i16 });
        }
        let only_hit_substitute = mon_hits == 0 && sub_dmg > 0;
        let times_count = if mon_hits > 0 { executed } else { 0 };
        apply_post_damage(&mut hb, side, md, sub_dmg + mon, mon > 0,
            only_hit_substitute, times_count, calcs[0].life_orb, calcs[0].def_item, calcs[0].def_ability, false);
        out.push((hb, only_hit_substitute));
    }
    out
}

/// Stat indices in `BoostIndex` order, for iterating a `[i8; 7]` boost array.
const BOOST_ORDER: [BoostIndex; BoostIndex::COUNT] = [
    BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
    BoostIndex::SpecialDefense, BoostIndex::Speed, BoostIndex::Accuracy, BoostIndex::Evasion,
];

/// Whether `status` can be inflicted on `p` right now (status-free + type + Purifying Salt).
fn status_applies(p: &crate::state::Pokemon, status: Status) -> bool {
    status_applies_src(p, status, false, false)
}

/// Source-aware variant: `source_corrosion` = the attacker has Corrosion, which lets its
/// move-inflicted poison/toxic bypass the target's Poison/Steel type immunity (PS setStatus).
/// `breaker` = the attacker's move ignores breakable abilities (Mold Breaker family, or a
/// Mycelium Might status move), piercing the ability-based immunities but not type ones
/// (nor Comatose, which is `cantsuppress`).
fn status_applies_src(p: &crate::state::Pokemon, status: Status, source_corrosion: bool, breaker: bool) -> bool {
    if p.status != Status::None || !p.is_alive() {
        return false;
    }
    use crate::ids::Ability as Ab;
    // Blanket status immunities (Purifying Salt is breakable; Comatose is not).
    if p.ability == Ab::Comatose || (!breaker && p.ability == Ab::PurifyingSalt) {
        return false;
    }
    // Shields Down: a Minior in its Meteor shell takes no status at all — PS's `onSetStatus`
    // (abilities.ts:4221) returns `false` unconditionally once `species.id === 'miniormeteor'`
    // (the `-immune` message is the only part gated on the effect being a move), and its
    // `onTryAddVolatile` additionally refuses Yawn (rb1334: the engine's Meteor Minior carried a
    // `yawn` volatile PS never gave it). Shields Down has no `breakable` flag, so Mold Breaker
    // does NOT pierce this.
    if p.ability == Ab::ShieldsDown && !p.transformed && p.species.to_id() == "miniormeteor" {
        return false;
    }
    let ab = if breaker { Ab::None } else { p.ability };
    match status {
        Status::Burn => !p.types.contains(&Type::Fire) && !matches!(ab, Ab::WaterVeil | Ab::WaterBubble | Ab::ThermalExchange),
        Status::Paralysis => !p.types.contains(&Type::Electric) && ab != Ab::Limber,
        Status::Poison | Status::Toxic => {
            (source_corrosion || !(p.types.contains(&Type::Poison) || p.types.contains(&Type::Steel)))
                && ab != Ab::Immunity
        }
        // Insomnia / Vital Spirit / Sweet Veil grant immunity to sleep.
        Status::Sleep => !matches!(ab, Ab::Insomnia | Ab::VitalSpirit | Ab::SweetVeil),
        Status::Freeze => !p.types.contains(&Type::Ice) && ab != Ab::MagmaArmor,
        _ => true,
    }
}

/// Field-level status blocks: Electric Terrain blocks sleep and Misty Terrain blocks all
/// status for grounded targets. Checked alongside `status_applies` at application sites.
fn status_blocked_by_field(state: &State, target: SideId, status: Status) -> bool {
    // Leaf Guard: in (harsh) sun the holder cannot be given a major status at all.
    if state.side(target).active().ability == crate::ids::Ability::LeafGuard
        && matches!(effective_weather(state), Weather::Sun | Weather::HarshSun)
    {
        return true;
    }
    if !is_grounded(state, target) {
        return false;
    }
    match state.terrain {
        crate::ids::Terrain::Electric => status == Status::Sleep,
        crate::ids::Terrain::Misty => true,
        _ => false,
    }
}

/// Lum Berry: cures any status the instant it lands, consuming the berry (a real state
/// change — the old model pretended the status never stuck, leaving the berry in hand).
fn consume_lum_if_statused(b: &mut Branch, side: SideId) {
    if matches!(
        b.state.side(side.other()).active().ability,
        crate::ids::Ability::Unnerve | crate::ids::Ability::AsOneGlastrier | crate::ids::Ability::AsOneSpectrier
    ) && b.state.side(side.other()).active().is_alive() {
        return;
    }
    // Lum also cures confusion (it triggers on any status condition incl. volatile confusion).
    {
        let p = b.state.side(side).active();
        if p.item == Item::LumBerry
            && p.status == Status::None
            && p.is_alive()
            && b.state.side(side).volatiles.contains(VolatileStatus::Confusion)
        {
            let slot = b.state.side(side).active_index;
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Confusion });
            let ct = b.state.side(side).confusion_turns;
            if ct != 0 {
                push(b, Instruction::SetActiveCounter { side, which: crate::instruction::ActiveCounter::Confusion, previous: ct, new: 0 });
            }
            push(b, Instruction::ChangeItem { side, slot, previous: Item::LumBerry, new: Item::None });
            on_berry_eaten_id(b, side, Item::LumBerry);
            return;
        }
    }
    let p = b.state.side(side).active();
    if p.item == Item::LumBerry && p.status != Status::None && p.is_alive() {
        let slot = b.state.side(side).active_index;
        let prev_status = p.status;
        let prev_ctr = p.status_counter;
        push(b, Instruction::ChangeStatus { side, slot, previous: prev_status, new: Status::None });
        if prev_ctr != 0 {
            push(b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 0 });
        }
        push(b, Instruction::ChangeItem { side, slot, previous: Item::LumBerry, new: Item::None });
        on_berry_eaten_id(b, side, Item::LumBerry);
    }
    // Chesto wakes the holder the instant it sleeps.
    let p = b.state.side(side).active();
    if p.item == Item::ChestoBerry && p.status == Status::Sleep && p.is_alive() {
        let slot = b.state.side(side).active_index;
        let prev_ctr = p.status_counter;
        push(b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
        if prev_ctr != 0 {
            push(b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 0 });
        }
        push(b, Instruction::ChangeItem { side, slot, previous: Item::ChestoBerry, new: Item::None });
        // A Chesto is EATEN (`eatItem`), so PS records it in `lastItem` / `ateBerry` — the state
        // the engine calls `last_berry` and Harvest / Cud Chew / Cheek Pouch read. This site
        // called the bare item-lost hook and recorded nothing. rb1347 d61 t56: a Trick hands a
        // sleeping Vaporeon the Chesto and it wakes on the spot; PS ends with
        // `lastItem: chestoberry`, the engine with none.
        on_berry_eaten_id(b, side, Item::ChestoBerry);
    }
}

/// High Jump Kick / Jump Kick / Supercell Slam's crash: `onMoveFail` →
/// `this.damage(source.baseMaxhp / 2, source, source, condition('High Jump Kick'))`.
/// `useMoveInner` fires `MoveFail` whenever `trySpreadMoveHit` returns false — i.e. whenever
/// EVERY target was filtered out by one of the hit steps: accuracy miss, Protect, a
/// semi-invulnerable dodge, a type/ability/flag immunity. The one non-crash failure is
/// "no target at all", which returns before the MoveFail line (`battle-actions.ts:511`).
/// Magic Guard blocks it: the damage source is a Condition, not a Move.
fn apply_crash_damage(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if !matches!(md.id.to_id(), "highjumpkick" | "jumpkick" | "supercellslam") {
        return;
    }
    let p = b.state.side(side).active();
    if p.ability == crate::ids::Ability::MagicGuard || !p.is_alive() {
        return;
    }
    let crash = (p.max_hp / 2).min(p.hp);
    if crash > 0 {
        let slot = b.state.side(side).active_index;
        push(b, Instruction::Damage { side, slot, amount: crash });
    }
}

/// Bookkeeping when the active's held item is consumed or removed: Unburden doubles Speed
/// (modeled as a volatile read by `effective_speed`); Cheek Pouch heals 1/3 max HP when the
/// loss was eating a berry (callers that consume berries call `on_berry_eaten` instead).
fn on_item_lost(b: &mut Branch, side: SideId) {
    if b.state.side(side).volatiles.contains(VolatileStatus::ChoiceLock) {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::ChoiceLock });
    }
    let p = b.state.side(side).active();
    if p.ability == crate::ids::Ability::Unburden
        && !b.state.side(side).volatiles.contains(VolatileStatus::Unburden)
    {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Unburden });
    }
}

/// Berry consumption: item-loss bookkeeping plus Cheek Pouch's 1/3 max HP heal.
/// Sitrus Berry: when damage leaves the holder at 1/2 max HP or less, it eats the berry
/// and heals 1/4 max HP.
fn maybe_eat_sitrus(b: &mut Branch, side: SideId) {
    if matches!(
        b.state.side(side.other()).active().ability,
        crate::ids::Ability::Unnerve | crate::ids::Ability::AsOneGlastrier | crate::ids::Ability::AsOneSpectrier
    ) && b.state.side(side.other()).active().is_alive() {
        return;
    }
    // The Sitrus Berry carries its own `onTryEatItem` (`data/items.ts:5752`):
    // `if (!this.runEvent('TryHeal', pokemon, null, this.effect, pokemon.baseMaxhp / 4)) return false;`
    // — so under Heal Block the berry is not merely a no-op heal, it is NOT EATEN AT ALL and
    // stays in hand. rb1003 t14: Psychic Noise heal-blocks a Cheek Pouch Dedenne and drops it
    // under half in the same hit; PS keeps the berry and stays at 70, the engine ate it and
    // healed 65 + 87.
    if heal_blocked(b, side) {
        return;
    }
    let p = b.state.side(side).active();
    if p.item == Item::SitrusBerry && p.is_alive() && p.hp * 2 <= p.max_hp {
        let slot = b.state.side(side).active_index;
        let amt = (p.max_hp / 4).max(1).min(p.max_hp - p.hp);
        push(b, Instruction::Heal { side, slot, amount: amt });
        push(b, Instruction::ChangeItem { side, slot, previous: Item::SitrusBerry, new: Item::None });
        on_berry_eaten_id(b, side, Item::SitrusBerry);
    }
}

/// Apply a berry's on-eat effect to the active mon WITHOUT consuming an item — used by Cud Chew
/// to re-apply the effect of an already-eaten berry. Covers the healing/status-curing berries
/// that random battles actually carry on Cud Chew users (Sitrus, Lum, single-status cures).
fn apply_berry_eat_effect(b: &mut Branch, side: SideId, berry: Item) {
    let (hp, max, status, slot) = {
        let p = b.state.side(side).active();
        (p.hp, p.max_hp, p.status, b.state.side(side).active_index)
    };
    let cure = |b: &mut Branch, want: Status| {
        if status == want || want == Status::None {
            let counter = b.state.side(side).active().status_counter;
            push(b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
            if counter != 0 {
                push(b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: 0 });
            }
        }
    };
    match berry {
        Item::SitrusBerry => {
            let heal = (max / 4).max(1).min(max - hp);
            if heal > 0 {
                push(b, Instruction::Heal { side, slot, amount: heal });
            }
        }
        Item::LumBerry => {
            if status != Status::None {
                cure(b, Status::None);
            }
            if b.state.side(side).volatiles.contains(VolatileStatus::Confusion) {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Confusion });
            }
        }
        Item::ChestoBerry => cure(b, Status::Sleep),
        Item::PechaBerry => { if matches!(status, Status::Poison | Status::Toxic) { cure(b, status); } }
        Item::RawstBerry => cure(b, Status::Burn),
        Item::CheriBerry => cure(b, Status::Paralysis),
        Item::AspearBerry => cure(b, Status::Freeze),
        _ => {}
    }
}

/// `berry`: the eaten berry id, recorded for Harvest regrowth.
fn on_berry_eaten_id(b: &mut Branch, side: SideId, berry: Item) {
    if berry != Item::None {
        let slot = b.state.side(side).active_index;
        let prev = b.state.side(side).active().last_berry;
        if prev != berry {
            push(b, Instruction::SetLastBerry { side, slot, previous: prev, new: berry });
        }
        // Cud Chew stores the eaten berry with a 2-residual countdown (PS onEatItem sets
        // counter = 2; the same-turn residual ticks it to 1, so the re-eat lands at the END
        // OF THE NEXT TURN — identical net timing to PS's queue-empty `counter--` special
        // case for during-residual eats, since our residual tick runs after the eat sites).
        if b.state.side(side).active().ability == crate::ids::Ability::CudChew {
            let cc = b.state.side(side).active().cudchew_turns;
            if cc != 2 {
                push(b, Instruction::SetCudChew { side, slot, previous: cc, new: 2 });
            }
        }
    }
    on_item_lost(b, side);
    let p = b.state.side(side).active();
    if p.ability == crate::ids::Ability::CheekPouch && p.is_alive() && p.hp < p.max_hp {
        let amt = (p.max_hp / 3).max(1).min(p.max_hp - p.hp);
        let slot = b.state.side(side).active_index;
        push(b, Instruction::Heal { side, slot, amount: amt });
    }
}

/// The no-Mold-Breaker case (switch-in abilities, hazards, residuals, contact abilities).
fn apply_boost_clamped(b: &mut Branch, target: SideId, stat: BoostIndex, delta: i8) -> i8 {
    apply_boost_clamped_ex(b, target, stat, delta, false)
}

/// Apply a stat-stage change to the target, respecting Clear Body (blocks reductions) and
/// the ±6 clamp. Returns the effective change actually applied (0 if blocked/clamped out).
///
/// `breaker` is "this change comes from a move whose user has Mold Breaker / Teravolt /
/// Turboblaze (or `move.ignoreAbility`)". It suppresses the target's `breakable` abilities for
/// exactly this change, which is what PS's `suppressingAbility` (`sim/battle.ts:365`) does — and
/// it is only true while such a MOVE is resolving, so Intimidate / Sticky Web / Octolock / a
/// contact ability's own drop pass `false`. **`Contrary` is `breakable: 1`**: rb1430 d26, a Mold
/// Breaker Tinkaton's Play Rough into a Malamar, where PS applied the −1 Attack as a DROP
/// (2 → 1) and the engine inverted it into a raise (2 → 3). `Full Metal Body` is NOT breakable
/// (`cantsuppress`), which is why the blocker set has to be filtered per-ability.
fn apply_boost_clamped_ex(b: &mut Branch, target: SideId, stat: BoostIndex, delta: i8, breaker: bool) -> i8 {
    use crate::ids::Ability as Ab;
    let seen = |a: Ab| if breaker && ability_breakable(a) { Ab::None } else { a };
    // Contrary inverts the change before anything else (so a "drop" becomes a raise and is no
    // longer blocked by Clear Body / counted as a drop by Defiant).
    let delta = if seen(b.state.side(target).active().ability) == Ab::Contrary { -delta } else { delta };
    // Every apply_boost_clamped call is an OPPONENT-inflicted change on `target` (self-drops go
    // through apply_self_boost), so the "source && target === source" self-skip in PS's onTryBoost
    // handlers never fires here. Protective abilities block foe-inflicted stat *drops*.
    if delta < 0 {
        let tgt = b.state.side(target).active();
        let ab = seen(tgt.ability);
        // Clear Body / Full Metal Body / White Smoke block every stat drop; Flower Veil does so
        // for a Grass-type holder; Big Pecks only Defense; Keen Eye / Mind's Eye only Accuracy.
        let block_all = matches!(ab, Ab::ClearBody | Ab::FullMetalBody | Ab::WhiteSmoke)
            || (ab == Ab::FlowerVeil && tgt.types.contains(&Type::Grass));
        let block_stat = (ab == Ab::BigPecks && stat == BoostIndex::Defense)
            || (matches!(ab, Ab::KeenEye | Ab::MindsEye) && stat == BoostIndex::Accuracy);
        if block_all || block_stat {
            return 0;
        }
        // Mirror Armor reflects a foe-inflicted drop back onto the source instead of the holder.
        if ab == Ab::MirrorArmor {
            let source = target.other();
            if b.state.side(target).boost(stat) > -6 && b.state.side(source).active().is_alive() {
                // The bounced drop respects the source's own Contrary; no re-bounce (PS guards
                // with effect.name === 'Mirror Armor').
                let sab = b.state.side(source).active().ability;
                let sdelta = if sab == Ab::Contrary { -delta } else { delta };
                let scur = b.state.side(source).boost(stat);
                let seff = (scur + sdelta).clamp(-6, 6) - scur;
                if seff != 0 {
                    push(b, Instruction::Boost { side: source, stat, amount: seff });
                    if seff < 0 {
                        mark_stats_lowered(b, source);
                        react_to_stat_drop(b, source);
                    } else {
                        mark_stats_raised(b, source);
                    }
                }
            }
            return 0;
        }
    }
    let cur = b.state.side(target).boost(stat);
    let eff = (cur + delta).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side: target, stat, amount: eff });
        if eff > 0 {
            mark_stats_raised(b, target);
        } else {
            mark_stats_lowered(b, target);
        }
    }
    eff
}

/// PS sets `pokemon.statsRaisedThisTurn` whenever a boost event actually raised a stat
/// (battle.ts `boost()`); it is reset for all actives at the end of every turn. Burning
/// Jealousy's burn is gated on it. Modeled as a volatile on the active (only the active can
/// have raised a stat this turn), cleared on switch-out and at end of turn.
fn mark_stats_raised(b: &mut Branch, side: SideId) {
    if !b.state.side(side).volatiles.contains(VolatileStatus::StatsRaisedThisTurn) {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::StatsRaisedThisTurn });
    }
}

/// PS sets `pokemon.statsLoweredThisTurn` whenever a boost event actually lowered a stat; it is
/// reset for all actives at end of turn. Lash Out's ×2 is gated on it. Modeled as an active-only
/// volatile, cleared on switch-out and at end of turn (mirrors `StatsRaisedThisTurn`).
fn mark_stats_lowered(b: &mut Branch, side: SideId) {
    if !b.state.side(side).volatiles.contains(VolatileStatus::StatsLoweredThisTurn) {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::StatsLoweredThisTurn });
    }
}

/// PS `side.foePokemonLeft()` (`sim/side.ts:364`) for the side that is about to be BOOSTED:
/// does the boosted mon's opponent still have an unfainted Pokémon? `Battle.boost()` returns
/// false without applying anything when it is zero in gen > 5 (`sim/battle.ts:2028`).
///
/// PS counts `side.pokemonLeft`, which `faintMessages` decrements — so this predicate is only
/// interchangeable with "hp > 0" at a site the engine reaches after PS's faint processing.
/// Call it only from such a site.
fn foe_pokemon_left(state: &State, boosted: SideId) -> bool {
    state.side(boosted.other()).pokemon.iter().any(|p| p.is_alive())
}

/// Apply a *self*-boost (Swords Dance, Leaf Storm's −2 SpA, ...). Self-boosts ignore Clear
/// Body but are inverted by Contrary. Returns nothing; clamps to ±6.
fn apply_self_boost(b: &mut Branch, side: SideId, stat: BoostIndex, delta: i8) {
    let delta = if b.state.side(side).active().ability == crate::ids::Ability::Contrary { -delta } else { delta };
    let cur = b.state.side(side).boost(stat);
    let eff = (cur + delta).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side, stat, amount: eff });
        if eff > 0 {
            mark_stats_raised(b, side);
        } else {
            mark_stats_lowered(b, side);
        }
    }
}

/// Raise a stat by `amount` on `side` (positive only; respects the +6 clamp). Used for the
/// reaction abilities, so it bypasses the Clear-Body / re-trigger paths of a normal drop.
fn raise_boost(b: &mut Branch, side: SideId, stat: BoostIndex, amount: i8) {
    let cur = b.state.side(side).boost(stat);
    let eff = (cur + amount).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side, stat, amount: eff });
        if eff > 0 {
            mark_stats_raised(b, side);
        }
    }
}

/// Defiant (+2 Atk) / Competitive (+2 SpA) fire once when an *opponent* lowers a stat. Call
/// after a foe-induced boost event in which at least one stat was actually reduced.
fn react_to_stat_drop(b: &mut Branch, target: SideId) {
    use crate::ids::Ability as Ab;
    match b.state.side(target).active().ability {
        Ab::Defiant => raise_boost(b, target, BoostIndex::Attack, 2),
        Ab::Competitive => raise_boost(b, target, BoostIndex::SpecialAttack, 2),
        _ => {}
    }
    // White Herb is PS's `onUpdate`, so it clears the negative stages left by ANY drop — not
    // just the move-secondary drops the explicit `apply_white_herb` call sites cover. The
    // switch-in ones were missing: Sticky Web's −1 Speed and Intimidate's −1 Attack both go
    // through `react_to_stat_drop` and left the herb unconsumed (rb1146/rb1219/rb1358 — PS ends
    // the switch with item None and boosts back at 0, the engine with WhiteHerb and −1).
    // Defiant/Competitive first: PS runs them as `onAfterEachBoost`, inside the boost, while
    // the herb waits for the Update. Idempotent — the second call finds no item and no negative.
    apply_white_herb(b, target);
}

/// Split a contact hit on a contact-triggered status ability (30%): the defender's Flame
/// Body / Static / Poison Point statuses the attacker, or the attacker's Poison Touch
/// poisons the target. Only one (the first applicable) is modeled; no-op off contact.
/// Beak Blast burns anything that makes CONTACT with its user while the beak is heating.
///
/// `data/moves.ts` beakblast: `priorityChargeCallback(pokemon) { pokemon.addVolatile('beakblast') }`
/// runs at `beforeTurn` (`sim/battle.ts:2764`, `case 'priorityChargeMove'`), so the volatile is up
/// from the START of the turn — long before the move itself, which is priority **-3** and so
/// effectively always moves last. The volatile's `onHit(target, source, move)` fires on the
/// HOLDER as the target of an incoming move and does
/// `if (checkMoveMakesContact(move, source, target)) source.trySetStatus('brn', target)`.
/// `onAfterMove` removes it once Beak Blast resolves.
///
/// Modelled off `Action::foe_pending_move` — the queued, not-yet-executed move of the mon being
/// hit — rather than a new volatile bit: PS's own volatile is `duration: 1` and is gone by every
/// request boundary the gate digests, and "the target has a Beak Blast still queued" is exactly
/// the window in which PS's handler exists. A mon dragged or switched off the field loses its
/// queued action already (`sequence_two_moves`), which is the same lifetime.
///
/// It is `onHit` — step 3 of `spreadMoveHit` — so it lands ahead of the self-drops (4), the
/// secondaries (5) and the `DamagingHit` contact abilities (7); a Flame Body / Static roll still
/// happens, it just finds the attacker already statused. A Substitute hit does NOT burn: PS nulls
/// the target entry (`targets[i] = null`, battle-actions.ts:1085) before `runMoveEffects` runs.
///
/// **`spreadMoveHit` is PER HIT, so on a multi-hit move the burn lands on hit 1 and hits 2+ are
/// computed against the HALVED Attack.** rb5348 d1 t1: a L88 Hitmontop (Technician) Triple Axels a
/// Toucannon that queued Beak Blast. PS's three hits are 48 / 48 / 81 = 177; the engine's were
/// 48 / 96 / 162 = 306, which killed the Toucannon outright and left PS's `randomChance[100,100]
/// @beakblast` unconsumed at decision ONE. The realized hit loops therefore call this per hit and
/// feed the result into their `restat_dirty` re-derivation, exactly as they already do for a Flame
/// Body burn (rb1198 d29). The enumerate/DP path keeps the once-per-move application below, which
/// is exact for the single-hit moves it verifies.
///
/// rb1108 d4 t5: p2's Blissey queues Beak Blast, p1's Ceruledge lands a contact Shadow Sneak
/// (+1 priority) first. PS burns the Ceruledge and it takes the 16-HP residual chip that turn; the
/// engine left it unburned at 89 where PS has 73.
fn apply_beak_blast_burn(b: &mut Branch, side: SideId, md: &crate::data::MoveData, charging: bool) {
    if !md.flag_contact || !charging {
        return;
    }
    let atk = b.state.side(side).active();
    if !atk.is_alive()
        || !status_applies(atk, Status::Burn)
        || status_blocked_by_field(&b.state, side, Status::Burn)
    {
        return;
    }
    let slot = b.state.side(side).active_index;
    push(b, Instruction::ChangeStatus { side, slot, previous: Status::None, new: Status::Burn });
}

fn apply_contact_secondaries(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let atk_ab = b.state.side(side).active().ability;
    let def_ab = b.state.side(foe).active().ability;
    // Effect Spore (defender): a contact hit rolls one d100 — <11 sleep, <21 paralysis,
    // <30 poison, else nothing (PS onDamagingHit). The whole roll is skipped for powder-
    // immune attackers (Grass types, Overcoat, Safety Goggles).
    if def_ab == Ab::EffectSpore && md.flag_contact {
        let a = b.state.side(side).active();
        let powder_immune = a.types.contains(&Type::Grass)
            || a.ability == Ab::Overcoat
            || a.item == Item::SafetyGoggles;
        // PS `runStatusImmunity('powder')` gates the roll: a powder-immune attacker (Grass /
        // Overcoat / Safety Goggles), or an absent source, means NO `random(100)` at all.
        //
        // "Absent" is `this.fainted`, and PS asks it at `runEvent('DamagingHit')` —
        // `battle-actions.ts:1142`, INSIDE `spreadMoveHit`. `move.recoil` and Life Orb's
        // `onAfterMoveSecondarySelf` both land after that, but the engine applies them in
        // `apply_post_damage`, which runs at the end of the hit loop and therefore BEFORE this.
        // Reading `is_alive()` alone asks the question one self-KO too late, so fall back to the
        // snapshot `apply_post_damage` takes for exactly this reason (`after_hit_user_alive`).
        // rb1416 d20 t15: a 3-HP burned Copperajah lands Superpower on a Vileplume and dies to its
        // own Life Orb; PS rolls `random[100] = 65 @ effectspore DamagingHit` first, the engine
        // read a corpse and rolled nothing, and ran a draw behind from turn 15 on.
        if !(a.is_alive() || b.after_hit_user_alive) || powder_immune {
            return vec![b];
        }
        let mut out = Vec::new();
        // PS rolls ONE `this.random(100)`: <11 slp, <21 par, <30 psn, else nothing. Each status
        // range is a real threshold band the realized decoder maps a raw roll into — so a status
        // that CAN'T land (type/ability immunity, field block, sleep clause) still occupies its
        // band as a no-op branch (state unchanged) at that band's threshold `res`, rather than
        // being folded into the noproc range. Folding it in shifts the thresholds and mis-decodes
        // a raw roll (e.g. a psn-range 25 against a Steel target would otherwise select the par
        // band 11). The status result value is a draw-and-discard for the DP path; the band is
        // what the seed-gate/differ realized selection reads.
        for (p, status, res) in [(0.11, Status::Sleep, 0i64), (0.10, Status::Paralysis, 11), (0.09, Status::Poison, 21)] {
            // PS order at `SetStatus`: ability immunities -> terrains (subOrder 2) ->
            // Safeguard (4) -> Sleep Clause Mod (5, LAST). So the clause is only ever REACHED
            // when every earlier gate passed, and only then does it speak.
            let pre_clause = status_applies(b.state.side(side).active(), status)
                && !status_blocked_by_field(&b.state, side, status);
            let clause = pre_clause && status == Status::Sleep && sleep_clause_blocks(&b.state, side);
            let applies = pre_clause && !clause;
            let mut proc = scaled(&b, p);
            draw(&mut proc, "random", &[100], res, "effectspore");
            if clause {
                push(&mut proc, Instruction::SleepClauseBlocked { side });
            }
            if !applies {
                // The roll lands in this band but the status fails: no state change, but the band
                // is retained so realized selection reads the correct threshold.
                out.push(proc);
                continue;
            }
            let slot = proc.state.side(side).active_index;
            push(&mut proc, Instruction::ChangeStatus { side, slot, previous: Status::None, new: status });
            if status == Status::Sleep {
                mark_slept_by_foe(&mut proc, side);
                out.extend(branch_sleep_counter(proc, side));
            } else {
                out.push(proc);
            }
        }
        let mut np = scaled(&b, 0.70);
        draw(&mut np, "random", &[100], 30, "effectspore");
        out.push(np);
        return out;
    }
    // Cute Charm (defender): PS `onDamagingHit` rolls `randomChance(3, 10)` on EVERY contact hit
    // against the holder — the gender / Oblivious / Aroma Veil / already-attracted checks live
    // inside `addVolatile('attract')`, so the roll fires regardless of whether the attacker can
    // actually be infatuated (a 30% draw-and-discard when it can't land). The holder being at
    // 0 HP (pre-faint) doesn't stop the roll. The engine previously emitted NO draw at all.
    if def_ab == Ab::CuteCharm && md.flag_contact {
        let a = b.state.side(side).active();
        let d = b.state.side(foe).active();
        let genders_ok = (a.gender == 1 && d.gender == 2) || (a.gender == 2 && d.gender == 1);
        let blocked = matches!(atk_ab, Ab::Oblivious | Ab::AromaVeil)
            || b.state.side(side).volatiles.contains(VolatileStatus::Attract);
        if a.is_alive() && genders_ok && !blocked {
            let mut proc = scaled(&b, 0.30);
            let mut noproc = scaled(&b, 0.70);
            draw(&mut proc, "randomChance", &[3, 10], 1, "cutecharm");
            draw(&mut noproc, "randomChance", &[3, 10], 0, "cutecharm");
            push(&mut proc, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Attract });
            return vec![proc, noproc];
        }
        // Roll fires but the effect can't land → single draw-and-discard branch.
        let mut b = b;
        draw(&mut b, "randomChance", &[3, 10], 0, "cutecharm");
        return vec![b];
    }
    // The DEFENDER's contact-status `onDamagingHit` abilities. The ATTACKER's
    // `onSourceDamagingHit` ones (Poison Touch, Toxic Chain) are a SEPARATE handler on the same
    // event and live in `apply_source_damaging_hit` — see it for why this is not a `match`.
    if !md.flag_contact {
        return vec![b];
    }
    let status = match def_ab {
        Ab::FlameBody => Status::Burn,
        Ab::Static => Status::Paralysis,
        Ab::PoisonPoint => Status::Poison,
        _ => return vec![b],
    };
    let _ = atk_ab;
    damaging_hit_status_roll(b, side, status)
}

/// The ATTACKER's `onSourceDamagingHit` status abilities — **Poison Touch** (contact-gated) and
/// **Toxic Chain** (any damaging hit). PS runs them in the SAME `runEvent('DamagingHit')` as the
/// defender's Flame Body / Static / Poison Point, as a DIFFERENT handler: `findEventHandlers`
/// collects the target's `on<Event>` first and the source's `onSource<Event>` last, so when both
/// sides carry one, **both roll, target first**.
///
/// The engine had them in a single `match` with the defender's abilities — the defender's arm
/// won and the attacker's roll vanished. rb5413 d2 t3 is the witness and it is DECISION 2: a
/// Muk-Alola (Poison Touch) Shadow Sneaks a Magcargo (Flame Body). PS's stream for the hit is
/// `acc, crit, roll, randomChance[3,10]=false, randomChance[3,10]=false` — TWO 30% rolls — and
/// the engine emitted one. d3's Poison Jab repeats it with `true, false`.
///
/// Both abilities return BEFORE their `randomChance` on a Shield Dust / Covert Cloak target
/// (`abilities.ts:3328`, `:5050`, each with the same "Despite not being a secondary" comment), so
/// that gate suppresses the DRAW, not just the effect. The defender-side abilities status the
/// ATTACKER and have no such check.
fn apply_source_damaging_hit(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let status = match b.state.side(side).active().ability {
        Ab::ToxicChain => Status::Toxic,
        Ab::PoisonTouch if md.flag_contact => Status::Poison,
        _ => return vec![b],
    };
    let t = b.state.side(foe).active();
    if t.ability == Ab::ShieldDust || t.item == Item::CovertCloak {
        return vec![b];
    }
    damaging_hit_status_roll(b, foe, status)
}

/// The `randomChance(3, 10)` every `DamagingHit` status ability shares.
///
/// PS rolls it INDEPENDENT of whether the status can be applied (`trySetStatus` runs inside the
/// proc branch). When it can't land — target already statused / type-immune / fainted this hit —
/// the roll is a single draw-and-discard.
fn damaging_hit_status_roll(b: Branch, target: SideId, status: Status) -> Vec<Branch> {
    let can_land = b.state.side(target).active().is_alive()
        && status_applies(b.state.side(target).active(), status);
    if !can_land {
        let mut b = b;
        draw(&mut b, "randomChance", &[3, 10], 0, "contact-status");
        return vec![b];
    }
    let chance = 0.30;
    let mut proc = scaled(&b, chance);
    let mut noproc = scaled(&b, 1.0 - chance);
    draw(&mut proc, "randomChance", &[3, 10], 1, "contact-status");
    draw(&mut noproc, "randomChance", &[3, 10], 0, "contact-status");
    let slot = proc.state.side(target).active_index;
    push(&mut proc, Instruction::ChangeStatus { side: target, slot, previous: Status::None, new: status });
    vec![proc, noproc]
}

/// Split a hit on the move's flinch chance (×2 under Serene Grace): the proc branch applies
/// the Flinch volatile to the target, which `sequence_two_moves` uses to skip a target that
/// hasn't moved yet. Inner Focus and an already-flinched target are immune.
/// A hit absorbed by the target's Substitute still costs PS the per-secondary `random(100)`
/// rolls (the effects no-op against the `null` target — see the call site in `execute_move`).
/// The target is `null` in `secondaries()`, so target-based strips (Shield Dust / Covert Cloak)
/// don't fire; only Sheer Force (which removes the move's secondaries at `onModifyMove`, before
/// the move) suppresses them. Emits, in PS's per-secondary order, one `random(100)` for the
/// move's target boost/status secondary (or a 100%-effect `extra_secondary_roll_move`) and one
/// for its flinch secondary — draw-and-discard (annotation-only; no state change, no split).
/// (Tri Attack / Dire Claw behind a Substitute — a `sample`-status secondary — are not modeled
/// here; no such matchup exists in the corpus and a partial emit would risk a new offset.)
fn emit_sub_secondary_rolls(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if !annotating() {
        return;
    }
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return;
    }
    // A Substitute does NOT stop `secondaries()` from rolling. `spreadMoveHit` records a sub hit
    // as `damage[i] === true`, and its target filter is
    // `if (!damage[i] && damage[i] !== 0) targets[i] = false` (sim/battle-actions.ts:1108-1110) —
    // `true` is truthy, so the target survives into step 5 and `secondaries()` rolls its
    // `random(100)` per secondary (sim/battle-actions.ts:1364). Only the EFFECT is then blocked.
    //
    // `secondary_chance` is the codegen's view, which is blind to a secondary whose payload is an
    // `onHit` closure (Tri Attack, Dire Claw) — those moves are modelled by their own handlers and
    // report chance 0, so they need naming here or their sub-hit roll goes missing.
    // rb1033 d42: Tri Attack into a Substitute — PS rolls `random[100]@triattack`, the engine
    // rolled nothing and ran one draw behind for the rest of the game.
    let closure_secondary = matches!(md.id.to_id(), "triattack" | "direclaw");
    if md.secondary_chance > 0 || extra_secondary_roll_move(md.id) || closure_secondary {
        draw(b, "random", &[100], 0, "secondary");
    }
    if md.flinch_chance > 0 {
        draw(b, "random", &[100], 0, "flinch");
    }
}

fn apply_flinch_split(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if md.flinch_chance == 0 {
        return vec![b];
    }
    // Sheer Force trades the flinch secondary for its x1.3 power boost.
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return vec![b];
    }
    let mut b = b;
    let foe = side.other();
    let d = b.state.side(foe).active();
    let alive = d.is_alive();
    // Shield Dust / Covert Cloak strip the flinch secondary via PS `ModifySecondaries` BEFORE
    // the roll — no draw. The shield is inert on a fainted mon.
    let shielded = d.ability == crate::ids::Ability::ShieldDust || d.item == Item::CovertCloak;
    if shielded {
        return vec![b];
    }
    // Inner Focus (`onTryAddVolatile`), an already-flinched target, and a fainted target all
    // still let PS roll the flinch `random(100)` in `secondaries()` — the block/no-op happens
    // later in the volatile-add path. Emit the draw-and-discard on a single branch (the effect
    // cannot land, so there is no proc/no-proc split).
    let can_flinch = alive
        && d.ability != crate::ids::Ability::InnerFocus
        && !b.state.side(foe).volatiles.contains(VolatileStatus::Flinch);
    if !can_flinch {
        draw(&mut b, "random", &[100], 0, "flinch");
        return vec![b];
    }
    let pct = if b.state.side(side).active().ability == crate::ids::Ability::SereneGrace {
        (md.flinch_chance as u16 * 2).min(100) as u8
    } else {
        md.flinch_chance
    };
    let chance = pct as f32 / 100.0;
    let mut proc = scaled(&b, chance);
    let mut noproc = scaled(&b, 1.0 - chance);
    // PS models flinch as a move secondary → one `random(100)`.
    draw(&mut proc, "random", &[100], 0, "flinch");
    draw(&mut noproc, "random", &[100], (pct as i64).max(1), "flinch");
    push(&mut proc, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Flinch });
    vec![proc, noproc]
}

/// The per-hit `DamagingHit` ability rolls, realized off a cursor (seed gate / differ only).
///
/// PS fires `runEvent('DamagingHit')` inside `spreadMoveHit` (battle-actions.ts:1142), i.e. ONCE
/// PER CONNECTING HIT of `hitStepMoveHitLoop` — not once per move. The handlers that roll are
/// **Order within the event: every TARGET handler, then every SOURCE handler.**
/// `runEvent('DamagingHit')` is one of the four event ids that are NOT speed-sorted —
/// `['Invulnerability', 'TryHit', 'DamagingHit', 'EntryHazard']` sort with
/// `Battle.compareLeftToRightOrder` (`sim/battle.ts:789-790`), which is `order` ascending
/// (undefined -> 4294967296, so the unordered handlers come LAST, after Rough Skin / Iron Barbs
/// / Aftermath / Innards Out / Electromorphosis / Wind Power at order 1), then `priority`
/// descending, then `index` — and for a single target every `index` is 0, so the sort is stable
/// and the COLLECTION order survives. `findEventHandlers` collects the target's `on<Event>`
/// first and pushes the source's `onSource<Event>` last, so a target's Cursed Body rolls before
/// the attacker's Toxic Chain / Poison Touch regardless of either mon's Speed.
///
/// rb1520 d35 t27: Fezandipiti (Toxic Chain, Spe 210) Moonblasts a Banette (Cursed Body,
/// Spe 174) to 0 HP. PS's stream is `cursedbody=false` then `toxicchain=true`; the engine ran
/// the faster mon's handler first, read `false` as Toxic Chain and `true` as Cursed Body, and
/// Disabled Moonblast where PS badly-poisoned nothing.
///
/// Cursed Body (`randomChance(3,10)` on the target), Toxic Chain (`onSourceDamagingHit`,
/// `randomChance(3,10)` on the attacker) and the contact-status set (Static / Flame Body / Poison
/// Point / Poison Touch / Cute Charm / Effect Spore). The engine's enumerate path applies them once
/// after the whole hit loop, which is exact for a single-hit move and wrong for a multi-hit one:
/// r3 d23 (Koraidon Scale Shot into Froslass) has PS rolling `cursedbody` after EACH hit's
/// crit+damage pair, and rb1049 (Fezandipiti's Beat Up) has `[crit, dmg, toxicchain] x 6`.
///
/// This reuses the very same `apply_contact_secondaries` / `apply_cursed_body` fork functions the
/// post-hit-loop block uses (no second implementation to drift), then collapses the fork to the one
/// branch the cursor's stream dictates and steps the cursor over exactly the draws that branch
/// appended — so hit n+1's crit peek lands on hit n+1's crit, not in hit n's ability slot.
fn realized_per_hit_damaging_hit(
    b: &mut Branch, side: SideId, md: &crate::data::MoveData, cur: &mut RealizedCursor,
) {
    let base = b.draws.len();
    let cands: Vec<Branch> = apply_cursed_body(b.clone(), side, md)
        .into_iter()
        .flat_map(|sb| apply_contact_secondaries(sb, side, md))
        .flat_map(|sb| apply_source_damaging_hit(sb, side, md))
        .collect();
    // Nothing rolled and nothing applied: the common case, no cursor movement.
    if cands.len() == 1 && cands[0].draws.len() == base {
        *b = cands.into_iter().next().unwrap();
        return;
    }
    // Pick the candidate whose appended draws reproduce the cursor's stream. Every candidate is
    // probed against a CLONE, so a failed probe costs nothing; the winner's draws then advance the
    // real cursor.
    // Prefer the candidate whose appended RESULTS reproduce the cursor's stream. Fall back to the
    // first candidate when none does: a fork that can't actually land its effect collapses to a
    // single "draw-and-discard" branch whose recorded result is the placeholder 0, while PS's raw
    // draw may well have been `true` (rb1152 t7: Fezandipiti's Beat Up into a target Toxic Chain
    // cannot badly-poison — PS rolls 0,1,0,0,0,1 and the engine's discard branch always says 0).
    // Either way the cursor must advance by the chosen branch's draw SHAPES, because PS's prng
    // consumed those draws regardless of the outcome; skipping them desyncs every later hit.
    let mut chosen: usize = 0;
    for (i, c) in cands.iter().enumerate() {
        let mut probe = cur.clone();
        let mut ok = true;
        for d in &c.draws[base..] {
            if d.kind == "shuffle" {
                probe.consume_shuffle(d.args[0]);
                continue;
            }
            if probe.peek(d.kind, &d.args) != d.result {
                ok = false;
                break;
            }
        }
        if ok {
            chosen = i;
            break;
        }
    }
    let c = cands.into_iter().nth(chosen).expect("candidate list is non-empty");
    for d in &c.draws[base..] {
        if d.kind == "shuffle" {
            cur.consume_shuffle(d.args[0]);
        } else {
            cur.peek(d.kind, &d.args);
        }
    }
    *b = c;
}

/// Cursed Body (defender): 30% chance to Disable the move that just hit it.
fn apply_cursed_body(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    let foe = side.other();
    // PS Cursed Body `onDamagingHit` rolls `randomChance(3, 10)` whenever the holder (foe) is hit
    // by a non-Struggle damaging move and the SOURCE isn't already Disabled. The roll fires even
    // when the source can't actually be disabled — a source that fainted from the hit (its
    // `.fainted` flag isn't set until after the hit resolves, so it's still present at the
    // DamagingHit event). The engine had skipped the draw entirely in those cases.
    let roll_fires = b.state.side(foe).active().ability == crate::ids::Ability::CursedBody
        && md.id != crate::ids::MoveId::None
        && md.id.to_id() != "struggle"
        && !b.state.side(side).volatiles.contains(VolatileStatus::Disable);
    if !roll_fires {
        return vec![b];
    }
    // The disable only actually lands on a still-living attacker that knows the move.
    let can_land = b.state.side(side).active().is_alive()
        && b.state.side(side).active().moves.iter().any(|m| m.id == md.id);
    if !can_land {
        // Draw-and-discard: PS rolls, the disable no-ops. State validates, so no split.
        let mut b = b;
        draw(&mut b, "randomChance", &[3, 10], 0, "cursedbody");
        return vec![b];
    }
    // randomChance procs use the boolean convention (proc=1, noproc=0) — same as crit / par / frz /
    // Cute Charm / Poison Touch — so the seed-gate `replicate_select` exact-matches the realized
    // `random_chance` value (0/1). (The 0/chance encoding is only for the `random(100)` secondaries,
    // which `replicate_select` threshold-decodes separately.)
    let mut proc = scaled(&b, 0.30);
    draw(&mut proc, "randomChance", &[3, 10], 1, "cursedbody");
    let mut noproc = scaled(&b, 0.70);
    draw(&mut noproc, "randomChance", &[3, 10], 0, "cursedbody");
    push(&mut proc, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Disable });
    let prev = proc.state.side(side).disable;
    // The attacker has already moved this turn -> full 4-turn disable (PS duration 5 - 1
    // only when the target will still move; here it has just moved).
    push(&mut proc, Instruction::SetDisable { side, previous: prev, new: (md.id, 4) });
    vec![proc, noproc]
}

/// Split a hit branch on a move's chance-based target secondary (proc vs no-proc).
/// Fire Spin / Bind / Infestation / … apply the `PartiallyTrapped` volatile with a duration rolled
/// 5-or-6 (PS `this.random(5,7)`, or a fixed 8 with Grip Claw) and a `boundDivisor` snapshotted
/// from the trapper's item (6 with Binding Band, else 8). Blocked by a Substitute. The residual
/// chip and the duration countdown are applied at end of turn (`apply_end_of_turn`).
fn apply_partial_trap(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if md.target_volatile != Some(VolatileStatus::PartiallyTrapped) {
        return vec![b];
    }
    let foe = side.other();
    if !b.state.side(foe).active().is_alive()
        || b.state.side(foe).volatiles.contains(VolatileStatus::PartiallyTrapped)
    {
        return vec![b];
    }
    let item = b.state.side(side).active().item;
    let div = if item == Item::BindingBand { 6 } else { 8 };
    let durations: Vec<(u8, f32)> = if item == Item::GripClaw {
        vec![(8, 1.0)]
    } else {
        vec![(5, 0.5), (6, 0.5)]
    };
    durations
        .into_iter()
        .map(|(turns, p)| {
            let mut nb = scaled(&b, p);
            push(&mut nb, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::PartiallyTrapped });
            // PS `partiallytrapped` onStart rolls `this.random(5, 7)` for the duration — unless
            // Grip Claw fixes it at 8 (no draw). Binding Band changes only the chip divisor, not
            // the duration, so it still rolls. Draw-and-discard (state carries the realized turns).
            if item != Item::GripClaw {
                draw(&mut nb, "random", &[5, 7], turns as i64, "partialtrap");
            }
            let prev = (nb.state.side(foe).partial_trap_turns, nb.state.side(foe).partial_trap_div);
            push(&mut nb, Instruction::SetPartialTrap { side: foe, previous: prev, new: (turns, div) });
            nb
        })
        .collect()
}

/// Burning Jealousy: its 100% "secondary" burns only targets whose stats were raised this
/// turn (PS secondary `onHit` gated on `statsRaisedThisTurn` — invisible to the codegen, so
/// the move carries `secondary_chance: 0` and is special-cased here). Ordinary secondary
/// rules apply: blocked by a Substitute (caller), Shield Dust and Covert Cloak; removed by
/// Sheer Force; respects status/type immunities and field blocks.
fn apply_burning_jealousy(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if md.id.to_id() != "burningjealousy" {
        return;
    }
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return;
    }
    let foe = side.other();
    let eligible = {
        let d = b.state.side(foe).active();
        d.is_alive()
            && d.ability != crate::ids::Ability::ShieldDust
            && d.item != Item::CovertCloak
            && status_applies(d, Status::Burn)
    };
    if !eligible
        || !b.state.side(foe).volatiles.contains(VolatileStatus::StatsRaisedThisTurn)
        || status_blocked_by_field(&b.state, foe, Status::Burn)
    {
        return;
    }
    let slot = b.state.side(foe).active_index;
    push(b, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: Status::Burn });
    apply_synchronize(b, foe, Status::Burn);
    consume_lum_if_statused(b, foe);
}

/// Moves PS models as a 100%-chance target-facing `secondary` (so `secondaries()` rolls one
/// `random(100)` before applying it), but which the engine realizes through a non-secondary field
/// (`target_volatile`, or a dedicated on-hit handler) with `secondary_chance == 0`. PS still
/// consumes the roll (draw-and-discard — the effect always lands); emit it at the secondary site.
/// Because the payload is target-facing, Shield Dust / Covert Cloak strip it before the roll and
/// Sheer Force removes it outright (no draw) — same as any other secondary.
fn extra_secondary_roll_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "saltcure" | "psychicnoise" | "throatchop" | "sparklingaria" | "syrupbomb" | "spiritshackle"
        // Alluring Voice: `secondary:{chance:100, onHit: confuse iff the target raised a stat this
        // turn}` — PS `secondaries()` always rolls one `random(100)`; the confusion is conditional
        // (and, when it lands, rolls its own 2-6 duration — unmodeled, absent from the corpus).
        | "alluringvoice"
        // Burning Jealousy: `secondary:{chance:100, onHit: burn iff statsRaisedThisTurn}` — the roll
        // always fires; the burn (applied in `apply_burning_jealousy`) is conditional.
        | "burningjealousy"
        // Ceaseless Edge / Stone Axe carry an EMPTY `secondary: {}` (present only so Sheer Force can
        // boost them); PS `secondaries()` still rolls one `random(100)` per empty secondary (chance
        // undefined ⇒ always "hits", does nothing). The Spikes / Stealth Rock are laid separately via
        // `onAfterHit`. Shield Dust / Covert Cloak filter the empty secondary out (no `.self`/
        // `.dustproof`) ⇒ no roll — the shared `shielded` guard already handles that.
        | "ceaselessedge" | "stoneaxe"
        // Diamond Storm carries an EMPTY `secondary: {}` too (Sheer-Force marker), so PS
        // `secondaries()` rolls a SECOND `random(100)` after the `self` roll. Its `self` is
        // `{chance:50, boosts:{def:2}}`: the first `random(100)` already emits via the self-drop
        // path (self_boosts=def+2) with matching kind/args — this adds the empty-secondary roll.
        // (The 50% self.chance is unmodeled in state — the def+2 currently applies unconditionally;
        // a STATE caveat, not a draw one. The sole corpus instance procs, so the sweep stays exact.)
        | "diamondstorm"
    )
}

/// Alluring Voice's `secondary: { chance: 100, onHit }` confuses the target only when it RAISED
/// a stat this turn (`target.statsRaisedThisTurn`, data/moves.ts alluringvoice). PS always rolls
/// the secondary's `random(100)` (emitted by `apply_target_secondary` via
/// `extra_secondary_roll_move`); when the condition holds, `addVolatile('confusion')` then rolls
/// its own `random(2, 6)` duration at that same position. rb1364 t23: Leafeon's Swords Dance
/// resolves first, so PS logs `random[2,6]=5` with `effect: confusion, event: Start`.
fn apply_alluringvoice_confusion(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if md.id.to_id() != "alluringvoice" {
        return vec![b];
    }
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return vec![b];
    }
    let foe = side.other();
    let d = b.state.side(foe).active();
    let blocked = d.ability == crate::ids::Ability::ShieldDust || d.item == Item::CovertCloak;
    if !d.is_alive()
        || blocked
        || d.ability == crate::ids::Ability::OwnTempo
        || !b.state.side(foe).volatiles.contains(VolatileStatus::StatsRaisedThisTurn)
        || b.state.side(foe).volatiles.contains(VolatileStatus::Confusion)
    {
        return vec![b];
    }
    let mut b = b;
    push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Confusion });
    let mut branches = branch_confusion_counter(b, foe);
    for nb in &mut branches {
        consume_lum_if_statused(nb, foe);
    }
    branches
}

fn apply_target_secondary(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    let mut b = b;
    // The attacker's Mold Breaker suppresses the target's `breakable` abilities for this move's
    // secondary boosts too — Contrary among them (rb1430 d26).
    let mb_move = move_breaks_abilities(&b, side, md);
    // 100%-secondary moves the engine applies through `target_volatile`/a dedicated handler
    // (secondary_chance == 0) still cost PS one `random(100)` at the secondaries site.
    if extra_secondary_roll_move(md.id)
        && b.state.side(side).active().ability != crate::ids::Ability::SheerForce
    {
        let foe = side.other();
        let shielded = b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak;
        if !shielded {
            draw(&mut b, "random", &[100], 0, "secondary");
        }
    }
    if md.secondary_chance == 0 {
        return vec![b];
    }
    // Sheer Force removes secondary effects entirely (in exchange for the ×1.3 above).
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return vec![b];
    }
    let foe = side.other();
    let has_self = md.secondary_self_boosts.iter().any(|&x| x != 0);
    let alive = b.state.side(foe).active().is_alive();
    // Shield Dust / Covert Cloak strip target-facing (non-`self`) secondaries via PS
    // `ModifySecondaries` BEFORE the `random(100)` roll — so no draw at all.
    //
    // The shield does NOT need the target to be alive. PS gates an ability's handlers on
    // `ignoringAbility()`, whose only liveness test is `!this.isActive` (sim/pokemon.ts:866) —
    // and `isActive` is cleared in `faintMessages` (sim/battle.ts:2579), which runs at the END of
    // the action. A target that just fainted TO THIS MOVE is still in its slot with `isActive`
    // true, so its Shield Dust still strips the secondary and PS makes no roll.
    // rb1343 d34: Flamethrower KOs a 22-HP Shield Dust Ribombee. PS rolls nothing after the damage
    // roll; the engine saw a dead target, dropped the shield, rolled the 10% burn and ran one draw
    // ahead for the rest of the game.
    let shielded = b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
        || b.state.side(foe).active().item == Item::CovertCloak;
    let target_eligible = alive && !shielded;
    // Shield Dust / Covert Cloak remove target-facing secondaries, but PS preserves a
    // secondary's `self` payload (Fiery Dance can still boost its user, including on a KO).
    if !target_eligible && !has_self {
        // PS `secondaries()` still rolls one `random(100)` per secondary when the target has
        // fainted from the hit — the target object is present (not `false`), so the roll fires
        // and the effect merely no-ops. Emit the draw-and-discard on the single branch (both
        // proc/no-proc outcomes are identical here, so no split — Enumerate/Sample unchanged).
        // A Shield-Dust/Covert-Cloak strip, by contrast, removes the secondary before the roll.
        if !shielded {
            draw(&mut b, "random", &[100], 0, "secondary");
        }
        return vec![b];
    }
    // Serene Grace doubles the secondary chance.
    let pct = if b.state.side(side).active().ability == crate::ids::Ability::SereneGrace {
        (md.secondary_chance as u16 * 2).min(100) as u8
    } else {
        md.secondary_chance
    };
    let chance = pct as f32 / 100.0;
    let mut proc = scaled(&b, chance);
    let mut noproc = scaled(&b, 1.0 - chance);
    // PS `secondaries()`: one `random(100)` per secondary (procs when roll < chance).
    draw(&mut proc, "random", &[100], 0, "secondary");
    draw(&mut noproc, "random", &[100], (pct as i64).max(1), "secondary");
    if has_self && proc.state.side(side).active().is_alive() {
        for (i, &delta) in md.secondary_self_boosts.iter().enumerate() {
            if delta != 0 {
                apply_self_boost(&mut proc, side, BOOST_ORDER[i], delta);
            }
        }
    }
    if target_eligible {
        for (i, &delta) in md.secondary_boosts.iter().enumerate() {
            // One `AfterEachBoost` per actually-changed stat (sim/battle.ts:2073).
            if delta != 0 && apply_boost_clamped_ex(&mut proc, foe, BOOST_ORDER[i], delta, mb_move) < 0 {
                react_to_stat_drop(&mut proc, foe);
                apply_white_herb(&mut proc, foe);
            }
        }
    }
    let mut applied_sleep = false;
    let mut applied_status_now = false;
    let pre_clause = target_eligible
        && md.secondary_status != Status::None
        && status_applies_src(proc.state.side(foe).active(), md.secondary_status,
            proc.state.side(side).active().ability == crate::ids::Ability::Corrosion,
            matches!(proc.state.side(side).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze))
        && !status_blocked_by_field(&proc.state, foe, md.secondary_status);
    let clause = pre_clause
        && md.secondary_status == Status::Sleep
        && sleep_clause_blocks(&proc.state, foe);
    if clause {
        push(&mut proc, Instruction::SleepClauseBlocked { side: foe });
    }
    if pre_clause && !clause {
        let slot = proc.state.side(foe).active_index;
        push(&mut proc, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.secondary_status });
        applied_status_now = true;
        applied_sleep = md.secondary_status == Status::Sleep;
        if applied_sleep {
            mark_slept_by_foe(&mut proc, foe);
        }
        apply_synchronize(&mut proc, foe, md.secondary_status);
        consume_lum_if_statused(&mut proc, foe);
        applied_sleep = sleep_survived_or_discard_duration(&mut proc, foe, applied_sleep);
    }
    let mut procs = vec![proc];
    if applied_sleep {
        procs = procs.into_iter().flat_map(|x| branch_sleep_counter(x, foe)).collect();
    }
    // Poison Puppeteer (Pecharunt): a foe it poisons or badly-poisons with a move is also
    // confused. Fires after the status actually lands; the confusion rolls its own 2-5 duration.
    if applied_status_now
        && b.state.side(side).active().ability == crate::ids::Ability::PoisonPuppeteer
        && matches!(md.secondary_status, Status::Poison | Status::Toxic)
    {
        procs = procs
            .into_iter()
            .flat_map(|mut x| {
                if x.state.side(foe).active().is_alive()
                    && !x.state.side(foe).volatiles.contains(VolatileStatus::Confusion)
                    && x.state.side(foe).active().ability != crate::ids::Ability::OwnTempo
                {
                    push(&mut x, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Confusion });
                    return branch_confusion_counter(x, foe);
                }
                vec![x]
            })
            .collect();
    }
    // Chance-based volatile secondaries (Hurricane / Dynamic Punch confusion, Dire Claw ...).
    use crate::instruction::ActiveCounter;
    if target_eligible {
      if let Some(v) = md.secondary_volatile {
        // Aroma Veil also blocks these when they arrive as a damaging move's secondary
        // (Psychic Noise's heal block); breakable, so Mold Breaker attackers pierce it.
        let aroma_veil_blocks = matches!(
            v,
            VolatileStatus::Attract | VolatileStatus::Disable | VolatileStatus::Encore
                | VolatileStatus::HealBlock | VolatileStatus::Taunt | VolatileStatus::Torment
        ) && b.state.side(foe).active().ability == crate::ids::Ability::AromaVeil
            && !matches!(
                b.state.side(side).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
            );
        procs = procs
            .into_iter()
            .flat_map(|mut x| {
                if !aroma_veil_blocks && x.state.side(foe).active().is_alive() && !x.state.side(foe).volatiles.contains(v)
                    && !(v == VolatileStatus::Confusion
                        && x.state.side(foe).active().ability == crate::ids::Ability::OwnTempo)
                {
                    push(&mut x, Instruction::ApplyVolatile { side: foe, volatile: v });
                    if v == VolatileStatus::Confusion {
                        return branch_confusion_counter(x, foe);
                    }
                    // Psychic Noise / Throat Chop: 2-turn countdowns alongside the volatile.
                    if v == VolatileStatus::HealBlock {
                        let prev = x.state.side(foe).heal_block_turns;
                        push(&mut x, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::HealBlock, previous: prev, new: 2 });
                    }
                    if v == VolatileStatus::ThroatChop {
                        let prev = x.state.side(foe).throat_chop_turns;
                        push(&mut x, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::ThroatChop, previous: prev, new: 2 });
                    }
                }
                vec![x]
            })
            .collect();
      }
    }
    procs.push(noproc);
    procs
}

/// Tri Attack's secondary: PS runs a single 20% roll (doubled by Serene Grace), then on proc
/// `this.sample(['brn','par','frz'])` picks one uniformly and `trySetStatus` applies it. Each
/// pick respects the target's existing status / type / field immunities; a pick that can't land
/// simply fails (its 1/3 share collapses back into the no-status outcome). Shield Dust / Covert
/// Cloak remove the whole secondary; Sheer Force does too (the ×1.3 base power is applied in
/// `compute_damage`).
fn apply_triattack_secondary(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    use crate::ids::Ability as Ab;
    if md.id.to_id() != "triattack" {
        return vec![b];
    }
    let foe = side.other();
    if b.state.side(side).active().ability == Ab::SheerForce {
        return vec![b];
    }
    // Shield Dust / Covert Cloak strip the secondary BEFORE the roll (no draw); a fainted-but-
    // present target still rolls (the `trySetStatus` merely no-ops), so gate only on the shield.
    let alive = b.state.side(foe).active().is_alive();
    let shielded = b.state.side(foe).active().ability == Ab::ShieldDust
        || b.state.side(foe).active().item == Item::CovertCloak;
    if shielded {
        return vec![b];
    }
    let pct: u16 = if b.state.side(side).active().ability == Ab::SereneGrace { 40 } else { 20 };
    let chance = pct as f32 / 100.0;
    let sun = effective_weather(&b.state) == Weather::Sun;
    // PS `secondaries()` rolls `random(100)` for the 20% chance; on a proc the secondary's
    // `onHit` runs `this.sample(['brn','par','frz'])` (one `sample[3]` draw) then `trySetStatus`.
    // The sample fires on any proc — even against an already-statused / status-immune target (the
    // set merely no-ops), so the draw is annotated regardless of `can_apply`.
    let mut noproc = scaled(&b, 1.0 - chance);
    draw(&mut noproc, "random", &[100], pct as i64, "secondary");
    let mut out = vec![noproc];
    for (idx, status) in [Status::Burn, Status::Paralysis, Status::Freeze].into_iter().enumerate() {
        let mut pb = scaled(&b, chance / 3.0);
        draw(&mut pb, "random", &[100], 0, "secondary");
        draw(&mut pb, "sample", &[3], idx as i64, "secondary");
        // Freeze additionally fails in harsh sunlight (PS `trySetStatus` weather guard).
        let can_apply = alive
            && status_applies(pb.state.side(foe).active(), status)
            && !status_blocked_by_field(&pb.state, foe, status)
            && !(status == Status::Freeze && sun);
        if can_apply {
            let slot = pb.state.side(foe).active_index;
            push(&mut pb, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: status });
            apply_synchronize(&mut pb, foe, status);
            consume_lum_if_statused(&mut pb, foe);
        }
        out.push(pb);
    }
    out
}

/// Dire Claw's secondary — PS `{chance:50, onHit: this.sample(['psn','par','slp'])}` (moves.ts).
/// The engine carries `secondary_chance == 0` for it (the status isn't a fixed-status secondary),
/// so `apply_target_secondary` never rolls; PS rolls one `random(100)` for the 50% and, on a proc,
/// one `sample[3]` for the status, then `trySetStatus`. Structurally identical to Tri Attack but
/// with a sleep member (index 2), which additionally rolls the `random(2,5)` duration on apply.
/// Sleep respects Sleep Clause; Shield Dust / Covert Cloak / Sheer Force strip the roll entirely.
fn apply_direclaw_secondary(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    use crate::ids::Ability as Ab;
    if md.id.to_id() != "direclaw" {
        return vec![b];
    }
    let foe = side.other();
    if b.state.side(side).active().ability == Ab::SheerForce {
        return vec![b];
    }
    let alive = b.state.side(foe).active().is_alive();
    let shielded = b.state.side(foe).active().ability == Ab::ShieldDust
        || b.state.side(foe).active().item == Item::CovertCloak;
    if shielded {
        return vec![b];
    }
    let pct: u16 = if b.state.side(side).active().ability == Ab::SereneGrace { 100 } else { 50 };
    let chance = pct as f32 / 100.0;
    let corrosion = b.state.side(side).active().ability == Ab::Corrosion;
    let breaker = matches!(b.state.side(side).active().ability,
        Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze);
    let mut noproc = scaled(&b, 1.0 - chance);
    draw(&mut noproc, "random", &[100], pct as i64, "secondary");
    let mut out = vec![noproc];
    for (idx, status) in [Status::Poison, Status::Paralysis, Status::Sleep].into_iter().enumerate() {
        let mut pb = scaled(&b, chance / 3.0);
        draw(&mut pb, "random", &[100], 0, "secondary");
        draw(&mut pb, "sample", &[3], idx as i64, "secondary");
        let pre_clause = alive
            && status_applies_src(pb.state.side(foe).active(), status, corrosion, breaker)
            && !status_blocked_by_field(&pb.state, foe, status);
        let clause = pre_clause && status == Status::Sleep && sleep_clause_blocks(&pb.state, foe);
        let can_apply = pre_clause && !clause;
        if clause {
            push(&mut pb, Instruction::SleepClauseBlocked { side: foe });
        }
        if can_apply {
            let slot = pb.state.side(foe).active_index;
            push(&mut pb, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: status });
            let slept = status == Status::Sleep && pb.state.side(foe).active().status == Status::Sleep;
            if slept {
                mark_slept_by_foe(&mut pb, foe);
            }
            apply_synchronize(&mut pb, foe, status);
            consume_lum_if_statused(&mut pb, foe);
            if sleep_survived_or_discard_duration(&mut pb, foe, slept) {
                // Freshly-applied sleep rolls its `random(2,5)` duration at the slp `onStart`.
                out.extend(branch_sleep_counter(pb, foe));
                continue;
            }
        }
        out.push(pb);
    }
    out
}

/// PS `selfDrops` (battle-actions.ts): a connecting move with `move.self.boosts` (Close Combat
/// −Def/−SpD, Draco Meteor −SpA, Rapid Spin +Spe, Make It Rain −SpA, …) rolls ONE `random(100)`
/// and applies the boost when `typeof self.chance === 'undefined' || roll < self.chance`. Almost
/// every such move has no `chance`, so the roll is a pure draw-and-discard; Diamond Storm's
/// `self: {chance: 50, boosts: {def: 2}}` is the single gen9 exception and genuinely FORKS.
/// The roll is consumed after the damage rolls and before the target secondaries (`selfDrops`
/// precedes `secondaries` in `spreadMoveHit`). Self-boosts apply even through a Substitute.
fn apply_self_drop(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if !md.self_boosts.iter().any(|&x| x != 0) {
        return vec![b];
    }
    let apply = |nb: &mut Branch| {
        for (i, &delta) in md.self_boosts.iter().enumerate() {
            if delta != 0 {
                apply_self_boost(nb, side, BOOST_ORDER[i], delta);
            }
        }
    };
    let pct = md.self_boost_chance;
    if pct == 0 || pct >= 100 {
        let mut b = b;
        draw(&mut b, "random", &[100], 0, "self-drop");
        apply(&mut b);
        return vec![b];
    }
    let chance = pct as f32 / 100.0;
    let mut proc = scaled(&b, chance);
    draw(&mut proc, "random", &[100], 0, "self-drop");
    apply(&mut proc);
    let mut noproc = scaled(&b, 1.0 - chance);
    draw(&mut noproc, "random", &[100], pct as i64, "self-drop");
    vec![proc, noproc]
}

/// A damaging move's remaining deterministic on-hit effects (`move.selfBoost`, 100%-chance self
/// secondaries, target volatiles). The `move.self.boosts` self-drop is handled by
/// [`apply_self_drop`], which runs immediately before this and may fork.
fn apply_damage_secondaries(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hit_sub: bool) {
    // PS `move.selfBoost` (Clanging Scales def−1, …) is applied at battle-actions.ts:521
    // (`if (move.selfBoost && moveResult) this.moveHit(pokemon, pokemon, move, move.selfBoost, …)`)
    // with NO `random(100)` roll — distinct from `move.self.boosts`, which rolls in `selfDrops`.
    // Emitted here (no draw) once the move connected.
    //
    // `Battle.boost()` REFUSES outright when the boosted mon's side has no living foes left:
    // `if (this.gen > 5 && !target.side.foePokemonLeft()) return false` (`sim/battle.ts:2028`).
    // `selfBoost` is the one boost site the engine reaches AFTER PS has already run
    // `faintMessages` (`battle-actions.ts:979`, at the end of `hitStepMoveHitLoop`, which is what
    // decrements `side.pokemonLeft`) — so a move that KOs the LAST foe takes no self-drop.
    // rb1233 d39: Kommo-o's second Clanging Scales KOs the last Farigiraf and PS leaves it at
    // Def −1, not −2. Everything else the engine boosts on a hit (`move.self.boosts`, the
    // secondaries below) runs INSIDE the hit loop, before that decrement, and is unaffected.
    if md.self_boost_only.iter().any(|&d| d != 0) && foe_pokemon_left(&b.state, side) {
        for (i, &delta) in md.self_boost_only.iter().enumerate() {
            if delta != 0 {
                apply_self_boost(b, side, BOOST_ORDER[i], delta);
            }
        }
    }
    // Secondary self-boosts (Trailblaze +Spe, Power-Up Punch +Atk) are SECONDARIES, so Sheer
    // Force removes them (in exchange for the ×1.3 base power it already applied).
    let sheer_force = b.state.side(side).active().ability == crate::ids::Ability::SheerForce
        && md.secondary_self_boosts.iter().any(|&x| x != 0);
    if !sheer_force && md.secondary_chance == 0 && md.secondary_self_boosts.iter().any(|&x| x != 0) {
        // A 100%-chance self-only secondary (`secondary: {chance: 100, self: {boosts}}` —
        // Rapid Spin +Spe, Trailblaze +Spe, Power-Up Punch +Atk, …) is still a SECONDARY: PS
        // `secondaries()` rolls one `random(100)` for it (always < 100 → always applies) before
        // `moveHit`. Emit the draw-and-discard, then apply the guaranteed self-boost. This is a
        // secondary, so it rolls after any `move.self.boosts` self-drop above.
        draw(b, "random", &[100], 0, "secondary");
        for (i, &delta) in md.secondary_self_boosts.iter().enumerate() {
            if delta != 0 {
                apply_self_boost(b, side, BOOST_ORDER[i], delta);
            }
        }
    }
    // Throat Chop's volatile is applied in PS via the secondary's onHit (not volatileStatus),
    // so the codegen can't see it; special-case it here (100% on hit, 2-turn duration). PS's
    // throatchop condition has no `onRestart`, so re-hitting a target that ALREADY has it does
    // nothing — the existing countdown keeps ticking and is not refreshed. Only set the counter
    // when the volatile is applied fresh.
    use crate::instruction::ActiveCounter;
    if md.id == crate::ids::MoveId::from_id("throatchop").unwrap_or(crate::ids::MoveId::None) && !hit_sub {
        let foe = side.other();
        // Throat Chop's lock is `secondary: {chance: 100, onHit(target) { target.addVolatile(
        // 'throatchop') }}` (`data/moves.ts`), so every gate that strips a SECONDARY strips it:
        // the target's Shield Dust / Covert Cloak, and the ATTACKER's Sheer Force, whose
        // `onModifyMove` deletes `move.secondaries` outright in exchange for the ×1.3 base power
        // `compute_damage` already applies. rb1072 d27 is the witness: a Sheer Force Tauros
        // Throat Chops Iron Thorns and PS leaves it unlocked — the engine locked it, which then
        // silently changed what Iron Thorns could pick for the rest of the game.
        let blocked = b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak
            || b.state.side(side).active().ability == crate::ids::Ability::SheerForce;
        if b.state.side(foe).active().is_alive()
            && !blocked
            && !b.state.side(foe).volatiles.contains(VolatileStatus::ThroatChop)
        {
            push(b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::ThroatChop });
            let prev = b.state.side(foe).throat_chop_turns;
            push(b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::ThroatChop, previous: prev, new: 2 });
        }
    }
    // A target volatile (Salt Cure, ...) is blocked by a Substitute. The partial-trap volatile
    // (Fire Spin / Bind / Infestation / …) is applied in the branching `apply_partial_trap` stage
    // instead — it rolls a 5-vs-6-turn duration and snapshots the Binding Band divisor.
    if !hit_sub {
        if let Some(v) = md.target_volatile {
            let foe = side.other();
            // Aroma Veil's `onAllyTryAddVolatile` (`data/abilities.ts:235-243`) returns null for
            // attract / disable / encore / healblock / taunt / torment whenever the source is a
            // MOVE — so it covers this path too, not just the status-move one and the
            // chance-secondary one that already check it. Breakable, so a Mold Breaker attacker
            // pierces it. rb1304 d16 is the witness: Hypno's Psychic Noise (`secondary:
            // {chance: 100, volatileStatus: 'healblock'}`, so `target_volatile` here) hits an
            // Alcremie that just switched in with Aroma Veil; PS blocks the Heal Block, the engine
            // applied it and then withheld the Alcremie's Leftovers for two turns.
            let aroma_veil_blocks = matches!(
                v,
                VolatileStatus::Attract | VolatileStatus::Disable | VolatileStatus::Encore
                    | VolatileStatus::HealBlock | VolatileStatus::Taunt | VolatileStatus::Torment
            ) && b.state.side(foe).active().ability == crate::ids::Ability::AromaVeil
                && !matches!(
                    b.state.side(side).active().ability,
                    crate::ids::Ability::MoldBreaker
                        | crate::ids::Ability::Teravolt
                        | crate::ids::Ability::Turboblaze
                );
            if v != VolatileStatus::PartiallyTrapped
                && !aroma_veil_blocks
                && !b.state.side(foe).volatiles.contains(v)
            {
                push(b, Instruction::ApplyVolatile { side: foe, volatile: v });
                // Heal Block carries a duration counter the end-of-turn residual decrements and
                // expires on; a damaging move that applies it (Psychic Noise, target_volatile
                // HealBlock) must seed the counter or the volatile sticks forever (r2 t8). PS's
                // `durationCallback`: Psychic Noise → 2, Heal Block move → 5.
                if v == VolatileStatus::HealBlock {
                    let dur = if md.id.to_id() == "psychicnoise" { 2 } else { 5 };
                    let prev = b.state.side(foe).heal_block_turns;
                    push(b, Instruction::SetActiveCounter { side: foe, which: ActiveCounter::HealBlock, previous: prev, new: dur });
                }
            }
        }
        // Jaw Lock traps BOTH the user and the target (PS `onHit` adds `trapped` to each). Neither
        // trap ends until one of the two leaves the field.
        if md.id.to_id() == "jawlock" {
            let foe = side.other();
            for s in [side, foe] {
                let p = b.state.side(s).active();
                if p.is_alive()
                    && !p.types.contains(&Type::Ghost)
                    && !b.state.side(s).volatiles.contains(VolatileStatus::Trapped)
                {
                    push(b, Instruction::ApplyVolatile { side: s, volatile: VolatileStatus::Trapped });
                }
            }
        }
        // Clear Smog resets the target's stat stages to 0 on hit.
        if md.id.to_id() == "clearsmog" {
            let foe = side.other();
            for stat in BOOST_ORDER {
                let cur = b.state.side(foe).boost(stat);
                if cur != 0 {
                    push(b, Instruction::Boost { side: foe, stat, amount: -cur });
                }
            }
        }
    }
}

/// Execute a status move from its data: self-heal, hazard, and/or target status, with an
/// accuracy hit/miss branch when the move can miss.
/// Whether a foe-targeting status move reaches PS's `hitStepAccuracy` — i.e. it is not stopped by
/// an earlier hit step. The two status-move immunities that skip accuracy (all other status moves
/// `ignoreImmunity`): powder immunity (Grass / Overcoat / Safety Goggles for a `flag_powder` move)
/// and type immunity for Thunder Wave (the lone `ignoreImmunity:false` status move). Used to gate
/// the accuracy roll when the move is later blocked by a Substitute (PS rolls accuracy first).
/// PS `hitStepTryImmunity`: a move's own `onTryImmunity` callback runs BEFORE `hitStepAccuracy`,
/// so a move it rejects makes **no accuracy draw at all** and applies nothing — even a
/// 100-accuracy move, which would otherwise still roll `randomChance(100, 100)`.
///
/// The gen9 set (read off the pinned dex — every move with an `onTryImmunity`) splits into status
/// moves, handled here, and three damaging ones (Dream Eater's asleep-or-Comatose target,
/// Endeavor's `source.hp < target.hp`, Synchronoise's shared-type target) which sit on the
/// damaging accuracy path and are NOT wired yet (no corpus instance; named open in the scoreboard).
///
/// Note where the engine already implements the same predicate as an EFFECT gate: Attract's
/// gender check lives in `apply_status_target_volatile`, i.e. after the roll. That is right for
/// the state and wrong for the stream — PS decides immunity first. Leech Seed's Grass immunity
/// was missing entirely (the engine seeded Grass types), which is why `leechseed` carried the
/// `rust-extra randomChance[90,100]@accuracy` class.
fn status_try_immunity_fails(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    use crate::ids::Ability as Ab;
    let a = b.state.side(side).active();
    let t = b.state.side(side.other()).active();
    if !t.is_alive() {
        return false;
    }
    // `hasAbility` respects `ignoringAbility()`, so Mold Breaker & co. bypass the ability-reading
    // guards. Worry Seed reads `target.ability` RAW in PS, so Mold Breaker does not bypass it.
    let mold_breaker = matches!(
        a.ability,
        Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze
    );
    match md.id.to_id() {
        // onTryImmunity(target) { return !target.hasType('Grass'); }
        "leechseed" => t.types.contains(&Type::Grass),
        // opposite genders only (0 = genderless never qualifies)
        "attract" | "captivate" => {
            !((a.gender == 1 && t.gender == 2) || (a.gender == 2 && t.gender == 1))
        }
        // return !target.hasAbility('stickyhold')
        "trick" | "switcheroo" => !mold_breaker && t.ability == Ab::StickyHold,
        // if (target.ability === 'truant' || target.ability === 'insomnia') return false
        "worryseed" => matches!(t.ability, Ab::Truant | Ab::Insomnia),
        // return this.dex.getImmunity('trapped', target)  — Ghost is immune to trapping
        "octolock" => t.types.contains(&Type::Ghost),
        _ => false,
    }
}

/// PS `hitStepTryHitEvent` is moveStep **2** — one step EARLIER than `hitStepTryImmunity` (4) and
/// three earlier than `hitStepAccuracy` (5) (`battle-actions.ts:551-566`). A `runEvent('TryHit')`
/// handler that returns `null` stops the move there, so it makes **no accuracy draw at all** —
/// even a 100-accuracy move, which would otherwise still roll `randomChance(100, 100)`.
///
/// **Oblivious** (`data/abilities.ts:2979-2984`) is the gen9 randbats instance: its `onTryHit`
/// returns `null` for `attract`, `captivate` and `taunt`. The engine had it only as an EFFECT gate
/// at the volatile-application site — right for the state, wrong for the stream, exactly the
/// mistake `status_try_immunity_fails`'s doc records for Attract's gender check and Leech Seed's
/// Grass immunity.
///
/// rb5477 d11 t10: an Enamorus Taunts an Oblivious Whiscash. PS draws NOTHING for the whole turn;
/// the engine drew `randomChance[100,100]@accuracy` and ran one draw ahead from turn 10 on, so
/// d12's Zen Headbutt read the accuracy roll PS had already spent, HIT a move PS missed, and
/// over-emitted a `randomChance[1,24]@crit`.
///
/// Oblivious carries `flags: { breakable: 1 }`, so Mold Breaker & co. suppress it.
/// **Aroma Veil is deliberately NOT here**: it is `onAllyTryAddVolatile`, which runs inside
/// `moveHit` — long after the accuracy roll — and the engine's late gate for it is correct.
fn status_try_hit_event_fails(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    use crate::ids::Ability as Ab;
    let a = b.state.side(side).active();
    let t = b.state.side(side.other()).active();
    if !t.is_alive() {
        return false;
    }
    let mold_breaker = matches!(a.ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze);
    !mold_breaker
        && t.ability == Ab::Oblivious
        && matches!(md.id.to_id(), "attract" | "captivate" | "taunt")
}

fn status_move_reaches_accuracy(b: &Branch, side: SideId, md: &crate::data::MoveData) -> bool {
    use crate::ids::Ability as Ab;
    let t = b.state.side(side.other()).active();
    if !t.is_alive() {
        return false;
    }
    if status_try_hit_event_fails(b, side, md) {
        return false;
    }
    if md.flag_powder
        && (t.types.contains(&Type::Grass) || t.ability == Ab::Overcoat || t.item == Item::SafetyGoggles)
    {
        return false;
    }
    if md.id.to_id() == "thunderwave" && crate::damage::type_multiplier(md.typ, t.types) == 0.0 {
        return false;
    }
    if status_try_immunity_fails(b, side, md) {
        return false;
    }
    true
}

fn execute_status_move(
    mut b: Branch,
    side: SideId,
    md: &crate::data::MoveData,
    foe_pending: Option<crate::ids::MoveId>,
) -> Vec<Branch> {
    let foe = side.other();
    let foe_moves_later = foe_pending.is_some();

    // PS `hitStepTryImmunity` is moveStep **3**, before `hitStepAccuracy` (4) — a move refused
    // there makes NO draw and applies nothing. It has to be checked before every move-specific
    // branch below, because several of those branches emit their own accuracy draw and would
    // otherwise emit it for a move PS never accuracy-rolled.
    //
    // This used to live 700 lines further down, after the whole `md.id` special-case chain, and
    // Trick/Switcheroo — which is IN that chain and emits its own accuracy draw — carried a
    // comment asserting the opposite ("Sticky Hold blocks the item swap later at `onTakeItem`,
    // not before accuracy"). It does not: `trick.onTryImmunity(target) { return
    // !target.hasAbility('stickyhold'); }` (`data/moves.ts:19886`) is exactly moveStep 3.
    // rb5062 d6: a Choice-Scarf Hoopa Tricks a Sticky Hold Dipplin; PS's three draws for the turn
    // are Giga Drain's accuracy/crit/roll and nothing else, the engine drew a fourth, and every
    // draw after it read the wrong PRNG value. Two engine copies of one PS predicate, again.
    if status_try_immunity_fails(&b, side, md) {
        return vec![b];
    }
    // ...and moveStep **2**, one earlier still: an ability `onTryHit` that returns `null`
    // (Oblivious vs Attract / Captivate / Taunt). See `status_try_hit_event_fails`.
    if status_try_hit_event_fails(&b, side, md) {
        return vec![b];
    }

    // Sleep Talk: `onTry` requires the user to be asleep (or Comatose) — used awake the move
    // simply fails, with no draw. `onHit` samples uniformly over the user's OTHER usable move
    // slots (PS `sleeptalk` excludes `nosleeptalk`/`charge`-flagged moves and empty slots; PP is
    // NOT consulted) and then `actions.useMove`s the pick, which resolves as a full sub-move with
    // its own complete draw stream (accuracy, crit, damage, secondaries) while the user stays
    // asleep — `useMove` fires no `BeforeMove` event, pays no PP, and does no `moveUsed`
    // bookkeeping, so `lastMove`/streak stay on Sleep Talk itself.
    if md.id.to_id() == "sleeptalk" {
        let user = b.state.side(side).active();
        if user.status != Status::Sleep && user.ability != crate::ids::Ability::Comatose {
            return vec![b];
        }
        let pool: Vec<crate::ids::MoveId> = user
            .moves
            .iter()
            .map(|m| m.id)
            .filter(|&id| {
                if id == crate::ids::MoveId::None {
                    return false;
                }
                let cd = move_data(id);
                !cd.flag_nosleeptalk && !cd.flag_charge
            })
            .collect();
        if pool.is_empty() {
            return vec![b]; // PS: `if (!randomMove) return false` — no sample draw
        }
        let n = pool.len() as i32;
        let p = 1.0 / pool.len() as f32;
        return pool
            .into_iter()
            .enumerate()
            .flat_map(|(idx, called_id)| {
                let mut nb = scaled(&b, p);
                draw(&mut nb, "sample", &[n], idx as i64, "sleeptalk");
                // **Pressure taxes the CALLER, for the CALLED move's targets.**
                // `useMoveInner` (`sim/battle-actions.ts:472-483`):
                //   const callerMoveForPressure = sourceEffect && sourceEffect.pp ? sourceEffect : null;
                //   if (!sourceEffect || callerMoveForPressure || sourceEffect.id === 'pursuit') {
                //       let extraPP = 0;
                //       for (const source of pressureTargets) { extraPP += runEvent('DeductPP', …) }
                //       if (extraPP > 0) pokemon.deductPP(callerMoveForPressure || moveOrMoveName, extraPP);
                //   }
                // Sleep Talk has `pp: 10`, so it IS a `callerMoveForPressure`: the loop runs over
                // the CALLED move's pressure targets and the extra PP comes off **Sleep Talk's own
                // slot**. So a Sleep Talk that calls a foe-targeting move into a Pressure holder
                // costs 2 PP of Sleep Talk and 0 of the called move.
                // rb5098 d28 t24: a Guts mon Sleep Talks into a Pressure Slowbro; PS leaves Sleep
                // Talk at 13 PP, the engine at 14.
                let called_md = move_data(called_id);
                let user_is_ghost = nb.state.side(side).active().types.contains(&Type::Ghost);
                let foe_active = nb.state.side(side.other()).active();
                if foe_active.is_alive()
                    && foe_active.ability == crate::ids::Ability::Pressure
                    && pressure_affected(&called_md, user_is_ghost)
                {
                    // `deductPP` resolves the slot by move id and bails at 0 PP.
                    let slot = nb.state.side(side).active_index;
                    let caller_idx = nb.state.side(side).active().moves.iter()
                        .position(|m| m.id == md.id);
                    if let Some(mi) = caller_idx {
                        if nb.state.side(side).active().moves[mi].pp > 0 {
                            push(&mut nb, Instruction::DecrementPp {
                                side, slot, move_index: mi as u8, amount: 1,
                            });
                        }
                    }
                }
                // `dispatch_move_inner`, not `execute_move`: PS's `useMove` skips the whole
                // BeforeMove gauntlet (and the Glaive Rush drop that sits above it).
                dispatch_move_inner(
                    nb,
                    Action {
                        side,
                        move_idx: 0,
                        pivot: Pivot::Stay,
                        shell_phys: None,
                        foe_pending_move: foe_pending,
                        custap: false,
                        struggling: false, // a called move (Sleep Talk) never Struggles
                        external_move: Some(called_id),
                        called: true,
                    },
                )
            })
            .collect();
    }

    // Powder moves have no effect on Grass types, Overcoat, or Safety Goggles holders.
    if md.flag_powder && md.target != crate::data::MoveTarget::User {
        let t = b.state.side(foe).active();
        if t.types.contains(&Type::Grass)
            || t.ability == crate::ids::Ability::Overcoat
            || t.item == Item::SafetyGoggles
        {
            return vec![b];
        }
    }

    // Protect family: succeeds with probability 1/3^n on the (n+1)ᵗʰ consecutive use (n is
    // the stall counter). Success sets the Protect volatile and bumps the counter; failure
    // resets it. We enumerate both branches so PS's actual outcome is always a member.
    if is_protect_move(md.id) {
        let n = b.state.side(side).stall_counter;
        // PS gates Protect on `queue.willAct()`: it fails outright (and resets the chain)
        // when nothing acts after it this turn — foe switched, already moved, or no foe move.
        if !foe_moves_later {
            let mut fb = b;
            if fb.state.side(side).stall_counter != 0 {
                push(&mut fb, Instruction::SetStallCounter { side, previous: n, new: 0 });
            }
            return vec![fb];
        }
        // PS `stall` volatile: the FIRST protect use has no `stall` volatile yet, so its
        // `onStallMove` never runs — guaranteed success, no draw. Each subsequent consecutive use
        // rolls `randomChance(1, counter)` with `counter = 3^n` (capped at 729 = 3^6). The engine's
        // `stall_counter` is exactly that `n`, so `n == 0` makes no draw and `n >= 1` rolls
        // `randomChance(1, 3^n)`.
        let denom = 3i32.pow(n.min(6) as u32);
        let success_p = 1.0 / denom as f32;
        let mut out = Vec::new();
        // Success branch.
        let mut sb = scaled(&b, success_p);
        if n >= 1 {
            draw(&mut sb, "randomChance", &[1, denom], 1, "stall");
        }
        if !sb.state.side(side).volatiles.contains(VolatileStatus::Protect) {
            push(&mut sb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Protect });
        }
        push(&mut sb, Instruction::SetStallCounter { side, previous: n, new: n.saturating_add(1) });
        // The `stall` volatile is (re-)armed to duration 2 by a successful Protect (PS onStart/
        // onRestart); it survives one turn past this one for the Residual handler list.
        let prev_st = sb.state.side(side).stall_turns;
        if prev_st != 2 {
            push(&mut sb, Instruction::SetStallTurns { side, previous: prev_st, new: 2 });
        }
        out.push(sb);
        // Failure branch (the move fails, breaking the chain) — only when failure is possible.
        if success_p < 1.0 {
            let mut fb = scaled(&b, 1.0 - success_p);
            if n >= 1 {
                draw(&mut fb, "randomChance", &[1, denom], 0, "stall");
            }
            if n != 0 {
                push(&mut fb, Instruction::SetStallCounter { side, previous: n, new: 0 });
            }
            out.push(fb);
        }
        return out;
    }

    // Mean Look / Block / Spider Web: trap the foe (PS `onHit` adds the `trapped` volatile). Ghost
    // types are immune to the `trapped` status, so the volatile is not added to them (they can
    // still switch); Shed Shell does NOT stop the volatile from being applied, only the switch-lock.
    if matches!(md.id.to_id(), "meanlook" | "block" | "spiderweb") {
        let mut b = b;
        let t = b.state.side(foe).active();
        if t.is_alive()
            && !t.types.contains(&Type::Ghost)
            && !b.state.side(foe).volatiles.contains(VolatileStatus::Trapped)
        {
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Trapped });
        }
        return vec![b];
    }

    // Ingrain: root the user — cannot switch (Ghost exempt at legality time), heals 1/16 at each
    // end of turn (residual order 7, before Leech Seed). Fails if already rooted.
    if md.id.to_id() == "ingrain" {
        let mut b = b;
        if !b.state.side(side).volatiles.contains(VolatileStatus::Ingrain) {
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Ingrain });
        }
        return vec![b];
    }

    // No Retreat: +1 to every attacking/defensive stat and Speed, self-trap. PS `onTry`: fails
    // outright when the user already has `noretreat` (no boosts); a user already held by the
    // Mean-Look-family `trapped` volatile still gets the boosts but not a second trap volatile.
    if md.id.to_id() == "noretreat" {
        let mut b = b;
        if b.state.side(side).volatiles.contains(VolatileStatus::NoRetreat) {
            return vec![b];
        }
        for stat in [
            BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
            BoostIndex::SpecialDefense, BoostIndex::Speed,
        ] {
            apply_self_boost(&mut b, side, stat, 1);
        }
        apply_white_herb(&mut b, side);
        if !b.state.side(side).volatiles.contains(VolatileStatus::Trapped) {
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::NoRetreat });
        }
        return vec![b];
    }

    // Octolock: trap the foe and grind Def/SpD down by 1 each end of turn while the user stays
    // in. PS `onTryImmunity` runs the `trapped` type immunity, so it fails entirely against
    // Ghost-types; fails if the target already has it.
    if md.id.to_id() == "octolock" {
        let mut b = b;
        let (alive, ghost) = {
            let t = b.state.side(foe).active();
            (t.is_alive(), t.types.contains(&Type::Ghost))
        };
        // Octolock is a foe-targeting numeric-accuracy (100) status move: PS `hitStepAccuracy`
        // rolls `randomChance(100, 100)` — but only after `hitStepTryImmunity` passes, so a
        // Ghost target (immune to `trapped`) fails first and never rolls. Special-cased above the
        // general status-accuracy branch, so emit the draw here (draw-and-discard, 100% hits).
        // Also skipped when accuracy is forced `true` (No Guard target / Glaive Rush).
        if alive && !ghost && !accuracy_forced_true(&b, side, md) {
            draw(&mut b, "randomChance", &[100, 100], 1, "accuracy");
        }
        if alive
            && !ghost
            && !b.state.side(foe).volatiles.contains(VolatileStatus::Octolock)
        {
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Octolock });
        }
        return vec![b];
    }

    // Haze: reset every stat stage on both actives to 0 (one grouped ClearBoosts per side, so the
    // display renders PS's `|-clearallboost|`). State-equivalent to the per-stat Boost run.
    if md.id.to_id() == "haze" {
        let mut b = b;
        for s in [SideId::One, SideId::Two] {
            let prev = b.state.side(s).boosts;
            if prev.iter().any(|&x| x != 0) {
                push(&mut b, Instruction::ClearBoosts { side: s, previous: prev });
            }
        }
        return vec![b];
    }

    // Take Heart is callback-only in PS data: cure the user's status and raise SpA/SpD by 1.
    if md.id.to_id() == "takeheart" {
        let mut b = b;
        let (status, counter, slot) = {
            let p = b.state.side(side).active();
            (p.status, p.status_counter, b.state.side(side).active_index)
        };
        if status != Status::None {
            push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
            if counter != 0 {
                push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: 0 });
            }
        }
        apply_self_boost(&mut b, side, BoostIndex::SpecialAttack, 1);
        apply_self_boost(&mut b, side, BoostIndex::SpecialDefense, 1);
        return vec![b];
    }

    // Heal Bell: cure every party member's status, benched included (PS `onHit` walks
    // `side.pokemon`). Soundproof / Good as Gold ALLIES are immune — but not the user
    // itself (PS skips the immunity checks when `ally === source`). Fainted mons keep
    // their serialized status untouched (PS `cureStatus` bails on 0 HP).
    if md.id.to_id() == "healbell" {
        let mut b = b;
        let active_idx = b.state.side(side).active_index;
        for slot in 0..6u8 {
            let (species, alive, status, counter, ability) = {
                let p = &b.state.side(side).pokemon[slot as usize];
                (p.species, p.is_alive(), p.status, p.status_counter, p.ability)
            };
            if species == crate::ids::Species::None || !alive || status == Status::None {
                continue;
            }
            if slot != active_idx
                && matches!(ability, crate::ids::Ability::Soundproof | crate::ids::Ability::GoodAsGold)
            {
                continue;
            }
            push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
            if counter != 0 {
                push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: 0 });
            }
            if slot == active_idx
                && status == Status::Sleep
                && b.state.side(side).volatiles.contains(VolatileStatus::Nightmare)
            {
                push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Nightmare });
            }
        }
        return vec![b];
    }

    // Curse: Ghost users pay 1/2 HP and curse the foe; others get +Atk/+Def/-Spe.
    if md.id.to_id() == "curse" {
        let mut b = b;
        let is_ghost = b.state.side(side).active().types.contains(&Type::Ghost);
        if is_ghost {
            let p = b.state.side(side).active();
            let cost = (p.max_hp / 2).min(p.hp);
            let slot = b.state.side(side).active_index;
            if cost > 0 {
                push(&mut b, Instruction::Damage { side, slot, amount: cost });
            }
            if b.state.side(foe).active().is_alive() && !b.state.side(foe).volatiles.contains(VolatileStatus::Curse) {
                push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Curse });
            }
        } else {
            // Non-Ghost Curse: PS `onTryHit` rewrites the move to `move.self = {boosts:
            // {spe:-1, atk:1, def:1}}`, so `selfDrops` rolls one `random(100)` draw-and-discard
            // (no `self.chance` → always applies) before the boosts land. Curse's accuracy is
            // `true`, so this self-drop roll is the move's only draw.
            draw(&mut b, "random", &[100], 0, "self-drop");
            for (stat, delta) in [(BoostIndex::Attack, 1), (BoostIndex::Defense, 1), (BoostIndex::Speed, -1)] {
                let cur = b.state.side(side).boost(stat);
                let eff = (cur + delta).clamp(-6, 6) - cur;
                if eff != 0 {
                    push(&mut b, Instruction::Boost { side, stat, amount: eff });
                }
            }
        }
        return vec![b];
    }

    // Substitute: pay 1/4 max HP to put up a substitute with that much HP.
    if md.id.to_id() == "substitute" {
        let mut b = b;
        let p = b.state.side(side).active();
        let cost = p.max_hp / 4;
        if p.hp > cost && !b.state.side(side).volatiles.contains(VolatileStatus::Substitute) {
            let slot = b.state.side(side).active_index;
            push(&mut b, Instruction::Damage { side, slot, amount: cost });
            push(&mut b, Instruction::ChangeSubstituteHp { side, amount: cost });
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Substitute });
        }
        return vec![b];
    }

    // Pain Split: average the two actives' current HP.
    if md.id.to_id() == "painsplit" {
        let mut b = b;
        if b.state.side(foe).active().is_alive() {
            let (a_hp, a_max) = { let a = b.state.side(side).active(); (a.hp, a.max_hp) };
            let (f_hp, f_max) = { let f = b.state.side(foe).active(); (f.hp, f.max_hp) };
            let avg = (a_hp + f_hp) / 2;
            set_hp(&mut b, side, avg.min(a_max));
            set_hp(&mut b, foe, avg.min(f_max));
        }
        return vec![b];
    }

    // Rest: cures status, falls asleep, and heals to full. Implemented as an onHit callback
    // in PS (no static fields). Fails at full HP. The sleep counter is excluded from the
    // comparison and re-projected each turn, so we only set the status + heal.
    // Defog: -1 evasion to the target, clears hazards on BOTH sides, the target's screens,
    // and terrain (gen 8+).
    if md.id.to_id() == "defog" {
        let mut b = b;
        let foe2 = side.other();
        // PS `defog` `onHit` (data/moves.ts:3458): `if (!target.volatiles['substitute'] ||
        // move.infiltrates) success = !!this.boost({evasion: -1})`. Defog's `bypasssub` flag lets
        // the move REACH `onHit` through a Substitute, but the evasion drop itself is still blocked
        // by one — only an Infiltrator user gets through. The hazard/screen/terrain clears below
        // are outside that guard and happen either way.
        let sub_blocks = b.state.side(foe2).volatiles.contains(VolatileStatus::Substitute)
            && b.state.side(side).active().ability != crate::ids::Ability::Infiltrator;
        if b.state.side(foe2).active().is_alive() && !sub_blocks {
            let mb_move = move_breaks_abilities(&b, side, &md);
            if apply_boost_clamped_ex(&mut b, foe2, BoostIndex::Evasion, -1, mb_move) < 0 {
                react_to_stat_drop(&mut b, foe2);
            }
        }
        for sd in [side, foe2] {
            clear_hazards(&mut b, sd);
        }
        let fc = b.state.side(foe2).side_conditions;
        for (sc, cur) in [
            (SideConditionId::Reflect, fc.reflect),
            (SideConditionId::LightScreen, fc.light_screen),
            (SideConditionId::AuroraVeil, fc.aurora_veil),
        ] {
            if cur > 0 {
                push(&mut b, Instruction::SetSideCondition { side: foe2, condition: sc, previous: cur, new: 0 });
            }
        }
        let (prev_t, prev_tt) = (b.state.terrain, b.state.terrain_turns);
        if prev_t != crate::ids::Terrain::None {
            emit_field_change_shuffle(&mut b); // clearTerrain -> eachEvent('TerrainChange')
            push(&mut b, Instruction::ChangeTerrain {
                previous: prev_t,
                previous_turns: prev_tt,
                new: crate::ids::Terrain::None,
                new_turns: 0,
            });
            refresh_proto_quark(&mut b); // PS Quark Drive `onTerrainChange`
        }
        return vec![b];
    }
    // Tidy Up: clears hazards and Substitutes on BOTH sides, then +1 Atk / +1 Spe.
    if md.id.to_id() == "tidyup" {
        let mut b = b;
        for sd in [side, side.other()] {
            clear_hazards(&mut b, sd);
            if b.state.side(sd).volatiles.contains(VolatileStatus::Substitute) {
                push(&mut b, Instruction::RemoveVolatile { side: sd, volatile: VolatileStatus::Substitute });
                let sub = b.state.side(sd).substitute_hp;
                if sub > 0 {
                    push(&mut b, Instruction::ChangeSubstituteHp { side: sd, amount: sub });
                }
            }
        }
        apply_self_boost(&mut b, side, BoostIndex::Attack, 1);
        apply_self_boost(&mut b, side, BoostIndex::Speed, 1);
        return vec![b];
    }
    // Fillet Away: costs 1/2 max HP (fails if HP is half or less) for +2 Atk/SpA/Spe.
    if md.id.to_id() == "filletaway" {
        let mut b = b;
        let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
        let cost = maxhp / 2;
        if hp > cost {
            let slot = b.state.side(side).active_index;
            push(&mut b, Instruction::Damage { side, slot, amount: cost });
            apply_self_boost(&mut b, side, BoostIndex::Attack, 2);
            apply_self_boost(&mut b, side, BoostIndex::SpecialAttack, 2);
            apply_self_boost(&mut b, side, BoostIndex::Speed, 2);
        }
        return vec![b];
    }
    // Belly Drum: pay 1/2 max HP (via directDamage) to max Attack (+6). PS `onHit` fails
    // outright — no HP cost — when HP is already at/below 1/2 max, Attack is already +6, or the
    // user is Shedinja (maxhp == 1). The boost is `{atk: 12}`, clamped to the +6 cap.
    if md.id.to_id() == "bellydrum" {
        let mut b = b;
        let (hp, maxhp, atk) = {
            let p = b.state.side(side).active();
            (p.hp, p.max_hp, b.state.side(side).boost(BoostIndex::Attack))
        };
        if hp <= maxhp / 2 || atk >= 6 || maxhp == 1 {
            return vec![b]; // fails; PP already paid, no HP cost
        }
        let slot = b.state.side(side).active_index;
        push(&mut b, Instruction::Damage { side, slot, amount: maxhp / 2 });
        apply_self_boost(&mut b, side, BoostIndex::Attack, 12);
        return vec![b];
    }
    // Clangorous Soul: pay floor(maxHP·33/100) HP for +1 to all five stats. PS `onTry` fails
    // (no cost) when HP is at/below that cost or the user is Shedinja; `onTryHit` fails when the
    // boost would change nothing (all five already +6), leaving HP untouched. The boost is
    // applied BEFORE the HP cost (directDamage in `onHit`).
    if md.id.to_id() == "clangoroussoul" {
        let mut b = b;
        let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
        let cost = (maxhp as i32 * 33 / 100) as i16;
        if hp <= cost || maxhp == 1 {
            return vec![b]; // onTry fail; PP already paid, no cost
        }
        let boostable = [
            BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
            BoostIndex::SpecialDefense, BoostIndex::Speed,
        ]
        .iter()
        .any(|&s| b.state.side(side).boost(s) < 6);
        if !boostable {
            return vec![b]; // onTryHit fail: all five already maxed, no HP paid
        }
        for stat in [
            BoostIndex::Attack, BoostIndex::Defense, BoostIndex::SpecialAttack,
            BoostIndex::SpecialDefense, BoostIndex::Speed,
        ] {
            apply_self_boost(&mut b, side, stat, 1);
        }
        let slot = b.state.side(side).active_index;
        push(&mut b, Instruction::Damage { side, slot, amount: cost });
        // Clangorous Soul is a SOUND move, and Throat Spray is `onAfterMoveSecondarySelf` —
        // fired by `useMoveInner` for any move that succeeded. This branch returns before the
        // shared status tail, so it needs its own call (the two failure returns above do NOT:
        // `useMoveInner` reaches `MoveFail` instead). rb1089 t2.
        apply_throat_spray(&mut b, side, md);
        return vec![b];
    }
    // Court Change: swap BOTH sides' entry-hazard / screen / Tailwind side conditions wholesale
    // (PS's full `sideConditions` list, intersected with what the engine models).
    if md.id.to_id() == "courtchange" {
        let mut b = b;
        let a = b.state.side(side).side_conditions;
        let c = b.state.side(foe).side_conditions;
        use SideConditionId::*;
        let pairs = [
            (StealthRock, a.stealth_rock as u8, c.stealth_rock as u8),
            (Spikes, a.spikes, c.spikes),
            (ToxicSpikes, a.toxic_spikes, c.toxic_spikes),
            (StickyWeb, a.sticky_web as u8, c.sticky_web as u8),
            (Reflect, a.reflect, c.reflect),
            (LightScreen, a.light_screen, c.light_screen),
            (AuroraVeil, a.aurora_veil, c.aurora_veil),
            (Tailwind, a.tailwind, c.tailwind),
        ];
        for (cond, av, cv) in pairs {
            if av != cv {
                push(&mut b, Instruction::SetSideCondition { side, condition: cond, previous: av, new: cv });
                push(&mut b, Instruction::SetSideCondition { side: foe, condition: cond, previous: cv, new: av });
            }
        }
        return vec![b];
    }
    // Healing Wish / Lunar Dance: the user faints (self_destruct) leaving a healing wish
    // for the next damaged replacement. Fails if there is nothing to switch to.
    if matches!(md.id.to_id(), "healingwish" | "lunardance") {
        let mut b = b;
        let can_switch = b.state.side(side).pokemon.iter().enumerate().any(|(i, p)| {
            i as u8 != b.state.side(side).active_index && p.species != crate::ids::Species::None && p.is_alive()
        });
        if can_switch && !b.state.side(side).healing_wish {
            push(&mut b, Instruction::SetHealingWish { side, previous: false, new: true });
        }
        if can_switch {
            apply_post_status_self_destruct(&mut b, side, md);
        }
        return vec![b];
    }
    // Trick Room: toggles; setting gives 5 turns (ticked at residual), reusing cancels.
    if md.id.to_id() == "trickroom" {
        let mut b = b;
        let (prev_tr, prev_turns) = (b.state.trick_room, b.state.trick_room_turns);
        if prev_tr {
            push(&mut b, Instruction::ToggleTrickRoom { previous_turns: prev_turns, new_turns: 0 });
        } else {
            push(&mut b, Instruction::ToggleTrickRoom { previous_turns: prev_turns, new_turns: 5 });
        }
        return vec![b];
    }
    // Transform copies the foe's battle identity.
    if md.id.to_id() == "transform" {
        let mut b = b;
        apply_transform(&mut b, side);
        return vec![b];
    }
    // Trick / Switcheroo: swap held items (blocked by Sticky Hold).
    if matches!(md.id.to_id(), "trick" | "switcheroo") {
        let mut b = b;
        let foe2 = side.other();
        let (mine, theirs) = (b.state.side(side).active().item, b.state.side(foe2).active().item);
        let sticky = b.state.side(foe2).active().ability == crate::ids::Ability::StickyHold;
        // Trick / Switcheroo are foe-targeting numeric-accuracy (100) status moves: PS
        // `hitStepAccuracy` rolls `randomChance(accuracy,100)`. They reach it only when
        // `onTryImmunity` let them through — Sticky Hold refuses at moveStep 3, which the hoisted
        // `status_try_immunity_fails` at the top of this function now handles for every branch.
        // Special-cased above the general status-accuracy branch, so emit the draw here
        // (draw-and-discard, 100% hits). The arg is
        // post-`ModifyAccuracy`/stage: a +1-accuracy Trick user rolls `randomChance(133,100)`
        // (r10 t17). A fainted foe (no target) fails earlier and never rolls.
        if annotating() && b.state.side(foe2).active().is_alive() && !accuracy_forced_true(&b, side, md) {
            let acc = accuracy_arg(&b, side, md);
            draw(&mut b, "randomChance", &[acc, 100], 1, "accuracy");
        }
        // PS `trick.onHit` (`data/moves.ts:19889-19904`) fails the WHOLE swap the moment either
        // side of the transfer is refused:
        //   `const yourItem = target.takeItem(source); const myItem = source.takeItem();`
        //   `if (yourItem === false || myItem === false || (!yourItem && !myItem)) { restore; return false }`
        // and then a SECOND pair of `singleEvent('TakeItem')` checks with the holders CROSSED, so
        // an item that cannot be HELD by the other end fails too. Both are the item's `onTakeItem`,
        // which the Arceus plates / Silvally memories / Genesect drives / Origin items / Rusted
        // Sword & Shield / Ogerpon masks / Blue & Red Orb refuse — most of them symmetrically
        // (`(source && source.baseSpecies.num === 493) || pokemon.baseSpecies.num === 493`), which
        // is exactly what `item_removable_from`'s `source` argument models.
        //
        // rb1099 d57: a Choice Scarf Chandelure Tricks an Arceus-Dark holding a Dread Plate. PS
        // fails outright — both keep their items and Chandelure keeps its `choicelock`. The engine
        // swapped, and moved the Choice lock to the Arceus with the Scarf.
        let my_sp = b.state.side(side).active().species;
        let their_sp = b.state.side(foe2).active().species;
        let tradeable = item_removable_from(their_sp, theirs, Some(my_sp))
            && item_removable_from(my_sp, mine, Some(their_sp));
        if b.state.side(foe2).active().is_alive() && !sticky && tradeable
            && (mine != Item::None || theirs != Item::None)
        {
            let my_slot = b.state.side(side).active_index;
            let their_slot = b.state.side(foe2).active_index;
            push(&mut b, Instruction::ChangeItem { side, slot: my_slot, previous: mine, new: theirs });
            push(&mut b, Instruction::ChangeItem { side: foe2, slot: their_slot, previous: theirs, new: mine });
            for sd in [side, foe2] {
                if b.state.side(sd).volatiles.contains(VolatileStatus::ChoiceLock) {
                    push(&mut b, Instruction::RemoveVolatile { side: sd, volatile: VolatileStatus::ChoiceLock });
                }
            }
            if theirs == Item::None {
                on_item_lost(&mut b, side);
            }
            if mine == Item::None {
                on_item_lost(&mut b, foe2);
            }
        }
        return vec![b];
    }
    // Wish: heals half the caster's max HP into whatever occupies the slot, at the end of
    // NEXT turn. Fails while one is already pending.
    if md.id.to_id() == "wish" {
        let mut b = b;
        if b.state.side(side).wish.0 == 0 {
            let amt = b.state.side(side).active().max_hp / 2;
            let prev = b.state.side(side).wish;
            push(&mut b, Instruction::SetWish { side, previous: prev, new: (2, amt) });
        }
        return vec![b];
    }
    // (Future Sight / Doom Desire are category Special and are intercepted in the damaging-move
    // path before reaching here — they schedule a delayed strike rather than acting this turn.)
    if md.id.to_id() == "rest" {
        let mut b = b;
        let (hp, maxhp, status, item) = {
            let p = b.state.side(side).active();
            (p.hp, p.max_hp, p.status, p.item)
        };
        let blocked = status == Status::Sleep
            || matches!(b.state.side(side).active().ability, crate::ids::Ability::Insomnia | crate::ids::Ability::VitalSpirit | crate::ids::Ability::Comatose | crate::ids::Ability::PurifyingSalt)
            || status_blocked_by_field(&b.state, side, Status::Sleep);
        if hp < maxhp && !blocked {
            let slot = b.state.side(side).active_index;
            // PS Rest `onHit` calls `setStatus('slp')` FIRST — the `slp` condition's `onStart`
            // rolls `this.random(2, 5)` (data/conditions.ts) — and only THEN overrides
            // `statusState.time = 3`. So the duration draw is consumed and discarded on every
            // successful Rest (including the Chesto-cured case, since Chesto's `onUpdate` cures
            // the sleep AFTER it is set). Emit it as a draw-and-discard at the apply moment.
            if annotating() {
                draw(&mut b, "random", &[2, 5], 3, "slp");
            }
            // Chesto Berry immediately cures the Rest sleep (and is eaten).
            if item == Item::ChestoBerry {
                if status != Status::None {
                    push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
                }
                clear_status_counter(&mut b, side, slot);
                push(&mut b, Instruction::ChangeItem { side, slot, previous: Item::ChestoBerry, new: Item::None });
                on_berry_eaten_id(&mut b, side, Item::ChestoBerry);
            } else {
                push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::Sleep });
                // Rest's sleep is a fixed 2-turn nap (PS statusState.time = 3).
                let prev_ctr = b.state.side(side).active().status_counter;
                push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 3 });
            }
            push(&mut b, Instruction::Heal { side, slot, amount: maxhp - hp });
        }
        return vec![b];
    }

    // Strength Sap: heal the user by the target's (boost-adjusted) Attack stat, then lower
    // the target's Attack by 1 (an opponent-induced drop, so Defiant/Competitive can react).
    if md.id.to_id() == "strengthsap" {
        let mut b = b;
        if b.state.side(foe).active().is_alive() {
            // Strength Sap is a foe-targeting numeric-accuracy status move — PS `hitStepAccuracy`
            // rolls `randomChance(accuracy, 100)` before the drain/drop (accuracy post-ModifyAccuracy
            // / stages via `accuracy_arg`). (Special-cased above the general status accuracy branch.)
            // Skipped when accuracy is forced `true` — No Guard on the target (r11: Strength Sap
            // into a No Guard Golurk), a Glaive Rush target, or weather-perfect accuracy.
            if !accuracy_forced_true(&b, side, md) {
                let acc = accuracy_arg(&b, side, md);
                draw(&mut b, "randomChance", &[acc, 100], 1, "accuracy");
            }
            let atk_val = {
                let t = b.state.side(foe).active();
                let boost = b.state.side(foe).boost(BoostIndex::Attack);
                (t.stat(crate::ids::StatIndex::Attack) as f32 * boost_multiplier(boost)) as i16
            };
            // Liquid Ooze is `onSourceTryHeal`, and its `canOoze` list is exactly
            // `['drain', 'leechseed', 'strengthsap']` (`data/abilities.ts:2360`) — enumerated from
            // the pin, not guessed. The engine had it on `drain` only. `battle.heal` runs
            // `runEvent('TryHeal')` at `sim/battle.ts:2284`, BEFORE the `target.hp >= target.maxhp`
            // bail at :2288 ("for things like Liquid Ooze, the Heal event still happens when
            // nothing is healed"), so the ooze damage is the FULL sapped amount and lands even on
            // a user at full HP — it must not be capped by missing HP the way the heal is.
            //
            // rb1745 d26 t20: Bellossom's Strength Sap into a Liquid Ooze Tentacruel. PS takes
            // Bellossom from 202 to 0; the engine healed it 166.
            let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
            let slot = b.state.side(side).active_index;
            if b.state.side(foe).active().ability == crate::ids::Ability::LiquidOoze {
                let dmg = atk_val.min(hp);
                if dmg > 0 {
                    push(&mut b, Instruction::Damage { side, slot, amount: dmg });
                }
            } else {
                let amount = atk_val.min(maxhp - hp);
                if amount > 0 {
                    push(&mut b, Instruction::Heal { side, slot, amount });
                }
            }
            let mb_move = move_breaks_abilities(&b, side, &md);
            if apply_boost_clamped_ex(&mut b, foe, BoostIndex::Attack, -1, mb_move) < 0 {
                react_to_stat_drop(&mut b, foe);
            }
        }
        return vec![b];
    }

    // Weather-dependent recovery (Moonlight / Synthesis / Morning Sun: 1/2 normally, 0.667 in
    // sun, 1/4 in any other weather; Shore Up: 1/2, 0.667 in sand). PS implements these as
    // `onHit` callbacks, so they carry no static `heal` field for the codegen to read. The
    // sun/sand factor is PS's literal 0.667 → modifier tr(0.667·4096)=2732, not exactly 2/3.
    let weather_heal: Option<(i64, i64)> = match md.id.to_id() {
        "moonlight" | "synthesis" | "morningsun" => Some(match b.state.weather {
            Weather::None => (1, 2),
            Weather::Sun | Weather::HarshSun => (2732, 4096),
            _ => (1, 4),
        }),
        "shoreup" => Some(match b.state.weather {
            Weather::Sand => (2732, 4096),
            _ => (1, 2),
        }),
        _ => None,
    };
    if let Some((num, den)) = weather_heal {
        let mut b = b;
        let p = b.state.side(side).active();
        let amount = (crate::damage::modify(p.max_hp as i64, num, den) as i16).min(p.max_hp - p.hp);
        if amount > 0 {
            let slot = b.state.side(side).active_index;
            push(&mut b, Instruction::Heal { side, slot, amount });
        }
        return vec![b];
    }

    // PS `hitStepTypeImmunity` runs BEFORE `hitStepAccuracy`, but for STATUS moves it is bypassed
    // (`move.ignoreImmunity` defaults to `category === 'Status'`) — EXCEPT the lone status move that
    // explicitly sets `ignoreImmunity: false`: **Thunder Wave**. So a Ground-type target (0× to
    // Electric) fails Thunder Wave outright — no accuracy roll, no paralysis — while every other
    // status move ignores type-chart immunity (Toxic vs Steel still rolls then fails at setStatus;
    // Roar/Growl affect Ghost; hazards target a side). Electric-type targets take 0.5× (not 0), so
    // Thunder Wave still rolls accuracy against them and the paralysis fails later at setStatus.
    // This is a real (non-annotation) mechanics gate: PS makes no draw and applies nothing.
    if md.id.to_id() == "thunderwave" {
        let foe_p = b.state.side(foe).active();
        if foe_p.is_alive() && crate::damage::type_multiplier(md.typ, foe_p.types) == 0.0 {
            return vec![b];
        }
    }
    let hit_prob = accuracy_of(&b, side, md);
    let miss_prob = 1.0 - hit_prob;
    // PS `hitStepAccuracy`: a foe-targeting status move with numeric accuracy rolls
    // `randomChance(accuracy, 100)` once (before its effect resolves). The roll is bypassed —
    // accuracy forced to `true` — for `accuracy: true` moves (md.accuracy == 0), self-targeting
    // status moves (`target === 'self'`: Swords Dance, Calm Mind, Recover, weather, hazards…),
    // and Toxic used by a Poison-type (gen >= 8). Emit on `b` so both the hit and miss branches
    // inherit it. The arg is the post-`ModifyAccuracy` numerator (`accuracy_arg`: Wide Lens /
    // Compound Eyes ×4096 chain + accuracy/evasion stage boosts) — the same value `hit_prob`
    // (`accuracy_of`) already derives, so arg and result stay consistent (e.g. Wide Lens Encore/
    // Taunt roll `randomChance(110, 100)`).
    if md.accuracy != 0
        && md.target != crate::data::MoveTarget::User
        && !accuracy_forced_true(&b, side, md)
    {
        let acc = accuracy_arg(&b, side, md);
        draw(&mut b, "randomChance", &[acc, 100], (hit_prob > 0.0) as i64, "accuracy");
    }
    let mut hit = scaled(&b, hit_prob);

    // Whether the heal user was at full HP *before* healing — a self-heal move (Roost/Recover)
    // fails outright at full HP, which matters for Roost's Flying-type removal below.
    let heal_user_was_full = hit.state.side(side).active().hp >= hit.state.side(side).active().max_hp;
    if md.heal.0 > 0 {
        let p = hit.state.side(side).active();
        // PS heals `Math.round(maxhp · num / den)` (round half up), not floor or the 4096
        // `modify` (which differs from round on some odd max HP — Roost on Dragonite, etc.).
        let amount = (round_div(p.max_hp as i32 * md.heal.0 as i32, md.heal.1 as i32) as i16)
            .min(p.max_hp - p.hp);
        if amount > 0 {
            let slot = hit.state.side(side).active_index;
            push(&mut hit, Instruction::Heal { side, slot, amount });
        }
    }
    // Roost: the user loses its Flying type until the end of the turn, so the foe's move this
    // turn hits the grounded typing; the `Roosted` volatile marks it for restoration at end
    // of turn (PS's `roost` volatile has duration 1 — cosim caught the engine dropping
    // Flying permanently). Two PS caveats: Roost fails entirely at full HP (the heal fails →
    // the volatile is never added → Flying is kept), and a Terastallized user keeps its
    // Flying type (the volatile's onStart bails).
    if md.id.to_id() == "roost" {
        let p = hit.state.side(side).active();
        if p.types.contains(&Type::Flying) && !heal_user_was_full && !p.terastallized {
            let slot = hit.state.side(side).active_index;
            let prev = p.types;
            let new = [
                if prev[0] == Type::Flying { Type::None } else { prev[0] },
                if prev[1] == Type::Flying { Type::None } else { prev[1] },
            ];
            // EFFECTIVE typing only: PS's `roost` volatile filters Flying out of `getTypes()`
            // through `onType` and never touches `pokemon.types`, so no `ChangeLiveTypes`.
            push(&mut hit, Instruction::ChangeTypes { side, slot, previous: prev, new });
            push(&mut hit, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Roosted });
        }
    }
    // Self-boosts (Swords Dance, Dragon Dance, ...).
    for (i, &delta) in md.self_boosts.iter().enumerate() {
        if delta != 0 {
            apply_self_boost(&mut hit, side, BOOST_ORDER[i], delta);
        }
    }
    // White Herb restores the user's own drops (Shell Smash's −Def/−SpD).
    apply_white_herb(&mut hit, side);
    // Good as Gold blocks status moves that target the holder (boosts/status against it).
    // It is a `breakable` ability: the Mold Breaker family pierces it, and Mycelium Might
    // makes the user's status moves ignore the target's ability entirely (PS onModifyMove
    // move.ignoreAbility for status moves).
    let status_breaker = matches!(
        hit.state.side(side).active().ability,
        crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt
            | crate::ids::Ability::Turboblaze | crate::ids::Ability::MyceliumMight
    );
    let foe_immune =
        hit.state.side(foe).active().ability == crate::ids::Ability::GoodAsGold && !status_breaker;
    // Boosts a status move applies to the foe (Growl, ...), respecting Clear Body.
    if hit.state.side(foe).active().is_alive() && !foe_immune {
        for (i, &delta) in md.target_boosts.iter().enumerate() {
            if delta != 0 && apply_boost_clamped_ex(&mut hit, foe, BOOST_ORDER[i], delta, status_breaker) < 0 {
                // PS fires `AfterEachBoost` INSIDE `boost()`'s per-stat loop (sim/battle.ts:2073),
                // once for every stat whose `boostBy` was non-zero — so a TWO-stat drop wakes
                // Defiant / Competitive TWICE. Parting Shot (atk -1, spa -1) into Competitive is
                // 0 -> -1, then spa 0 -> -1 +2 +2 = +3 (rb1211 t44, rb1371 t7, rb1152 t12).
                react_to_stat_drop(&mut hit, foe);
                apply_white_herb(&mut hit, foe);
            }
        }
    }
    if let Some(sc) = md.side_condition {
        match sc {
            // Screens / Tailwind are set on the USER's side with a turn duration (the old code
            // routed everything to the foe's side with value 1 — caught by cosim).
            SideConditionId::Reflect | SideConditionId::LightScreen | SideConditionId::AuroraVeil
            | SideConditionId::Tailwind => {
                apply_own_side_condition(&mut hit, side, sc);
            }
            _ => apply_hazard(&mut hit, foe, sc),
        }
    }
    if md.weather != Weather::None && hit.state.weather != md.weather {
        let turns = weather_set_turns(hit.state.side(side).active().item, md.weather);
        set_weather(&mut hit, md.weather, turns);
    }
    let mut applied_sleep = false;
    let pre_clause = md.status != Status::None
        && !foe_immune
        && status_applies_src(hit.state.side(foe).active(), md.status,
            hit.state.side(side).active().ability == crate::ids::Ability::Corrosion,
            status_breaker)
        && !status_blocked_by_field(&hit.state, foe, md.status);
    let clause = pre_clause && md.status == Status::Sleep && sleep_clause_blocks(&hit.state, foe);
    if clause {
        push(&mut hit, Instruction::SleepClauseBlocked { side: foe });
    }
    if pre_clause && !clause {
        let slot = hit.state.side(foe).active_index;
        push(&mut hit, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.status });
        applied_sleep = md.status == Status::Sleep;
        if applied_sleep {
            mark_slept_by_foe(&mut hit, foe);
        }
        apply_synchronize(&mut hit, foe, md.status);
        consume_lum_if_statused(&mut hit, foe);
        applied_sleep = sleep_survived_or_discard_duration(&mut hit, foe, applied_sleep);
    }

    // Sleep durations are stochastic (1-3 turns), and target volatiles (Taunt / Encore /
    // Yawn / Confuse Ray / Perish Song / ...) carry counters and may themselves branch.
    let mut hits = vec![hit];
    if applied_sleep {
        hits = hits.into_iter().flat_map(|x| branch_sleep_counter(x, foe)).collect();
    }
    if md.target_volatile.is_some() && !foe_immune {
        hits = hits
            .into_iter()
            .flat_map(|x| apply_status_target_volatile(x, side, md, foe_moves_later))
            .collect();
    }
    if md.force_switch {
        // Roar / Whirlwind: phaze the foe into a random bench mon.
        hits = hits.into_iter().flat_map(|x| apply_drag(x, foe)).collect();
    }
    // Self-sacrificing status moves (Memento, Final Gambit-likes) faint the user on hit.
    if md.self_destruct {
        for x in hits.iter_mut() {
            apply_post_status_self_destruct(x, side, md);
        }
    }

    // Throat Spray is `onAfterMoveSecondarySelf`, which `useMoveInner` fires after ANY move that
    // succeeded — a self-targeting STATUS sound move counts. The engine only ran it on the
    // damaging-hit path. rb1089 t2: Kommo-o's Clangorous Soul leaves PS at +2 SpA with the spray
    // consumed, the engine at +1 still holding it.
    for x in hits.iter_mut() {
        apply_throat_spray(x, side, md);
    }

    // NOTE: a connecting status move runs `moveHit`, after which PS fires the per-hit
    // `eachEvent('Update')` (battle-actions.ts:970). It is a documented DEFERRAL — no clean emit
    // point exists under the current model. PS skips `moveHit`/970 when the move fails at `tryHit`
    // (immune foe, Recover at full HP, boost at cap), which the post-effect "hit" branch can't
    // distinguish from a real hit; and the 970 stream POSITION depends on the move sub-type — a
    // phaze/drag (Roar/Whirlwind `sample`) fires AFTER 970 while onHit status/boost/volatile draws
    // fire BEFORE it, so a single emit site mis-orders one class or the other. Both a blanket emit
    // and an instruction-count-gated ("moveHit changed state") emit measured net-negative (the
    // mis-ordered phaze/self-destruct cases outweigh the gains). Needs move-subtype-aware placement
    // plus a moveHit-ran signal.
    if miss_prob > 0.0 {
        let mut mb = scaled(&b, miss_prob);
        // The accuracy roll came up a miss on this branch: flip the inherited hit-result (1) to 0
        // so the seed-gate Replicate filter selects hit vs miss by the realized `randomChance`
        // value — mirroring the damaging-move miss branch. Without this both branches carried
        // result 1, leaving a real miss unselectable (the filter fell through and applied the
        // status, e.g. Thunder Wave paralysing on a recorded miss — r6/d1/c3c1s73).
        if mb.draws.last().is_some_and(|d| d.site == "accuracy") {
            if let Some(d) = mb.draws.last_mut() {
                d.result = 0;
            }
        }
        hits.push(mb);
        hits
    } else {
        hits
    }
}

/// Protect-family moves that block the opponent's move for the turn.
fn is_protect_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "protect" | "detect" | "spikyshield" | "kingsshield" | "banefulbunker"
            | "silktrap" | "burningbulwark" | "obstruct" | "maxguard"
    )
}

/// Two-turn charge moves that strike on the second turn (no semi-invulnerability).
fn is_charge_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "solarbeam" | "solarblade" | "skyattack" | "razorwind" | "skullbash"
            | "freezeshock" | "iceburn" | "meteorbeam" | "electroshot"
    )
}

/// Charge moves that raise a stat of the user on the charge turn (PS `onTryMove` `this.boost`):
/// Meteor Beam / Electro Shot both give +1 SpA. Applied on the charge turn and, when Power Herb
/// skips the charge, on the strike turn — never on the second turn of a normally-charged use.
fn charge_self_boost(id: crate::ids::MoveId) -> Option<(BoostIndex, i8)> {
    match id.to_id() {
        "meteorbeam" | "electroshot" => Some((BoostIndex::SpecialAttack, 1)),
        _ => None,
    }
}

/// Moves flagged `cantusetwice` (PS): they cannot be re-selected the turn after a use (PS
/// disables the slot when `lastMove?.id === moveSlot.id`). Gen9 has exactly these two.
pub fn is_cantusetwice_move(id: crate::ids::MoveId) -> bool {
    matches!(id.to_id(), "gigatonhammer" | "bloodmoon")
}

/// Whether `move_id` is a `cantusetwice` move currently locked out of selection for `side`'s
/// active because it was that mon's last executed move (PS's `DisableMove` for the flag).
pub fn cantusetwice_locked(state: &State, side: SideId, move_id: crate::ids::MoveId) -> bool {
    is_cantusetwice_move(move_id) && state.side(side).last_used_move == move_id
}

/// PS `Pokemon.getMoves(null)` ends `return hasValidMove ? moves : []` (`sim/pokemon.ts:1042`),
/// and `getMoveRequestData` turns an empty list into the single pseudo-move **Struggle**
/// (`:1104-1107`). So a mon whose every slot is DISABLED — not merely out of PP — is forced to
/// Struggle, with all of its PP intact.
///
/// The witness is rb1231 d15: a Tinkaton that used Gigaton Hammer (whose `cantusetwice` flag
/// disables it the following turn) is then hit by **Encore**, whose gen-9 `onDisableMove`
/// disables every slot EXCEPT the encored one — which is the already-disabled Gigaton Hammer.
/// All four slots disabled, so PS's request offers only Struggle. The engine instead honoured
/// the Encore override and re-used Gigaton Hammer, spending a PP PS never spent.
///
/// A HARD lock (rampage / charge / recharge) short-circuits `getMoves` before the `disabled`
/// scan and returns the locked move, so such a mon never Struggles — hence the early return.
pub fn no_usable_move(state: &State, side: SideId) -> bool {
    let s = state.side(side);
    if s.pending_move != crate::state::PendingMove::None {
        return false;
    }
    let p = s.active();
    let choice_locked = s.volatiles.contains(VolatileStatus::ChoiceLock);
    !p.moves.iter().any(|m| {
        m.id != crate::ids::MoveId::None
            && m.pp > 0
            && !m.disabled
            // Gigaton Hammer / Blood Moon, the turn after use.
            && !cantusetwice_locked(state, side, m.id)
            // Encore disables every OTHER slot; Disable disables its own.
            && (s.encore.1 == 0 || m.id == s.encore.0)
            && (s.disable.1 == 0 || m.id != s.disable.0)
            // Taunt disables every status move; a Choice lock every move but the locked one.
            && !(s.taunt_turns > 0 && move_data(m.id).category == MoveCategory::Status)
            && !(choice_locked && s.last_used_move != crate::ids::MoveId::None && m.id != s.last_used_move)
    })
}

/// Two-turn moves whose user is untargetable during the charge turn.
fn is_semi_invuln_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "fly" | "dig" | "dive" | "bounce" | "phantomforce" | "shadowforce"
    )
}

pub fn is_two_turn_move(id: crate::ids::MoveId) -> bool {
    is_charge_move(id) || is_semi_invuln_move(id)
}

/// Moves that force the user to recharge (forfeit) the turn after they connect.
fn is_recharge_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "hyperbeam" | "gigaimpact" | "blastburn" | "hydrocannon" | "frenzyplant"
            | "rockwrecker" | "roaroftime" | "prismaticlaser" | "eternabeam" | "meteorassault"
    )
}

/// Charge moves that skip the charge turn under the right weather (Solar Beam/Blade in sun).
fn charges_instantly(id: crate::ids::MoveId, weather: Weather) -> bool {
    matches!(id.to_id(), "solarbeam" | "solarblade") && matches!(weather, Weather::Sun | Weather::HarshSun)
}

/// Move the active Pokémon's HP to `target_hp` via a Heal or Damage instruction.
fn set_hp(b: &mut Branch, side: SideId, target_hp: i16) {
    let p = b.state.side(side).active();
    let slot = b.state.side(side).active_index;
    let delta = target_hp - p.hp;
    if delta > 0 {
        push(b, Instruction::Heal { side, slot, amount: delta });
    } else if delta < 0 {
        push(b, Instruction::Damage { side, slot, amount: -delta });
    }
}

/// Set the field weather (and its duration). PS's `Field.setWeather` / `clearWeather` end with
/// `eachEvent('WeatherChange')` — one tie-gated `shuffle[2,0,2]`, emitted on the pre-change board.
fn set_weather(b: &mut Branch, weather: Weather, turns: i8) {
    emit_field_change_shuffle(b);
    push(b, Instruction::ChangeWeather {
        previous: b.state.weather,
        previous_turns: b.state.weather_turns,
        new: weather,
        new_turns: turns,
    });
    refresh_proto_quark(b); // PS Protosynthesis `onWeatherChange`
    for s in [SideId::One, SideId::Two] {
        restore_ice_face(b, s); // PS Ice Face `onWeatherChange`
    }
}

/// Set or increment a hazard on `target`'s side (capped at its max layers).
fn apply_hazard(b: &mut Branch, target: SideId, sc: SideConditionId) {
    let conds = &b.state.side(target).side_conditions;
    let (cur, max) = match sc {
        SideConditionId::StealthRock => (conds.stealth_rock as u8, 1),
        SideConditionId::Spikes => (conds.spikes, 3),
        SideConditionId::ToxicSpikes => (conds.toxic_spikes, 2),
        SideConditionId::StickyWeb => (conds.sticky_web as u8, 1),
        _ => (1, 1),
    };
    if cur < max {
        push(b, Instruction::SetSideCondition { side: target, condition: sc, previous: cur, new: cur + 1 });
    }
}

/// Set an own-side duration condition (Reflect / Light Screen / Aurora Veil / Tailwind).
/// Fails (no-op) if already up; Aurora Veil additionally requires Snow. Light Clay extends
/// screens to 8 turns.
fn apply_own_side_condition(b: &mut Branch, side: SideId, sc: SideConditionId) {
    let conds = &b.state.side(side).side_conditions;
    let cur = match sc {
        SideConditionId::Reflect => conds.reflect,
        SideConditionId::LightScreen => conds.light_screen,
        SideConditionId::AuroraVeil => conds.aurora_veil,
        SideConditionId::Tailwind => conds.tailwind,
        _ => return,
    };
    if cur > 0 {
        return; // already active: the move fails
    }
    if sc == SideConditionId::AuroraVeil && b.state.weather != Weather::Snow {
        return; // Aurora Veil only works in snow
    }
    let clay = b.state.side(side).active().item == Item::LightClay;
    let turns = match sc {
        SideConditionId::Tailwind => 4,
        _ if clay => 8,
        _ => 5,
    };
    push(b, Instruction::SetSideCondition { side, condition: sc, previous: cur, new: turns });
    // Wind Rider: +1 Atk when Tailwind starts on the holder's side (PS onSideConditionStart;
    // in singles the only mon on the side is the setter itself).
    if sc == SideConditionId::Tailwind
        && b.state.side(side).active().ability == crate::ids::Ability::WindRider
        && b.state.side(side).active().is_alive()
    {
        raise_boost(b, side, BoostIndex::Attack, 1);
    }
}

/// Self-set weather duration: 8 turns when the setter holds the matching rock, else 5.
fn weather_set_turns(item: Item, weather: Weather) -> i8 {
    let extended = matches!(
        (item, weather),
        (Item::HeatRock, Weather::Sun)
            | (Item::DampRock, Weather::Rain)
            | (Item::SmoothRock, Weather::Sand)
            | (Item::IcyRock, Weather::Snow)
    );
    if extended { 8 } else { 5 }
}

/// Clear entry hazards from one side (Rapid Spin's own-side sweep; Defog/Tidy Up both sides).
fn clear_hazards(b: &mut Branch, side: SideId) {
    use SideConditionId::*;
    let c = b.state.side(side).side_conditions;
    for (sc, cur) in [
        (StealthRock, c.stealth_rock as u8),
        (Spikes, c.spikes),
        (ToxicSpikes, c.toxic_spikes),
        (StickyWeb, c.sticky_web as u8),
    ] {
        if cur > 0 {
            push(b, Instruction::SetSideCondition { side, condition: sc, previous: cur, new: 0 });
        }
    }
}

/// Post-hit hazard removal for spin moves; Defog / Tidy Up handle theirs in the status path.
fn apply_spin_clear(b: &mut Branch, side: SideId, md: &crate::data::MoveData) {
    if !matches!(md.id.to_id(), "rapidspin" | "mortalspin") {
        return;
    }
    if !b.state.side(side).active().is_alive() {
        return;
    }
    clear_hazards(b, side);
    for v in [VolatileStatus::LeechSeed, VolatileStatus::PartiallyTrapped] {
        if b.state.side(side).volatiles.contains(v) {
            push(b, Instruction::RemoveVolatile { side, volatile: v });
        }
    }
    let pt = (b.state.side(side).partial_trap_turns, b.state.side(side).partial_trap_div);
    if pt != (0, 0) {
        push(b, Instruction::SetPartialTrap { side, previous: pt, new: (0, 0) });
    }
}

/// Whirlwind / Roar / Dragon Tail / Circle Throw: drag the foe into a uniformly-random alive
/// bench mon (each target is its own branch). No-op if the foe has no bench or fainted.
fn apply_drag(b: Branch, dragged: SideId) -> Vec<Branch> {
    if !b.state.side(dragged).active().is_alive() {
        return vec![b];
    }
    // Ingrain blocks forced switching (`onDragOut`). Note the Mean-Look-family `trapped` and
    // partial traps do NOT block dragging — forced switches bypass ordinary trapping.
    if b.state.side(dragged).volatiles.contains(VolatileStatus::Ingrain) {
        return vec![b];
    }
    let sd = b.state.side(dragged);
    let alive_benched = |i: u8| i != sd.active_index
        && sd.pokemon[i as usize].species != crate::ids::Species::None
        && sd.pokemon[i as usize].is_alive();
    // PS's `getRandomSwitchable` samples over `side.pokemon.slice(active.length)` — its CURRENT
    // array order (active-first, swap-tracked by `switchIn`), NOT canonical teampreview order. So
    // the drawn index maps into PS's array order. The seed gate installs the PRE-STATE array order
    // (shared with Beat Up); Enumerate/Sample leave it None and fall back to canonical slot order.
    //
    // If the dragged side switched EARLIER this turn (its own switch, before the drag), `switchIn`
    // swapped the incoming mon to array position 0, so the pre-state order's head is stale. Mirror
    // that single swap: the current active (`active_index`) sat at some position `j` in the
    // pre-state order and PS moved it to the front via `swap(0, j)`. Reconstructing that yields PS's
    // live array order to sample over (d3: p1 switched Tornadus in, then Dragon Tail dragged — the
    // bench must be PS's post-switch array, not canonical). Without an intra-turn switch `j == 0`
    // and the swap is a no-op, so the common case is unchanged.
    let bench: Vec<u8> = sd.roster.iter().copied().filter(|&i| alive_benched(i)).collect();
    if bench.is_empty() {
        return vec![b];
    }
    // PS picks the drag target with `sample(possibleSwitches)` (battle.ts getRandomSwitchable):
    // one `sample` draw over the bench in party order. Each branch carries the drawn index as its
    // `sample` result so the Replicate filter selects the realized target. Annotation-only.
    let n = bench.len() as i32;
    let p = 1.0 / bench.len() as f32;
    bench
        .into_iter()
        .enumerate()
        .map(|(idx, t)| {
            let mut nb = scaled(&b, p);
            draw(&mut nb, "sample", &[n], idx as i64, "drag");
            emit_drag_switchin_sort(&mut nb, dragged, t);
            apply_switch(&mut nb, dragged, t);
            nb
        })
        .collect()
}

/// PS's `pokemon.speed` for a BENCHED mon is ALWAYS its unboosted `storedStats.spe`: every path
/// off the field ends in `clearVolatile()` (`sim/pokemon.ts:1509`), whose last statement is
/// `setSpecies(this.baseSpecies)`, whose last statement is `this.speed = this.storedStats.spe`
/// (`:1419`). Measured against the recorded corpus: **197714 benched snapshots across the 401
/// sidecars, ZERO with `speed !== storedStats.spe`.** No item, ability, boost, paralysis, Tailwind
/// or weather modifier is in it — those live in `getActionSpeed()`, which only `updateSpeed()`
/// calls.
fn benched_speed_cache(state: &State, side: SideId, slot: u8) -> i32 {
    state.side(side).pokemon[slot as usize].stat(crate::ids::StatIndex::Speed) as i32
}

/// A DRAG does NOT refresh the incoming mon's Speed cache, and every later speed-sort in the
/// action sees the POST-drag board.
///
/// `switchIn(..., isDrag=true)` on gen >= 5 calls `this.runSwitch(pokemon)` DIRECTLY
/// (`sim/battle-actions.ts:145-150`) instead of `queue.insertChoice({choice:'runSwitch'})` — and
/// `insertChoice` is the ONLY caller of `choice.pokemon.updateSpeed()`
/// (`sim/battle-queue.ts:374`). So unlike an ordinary switch (see `switch_entry_speed`), the
/// dragged-in mon's `pokemon.speed` is never recomputed: it keeps the benched value above until
/// the end-of-`runAction` `updateSpeed()` at `sim/battle.ts:2942`.
///
/// Two sorts then run on that board, both potentially one `shuffle[2,0,2]`:
///   1. `runSwitch`'s `speedSort(getAllActive(true))` (`sim/battle-actions.ts:181-182`) — THIS
///      function. It precedes `fieldEvent('SwitchIn')`, so it is emitted BEFORE `apply_switch`
///      runs hazards and switch-in abilities, on the post-drag Speed pair.
///   2. the move action's trailing `eachEvent('Update')` (`sim/battle.ts:2879`) — carried on the
///      branch as `drag_tie_speeds` and consumed in `run_move_action`.
///
/// Witnesses, all rb1360 (Hydrapple 121 phazes with Dragon Tail):
///   * d31 t27 and d77 t67 drag **Dipplin** (stored Spe 121) back in: PS records exactly the two
///     shuffles above, `sample[2]@dragontail` then `shuffle[2,0,2]` (eventid `null` = the
///     `speedSort`) then `shuffle[2,0,2]` (eventid `Update` = the 2882), both groups reading
///     `["p1: Hydrapple:121", "p2: Dipplin:121"]`.
///   * d6 t6 drags **Empoleon** (stored Spe 149) in: PS's unit stream ENDS at `sample[4]@drag`.
///     The engine used to emit a twelfth draw here because the frozen PRE-move pair was the
///     OUTGOING Dipplin's 121 against Hydrapple's 121 — a tie that the replacement breaks.
///     That extra `shuffle` was the corpus's last PRNG offset (`engine=53 ps=52` at the d7
///     boundary).
fn emit_drag_switchin_sort(b: &mut Branch, dragged: SideId, target: u8) {
    let other = dragged.other();
    let mut sp = MOVE_TIE_SPEEDS.with(|c| c.get()).unwrap_or([
        effective_speed(&b.state, SideId::One),
        effective_speed(&b.state, SideId::Two),
    ]);
    sp[dragged as usize] = benched_speed_cache(&b.state, dragged, target);
    // Carry the pair to the action's trailing 2882 (which sorts `getAllActive()` — fainted
    // excluded, so `emit_update`'s own liveness test still applies).
    b.drag_tie_speeds = Some(sp);
    // `runSwitch` sorts `getAllActive(TRUE)`, i.e. fainted actives are still in the list — the
    // attacker may have died to recoil / Rocky Helmet before the phaze resolved. A fainted mon has
    // already been through `clearVolatile(false)`, so it sorts on the same benched cache.
    let other_slot = b.state.side(other).active_index;
    let other_occupied = b.state.side(other).active().species != crate::ids::Species::None;
    if !b.state.side(other).active().is_alive() {
        sp[other as usize] = benched_speed_cache(&b.state, other, other_slot);
    }
    if annotating() && other_occupied && sp[0] == sp[1] {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
    }
}

fn future_sight_rolls_crit(state: &State, target_side: SideId, caster_slot: u8, is_crit: bool) -> [i16; 16] {
    let src_side = target_side.other();
    let caster = &state.side(src_side).pokemon[(caster_slot as usize).min(5)];
    let target = state.side(target_side).active();
    let doom = caster.moves.iter().any(|m| m.id.to_id() == "doomdesire");
    let (bp, typ) = if doom { (140u16, Type::Steel) } else { (120u16, Type::Psychic) };
    let sc = &state.side(target_side).side_conditions;
    let screened = sc.light_screen > 0 || sc.aurora_veil > 0;
    let input = crate::damage::DamageInput {
        level: caster.level,
        base_power: bp,
        category: MoveCategory::Special,
        move_type: typ,
        attacker_types: caster.types,
        attacker_base_types: caster.live_types,
        defender_types: target.types,
        attack_stat: caster.stat(crate::ids::StatIndex::SpecialAttack),
        defense_stat: target.stat(crate::ids::StatIndex::SpecialDefense).max(1),
        is_crit,
        attacker_burned: false,
        weather: state.weather,
        terastallized: caster.terastallized,
        defender_terastallized: false,
        tera_type: caster.tera_type,
        life_orb: false,
        adaptability: false,
        tera_shell: false,
        freeze_dry: false,
        trunc_16: state.ruleset.bit_truncation,
        final_num: 1,
        final_den: if screened { 2 } else { 1 },
    };
    crate::damage::damage_rolls(&input)
}

// --- end of turn -------------------------------------------------------------

/// One PS end-of-turn Residual event-handler, reduced to the keys `comparePriority` ties on.
/// PS collects a handler for every effect (status / volatile / ability / item / side-condition /
/// weather / terrain / trick-room) that has an `onResidual`-family callback OR a live `duration`,
/// then `speedSort`s them; a tie needs equal (order, priority, speed, subOrder, effectOrder). For
/// the Residual event priority and effectOrder are 0 for all handlers, so a tie ⟺ equal
/// (order, speed, subOrder). `order` "false" (no `onResidualOrder`) sorts last, as `i64::MAX`.
#[derive(Clone, Copy)]
struct ResHandler {
    order: i64,
    speed: i64,
    sub_order: i64,
}

/// Build PS's Residual handler list from the current board (keys per `resolvePriority` /
/// data at pin `b9dc987d`). Orders/subOrders are the resolved values recorded by the cosim
/// label-audit (weather onFieldResidual 1/5; terrain field 27/7 + Grassy per-active 5/2;
/// trick-room 27/1; screens 26/{reflect1,lightscreen2,tailwind5,auroraveil10}; leftovers &
/// black sludge 5/4; toxic/flame orb & sticky barb 28/3; speedboost/baddreams/harvest/cudchew
/// 28/2; hungerswitch 29/7; wish slot-condition 4/3; psn/tox 9/0, brn 10/0; and the ordered
/// volatiles leechseed 8, nightmare 11, curse 12, saltcure/partialtrap 13, octolock 14, taunt 15,
/// encore 16, disable 17, magnetrise 18, healblock 20, throatchop 22, yawn 23, perish 24,
/// ingrain 7, roost 25 — all subOrder 2; and protect/stall subOrder 2 at order "false").
fn residual_handlers(state: &State) -> Vec<ResHandler> {
    use crate::ids::{Ability as Ab, Item as It, Status as St, Terrain, Weather};
    const FALSE: i64 = i64::MAX;
    let mut hs: Vec<ResHandler> = Vec::new();
    let field = |order: i64, sub: i64| ResHandler { order, speed: 0, sub_order: sub };

    // --- field-level handlers (holder = field/side, speed 0) ---
    if state.weather != Weather::None {
        hs.push(field(1, 5)); // weather onFieldResidual (all weathers)
    }
    if state.terrain != Terrain::None {
        hs.push(field(27, 7)); // terrain field-level duration handler
    }
    if state.trick_room {
        hs.push(field(27, 1));
    }
    for side in [SideId::One, SideId::Two] {
        let sc = &state.side(side).side_conditions;
        if sc.reflect > 0 { hs.push(field(26, 1)); }
        if sc.light_screen > 0 { hs.push(field(26, 2)); }
        if sc.tailwind > 0 { hs.push(field(26, 5)); }
        if sc.aurora_veil > 0 { hs.push(field(26, 10)); }
    }

    // --- per-active handlers (holder = active, speed = its current Speed) ---
    for side in [SideId::One, SideId::Two] {
        let s = state.side(side);
        let p = s.active();
        let speed = effective_speed(state, side) as i64;
        if !p.is_alive() {
            // A mon that fainted THIS turn is still in PS's `side.active`, so `fieldEvent('Residual')`
            // collects its surviving residual handlers into the `speedSort` (the while-loop then skips
            // EXECUTION for the fainted holder, but the SORT — hence the shuffle — already ran over it).
            // `clearVolatile` on faint (pokemon.ts:1509) wipes volatiles + boosts but the ITEM and
            // STATUS persist, so only the item/orb and status residuals survive to tie the foe's. This
            // fires only while the fainted mon still occupies the active slot (before its replacement
            // switches in) — the mid-turn-faint-then-residual window. Ground-truthed on c3/c4/c5/r6: a
            // just-fainted Leftovers holder ties the surviving foe's Leftovers → one extra shuffle[2,0,2]
            // (r6: two Cud Chew holders tie on the ABILITY residual → same). Only faint-surviving
            // handlers (item / status / ability) are collected; every volatile/side/terrain residual is
            // wiped by `clearVolatile` on faint.
            // ...and because `clearVolatile` already ran, the Speed this handler sorts on is the
            // mon's UNBOOSTED, volatile-free Speed. `clearVolatile` zeroes `this.boosts` and empties
            // `this.volatiles` (sim/pokemon.ts:1509), and `runAction`'s `case 'residual'` calls
            // `updateSpeed()` at its START (sim/battle.ts:2835) — AFTER `faintMessages` — so the
            // cache the sort reads is recomputed from the cleared board. Status, item and the side
            // conditions survive, so paralysis / Choice Scarf / Tailwind still count.
            //
            // rb1021 d102 t91 is the witness: p1's Magnezone sits at spe −1 under its own side's
            // Sticky Web (`pokemon.speed` 100 in the sidecar at d101, boosts.spe −1) and is KO'd by
            // Sylveon's Hyper Voice. PS's residual list is the two Leftovers holders at speed
            // **151 and 151** — Magnezone's web drop is gone with its boosts — so they TIE and PS
            // draws one `shuffle[2,0,2]`. The engine read the live boosted 100, saw no tie, made no
            // draw and ran −1 behind for the rest of the game.
            let speed = {
                let mut st = *state;
                let sm = st.side_mut(side);
                sm.boosts = [0; 7];
                sm.volatiles = Default::default();
                effective_speed(&st, side) as i64
            };
            let mut fpush = |order: i64, sub: i64| hs.push(ResHandler { order, speed, sub_order: sub });
            // Grassy Terrain's per-active heal is a FIELD handler collected per active
            // (`findFieldEventHandlers(field, 'onResidual', undefined, active)`, battle.ts:503) —
            // it lives on the terrain, not on the mon, so `clearVolatile` on faint does NOT remove
            // it and the fainted active still contributes one to the speedSort. (r10 d32: Fire
            // Blast KOs Rillaboom under its own Grassy Surge terrain; PS's residual list is
            // [snowscape(1,5), grassy/p1(5,2), grassy/p2-fainted(5,2), grassy-field(27,7)] →
            // `shuffle[4,1,3]`, which the engine dropped to a 3-handler untied list.)
            if state.terrain == Terrain::Grassy {
                fpush(5, 2);
            }
            match p.item {
                It::Leftovers | It::BlackSludge => fpush(5, 4),
                It::ToxicOrb | It::FlameOrb | It::StickyBarb => fpush(28, 3),
                It::WhiteHerb => fpush(29, 8),
                _ => {}
            }
            match p.status {
                St::Poison | St::Toxic => fpush(9, 0),
                St::Burn => fpush(10, 0),
                _ => {}
            }
            match p.ability {
                Ab::Hydration | Ab::ShedSkin => fpush(5, 3),
                Ab::SpeedBoost | Ab::BadDreams | Ab::Harvest | Ab::CudChew | Ab::Moody | Ab::Pickup | Ab::SlowStart => fpush(28, 2),
                Ab::HungerSwitch | Ab::ShieldsDown => fpush(29, 7),
                _ => {}
            }
            continue;
        }
        let mut push = |order: i64, sub: i64| hs.push(ResHandler { order, speed, sub_order: sub });

        // Grassy Terrain heals each active (per-active handler), regardless of grounding
        // (the grounding test is inside PS's callback, after collection).
        if state.terrain == Terrain::Grassy {
            push(5, 2);
        }
        // Status residuals.
        match p.status {
            St::Poison | St::Toxic => push(9, 0),
            St::Burn => push(10, 0),
            _ => {}
        }
        // Ordered volatile / counter conditions (all subOrder 2).
        use crate::volatile::VolatileStatus as V;
        let v = s.volatiles;
        if v.contains(V::Ingrain) { push(7, 2); }
        if v.contains(V::LeechSeed) { push(8, 2); }
        if v.contains(V::Nightmare) { push(11, 2); }
        if v.contains(V::Curse) { push(12, 2); }
        if v.contains(V::SaltCure) { push(13, 2); }
        if s.partial_trap_turns > 0 { push(13, 2); }
        if v.contains(V::Octolock) { push(14, 2); }
        if s.taunt_turns > 0 { push(15, 2); }
        if s.encore.1 > 0 { push(16, 2); }
        if s.disable.1 > 0 { push(17, 2); }
        if s.magnet_rise_turns > 0 { push(18, 2); }
        if s.heal_block_turns > 0 { push(20, 2); }
        if s.throat_chop_turns > 0 { push(22, 2); }
        if s.yawn_turns > 0 { push(23, 2); }
        if s.perish_turns > 0 { push(24, 2); }
        if v.contains(V::Roosted) { push(25, 2); } // roost's end-of-turn type-restore volatile
                                                    // (engine uses `Roosted`; `Roost` is never set)
        // Wish resolves as a slot condition on the occupant (order 4, subOrder 3).
        if s.wish.0 > 0 { push(4, 3); }
        // Items.
        match p.item {
            It::Leftovers | It::BlackSludge => push(5, 4),
            It::ToxicOrb | It::FlameOrb | It::StickyBarb => push(28, 3),
            // White Herb's `onResidual` is `onResidualOrder: 29` with the default Item subOrder 8
            // (`data/items.ts:7697`, `sim/battle.ts:968`) — the LAST ordered handler in the queue,
            // behind Hunger Switch / Shields Down (29/7). It is collected for every holder whether
            // or not it has a negative stage to clear (the check is inside the callback), so two
            // White Herb holders at equal Speed tie. rb1345 d11: both Blastoise hold one and the
            // whole residual list is exactly those two → `shuffle[2,0,2]`, which the engine had as
            // an EMPTY list. Also rb1034 d32 / rb1303 d32.
            It::WhiteHerb => push(29, 8),
            _ => {}
        }
        // Abilities with an end-of-turn onResidual.
        match p.ability {
            // Hydration (order 5, subOrder 3): its onResidual is collected for every living
            // holder regardless of weather (the rain/status check is inside the callback), so
            // it sits in the Residual handler list ahead of Leftovers (order 5, subOrder 4) and
            // the order-`false` stall/protect tie. The engine previously omitted it, shortening
            // the list by one and mis-sizing the tail shuffle (`[2,0,2]`/`[3,1,3]` where PS has
            // `[3,1,3]`/`[4,2,4]`). Verified as the ONLY residual handler missing corpus-wide.
            // Shed Skin is the same slot: `onResidualOrder: 5, onResidualSubOrder: 3`
            // (data/abilities.ts:4142-4151), and like Hydration its `pokemon.status` test lives
            // INSIDE the callback, so the handler is collected for every living holder. No corpus
            // residual shuffle has ever fired with a Shed Skin holder on the field, so this is
            // reasoned from the pin rather than measured; it cannot change any recorded shuffle.
            Ab::Hydration | Ab::ShedSkin => push(5, 3),
            Ab::SpeedBoost | Ab::BadDreams | Ab::Harvest | Ab::CudChew | Ab::Moody | Ab::Pickup | Ab::SlowStart => push(28, 2),
            // Minior's Shields Down is an `onResidual` at order 29 with the default Ability
            // subOrder 7 — the same slot Hunger Switch occupies. rb1034 d32.
            Ab::HungerSwitch | Ab::ShieldsDown => push(29, 7),
            _ => {}
        }
        // PS registers a Residual handler for EVERY effect carrying a live `duration`, whether or
        // not it has an `onResidual` (`fieldEvent`'s `getKey = 'duration'`, `sim/battle.ts:486`,
        // and `findPokemonEventHandlers`'s `getKey && volatileState[getKey]` at `:1111`). Two
        // duration-only volatiles the engine never listed:
        //   `flinch` (duration 1, `data/conditions.ts:198`) — still on the flinched mon at the
        //     residual, removed by endTurn. rb1034 d56 `[3,1,3]` = leftovers/flinch/stall at one
        //     Speed; rb1378 d4 `[4,2,4]`.
        //   `twoturnmove` (duration 2, `:287`) — on a mon spending this turn charging. The
        //     SEMI-INVULNERABLE moves (Fly / Dig / Dive / Bounce / Phantom Force / Shadow Force /
        //     Sky Drop) add a SECOND handler, their own condition (also duration 2); Solar Beam /
        //     Meteor Beam / Electro Shot / Sky Attack have no condition at all and add just the
        //     one (rb1024 d106 records exactly one `twoturnmove/false/2`).
        // Both are Conditions with no `onResidualOrder` → order `false`, default subOrder 2, so
        // they tie with the protect/stall pair at the tail of the queue.
        if v.contains(V::Flinch) {
            push(FALSE, 2);
        }
        if let crate::state::PendingMove::Charging(m) = s.pending_move {
            push(FALSE, 2); // twoturnmove
            if is_semi_invuln_move(m) {
                push(FALSE, 2); // the move's own semi-invulnerability condition
            }
        }
        // `lockedmove` is the third of the same family: `duration: 2` AND a real `onResidual`
        // (`data/conditions.ts:253-262`), no `onResidualOrder` → order `false`, Condition subOrder
        // 2. A rampaging mon therefore contributes one handler for every turn its lock is live.
        // Like Shed Skin above this is reasoned from the pin, not measured: no recorded residual
        // shuffle in the 401-game corpus fired while a rampage was live, so the `full` census
        // shows no `lockedmove` row either way. The gate below is the check.
        if v.contains(V::LockedMove) {
            push(FALSE, 2);
        }
        // Protect + Stall: PS registers a Residual handler (via `getKey:'duration'`,
        // battle.ts:487) for EACH duration-carrying volatile, independent of any onResidual
        // callback. The `protect` volatile has duration 1 (removed the turn it's used); the
        // `stall` volatile has duration 2 (conditions.ts), so it survives ONE residual PAST the
        // protect volatile — on the turn AFTER a Protect (protect gone, stall still counting down)
        // PS still keeps the stall handler, giving a 1-longer list (`[5,2,4]` vs the engine's old
        // `[4,2,4]`). So the two handlers are gated INDEPENDENTLY: `protect` iff the Protect
        // volatile is present (its own turn), `stall` iff the stall volatile is present. The stall
        // volatile's presence is `stall_turns` (its `duration`, decremented each end of turn), NOT
        // `stall_counter` (the onStallMove 3^n denominator) — a non-Protect move resets the counter
        // to 0 for the roll chain but the volatile persists to this turn's residual (t1 t17: golurk
        // protects, then Shadow Punch next turn — PS still lists stall → `[3,0,2]`, engine dropped
        // it → `[2,0,2]`), and log3 rounds the turn-after counter to 0 while the volatile lives.
        // Both order "false", subOrder 2. (Stream-neutral for the from-seed gate — both list lengths
        // consume one `random` over the same tie-group — so this only sharpens the differ's strict
        // args comparison; no game's draw count changes.)
        if v.contains(V::Protect) {
            push(FALSE, 2); // protect (own-turn: duration-1 volatile)
        }
        if s.stall_turns > 0 {
            push(FALSE, 2); // stall (duration-2 volatile; survives one turn past protect —
                            // presence tracked by `stall_turns`, NOT the onStallMove `stall_counter`)
        }
    }
    hs
}

/// Put back the Flying type Roost stripped from `side`'s active. The engine encodes PS's
/// `roost` volatile (whose `onType` filters Flying out of `getTypes()`) as a real type change
/// plus the `Roosted` marker; this is the undo half.
fn restore_roost_typing_side(b: &mut Branch, side: SideId) {
    let p = b.state.side(side).active();
    if !p.is_alive() {
        return;
    }
    let slot = b.state.side(side).active_index;
    // Restore to the LIVE type array, not the species typing: a Protean / Double Shock user
    // that then Roosts must come back to what `pokemon.types` actually holds. (PS has nothing
    // to restore — removing the volatile stops the `onType` filter — so `live_types` never
    // moved and is the ground truth here.)
    let restored = if p.terastallized { [p.tera_type, Type::None] } else { p.live_types };
    if p.types != restored {
        push(b, Instruction::ChangeTypes { side, slot, previous: p.types, new: restored });
    }
}

/// Both sides' Roost typing, for the battle-ended abort path (the `Roosted` marker itself is
/// left standing, matching PS, and is masked out of the state digest).
fn restore_roost_typing(b: &mut Branch) {
    for side in [SideId::One, SideId::Two] {
        if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {
            restore_roost_typing_side(b, side);
        }
    }
}

/// The two sides in PS's residual `speedSort` order — fastest active first, ties left in
/// side order (a tie is exactly the case the emitted `shuffle` randomizes, and the digest
/// cannot see which way it fell unless the two chips interact). Uses the same
/// `effective_speed` `residual_handlers` sorts its per-active handlers by, so the applied
/// order and the emitted shuffle order are derived from one number.
fn residual_side_order(state: &State) -> [SideId; 2] {
    if effective_speed(state, SideId::Two) > effective_speed(state, SideId::One) {
        [SideId::Two, SideId::One]
    } else {
        [SideId::One, SideId::Two]
    }
}

/// Emit the `speedSort` shuffle draws PS makes over the Residual handler list. Selection-sort
/// mirror of `comparePriority`: sort best-first (order asc, speed desc, subOrder asc), then for
/// every maximal run of ≥2 mutually-tying handlers emit one `shuffle[len, start, start+run]`, in
/// ascending start position — exactly the sequence `battle.speedSort` produces.
fn emit_residual_shuffles(b: &mut Branch) {
    let mut hs = residual_handlers(&b.state);
    if hs.len() < 2 {
        return;
    }
    let len = hs.len() as i32;
    // Stable sort keeps handler build order within a tie group (irrelevant — the group is shuffled).
    hs.sort_by(|a, c| a.order.cmp(&c.order).then(c.speed.cmp(&a.speed)).then(a.sub_order.cmp(&c.sub_order)));
    let ties = |a: &ResHandler, c: &ResHandler| a.order == c.order && a.speed == c.speed && a.sub_order == c.sub_order;
    let mut i = 0usize;
    while i + 1 < hs.len() {
        let mut j = i + 1;
        while j < hs.len() && ties(&hs[i], &hs[j]) {
            j += 1;
        }
        if j - i >= 2 {
            draw(b, "shuffle", &[len, i as i32, j as i32], -1, "residual");
        }
        i = j;
    }
}

/// Emit the `speedSort` shuffle draws PS makes over the **`AfterMove`** handler list.
///
/// `runMove` fires `this.battle.runEvent('AfterMove', pokemon, target, move)` right after
/// `useMove` returns (sim/battle-actions.ts:312) — after the move's own internal 970/1024 Updates
/// and before the move action's trailing runAction 2882. `runEvent` collects `onAnyAfterMove` from
/// EVERY active on the field, so a handler is contributed per holder regardless of which side
/// moved. Exactly four effects register one at pin `b9dc987d`:
///   White Herb (data/items.ts:7694), Eject Pack (:1729), Mirror Herb (:4176) — Items, default
///   subOrder 8 — and Opportunist (data/abilities.ts:3024) — Ability, default subOrder 7.
/// None declares an order, so all sit at order `false`, and two holders at equal Speed tie.
///
/// Witness rb1345 d11: both Blastoise hold a White Herb at equal Speed. PS records the shuffle
/// TWICE in the unit (once per move action) with `full` = exactly `[whiteherb/Item/false/8] x2`,
/// which is also the whole census: across all 401 sidecars the ONLY `AfterMove` handler rows are
/// those four `whiteherb` entries, so nothing else in the corpus lengthens this list.
///
/// (The mover's own `onAfterMove` handlers — `lockedmove` (data/conditions.ts:273, Condition
/// subOrder 2) — and the MOVE's own `onAfterMove`, which `runEvent` unshifts as a `sourceEffect`
/// at subOrder 0 (sim/battle.ts:783), would lengthen the list without joining the White Herb tie.
/// The corpus contains no instance of either co-occurring with a tie, so they are deliberately not
/// modelled here; add them with a witness.)
fn emit_after_move_shuffles(b: &mut Branch) {
    if !annotating() {
        return;
    }
    let mut hs: Vec<ResHandler> = Vec::new();
    for side in [SideId::One, SideId::Two] {
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let speed = effective_speed(&b.state, side) as i64;
        if matches!(p.item, Item::WhiteHerb | Item::EjectPack | Item::MirrorHerb) {
            hs.push(ResHandler { order: i64::MAX, speed, sub_order: 8 });
        }
        if p.ability == crate::ids::Ability::Opportunist {
            hs.push(ResHandler { order: i64::MAX, speed, sub_order: 7 });
        }
    }
    if hs.len() < 2 {
        return;
    }
    let len = hs.len() as i32;
    hs.sort_by(|a, c| a.order.cmp(&c.order).then(c.speed.cmp(&a.speed)).then(a.sub_order.cmp(&c.sub_order)));
    let ties = |a: &ResHandler, c: &ResHandler| a.order == c.order && a.speed == c.speed && a.sub_order == c.sub_order;
    let mut i = 0usize;
    while i + 1 < hs.len() {
        let mut j = i + 1;
        while j < hs.len() && ties(&hs[i], &hs[j]) {
            j += 1;
        }
        if j - i >= 2 {
            draw(b, "shuffle", &[len, i as i32, j as i32], -1, "aftermove");
        }
        i = j;
    }
}

/// Shed Skin's `onResidual` is `onResidualOrder: 5, onResidualSubOrder: 3`
/// (`data/abilities.ts:4142-4151`) — the SAME slot as Hydration, and BEFORE the psn/tox/brn chip
/// at order 9/10, so a holder cured this turn takes no status damage. But it is a 33% SPLIT and
/// `apply_end_of_turn`'s deterministic core is a single `&mut Branch` walk over orders 1..29, so
/// the split cannot happen inside it.
///
/// This wrapper hoists it: it enumerates the outcome combinations (at most one holder per side,
/// so at most four) UP FRONT and runs the core once per combination with each holder's outcome
/// FORCED. The core then emits the `randomChance(33,100)` draw and applies the cure at the exact
/// order-5/3 slot. `None` = "this side has no Shed Skin roll to make".
///
/// PS's own guard is `if (pokemon.hp && pokemon.status && this.randomChance(33, 100))` — the roll
/// is short-circuited unless the holder is alive AND statused, which is why the combination set is
/// computed from exactly that predicate. It is evaluated here on the PRE-residual board and again,
/// authoritatively, at the order-5/3 site; nothing between residual start and order 5/3 can add or
/// remove a status, so the only way the two disagree is a holder that faints to the order-1 weather
/// chip. In that case the site makes no draw and both forced branches are byte-identical — a
/// harmless duplicate whose probabilities still sum to the parent's.
///
/// Witnesses: rb1315 d28 (PS 205, engine 189 = 205 − 258/16 — exactly one skipped status chip) and
/// rb1380 d15.
/// PS's `tox` residual stops advancing at stage 15 (`data/conditions.ts:156`).
const TOXIC_MAX_STAGE: u8 = 15;

pub(crate) fn apply_end_of_turn(branch: Branch, switched: [bool; 2]) -> Vec<Branch> {
    let mut holders: Vec<SideId> = Vec::new();
    for side in [SideId::One, SideId::Two] {
        let p = branch.state.side(side).active();
        if p.ability == crate::ids::Ability::ShedSkin && p.status != Status::None && p.is_alive() {
            holders.push(side);
        }
    }
    // A rampage lock whose LAST turn passed without the move being used expires at the residual
    // and `onEnd`-confuses (see the residual block for the rule and the witnesses). Its
    // `random(2, 6)` duration draw is pre-forked here for the same reason Shed Skin's is: the
    // residual body cannot branch.
    let mut conf: Vec<SideId> = Vec::new();
    for side in [SideId::One, SideId::Two] {
        if residual_rampage_confuses(&branch.state, side) {
            conf.push(side);
        }
    }
    if holders.is_empty() && conf.is_empty() {
        return apply_end_of_turn_inner(branch, switched, [None, None], [None, None]);
    }
    let mut out = Vec::new();
    let conf_combos = 4usize.pow(conf.len() as u32);
    for mask in 0u32..(1u32 << holders.len()) {
        let mut shed = [None, None];
        let mut w = 1.0f32;
        for (i, &side) in holders.iter().enumerate() {
            let proc = (mask >> i) & 1 == 1;
            shed[side.index()] = Some(proc);
            w *= if proc { 33.0 / 100.0 } else { 67.0 / 100.0 };
        }
        for cmask in 0..conf_combos {
            let mut conf_dur = [None, None];
            let mut cw = w;
            let mut m = cmask;
            for &side in &conf {
                conf_dur[side.index()] = Some(2 + (m % 4) as u8);
                m /= 4;
                cw *= 0.25;
            }
            out.extend(apply_end_of_turn_inner(scaled(&branch, cw), switched, shed, conf_dur));
        }
    }
    out
}

/// Will this side's rampage lock expire AT THE RESIDUAL with a confusion?
///
/// `lockedmove` carries `duration: 2` and `onEnd(target) { if (trueDuration > 1) return;
/// target.addVolatile('confusion') }` (`data/conditions.ts`). The DURATION is what ends the
/// volatile, and PS ticks it in the residual pass — **whether or not the mon used the move that
/// turn**. The engine releases the lock at move time (the `n == 1` arm of `apply_rampage_state`),
/// which agrees with PS on every turn the locked mon actually moves and leaves the lock armed
/// forever on a turn it does not.
///
/// PS gives out such turns freely, and the corpus has three:
/// * rb5059 d31-d33: a Lilligant's Petal Dance KOs the Chi-Yu at t26; t27 is the REPLACEMENT
///   turn — a `switch` request whose `go()` still inserts `beforeTurn`+`residual` — so PS ticks
///   the duration to 0 and releases with no move used at all. Own Tempo, so no confusion.
/// * rb5160 and rb5321, same shape. rb5321 is the one that shows the missing draw directly:
///   `PS-unconsumed random[2, 6]@confusion`.
fn residual_rampage_confuses(state: &State, side: SideId) -> bool {
    let crate::state::PendingMove::Rampaging(_, n) = state.side(side).pending_move else {
        return false;
    };
    let p = state.side(side).active();
    n <= 1
        && p.is_alive()
        // A sleeping rampager's lock is DELETED rather than ended, so `onEnd` never confuses.
        && p.status != Status::Sleep
        && p.ability != crate::ids::Ability::OwnTempo
        && !state.side(side).volatiles.contains(VolatileStatus::Confusion)
}

fn apply_end_of_turn_inner(
    mut branch: Branch, _switched: [bool; 2], shed: [Option<bool>; 2], conf_dur: [Option<u8>; 2],
) -> Vec<Branch> {
    // PS's residual pass ABORTS the moment the battle ends: `fieldEvent` runs
    // `this.faintMessages(); if (this.ended) return;` after EVERY handler (sim/battle.ts:565-566,
    // and again at :519 for a duration-expiry `end`). `turnLoop` then returns on `this.ended`
    // (:2974) WITHOUT calling `endTurn()`, so none of endTurn's per-active bookkeeping runs
    // either. The engine used to run the whole residual to completion and then do the end-of-turn
    // resets unconditionally, which over-applied every handler ordered after the killing one.
    use crate::instruction::ActiveCounter;
    let mut ended_early = false;
    let mut yawn_fired = [false; 2];
    let mut fs_fired: [Option<u8>; 2] = [None, None];
    // `runAction`'s `case 'residual'` calls `this.updateSpeed()` at its START (sim/battle.ts:2835),
    // and that is the LAST `updateSpeed` before the action's trailing `eachEvent('Update')` at
    // :2882. So the trailing Update speed-sorts on the Speed cached BEFORE any residual ran — a
    // Speed change the residual phase itself makes must not break (or make) its tie. Same cache
    // rule as the switch bracket, one event later. See `switch_entry_speed` / trap #2.
    let pre_residual_speeds =
        [effective_speed(&branch.state, SideId::One), effective_speed(&branch.state, SideId::Two)];
    'residual: {
    let b = &mut branch;
    // PS `fieldEvent('Residual')` speed-sorts the collected residual handlers via `speedSort`
    // (battle.ts:507); every tie-group of ≥2 handlers equal under `comparePriority` consumes one
    // `prng.shuffle`. Emit those shuffles here, in handler order, before any residual state is
    // applied (the shuffles are state-neutral — order is validated by `stateAfter`). No-op unless
    // draw annotation is on, so `Enumerate`/`Sample` are byte-unchanged.
    if annotating() {
        emit_residual_shuffles(b);
    }
    // Mirror Coat's per-turn special-damage record (PS's duration-1 `mirrorcoat` volatile) does
    // not survive to the next turn — clear it so it never leaks into a later Mirror Coat.
    for side in [SideId::One, SideId::Two] {
        let prev = b.state.side(side).special_damage_taken;
        if prev != 0 {
            push(b, Instruction::SetSpecialDamageTaken { side, previous: prev, new: 0 });
        }
        let prevp = b.state.side(side).physical_damage_taken;
        if prevp != 0 {
            push(b, Instruction::SetPhysicalDamageTaken { side, previous: prevp, new: 0 });
        }
    }
    // A Tera forme that fainted to a residual (poison, Leech Seed, ...) also regresses.
    regress_fainted_tera_formes(b);
    // PS decrements a residual effect's duration FIRST and, if it hits 0, ends the effect and
    // SKIPS its onResidual that turn (battle.ts residual loop). The SANDSTORM/snow CHIP and the
    // weather-tied ability heals (Rain Dish, Ice Body) are part of the weather's own residual, so
    // they are skipped on the weather's final turn — tick the weather here, before that loop. (The
    // Grassy Terrain heal is a separate per-mon handler that still fires on the terrain's final
    // turn, so the terrain duration is ticked AFTER the loop, below.)
    if b.state.weather != Weather::None {
        if b.state.weather_turns > 0 {
            push(b, Instruction::DecrementWeatherTurns);
            if b.state.weather_turns == 0 {
                // Duration hit 0: PS calls `field.clearWeather()` INSTEAD of the handler — one
                // `WeatherChange` shuffle, and no `eachEvent('Weather')` upkeep pair.
                set_weather(b, Weather::None, 0);
            } else {
                emit_weather_upkeep_shuffles(b);
            }
        } else {
            // Permanent weather (Primordial Sea / Desolate Land / Delta Stream, `duration: 0`):
            // the residual loop's `handler.state?.duration` check is falsy, so the upkeep handler
            // runs every turn.
            emit_weather_upkeep_shuffles(b);
        }
    }
    // Order: weather, then per active: Leftovers heal, status residual, Salt Cure.
    // (PS uses a finer speed-ordered residual queue; this covers the common cases.)
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let maxhp = p.max_hp;

        use crate::ids::Ability as Ab;
        let ability = p.ability;
        // Magic Guard cancels all indirect (residual) damage but not healing.
        let magic_guard = ability == Ab::MagicGuard;

        // Sandstorm chip — skipped for Rock/Ground/Steel types and sand-immune abilities.
        // The immunity set is exactly PS's four `onImmunity` abilities (data/abilities.ts:3921
        // sandforce, :3935 sandrush, :3962 sandveil, :3064 overcoat) plus Magic Guard. **Sand
        // Stream is NOT one of them** — a Tyranitar/Hippowdon standing in its own sandstorm is
        // chipped like anything else, and so is a Trace user that copied it. The types are
        // `getTypes()`, i.e. the TERA type once terastallized (sim/pokemon.ts:2138-2141), which the
        // engine already models by rewriting `types` on tera. rb1116 d7 (Tera Ghost Tyranitar:
        // Leftovers +17 then sand -17, netting 251 -> the engine healed to 268) and rb1283 d17
        // (a Gardevoir that Traced Sand Stream takes the chip).
        if effective_weather(&b.state) == Weather::Sand && !magic_guard {
            let immune = p.types.contains(&Type::Rock)
                || p.types.contains(&Type::Ground)
                || p.types.contains(&Type::Steel)
                || matches!(ability, Ab::SandVeil | Ab::SandRush | Ab::SandForce | Ab::Overcoat);
            if !immune {
                let dmg = (maxhp / 16).max(1).min(b.state.side(side).active().hp);
                if dmg > 0 {
                    push(b, Instruction::Damage { side, slot, amount: dmg });
                }
            }
        }
        // Weather healing abilities: Rain Dish (rain) / Ice Body (snow) / Dry Skin (rain)
        // restore 1/16; Dry Skin loses 1/8 in sun.
        {
            let p = b.state.side(side).active();
            if p.is_alive() {
                let heal16 = (matches!(ability, Ab::RainDish | Ab::DrySkin) && matches!(b.state.weather, Weather::Rain | Weather::HeavyRain))
                    || (ability == Ab::IceBody && b.state.weather == Weather::Snow);
                if heal16 && p.hp < p.max_hp && !heal_blocked(b, side) {
                    let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
                    push(b, Instruction::Heal { side, slot, amount: heal });
                } else if ability == Ab::DrySkin && matches!(b.state.weather, Weather::Sun | Weather::HarshSun) && !magic_guard {
                    let dmg = (maxhp / 8).max(1).min(p.hp);
                    push(b, Instruction::Damage { side, slot, amount: dmg });
                }
            }
        }
    }

    // --- residual order 4: Wish (slot condition) ---
    // Wish (PS `onResidualOrder: 4`, data/moves.ts:20945) is a SLOT condition, and
    // `fieldEvent('Residual')` runs slot-condition handlers even when the slot's occupant has
    // fainted: `if ((handler.effectHolder as Pokemon).fainted) { if (!(handler.state
    // ?.isSlotCondition)) continue; }` (sim/battle.ts:512-514). So the tick is unconditional —
    // it does NOT take the fainted-active guard — and PS's own bookkeeping is date-based
    // (`getOverflowedTurnCount() <= startingTurn` → not yet), so nothing can defer it.
    // On maturity the handler calls `removeSlotCondition` unconditionally; only the HEAL is
    // gated on a live occupant (`onEnd(target) { if (target && !target.fainted) heal }`). A
    // Wish that matures over a fainted slot is therefore CONSUMED with no heal — it does not
    // linger to a later end of turn.
    //
    // Order 4 puts it AFTER the weather chip/heals (field order 1) and BEFORE Grassy Terrain
    // (5/2), Leftovers (5/4), Ingrain (7), Leech Seed (8) and the status chip (9/10). The engine
    // used to run it at the very end of the residual pass, which let a later heal top up HP the
    // wish had already restored — rb1209 d28 is the witness: a matured Wish over Mismagius (at
    // FULL HP, so PS's `this.heal(212)` returns 0) then Leech Seed's order-8 drain of 30. PS
    // ends the turn at 213/243; the engine drained first and let the wish heal the 30 back.
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let wish = b.state.side(side).wish;
        if wish.0 == 0 {
            continue;
        }
        let landed = wish.0 == 1;
        let new = if landed { (0, 0) } else { (wish.0 - 1, wish.1) };
        push(b, Instruction::SetWish { side, previous: wish, new });
        if landed {
            let p = b.state.side(side).active();
            if p.is_alive() && p.hp < p.max_hp && !heal_blocked(b, side) {
                let amt = wish.1.min(p.max_hp - p.hp);
                let slot = b.state.side(side).active_index;
                push(b, Instruction::Heal { side, slot, amount: amt });
            }
        }
    }

    // --- residual order 5/3: Shed Skin ---
    // PS `data/abilities.ts:4142-4151`: `onResidualOrder: 5, onResidualSubOrder: 3`, guard
    // `if (pokemon.hp && pokemon.status && this.randomChance(33, 100)) pokemon.cureStatus()`.
    // That is BEFORE the psn/tox/brn chip at order 9/10, so a holder cured this turn takes NO
    // status damage — the whole point of the fix (rb1315 d28: PS 205, engine 189 = 205 − 258/16;
    // rb1380 d15). The outcome was forced by `apply_end_of_turn`'s wrapper because the core
    // cannot branch mid-walk; see it for why.
    //
    // Speed order, not side order: with two holders the two draws are consecutive and the residual
    // handler list is `speedSort`ed, so the FASTER holder rolls first. (The orders 5-7 loop below
    // still runs in side order — a standing approximation that no drawing handler sits in.)
    // Placed just ahead of that loop, i.e. between Wish (4) and Grassy Terrain (5/2): the ≤1-slot
    // inversion against 5/2 is unobservable — no handler at order 5 reads or writes `status`, and
    // none of them draws, so neither the stream position nor any HP is affected.
    for side in residual_side_order(&b.state) {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let Some(proc) = shed[side.index()] else { continue };
        let p = b.state.side(side).active();
        // Re-check PS's own short-circuit on the live board: a holder that fainted to the order-1
        // weather chip makes no roll at all.
        if p.ability != crate::ids::Ability::ShedSkin || p.status == Status::None || !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let (prev, prev_ctr) = (p.status, p.status_counter);
        draw(b, "randomChance", &[33, 100], proc as i64, "shedskin");
        if proc {
            push(b, Instruction::ChangeStatus { side, slot, previous: prev, new: Status::None });
            if prev_ctr != 0 {
                push(b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 0 });
            }
        }
    }

    // --- residual orders 5-7: Grassy Terrain (5/2), Leftovers / Black Sludge (5/4), Ingrain (7) ---
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let maxhp = p.max_hp;
        use crate::ids::Ability as Ab;
        let magic_guard = p.ability == Ab::MagicGuard;

        // Grassy Terrain heals grounded actives 1/16 max HP at end of turn (5/2 — AHEAD of
        // Leftovers at 5/4; both clamp at max HP, so with a Black Sludge chip in between the
        // two orders give different HP).
        let p = b.state.side(side).active();
        let grounded = !p.types.contains(&Type::Flying) && p.ability != crate::ids::Ability::Levitate;
        if b.state.terrain == crate::ids::Terrain::Grassy && grounded && p.hp < p.max_hp && p.is_alive() && !heal_blocked(b, side) {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
        }

        // Hydration cures any non-volatile status while it's raining — `onResidualOrder: 5`,
        // `onResidualSubOrder: 3` (`data/abilities.ts:1880-1889`). That is BEFORE the poison /
        // burn chip at order 9/10, so a Hydration holder in rain takes NO status damage on the
        // turn it is cured. The engine used to cure after the chip.
        let p = b.state.side(side).active();
        if p.ability == Ab::Hydration
            && matches!(b.state.weather, Weather::Rain | Weather::HeavyRain)
            && p.status != Status::None
            && p.is_alive()
        {
            let prev = p.status;
            let prev_ctr = p.status_counter;
            push(b, Instruction::ChangeStatus { side, slot, previous: prev, new: Status::None });
            if prev_ctr != 0 {
                push(b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 0 });
            }
        }

        // Leftovers (5/4).
        let p = b.state.side(side).active();
        if p.item == Item::Leftovers && p.hp < p.max_hp && p.is_alive() && !heal_blocked(b, side) {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
        }
        // Black Sludge: Leftovers for Poison types; 1/8 chip for anyone else.
        let p = b.state.side(side).active();
        if p.item == Item::BlackSludge && p.is_alive() {
            if p.types.contains(&Type::Poison) {
                if p.hp < p.max_hp && !heal_blocked(b, side) {
                    let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
                    push(b, Instruction::Heal { side, slot, amount: heal });
                }
            } else if !magic_guard {
                let dmg = (maxhp / 8).max(1).min(p.hp);
                push(b, Instruction::Damage { side, slot, amount: dmg });
            }
        }

        // Ingrain heals the rooted mon 1/16 max HP (PS residual order 7, before Leech Seed).
        let p = b.state.side(side).active();
        if p.is_alive()
            && b.state.side(side).volatiles.contains(VolatileStatus::Ingrain)
            && p.hp < p.max_hp
            && !heal_blocked(b, side)
        {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
        }

    }
    // Leech Seed (PS residual order 8): runs for BOTH actives before any status chip (order
    // 9) — PS's residual queue is globally ordered, so the drain's heal must see the healer
    // at its pre-burn HP (cosim caught the per-side interleaving healing a burn chip away).
    // PS also skips the residual entirely — drain AND heal — when the seeding slot's
    // occupant has fainted and not yet been replaced (`getAtSlot(sourceSlot)` -> fainted).
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let (alive, hp, maxhp, magic_guard) = {
            let p = b.state.side(side).active();
            (p.is_alive(), p.hp, p.max_hp, p.ability == crate::ids::Ability::MagicGuard)
        };
        if !alive || magic_guard || !b.state.side(side).volatiles.contains(VolatileStatus::LeechSeed) {
            continue;
        }
        let other = side.other();
        if !b.state.side(other).active().is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let drain = (maxhp / 8).max(1).min(hp);
        push(b, Instruction::Damage { side, slot, amount: drain });
        let (f_room, fslot) = {
            let f = b.state.side(other).active();
            (f.max_hp - f.hp, b.state.side(other).active_index)
        };
        // Liquid Ooze on the SEEDED mon turns the seeder's payout into damage — `leechseed` is
        // on its `canOoze` list (`data/abilities.ts:2360`) alongside `drain` and `strengthsap`.
        // Heal Block still suppresses the heal; it does not suppress the ooze, which is a
        // `this.damage` inside `onSourceTryHeal` and never reaches `heal`'s Heal-Block gate.
        if b.state.side(side).active().ability == crate::ids::Ability::LiquidOoze {
            let f_hp = b.state.side(other).active().hp;
            let dmg = drain.min(f_hp);
            if dmg > 0 {
                push(b, Instruction::Damage { side: other, slot: fslot, amount: dmg });
            }
        } else if !heal_blocked(b, other) {
            let heal = drain.min(f_room);
            if heal > 0 {
                push(b, Instruction::Heal { side: other, slot: fslot, amount: heal });
            }
        }
    }
    // --- residual order 9 (psn / tox) then order 10 (brn) ---
    // `data/conditions.ts` gives `psn` and `tox` `onResidualOrder: 9` (:133, :154) and `brn`
    // `onResidualOrder: 10` (:15). They are DIFFERENT orders, so PS's single globally ordered
    // queue runs EVERY poison chip before ANY burn chip, whichever side holds them; within an
    // order the two actives are `speedSort`ed, fastest first. The engine used to fold both into
    // one per-side loop that always ran side One first, putting a side-One burn ahead of a
    // side-Two poison. Invisible while both holders survive — the chips touch different HP bars —
    // and decisive when one of them ENDS the battle, because PS's residual then returns and the
    // later chip never happens (rb1279 d48: p2's Ho-Oh is `tox` at order 9 and p1's mon is `brn`
    // at order 10; PS ticks the toxic first, then the burn KOs p1's last mon and the pass stops).
    for order in [9u8, 10u8] {
        for side in residual_side_order(&b.state) {
            if battle_over(&b.state) { ended_early = true; break 'residual; }
            let p = b.state.side(side).active();
            if !p.is_alive() {
                continue;
            }
            let slot = b.state.side(side).active_index;
            let maxhp = p.max_hp;
            let php = p.hp;
            use crate::ids::Ability as Ab;
            let ability = p.ability;
            let magic_guard = ability == Ab::MagicGuard;
            let pstatus = p.status;
            let this_order = match pstatus {
                Status::Poison | Status::Toxic => 9u8,
                Status::Burn => 10u8,
                _ => continue,
            };
            if this_order != order {
                continue;
            }
            // Poison Heal *heals* 1/8 instead of taking poison/toxic damage; Magic Guard cancels
            // the damage entirely.
            if ability == Ab::PoisonHeal {
                let heal = (maxhp / 8).max(1).min(maxhp - php);
                if heal > 0 && !heal_blocked(b, side) {
                    push(b, Instruction::Heal { side, slot, amount: heal });
                }
                // Toxic still advances its counter even under Poison Heal — and still STOPS at
                // 15 (`data/conditions.ts:156`, `if (this.effectState.stage < 15)`).
                if pstatus == Status::Toxic {
                    let cur = b.state.side(side).active().status_counter;
                    if cur < TOXIC_MAX_STAGE {
                        push(b, Instruction::ChangeStatusCounter { side, slot, previous: cur, new: cur + 1 });
                    }
                }
            } else if !magic_guard {
                match pstatus {
                    Status::Burn | Status::Poison => {
                        // Heatproof halves burn chip (1/32 — PS `onDamage`).
                        let frac = if pstatus == Status::Burn {
                            if ability == Ab::Heatproof { 32 } else { 16 }
                        } else { 8 };
                        let dmg = (maxhp / frac).max(1).min(php);
                        push(b, Instruction::Damage { side, slot, amount: dmg });
                    }
                    Status::Toxic => {
                        // **PS CAPS THE STAGE AT 15**: `onResidual` is
                        // `if (this.effectState.stage < 15) this.effectState.stage++;` and only
                        // THEN multiplies (`data/conditions.ts:155-160`). A 16th badly-poisoned
                        // turn therefore deals 15/16, not 16/16, and the counter stops moving.
                        // rb5133 d34 and rb5255 d49: `status_counter` engine 16 / PS 15.
                        let cur = b.state.side(side).active().status_counter;
                        let stage = (cur + 1).min(TOXIC_MAX_STAGE) as i16;
                        let dmg = ((maxhp / 16) * stage).max(1).min(b.state.side(side).active().hp);
                        push(b, Instruction::Damage { side, slot, amount: dmg });
                        if cur != stage as u8 {
                            push(b, Instruction::ChangeStatusCounter { side, slot, previous: cur, new: stage as u8 });
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let maxhp = p.max_hp;
        use crate::ids::Ability as Ab;
        let ability = p.ability;
        let magic_guard = ability == Ab::MagicGuard;

        // Curse (Ghost, `onResidualOrder: 12`, data/moves.ts:3298): the cursed mon loses 1/4 max
        // HP each turn — AHEAD of Salt Cure / the partial trap (13) and Octolock (14), so a
        // Curse KO cancels those.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::Curse) {
            let dmg = (maxhp / 4).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }

        // Salt Cure.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::SaltCure) {
            let heavy = p.types.contains(&Type::Water) || p.types.contains(&Type::Steel);
            let frac = if heavy { 4 } else { 8 };
            let dmg = (maxhp / frac).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }

        // Partially-trapped (Fire Spin / Bind / Whirlpool / Magma Storm / Infestation / …). PS
        // (battle.ts residual loop) decrements the `duration` FIRST; if it hits 0 the trap ends and
        // its onResidual is skipped (no chip). Otherwise the onResidual ends it with no chip if the
        // trapper is no longer active (fainted here; the switch-out case is cleared eagerly on
        // switch), else deals 1/divisor damage (6 with Binding Band, else 8). Magic Guard blocks
        // only the chip, not the countdown.
        if b.state.side(side).volatiles.contains(VolatileStatus::PartiallyTrapped) {
            let turns = b.state.side(side).partial_trap_turns;
            let div = b.state.side(side).partial_trap_div.max(1);
            let new_turns = turns.saturating_sub(1);
            let trapper_gone = !b.state.side(side.other()).active().is_alive();
            if new_turns == 0 || trapper_gone {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::PartiallyTrapped });
                push(b, Instruction::SetPartialTrap { side, previous: (turns, div), new: (0, 0) });
            } else {
                push(b, Instruction::SetPartialTrap { side, previous: (turns, div), new: (new_turns, div) });
                let p = b.state.side(side).active();
                if p.is_alive() && !magic_guard {
                    let dmg = (maxhp / div as i16).max(1).min(p.hp);
                    push(b, Instruction::Damage { side, slot, amount: dmg });
                }
            }
        }

        // Octolock (PS residual order 14, after partiallytrapped): ends silently if the trapper
        // is gone (switch-out case is cleared eagerly), else lowers the victim's Def and SpD by 1
        // (a foe-sourced drop: Clear Body blocks it, Defiant/Competitive react to it).
        if b.state.side(side).volatiles.contains(VolatileStatus::Octolock) {
            if !b.state.side(side.other()).active().is_alive() {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Octolock });
            } else if b.state.side(side).active().is_alive() {
                for stat in [BoostIndex::Defense, BoostIndex::SpecialDefense] {
                    // One `AfterEachBoost` per actually-changed stat (sim/battle.ts:2073).
                    if apply_boost_clamped(b, side, stat, -1) < 0 {
                        react_to_stat_drop(b, side);
                        apply_white_herb(b, side);
                    }
                }
            }
        }

        // NOTE: the pinch-berry check does NOT belong here. A berry's trigger is an `onUpdate`
        // handler, and PS runs no `eachEvent('Update')` anywhere inside the residual action —
        // the first one is `runAction`'s trailing Update (`sim/battle.ts:2882`), AFTER the whole
        // residual queue. It is applied there, at the end of this function.
    }

    // Active-mon countdowns: Taunt / Encore / Disable tick and clear; Yawn ticks and puts the
    // holder to sleep at 0; Perish Song ticks and faints the holder at 0.
    use crate::instruction::ActiveCounter;
    yawn_fired = [false; 2];
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        if !b.state.side(side).active().is_alive() {
            continue;
        }
        let t = b.state.side(side).taunt_turns;
        if t > 0 {
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::Taunt, previous: t, new: t - 1 });
            if t == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Taunt });
            }
        }
        let tc = b.state.side(side).throat_chop_turns;
        if tc > 0 {
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::ThroatChop, previous: tc, new: tc - 1 });
            if tc == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::ThroatChop });
            }
        }
        let hb = b.state.side(side).heal_block_turns;
        if hb > 0 {
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::HealBlock, previous: hb, new: hb - 1 });
            if hb == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::HealBlock });
            }
        }
        // Magnet Rise: `duration: 5`, `onResidualOrder: 18` (`data/moves.ts` magnetrise).
        let mr = b.state.side(side).magnet_rise_turns;
        if mr > 0 {
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::MagnetRise, previous: mr, new: mr - 1 });
            if mr == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::MagnetRise });
            }
        }
        let enc = b.state.side(side).encore;
        if enc.1 > 0 {
            let ends = enc.1 == 1
                // Encore also ends when the encored move runs out of PP.
                || !b.state.side(side).active().moves.iter().any(|m| m.id == enc.0 && m.pp > 0);
            let new = if ends { (crate::ids::MoveId::None, 0) } else { (enc.0, enc.1 - 1) };
            push(b, Instruction::SetEncore { side, previous: enc, new });
            if ends {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Encore });
            }
        }
        let dis = b.state.side(side).disable;
        if dis.1 > 0 {
            let new = if dis.1 == 1 { (crate::ids::MoveId::None, 0) } else { (dis.0, dis.1 - 1) };
            push(b, Instruction::SetDisable { side, previous: dis, new });
            if dis.1 == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Disable });
            }
        }
        let y = b.state.side(side).yawn_turns;
        if y > 0 {
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::Yawn, previous: y, new: y - 1 });
            if y == 1 {
                push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Yawn });
                yawn_fired[side.index()] = true;
            }
        }
        if b.state.side(side).volatiles.contains(VolatileStatus::PerishSong) {
            let pt = b.state.side(side).perish_turns;
            let new = pt.saturating_sub(1);
            push(b, Instruction::SetActiveCounter { side, which: ActiveCounter::Perish, previous: pt, new });
            if new == 0 {
                let p = b.state.side(side).active();
                let slot = b.state.side(side).active_index;
                let hp = p.hp;
                if hp > 0 {
                    push(b, Instruction::Damage { side, slot, amount: hp });
                }
            }
        }
    }

    // --- residual order 25: Roost wears off, restoring the user's pre-Roost typing. (In the
    // modeled scope, types only change via Roost and Tera, so base types — or the tera type —
    // are exact.) `data/moves.ts` roost's volatile is `onResidualOrder: 25`, so it lands after
    // the Perish Song faint (24) and before the screens (26).
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {
            restore_roost_typing_side(b, side);
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Roosted });
        }
    }

    // Safety net for the linked `trapped` release: a trap source that died to a RESIDUAL (burn,
    // its own partial trap, …) rather than a hit also frees the foe before the decision boundary.
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        if b.state.side(side).volatiles.contains(VolatileStatus::Trapped)
            && !b.state.side(side.other()).active().is_alive()
        {
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Trapped });
        }
    }

    // The Protect stall chain expires at end of turn unless it was refreshed by a successful
    // Protect-family use THIS turn (PS `stall` volatile: duration 2, `onRestart` re-arms it; a
    // turn where the holder did anything else — or never acted at all: full paralysis, sleep,
    // flinch — lets the duration run out). The Protect volatile is exactly the this-turn marker.
    // Then clear the single-turn volatiles themselves (PS removes duration-1 volatiles in the
    // same residual pass).
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let stall = b.state.side(side).stall_counter;
        if stall != 0 && !b.state.side(side).volatiles.contains(VolatileStatus::Protect) {
            push(b, Instruction::SetStallCounter { side, previous: stall, new: 0 });
        }
        // The `stall` volatile's duration ticks down every end of turn (a successful Protect this
        // turn already re-armed it to 2 above). It stays in the Residual handler list until it
        // reaches 0 — one turn PAST the Protect — matching PS's duration-2 lifetime. The residual
        // shuffle was already emitted (top of this fn) off the pre-tick count, so this tick only
        // affects NEXT turn's list length; state-neutral otherwise.
        let st = b.state.side(side).stall_turns;
        if st != 0 {
            push(b, Instruction::SetStallTurns { side, previous: st, new: st - 1 });
        }
        for v in [VolatileStatus::Protect, VolatileStatus::Endure, VolatileStatus::Flinch] {
            if b.state.side(side).volatiles.contains(v) {
                push(b, Instruction::RemoveVolatile { side, volatile: v });
            }
        }
    }

    // Screens / Tailwind tick per side and clear at 0.
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        use SideConditionId::*;
        let conds = b.state.side(side).side_conditions;
        for (sc, cur) in [
            (Reflect, conds.reflect),
            (LightScreen, conds.light_screen),
            (AuroraVeil, conds.aurora_veil),
            (Tailwind, conds.tailwind),
        ] {
            if cur > 0 {
                push(b, Instruction::SetSideCondition { side, condition: sc, previous: cur, new: cur - 1 });
            }
        }
    }

    // --- duration ticking (cosim caught all of these as permanently-stuck effects) ---
    // Terrain / Trick Room count down at end of turn and expire at 0. (Weather is ticked BEFORE
    // the residual loop above so sandstorm doesn't chip on its final turn; terrain is ticked here,
    // AFTER, so Grassy Terrain still heals on its final turn — its heal is a separate per-mon
    // residual handler, not skipped by the field-duration decrement.)
    if b.state.terrain != crate::ids::Terrain::None && b.state.terrain_turns > 0 {
        push(b, Instruction::DecrementTerrainTurns);
        if b.state.terrain_turns == 0 {
            emit_field_change_shuffle(b); // clearTerrain -> eachEvent('TerrainChange')
            push(b, Instruction::ChangeTerrain {
                previous: b.state.terrain,
                previous_turns: 0,
                new: crate::ids::Terrain::None,
                new_turns: 0,
            });
            refresh_proto_quark(b); // PS Quark Drive `onTerrainChange`
        }
    }
    if b.state.trick_room && b.state.trick_room_turns > 0 {
        push(b, Instruction::DecrementTrickRoomTurns);
        if b.state.trick_room_turns == 0 {
            push(b, Instruction::ToggleTrickRoom { previous_turns: 0, new_turns: 0 });
        }
    }
    // --- residual order 28, subOrder 2: Bad Dreams / Cud Chew / Speed Boost ---
    // All three are `onResidualOrder: 28, onResidualSubOrder: 2` (`data/abilities.ts:310`,
    // `:2657`, `:4408`), so they run AFTER every counter (Taunt 15 … Perish Song 24), after the
    // screens (26) and after the terrain / Trick Room duration (27) — and BEFORE the status orbs
    // at 28/3. The engine used to run them inside the order-9 status loop, which (a) let Bad
    // Dreams miss a foe that Yawn had only just put to sleep at order 23 and (b) gave Speed Boost
    // to a mon Perish Song had already fainted at order 24.
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        use crate::ids::Ability as Ab;
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let (ability, slot) = (p.ability, b.state.side(side).active_index);

        // Bad Dreams (Darkrai): each sleeping foe loses 1/8 max HP. Comatose counts as asleep;
        // Magic Guard on the sleeper prevents the damage.
        if ability == Ab::BadDreams {
            let foe = side.other();
            let f = b.state.side(foe).active();
            if f.is_alive()
                && (f.status == Status::Sleep || f.ability == Ab::Comatose)
                && f.ability != Ab::MagicGuard
            {
                let fslot = b.state.side(foe).active_index;
                let dmg = (f.max_hp / 8).max(1).min(f.hp);
                push(b, Instruction::Damage { side: foe, slot: fslot, amount: dmg });
                // The chip can put the sleeper into its own pinch-berry range.
                apply_pinch_berry(b, foe);
            }
        }

        // Cud Chew: re-apply the eaten berry's effect. The counter was set to 2 on eat and ticks
        // down here; at 0 the berry effect fires again.
        let cc = b.state.side(side).active().cudchew_turns;
        if cc > 0 {
            push(b, Instruction::SetCudChew { side, slot, previous: cc, new: cc - 1 });
            if cc - 1 == 0 && b.state.side(side).active().is_alive() {
                let berry = b.state.side(side).active().last_berry;
                apply_berry_eat_effect(b, side, berry);
            }
        }

        // Speed Boost: +1 Spe at end of turn, but not the turn the mon entered. PS's gate is
        // `if (pokemon.activeTurns)` (`data/abilities.ts:4412`) — a counter reset to 0 by
        // `switchIn` (`sim/battle-actions.ts:137`) and bumped in `nextTurn`, so it is 0 for EVERY
        // way of entering: a chosen switch, a pivot, a faint replacement AND a DRAG. The engine
        // read a `switched` flag built from the two sides' chosen actions, which misses the drag
        // (rb1239 d34: p1's Roar drags a Speed Boost mon in and the engine handed it +1 Spe on
        // the spot). The `switched` parameter is now vestigial — its plumbing through `request.rs`
        // can be removed the next time that file is touched.
        if ability == Ab::SpeedBoost
            && b.state.side(side).active_turns != 0
            && b.state.side(side).active().is_alive()
        {
            raise_boost(b, side, BoostIndex::Speed, 1);
        }
    }

    // --- residual order 28, subOrder 3: the status orbs ---
    // Toxic Orb / Flame Orb status the holder at end of turn if it has no status yet (the chip
    // starts next turn). `data/items.ts` gives them `onResidualOrder: 28, onResidualSubOrder: 3`,
    // i.e. after the 28/2 abilities above. (Harvest is also 28/2 but emits a draw, so it lives in
    // the branching tail below; it cannot interact with an orb's status.)
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let orb_status = match p.item {
            Item::ToxicOrb => Status::Toxic,
            Item::FlameOrb => Status::Burn,
            _ => Status::None,
        };
        if orb_status != Status::None && status_applies(p, orb_status) {
            push(b, Instruction::ChangeStatus { side, slot, previous: Status::None, new: orb_status });
        }
    }

    // --- residual order 29: Hunger Switch (Morpeko) ---
    // The forme toggles every end of turn (`data/abilities.ts` `onResidualOrder: 29`) unless
    // Terastallized. Both formes share stats and typing, so this is exactly a species-id swap;
    // Aura Wheel reads the forme at use time. The engine used to toggle it at the TOP of the
    // residual pass, ahead of every order-1..28 handler.
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let p = b.state.side(side).active();
        if p.ability == crate::ids::Ability::HungerSwitch && p.is_alive() && !p.terastallized && !p.transformed {
            let plain = crate::ids::Species::from_id("morpeko").unwrap_or(crate::ids::Species::None);
            let hangry = crate::ids::Species::from_id("morpekohangry").unwrap_or(crate::ids::Species::None);
            let target_forme = if p.species == plain {
                hangry
            } else if p.species == hangry {
                plain
            } else {
                continue;
            };
            let previous = transform_data_of(&b.state, side);
            let mut new = previous;
            new.species = target_forme;
            let slot = b.state.side(side).active_index;
            let previous_base_moves = b.state.side(side).active().base_moves;
            push(b, Instruction::Transform { side, slot, previous, new, previous_base_moves });
        }
    }

    // Future Sight: tick; mark a strike when it lands (stochastic rolls -> branch below).
    fs_fired = [None, None];
    for side in [SideId::One, SideId::Two] {
        if battle_over(&b.state) { ended_early = true; break 'residual; }
        let fs = b.state.side(side).future_sight;
        if fs.0 > 0 {
            let lands = fs.0 == 1;
            let new = if lands { (0, 0) } else { (fs.0 - 1, fs.1) };
            push(b, Instruction::SetFutureSight { side, previous: fs, new });
            if lands && b.state.side(side).active().is_alive() {
                fs_fired[side.index()] = Some(fs.1);
            }
        }
    }

    // Harvest: 50% chance (always in sun) to regrow the holder's eaten berry.
    } // 'residual

    let mut out_h = vec![branch];
    if ended_early {
        // PS returned out of `fieldEvent('Residual')` the instant the battle ended, so NO later
        // residual handler runs (Harvest, Shed Skin, the Yawn expiry, the Future Sight strike),
        // `runAction`'s trailing `eachEvent('Update')` never fires, and `turnLoop` returns before
        // `endTurn()` — so its per-active bookkeeping (the `activeTurns` bump, the DisableMove /
        // TrapPokemon speed sorts) never happens either. Hand the aborted state back as-is —
        // except for Roost's typing, which is an ENCODING artifact, not PS state: PS never
        // mutates `pokemon.types` for Roost (the `roost` volatile only filters Flying out of
        // `getTypes()` via `onType`, data/moves.ts:15459-15463), while the engine strips the
        // type and puts it back at order 25. An abort before order 25 would therefore leave the
        // engine's stripped encoding facing PS's untouched `[Steel, Flying]` (rb1180 d42). The
        // `Roosted` bit itself is already masked out of the state digest for the same reason.
        for nb in &mut out_h {
            restore_roost_typing(nb);
        }
        return out_h;
    }
    // Yawn expiry: the drowsy mon falls asleep now (stochastic 1-3 turn duration).
    //
    // **`onResidualOrder: 23` (`data/moves.ts` yawn `condition`), and Harvest is 28** — so the
    // `random(2, 5)` sleep duration is drawn BEFORE Harvest's `randomChance(1, 2)`. This block
    // used to sit in the branching tail after Harvest, five orders late; rb5162 d36 t30 is the
    // witness (an Exeggutor with Harvest + Sitrus is Yawned to sleep: PS's pair is
    // `random[2,5]=3@slp` then `randomChance[1,2]=False@harvest`, the engine's was the reverse).
    // Same mistake, same tail, as the Shed Skin note directly below.
    for (i, fired) in yawn_fired.into_iter().enumerate() {
        if !fired {
            continue;
        }
        let side = if i == 0 { SideId::One } else { SideId::Two };
        out_h = out_h
            .into_iter()
            .flat_map(|mut x| {
                let p = x.state.side(side).active();
                let pre_clause = p.is_alive()
                    && p.status == Status::None
                    && status_applies(p, Status::Sleep)
                    && !status_blocked_by_field(&x.state, side, Status::Sleep);
                if pre_clause && sleep_clause_blocks(&x.state, side) {
                    push(&mut x, Instruction::SleepClauseBlocked { side });
                    return vec![x];
                }
                if pre_clause {
                    let slot = x.state.side(side).active_index;
                    push(&mut x, Instruction::ChangeStatus { side, slot, previous: Status::None, new: Status::Sleep });
                    mark_slept_by_foe(&mut x, side);
                    consume_lum_if_statused(&mut x, side);
                    if sleep_survived_or_discard_duration(&mut x, side, true) {
                        branch_sleep_counter(x, side)
                    } else {
                        vec![x]
                    }
                } else {
                    vec![x]
                }
            })
            .collect();
    }
    // Speed order, not side order: two Harvest holders make two consecutive, IDENTICALLY-SHAPED
    // `randomChance[1,2]` draws, so the differ cannot tell them apart but the SELECTOR must —
    // the seed gate hands the first recorded result to whichever holder the engine rolls first.
    // The residual handler list is `speedSort`ed, so the FASTER holder rolls first. rb5073 d51:
    // a Tropius (base Spe 51, berry still held, roll discarded) and an Exeggutor-Alola (base 45,
    // berry eaten) both carry Harvest+Sitrus; PS's pair is `False, True`, and rolling side-One
    // first gave the Exeggutor the False, so its Sitrus never regrew and never healed the 78 HP
    // that `hitStepMoveHitLoop`'s Update would have eaten it for.
    for side in residual_side_order(&out_h[0].state) {
        out_h = out_h
            .into_iter()
            .flat_map(|b| {
                let p = b.state.side(side).active();
                // PS Harvest `onResidual` (order 28) runs for EVERY living Harvest holder each
                // end of turn: `if (sun || randomChance(1,2)) { if (!item && lastItem is berry)
                // restore }`. So the `randomChance(1,2)` is rolled whenever it's not sunny —
                // independent of whether a berry can actually be restored (the restore short-
                // circuits inside). Only the state change is conditional on a consumed berry.
                if p.ability != crate::ids::Ability::Harvest || !p.is_alive() {
                    return vec![b];
                }
                let slot = b.state.side(side).active_index;
                let berry = p.last_berry;
                let can_restore = p.item == Item::None && berry != Item::None;
                let sunny = matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun);
                if sunny {
                    // Sun short-circuits the roll (no draw); restore is guaranteed if possible.
                    if !can_restore {
                        return vec![b];
                    }
                    let mut grow = b;
                    push(&mut grow, Instruction::ChangeItem { side, slot, previous: Item::None, new: berry });
                    push(&mut grow, Instruction::SetLastBerry { side, slot, previous: berry, new: Item::None });
                    maybe_eat_sitrus(&mut grow, side);
                    return vec![grow];
                }
                // Not sunny: the roll always fires. When no berry can be restored both outcomes
                // are identical, so emit a single draw-and-discard branch; otherwise split 50/50.
                if !can_restore {
                    let mut nb = b;
                    draw(&mut nb, "randomChance", &[1, 2], 0, "harvest");
                    return vec![nb];
                }
                let mut grow = scaled(&b, 0.5);
                draw(&mut grow, "randomChance", &[1, 2], 1, "harvest");
                push(&mut grow, Instruction::ChangeItem { side, slot, previous: Item::None, new: berry });
                push(&mut grow, Instruction::SetLastBerry { side, slot, previous: berry, new: Item::None });
                // Restoring a berry runs PS's item Update event immediately.  A Harvested
                // Sitrus is therefore eaten in the same residual event when HP is already
                // at or below half (it does not wait for the next damage/end-turn check).
                maybe_eat_sitrus(&mut grow, side);
                let mut nogrow = scaled(&b, 0.5);
                draw(&mut nogrow, "randomChance", &[1, 2], 0, "harvest");
                vec![grow, nogrow]
            })
            .collect();
    }
    // Shields Down (Minior) re-picks its forme at `onResidualOrder: 29` — after Harvest (28).
    // State-only, no draw.
    let branches_after_harvest = out_h
        .into_iter()
        .map(|mut b| {
            for side in [SideId::One, SideId::Two] {
                shields_down_forme(&mut b, side);
            }
            b
        })
        .collect::<Vec<_>>();

    // Shed Skin used to run HERE, in the branching tail after Harvest (28) — ten orders late, so a
    // cured holder still took the order-9/10 status chip. It now runs at its real slot (5/3) inside
    // the deterministic core; `apply_end_of_turn`'s wrapper hoists the 33% split.
    let out = branches_after_harvest;

    let mut out = out;
    // --- residual order `false`: the duration-only handlers, which PS sorts LAST ---
    //
    // The rampage (`lockedmove`) expiry lives here, in the branching tail, for two reasons that
    // are both PS facts rather than engine convenience: it sorts after every numbered handler
    // (Yawn 23, Harvest 28), and it can CONFUSE, which forks.
    let mut out = out
        .into_iter()
        .flat_map(|mut b| {
            for side in [SideId::One, SideId::Two] {
                let crate::state::PendingMove::Rampaging(m, n) = b.state.side(side).pending_move
                else {
                    continue;
                };
                // **A SLEEPING rampager drops the lock, and is not confused for it.**
                // `lockedmove.onResidual` opens with
                //     if (target.status === 'slp') { delete target.volatiles['lockedmove']; }
                // — a `delete` on the volatile, not an `end()`, so `onEnd` never runs, and PS's
                // own comment says why: "don't lock, and bypass confusion for calming".
                //
                // rb5160 d42 t35: an Outrage user is Yawned to sleep on the very turn it starts
                // the rampage (`random[2,4]=3@lockedmove` and `random[2,5]=2@slp` in one unit).
                // The sleep lands at Yawn's order 23; the lock expiry reads it at order-last. The
                // engine used to run this whole block in the DETERMINISTIC core, ahead of the
                // Yawn sleep that the tail applies — so it saw a statusless mon and ticked 3 -> 2.
                if b.state.side(side).active().status == Status::Sleep {
                    push(&mut b, Instruction::SetPendingMove {
                        side,
                        previous: crate::state::PendingMove::Rampaging(m, n),
                        new: crate::state::PendingMove::None,
                    });
                    if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
                    }
                    continue;
                }
                if n >= 2 {
                    // The move action stored the mid-turn (kernel) value; this ticks it down.
                    push(&mut b, Instruction::SetPendingMove {
                        side,
                        previous: crate::state::PendingMove::Rampaging(m, n),
                        new: crate::state::PendingMove::Rampaging(m, n - 1),
                    });
                    continue;
                }
                // n == 1 AND the lock is still armed: the mon did NOT use the move this turn (a
                // use releases it at move time), so this is PS's `duration` reaching 0 with the
                // volatile still up — `onEnd` fires here. See `residual_rampage_confuses`.
                push(&mut b, Instruction::SetPendingMove {
                    side,
                    previous: crate::state::PendingMove::Rampaging(m, n),
                    new: crate::state::PendingMove::None,
                });
                if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                    push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
                }
                if let Some(t) = conf_dur[side.index()] {
                    let prev = b.state.side(side).confusion_turns;
                    draw(&mut b, "random", &[2, 6], t as i64, "confusion");
                    push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Confusion });
                    push(&mut b, Instruction::SetActiveCounter {
                        side,
                        which: crate::instruction::ActiveCounter::Confusion,
                        previous: prev,
                        new: t,
                    });
                    consume_lum_if_statused(&mut b, side);
                }
            }
            vec![b]
        })
        .collect::<Vec<_>>();

    // Future Sight strikes: 16 damage rolls, each its own branch.
    for (i, fired) in fs_fired.into_iter().enumerate() {
        let Some(caster_slot) = fired else { continue };
        let side = if i == 0 { SideId::One } else { SideId::Two };
        out = out
            .into_iter()
            .flat_map(|x| {
                let target = x.state.side(side).active();
                if !target.is_alive() {
                    return vec![x];
                }
                // The delayed strike still runs a full damage calc: a type/ability-immune target
                // (Psychic Future Sight vs a Dark mon, Steel Doom Desire vs an immune ability) makes
                // it fail — no accuracy/crit/damage draws and no damage (c2b3 t8: Future Sight vs
                // Dark Iron Jugulis). Gate the whole strike on connecting.
                let (fs_type, _) = {
                    let caster = &x.state.side(side.other()).pokemon[(caster_slot as usize).min(5)];
                    let doom = caster.moves.iter().any(|m| m.id.to_id() == "doomdesire");
                    if doom { (Type::Steel, 140u16) } else { (Type::Psychic, 120u16) }
                };
                let connects = crate::damage::type_multiplier(fs_type, target.types) != 0.0
                    && !ability_immune(fs_type, target.ability);
                if !connects {
                    return vec![x];
                }
                let hp = target.hp;
                let slot = x.state.side(side).active_index;
                // Apply the 16 damage rolls (with the given crit flag) as sibling branches.
                let strike = |base: &Branch, is_crit: bool| -> Vec<Branch> {
                    let rolls = future_sight_rolls_crit(&base.state, side, caster_slot, is_crit);
                    rolls
                        .into_iter()
                        .enumerate()
                        .map(|(roll, r)| {
                            let mut nb = scaled(base, 1.0 / 16.0);
                            // PS rolls the damage `random(16)` for the delayed strike. `damage_rolls`
                            // is indexed BY the drawn roll (index 0 = factor 100/100), so annotate the
                            // branch with its roll index — otherwise Replicate cannot filter the
                            // 16-way fan-out and every roll survives as an "ambiguous fork", picking
                            // the highest-probability branch instead of the realized one (c2a1 t8:
                            // PS `random[16]=8@futuresight`, engine damage 7 HP short).
                            if annotating() {
                                draw(&mut nb, "random", &[16], roll as i64, "futuremove");
                            }
                            let dmg = r.min(hp).max(0);
                            if dmg > 0 {
                                push(&mut nb, Instruction::Damage { side, slot, amount: dmg });
                                // The delayed strike counts as a hit for Rage Fist (PS `timesAttacked`).
                                let cur = nb.state.side(side).active().times_hit;
                                let new = cur.saturating_add(1).min(250);
                                push(&mut nb, Instruction::SetTimesHit { side, slot, previous: cur, new });
                            }
                            nb
                        })
                        .collect::<Vec<_>>()
                };
                if annotating() {
                    // PS resolves the delayed strike as a full move at end of turn: accuracy (always
                    // 100 — the strike bypasses invulnerability but PS still logs `randomChance(100,
                    // 100)`), then the crit `randomChance(1,24)`, then the damage `random(16)`
                    // (battle-actions.ts getDamage). Emit the realized stream so the differ / seed
                    // gate match (c2a1 t8/t21). Enumerate/Sample keep the crit-free 16-way fold below.
                    let mut x = x;
                    draw(&mut x, "randomChance", &[100, 100], 1, "accuracy");
                    let mut branches = Vec::new();
                    let mut nc = scaled(&x, 23.0 / 24.0);
                    draw(&mut nc, "randomChance", &[1, 24], 0, "futuremove");
                    branches.extend(strike(&nc, false));
                    let mut cr = scaled(&x, 1.0 / 24.0);
                    draw(&mut cr, "randomChance", &[1, 24], 1, "futuremove");
                    branches.extend(strike(&cr, true));
                    branches
                } else {
                    strike(&x, false)
                }
            })
            .collect();
    }
    // `runAction`'s trailing `eachEvent('Update')` (`sim/battle.ts:2882`) — the STATE half. Every
    // berry trigger is an `onUpdate` handler and the residual action fires no Update of its own,
    // so a Sitrus that end-of-turn chip brought into range is eaten HERE, after the whole residual
    // queue, not at the chip.
    //
    // rb1030 d67 t59 is the witness and it is only visible because of **Harvest**, which sits at
    // residual order 28/2 — inside the queue. PS: the Arboliva is still holding its Sitrus when
    // Harvest rolls, so `!pokemon.item` is false and nothing is regrown; the trailing Update then
    // eats the berry, leaving `item: None` / `lastItem: Sitrus`. The engine ate the berry at the
    // chip (ten-plus orders early), so Harvest found an empty hand, rolled its 50%, and put the
    // berry straight back.
    //
    // Runs before the `activeTurns` bump below, matching PS: the 2882 Update is part of the
    // residual ACTION, while `nextTurn` is the later `endTurn`.
    for nb in &mut out {
        if battle_over(&nb.state) {
            continue;
        }
        run_update_event(nb);
    }

    // Advance the active mon's turn counter (Fake Out / First Impression / Slow Start / Stakeout).
    // PS does this in `nextTurn()` (battle.ts:1762) — NOT in the residual phase — which is reached
    // only after the whole turn survives to `endTurn()`. Two consequences the residual-phase
    // placement got wrong:
    //   * a faint that ENDS the battle stops the turn where it happens (`runAction` returns on
    //     `this.ended` right after `faintMessages()`, battle.ts:2857), so `nextTurn` never runs and
    //     NO active is advanced — including the winner's;
    //   * `nextTurn` skips a fainted active (`if (pokemon.fainted) continue`, battle.ts:1756). A
    //     mon that faints to a residual is replaced first (`checkFainted` → `makeRequest('switch')`
    //     at battle.ts:2864/2933 returns before `endTurn`), and its replacement enters at
    //     `activeTurns = 0` (battle-actions.ts:137) to be advanced by the `nextTurn` that follows.
    // Caps so it can't overflow in a long stall.
    for nb in &mut out {
        if battle_over(&nb.state) {
            continue;
        }
        for side in [SideId::One, SideId::Two] {
            if !nb.state.side(side).active().is_alive() {
                continue;
            }
            let cur = nb.state.side(side).active_turns;
            if cur < 250 {
                push(nb, Instruction::SetActiveCounter {
                    side,
                    which: ActiveCounter::ActiveTurns,
                    previous: cur,
                    new: cur + 1,
                });
            }
        }
    }
    // runAction Update after the `residual` action completes (battle.ts:2882): one `shuffle[2,0,2]`
    // on a surviving equal-Speed pair. Emitted last, after every residual draw (incl. Future Sight).
    //
    // It sorts on `pre_residual_speeds` — the cache `case 'residual'`'s own `updateSpeed()` wrote
    // at :2835, before the first handler ran. LIVENESS is still read off the post-residual board
    // (`getAllActive` after `faintMessages`), which is exactly what `MOVE_TIE_SPEEDS` overrides.
    // Mirror-pair witnesses, both Regigigas games where Slow Start's counter expires this turn:
    // rb1369 d49 t45 (engine emitted a trailing `@update` shuffle PS does not — the engine had
    // already un-halved the Speed via the `activeTurns` bump above, tying the foe) and rb1310
    // d35 t28 (the exact mirror: PS records one extra shuffle after the residual sort, because its
    // cache still holds the HALVED Speed, which is the one that ties).
    if annotating() {
        for nb in &mut out {
            let prev_tie = MOVE_TIE_SPEEDS.with(|c| c.replace(Some(pre_residual_speeds)));
            emit_update(nb);
            MOVE_TIE_SPEEDS.with(|c| c.set(prev_tie));
            // getRequests' trap shuffle is a fresh sort outside the residual action — live Speed.
            // Then PS builds the next move request (`getRequests` → per-active TrapPokemon), whose
            // multi-trap tie shuffle is the turn's trailing draw.
            emit_trap_pokemon_shuffles(nb);
        }
    }
    out
}
