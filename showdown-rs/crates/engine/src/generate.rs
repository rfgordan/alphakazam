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

    // 1) Switches resolve before moves (deterministic).
    for (side, choice) in [(SideId::One, s1), (SideId::Two, s2)] {
        if let MoveChoice::Switch(target) = choice {
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
    let s = b.state.side(side);
    let previous = s.active_index;
    if previous == target {
        return;
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

    apply_entry_hazards(b, side);
    apply_switch_in_ability(b, side);
}

/// Zero all of a side's active-only state that resets on switch: consecutive-use tracking
/// plus the multi-turn move / restriction / countdown fields. Emitted as explicit reversible
/// deltas so the instruction list stays exactly invertible.
fn reset_move_tracking(b: &mut Branch, side: SideId) {
    use crate::ids::MoveId;
    use crate::instruction::ActiveCounter::{ActiveTurns, Confusion, Perish, Taunt, Yawn};
    let s = b.state.side(side);
    let (lm, streak, stall) = (s.last_used_move, s.move_streak, s.stall_counter);
    let (pending, encore, disable) = (s.pending_move, s.encore, s.disable);
    let (taunt, conf, perish, yawn, active) =
        (s.taunt_turns, s.confusion_turns, s.perish_turns, s.yawn_turns, s.active_turns);
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
        Drought => Weather::Sun,
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
        ElectricSurge => crate::ids::Terrain::Electric,
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
        );
        if b.state.side(foe).active().is_alive() && !untraceable {
            let slot = b.state.side(side).active_index;
            push(b, Instruction::ChangeAbility { side, slot, previous: Trace, new: fa });
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
        if fb.state.side(second.side).active().is_alive() && !flinched {
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
        out.extend(confusion_self_hit(scaled(&b, act * (1.0 / 3.0)), side));
        act *= 2.0 / 3.0;
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
            let dur = if foe_moves_later { 3 } else { 4 };
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
    let move_id = if struggling { move_id } else { b.state.side(side).active().moves[move_idx as usize].id };

    // Disable: the disabled move fails outright; Taunt: status moves fail.
    if !struggling {
        let dis = b.state.side(side).disable;
        if dis.0 != crate::ids::MoveId::None && dis.0 == md.id {
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
    if !executing_charge {
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
        return apply_recharge(out, side, move_id);
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
        }
        if dealt > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: dealt });
        }
        apply_post_damage(&mut hb, side, &md, dealt as i32, dealt > 0, false, (dealt > 0) as u8, calc.life_orb, calc.def_item, calc.def_ability);
        vec![(hb, false)]
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

    // Unaware: the attacker ignores the defender's defensive boosts, and a defender with
    // Unaware ignores the attacker's offensive boosts.
    let atk_boost = if def_ab == Ab::Unaware { 0 } else { b.state.side(side).boost(atk_boost_idx) };
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
        let mut atk_stat = boosted_stat(attacker.stat(atk_idx) as i64, boost);
        // Item stat modifiers (PS applies these via `modify`, round-half-up).
        match (attacker.item, md.category) {
            (Item::ChoiceBand, MoveCategory::Physical) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            (Item::ChoiceSpecs, MoveCategory::Special) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            _ => {}
        }
        // Purifying Salt halves the attacker's offensive stat vs Ghost moves (onSourceModify
        // Atk/SpA chainModify(0.5)) — NOT the final damage, so the rounding point matters.
        if def_ab == Ab::PurifyingSalt && md.typ == Type::Ghost {
            atk_stat = crate::damage::modify(atk_stat, 1, 2);
        }
        if proto_atk {
            atk_stat = crate::damage::modify(atk_stat, 5325, 4096);
        }
        // Offensive ability multipliers.
        match attacker.ability {
            Ab::HugePower | Ab::PurePower => atk_stat = crate::damage::modify(atk_stat, 2, 1),
            Ab::Guts if attacker.status != Status::None => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Technician if md.base_power <= 60 => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Overgrow if md.typ == Type::Grass && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Blaze if md.typ == Type::Fire && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Torrent if md.typ == Type::Water && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Swarm if md.typ == Type::Bug && pinch => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            // Sheer Force: ×1.3 when the move has a secondary (the secondary is then removed).
            Ab::SheerForce if md.secondary_chance > 0 => atk_stat = crate::damage::modify(atk_stat, 5325, 4096),
            Ab::Reckless if md.recoil.0 > 0 => atk_stat = crate::damage::modify(atk_stat, 4915, 4096),
            Ab::Defeatist if (attacker.hp as i32) * 2 <= attacker.max_hp as i32 => atk_stat = crate::damage::modify(atk_stat, 1, 2),
            Ab::ToughClaws if md.flag_contact => atk_stat = crate::damage::modify(atk_stat, 5325, 4096), // ×1.3
            Ab::IronFist if md.flag_punch => atk_stat = crate::damage::modify(atk_stat, 4915, 4096), // ×1.2
            Ab::StrongJaw if md.flag_bite => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::Sharpness if md.flag_slicing => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::MegaLauncher if md.flag_pulse => atk_stat = crate::damage::modify(atk_stat, 3, 2),
            Ab::PunkRock if md.flag_sound => atk_stat = crate::damage::modify(atk_stat, 5325, 4096), // ×1.3
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

    // Final defender-side damage modifiers (stack multiplicatively).
    let (mut fnum, mut fden) = (1i64, 1i64);
    if matches!(def_ab, Ab::Multiscale | Ab::ShadowShield) && defender.hp == defender.max_hp {
        fden *= 2;
    }
    let scrappy = attacker.ability == Ab::Scrappy;
    let def_types_eff = effective_def_types(scrappy, md.typ, defender.types);
    let type_mult = crate::damage::type_multiplier(md.typ, def_types_eff);
    if matches!(def_ab, Ab::Filter | Ab::SolidRock | Ab::PrismArmor) && type_mult > 1.0 {
        fnum *= 3072;
        fden *= 4096;
    }
    if def_ab == Ab::IceScales && md.category == MoveCategory::Special {
        fden *= 2;
    }
    // Punk Rock halves sound-move damage taken.
    if def_ab == Ab::PunkRock && md.flag_sound {
        fden *= 2;
    }
    // Attacker final-damage modifiers keyed on effectiveness / item.
    if attacker.ability == Ab::TintedLens && type_mult < 1.0 {
        fnum *= 2;
    }
    if attacker.ability == Ab::Neuroforce && type_mult > 1.0 {
        fnum *= 5120;
        fden *= 4096;
    }
    if attacker.item == Item::ExpertBelt && type_mult > 1.0 {
        fnum *= 4915;
        fden *= 4096;
    }
    if attacker.item == Item::MuscleBand && md.category == MoveCategory::Physical {
        fnum *= 4505;
        fden *= 4096;
    }
    if attacker.item == Item::WiseGlasses && md.category == MoveCategory::Special {
        fnum *= 4505;
        fden *= 4096;
    }
    let adaptability = attacker.ability == Ab::Adaptability;
    // Returned for post-damage (contact punishers); also suppressed under Mold Breaker.
    let def_ability = def_ab;
    let def_item = defender.item;
    let def_maxhp = defender.max_hp;
    let life_orb = attacker.item == Item::LifeOrb;

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
        life_orb,
        adaptability,
        final_num: fnum,
        final_den: fden,
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
        input.final_den *= 2;
    }
    let rolls_nocrit = damage_rolls(&input);

    DamageCalc { rolls_nocrit, rolls_crit, def_ability, def_item, def_maxhp, life_orb }
}

/// Applies a damaging move's hits sequentially (each its own roll and crit), clamped to HP
/// and routed through any Substitute; returns true if a Substitute absorbed the damage (so
/// the target's own secondaries/volatiles are blocked).
fn apply_damage_hit(b: &mut Branch, side: SideId, md: &crate::data::MoveData, hits: &[(u8, bool)]) -> bool {
    use crate::ids::Ability as Ab;
    let foe = side.other();
    let DamageCalc { rolls_nocrit, rolls_crit, def_ability, def_item, def_maxhp, life_orb } =
        compute_damage(b, side, md);
    // Apply each hit's damage independently (own roll and crit), clamped to current HP
    // (a hit that faints the target ends the sequence; remaining hits add nothing).
    let mut any_damage = false;
    let mut hit_sub = false;
    let mut total_dealt: i32 = 0;
    let mut hits_landed: u8 = 0;
    for &(roll, crit) in hits {
        let rolls = if crit { &rolls_crit } else { &rolls_nocrit };
        let raw = rolls[roll as usize];
        // Route to the Substitute if the target has one up (it absorbs the whole hit).
        let sub_hp = b.state.side(foe).substitute_hp;
        if sub_hp > 0 && b.state.side(foe).volatiles.contains(VolatileStatus::Substitute) {
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
}

/// Sitrus Berry: when the holder's HP is at or below 1/2, it eats the berry and heals 1/4
/// of max HP. The berry's *consumption* isn't compared (item is excluded from `relaxed_eq`,
/// and the harness re-projects PS's pre-turn item each turn), so we only emit the heal.
fn apply_pinch_berry(b: &mut Branch, side: SideId) {
    // Unnerve on the opponent's active suppresses this side's berries.
    if b.state.side(side.other()).active().ability == crate::ids::Ability::Unnerve {
        return;
    }
    let p = b.state.side(side).active();
    if p.is_alive() && p.item == Item::SitrusBerry && p.hp * 2 <= p.max_hp {
        let heal = (p.max_hp / 4).min(p.max_hp - p.hp);
        if heal > 0 {
            let slot = b.state.side(side).active_index;
            push(b, Instruction::Heal { side, slot, amount: heal });
        }
    }
}

/// White Herb: once any of the holder's stats is below 0, it restores every negative stage
/// to 0. Consumption isn't compared (item excluded + re-projected), so we only emit the
/// restoring boosts. Triggers regardless of who caused the drop (self-drops included).
fn apply_white_herb(b: &mut Branch, side: SideId) {
    if b.state.side(side).active().item != Item::WhiteHerb {
        return;
    }
    for stat in BOOST_ORDER {
        let cur = b.state.side(side).boost(stat);
        if cur < 0 {
            push(b, Instruction::Boost { side, stat, amount: -cur });
        }
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
        on_item_lost(b, side);
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
    let p = b.state.side(side).active();
    if p.ability == crate::ids::Ability::Unburden
        && !b.state.side(side).volatiles.contains(VolatileStatus::Unburden)
    {
        push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Unburden });
    }
}

/// Berry consumption: item-loss bookkeeping plus Cheek Pouch's 1/3 max HP heal.
fn on_berry_eaten(b: &mut Branch, side: SideId) {
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
        || !b.state.side(foe).active().is_alive()
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
    if !b.state.side(foe).active().is_alive() {
        return vec![b];
    }
    // Shield Dust / Covert Cloak block chance-based secondaries against the holder.
    if b.state.side(foe).active().ability == crate::ids::Ability::ShieldDust
        || b.state.side(foe).active().item == Item::CovertCloak
    {
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
    let mut lowered = false;
    for (i, &delta) in md.secondary_boosts.iter().enumerate() {
        if delta != 0 {
            lowered |= apply_boost_clamped(&mut proc, foe, BOOST_ORDER[i], delta) < 0;
        }
    }
    if lowered {
        react_to_stat_drop(&mut proc, foe);
    }
    let mut applied_sleep = false;
    if md.secondary_status != Status::None
        && status_applies(proc.state.side(foe).active(), md.secondary_status)
        && !status_blocked_by_field(&proc.state, foe, md.secondary_status)
    {
        let slot = proc.state.side(foe).active_index;
        push(&mut proc, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.secondary_status });
        applied_sleep = md.secondary_status == Status::Sleep;
        apply_synchronize(&mut proc, foe, md.secondary_status);
        consume_lum_if_statused(&mut proc, foe);
        applied_sleep = applied_sleep && proc.state.side(foe).active().status == Status::Sleep;
    }
    let mut procs = vec![proc];
    if applied_sleep {
        procs = procs.into_iter().flat_map(|x| branch_sleep_counter(x, foe)).collect();
    }
    // Chance-based volatile secondaries (Hurricane / Dynamic Punch confusion, Dire Claw ...).
    if let Some(v) = md.secondary_volatile {
        procs = procs
            .into_iter()
            .flat_map(|mut x| {
                if x.state.side(foe).active().is_alive() && !x.state.side(foe).volatiles.contains(v) {
                    push(&mut x, Instruction::ApplyVolatile { side: foe, volatile: v });
                    if v == VolatileStatus::Confusion {
                        return branch_confusion_counter(x, foe);
                    }
                }
                vec![x]
            })
            .collect();
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

    // Protect family: succeeds with probability 1/3^n on the (n+1)ᵗʰ consecutive use (n is
    // the stall counter). Success sets the Protect volatile and bumps the counter; failure
    // resets it. We enumerate both branches so PS's actual outcome is always a member.
    if is_protect_move(md.id) {
        let n = b.state.side(side).stall_counter;
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
        if hp < maxhp {
            let slot = b.state.side(side).active_index;
            // Chesto Berry immediately cures the Rest sleep, so the user stays awake.
            if status != Status::Sleep && item != Item::ChestoBerry {
                push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::Sleep });
                // Rest's sleep is a fixed 2-turn nap (PS statusState.time = 3).
                let prev_ctr = b.state.side(side).active().status_counter;
                push(&mut b, Instruction::ChangeStatusCounter { side, slot, previous: prev_ctr, new: 3 });
            } else if status != Status::None && item == Item::ChestoBerry {
                // Rest first cures the prior status; Chesto then prevents the new sleep.
                push(&mut b, Instruction::ChangeStatus { side, slot, previous: status, new: Status::None });
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
    {
        let slot = hit.state.side(foe).active_index;
        push(&mut hit, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.status });
        applied_sleep = md.status == Status::Sleep;
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
        final_num: 1,
        final_den: if screened { 2 } else { 1 },
    };
    crate::damage::damage_rolls(&input)
}

// --- end of turn -------------------------------------------------------------

fn apply_end_of_turn(mut branch: Branch, switched: [bool; 2]) -> Vec<Branch> {
    let b = &mut branch;
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
                if heal16 && p.hp < p.max_hp {
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
        if p.item == Item::Leftovers && p.hp < p.max_hp && p.is_alive() {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
        }
        // Black Sludge: Leftovers for Poison types; 1/8 chip for anyone else.
        let p = b.state.side(side).active();
        if p.item == Item::BlackSludge && p.is_alive() {
            if p.types.contains(&Type::Poison) {
                if p.hp < p.max_hp {
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
        if b.state.terrain == crate::ids::Terrain::Grassy && grounded && p.hp < p.max_hp && p.is_alive() {
            let heal = (maxhp / 16).max(1).min(p.max_hp - p.hp);
            push(b, Instruction::Heal { side, slot, amount: heal });
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
                if heal > 0 {
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

        // Leech Seed: the seeded active loses 1/8 max HP and the opposing active heals that
        // amount (Magic Guard prevents the drain entirely).
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::LeechSeed) {
            let drain = (maxhp / 8).max(1).min(p.hp);
            push(b, Instruction::Damage { side, slot, amount: drain });
            let other = side.other();
            let (f_alive, f_room, fslot) = {
                let f = b.state.side(other).active();
                (f.is_alive(), f.max_hp - f.hp, b.state.side(other).active_index)
            };
            if f_alive {
                let heal = drain.min(f_room);
                if heal > 0 {
                    push(b, Instruction::Heal { side: other, slot: fslot, amount: heal });
                }
            }
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
    // Weather / terrain / Trick Room count down each end of turn and expire at 0.
    if b.state.weather != Weather::None && b.state.weather_turns > 0 {
        push(b, Instruction::DecrementWeatherTurns);
        if b.state.weather_turns == 0 {
            set_weather(b, Weather::None, 0);
        }
    }
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
        // Wish: ticks each end of turn; heals the slot's occupant when it lands.
        let wish = b.state.side(side).wish;
        if wish.0 > 0 {
            let landed = wish.0 == 1;
            let new = if landed { (0, 0) } else { (wish.0 - 1, wish.1) };
            push(b, Instruction::SetWish { side, previous: wish, new });
            if landed {
                let p = b.state.side(side).active();
                if p.is_alive() && p.hp < p.max_hp {
                    let amt = wish.1.min(p.max_hp - p.hp);
                    let slot = b.state.side(side).active_index;
                    push(b, Instruction::Heal { side, slot, amount: amt });
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

    // Yawn expiry: the drowsy mon falls asleep now (stochastic 1-3 turn duration).
    let mut out = vec![branch];
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
                {
                    let slot = x.state.side(side).active_index;
                    push(&mut x, Instruction::ChangeStatus { side, slot, previous: Status::None, new: Status::Sleep });
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
