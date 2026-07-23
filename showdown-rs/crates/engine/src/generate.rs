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
}

thread_local! {
    static REALIZED_SOURCE: std::cell::RefCell<Option<RealizedSource>> =
        const { std::cell::RefCell::new(None) };
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
enum RealizedCursor {
    Prng(crate::psprng::PsPrng),
    Recorded { results: std::rc::Rc<Vec<i64>>, idx: usize },
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
        }
    }
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
    })
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
    let mut branches = vec![Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new() }];
    let custap = custap_stage(&mut branches, state, s1, s2);
    let mk = |side: SideId, idx: u8, cu: bool| Action {
        side, move_idx: idx, pivot: Pivot::Stay, shell_phys: None,
        foe_pending_move: None, custap: cu, external_move: None,
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

/// Whether `item` can be removed from a mon of `species` by Knock Off / Magician /
/// Pickpocket (PS `onTakeItem` returning false). Species-locked items can never be taken
/// from their signature holder: Rusted Sword/Shield (Zacian/Zamazenta), the Ogerpon masks,
/// and the Origin orbs/crystals (Dialga/Palkia/Giratina). Arceus plates are locked only to
/// Arceus, which is outside the randbats pool.
fn item_removable(species: crate::ids::Species, item: Item) -> bool {
    let sid = species.to_id();
    !match item {
        Item::RustedSword => sid.starts_with("zacian"),
        Item::RustedShield => sid.starts_with("zamazenta"),
        Item::HearthflameMask | Item::WellspringMask | Item::CornerstoneMask => sid.starts_with("ogerpon"),
        Item::AdamantCrystal | Item::AdamantOrb => sid.starts_with("dialga"),
        Item::LustrousGlobe | Item::LustrousOrb => sid.starts_with("palkia"),
        Item::GriseousCore | Item::GriseousOrb => sid.starts_with("giratina"),
        Item::None => true, // "removable" is meaningless without an item
        _ => false,
    }
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
                external_move: Some(move_id),
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
fn is_wind_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "aeroblast" | "aircutter" | "bleakwindstorm" | "blizzard" | "fairywind" | "gust"
            | "heatwave" | "hurricane" | "icywind" | "petalblizzard" | "sandsearstorm"
            | "springtidestorm" | "twister" | "wildboltstorm"
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
    let s = state.side(side);
    let p = s.active();
    let mut spe = p.stat(crate::ids::StatIndex::Speed) as f32;
    spe *= boost_multiplier(s.boost(BoostIndex::Speed));
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
    if has_proto(s) && proto_stat(p) == crate::ids::StatIndex::Speed {
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
    if p.ability == SlowStart && s.active_turns <= 5 {
        spe *= 0.5;
    }
    spe as i32
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
    live_ok && effective_speed(state, SideId::One) == effective_speed(state, SideId::Two)
}

/// Both actives alive and equal `effective_speed` (turn-order / commitChoices tie predicate).
fn actives_speed_tied(state: &State) -> bool {
    actives_update_tie(state, false)
}

/// Emit the post-action `eachEvent('Update')` shuffle (battle.ts:2882 runAction, post-residual,
/// switch/tera brackets, and post-hit-loop 1024) — fires iff both actives are alive and tied.
/// Annotation-only; state-neutral (PS logs the shuffle order as null, validated via `stateAfter`).
fn emit_update(b: &mut Branch) {
    if annotating() && actives_update_tie(&b.state, false) {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
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

/// Emit the per-hit `eachEvent('Update')` shuffle (battle-actions.ts:970) — fires once per
/// connecting hit, on the PRE-faint-message board (a target at 0 HP still counts as on-field).
fn emit_update_hit(b: &mut Branch) {
    if annotating() && actives_update_tie(&b.state, true) {
        draw(b, "shuffle", &[2, 0, 2], -1, "update");
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
    let mut k = 0i32;
    for side in [SideId::One, SideId::Two] {
        let sc = &b.state.side(side).side_conditions;
        k += (sc.reflect > 0) as i32 + (sc.light_screen > 0) as i32 + (sc.aurora_veil > 0) as i32;
    }
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
    for side in [SideId::One, SideId::Two] {
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
        external_move: None,
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
    // a length-(k+2) list → `shuffle[k+2, k, k+2]`. Each tera action also runs its own `runAction`
    // → an extra `eachEvent('Update')` shuffle (battle.ts:2882) on a Speed tie. (k=2 with two
    // equal-Speed teras also ties the teras themselves at [0,2); left unmodeled — vanishingly rare.)
    let k = (tera[0] && matches!(s1, MoveChoice::Move(_))) as i32
        + (tera[1] && matches!(s2, MoveChoice::Move(_))) as i32;
    if commit_tie {
        draw(b, "shuffle", &[2 + k, k, k + 2], -1, "update"); // 1. commitChoices sort (tera-shifted)
    }
    if speed_tie {
        draw(b, "shuffle", &[2, 0, 2], -1, "update"); // 2. eachEvent('BeforeTurn')
        draw(b, "shuffle", &[2, 0, 2], -1, "update"); // 3. runAction Update (after beforeTurn)
        for _ in 0..k {
            draw(b, "shuffle", &[2, 0, 2], -1, "update"); // 3b. runAction Update after each tera action
        }
    }
    if both_move && commit_tie {
        draw(b, "shuffle", &[3, 0, 2], -1, "update"); // 4. dynamic-speed re-sort (len-3 queue [move,move,residual])
    }
}

/// Whether the active Pokémon has a Protosynthesis / Quark Drive boost active.
fn has_proto(s: &crate::state::Side) -> bool {
    s.volatiles.contains(VolatileStatus::Protosynthesis) || s.volatiles.contains(VolatileStatus::QuarkDrive)
}

/// The stat Protosynthesis / Quark Drive boosts: the highest of atk/def/spa/spd/spe,
/// matching PS's `bestStat` (first max in that order).
fn proto_stat(p: &crate::state::Pokemon) -> crate::ids::StatIndex {
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
        ability: p.ability,
        moves: p.moves,
        transformed: p.transformed,
        times_hit: p.times_hit,
    }
}

/// Transform / Imposter: copy the foe's battle identity onto `side`'s active. Mirrors PS
/// `transformInto`: species/types/stats(except HP)/ability/boosts copied; each copied move
/// gets PP = min(5, base PP); crit volatiles (Focus Energy) copied. Fails against a
/// substitute, a transformed target, or when the user is already transformed.
fn apply_transform(b: &mut Branch, side: SideId) -> bool {
    let foe = side.other();
    let user_ok = b.state.side(side).active().is_alive() && !b.state.side(side).active().transformed;
    let target = b.state.side(foe).active();
    let target_ok = target.is_alive()
        && !target.transformed
        && !b.state.side(foe).volatiles.contains(VolatileStatus::Substitute);
    if !user_ok || !target_ok {
        return false;
    }
    let previous = transform_data_of(&b.state, side);
    let mut new = transform_data_of(&b.state, foe);
    new.stats[0] = previous.stats[0]; // HP is never copied
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
    md.status != Status::None
        || md.target_boosts.iter().any(|&x| x != 0)
        || md.target_volatile.is_some()
        || md.force_switch
        || matches!(md.id.to_id(), "partingshot" | "trick" | "switcheroo" | "encore" | "disable" | "taunt" | "whirlwind" | "roar" | "defog")
}

/// Sleep Clause Mod: an induced (non-Rest) sleep fails while any other Pokémon on the
/// target's side is already asleep.
fn sleep_clause_blocks(state: &State, side: SideId) -> bool {
    if !state.sleep_clause {
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

/// Rampage moves lock the user in for 2-3 turns total, then confuse it.
fn is_rampage_move(id: crate::ids::MoveId) -> bool {
    matches!(id.to_id(), "outrage" | "petaldance" | "thrash" | "ragingfury")
}

/// A rampage use that FAILED to connect — missed (Hustle Outrage), was Protect-blocked, or
/// had no living target: PS drops the `lockedmove` volatile with no confusion (battle-
/// actions removes it on any unsuccessful use), and a FIRST use simply never locks (the
/// `lockedmove` self-volatile only applies on a hit).
fn end_rampage_on_fail(b: &mut Branch, side: SideId, move_id: crate::ids::MoveId) {
    if !is_rampage_move(move_id) {
        return;
    }
    let pending = b.state.side(side).pending_move;
    if matches!(pending, crate::state::PendingMove::Rampaging(m, _) if m == move_id) {
        push(b, Instruction::SetPendingMove { side, previous: pending, new: crate::state::PendingMove::None });
    }
    if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
        push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
    }
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
        return out
            .into_iter()
            .map(|mut b| {
                let pending = b.state.side(side).pending_move;
                if matches!(pending, PendingMove::Rampaging(m, _) if m == move_id) {
                    push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
                    if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                        push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
                    }
                }
                b
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
                            consume_lum_if_statused(&mut b, side);
                            if !b.state.side(side).volatiles.contains(VolatileStatus::Confusion) {
                                return vec![b];
                            }
                            return branch_confusion_counter(b, side);
                        }
                        vec![b]
                    }
                }
                PendingMove::None => {
                    // Starting a rampage: PS's lockedmove `onStart` sets `trueDuration =
                    // this.random(2,4)` (2 or 3), which is the mid-turn (kernel) value; the
                    // end-of-turn `onResidual` then decrements it, so the terminal snapshot is
                    // {1,2}. The engine stores the kernel value here and decrements at end of turn.
                    [2u8, 3]
                        .into_iter()
                        .map(|rem| {
                            let mut nb = scaled(&b, 0.5);
                            // PS `lockedmove` `onStart`: `trueDuration = this.random(2, 4)` (2/3).
                            draw(&mut nb, "random", &[2, 4], rem as i64, "lockedmove");
                            push(&mut nb, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::Rampaging(move_id, rem) });
                            if !nb.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                                push(&mut nb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::LockedMove });
                            }
                            nb
                        })
                        .collect()
                }
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

/// Whether PS overrides a move's accuracy to `true` (bypassing the `hitStepAccuracy` roll
/// entirely) via an `Accuracy`/`ModifyMove` event, as opposed to a numeric accuracy that merely
/// evaluates to 100. A `true` override means PS makes NO accuracy draw — but a later crit /
/// damage roll still happens, so the engine must not emit an accuracy draw here. Cases:
///   * No Guard on either side (`onAnyAccuracy` returns true),
///   * a Glaive Rush target (its volatile's `onAccuracy` returns true),
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
    let mut exec = Exec::Sample(*rng);
    let mut out = generate_instructions_ctx(state, s1, s2, pivot, tera, &mut exec);
    if let Exec::Sample(s) = exec {
        *rng = s;
    }
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
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new() };
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
        external_move: None,
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
        push(b, Instruction::ChangeTypes { side, slot, previous: prev, new: [tera_type, Type::None] });
    }
    push(b, Instruction::ToggleTerastallized { side, slot });
    // Hidden-info: Terastallizing reveals the Tera type to the foe.
    reveal(b, side, 0, crate::state::Reveal::TERA);
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
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new() };
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
    let mut switch_actions: Vec<(SideId, u8)> = [(SideId::One, s1), (SideId::Two, s2)]
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
            if effective_speed(&b.state, order[1].0) > effective_speed(&b.state, order[0].0) {
                order.swap(0, 1);
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
                apply_switch(b, side, target);
                emit_update(b); // switch runAction Update (2882)
                emit_update(b); // runSwitch getAllActive speedSort
                emit_update(b); // runSwitch runAction Update (2882)
            }
        }
    } else {
        for (side, target) in switch_actions {
            for b in &mut branches {
                // Switch-out `eachEvent('Update')` (battle-actions.ts:83) — PRE-swap board.
                emit_switch_pre_update(b);
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
                emit_update(b); // switch action runAction Update
                emit_update(b); // runSwitch getAllActive speedSort
                emit_update(b); // runSwitch runAction Update
            }
        }
    }

    // 1.5) Terastallization happens at turn start (gen9), before moves, for staying mons.
    for (i, side) in [SideId::One, SideId::Two].into_iter().enumerate() {
        if tera[i] && matches!([s1, s2][i], MoveChoice::Move(_)) {
            for b in &mut branches {
                apply_tera(b, side);
            }
        }
    }

    // 2) Moves, ordered by priority then effective speed (speed ties branch 50/50).
    let move_actions: Vec<Action> = [(SideId::One, s1, pivot[0], custap[0]), (SideId::Two, s2, pivot[1], custap[1])]
        .into_iter()
        .filter_map(|(side, c, pv, cu)| match c {
            MoveChoice::Move(idx) => Some(Action { side, move_idx: idx, pivot: pv, foe_pending_move: None, shell_phys: None, custap: cu, external_move: None }),
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
                        // PS `endTurn` resets `statsRaisedThisTurn` on every active after the
                        // residuals run (battle.ts) — drop the volatile at the same boundary.
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

/// Apply a (forced) switch-in directly to `state`: reset the outgoing active's boosts
/// and volatiles, change the active slot, and apply entry hazards. Used by the
/// differential harness to apply post-faint replacement switches.
pub fn switch_into(state: &mut State, side: SideId, target: u8) {
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new() };
    apply_switch(&mut b, side, target);
    clear_stats_raised_markers(&mut b.state);
    *state = b.state;
}

/// Faint replacements resolve inside a pseudo-turn of their own in PS: after the switch-in
/// (and its abilities — Download, Intimidate reactions, ...) another `endTurn` runs, whose
/// per-mon bookkeeping resets `statsRaisedThisTurn` on every active. The replacement entry
/// APIs (`switch_into` / `switch_into_pair`) therefore never leave the marker set.
fn clear_stats_raised_markers(state: &mut State) {
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
    let (species, alive, max_hp, status, counter) = {
        let p = &b.state.side(side).pokemon[slot as usize];
        (p.species, p.is_alive(), p.max_hp, p.status, p.status_counter)
    };
    // Only a genuinely fainted party member is a legal target.
    if species == crate::ids::Species::None || alive || slot == b.state.side(side).active_index {
        return;
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
    // The outgoing mon is the current opposing active from the foe's perspective, so its exit ends
    // any foe-sourced trap (partial trap / Mean Look / Octolock / Jaw Lock) it was holding the foe
    // in — PS clears the linked `trapped`/`partiallytrapped` when the trapper's `clearVolatile`
    // runs. The leaving mon's own trapping volatiles are cleared below via `ALL_VOLATILES`.
    clear_foe_sourced_traps(b, side.other());
    // A traced / copied ability reverts on switch-out (Transform handles its own below).
    {
        let p = b.state.side(side).active();
        if !p.transformed && p.ability != p.base_ability {
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
    // stays Hero. Random-battle spread (31 IV / 85 EV / neutral) assumed for the stat recompute.
    {
        let p = b.state.side(side).active();
        let palafin = crate::ids::Species::from_id("palafin");
        // PS's `onSwitchOut` forme change does NOT run for a fainted mon — a Palafin that faints
        // stays in its base forme.
        if !replacing_fainted && p.ability == crate::ids::Ability::ZeroToHero && Some(p.species) == palafin {
            if let Some(hero) = crate::ids::Species::from_id("palafinhero") {
                let level = p.level;
                let base = crate::data::base_stats(hero);
                let mut stats = [0i16; 6];
                stats[0] = p.stats[0];
                for (si, stat) in [
                    crate::ids::StatIndex::Attack, crate::ids::StatIndex::Defense,
                    crate::ids::StatIndex::SpecialAttack, crate::ids::StatIndex::SpecialDefense,
                    crate::ids::StatIndex::Speed,
                ].into_iter().enumerate() {
                    stats[si + 1] = crate::damage::compute_stat(base[si + 1], 31, 85, level, crate::ids::Nature::Serious, stat);
                }
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
    // Type changes (Protean/Libero, Conversion, Reflect Type, …) revert as the mon leaves
    // the field — PS's clearVolatile resets `types` to baseTypes. A terastallized mon keeps
    // its Tera typing across switches, so leave that untouched.
    {
        let p = b.state.side(side).active();
        if !p.terastallized && p.types != p.base_types {
            let slot = previous;
            push(b, Instruction::ChangeTypes { side, slot, previous: p.types, new: p.base_types });
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
        (o.ability, o.status, o.hp, o.max_hp)
    };
    if out_ability == crate::ids::Ability::NaturalCure && out_status != Status::None {
        push(b, Instruction::ChangeStatus { side, slot: previous, previous: out_status, new: Status::None });
    }
    if out_ability == crate::ids::Ability::Regenerator && out_hp > 0 && out_hp < out_max {
        let heal = (out_max / 3).min(out_max - out_hp);
        if heal > 0 {
            push(b, Instruction::Heal { side, slot: previous, amount: heal });
        }
    }
    // Consecutive-use tracking belongs to the active slot — reset it as the mon leaves.
    reset_move_tracking(b, side);
    push(b, Instruction::Switch { side, previous, next: target });

    // A matured Wish can linger while its slot is empty.  When a faint replacement finally
    // enters that slot, PS removes the stale slot condition without healing the replacement
    // (the healing event belonged to the earlier residual).  Keeping it for another residual
    // incorrectly lets the new occupant receive an expired Wish.
    if replacing_fainted {
        let wish = b.state.side(side).wish;
        if wish.0 == 1 {
            push(b, Instruction::SetWish { side, previous: wish, new: (0, 0) });
        }
    }

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
    }
}

/// Switch both sides simultaneously: entries (and hazards) in speed order of the OUTGOING
/// actives, then switch-in abilities in speed order of the INCOMING actives.
pub fn switch_into_pair(state: &mut State, pairs: [(SideId, u8); 2]) {
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new(), draws: Vec::new() };
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
    clear_stats_raised_markers(&mut b.state);
    *state = b.state;
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
    // Any non-Protect action breaks the Protect chain.
    if !is_protect_move(move_id) && prev_stall != 0 {
        push(b, Instruction::SetStallCounter { side, previous: prev_stall, new: 0 });
    }
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

/// Does Pressure tax this move's PP? Exact via the move's codegen'd target field.
fn pressure_affected(md: &crate::data::MoveData) -> bool {
    // PS: Pressure deducts an extra PP when the move's resolved target includes the Pressure
    // holder or its side. With the codegen'd target field this is now exact.
    md.target.targets_foe()
}

/// On-switch-in ability effects (weather setters and Intimidate).
fn apply_switch_in_ability(b: &mut Branch, side: SideId) {
    use crate::ids::Ability::*;
    let ability = b.state.side(side).active().ability;
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
        push(b, Instruction::ChangeTerrain {
            previous: b.state.terrain,
            previous_turns: b.state.terrain_turns,
            new: terrain,
            new_turns: turns,
        });
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
        let untraceable = matches!(
            fa,
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
        );
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
    // Download: +1 Atk if the foe's Defense ≤ Special Defense, else +1 SpA.
    if ability == Download {
        let foe = side.other();
        if b.state.side(foe).active().is_alive() {
            let f = b.state.side(foe).active();
            let (def, spd) = (f.stat(crate::ids::StatIndex::Defense), f.stat(crate::ids::StatIndex::SpecialDefense));
            let stat = if def <= spd { BoostIndex::Attack } else { BoostIndex::SpecialAttack };
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

    // Heavy-Duty Boots negate *all* entry hazards for the holder.
    if p.item == Item::HeavyDutyBoots {
        return;
    }
    let magic_guard = p.ability == crate::ids::Ability::MagicGuard;

    // Stealth Rock — hits everything, scaled by Rock effectiveness (Magic Guard blocks it).
    if s.side_conditions.stealth_rock && !magic_guard {
        let mult = type_multiplier(Type::Rock, p.types);
        let dmg = ((maxhp as f32 / 8.0) * mult).floor() as i16;
        let dmg = dmg.max(1).min(p.hp);
        if dmg > 0 {
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }
    }
    // Spikes — grounded only (Magic Guard blocks it).
    let layers = b.state.side(side).side_conditions.spikes;
    if grounded && layers > 0 && !magic_guard {
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
            if status_applies(p, status) && !status_blocked_by_field(&b.state, side, status) {
                push(b, Instruction::ChangeStatus { side, slot, previous: Status::None, new: status });
                consume_lum_if_statused(b, side);
            }
        }
    }
    // Sticky Web — grounded: −1 Speed on entry.
    if grounded && b.state.side(side).side_conditions.sticky_web {
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
    /// A Dancer-invoked copy of `Some(move)` (PS `externalMove`): the move executes from
    /// this side without a move slot — no PP cost, no Encore/rampage override, no move-use
    /// bookkeeping, no rampage lock (the "Dancer Petal Dance hack"), and no re-trigger of
    /// Dancer. The full BeforeMove gauntlet (sleep/attract/confusion/paralysis) still runs.
    pub(crate) external_move: Option<crate::ids::MoveId>,
}

/// Run one move action and append its trailing runAction Update (battle.ts:2882): after EVERY
/// move action completes — hit, miss, immunity, or a fully-cancelled attempt — PS fires
/// `eachEvent('Update')`, which shuffles on a surviving equal-Speed pair. The in-kernel per-hit
/// (970) and post-hit-loop (1024) Updates are emitted inside `execute_move`; this adds the 2882.
fn run_move_action(b: Branch, action: Action) -> Vec<Branch> {
    let mut out = execute_move(b, action);
    if annotating() {
        for nb in &mut out {
            emit_update(nb);
        }
    }
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
    Branch { prob: b.prob * f, state: b.state, ins: b.ins.clone(), draws: b.draws.clone() }
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
        if fb.state.side(second.side).active().is_alive() && !battle_over(&fb.state) {
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

/// A move's effective priority, including Prankster (+1 to the user's status moves).
fn effective_priority(state: &State, side: SideId, move_idx: u8) -> i8 {
    let md = move_data(state.side(side).active().moves[move_idx as usize].id);
    let mut pri = md.priority;
    if md.category == MoveCategory::Status
        && state.side(side).active().ability == crate::ids::Ability::Prankster
    {
        pri += 1;
    }
    if md.id.to_id() == "grassyglide"
        && state.terrain == crate::ids::Terrain::Grassy
        && is_grounded(state, side)
    {
        pri += 1;
    }
    if state.side(side).active().ability == crate::ids::Ability::GaleWings
        && md.typ == Type::Flying
        && state.side(side).active().hp >= state.side(side).active().max_hp
    {
        pri += 1;
    }
    // Triage: +3 priority to moves with the heal flag (drain moves like Draining Kiss, and
    // recovery moves). PS `onModifyPriority`.
    if md.flag_heal && state.side(side).active().ability == crate::ids::Ability::Triage {
        pri += 3;
    }
    pri
}

pub(crate) fn move_order(state: &State, a: &Action, b: &Action) -> Order {
    let (sa, sb) = (a.side, b.side);
    let pa = effective_priority(state, sa, a.move_idx);
    let pb = effective_priority(state, sb, b.move_idx);
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
            && move_data(p.moves[act.move_idx as usize].id).category == MoveCategory::Status
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
    // Trapping volatiles clear when their holder leaves the field (self-traps and the trapped
    // mon's own copy). Foe-sourced traps additionally end when the TRAPPER leaves — handled by
    // `clear_foe_sourced_traps` in `apply_switch_inner`.
    VolatileStatus::Trapped, VolatileStatus::Ingrain, VolatileStatus::NoRetreat, VolatileStatus::Octolock,
    // Protosynthesis / Quark Drive end on switch-out (PS ability `onEnd` deletes the volatile);
    // they are re-derived on switch-in in `apply_switch_in_ability` (weather/terrain, else the
    // one-shot Booster Energy). A mon that stays in never reaches this path, so its boost persists.
    VolatileStatus::Protosynthesis, VolatileStatus::QuarkDrive,
    // Flash Fire's activation ends on switch-out (PS ability `onEnd` removes the volatile).
    VolatileStatus::FlashFire,
    // Truant's loaf marker is a PS volatile — cleared on switch-out; the mon acts on its
    // first attempt after re-entering. `statsRaisedThisTurn` is a per-Pokémon PS field, but
    // only the active can have raised a stat this turn, so the volatile model is exact.
    VolatileStatus::Truant, VolatileStatus::StatsRaisedThisTurn,
];

/// Per-hit critical-hit probability (gen9 base, no crit-stage modifiers modeled).
const CRIT: f32 = 1.0 / 24.0;

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

/// Execute one move, first splitting on confusion: a confused, awake mon has a 1/3 chance to
/// hit itself instead of acting. The 2/3 "acts normally" branch is identical to no-confusion
/// behavior, so this only *adds* the self-hit outcomes (no regression on the common path).
pub(crate) fn execute_move(b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    let mut b = b;
    let (alive, status, confused) = {
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
    if !alive || status == Status::Sleep {
        // The sleep-wake attempt toggles Truant inside `execute_move_inner` (slp's
        // BeforeMove handler at priority 10 runs before Truant's 9).
        return dispatch_move_inner(b, action);
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
            if truant_gate(&mut b, side) {
                return vec![b];
            }
            return dispatch_move_inner(b, action);
        }
        let mut frozen = scaled(&b, 0.80);
        draw(&mut frozen, "randomChance", &[1, 5], 0, "frz");
        let mut out = vec![frozen];
        let mut thawed = scaled(&b, 0.20);
        draw(&mut thawed, "randomChance", &[1, 5], 1, "frz");
        let slot = thawed.state.side(side).active_index;
        push(&mut thawed, Instruction::ChangeStatus { side, slot, previous: Status::Freeze, new: Status::None });
        // frz (priority 10) ran; Truant (9) is next — a thawed loafer stays put this turn.
        if truant_gate(&mut thawed, side) {
            out.push(thawed);
        } else {
            out.extend(dispatch_move_inner(thawed, action));
        }
        return out;
    }
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
    let mut out = execute_move_inner(scaled(&b, 0.5), Action { shell_phys: Some(true), ..action });
    out.extend(execute_move_inner(scaled(&b, 0.5), Action { shell_phys: Some(false), ..action }));
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
            let last = b.state.side(foe).last_used_move;
            let encorable = last != crate::ids::MoveId::None
                && !matches!(last.to_id(), "struggle" | "encore" | "mimic" | "mirrormove" | "sketch" | "transform")
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
            let t = b.state.side(foe).active();
            if t.status == Status::None && status_applies(t, Status::Sleep) {
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
        _ => {
            push(&mut b, Instruction::ApplyVolatile { side: foe, volatile: v });
        }
    }
    vec![b]
}

/// The confusion self-hit: a 40-BP typeless physical attack the mon lands on itself, using
/// its own (boosted) Attack and Defense, halved by burn. Enumerates the 16 damage rolls.
fn confusion_self_hit(b: Branch, side: SideId) -> Vec<Branch> {
    let (level, atk, def, burned, hp) = {
        let s = b.state.side(side);
        let p = s.active();
        let atk = boosted_stat(p.stat(crate::ids::StatIndex::Attack) as i64, s.boost(BoostIndex::Attack));
        let def = boosted_stat(p.stat(crate::ids::StatIndex::Defense) as i64, s.boost(BoostIndex::Defense)).max(1);
        (p.level as i64, atk, def, p.status == Status::Burn, p.hp)
    };
    let lvl_factor = 2 * level / 5 + 2;
    let bd = (lvl_factor * 40 * atk) / def / 50 + 2;
    let mut out = Vec::with_capacity(16);
    for i in 0..16i64 {
        let mut dmg = bd * (85 + i) / 100;
        if burned {
            dmg /= 2;
        }
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
    let (base, restat) = if p.species == pirouette {
        (crate::ids::Species::from_id("meloetta").unwrap_or(crate::ids::Species::None), true)
    } else if p.species == hangry {
        (crate::ids::Species::from_id("morpeko").unwrap_or(crate::ids::Species::None), false)
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
        if !p.terastallized {
            new.types = crate::data::species_types(base);
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
        types: p.base_types,
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
            }
        }
    }
    out
}

/// Execute one move from `action.side`, returning the resulting branches.
fn execute_move_inner(b: Branch, action: Action) -> Vec<Branch> {
    let Action { side, move_idx, pivot, foe_pending_move, external_move, .. } = action;
    let external = external_move.is_some();
    let attacker = b.state.side(side).active();
    if !attacker.is_alive() {
        return vec![b];
    }
    // A Dancer-invoked copy carries its move directly (it need not be on the user's set).
    let move_id = external_move.unwrap_or(attacker.moves[move_idx as usize].id);
    // Struggle: a mon forced to act with no usable moves (the chosen slot is out of PP) uses
    // Struggle instead — a typeless 50-BP physical hit that connects on everything and recoils
    // 1/4 of the user's max HP.
    let struggling = !external && attacker.moves[move_idx as usize].pp == 0;
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
    // Judgment: takes the type of the user's held Plate (PS `onModifyType` via
    // `item.onPlate`; randbats Arceus formes always hold their matching Plate).
    if md.id.to_id() == "judgment" {
        if let Some(t) = plate_type(attacker.item) {
            md.typ = t;
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
            md.base_power = crate::damage::modify(md.base_power as i64, 4915, 4096) as u16;
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
    // NOTE: dynamic-BP moves (weight/HP-scaled) recompute base power inside compute_damage
    // and drop this multiply; no in-format Analytic holder carries one.
    if attacker.ability == crate::ids::Ability::Analytic
        && foe_pending_move.is_none()
        && md.category != MoveCategory::Status
        && md.base_power > 0
    {
        md.base_power = crate::damage::modify(md.base_power as i64, 5325, 4096) as u16;
    }
    let foe = side.other();

    let mut b = b;
    let mut move_idx = move_idx;
    let slot = b.state.side(side).active_index;

    // Encore: the user is locked into its encored move — the chosen slot is overridden
    // (PS onOverrideAction). Skipped while committed to a multi-turn move or Struggling.
    let enc = b.state.side(side).encore;
    if enc.0 != crate::ids::MoveId::None
        && !struggling
        && !external
        && b.state.side(side).pending_move == crate::state::PendingMove::None
    {
        if let Some(enc_slot) = b.state.side(side).active().moves.iter().position(|m| m.id == enc.0 && m.pp > 0) {
            if enc_slot as u8 != move_idx {
                move_idx = enc_slot as u8;
                md = move_data(enc.0);
            }
        }
    }
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

    // Destiny Bond lasts until the user's next move: moving again drops it.
    if b.state.side(side).volatiles.contains(VolatileStatus::DestinyBond) {
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
            push(&mut b, Instruction::ChangeTypes { side, slot, previous: prev, new: [md.typ, Type::None] });
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

    // Disable: the disabled move fails outright; Taunt: status moves fail.
    if !struggling {
        let dis = b.state.side(side).disable;
        if dis.0 != crate::ids::MoveId::None && dis.0 == md.id {
            return vec![b];
        }
        // Throat Chop: sound moves fail. Heal Block: heal-flag moves (incl. drains) fail.
        if b.state.side(side).volatiles.contains(VolatileStatus::ThroatChop) && md.flag_sound {
            return vec![b];
        }
        if b.state.side(side).volatiles.contains(VolatileStatus::HealBlock) && md.flag_heal {
            return vec![b];
        }
        if b.state.side(side).taunt_turns > 0 && md.category == MoveCategory::Status {
            return vec![b];
        }
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
    if status == Status::Sleep {
        // Early Bird burns sleep turns twice as fast (PS slp onBeforeMove: an extra time--).
        let tick = if b.state.side(side).active().ability == crate::ids::Ability::EarlyBird { 2 } else { 1 };
        if counter > tick {
            push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: counter - tick });
            return vec![b];
        }
        push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
        // The wake attempt reaches Truant's BeforeMove handler (priority 9, right after slp's
        // 10): the toggle fires now, and a due loaf consumes the freshly-woken turn.
        if truant_gate(&mut b, side) {
            return vec![b];
        }
    }
    // Freeze: a frozen mon can't move (the 20% thaw + act is left unmodeled for now).
    if b.state.side(side).active().status == Status::Freeze {
        return vec![b];
    }

    // --- multi-turn move commitment (charge / semi-invulnerable / recharge) ---
    use crate::state::PendingMove;
    let pending = b.state.side(side).pending_move;
    // Recharge: the mon spent a recharge move last turn and forfeits this one.
    if matches!(pending, PendingMove::Recharging) {
        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
        return vec![b];
    }
    // Are we cashing in a two-turn move that finished charging last turn?
    let executing_charge = matches!(pending, PendingMove::Charging(m) if m == move_id);

    // PP is paid on the charge turn, not the strike turn. Pressure on the opposing active
    // costs one extra PP for any move that targets it (PS onDeductPP; cosim caught this).
    if !executing_charge && !rampaging_now && !external {
        let pp = b.state.side(side).active().moves[move_idx as usize].pp;
        if pp > 0 {
            let foe_active = b.state.side(side.other()).active();
            let pressured = foe_active.is_alive()
                && foe_active.ability == crate::ids::Ability::Pressure
                && pressure_affected(&md);
            let amount = if pressured { 2u8.min(pp) } else { 1 };
            push(&mut b, Instruction::DecrementPp { side, slot, move_index: move_idx, amount });
        }
    }

    // Record the move use for consecutive-use mechanics (streak / Protect stall chain). The
    // mon has passed sleep/freeze, so it is actually acting this turn. A Dancer copy is
    // `isExternal` in PS: no lastMove/streak bookkeeping and no Choice lock.
    if !external {
        record_move_use(&mut b, side, move_id);
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

    // Absorbing abilities (Volt Absorb / Water Absorb / Dry Skin / Earth Eater) nullify a move
    // of their type that targets the holder AND heal it 1/4 max HP (PS onTryHit). This fires for
    // damaging AND status moves alike — e.g. Thunder Wave vs Volt Absorb heals and prevents the
    // paralysis. Mold Breaker bypasses. Side/field moves (hazards) don't target the mon.
    {
        use crate::ids::Ability as A;
        let foe_status_target = md.status != Status::None
            || md.target_boosts.iter().any(|&x| x != 0)
            || md.target_volatile.is_some()
            || md.force_switch
            // Strength Sap's foe-facing effect is `onHit`-only (invisible to the codegen),
            // but it targets the mon — Sap Sipper absorbs it (cosim caught the miss).
            || md.id.to_id() == "strengthsap";
        let affects_foe_mon = md.category != MoveCategory::Status || foe_status_target;
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
            return vec![b];
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
            return vec![b];
        }
    }

    // Self-destructing "always" moves (Explosion / Self-Destruct / Misty Explosion) faint the
    // user BEFORE the hit is attempted — PS gen9 queues `battle.faint(pokemon)` in `useMove`
    // ahead of `tryMoveHit`. So the user faints even against a type-immune target, through
    // Protect, or on a miss. (Damp already cancelled the move above, so no faint there.) The
    // hit-branch self-destruct in `apply_post_damage` is a no-op once the user is already down.
    if matches!(move_id.to_id(), "explosion" | "selfdestruct" | "mistyexplosion") {
        let (alive, hp, aslot) = {
            let p = b.state.side(side).active();
            (p.is_alive(), p.hp, b.state.side(side).active_index)
        };
        if alive {
            push(&mut b, Instruction::Damage { side, slot: aslot, amount: hp });
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
                Pivot::Target(t) => apply_switch_pass_sub(&mut b, side, t),
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
        // recovery, weather, hazards).
        let targets_foe = md.status != Status::None
            || md.target_boosts.iter().any(|&x| x != 0)
            || md.target_volatile.is_some()
            || md.force_switch;
        if targets_foe && b.state.side(foe).volatiles.contains(VolatileStatus::Protect) {
            return vec![b];
        }
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
            return execute_status_move(b, foe, &md, false);
        }
        // A Substitute blocks foe-targeting status moves unless they bypass it (sound
        // moves, Taunt, Encore, ...) or the user has Infiltrator.
        if targets_foe
            && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute)
            && !md.flag_bypass_sub
            && b.state.side(side).active().ability != crate::ids::Ability::Infiltrator
        {
            return vec![b];
        }
        let ins_before = b.ins.len();
        let mut branches = execute_status_move(b, side, &md, foe_pending_move.is_some());
        // PS runs a status move through `hitStepMoveHitLoop` exactly like a damaging move: a
        // `moveHit` that applies its effect fires the per-hit `eachEvent('Update')`
        // (battle-actions.ts:970) and the post-hit-loop `eachEvent('Update')` (:1024). A move that
        // fails at `tryHit` / `spreadMoveHit` (immune / already-statused / missed / redundant boost)
        // breaks BEFORE 970 and fires neither. Both are actives-Speed-tie shuffles (no-op off a
        // tie), so this only affects tied boards. Detect a successful moveHit per-branch as "the
        // status resolution added ≥1 effect instruction" — a failed move applies none. Pure
        // protect-fail bookkeeping (only a `SetStallCounter` reset) is NOT a moveHit success.
        if annotating() {
            for sb in &mut branches {
                let did_something = sb.ins.len() > ins_before
                    && sb.ins[ins_before..]
                        .iter()
                        .any(|i| !matches!(i, Instruction::SetStallCounter { .. }));
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
                        apply_switch(sb, side, t);
                    }
                }
            }
            Pivot::Pause => {
                for sb in &mut branches {
                    if sb.state.side(side).active().is_alive() {
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
        if matches!(md.id.to_id(), "highjumpkick" | "jumpkick" | "supercellslam") {
            let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
            let crash = (maxhp / 2).min(hp);
            if crash > 0 && b.state.side(side).active().ability != crate::ids::Ability::MagicGuard {
                let slot = b.state.side(side).active_index;
                push(&mut b, Instruction::Damage { side, slot, amount: crash });
            }
        }
        // A blocked rampage ends its lock without confusion; a blocked first use never locks.
        end_rampage_on_fail(&mut b, side, move_id);
        return vec![b];
    }

    // Psychic Terrain blocks priority moves aimed at grounded targets.
    if b.state.terrain == crate::ids::Terrain::Psychic
        && md.priority > 0
        && is_grounded(&b.state, foe)
        && b.state.side(foe).active().is_alive()
    {
        return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
    }

    // Queenly Majesty (breakable): the foe's increased-priority moves fail against the
    // holder's side (PS onFoeTryMove, move.priority > 0.1 — the EFFECTIVE priority, so
    // Prankster / Gale Wings / Grassy Glide boosts count; foeSide-targeting moves exempt).
    {
        let holder = b.state.side(foe).active();
        if holder.is_alive() && holder.ability == crate::ids::Ability::QueenlyMajesty {
            let atk = b.state.side(side).active();
            let mb = matches!(atk.ability, crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze);
            let mut pri = md.priority;
            if md.category == MoveCategory::Status && atk.ability == crate::ids::Ability::Prankster {
                pri += 1;
            }
            if atk.ability == crate::ids::Ability::GaleWings && md.typ == Type::Flying && atk.hp >= atk.max_hp {
                pri += 1;
            }
            if md.id.to_id() == "grassyglide" && b.state.terrain == crate::ids::Terrain::Grassy && is_grounded(&b.state, side) {
                pri += 1;
            }
            let side_targeting = md.side_condition.is_some() && md.target != crate::data::MoveTarget::User;
            if pri > 0 && !mb && !side_targeting {
                return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
            }
        }
    }

    // Air Balloon: the holder is untargetable by Ground moves until the balloon pops.
    if md.typ == Type::Ground
        && b.state.side(foe).active().item == Item::AirBalloon
        && b.state.side(foe).active().is_alive()
    {
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
    if annotating() && md.accuracy != 0 && !accuracy_forced_true(&b, side, &md) {
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
        if matches!(md.id.to_id(), "highjumpkick" | "jumpkick" | "supercellslam") {
            let (hp, maxhp) = { let p = mb.state.side(side).active(); (p.hp, p.max_hp) };
            let crash = (maxhp / 2).min(hp);
            if crash > 0 {
                let slot = mb.state.side(side).active_index;
                push(&mut mb, Instruction::Damage { side, slot, amount: crash });
            }
        }
        end_rampage_on_fail(&mut mb, side, move_id);
        miss_out.push(mb);
    }

    let foe_alive = b.state.side(foe).active().is_alive();
    if !foe_alive {
        // No living target: the move fails outright — no rampage lock, no recharge.
        let mut hb = scaled(&b, hit_prob);
        end_rampage_on_fail(&mut hb, side, move_id);
        out.push(hb);
        out.extend(miss_out);
        return out;
    }
    // A target mid-Fly/Dig/etc. (semi-invulnerable) dodges the move entirely.
    if matches!(b.state.side(foe).pending_move, PendingMove::Charging(m) if is_semi_invuln_move(m)) {
        let mut hb = scaled(&b, hit_prob);
        end_rampage_on_fail(&mut hb, side, move_id);
        out.push(hb);
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
        if def_ab == crate::ids::Ability::WellBakedBody && md.typ == Type::Fire {
            raise_boost(&mut ib, foe, BoostIndex::Defense, 2);
        }
        if wind_immune {
            raise_boost(&mut ib, foe, BoostIndex::Attack, 1);
        }
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
        break_ice_face(&mut hb, foe);
        out.push(hb);
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
        bust_disguise(&mut hb, foe);
        match pivot {
            Pivot::Target(t) => if hb.state.side(side).active().is_alive() { apply_switch(&mut hb, side, t); },
            Pivot::Pause => if hb.state.side(side).active().is_alive() { push(&mut hb, Instruction::PivotPending { side }); },
            Pivot::Stay => {}
        }
        out.push(hb);
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
        apply_multihit_realized(&b, side, &md, hit_prob, cur)
    } else if md.id.to_id() == "beatup" {
        // Beat Up: one hit per eligible party member with per-member base power.
        apply_beatup(&b, side, &md, hit_prob)
    } else if let Some(fixed) = fixed_damage_amount(&md, &b.state, side) {
        // Fixed-damage moves (Night Shade / Seismic Toss = level, Dragon Rage = 40, ...) skip
        // the damage formula entirely: one deterministic outcome, no rolls or crit.
        let mut hb = scaled(&b, hit_prob);
        let calc = compute_damage(&hb, side, &md);
        let target_hp = hb.state.side(foe).active().hp;
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
        apply_post_damage(&mut hb, side, &md, dealt as i32, dealt > 0, false, (dealt > 0) as u8, calc.life_orb, calc.def_item, calc.def_ability);
        vec![(hb, false)]
    } else if matches!(md.id.to_id(), "populationbomb")
        && realized_cursor(&b).is_some()
    {
        // Population Bomb (10 hits, multiaccuracy) can't enumerate its per-hit product — realize the
        // single branch off the source (per-hit accuracy + crit + damage, Loaded Dice count).
        let cur = realized_cursor(&b).unwrap();
        let calcs = vec![compute_damage(&b, side, &md)];
        apply_multihit_realized_ma(&b, side, &md, hit_prob, &calcs, cur)
    } else if matches!(md.id.to_id(), "tripleaxel" | "triplekick") {
        // Ascending power (20/40/60 or 10/20/30) with a fresh 90% accuracy check per hit;
        // a miss ends the move. hit_prob here is the single-hit accuracy.
        let step = md.base_power;
        let mds: Vec<crate::data::MoveData> = (1..=3u16)
            .map(|i| { let mut m = md; m.base_power = step * i; m })
            .collect();
        let calcs: Vec<DamageCalc> = mds.iter().map(|m| compute_damage(&b, side, m)).collect();
        if let Some(cur) = realized_cursor(&b) {
            // Realized single path (seed gate / differ): per-hit accuracy + crit + damage off the
            // source, KO-truncated. The enumerated branch below serves Enumerate/Sample (no draws).
            apply_multihit_realized_ma(&b, side, &md, hit_prob, &calcs, cur)
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
        for combo in HitCombos::new(hits_min) {
            let mut prob = hit_prob;
            for &(_, crit) in &combo {
                prob *= (1.0 / 16.0) * if crit { crit_p } else { 1.0 - crit_p };
            }
            if prob <= 0.0 {
                continue;
            }
            let mut hb = scaled(&b, prob);
            // Per-hit crit+damage draws are emitted INSIDE the hit loop (KO-terminating), so a
            // multi-hit move that faints the target early stops rolling exactly where PS does
            // (`hitStepMoveHitLoop`: the top-of-loop `targets.every(!hp)` break precedes the next
            // hit's crit/damage rolls). See `apply_damage_hit`.
            let hit_sub = apply_damage_hit(&mut hb, side, &md, &combo, crit_den);
            v.push((hb, hit_sub));
        }
        v
    } else if ice_face_is_intact(&b, foe, &md) {
        apply_multihit_dp_ice_face(&b, side, &md, hits_min, hits_max, hit_prob)
    } else {
        apply_multihit_dp(&b, side, &md, hits_min, hits_max, hit_prob)
    };
    for (mut hb, hit_sub) in damaged {
        apply_damage_secondaries(&mut hb, side, &md, hit_sub);
        // Double Shock: the connecting hit strips the user's Electric type (PS `self.onHit`
        // `setType`, mapping Electric -> "???" typeless). A pure-Electric user becomes fully
        // typeless; Pawmot (Electric/Fighting) keeps only Fighting. Modeled as Type::None in the
        // stripped slot (the engine's typeless).
        if md.id.to_id() == "doubleshock" && hb.state.side(side).active().is_alive() {
            let p = hb.state.side(side).active();
            if p.types.contains(&Type::Electric) {
                let prev = p.types;
                let new = [
                    if prev[0] == Type::Electric { Type::None } else { prev[0] },
                    if prev[1] == Type::Electric { Type::None } else { prev[1] },
                ];
                let slot = hb.state.side(side).active_index;
                push(&mut hb, Instruction::ChangeTypes { side, slot, previous: prev, new });
            }
        }
        // Weakness Policy on the target (super-effective hit), then White Herb if the user's
        // own self-drops (Leaf Storm, Close Combat, ...) left a negative stage.
        apply_weakness_policy(&mut hb, foe, &md);
        apply_justified(&mut hb, foe, &md);
        // Rattled / Thermal Exchange (onDamagingHit), Bug Bite's berry steal and the frozen-
        // target thaw (move onHit / frz onHit) don't fire when a Substitute took the hit.
        if !hit_sub {
            apply_rattled(&mut hb, foe, &md);
            apply_thermal_exchange(&mut hb, foe, &md);
            apply_bug_bite(&mut hb, side, &md);
            apply_thaw_on_hit(&mut hb, foe, &md);
            apply_spirit_shackle(&mut hb, side, &md);
            apply_sparkling_aria(&mut hb, side, &md);
        }
        // Stone Axe sets Stealth Rock on the target's side whether the hit landed on the mon
        // OR its Substitute (PS has both `onAfterHit` and `onAfterSubDamage`), as long as the
        // user is still standing. Glaive Rush's self-drawback likewise applies on any hit.
        if md.id.to_id() == "stoneaxe" && hb.state.side(side).active().is_alive() {
            apply_hazard(&mut hb, foe, SideConditionId::StealthRock);
        }
        // Ceaseless Edge lays a layer of Spikes on the target's side on any hit (PS
        // `onAfterHit`/`onAfterSubDamage`), as long as the user is still standing.
        if md.id.to_id() == "ceaselessedge" && hb.state.side(side).active().is_alive() {
            apply_hazard(&mut hb, foe, SideConditionId::Spikes);
        }
        if md.id.to_id() == "glaiverush" && hb.state.side(side).active().is_alive()
            && !hb.state.side(side).volatiles.contains(VolatileStatus::GlaiveRush)
        {
            push(&mut hb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::GlaiveRush });
        }
        apply_relic_song_forme(&mut hb, side, &md);
        apply_weak_armor(&mut hb, foe, &md);
        apply_throat_spray(&mut hb, side, &md);
        apply_spin_clear(&mut hb, side, &md);
        apply_white_herb(&mut hb, side);
        // Pinch berries fire on the HP drop from the move (defender) and any recoil (user).
        apply_pinch_berry(&mut hb, foe);
        apply_pinch_berry(&mut hb, side);
        // A Substitute blocks the target's own secondaries (boosts/status) and contact
        // abilities; otherwise split on the move's secondary, then the contact-status ability.
        let branches = if hit_sub {
            // PS `spreadMoveHit` sets `targets[i] = null` on a Substitute hit (battle-actions.ts:1085),
            // and `secondaries()` skips only `target === false` (`null !== false`), so it STILL rolls
            // `this.random(100)` per secondary — the effect merely no-ops (moveHit on a null target).
            // `ModifySecondaries` runs on the (null) target, so Shield Dust / Covert Cloak do NOT
            // strip it here; only Sheer Force (which removed the secondaries upfront) suppresses the
            // roll. Emit those draw-and-discard rolls so the sub hit consumes the same stream PS does.
            emit_sub_secondary_rolls(&mut hb, side, &md);
            vec![hb]
        } else {
            apply_burning_jealousy(&mut hb, side, &md);
            apply_target_secondary(hb, side, &md)
                .into_iter()
                .flat_map(|sb| apply_triattack_secondary(sb, side, &md))
                .flat_map(|sb| apply_direclaw_secondary(sb, side, &md))
                .flat_map(|sb| apply_partial_trap(sb, side, &md))
                .flat_map(|sb| apply_contact_secondaries(sb, side, &md))
                .flat_map(|sb| apply_flinch_split(sb, side, &md))
                .flat_map(|sb| apply_cursed_body(sb, side, &md))
                .collect::<Vec<_>>()
        };
        for mut sb in branches {
            // In-kernel Update shuffles for this connecting hit, in PS order (after `spreadMoveHit`
            // = self-drops + target secondaries + DamagingHit contact abilities have all rolled):
            //   970  per-hit `eachEvent('Update')` — fires on the PRE-faint board (a target
            //        reduced to 0 HP this hit is still in getAllActive), so a KO still shuffles.
            //   1024 post-hit-loop `eachEvent('Update')` — fires once for the move but AFTER
            //        faintMessages, so a KO'd (now-fainted) target breaks the tie and it doesn't.
            // Both emitted before the pivot/drag switch changes the on-field mon (and its Speed).
            emit_update_hit(&mut sb);
            emit_update(&mut sb);
            // Pivot move (U-turn): switch the user out now that it connected.
            match pivot {
                Pivot::Target(t) => {
                    if sb.state.side(side).active().is_alive() {
                        apply_switch(&mut sb, side, t);
                    }
                }
                Pivot::Pause => {
                    if sb.state.side(side).active().is_alive() {
                        push(&mut sb, Instruction::PivotPending { side });
                    }
                }
                Pivot::Stay => {}
            }
            // Dragon Tail / Circle Throw: the survivor is dragged out (uniform over the bench).
            // Only a connecting hit drags — a MISS (which sits in `out` from the accuracy split
            // above) must leave the target in place, so the drag lives on the hit branches here.
            if md.force_switch {
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
    for &(roll, crit) in combo {
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
fn compute_damage(b: &Branch, side: SideId, md: &crate::data::MoveData) -> DamageCalc {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let attacker = b.state.side(side).active();
    let defender = b.state.side(foe).active();
    // Mold Breaker suppresses the defender's damage-affecting ability for this move; `def_ab`
    // is the defender's ability as the damage calc should see it (None when suppressed).
    let mb = matches!(attacker.ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze)
        || move_ignores_ability(md.id);
    let def_ab = if mb { Ab::None } else { defender.ability };

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
    let def_boost = if attacker.ability == crate::ids::Ability::Unaware { 0 } else { b.state.side(foe).boost(def_boost_idx) };

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
        let mut atk_stat = boosted_stat(atk_owner.stat(atk_idx) as i64, boost);
        // Heatproof (defender): incoming Fire damage halved via the offensive stat
        // (PS `onSourceModifyAtk`/`onSourceModifySpA` chainModify(0.5)).
        if def_ab == Ab::Heatproof && md.typ == Type::Fire {
            atk_stat = crate::damage::modify(atk_stat, 1, 2);
        }
        if tablets && md.category == MoveCategory::Physical {
            atk_stat = crate::damage::modify(atk_stat, 3, 4);
        }
        if vessel && md.category == MoveCategory::Special {
            atk_stat = crate::damage::modify(atk_stat, 3, 4);
        }
        // Item stat modifiers (PS applies these via `modify`, round-half-up).
        match (attacker.item, md.category) {
            (Item::ChoiceBand, MoveCategory::Physical) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            (Item::ChoiceSpecs, MoveCategory::Special) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            _ => {}
        }
        // Light Ball doubles both offensive stats for every Pikachu forme. PS keys this on
        // `baseSpecies.baseSpecies === Pikachu`; all generated forme ids share the prefix.
        if attacker.item == Item::LightBall && attacker.species.to_id().starts_with("pikachu") {
            atk_stat = crate::damage::modify(atk_stat, 2, 1);
        }
        // Purifying Salt halves the attacker's offensive stat vs Ghost moves (onSourceModify
        // Atk/SpA chainModify(0.5)) — NOT the final damage, so the rounding point matters.
        if def_ab == Ab::PurifyingSalt && md.typ == Type::Ghost {
            atk_stat = crate::damage::modify(atk_stat, 1, 2);
        }
        if proto_atk {
            atk_stat = crate::damage::modify(atk_stat, 5325, 4096);
        }
        // Orichalcum Pulse (physical Atk in sun) / Hadron Engine (special SpA in Electric
        // Terrain): ×5461/4096 — PS onModifyAtk / onModifySpA.
        if attacker.ability == Ab::OrichalcumPulse
            && md.category == MoveCategory::Physical
            && matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun)
        {
            atk_stat = crate::damage::modify(atk_stat, 5461, 4096);
        }
        if attacker.ability == Ab::HadronEngine
            && md.category == MoveCategory::Special
            && b.state.terrain == crate::ids::Terrain::Electric
        {
            atk_stat = crate::damage::modify(atk_stat, 5461, 4096);
        }
        // Offensive ability multipliers.
        match attacker.ability {
            Ab::HugePower | Ab::PurePower => atk_stat = crate::damage::modify(atk_stat, 2, 1),
            Ab::Guts if attacker.status != Status::None => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::SlowStart if md.category == MoveCategory::Physical && b.state.side(side).active_turns <= 5 => {
                atk_stat = crate::damage::modify(atk_stat, 1, 2)
            }
            Ab::Overgrow if md.typ == Type::Grass && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Blaze if md.typ == Type::Fire && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Torrent if md.typ == Type::Water && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Swarm if md.typ == Type::Bug && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            // Sheer Force: ×1.3 when the move has a secondary (the secondary is then removed).

            Ab::Reckless if md.recoil.0 > 0 => atk_stat = crate::damage::modify(atk_stat, 4915, 4096),
            Ab::Defeatist if (attacker.hp as i32) * 2 <= attacker.max_hp as i32 => atk_stat = crate::damage::modify(atk_stat, 1, 2),
            Ab::ToughClaws if md.flag_contact => atk_stat = crate::damage::modify(atk_stat, 5325, 4096), // ×1.3
            // Sharpness is handled as a base-power modifier below (PS `onBasePower`), not here —
            // its ×1.5 placement matters once a ×0.5 type multiplier is in play (cosim caught a
            // Ceaseless Edge unit whose rounding only matched with the base-power floor).
            Ab::MegaLauncher if md.flag_pulse => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            // Punk Rock is handled as a base-power modifier below (PS `onBasePower`), not here.
            Ab::Hustle if md.category == MoveCategory::Physical => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::ToxicBoost
                if matches!(attacker.status, Status::Poison | Status::Toxic)
                    && md.category == MoveCategory::Physical =>
            {
                atk_stat = crate::damage::modify(atk_stat, 3, 2)
            }
            Ab::FlareBoost
                if attacker.status == Status::Burn && md.category == MoveCategory::Special =>
            {
                atk_stat = crate::damage::modify(atk_stat, 3, 2)
            }
            // Type-boosting abilities (applied to the offensive stat like the others above).
            // Stakeout: ×2 offensive stat vs a target that switched in this turn (activeTurns==0).
            // PS onModifyAtk / onModifySpA chainModify(2).
            Ab::Stakeout if b.state.side(foe).active_turns == 0 => atk_stat = crate::damage::modify(atk_stat, 2, 1),
            Ab::WaterBubble if md.typ == Type::Water => atk_stat = crate::damage::modify(atk_stat, 2, 1),
            Ab::Transistor if md.typ == Type::Electric => atk_stat = crate::damage::modify(atk_stat, 5325, 4096), // ×1.3
            Ab::DragonsMaw if md.typ == Type::Dragon => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::RockyPayload if md.typ == Type::Rock => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Steelworker if md.typ == Type::Steel => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            // Flash Fire: ×1.5 to the holder's Fire moves once activated (the `flashfire`
            // volatile is present). PS `onModifyAtk`/`onModifySpA` in the ability's condition.
            Ab::FlashFire
                if md.typ == Type::Fire
                    && b.state.side(side).volatiles.contains(VolatileStatus::FlashFire) =>
            {
                atk_stat = crate::damage::modify(atk_stat, 3, 2)
            }
            _ => {}
        }
        // Thick Fat (defender) halves the attack of Fire/Ice moves; Water Bubble (defender)
        // halves Fire-move attack.
        if def_ab == Ab::ThickFat && (md.typ == Type::Fire || md.typ == Type::Ice) {
            atk_stat = crate::damage::modify(atk_stat, 1, 2);
        }
        if def_ab == Ab::WaterBubble && md.typ == Type::Fire {
            atk_stat = crate::damage::modify(atk_stat, 1, 2);
        }
        atk_stat
    };
    let finalize_def = |boost: i8| -> i64 {
        let mut def_stat = boosted_stat(defender.stat(def_idx) as i64, boost);
        // Ruin abilities and Assault Vest key on the defensive STAT actually used (Def vs SpD),
        // not the move category — so an overrideDefensiveStat move (Psyshock) is treated by its
        // physical Defense: Sword of Ruin / Fur Coat / Snow / Marvel Scale apply, while Beads of
        // Ruin / Assault Vest (SpD modifiers) do not.
        if sword && def_idx == crate::ids::StatIndex::Defense {
            def_stat = crate::damage::modify(def_stat, 3, 4);
        }
        if beads && def_idx == crate::ids::StatIndex::SpecialDefense {
            def_stat = crate::damage::modify(def_stat, 3, 4);
        }
        if defender.item == Item::AssaultVest && def_idx == crate::ids::StatIndex::SpecialDefense {
            def_stat = crate::damage::modify(def_stat, 3, 2);
        }
        // Eviolite: ×1.5 to the defensive stat (Def and SpD) of a not-fully-evolved Pokémon.
        if defender.item == Item::Eviolite && crate::data::species_is_nfe(defender.species) {
            def_stat = crate::damage::modify(def_stat, 3, 2);
        }
        if proto_def {
            def_stat = crate::damage::modify(def_stat, 5325, 4096);
        }
        // Weather defensive boosts: Sandstorm ×1.5 SpD for Rock types, Snow ×1.5 Def for Ice.
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
        // Marvel Scale / Fur Coat (defender) raise physical Defense.
        if md.category == MoveCategory::Physical && def_idx == crate::ids::StatIndex::Defense {
            if def_ab == Ab::FurCoat {
                def_stat = crate::damage::modify(def_stat, 2, 1);
            } else if def_ab == Ab::MarvelScale && defender.status != Status::None {
                def_stat = crate::damage::modify(def_stat, 3, 2);
            }
        }
        def_stat
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
    let type_mult = crate::damage::type_multiplier(md.typ, def_types_eff);
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
    let tera_shell = def_ab == Ab::TeraShell
        && defender.hp == defender.max_hp
        && crate::ids::Species::from_id("terapagosterastal") == Some(defender.species);
    // Returned for post-damage (contact punishers); also suppressed under Mold Breaker.
    let def_ability = def_ab;
    let def_item = defender.item;
    let def_maxhp = defender.max_hp;
    let sheer_force_active = attacker.ability == Ab::SheerForce
        && (md.secondary_chance > 0 || md.flinch_chance > 0
            || md.secondary_self_boosts.iter().any(|&x| x != 0)
            // Tri Attack's secondary is a sample-based onHit that the move table can't encode,
            // so it isn't reflected in `secondary_chance`.
            || md.id.to_id() == "triattack");
    // Life Orb's ×1.3 DAMAGE (onModifyDamage) always applies while held; Sheer Force only
    // suppresses the RECOIL (onAfterMoveSecondarySelf). Keep the two flags separate.
    let life_orb = attacker.item == Item::LifeOrb;
    let life_orb_recoil = life_orb && !sheer_force_active;
    if life_orb {
        fmod = chain_final(fmod, 5324);
    }

    // Knock Off: ×1.5 base power when the target is holding a REMOVABLE item — no boost when
    // the item is species-locked (Rusted Sword/Shield, Ogerpon masks, Origin orbs) or the
    // holder has Sticky Hold (PS's basePowerCallback runs the TakeItem event first).
    let mut base_power = if md.id.to_id() == "knockoff"
        && defender.item != Item::None
        && item_removable(defender.species, defender.item)
        && def_ab != Ab::StickyHold // breakable → def_ab is already Mold-Breaker-suppressed
    {
        crate::damage::modify(md.base_power as i64, 3, 2) as u16
    } else {
        md.base_power
    };
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
        // Collision Course / Electro Drift: ~x1.33 when super effective.
        "collisioncourse" | "electrodrift"
            if crate::damage::type_multiplier(md.typ, b.state.side(side.other()).active().types) > 1.0 =>
        {
            base_power = crate::damage::modify(base_power as i64, 5461, 4096) as u16;
        }
        // Psyblade: x1.5 in Electric Terrain (any user; terrain applies to the field).
        "psyblade" if b.state.terrain == crate::ids::Terrain::Electric => {
            base_power = crate::damage::modify(base_power as i64, 6144, 4096) as u16;
        }
        // Expanding Force: x1.5 when its GROUNDED user attacks in Psychic Terrain.
        "expandingforce"
            if b.state.terrain == crate::ids::Terrain::Psychic && is_grounded(&b.state, side) =>
        {
            base_power = crate::damage::modify(base_power as i64, 6144, 4096) as u16;
        }
        _ => {}
    }
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
    if type_item_boost || orb_boost {
        base_power = crate::damage::modify(base_power as i64, 4915, 4096) as u16;
    }
    // Soul Dew: ×1.2 base power for Latias/Latios Psychic- and Dragon-type moves (PS `onBasePower`).
    if attacker.item == Item::SoulDew
        && matches!(attacker.species, sp if sp == crate::ids::Species::from_id("latias").unwrap_or(crate::ids::Species::None)
            || sp == crate::ids::Species::from_id("latios").unwrap_or(crate::ids::Species::None))
        && matches!(md.typ, Type::Psychic | Type::Dragon)
    {
        base_power = crate::damage::modify(base_power as i64, 4915, 4096) as u16;
    }

    // Sheer Force: x1.3 base power for moves with any secondary (which is then removed).
    if sheer_force_active {
        base_power = crate::damage::modify(base_power as i64, 5325, 4096) as u16;
    }
    // Technician is an onBasePower modifier in PS.  This placement matters for rounding and
    // for variable-power moves such as Triple Axel, whose 20/40/60-power hits are evaluated
    // independently.  Applying it to Attack produces a different damage support.
    if attacker.ability == Ab::Technician && base_power <= 60 {
        base_power = crate::damage::modify(base_power as i64, 6144, 4096) as u16;
    }
    // Strong Jaw is an onBasePower modifier in PS. Applying it to Attack changes rounding
    // support (caught by the exact Crunch kernel on Strong Jaw Bruxish).
    if attacker.ability == Ab::StrongJaw && md.flag_bite {
        base_power = crate::damage::modify(base_power as i64, 6144, 4096) as u16;
    }
    // Sharpness: ×1.5 base power for slicing moves (PS `onBasePower`). On base power, not the
    // attack stat, so the floor lands where PS's does under a ×0.5 type matchup.
    if attacker.ability == Ab::Sharpness && md.flag_slicing {
        base_power = crate::damage::modify(base_power as i64, 6144, 4096) as u16;
    }
    // Iron Fist: ×1.2 base power for punch moves (PS `onBasePower`). Moved off the attack
    // stat when a cosim Ice Hammer unit exposed the rounding difference.
    if attacker.ability == Ab::IronFist && md.flag_punch {
        base_power = crate::damage::modify(base_power as i64, 4915, 4096) as u16;
    }
    // Punk Rock: ×1.3 base power for sound moves (PS `onBasePower`). Applied to base power
    // rather than the attack stat so the floor lands where PS's does.
    if attacker.ability == Ab::PunkRock && md.flag_sound {
        base_power = crate::damage::modify(base_power as i64, 5325, 4096) as u16;
    }
    // Ogerpon masks (Hearthflame / Wellspring / Cornerstone) give ×1.2 base power to all of
    // Ogerpon's moves (PS `onBasePower`). Only Ogerpon holds them.
    if matches!(attacker.item, Item::HearthflameMask | Item::WellspringMask | Item::CornerstoneMask) {
        base_power = crate::damage::modify(base_power as i64, 4915, 4096) as u16;
    }
    // Supreme Overlord: +10% base power per fallen ally — PS uses an exact 4096 lookup table
    // (onBasePower), not 1+0.1·n, so this matches its rounding precisely.
    if attacker.ability == Ab::SupremeOverlord {
        let fallen = b.state.side(side).pokemon.iter()
            .filter(|p| p.species != crate::ids::Species::None && p.hp <= 0)
            .count()
            .min(5);
        if fallen > 0 {
            const POW: [i64; 6] = [4096, 4506, 4915, 5325, 5734, 6144];
            base_power = crate::damage::modify(base_power as i64, POW[fallen], 4096) as u16;
        }
    }
    // Terrain base-power modifiers (gen9, grounded users/targets; ×1.3 = chainModify 5325).
    // The terrain is part of the projected state, so terrain-setting abilities/moves needn't
    // be modeled here. Grounded ≈ not Flying and not Levitate (Air Balloon etc. unmodeled).
    use crate::ids::Terrain;
    let atk_grounded = !attacker.types.contains(&Type::Flying) && attacker.ability != Ab::Levitate;
    let def_grounded = !defender.types.contains(&Type::Flying) && defender.ability != Ab::Levitate;
    let terrain_boost = atk_grounded
        && matches!(
            (b.state.terrain, md.typ),
            (Terrain::Electric, Type::Electric) | (Terrain::Grassy, Type::Grass) | (Terrain::Psychic, Type::Psychic)
        );
    if terrain_boost {
        base_power = crate::damage::modify(base_power as i64, 5325, 4096) as u16;
    }
    // Grassy Terrain halves the ground-shaking moves vs grounded targets; Misty Terrain
    // halves Dragon moves vs grounded targets.
    let terrain_halve = (b.state.terrain == Terrain::Grassy
        && def_grounded
        && matches!(md.id.to_id(), "earthquake" | "bulldoze" | "magnitude"))
        || (b.state.terrain == Terrain::Misty && def_grounded && md.typ == Type::Dragon);
    if terrain_halve {
        base_power = crate::damage::modify(base_power as i64, 2048, 4096) as u16;
    }
    // Terastallization STAB floor: a terastallized mon's move matching its (post-Tera) type with
    // base power < 60 is raised to 60 — applied AFTER every onBasePower modifier. Excludes
    // priority moves, multi-hit moves, and variable-power moves whose dex base power is 0 or 150
    // (Dragon Energy / Eruption / Water Spout, …). Stellar Tera uses a different rule (skipped).
    if attacker.terastallized
        && attacker.tera_type != Type::Stellar
        && attacker.types.contains(&md.typ)
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
        attacker_base_types: crate::data::species_types(attacker.species),
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
        tera_type: attacker.tera_type,
        life_orb: false,
        adaptability,
        tera_shell,
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
fn ice_face_is_intact(b: &Branch, foe: SideId, md: &crate::data::MoveData) -> bool {
    let p = b.state.side(foe).active();
    md.category == MoveCategory::Physical
        && p.ability == crate::ids::Ability::IceFace
        && p.species == crate::ids::Species::from_id("eiscue").unwrap_or(crate::ids::Species::None)
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

fn apply_damage_hit(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hits: &[(u8, bool)], crit_den: i32) -> bool {
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
    for &(roll, crit) in hits {
        // PS's `hitStepMoveHitLoop` checks `targets.every(!hp)` at the TOP of each iteration,
        // BEFORE that hit's crit/damage rolls (which happen inside `spreadMoveHit` → `getDamage`).
        // So once the target has fainted, no further crit/damage draws are rolled. Emit the
        // per-hit draws here — after this KO check, before applying the hit — so a multi-hit move
        // that KOs early stops the draw stream exactly where PS does (fixes phantom-hit over-roll).
        // A user faint mid-loop (recoil/contact item) is folded into `apply_post_damage`, so it
        // never truncates the loop here; the corpus has no such multi-hit matchup.
        if b.state.side(foe).active().hp <= 0 {
            break;
        }
        if crit_den > 0 {
            draw(b, "randomChance", &[1, crit_den], crit as i64, "crit");
        }
        draw(b, "random", &[16], roll as i64, "damage-roll");
        // ModifyDamage screen-tie shuffle (per getDamage, after the damage roll).
        emit_modifydamage_shuffle(b);
        let rolls = if crit { &calc.rolls_crit } else { &calc.rolls_nocrit };
        let raw = rolls[roll as usize];
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
    }
    // PS's `timesAttacked += hit - 1` counts every EXECUTED hit — including ones a Substitute
    // absorbed and the KO-ing hit (nothing after a faint executes) — but only when at least
    // one hit connected with the Pokémon itself (a fully sub-absorbed move records nothing:
    // its per-target damage entry stays `false`). Verified against the pin empirically.
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, times_count, life_orb, def_item, def_ability);
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
    for (i, &(roll, crit)) in hits.iter().enumerate() {
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
        // Indexed moves change power each hit, and Ice Face can change Defense between hits.
        let noice_calc = if b.state.side(foe).active().species == crate::ids::Species::from_id("eiscuenoice").unwrap_or(crate::ids::Species::None) {
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
    }
    // PS's `timesAttacked += hit - 1` counts every EXECUTED hit — including ones a Substitute
    // absorbed and the KO-ing hit (nothing after a faint executes) — but only when at least
    // one hit connected with the Pokémon itself (a fully sub-absorbed move records nothing:
    // its per-target damage entry stays `false`). Verified against the pin empirically.
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, times_count, life_orb, def_item, def_ability);
    hit_sub
}

/// Effects keyed on the *total* damage a move dealt: drain, move recoil, Life Orb recoil,
/// contact punishers (Rocky Helmet / Rough Skin / Iron Barbs), and Toxic Debris. Shared by
/// the exact per-hit path and the multi-hit sumset-DP path so both stay in lockstep.
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
) {
    use crate::ids::Ability as Ab;
    let foe = side.other();
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
        // Rock Head and Magic Guard prevent recoil.
        let recoil_immune = matches!(b.state.side(side).active().ability, Ab::RockHead | Ab::MagicGuard);
        if md.recoil.0 > 0 && !recoil_immune {
            let atk = b.state.side(side).active();
            if atk.is_alive() {
                let rec = (round_div(total_dealt * md.recoil.0 as i32, md.recoil.1 as i32) as i16).max(1).min(atk.hp);
                push(b, Instruction::Damage { side, slot: aslot, amount: rec });
            }
        }
        // Life Orb recoil: 10% of the attacker's max HP, once after a damaging move.
        if life_orb {
            let atk = b.state.side(side).active();
            if atk.is_alive() {
                let recoil = (atk.max_hp / 10).max(1).min(atk.hp);
                push(b, Instruction::Damage { side, slot: aslot, amount: recoil });
            }
        }
        // Contact punishers: Rough Skin / Iron Barbs (1/8, ability onDamagingHit) AND Rocky
        // Helmet (1/6, item) — PS runs BOTH when the holder has ability + item (the c5
        // directed traces caught the engine applying only one).
        if md.flag_contact && !hit_sub {
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
        // Electromorphosis charges the holder after any damaging hit. The Charge volatile
        // doubles its next Electric move and is consumed by that move.
        if !hit_sub
            && def_ability == Ab::Electromorphosis
            && b.state.side(foe).active().is_alive()
            && !b.state.side(foe).volatiles.contains(VolatileStatus::Charge)
        {
            push(b, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Charge });
        }
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
    {
        let aslot = b.state.side(side).active_index;
        let atk = b.state.side(side).active();
        let dmg = (atk.max_hp / 4).max(1).min(atk.hp);
        push(b, Instruction::Damage { side, slot: aslot, amount: dmg });
    }

    // Toxic Debris: a physical hit on the holder scatters a Toxic Spikes layer onto the
    // attacker's side (up to 2).
    if md.category == MoveCategory::Physical
        && b.state.side(foe).active().ability == crate::ids::Ability::ToxicDebris
    {
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

    // Defender on-hit reaction abilities (only when the hit connected with the mon itself).
    if any_damage && !hit_sub {
        maybe_eat_sitrus(b, foe);
        let f = b.state.side(foe).active();
        if f.is_alive() {
            use crate::ids::Ability as Ab;
            match f.ability {
                Ab::Stamina => {
                    raise_boost(b, foe, BoostIndex::Defense, 1);
                }
                Ab::WaterCompaction if md.typ == Type::Water => {
                    raise_boost(b, foe, BoostIndex::Defense, 2);
                }
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
                    push(b, Instruction::ChangeTerrain {
                        previous: b.state.terrain,
                        previous_turns: b.state.terrain_turns,
                        new: crate::ids::Terrain::Grassy,
                        new_turns: turns,
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
        // Soul-Heart: +1 SpA whenever a Pokémon faints from this hit.
        if !b.state.side(foe).active().is_alive()
            && b.state.side(side).active().ability == crate::ids::Ability::SoulHeart
            && b.state.side(side).active().is_alive()
        {
            raise_boost(b, side, BoostIndex::SpecialAttack, 1);
        }
    }

    // Knock Off removes the target's held item (so it no longer triggers Leftovers heals
    // etc.) — unless the item is species-locked to the holder (PS onTakeItem false) or the
    // holder has Sticky Hold (suppressed by Mold Breaker, but def_ability reflects that).
    if md.id.to_id() == "knockoff" && !hit_sub {
        let f = b.state.side(foe).active();
        if f.is_alive()
            && f.item != Item::None
            && item_removable(f.species, f.item)
            && def_ability != Ab::StickyHold
        {
            let (prev, fslot) = (f.item, b.state.side(foe).active_index);
            push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: prev, new: Item::None });
            on_item_lost(b, foe);
            // Knocking the item off reveals what it was.
            reveal(b, foe, 0, crate::state::Reveal::ITEM);
        }
    }

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
        if f.item != Item::None && item_removable(f.species, f.item) && def_ability != Ab::StickyHold {
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
            && item_removable(a.species, a.item)
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

    // A transformed mon that fainted this hit (the target, or the attacker via a contact
    // punisher / recoil) reverts to its own identity — PS runs clearVolatile on faint.
    // A fainting mon also releases the Mean-Look-family `trapped` it was holding the OTHER
    // side in — PS's `trapped` is linked to the trapper's `trapper` volatile, and linked
    // volatiles are removed the moment their partner clears on faint (before residuals).
    // Likewise a faint releases the OPPONENT's infatuation (Attract's source is gone).
    if !b.state.side(foe).active().is_alive() {
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

/// A frozen target thaws when hit by a damaging Fire-type move or one of the `thawsTarget`
/// moves (Scald / Steam Eruption / Matcha Gotcha / Hydro Steam — PS frz `onHit`).
fn apply_thaw_on_hit(b: &mut Branch, foe: SideId, md: &crate::data::MoveData) {
    let d = b.state.side(foe).active();
    let thaws = (md.typ == Type::Fire && md.category != MoveCategory::Status)
        || matches!(md.id.to_id(), "scald" | "steameruption" | "matchagotcha" | "hydrosteam");
    if d.is_alive() && d.status == Status::Freeze && thaws {
        let slot = b.state.side(foe).active_index;
        push(b, Instruction::ChangeStatus { side: foe, slot, previous: Status::Freeze, new: Status::None });
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
/// differ in every other base stat. Random-battle spread (31 IV / 85 EV / neutral) assumed
/// for the recompute — directed teamsets must use that spread.
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
    let level = p.level;
    let base = crate::data::base_stats(target_forme);
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
    new.species = target_forme;
    new.stats = stats;
    // The formes differ in typing too (Aria Normal/Psychic, Pirouette Normal/Fighting); a
    // terastallized Meloetta keeps its tera typing.
    if !p.terastallized {
        new.types = crate::data::species_types(target_forme);
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
        && crate::damage::type_multiplier(md.typ, d.types) > 1.0
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
            calc.life_orb, calc.def_item, calc.def_ability,
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
            damaging_hits, noice_calc.life_orb, noice_calc.def_item, noice_calc.def_ability,
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
        apply_post_damage(&mut hb, side, md, total, total > 0, false, hits, calc.life_orb, calc.def_item, calc.def_ability);
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
            only_hit_substitute, times_count, calc.life_orb, calc.def_item, calc.def_ability);
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
    // Peek each hit's crit + damage roll in PS order; `apply_damage_hit` emits them (and the
    // per-hit ModifyDamage shuffle) and applies with KO/Substitute-break termination — a hit past
    // a faint never executes, so its peeked (over-read) values are simply unused.
    let crit_den = ps_crit_den(&hb, side, md);
    let mut hits: Vec<(u8, bool)> = Vec::with_capacity(count);
    for _ in 0..count {
        let crit = crit_den > 0 && cur.peek("randomChance", &[1, crit_den]) != 0;
        let roll = cur.peek("random", &[16]) as u8;
        hits.push((roll & 0x0F, crit));
    }
    let hit_sub = apply_damage_hit(&mut hb, side, md, &hits, crit_den);
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
    calcs: &[DamageCalc], mut cur: RealizedCursor,
) -> Vec<(Branch, bool)> {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let mut hb = scaled(b, hit_prob);
    let loaded = hb.state.side(side).active().item == Item::LoadedDice;
    let multiacc = !loaded; // Loaded Dice deletes multiaccuracy → no per-hit accuracy roll
    // Hit count: fixed (Triple Axel/Kick 3, Population Bomb 10); Loaded Dice Population Bomb rolls
    // `10 - random(7)` (battle-actions.ts:877).
    let mut count = md.hits_max as usize;
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
    for i in 0..count {
        // PS breaks the loop at the TOP once the target has fainted (before any hit draw).
        if hb.state.side(foe).active().hp <= 0 {
            break;
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
        let calc = &calcs[i.min(calcs.len() - 1)];
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
    }
    let times_count = if hits_landed > 0 { hits_executed } else { 0 };
    apply_post_damage(&mut hb, side, md, total, any_damage, hit_sub, times_count, life_orb, def_item, def_ability);
    vec![(hb, hit_sub)]
}

/// Beat Up: one hit per eligible party member (PS `onModifyMove` filter: the user always, plus
/// any ally that is neither fainted nor statused), in party order. Each hit's base power is
/// `5 + floor(species base Atk / 10)` of that member, but the damage otherwise uses the USER's
/// Attack vs the target's Defense (Dark, physical, no contact). We convolve the per-hit damage
/// distributions (each 16 rolls × crit) in order, tracking `(sub_remaining, mon_damage, landed
/// hits, sash_used)` so Substitute break, early faint, and Sturdy/Focus Sash stay exact.
fn apply_beatup(b: &Branch, side: SideId, md: &crate::data::MoveData, hit_prob: f32) -> Vec<(Branch, bool)> {
    use std::collections::HashMap;
    let foe = side.other();
    // Participating party members (party order): the user always, plus alive, status-free allies.
    let mut calcs: Vec<DamageCalc> = Vec::new();
    {
        let s = b.state.side(side);
        for i in 0..6usize {
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
            calcs.push(compute_damage(b, side, &m));
        }
    }
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
            only_hit_substitute, times_count, calcs[0].life_orb, calcs[0].def_item, calcs[0].def_ability);
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
        on_item_lost(b, side);
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
    let p = b.state.side(side).active();
    if p.item == Item::SitrusBerry && p.is_alive() && p.hp * 2 <= p.max_hp {
        let slot = b.state.side(side).active_index;
        let amt = (p.max_hp / 4).max(1).min(p.max_hp - p.hp);
        push(b, Instruction::Heal { side, slot, amount: amt });
        push(b, Instruction::ChangeItem { side, slot, previous: Item::SitrusBerry, new: Item::None });
        on_berry_eaten_id(b, side, Item::SitrusBerry);
    }
}

fn on_berry_eaten(b: &mut Branch, side: SideId) {
    on_berry_eaten_id(b, side, Item::None)
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

/// Apply a stat-stage change to the target, respecting Clear Body (blocks reductions) and
/// the ±6 clamp. Returns the effective change actually applied (0 if blocked/clamped out).
fn apply_boost_clamped(b: &mut Branch, target: SideId, stat: BoostIndex, delta: i8) -> i8 {
    use crate::ids::Ability as Ab;
    // Contrary inverts the change before anything else (so a "drop" becomes a raise and is no
    // longer blocked by Clear Body / counted as a drop by Defiant).
    let delta = if b.state.side(target).active().ability == Ab::Contrary { -delta } else { delta };
    // Every apply_boost_clamped call is an OPPONENT-inflicted change on `target` (self-drops go
    // through apply_self_boost), so the "source && target === source" self-skip in PS's onTryBoost
    // handlers never fires here. Protective abilities block foe-inflicted stat *drops*.
    if delta < 0 {
        let tgt = b.state.side(target).active();
        let ab = tgt.ability;
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
}

/// Split a contact hit on a contact-triggered status ability (30%): the defender's Flame
/// Body / Static / Poison Point statuses the attacker, or the attacker's Poison Touch
/// poisons the target. Only one (the first applicable) is modeled; no-op off contact.
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
        if !a.is_alive() || powder_immune {
            return vec![b];
        }
        let mut out = Vec::new();
        let mut noproc_p = 0.70f32;
        // PS rolls ONE `this.random(100)`: <11 slp, <21 par, <30 psn, else nothing — emitted on
        // every branch out of this ability (draw-and-discard; result isn't compared, state is).
        for (p, status, res) in [(0.11, Status::Sleep, 0i64), (0.10, Status::Paralysis, 11), (0.09, Status::Poison, 21)] {
            let applies = status_applies(b.state.side(side).active(), status)
                && !status_blocked_by_field(&b.state, side, status)
                && !(status == Status::Sleep && sleep_clause_blocks(&b.state, side));
            if !applies {
                noproc_p += p; // the roll lands there but the status fails: no state change
                continue;
            }
            let mut proc = scaled(&b, p);
            draw(&mut proc, "random", &[100], res, "effectspore");
            let slot = proc.state.side(side).active_index;
            push(&mut proc, Instruction::ChangeStatus { side, slot, previous: Status::None, new: status });
            if status == Status::Sleep {
                mark_slept_by_foe(&mut proc, side);
                out.extend(branch_sleep_counter(proc, side));
            } else {
                out.push(proc);
            }
        }
        let mut np = scaled(&b, noproc_p);
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
    // Toxic Chain (attacker) badly-poisons the target on any damaging hit (30%, not contact-
    // gated). Otherwise a contact hit can trigger the defender's status ability or the
    // attacker's Poison Touch.
    let (target, status, poison_touch) = if atk_ab == Ab::ToxicChain {
        (foe, Status::Toxic, false)
    } else if !md.flag_contact {
        return vec![b];
    } else {
        match def_ab {
            Ab::FlameBody => (side, Status::Burn, false),
            Ab::Static => (side, Status::Paralysis, false),
            Ab::PoisonPoint => (side, Status::Poison, false),
            _ if atk_ab == Ab::PoisonTouch => (foe, Status::Poison, true),
            _ => return vec![b],
        }
    };
    // Poison Touch is the ONE contact ability that PS gates on the target's Shield Dust /
    // Covert Cloak: it returns BEFORE the roll, so no `randomChance` at all (the other contact
    // abilities status the attacker and ignore its Shield Dust). PS ref: abilities.ts poisontouch.
    if poison_touch {
        let t = b.state.side(target).active();
        if t.ability == Ab::ShieldDust || t.item == Item::CovertCloak {
            return vec![b];
        }
    }
    // PS `onDamagingHit` / `onSourceDamagingHit` rolls `randomChance(3, 10)` on every qualifying
    // hit, INDEPENDENT of whether the status can be applied (`trySetStatus` runs inside the proc
    // branch). When it can't land — target already statused / type-immune / fainted this hit —
    // the roll is a single draw-and-discard. The engine previously skipped the draw entirely.
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
    // PS contact-status abilities (Static/Flame Body/Poison Point) and Poison Touch/Toxic Chain
    // roll `randomChance(3, 10)` in their onDamagingHit handler.
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
    if md.secondary_chance > 0 || extra_secondary_roll_move(md.id) {
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
    let shielded = alive
        && (d.ability == crate::ids::Ability::ShieldDust || d.item == Item::CovertCloak);
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
        draw(&mut b, "randomChance", &[3, 10], 3, "cursedbody");
        return vec![b];
    }
    let mut proc = scaled(&b, 0.30);
    draw(&mut proc, "randomChance", &[3, 10], 0, "cursedbody");
    let mut noproc = scaled(&b, 0.70);
    draw(&mut noproc, "randomChance", &[3, 10], 3, "cursedbody");
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
    )
}

fn apply_target_secondary(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    let mut b = b;
    // 100%-secondary moves the engine applies through `target_volatile`/a dedicated handler
    // (secondary_chance == 0) still cost PS one `random(100)` at the secondaries site.
    if extra_secondary_roll_move(md.id)
        && b.state.side(side).active().ability != crate::ids::Ability::SheerForce
    {
        let foe = side.other();
        let alive = b.state.side(foe).active().is_alive();
        let shielded = alive
            && (b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
                || b.state.side(foe).active().item == Item::CovertCloak);
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
    // `ModifySecondaries` BEFORE the `random(100)` roll — so no draw at all. The shield is only
    // active while the mon is on the field alive (a fainted mon's ability is inert).
    let shielded = alive
        && (b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak);
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
    let mut lowered = false;
    if target_eligible {
        for (i, &delta) in md.secondary_boosts.iter().enumerate() {
            if delta != 0 {
                lowered |= apply_boost_clamped(&mut proc, foe, BOOST_ORDER[i], delta) < 0;
            }
        }
    }
    if lowered {
        react_to_stat_drop(&mut proc, foe);
        apply_white_herb(&mut proc, foe);
    }
    let mut applied_sleep = false;
    let mut applied_status_now = false;
    if target_eligible
        && md.secondary_status != Status::None
        && status_applies_src(proc.state.side(foe).active(), md.secondary_status,
            proc.state.side(side).active().ability == crate::ids::Ability::Corrosion,
            matches!(proc.state.side(side).active().ability,
                crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze))
        && !status_blocked_by_field(&proc.state, foe, md.secondary_status)
        && !(md.secondary_status == Status::Sleep && sleep_clause_blocks(&proc.state, foe))
    {
        let slot = proc.state.side(foe).active_index;
        push(&mut proc, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.secondary_status });
        applied_status_now = true;
        applied_sleep = md.secondary_status == Status::Sleep;
        if applied_sleep {
            mark_slept_by_foe(&mut proc, foe);
        }
        apply_synchronize(&mut proc, foe, md.secondary_status);
        consume_lum_if_statused(&mut proc, foe);
        applied_sleep = applied_sleep && proc.state.side(foe).active().status == Status::Sleep;
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
    let shielded = alive
        && (b.state.side(foe).active().ability == Ab::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak);
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
    let shielded = alive
        && (b.state.side(foe).active().ability == Ab::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak);
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
        let can_apply = alive
            && status_applies_src(pb.state.side(foe).active(), status, corrosion, breaker)
            && !status_blocked_by_field(&pb.state, foe, status)
            && !(status == Status::Sleep && sleep_clause_blocks(&pb.state, foe));
        if can_apply {
            let slot = pb.state.side(foe).active_index;
            push(&mut pb, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: status });
            let slept = status == Status::Sleep && pb.state.side(foe).active().status == Status::Sleep;
            if slept {
                mark_slept_by_foe(&mut pb, foe);
            }
            apply_synchronize(&mut pb, foe, status);
            consume_lum_if_statused(&mut pb, foe);
            if slept && pb.state.side(foe).active().status == Status::Sleep {
                // Freshly-applied sleep rolls its `random(2,5)` duration at the slp `onStart`.
                out.extend(branch_sleep_counter(pb, foe));
                continue;
            }
        }
        out.push(pb);
    }
    out
}

/// A damaging move's deterministic on-hit effects: user self-boosts and any target
/// volatile (Salt Cure, etc.).
fn apply_damage_secondaries(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hit_sub: bool) {
    // PS `selfDrops` (battle-actions.ts): a connecting move with `move.self.boosts` (Close
    // Combat −Def/−SpD, Draco Meteor −SpA, Rapid Spin +Spe, Make It Rain −SpA, …) rolls a
    // `random(100)` draw-and-discard even at a guaranteed 100% self-drop (these have no
    // `self.chance`, so the boost always applies regardless of the roll). The roll is consumed
    // after the damage rolls and *before* the target secondaries (`selfDrops` precedes
    // `secondaries` in `spreadMoveHit`). Self-boosts apply even through a Substitute.
    if md.self_boosts.iter().any(|&x| x != 0) {
        draw(b, "random", &[100], 0, "self-drop");
    }
    // Self-boosts apply even through a Substitute (they affect the attacker).
    for (i, &delta) in md.self_boosts.iter().enumerate() {
        if delta != 0 {
            apply_self_boost(b, side, BOOST_ORDER[i], delta);
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
        let blocked = b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
            || b.state.side(foe).active().item == Item::CovertCloak;
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
            if v != VolatileStatus::PartiallyTrapped && !b.state.side(foe).volatiles.contains(v) {
                push(b, Instruction::ApplyVolatile { side: foe, volatile: v });
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
fn execute_status_move(mut b: Branch, side: SideId, md: &crate::data::MoveData, foe_moves_later: bool) -> Vec<Branch> {
    let foe = side.other();

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
        if alive && !ghost {
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

    // Haze: reset every stat stage on both actives to 0.
    if md.id.to_id() == "haze" {
        let mut b = b;
        for s in [SideId::One, SideId::Two] {
            for stat in BOOST_ORDER {
                let cur = b.state.side(s).boost(stat);
                if cur != 0 {
                    push(&mut b, Instruction::Boost { side: s, stat, amount: -cur });
                }
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
        if b.state.side(foe2).active().is_alive() {
            if apply_boost_clamped(&mut b, foe2, BoostIndex::Evasion, -1) < 0 {
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
            push(&mut b, Instruction::ChangeTerrain {
                previous: prev_t,
                previous_turns: prev_tt,
                new: crate::ids::Terrain::None,
                new_turns: 0,
            });
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
        if b.state.side(foe2).active().is_alive() && !sticky && (mine != Item::None || theirs != Item::None) {
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
            // rolls `randomChance(100, 100)` before the drain/drop. (Special-cased above the
            // general status accuracy branch, so emit it here.)
            draw(&mut b, "randomChance", &[md.accuracy as i32, 100], 1, "accuracy");
            let atk_val = {
                let t = b.state.side(foe).active();
                let boost = b.state.side(foe).boost(BoostIndex::Attack);
                (t.stat(crate::ids::StatIndex::Attack) as f32 * boost_multiplier(boost)) as i16
            };
            let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
            let amount = atk_val.min(maxhp - hp);
            if amount > 0 {
                let slot = b.state.side(side).active_index;
                push(&mut b, Instruction::Heal { side, slot, amount });
            }
            if apply_boost_clamped(&mut b, foe, BoostIndex::Attack, -1) < 0 {
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
    // inherit it. (Accuracy/evasion stages and accuracy-boosting abilities/items shift the
    // recorded arg away from the raw accuracy; unmodeled here — the differ flags those.)
    if md.accuracy != 0
        && md.target != crate::data::MoveTarget::User
        && !(md.id.to_id() == "toxic" && b.state.side(side).active().types.contains(&Type::Poison))
        && !accuracy_forced_true(&b, side, md)
    {
        draw(&mut b, "randomChance", &[md.accuracy as i32, 100], (hit_prob > 0.0) as i64, "accuracy");
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
        let mut lowered = false;
        for (i, &delta) in md.target_boosts.iter().enumerate() {
            if delta != 0 {
                lowered |= apply_boost_clamped(&mut hit, foe, BOOST_ORDER[i], delta) < 0;
            }
        }
        if lowered {
            react_to_stat_drop(&mut hit, foe);
            apply_white_herb(&mut hit, foe);
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
    if md.status != Status::None
        && !foe_immune
        && status_applies_src(hit.state.side(foe).active(), md.status,
            hit.state.side(side).active().ability == crate::ids::Ability::Corrosion,
            status_breaker)
        && !status_blocked_by_field(&hit.state, foe, md.status)
        && !(md.status == Status::Sleep && sleep_clause_blocks(&hit.state, foe))
    {
        let slot = hit.state.side(foe).active_index;
        push(&mut hit, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.status });
        applied_sleep = md.status == Status::Sleep;
        if applied_sleep {
            mark_slept_by_foe(&mut hit, foe);
        }
        apply_synchronize(&mut hit, foe, md.status);
        consume_lum_if_statused(&mut hit, foe);
        applied_sleep = applied_sleep && hit.state.side(foe).active().status == Status::Sleep;
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
        hits.push(scaled(&b, miss_prob));
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

/// Set the field weather (and its duration).
fn set_weather(b: &mut Branch, weather: Weather, turns: i8) {
    push(b, Instruction::ChangeWeather {
        previous: b.state.weather,
        previous_turns: b.state.weather_turns,
        new: weather,
        new_turns: turns,
    });
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
    let bench: Vec<u8> = (0..6u8)
        .filter(|&i| {
            i != sd.active_index
                && sd.pokemon[i as usize].species != crate::ids::Species::None
                && sd.pokemon[i as usize].is_alive()
        })
        .collect();
    if bench.is_empty() {
        return vec![b];
    }
    // PS picks the drag target with `sample(possibleSwitches)` (battle.ts getRandomSwitchable):
    // one `sample` draw over the bench in party order (`side.pokemon` from active.length, fainted
    // skipped). Each branch carries the drawn index as its `sample` result so the Replicate filter
    // selects the realized target. Annotation-only (state-neutral to Enumerate/Sample).
    let n = bench.len() as i32;
    let p = 1.0 / bench.len() as f32;
    bench
        .into_iter()
        .enumerate()
        .map(|(idx, t)| {
            let mut nb = scaled(&b, p);
            draw(&mut nb, "sample", &[n], idx as i64, "drag");
            apply_switch(&mut nb, dragged, t);
            nb
        })
        .collect()
}

/// The 16 damage rolls of a landing Future Sight / Doom Desire: computed at hit time from the
/// stored caster's Special Attack vs the target's current Special Defense. (Approximation: no
/// crit branch, no caster boosts.)
fn future_sight_rolls(state: &State, target_side: SideId, caster_slot: u8) -> [i16; 16] {
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
        attacker_base_types: caster.base_types,
        defender_types: target.types,
        attack_stat: caster.stat(crate::ids::StatIndex::SpecialAttack),
        defense_stat: target.stat(crate::ids::StatIndex::SpecialDefense).max(1),
        is_crit: false,
        attacker_burned: false,
        weather: state.weather,
        terastallized: caster.terastallized,
        tera_type: caster.tera_type,
        life_orb: false,
        adaptability: false,
        tera_shell: false,
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
        if !p.is_alive() {
            continue;
        }
        let speed = effective_speed(state, side) as i64;
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
        if v.contains(V::Roost) { push(25, 2); }
        // Wish resolves as a slot condition on the occupant (order 4, subOrder 3).
        if s.wish.0 > 0 { push(4, 3); }
        // Items.
        match p.item {
            It::Leftovers | It::BlackSludge => push(5, 4),
            It::ToxicOrb | It::FlameOrb | It::StickyBarb => push(28, 3),
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
            Ab::Hydration => push(5, 3),
            Ab::SpeedBoost | Ab::BadDreams | Ab::Harvest | Ab::CudChew | Ab::Moody | Ab::Pickup | Ab::SlowStart => push(28, 2),
            Ab::HungerSwitch => push(29, 7),
            _ => {}
        }
        // Protect + Stall: PS registers a Residual handler (via `getKey:'duration'`,
        // battle.ts:487) for EACH duration-carrying volatile, independent of any onResidual
        // callback. The `protect` volatile has duration 1 (removed the turn it's used); the
        // `stall` volatile has duration 2 (conditions.ts), so it survives ONE residual PAST the
        // protect volatile — on the turn AFTER a Protect (protect gone, stall still counting down)
        // PS still keeps the stall handler, giving a 1-longer list (`[5,2,4]` vs the engine's old
        // `[4,2,4]`). So the two handlers are gated INDEPENDENTLY: `protect` iff the Protect
        // volatile is present (its own turn), `stall` iff the stall volatile is present. In the
        // per-decision differ `stall_counter` is set from PS's stall volatile in the snapshot
        // (convert.rs:485), so `stall_counter > 0` ⟺ the stall volatile is present — the exact
        // predicate; both order "false", subOrder 2. (Stream-neutral for the from-seed gate — both
        // list lengths consume one `random` over the same tie-group — so this only sharpens the
        // differ's strict args comparison; no game's draw count changes.)
        if v.contains(V::Protect) {
            push(FALSE, 2); // protect (own-turn: duration-1 volatile)
        }
        if s.stall_counter > 0 {
            push(FALSE, 2); // stall (duration-2 volatile; survives one turn past protect)
        }
    }
    hs
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

pub(crate) fn apply_end_of_turn(mut branch: Branch, switched: [bool; 2]) -> Vec<Branch> {
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
    // Hunger Switch (Morpeko): the forme toggles every end of turn (PS `onResidual` order
    // 29) unless Terastallized. Both formes share stats and typing, so this is exactly a
    // species-id swap; Aura Wheel reads the forme at use time.
    for side in [SideId::One, SideId::Two] {
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
    // Rampage (lockedmove) `onResidual`: decrement `trueDuration` each end of turn. The move
    // action stored the mid-turn (kernel) value {2,3 on start, s on continuation}; this ticks it
    // to the terminal value {1,2, s-1}. The final locked turn (n==1) already ended and confused
    // at move time, so a live Rampaging here always has n>=2 → stays >=1.
    for side in [SideId::One, SideId::Two] {
        if let crate::state::PendingMove::Rampaging(m, n) = b.state.side(side).pending_move {
            if n >= 2 {
                push(b, Instruction::SetPendingMove {
                    side,
                    previous: crate::state::PendingMove::Rampaging(m, n),
                    new: crate::state::PendingMove::Rampaging(m, n - 1),
                });
            }
        }
    }
    // PS decrements a residual effect's duration FIRST and, if it hits 0, ends the effect and
    // SKIPS its onResidual that turn (battle.ts residual loop). The SANDSTORM/snow CHIP and the
    // weather-tied ability heals (Rain Dish, Ice Body) are part of the weather's own residual, so
    // they are skipped on the weather's final turn — tick the weather here, before that loop. (The
    // Grassy Terrain heal is a separate per-mon handler that still fires on the terrain's final
    // turn, so the terrain duration is ticked AFTER the loop, below.)
    if b.state.weather != Weather::None && b.state.weather_turns > 0 {
        push(b, Instruction::DecrementWeatherTurns);
        if b.state.weather_turns == 0 {
            set_weather(b, Weather::None, 0);
        }
    }
    // Order: weather, then per active: Leftovers heal, status residual, Salt Cure.
    // (PS uses a finer speed-ordered residual queue; this covers the common cases.)
    for side in [SideId::One, SideId::Two] {
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
        if effective_weather(&b.state) == Weather::Sand && !magic_guard {
            let immune = p.types.contains(&Type::Rock)
                || p.types.contains(&Type::Ground)
                || p.types.contains(&Type::Steel)
                || matches!(ability, Ab::SandVeil | Ab::SandRush | Ab::SandForce | Ab::Overcoat | Ab::SandStream);
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

        // Leftovers.
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

        // Grassy Terrain heals grounded actives 1/16 max HP at end of turn.
        let p = b.state.side(side).active();
        let grounded = !p.types.contains(&Type::Flying) && p.ability != crate::ids::Ability::Levitate;
        if b.state.terrain == crate::ids::Terrain::Grassy && grounded && p.hp < p.max_hp && p.is_alive() && !heal_blocked(b, side) {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
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
        if !heal_blocked(b, other) {
            let heal = drain.min(f_room);
            if heal > 0 {
                push(b, Instruction::Heal { side: other, slot: fslot, amount: heal });
            }
        }
    }
    for side in [SideId::One, SideId::Two] {
        let p = b.state.side(side).active();
        if !p.is_alive() {
            continue;
        }
        let slot = b.state.side(side).active_index;
        let maxhp = p.max_hp;
        use crate::ids::Ability as Ab;
        let ability = p.ability;
        let magic_guard = ability == Ab::MagicGuard;

        // Status residual. Poison Heal *heals* 1/8 instead of taking poison/toxic damage;
        // Magic Guard cancels the damage entirely.
        let (palive, pstatus, php) = {
            let p = b.state.side(side).active();
            (p.is_alive(), p.status, p.hp)
        };
        if palive {
            let poisoned = matches!(pstatus, Status::Poison | Status::Toxic);
            if ability == Ab::PoisonHeal && poisoned {
                let heal = (maxhp / 8).max(1).min(maxhp - php);
                if heal > 0 && !heal_blocked(b, side) {
                    push(b, Instruction::Heal { side, slot, amount: heal });
                }
                // Toxic still advances its counter even under Poison Heal.
                if pstatus == Status::Toxic {
                    let cur = b.state.side(side).active().status_counter;
                    push(b, Instruction::ChangeStatusCounter { side, slot, previous: cur, new: cur + 1 });
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
                        let stage = (b.state.side(side).active().status_counter as i16 + 1).max(1);
                        let dmg = ((maxhp / 16) * stage).max(1).min(b.state.side(side).active().hp);
                        push(b, Instruction::Damage { side, slot, amount: dmg });
                        push(b, Instruction::ChangeStatusCounter { side, slot, previous: stage as u8 - 1, new: stage as u8 });
                    }
                    _ => {}
                }
            }
        }

        // Hydration cures any non-volatile status at end of turn while it's raining (after
        // that turn's status damage has already been dealt).
        let p = b.state.side(side).active();
        if ability == Ab::Hydration
            && matches!(b.state.weather, Weather::Rain | Weather::HeavyRain)
            && p.status != Status::None
            && p.is_alive()
        {
            let prev = p.status;
            push(b, Instruction::ChangeStatus { side, slot, previous: prev, new: Status::None });
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
                let mut lowered = false;
                for stat in [BoostIndex::Defense, BoostIndex::SpecialDefense] {
                    lowered |= apply_boost_clamped(b, side, stat, -1) < 0;
                }
                if lowered {
                    react_to_stat_drop(b, side);
                    apply_white_herb(b, side);
                }
            }
        }

        // Curse (Ghost): the cursed mon loses 1/4 max HP each turn.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::Curse) {
            let dmg = (maxhp / 4).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }

        // Sitrus Berry can also fire here if end-of-turn chip drops the holder to ≤ 1/2.
        apply_pinch_berry(b, side);

        // Status orbs poison/burn the holder at end of turn (Toxic Orb → Toxic, Flame Orb →
        // Burn) if it has no status yet. The status damage starts next turn.
        let p = b.state.side(side).active();
        if p.is_alive() {
            let orb_status = match p.item {
                Item::ToxicOrb => Status::Toxic,
                Item::FlameOrb => Status::Burn,
                _ => Status::None,
            };
            if orb_status != Status::None && status_applies(p, orb_status) {
                push(b, Instruction::ChangeStatus { side, slot, previous: Status::None, new: orb_status });
            }
        }

        // Bad Dreams (Darkrai): each sleeping foe loses 1/8 max HP (PS onResidual 28.2).
        // Comatose counts as asleep; Magic Guard on the sleeper prevents the damage.
        if ability == Ab::BadDreams && b.state.side(side).active().is_alive() {
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

        // Cud Chew: re-apply the eaten berry's effect at end of turn (PS onResidualOrder 28).
        // The counter was set to 2 on eat and ticks down here; at 0 the berry effect fires again.
        let cc = b.state.side(side).active().cudchew_turns;
        if cc > 0 {
            push(b, Instruction::SetCudChew { side, slot, previous: cc, new: cc - 1 });
            if cc - 1 == 0 && b.state.side(side).active().is_alive() {
                let berry = b.state.side(side).active().last_berry;
                apply_berry_eat_effect(b, side, berry);
            }
        }

        // Speed Boost: +1 Spe at end of turn, but not the turn the mon switched in.
        let side_idx = match side { SideId::One => 0, SideId::Two => 1 };
        if ability == Ab::SpeedBoost && !switched[side_idx] && b.state.side(side).active().is_alive() {
            raise_boost(b, side, BoostIndex::Speed, 1);
        }

        // Roost wears off: restore the user's pre-Roost typing. (In the modeled scope, types
        // only change via Roost and Tera, so base types — or the tera type — are exact.)
        if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {
            let p = b.state.side(side).active();
            if p.is_alive() {
                let restored = if p.terastallized { [p.tera_type, Type::None] } else { p.base_types };
                if p.types != restored {
                    push(b, Instruction::ChangeTypes { side, slot, previous: p.types, new: restored });
                }
            }
            push(b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::Roosted });
        }

        // Advance the active mon's turn counter (used by Fake Out / First Impression / Slow
        // Start). Caps so it can't overflow in a long stall.
        let cur = b.state.side(side).active_turns;
        if cur < 250 {
            push(b, Instruction::SetActiveCounter {
                side,
                which: crate::instruction::ActiveCounter::ActiveTurns,
                previous: cur,
                new: cur + 1,
            });
        }
    }

    // Safety net for the linked `trapped` release: a trap source that died to a RESIDUAL (burn,
    // its own partial trap, …) rather than a hit also frees the foe before the decision boundary.
    for side in [SideId::One, SideId::Two] {
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
        let stall = b.state.side(side).stall_counter;
        if stall != 0 && !b.state.side(side).volatiles.contains(VolatileStatus::Protect) {
            push(b, Instruction::SetStallCounter { side, previous: stall, new: 0 });
        }
        for v in [VolatileStatus::Protect, VolatileStatus::Endure, VolatileStatus::Flinch] {
            if b.state.side(side).volatiles.contains(v) {
                push(b, Instruction::RemoveVolatile { side, volatile: v });
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
            push(b, Instruction::ChangeTerrain {
                previous: b.state.terrain,
                previous_turns: 0,
                new: crate::ids::Terrain::None,
                new_turns: 0,
            });
        }
    }
    if b.state.trick_room && b.state.trick_room_turns > 0 {
        push(b, Instruction::DecrementTrickRoomTurns);
        if b.state.trick_room_turns == 0 {
            push(b, Instruction::ToggleTrickRoom { previous_turns: 0, new_turns: 0 });
        }
    }
    // Screens / Tailwind tick per side and clear at 0.
    for side in [SideId::One, SideId::Two] {
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

    // Active-mon countdowns: Taunt / Encore / Disable tick and clear; Yawn ticks and puts the
    // holder to sleep at 0; Perish Song ticks and faints the holder at 0.
    use crate::instruction::ActiveCounter;
    let mut yawn_fired = [false; 2];
    for side in [SideId::One, SideId::Two] {
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
        // Wish: ticks each end of turn; heals the slot's occupant when it lands. PS's
        // residual handler doesn't run for an empty/fainted slot, so a matured wish
        // LINGERS until an end-of-turn where the slot has a live occupant.
        let wish = b.state.side(side).wish;
        if wish.0 > 0 {
            let landed = wish.0 == 1;
            if landed && !b.state.side(side).active().is_alive() {
                // linger: try again next end of turn
            } else {
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

    // Future Sight: tick; mark a strike when it lands (stochastic rolls -> branch below).
    let mut fs_fired: [Option<u8>; 2] = [None, None];
    for side in [SideId::One, SideId::Two] {
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
    let mut out_h = vec![branch];
    for side in [SideId::One, SideId::Two] {
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
    let branches_after_harvest = out_h;

    // Shed Skin: 33% chance each end of turn to cure the holder's status (branches).
    let mut out = branches_after_harvest;
    for side in [SideId::One, SideId::Two] {
        out = out
            .into_iter()
            .flat_map(|b| {
                let p = b.state.side(side).active();
                if p.ability == crate::ids::Ability::ShedSkin && p.status != Status::None && p.is_alive() {
                    let slot = b.state.side(side).active_index;
                    let (prev, prev_ctr) = (p.status, p.status_counter);
                    let mut cure = scaled(&b, 33.0 / 100.0);
                    push(&mut cure, Instruction::ChangeStatus { side, slot, previous: prev, new: Status::None });
                    if prev_ctr != 0 {
                        push(&mut cure, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 0 });
                    }
                    let keep = scaled(&b, 67.0 / 100.0);
                    vec![cure, keep]
                } else {
                    vec![b]
                }
            })
            .collect();
    }

    // Yawn expiry: the drowsy mon falls asleep now (stochastic 1-3 turn duration).
    let mut out = out;
    for (i, fired) in yawn_fired.into_iter().enumerate() {
        if !fired {
            continue;
        }
        let side = if i == 0 { SideId::One } else { SideId::Two };
        out = out
            .into_iter()
            .flat_map(|mut x| {
                let p = x.state.side(side).active();
                if p.is_alive()
                    && p.status == Status::None
                    && status_applies(p, Status::Sleep)
                    && !status_blocked_by_field(&x.state, side, Status::Sleep)
                    && !sleep_clause_blocks(&x.state, side)
                {
                    let slot = x.state.side(side).active_index;
                    push(&mut x, Instruction::ChangeStatus { side, slot, previous: Status::None, new: Status::Sleep });
                    mark_slept_by_foe(&mut x, side);
                    consume_lum_if_statused(&mut x, side);
                    if x.state.side(side).active().status == Status::Sleep {
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
                let rolls = future_sight_rolls(&x.state, side, caster_slot);
                let hp = target.hp;
                let slot = x.state.side(side).active_index;
                rolls
                    .into_iter()
                    .map(|r| {
                        let mut nb = scaled(&x, 1.0 / 16.0);
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
            })
            .collect();
    }
    // runAction Update after the `residual` action completes (battle.ts:2882): one `shuffle[2,0,2]`
    // on a surviving equal-Speed pair. Emitted last, after every residual draw (incl. Future Sight).
    if annotating() {
        for nb in &mut out {
            emit_update(nb);
            // Then PS builds the next move request (`getRequests` → per-active TrapPokemon), whose
            // multi-trap tie shuffle is the turn's trailing draw.
            emit_trap_pokemon_shuffles(nb);
        }
    }
    out
}
