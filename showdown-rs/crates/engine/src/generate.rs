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

/// One node of the outcome tree: a probability, the resulting state, and the
/// instructions that produced it (relative to the input state).
#[derive(Clone)]
struct Branch {
    prob: f32,
    state: State,
    ins: Vec<Instruction>,
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
        _ => false,
    }
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
    // Slow Start halves Speed for the first five active turns after each switch-in.
    if p.ability == SlowStart && s.active_turns <= 5 {
        spe *= 0.5;
    }
    spe as i32
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
fn battle_over(state: &State) -> bool {
    [SideId::One, SideId::Two].into_iter().any(|side| {
        !state.side(side).pokemon.iter().any(|p| p.species != crate::ids::Species::None && p.is_alive())
    })
}

/// Is the active Pokémon grounded (subject to Spikes / Toxic Spikes / Sticky Web)?
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
                    if n > 1 {
                        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::Rampaging(m, n - 1) });
                        vec![b]
                    } else {
                        // Natural end: the user becomes confused.
                        push(&mut b, Instruction::SetPendingMove { side, previous: pending, new: PendingMove::None });
                        if b.state.side(side).volatiles.contains(VolatileStatus::LockedMove) {
                            push(&mut b, Instruction::RemoveVolatile { side, volatile: VolatileStatus::LockedMove });
                        }
                        if !b.state.side(side).volatiles.contains(VolatileStatus::Confusion) {
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
                    // Starting a rampage: total 2 or 3 turns -> 1 or 2 remaining (uniform).
                    [1u8, 2]
                        .into_iter()
                        .map(|rem| {
                            let mut nb = scaled(&b, 0.5);
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
    let id = md.id.to_id();
    match (id, effective_weather(&b.state)) {
        ("blizzard", Weather::Snow) => return 1.0,
        ("thunder" | "hurricane" | "bleakwindstorm" | "wildboltstorm" | "sandsearstorm", Weather::Rain | Weather::HeavyRain) => return 1.0,
        ("thunder" | "hurricane", Weather::Sun | Weather::HarshSun) => return 0.5,
        _ => {}
    }
    let mut acc = md.accuracy as f32;
    if atk.ability == Ab::CompoundEyes {
        acc *= 1.3;
    }
    if atk.ability == Ab::Hustle && md.category == MoveCategory::Physical {
        acc *= 0.8;
    }
    if atk.item == Item::WideLens {
        acc *= 1.1;
    }
    (acc / 100.0).min(1.0)
}

/// Public entry point. `s1`/`s2` are side one's and side two's chosen actions.
pub fn generate_instructions(state: &State, s1: MoveChoice, s2: MoveChoice) -> Vec<StateInstructions> {
    generate_instructions_ex(state, s1, s2, [None, None], [false, false])
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
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new() };
    // In the full turn resolver the queue suppresses a flinched action before calling the move
    // executor. The factorized/request-model entry point must preserve that same boundary.
    if state.side(side).volatiles.contains(VolatileStatus::Flinch) {
        return vec![StateInstructions { percentage: 100.0, instructions: Vec::new() }];
    }
    execute_move(start, Action { side, move_idx, pivot, foe_pending_move })
        .into_iter()
        .map(|b| StateInstructions { percentage: b.prob, instructions: b.ins })
        .collect()
}

/// Apply Terastallization to a side's active at turn start: its types become its tera type
/// (Stellar keeps the original types) and the terastallized flag flips. Done before moves so
/// the new typing affects both its own STAB and the damage it takes this turn.
fn apply_tera(b: &mut Branch, side: SideId) {
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
    let start = Branch { prob: 100.0, state: *state, ins: Vec::new() };
    let mut branches = vec![start];

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
            for &(side, target) in &order {
                apply_switch(b, side, target);
            }
        }
    } else {
        for (side, target) in switch_actions {
            for b in &mut branches {
                apply_switch(b, side, target);
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
    let move_actions: Vec<Action> = [(SideId::One, s1, pivot[0]), (SideId::Two, s2, pivot[1])]
        .into_iter()
        .filter_map(|(side, c, pv)| match c {
            MoveChoice::Move(idx) => Some(Action { side, move_idx: idx, pivot: pv, foe_pending_move: None }),
            MoveChoice::Switch(_) => None,
        })
        .collect();

    branches = resolve_moves(branches, &move_actions);

    // A side's active was switched in this turn (chose to switch, or used a pivot move) — it
    // hasn't earned an end-of-turn Speed Boost yet.
    let switched = [
        matches!(s1, MoveChoice::Switch(_)) || pivot[0].is_some(),
        matches!(s2, MoveChoice::Switch(_)) || pivot[1].is_some(),
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
            }
        })
        .collect();

    branches
        .into_iter()
        .map(|b| StateInstructions { percentage: b.prob, instructions: b.ins })
        .collect()
}

/// Apply a (forced) switch-in directly to `state`: reset the outgoing active's boosts
/// and volatiles, change the active slot, and apply entry hazards. Used by the
/// differential harness to apply post-faint replacement switches.
pub fn switch_into(state: &mut State, side: SideId, target: u8) {
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new() };
    apply_switch(&mut b, side, target);
    *state = b.state;
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

fn apply_switch(b: &mut Branch, side: SideId, target: u8) {
    apply_switch_inner(b, side, target, true);
}

/// `fire_ability: false` defers the incoming mon's switch-in ability (simultaneous entries
/// run all abilities after every replacement is on the field, in speed order — PS event
/// semantics; Intimidate must see the other fresh switch-in).
fn apply_switch_inner(b: &mut Branch, side: SideId, target: u8, fire_ability: bool) {
    let s = b.state.side(side);
    let previous = s.active_index;
    let replacing_fainted = !s.active().is_alive();
    if previous == target {
        return;
    }
    // A traced / copied ability reverts on switch-out (Transform handles its own below).
    {
        let p = b.state.side(side).active();
        if !p.transformed && p.ability != p.base_ability {
            let slot = previous;
            push(b, Instruction::ChangeAbility { side, slot, previous: p.ability, new: p.base_ability });
        }
    }
    // A transformed mon reverts to its own identity as it leaves the field.
    revert_transform(b, side);
    // Zero to Hero (Palafin): on switch-out, the base forme transforms into Palafin-Hero
    // (higher offensive stats; HP base is unchanged so max HP carries). One-way — once Hero it
    // stays Hero. Random-battle spread (31 IV / 85 EV / neutral) assumed for the stat recompute.
    {
        let p = b.state.side(side).active();
        let palafin = crate::ids::Species::from_id("palafin");
        if p.ability == crate::ids::Ability::ZeroToHero && Some(p.species) == palafin {
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
        if vols.contains(*v) {
            push(b, Instruction::RemoveVolatile { side, volatile: *v });
        }
    }
    let sub = b.state.side(side).substitute_hp;
    if sub != 0 {
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
    let mut b = Branch { prob: 100.0, state: *state, ins: Vec::new() };
    let mut order = pairs;
    if effective_speed(&b.state, order[1].0) > effective_speed(&b.state, order[0].0) {
        order.swap(0, 1);
    }
    for &(side, target) in &order {
        apply_switch_inner(&mut b, side, target, false);
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

/// A move action: which side, which move slot, and (for a pivot move) the switch-in
/// target to use after it connects.
#[derive(Clone, Copy)]
struct Action {
    side: SideId,
    move_idx: u8,
    pivot: Option<u8>,
    /// The move the foe will use *after* this action this turn (None if the foe already
    /// moved, switched, or there is no second move). Lets Sucker Punch / Thunderclap know
    /// whether the target is about to attack.
    foe_pending_move: Option<crate::ids::MoveId>,
}

fn resolve_moves(branches: Vec<Branch>, actions: &[Action]) -> Vec<Branch> {
    let mut out = Vec::new();
    for b in branches {
        out.extend(resolve_moves_for_branch(b, actions));
    }
    out
}

fn resolve_moves_for_branch(b: Branch, actions: &[Action]) -> Vec<Branch> {
    match actions.len() {
        0 => vec![b],
        1 => execute_move(b, actions[0]),
        _ => {
            let (a, b_act) = (actions[0], actions[1]);
            let order = move_order(&b.state, a.side, a.move_idx, b_act.side, b_act.move_idx);
            match order {
                Order::First(first) => {
                    let (f, s) = if first == a.side { (a, b_act) } else { (b_act, a) };
                    sequence_two_moves(b, f, s)
                }
                Order::Tie => {
                    // 50/50 over the two orderings.
                    let mut res = sequence_two_moves(scaled(&b, 0.5), a, b_act);
                    res.extend(sequence_two_moves(scaled(&b, 0.5), b_act, a));
                    res
                }
            }
        }
    }
}

fn scaled(b: &Branch, f: f32) -> Branch {
    Branch { prob: b.prob * f, state: b.state, ins: b.ins.clone() }
}

fn sequence_two_moves(b: Branch, mut first: Action, second: Action) -> Vec<Branch> {
    // Tell the first mover what the (not-yet-moved) second mover is about to do, so Sucker
    // Punch / Thunderclap can tell whether the target is attacking. The second mover's foe
    // (the first) has already acted, so it stays None.
    first.foe_pending_move = Some(b.state.side(second.side).active().moves[second.move_idx as usize].id);
    let mut out = Vec::new();
    for fb in execute_move(b, first) {
        // The second mover acts only if its active is alive and wasn't flinched by the first.
        let flinched = fb.state.side(second.side).volatiles.contains(VolatileStatus::Flinch);
        // Once the first action ends the battle (for example Life Orb recoil KOs that side's
        // final Pokémon), PS never runs the queued slower action and therefore pays no PP for
        // it.  Do not use merely `first mover is alive` here: if it has a replacement available
        // PS can continue the queue, and that broader condition regresses valid Memento/status
        // cases.
        if fb.state.side(second.side).active().is_alive() && !flinched && !battle_over(&fb.state) {
            out.extend(execute_move(fb, second));
        } else {
            out.push(fb);
        }
    }
    out
}

enum Order {
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
        "superfang" => (defender.hp / 2).max(1),
        "endeavor" => (defender.hp - attacker.hp).max(0),
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
    pri
}

fn move_order(state: &State, sa: SideId, ma: u8, sb: SideId, mb: u8) -> Order {
    let pa = effective_priority(state, sa, ma);
    let pb = effective_priority(state, sb, mb);
    if pa != pb {
        return Order::First(if pa > pb { sa } else { sb });
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
    // Note: Protosynthesis / QuarkDrive are not cleared here yet; their re-application on
    // switch-in isn't modeled, so clearing would lose the boost for the common stay-in case.
    // ability-driven volatiles incorrectly; they're re-derived on switch-in (TODO).
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
fn execute_move(b: Branch, action: Action) -> Vec<Branch> {
    let side = action.side;
    let (alive, status, confused) = {
        let p = b.state.side(side).active();
        (p.is_alive(), p.status, b.state.side(side).volatiles.contains(VolatileStatus::Confusion))
    };
    // Sleep/freeze are handled inside (the mon can't act anyway). For an awake mon, split off
    // confusion self-hit (1/3) and full paralysis (1/4 of the remainder) — both branches where
    // the move doesn't execute. The remaining "acts normally" branch equals prior behavior, so
    // these only *add* outcomes (no regression on the common path).
    if !alive || status == Status::Sleep {
        return execute_move_inner(b, action);
    }
    // Freeze: 20% chance to thaw and act this turn, otherwise stay frozen (no move).
    if status == Status::Freeze {
        let mut out = vec![scaled(&b, 0.80)];
        let mut thawed = scaled(&b, 0.20);
        let slot = thawed.state.side(side).active_index;
        push(&mut thawed, Instruction::ChangeStatus { side, slot, previous: Status::Freeze, new: Status::None });
        out.extend(execute_move_inner(thawed, action));
        return out;
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
    let mut out = Vec::new();
    let mut act = 1.0f32;
    if confused {
        // PS uses randomChance(33, 100), not an exact one-third check.
        out.extend(confusion_self_hit(scaled(&b, act * 0.33), side));
        act *= 0.67;
    }
    if status == Status::Paralysis {
        out.push(scaled(&b, act * 0.25)); // fully paralyzed: no move
        act *= 0.75;
    }
    if out.is_empty() {
        execute_move_inner(b, action)
    } else {
        out.extend(execute_move_inner(scaled(&b, act), action));
        out
    }
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
    let Action { side, move_idx, pivot, foe_pending_move, .. } = action;
    let attacker = b.state.side(side).active();
    if !attacker.is_alive() {
        return vec![b];
    }
    let move_id = attacker.moves[move_idx as usize].id;
    // Struggle: a mon forced to act with no usable moves (the chosen slot is out of PP) uses
    // Struggle instead — a typeless 50-BP physical hit that connects on everything and recoils
    // 1/4 of the user's max HP.
    let struggling = attacker.moves[move_idx as usize].pp == 0;
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
    let foe = side.other();

    let mut b = b;
    let mut move_idx = move_idx;
    let slot = b.state.side(side).active_index;

    // Encore: the user is locked into its encored move — the chosen slot is overridden
    // (PS onOverrideAction). Skipped while committed to a multi-turn move or Struggling.
    let enc = b.state.side(side).encore;
    if enc.0 != crate::ids::MoveId::None
        && !struggling
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
        if !struggling {
            if let Some(slot_i) = b.state.side(side).active().moves.iter().position(|ms| ms.id == m) {
                if slot_i as u8 != move_idx {
                    move_idx = slot_i as u8;
                    md = move_data(m);
                }
            }
        }
    }
    let move_id = if struggling { move_id } else { b.state.side(side).active().moves[move_idx as usize].id };

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
        if counter > 1 {
            push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: counter, new: counter - 1 });
            return vec![b];
        }
        push(&mut b, Instruction::ChangeStatus { side, slot, previous: Status::Sleep, new: Status::None });
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
    if !executing_charge && !rampaging_now {
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
    // mon has passed sleep/freeze, so it is actually acting this turn.
    record_move_use(&mut b, side, move_id);

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
            || md.force_switch;
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
    }

    // Status moves handled specially.
    if md.category == MoveCategory::Status {
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
        // A Substitute blocks foe-targeting status moves unless they bypass it (sound
        // moves, Taunt, Encore, ...) or the user has Infiltrator.
        if targets_foe
            && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute)
            && !md.flag_bypass_sub
            && b.state.side(side).active().ability != crate::ids::Ability::Infiltrator
        {
            return vec![b];
        }
        let mut branches = execute_status_move(b, side, &md, foe_pending_move.is_some());
        // Self-switch status moves (Teleport, Chilly Reception, Parting Shot) pivot out.
        if let Some(t) = pivot {
            for sb in &mut branches {
                if sb.state.side(side).active().is_alive() {
                    apply_switch(sb, side, t);
                }
            }
        }
        return branches;
    }

    // Protect: a protected target blocks the incoming damaging move (Protect moves +4
    // priority, so the protector has already set the volatile this turn).
    if b.state.side(foe).volatiles.contains(VolatileStatus::Protect) {
        // High Jump Kick / Jump Kick still crash into the protector (1/2 max HP).
        if matches!(md.id.to_id(), "highjumpkick" | "jumpkick") {
            let (hp, maxhp) = { let p = b.state.side(side).active(); (p.hp, p.max_hp) };
            let crash = (maxhp / 2).min(hp);
            if crash > 0 && b.state.side(side).active().ability != crate::ids::Ability::MagicGuard {
                let slot = b.state.side(side).active_index;
                push(&mut b, Instruction::Damage { side, slot, amount: crash });
            }
        }
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

    // Air Balloon: the holder is untargetable by Ground moves until the balloon pops.
    if md.typ == Type::Ground
        && b.state.side(foe).active().item == Item::AirBalloon
        && b.state.side(foe).active().is_alive()
    {
        return apply_struggle_recoil(apply_recharge(vec![b], side, move_id), side, struggling);
    }

    // Damaging move: branch on accuracy (hit/miss), then crit, then the 16 rolls.
    let mut out = Vec::new();
    let hit_prob = accuracy_of(&b, side, &md);
    let miss_prob = 1.0 - hit_prob;

    if miss_prob > 0.0 {
        let mut mb = scaled(&b, miss_prob);
        // High Jump Kick / Jump Kick: missing costs the user 1/2 of its max HP (crash).
        if matches!(md.id.to_id(), "highjumpkick" | "jumpkick") {
            let (hp, maxhp) = { let p = mb.state.side(side).active(); (p.hp, p.max_hp) };
            let crash = (maxhp / 2).min(hp);
            if crash > 0 {
                let slot = mb.state.side(side).active_index;
                push(&mut mb, Instruction::Damage { side, slot, amount: crash });
            }
        }
        out.push(mb);
    }

    let foe_alive = b.state.side(foe).active().is_alive();
    if !foe_alive {
        out.push(scaled(&b, hit_prob));
        return apply_recharge(out, side, move_id);
    }
    // A target mid-Fly/Dig/etc. (semi-invulnerable) dodges the move entirely.
    if matches!(b.state.side(foe).pending_move, PendingMove::Charging(m) if is_semi_invuln_move(m)) {
        out.push(scaled(&b, hit_prob));
        return apply_recharge(out, side, move_id);
    }

    // A type-immune move (e.g. Close Combat vs a Ghost, or Ground vs Levitate) deals no
    // damage and skips its self-stat secondary — PS only applies `self` boosts on a hit.
    let defender = b.state.side(foe).active();
    // Mold Breaker also bypasses the defender's immunity abilities (Levitate, absorbs,
    // Soundproof, Bulletproof) — treat the ability as None for the immunity check.
    let def_ab = if matches!(
        b.state.side(side).active().ability,
        crate::ids::Ability::MoldBreaker | crate::ids::Ability::Teravolt | crate::ids::Ability::Turboblaze
    ) {
        crate::ids::Ability::None
    } else {
        defender.ability
    };
    let flag_immune = (md.flag_sound && def_ab == crate::ids::Ability::Soundproof)
        || (md.flag_bullet && def_ab == crate::ids::Ability::Bulletproof);
    let scrappy = b.state.side(side).active().ability == crate::ids::Ability::Scrappy;
    let def_types_eff = effective_def_types(scrappy, md.typ, defender.types);
    let connects = crate::damage::type_multiplier(md.typ, def_types_eff) != 0.0
        && !ability_immune(md.typ, def_ab)
        && !flag_immune;
    if !connects {
        out.push(scaled(&b, hit_prob));
        // A rampage move (Outrage/Thrash) that hits an immune target ENDS the lock (without
        // confusion) — route through the rampage/recoil tail rather than returning bare.
        let out = apply_rampage_state(out, side, move_id);
        return apply_struggle_recoil(apply_recharge(out, side, move_id), side, struggling);
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
    let damaged: Vec<(Branch, bool)> = if let Some(fixed) = fixed_damage_amount(&md, &b.state, side) {
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
    } else if matches!(md.id.to_id(), "tripleaxel" | "triplekick") {
        // Ascending power (20/40/60 or 10/20/30) with a fresh 90% accuracy check per hit;
        // a miss ends the move. hit_prob here is the single-hit accuracy.
        let step = md.base_power;
        let mds: Vec<crate::data::MoveData> = (1..=3u16)
            .map(|i| { let mut m = md; m.base_power = step * i; m })
            .collect();
        let calcs: Vec<DamageCalc> = mds.iter().map(|m| compute_damage(&b, side, m)).collect();
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
                let hit_sub = apply_damage_hit_indexed(&mut hb, side, &md, &calcs, &combo);
                v.push((hb, hit_sub));
            }
        }
        v
    } else if hits_min == hits_max && hits_min <= MAX_EXACT_HITS {
        let mut v = Vec::new();
        let crit_p = crit_chance(&b, side, &md);
        for combo in HitCombos::new(hits_min) {
            let mut prob = hit_prob;
            for &(_, crit) in &combo {
                prob *= (1.0 / 16.0) * if crit { crit_p } else { 1.0 - crit_p };
            }
            if prob <= 0.0 {
                continue;
            }
            let mut hb = scaled(&b, prob);
            let hit_sub = apply_damage_hit(&mut hb, side, &md, &combo);
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
        // Weakness Policy on the target (super-effective hit), then White Herb if the user's
        // own self-drops (Leaf Storm, Close Combat, ...) left a negative stage.
        apply_weakness_policy(&mut hb, foe, &md);
        apply_justified(&mut hb, foe, &md);
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
            vec![hb]
        } else {
            apply_target_secondary(hb, side, &md)
                .into_iter()
                .flat_map(|sb| apply_contact_secondaries(sb, side, &md))
                .flat_map(|sb| apply_flinch_split(sb, side, &md))
                .flat_map(|sb| apply_cursed_body(sb, side, &md))
                .collect::<Vec<_>>()
        };
        for mut sb in branches {
            // Pivot move (U-turn): switch the user out now that it connected.
            if let Some(t) = pivot {
                if sb.state.side(side).active().is_alive() {
                    apply_switch(&mut sb, side, t);
                }
            }
            out.push(sb);
        }
    }
    let out = if md.force_switch {
        // Dragon Tail / Circle Throw: the survivor is dragged out (uniform over the bench).
        out.into_iter().flat_map(|x| apply_drag(x, foe)).collect()
    } else {
        out
    };
    let out = apply_rampage_state(out, side, move_id);
    apply_struggle_recoil(apply_recharge(out, side, move_id), side, struggling)
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
    let mb = matches!(attacker.ability, Ab::MoldBreaker | Ab::Teravolt | Ab::Turboblaze);
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
    let (def_idx, def_boost_idx) = if md.category == MoveCategory::Physical {
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
            Ab::IronFist if md.flag_punch => atk_stat = crate::damage::modify(atk_stat, 4915, 4096), // ×1.2
            Ab::Sharpness if md.flag_slicing => atk_stat = crate::damage::modify(atk_stat, 3, 2),
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
            Ab::WaterBubble if md.typ == Type::Water => atk_stat = crate::damage::modify(atk_stat, 2, 1),
            Ab::Transistor if md.typ == Type::Electric => atk_stat = crate::damage::modify(atk_stat, 5325, 4096), // ×1.3
            Ab::DragonsMaw if md.typ == Type::Dragon => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::RockyPayload if md.typ == Type::Rock => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Steelworker if md.typ == Type::Steel => atk_stat = crate::damage::modify(atk_stat, 3, 2),
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
        if sword && md.category == MoveCategory::Physical {
            def_stat = crate::damage::modify(def_stat, 3, 4);
        }
        if beads && md.category == MoveCategory::Special {
            def_stat = crate::damage::modify(def_stat, 3, 4);
        }
        if defender.item == Item::AssaultVest && md.category == MoveCategory::Special {
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
    let scrappy = attacker.ability == Ab::Scrappy;
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
            || md.secondary_self_boosts.iter().any(|&x| x != 0));
    // Life Orb's ×1.3 DAMAGE (onModifyDamage) always applies while held; Sheer Force only
    // suppresses the RECOIL (onAfterMoveSecondarySelf). Keep the two flags separate.
    let life_orb = attacker.item == Item::LifeOrb;
    let life_orb_recoil = life_orb && !sheer_force_active;
    if life_orb {
        fmod = chain_final(fmod, 5324);
    }

    // Knock Off: ×1.5 base power when the target is holding a (removable) item.
    let mut base_power = if md.id.to_id() == "knockoff" && defender.item != Item::None {
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
        // HP-proportional spread moves: BP = floor(150 · userHP / userMaxHP), min 1.
        "eruption" | "waterspout" | "dragonenergy" => {
            let hp = attacker.hp.max(0) as u32;
            let max = attacker.max_hp.max(1) as u32;
            base_power = ((150 * hp / max).max(1)) as u16;
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
        _ => {}
    }
    // Type-boosting held items: ×1.2 base power for the matching move type (PS onBasePower
    // chainModify([4915, 4096])).
    let type_item_boost = matches!(
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
            | (Item::MindPlate, Type::Psychic)
            | (Item::SilverPowder, Type::Bug)
            | (Item::HardStone, Type::Rock)
            | (Item::SpellTag, Type::Ghost)
            | (Item::DragonFang, Type::Dragon)
            | (Item::BlackGlasses, Type::Dark)
            | (Item::MetalCoat, Type::Steel)
            | (Item::SilkScarf, Type::Normal)
            | (Item::FairyFeather, Type::Fairy)
    );
    if type_item_boost {
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
        weather: effective_weather(&b.state),
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

fn apply_damage_hit(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hits: &[(u8, bool)]) -> bool {
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
    for &(roll, crit) in hits {
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
        }
    }
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, hits_landed, life_orb, def_item, def_ability);
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
        }
    }
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, hits_landed, life_orb, def_item, def_ability);
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
        // Contact punishers: Rocky Helmet (1/6) and Rough Skin / Iron Barbs (1/8).
        if md.flag_contact && !hit_sub {
            let frac = if def_item == Item::RockyHelmet {
                Some(6)
            } else if matches!(def_ability, Ab::RoughSkin | Ab::IronBarbs) {
                Some(8)
            } else {
                None
            };
            if let Some(d) = frac {
                let atk = b.state.side(side).active();
                if atk.is_alive() {
                    let dmg = (atk.max_hp / d).max(1).min(atk.hp);
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

    // Moxie: +1 Atk when the move knocks out the target.
    if any_damage
        && !b.state.side(foe).active().is_alive()
        && b.state.side(side).active().ability == Ab::Moxie
        && b.state.side(side).active().is_alive()
    {
        raise_boost(b, side, BoostIndex::Attack, 1);
    }

    // Beast Boost: a KO raises the attacker's highest stat by 1.
    if any_damage
        && !b.state.side(foe).active().is_alive()
        && b.state.side(side).active().ability == Ab::BeastBoost
        && b.state.side(side).active().is_alive()
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

    // Track times the target has been hit (Rage Fist). PS counts each hit of a multi-hit
    // move separately (cosim caught the engine counting once per move). A hit absorbed by a
    // Substitute doesn't count.
    if any_damage && !hit_sub && hits_landed > 0 && b.state.side(foe).active().is_alive() {
        let fslot = b.state.side(foe).active_index;
        let cur = b.state.side(foe).active().times_hit;
        let new = cur.saturating_add(hits_landed).min(250);
        if new != cur {
            push(b, Instruction::SetTimesHit { side: foe, slot: fslot, previous: cur, new });
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
                _ => {}
            }
        }
        // Soul-Heart: +1 SpA whenever a Pokémon faints from this hit.
        if !b.state.side(foe).active().is_alive()
            && b.state.side(side).active().ability == crate::ids::Ability::SoulHeart
            && b.state.side(side).active().is_alive()
        {
            raise_boost(b, side, BoostIndex::SpecialAttack, 1);
        }
    }

    // Knock Off removes the target's held item (so it no longer triggers Leftovers heals etc.).
    if md.id.to_id() == "knockoff" && !hit_sub {
        let f = b.state.side(foe).active();
        if f.is_alive() && f.item != Item::None {
            let (prev, fslot) = (f.item, b.state.side(foe).active_index);
            push(b, Instruction::ChangeItem { side: foe, slot: fslot, previous: prev, new: Item::None });
            on_item_lost(b, foe);
            // Knocking the item off reveals what it was.
            reveal(b, foe, 0, crate::state::Reveal::ITEM);
        }
    }

    // A transformed mon that fainted this hit (the target, or the attacker via a contact
    // punisher / recoil) reverts to its own identity — PS runs clearVolatile on faint.
    if !b.state.side(foe).active().is_alive() {
        revert_transform(b, foe);
    }
    if !b.state.side(side).active().is_alive() {
        revert_transform(b, side);
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
/// and O(HP) branches. Substitute routing and Sturdy/Focus Sash are not modeled on this path:
/// both are one-hit effects (a Sash/Sturdy is broken by the second hit) and don't apply to
/// the ≥2-hit moves that reach here.
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
    // The distribution is keyed by (total damage, hit count): hit count is itself observable
    // state (Rage Fist's times-hit counts every hit), so merging across it would be lossy.
    let cap = b.state.side(foe).active().hp.max(0) as i32;
    let mut conv: HashMap<i32, f32> = HashMap::new();
    conv.insert(0, 1.0);
    let mut dist: HashMap<(i32, usize), f32> = HashMap::new();
    for k in 1..=max {
        let mut next: HashMap<i32, f32> = HashMap::with_capacity(conv.len() + 32);
        for (&t, &pt) in &conv {
            for &(v, pv) in &per_hit {
                *next.entry((t + v).min(cap)).or_insert(0.0) += pt * pv;
            }
        }
        conv = next;
        if let Some(&(_, pk)) = counts.iter().find(|(c, _)| *c == k) {
            for (&t, &p) in &conv {
                *dist.entry((t, k)).or_insert(0.0) += pk * p;
            }
        }
    }

    // One branch per distinct (total damage, hit count).
    let mut out = Vec::with_capacity(dist.len());
    for ((total, hits), p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        if total > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: total as i16 });
        }
        apply_post_damage(&mut hb, side, md, total, total > 0, false, hits as u8, calc.life_orb, calc.def_item, calc.def_ability);
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
    // Key: (sub_remaining, mon_damage). While the sub stands mon_damage == 0; once it breaks
    // (sub_remaining == 0) it absorbs no more and the overflow of the breaking hit is discarded.
    let mut conv: HashMap<(i32, i32), f32> = HashMap::new();
    conv.insert((sub_hp0, 0), 1.0);
    let mut dist: HashMap<(i32, i32, usize), f32> = HashMap::new();
    for k in 1..=max {
        let mut next: HashMap<(i32, i32), f32> = HashMap::with_capacity(conv.len() + 32);
        for (&(sub_rem, mon), &pt) in &conv {
            for &(v, pv) in per_hit {
                let key = if sub_rem > 0 {
                    if v < sub_rem { (sub_rem - v, mon) } else { (0, mon) }
                } else {
                    (0, (mon + v).min(cap))
                };
                *next.entry(key).or_insert(0.0) += pt * pv;
            }
        }
        conv = next;
        if let Some(&(_, pk)) = counts.iter().find(|(c, _)| *c == k) {
            for (&(sub_rem, mon), &p) in &conv {
                *dist.entry((sub_rem, mon, k)).or_insert(0.0) += pk * p;
            }
        }
    }

    let mut out = Vec::with_capacity(dist.len());
    for ((sub_rem, mon, hits), p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        let sub_dmg = sub_hp0 - sub_rem;
        if sub_dmg > 0 {
            push(&mut hb, Instruction::DamageSubstitute { side: foe, amount: sub_dmg as i16 });
        }
        if sub_rem == 0 {
            push(&mut hb, Instruction::RemoveVolatile { side: foe, volatile: VolatileStatus::Substitute });
        }
        if mon > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: mon as i16 });
        }
        apply_post_damage(&mut hb, side, md, sub_dmg + mon, sub_dmg + mon > 0, true, hits as u8, calc.life_orb, calc.def_item, calc.def_ability);
        out.push((hb, true));
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
    if p.status != Status::None || !p.is_alive() {
        return false;
    }
    use crate::ids::Ability as Ab;
    // Blanket status immunities.
    if matches!(p.ability, Ab::PurifyingSalt | Ab::Comatose) {
        return false;
    }
    match status {
        Status::Burn => !p.types.contains(&Type::Fire) && !matches!(p.ability, Ab::WaterVeil | Ab::WaterBubble | Ab::ThermalExchange),
        Status::Paralysis => !p.types.contains(&Type::Electric) && p.ability != Ab::Limber,
        Status::Poison | Status::Toxic => {
            !(p.types.contains(&Type::Poison) || p.types.contains(&Type::Steel))
                && p.ability != Ab::Immunity
        }
        // Insomnia / Vital Spirit / Sweet Veil grant immunity to sleep.
        Status::Sleep => !matches!(p.ability, Ab::Insomnia | Ab::VitalSpirit | Ab::SweetVeil),
        Status::Freeze => !p.types.contains(&Type::Ice) && p.ability != Ab::MagmaArmor,
        _ => true,
    }
}

/// Field-level status blocks: Electric Terrain blocks sleep and Misty Terrain blocks all
/// status for grounded targets. Checked alongside `status_applies` at application sites.
fn status_blocked_by_field(state: &State, target: SideId, status: Status) -> bool {
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
    if b.state.side(side.other()).active().ability == crate::ids::Ability::Unnerve && b.state.side(side.other()).active().is_alive() {
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
    if b.state.side(side.other()).active().ability == crate::ids::Ability::Unnerve && b.state.side(side.other()).active().is_alive() {
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
    // Contrary inverts the change before anything else (so a "drop" becomes a raise and is no
    // longer blocked by Clear Body / counted as a drop by Defiant).
    let delta = if b.state.side(target).active().ability == crate::ids::Ability::Contrary { -delta } else { delta };
    if delta < 0 && b.state.side(target).active().ability == crate::ids::Ability::ClearBody {
        return 0;
    }
    let cur = b.state.side(target).boost(stat);
    let eff = (cur + delta).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side: target, stat, amount: eff });
    }
    eff
}

/// Apply a *self*-boost (Swords Dance, Leaf Storm's −2 SpA, ...). Self-boosts ignore Clear
/// Body but are inverted by Contrary. Returns nothing; clamps to ±6.
fn apply_self_boost(b: &mut Branch, side: SideId, stat: BoostIndex, delta: i8) {
    let delta = if b.state.side(side).active().ability == crate::ids::Ability::Contrary { -delta } else { delta };
    let cur = b.state.side(side).boost(stat);
    let eff = (cur + delta).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side, stat, amount: eff });
    }
}

/// Raise a stat by `amount` on `side` (positive only; respects the +6 clamp). Used for the
/// reaction abilities, so it bypasses the Clear-Body / re-trigger paths of a normal drop.
fn raise_boost(b: &mut Branch, side: SideId, stat: BoostIndex, amount: i8) {
    let cur = b.state.side(side).boost(stat);
    let eff = (cur + amount).clamp(-6, 6) - cur;
    if eff != 0 {
        push(b, Instruction::Boost { side, stat, amount: eff });
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
    // Toxic Chain (attacker) badly-poisons the target on any damaging hit (30%, not contact-
    // gated). Otherwise a contact hit can trigger the defender's status ability or the
    // attacker's Poison Touch.
    let (target, status) = if atk_ab == Ab::ToxicChain {
        (foe, Status::Toxic)
    } else if !md.flag_contact {
        return vec![b];
    } else {
        match def_ab {
            Ab::FlameBody => (side, Status::Burn),
            Ab::Static => (side, Status::Paralysis),
            Ab::PoisonPoint => (side, Status::Poison),
            _ if atk_ab == Ab::PoisonTouch => (foe, Status::Poison),
            _ => return vec![b],
        }
    };
    if !status_applies(b.state.side(target).active(), status) {
        return vec![b];
    }
    let chance = 0.30;
    let mut proc = scaled(&b, chance);
    let noproc = scaled(&b, 1.0 - chance);
    let slot = proc.state.side(target).active_index;
    push(&mut proc, Instruction::ChangeStatus { side: target, slot, previous: Status::None, new: status });
    vec![proc, noproc]
}

/// Split a hit on the move's flinch chance (×2 under Serene Grace): the proc branch applies
/// the Flinch volatile to the target, which `sequence_two_moves` uses to skip a target that
/// hasn't moved yet. Inner Focus and an already-flinched target are immune.
fn apply_flinch_split(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if md.flinch_chance == 0 {
        return vec![b];
    }
    // Sheer Force trades the flinch secondary for its x1.3 power boost.
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return vec![b];
    }
    let foe = side.other();
    let d = b.state.side(foe).active();
    if !d.is_alive()
        || d.ability == crate::ids::Ability::InnerFocus
        || d.ability == crate::ids::Ability::ShieldDust
        || d.item == Item::CovertCloak
        || b.state.side(foe).volatiles.contains(VolatileStatus::Flinch)
    {
        return vec![b];
    }
    let pct = if b.state.side(side).active().ability == crate::ids::Ability::SereneGrace {
        (md.flinch_chance as u16 * 2).min(100) as u8
    } else {
        md.flinch_chance
    };
    let chance = pct as f32 / 100.0;
    let mut proc = scaled(&b, chance);
    let noproc = scaled(&b, 1.0 - chance);
    push(&mut proc, Instruction::ApplyVolatile { side: foe, volatile: VolatileStatus::Flinch });
    vec![proc, noproc]
}

/// Cursed Body (defender): 30% chance to Disable the move that just hit it.
fn apply_cursed_body(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    let foe = side.other();
    if b.state.side(foe).active().ability != crate::ids::Ability::CursedBody
        || !b.state.side(side).active().is_alive()
        || b.state.side(side).volatiles.contains(VolatileStatus::Disable)
        || md.id == crate::ids::MoveId::None
        || !b.state.side(side).active().moves.iter().any(|m| m.id == md.id)
    {
        return vec![b];
    }
    let mut proc = scaled(&b, 0.30);
    let noproc = scaled(&b, 0.70);
    push(&mut proc, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Disable });
    let prev = proc.state.side(side).disable;
    // The attacker has already moved this turn -> full 4-turn disable (PS duration 5 - 1
    // only when the target will still move; here it has just moved).
    push(&mut proc, Instruction::SetDisable { side, previous: prev, new: (md.id, 4) });
    vec![proc, noproc]
}

/// Split a hit branch on a move's chance-based target secondary (proc vs no-proc).
fn apply_target_secondary(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    if md.secondary_chance == 0 {
        return vec![b];
    }
    // Sheer Force removes secondary effects entirely (in exchange for the ×1.3 above).
    if b.state.side(side).active().ability == crate::ids::Ability::SheerForce {
        return vec![b];
    }
    let foe = side.other();
    let has_self = md.secondary_self_boosts.iter().any(|&x| x != 0);
    let target_eligible = b.state.side(foe).active().is_alive()
        && b.state.side(foe).active().ability != crate::ids::Ability::ShieldDust
        && b.state.side(foe).active().item != Item::CovertCloak;
    // Shield Dust / Covert Cloak remove target-facing secondaries, but PS preserves a
    // secondary's `self` payload (Fiery Dance can still boost its user, including on a KO).
    if !target_eligible && !has_self {
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
    let noproc = scaled(&b, 1.0 - chance);
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
        && status_applies(proc.state.side(foe).active(), md.secondary_status)
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
        procs = procs
            .into_iter()
            .flat_map(|mut x| {
                if x.state.side(foe).active().is_alive() && !x.state.side(foe).volatiles.contains(v) {
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

/// A damaging move's deterministic on-hit effects: user self-boosts and any target
/// volatile (Salt Cure, etc.).
fn apply_damage_secondaries(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hit_sub: bool) {
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
    if !sheer_force && md.secondary_chance == 0 {
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
    // A target volatile (Salt Cure, ...) is blocked by a Substitute.
    if !hit_sub {
        if let Some(v) = md.target_volatile {
            let foe = side.other();
            if !b.state.side(foe).volatiles.contains(v) {
                push(b, Instruction::ApplyVolatile { side: foe, volatile: v });
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
fn execute_status_move(b: Branch, side: SideId, md: &crate::data::MoveData, foe_moves_later: bool) -> Vec<Branch> {
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
        let success_p = 1.0 / 3f32.powi(n.min(6) as i32);
        let mut out = Vec::new();
        // Success branch.
        let mut sb = scaled(&b, success_p);
        if !sb.state.side(side).volatiles.contains(VolatileStatus::Protect) {
            push(&mut sb, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Protect });
        }
        push(&mut sb, Instruction::SetStallCounter { side, previous: n, new: n.saturating_add(1) });
        out.push(sb);
        // Failure branch (the move fails, breaking the chain) — only when failure is possible.
        if success_p < 1.0 {
            let mut fb = scaled(&b, 1.0 - success_p);
            if n != 0 {
                push(&mut fb, Instruction::SetStallCounter { side, previous: n, new: 0 });
            }
            out.push(fb);
        }
        return out;
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
    // Future Sight / Doom Desire: schedules an attack on the TARGET side that lands at the
    // end of the second turn from now, computed from the caster's stats at hit time.
    if matches!(md.id.to_id(), "futuresight" | "doomdesire") {
        let mut b = b;
        let target = side.other();
        if b.state.side(target).future_sight.0 == 0 {
            let caster_slot = b.state.side(side).active_index;
            let prev = b.state.side(target).future_sight;
            push(&mut b, Instruction::SetFutureSight { side: target, previous: prev, new: (3, caster_slot) });
        }
        return vec![b];
    }
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

    let hit_prob = accuracy_of(&b, side, md);
    let miss_prob = 1.0 - hit_prob;
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
    let foe_immune = hit.state.side(foe).active().ability == crate::ids::Ability::GoodAsGold;
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
        && status_applies(hit.state.side(foe).active(), md.status)
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

/// Two-turn moves whose user is untargetable during the charge turn.
fn is_semi_invuln_move(id: crate::ids::MoveId) -> bool {
    matches!(
        id.to_id(),
        "fly" | "dig" | "dive" | "bounce" | "phantomforce" | "shadowforce"
    )
}

fn is_two_turn_move(id: crate::ids::MoveId) -> bool {
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
}

/// Whirlwind / Roar / Dragon Tail / Circle Throw: drag the foe into a uniformly-random alive
/// bench mon (each target is its own branch). No-op if the foe has no bench or fainted.
fn apply_drag(b: Branch, dragged: SideId) -> Vec<Branch> {
    if !b.state.side(dragged).active().is_alive() {
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
    let p = 1.0 / bench.len() as f32;
    bench
        .into_iter()
        .map(|t| {
            let mut nb = scaled(&b, p);
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

fn apply_end_of_turn(mut branch: Branch, switched: [bool; 2]) -> Vec<Branch> {
    let b = &mut branch;
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

        // Leech Seed has residual order 8 in PS, before burn/poison (order 9).  This matters
        // when the seeded mon is also in range to faint from status: the drain and heal happen
        // first, then status damage KOs it.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::LeechSeed) {
            let drain = (maxhp / 8).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: drain });
            let other = side.other();
            let (f_alive, f_room, fslot) = {
                let f = b.state.side(other).active();
                (f.is_alive(), f.max_hp - f.hp, b.state.side(other).active_index)
            };
            if f_alive && !heal_blocked(b, other) {
                let heal = drain.min(f_room);
                if heal > 0 {
                    push(b, Instruction::Heal { side: other, slot: fslot, amount: heal });
                }
            }
        }

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
                        let frac = if pstatus == Status::Burn { 16 } else { 8 };
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

        // Partially-trapped (Fire Spin / Whirlpool / Magma Storm / Infestation / ...): 1/8.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::PartiallyTrapped) {
            let dmg = (maxhp / 8).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: dmg });
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
                if p.ability == crate::ids::Ability::Harvest
                    && p.item == Item::None
                    && p.last_berry != Item::None
                    && p.is_alive()
                {
                    let slot = b.state.side(side).active_index;
                    let berry = p.last_berry;
                    let sunny = matches!(effective_weather(&b.state), Weather::Sun | Weather::HarshSun);
                    let mut grow = scaled(&b, if sunny { 1.0 } else { 0.5 });
                    push(&mut grow, Instruction::ChangeItem { side, slot, previous: Item::None, new: berry });
                    push(&mut grow, Instruction::SetLastBerry { side, slot, previous: berry, new: Item::None });
                    // Restoring a berry runs PS's item Update event immediately.  A Harvested
                    // Sitrus is therefore eaten in the same residual event when HP is already
                    // at or below half (it does not wait for the next damage/end-turn check).
                    maybe_eat_sitrus(&mut grow, side);
                    if sunny {
                        vec![grow]
                    } else {
                        vec![grow, scaled(&b, 0.5)]
                    }
                } else {
                    vec![b]
                }
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
                        }
                        nb
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
    }
    out
}
