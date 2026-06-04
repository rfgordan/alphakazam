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

/// Effective speed including boost, paralysis, Choice Scarf, Tailwind and a Speed-based
/// Protosynthesis / Quark Drive boost.
fn effective_speed(state: &State, side: SideId) -> i32 {
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
fn is_grounded(state: &State, side: SideId) -> bool {
    let p = state.side(side).active();
    !(p.types.contains(&Type::Flying))
}

/// Public entry point. `s1`/`s2` are side one's and side two's chosen actions.
pub fn generate_instructions(state: &State, s1: MoveChoice, s2: MoveChoice) -> Vec<StateInstructions> {
    generate_instructions_ex(state, s1, s2, [None, None])
}

/// Like [`generate_instructions`], but `pivot` gives each side's switch-in target for a
/// pivot move (U-turn): when that move connects and the user survives, it switches out
/// mid-turn — so a faster pivot's switch happens *before* the opponent's move. Used by
/// the differential harness, which knows the recorded replacement target.
pub fn generate_instructions_ex(state: &State, s1: MoveChoice, s2: MoveChoice, pivot: [Option<u8>; 2]) -> Vec<StateInstructions> {
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

    // 2) Moves, ordered by priority then effective speed (speed ties branch 50/50).
    let move_actions: Vec<Action> = [(SideId::One, s1, pivot[0]), (SideId::Two, s2, pivot[1])]
        .into_iter()
        .filter_map(|(side, c, pv)| match c {
            MoveChoice::Move(idx) => Some(Action { side, move_idx: idx, pivot: pv }),
            MoveChoice::Switch(_) => None,
        })
        .collect();

    branches = resolve_moves(branches, &move_actions);

    // 3) End-of-turn residuals (deterministic) — skipped if the battle has ended (a side
    //    has no living Pokémon), matching PS, which stops the turn on a win.
    for b in &mut branches {
        if !battle_over(&b.state) {
            apply_end_of_turn(b);
        }
    }

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
    push(b, Instruction::Switch { side, previous, next: target });

    apply_entry_hazards(b, side);
    apply_switch_in_ability(b, side);
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
        set_weather(b, weather, 5);
    }
    // Intimidate: lower the opposing active's Attack by 1 on switch-in.
    if ability == IntimidateAbility {
        let foe = side.other();
        if b.state.side(foe).active().is_alive() {
            if apply_boost_clamped(b, foe, BoostIndex::Attack, -1) < 0 {
                react_to_stat_drop(b, foe);
            }
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

    // Stealth Rock — hits everything, scaled by Rock effectiveness.
    if s.side_conditions.stealth_rock {
        let mult = type_multiplier(Type::Rock, p.types);
        let dmg = ((maxhp as f32 / 8.0) * mult).floor() as i16;
        let dmg = dmg.max(1).min(p.hp);
        if dmg > 0 {
            push(b, Instruction::Damage { side, slot, amount: dmg });
        }
    }
    // Spikes — grounded only.
    let layers = b.state.side(side).side_conditions.spikes;
    if grounded && layers > 0 {
        let frac = match layers { 1 => 8, 2 => 6, _ => 4 };
        let p = b.state.side(side).active();
        let dmg = (p.max_hp / frac).max(1).min(p.hp);
        if dmg > 0 {
            push(b, Instruction::Damage { side, slot, amount: dmg });
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

fn sequence_two_moves(b: Branch, first: Action, second: Action) -> Vec<Branch> {
    let mut out = Vec::new();
    for fb in execute_move(b, first) {
        // Second mover only acts if its active is still alive.
        if fb.state.side(second.side).active().is_alive() {
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
    // Note: ChoiceLock / Protosynthesis / QuarkDrive intentionally persist through our
    // switch reset model is N/A — they are not in this list so a switch won't clear
    // ability-driven volatiles incorrectly; they're re-derived on switch-in (TODO).
];

/// Per-hit critical-hit probability (gen9 base, no crit-stage modifiers modeled).
const CRIT: f32 = 1.0 / 24.0;

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

/// Execute one move from `action.side`, returning the resulting branches.
fn execute_move(b: Branch, action: Action) -> Vec<Branch> {
    let Action { side, move_idx, pivot } = action;
    let attacker = b.state.side(side).active();
    if !attacker.is_alive() {
        return vec![b];
    }
    let move_id = attacker.moves[move_idx as usize].id;
    let md = move_data(move_id);
    let foe = side.other();

    let mut b = b;
    let slot = b.state.side(side).active_index;

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

    // PP decrement (every used move).
    if b.state.side(side).active().moves[move_idx as usize].pp > 0 {
        push(&mut b, Instruction::DecrementPp { side, slot, move_index: move_idx, amount: 1 });
    }

    // Status moves handled specially.
    if md.category == MoveCategory::Status {
        let mut branches = execute_status_move(b, side, &md);
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

    // Damaging move: branch on accuracy (hit/miss), then crit, then the 16 rolls.
    let mut out = Vec::new();
    let acc = md.accuracy;
    let (hit_prob, miss_prob) = if acc == 0 || acc >= 100 { (1.0, 0.0) } else { (acc as f32 / 100.0, 1.0 - acc as f32 / 100.0) };

    if miss_prob > 0.0 {
        out.push(scaled(&b, miss_prob)); // miss: nothing further happens
    }

    let foe_alive = b.state.side(foe).active().is_alive();
    if !foe_alive {
        out.push(scaled(&b, hit_prob));
        return out;
    }

    // A type-immune move (e.g. Close Combat vs a Ghost, or Ground vs Levitate) deals no
    // damage and skips its self-stat secondary — PS only applies `self` boosts on a hit.
    let defender = b.state.side(foe).active();
    let flag_immune = (md.flag_sound && defender.ability == crate::ids::Ability::Soundproof)
        || (md.flag_bullet && defender.ability == crate::ids::Ability::Bulletproof);
    let connects = crate::damage::type_multiplier(md.typ, defender.types) != 0.0
        && !ability_immune(md.typ, defender.ability)
        && !flag_immune;
    if !connects {
        out.push(scaled(&b, hit_prob));
        return out;
    }

    // Each hit rolls damage (16) and crit independently. For small hit counts we enumerate
    // the full per-hit product (exact, and preserves Substitute/Sturdy interleaving). For
    // large counts (Population Bomb's 10 hits → 32¹⁰ branches) that explodes the allocator
    // and crashes the machine, so we instead enumerate the distinct *total* damage via a
    // sumset DP — same set of observable result states, bounded memory.
    let hits_min = md.hits.max(1) as usize;
    let hits_max = (md.hits_max as usize).max(hits_min);
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
        apply_post_damage(&mut hb, side, &md, dealt as i32, dealt > 0, false, calc.life_orb, calc.def_item, calc.def_ability);
        vec![(hb, false)]
    } else if hits_min == hits_max && hits_min <= MAX_EXACT_HITS {
        let mut v = Vec::new();
        for combo in HitCombos::new(hits_min) {
            let mut prob = hit_prob;
            for &(_, crit) in &combo {
                prob *= (1.0 / 16.0) * if crit { CRIT } else { 1.0 - CRIT };
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
    out
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
    let foe = side.other();
    let attacker = b.state.side(side).active();
    let defender = b.state.side(foe).active();

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
    let atk_boost = if defender.ability == crate::ids::Ability::Unaware { 0 } else { b.state.side(side).boost(atk_boost_idx) };
    let def_boost = if attacker.ability == crate::ids::Ability::Unaware { 0 } else { b.state.side(foe).boost(def_boost_idx) };
    let mut atk_stat = (attacker.stat(atk_idx) as f32 * boost_multiplier(atk_boost)) as i64;
    let mut def_stat = (defender.stat(def_idx) as f32 * boost_multiplier(def_boost)) as i64;

    // Item stat modifiers (PS applies these via `modify`, round-half-up).
    match (attacker.item, md.category) {
        (Item::ChoiceBand, MoveCategory::Physical) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
        (Item::ChoiceSpecs, MoveCategory::Special) => atk_stat = crate::damage::modify(atk_stat, 3, 2),
        _ => {}
    }
    if defender.item == Item::AssaultVest && md.category == MoveCategory::Special {
        def_stat = crate::damage::modify(def_stat, 3, 2);
    }
    // Eviolite: ×1.5 to the defensive stat (Def and SpD) of a not-fully-evolved Pokémon.
    if defender.item == Item::Eviolite && crate::data::species_is_nfe(defender.species) {
        def_stat = crate::damage::modify(def_stat, 3, 2);
    }
    // Purifying Salt halves the attacker's offensive stat vs Ghost moves (onSourceModify
    // Atk/SpA chainModify(0.5)) — NOT the final damage, so the rounding point matters.
    if defender.ability == crate::ids::Ability::PurifyingSalt && md.typ == Type::Ghost {
        atk_stat = crate::damage::modify(atk_stat, 1, 2);
    }
    // Supreme Overlord: +10% to the offensive stat per fallen ally (max 5).
    if attacker.ability == crate::ids::Ability::SupremeOverlord {
        let fallen = b.state.side(side).pokemon.iter()
            .filter(|p| p.species != crate::ids::Species::None && p.hp <= 0)
            .count()
            .min(5) as i64;
        if fallen > 0 {
            atk_stat = crate::damage::modify(atk_stat, 10 + fallen, 10);
        }
    }
    // Protosynthesis / Quark Drive on the boosted offensive / defensive stat. PS uses
    // chainModify([5325, 4096]) — modifier 5325, NOT 13/10 (which rounds to 5324).
    //
    // Offensive modifiers run on the *category* stat event (ModifyAtk for physical,
    // ModifySpA for special) regardless of `overrideOffensiveStat` — so Body Press
    // (physical, reads Defense) is boosted by an 'atk' best-stat. We therefore compare
    // proto_stat to the category offensive stat, not the stat actually read.
    let category_off_stat = if md.category == MoveCategory::Physical {
        crate::ids::StatIndex::Attack
    } else {
        crate::ids::StatIndex::SpecialAttack
    };
    if has_proto(b.state.side(side)) && proto_stat(attacker) == category_off_stat {
        atk_stat = crate::damage::modify(atk_stat, 5325, 4096);
    }
    if has_proto(b.state.side(foe)) && proto_stat(defender) == def_idx {
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
    // Offensive ability multipliers.
    use crate::ids::Ability as Ab;
    let pinch = (attacker.hp as i32) * 3 <= attacker.max_hp as i32; // HP ≤ 1/3
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
        _ => {}
    }
    // Thick Fat (defender) halves the attack of Fire/Ice moves.
    if defender.ability == Ab::ThickFat && (md.typ == Type::Fire || md.typ == Type::Ice) {
        atk_stat = crate::damage::modify(atk_stat, 1, 2);
    }
    // Marvel Scale / Fur Coat (defender) raise physical Defense.
    if md.category == MoveCategory::Physical && def_idx == crate::ids::StatIndex::Defense {
        if defender.ability == Ab::FurCoat {
            def_stat = crate::damage::modify(def_stat, 2, 1);
        } else if defender.ability == Ab::MarvelScale && defender.status != Status::None {
            def_stat = crate::damage::modify(def_stat, 3, 2);
        }
    }
    // Guts ignores the burn attack drop.
    let burned = attacker.status == Status::Burn && attacker.ability != Ab::Guts;

    // Final defender-side damage modifiers (stack multiplicatively).
    let (mut fnum, mut fden) = (1i64, 1i64);
    if matches!(defender.ability, Ab::Multiscale | Ab::ShadowShield) && defender.hp == defender.max_hp {
        fden *= 2;
    }
    let type_mult = crate::damage::type_multiplier(md.typ, defender.types);
    if matches!(defender.ability, Ab::Filter | Ab::SolidRock | Ab::PrismArmor) && type_mult > 1.0 {
        fnum *= 3072;
        fden *= 4096;
    }
    if defender.ability == Ab::IceScales && md.category == MoveCategory::Special {
        fden *= 2;
    }
    // Punk Rock halves sound-move damage taken.
    if defender.ability == Ab::PunkRock && md.flag_sound {
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
    let def_ability = defender.ability;
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
            let w = crate::data::species_weight_hg(defender.species);
            base_power = if w >= 2000 { 120 } else if w >= 1000 { 100 } else if w >= 500 { 80 }
                else if w >= 250 { 60 } else if w >= 100 { 40 } else { 20 };
        }
        "heavyslam" | "heatcrash" => {
            let wu = crate::data::species_weight_hg(attacker.species).max(1);
            let wt = crate::data::species_weight_hg(defender.species).max(1);
            let ratio = wu / wt;
            base_power = if ratio >= 5 { 120 } else if ratio >= 4 { 100 } else if ratio >= 3 { 80 }
                else if ratio >= 2 { 60 } else { 40 };
        }
        _ => {}
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
        defender_types: defender.types,
        attack_stat: atk_stat as i16,
        defense_stat: def_stat.max(1) as i16,
        is_crit: false,
        attacker_burned: burned,
        weather: b.state.weather,
        terastallized: attacker.terastallized,
        tera_type: attacker.tera_type,
        life_orb,
        adaptability,
        final_num: fnum,
        final_den: fden,
    };
    // Crit rolls are computed from the screen-free modifiers (a crit ignores screens).
    let mut input = input;
    let mut input_crit = input;
    input_crit.is_crit = true;
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
        }
    }
    apply_post_damage(b, side, md, total_dealt, any_damage, hit_sub, life_orb, def_item, def_ability);
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
        if md.recoil.0 > 0 {
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
}

/// Sitrus Berry: when the holder's HP is at or below 1/2, it eats the berry and heals 1/4
/// of max HP. The berry's *consumption* isn't compared (item is excluded from `relaxed_eq`,
/// and the harness re-projects PS's pre-turn item each turn), so we only emit the heal.
fn apply_pinch_berry(b: &mut Branch, side: SideId) {
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
    for i in 0..16 {
        per_hit.push((calc.rolls_nocrit[i] as i32, (1.0 / 16.0) * (1.0 - CRIT)));
        per_hit.push((calc.rolls_crit[i] as i32, (1.0 / 16.0) * CRIT));
    }

    // Convolve the per-hit distribution up to `max` times, clamping cumulative damage at the
    // target's HP (all overkill collapses to one faint outcome, bounding the support size).
    // After the kᵗʰ convolution, mix in the branch for "exactly k hits" weighted by P(k).
    let cap = b.state.side(foe).active().hp.max(0) as i32;
    let mut conv: HashMap<i32, f32> = HashMap::new();
    conv.insert(0, 1.0);
    let mut dist: HashMap<i32, f32> = HashMap::new();
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
                *dist.entry(t).or_insert(0.0) += pk * p;
            }
        }
    }

    // One branch per distinct total damage.
    let mut out = Vec::with_capacity(dist.len());
    for (total, p) in dist {
        let mut hb = scaled(b, hit_prob * p);
        if total > 0 {
            let slot = hb.state.side(foe).active_index;
            push(&mut hb, Instruction::Damage { side: foe, slot, amount: total as i16 });
        }
        apply_post_damage(&mut hb, side, md, total, total > 0, false, calc.life_orb, calc.def_item, calc.def_ability);
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
    // Purifying Salt grants blanket status immunity (to both moves and secondaries).
    if p.ability == crate::ids::Ability::PurifyingSalt {
        return false;
    }
    // A Lum Berry holder immediately cures any inflicted status, so it never sticks (the
    // berry's consumption isn't compared, and the holder's item is re-projected each turn).
    if p.item == crate::ids::Item::LumBerry {
        return false;
    }
    match status {
        Status::Burn => !p.types.contains(&Type::Fire),
        Status::Paralysis => !p.types.contains(&Type::Electric),
        Status::Poison | Status::Toxic => !(p.types.contains(&Type::Poison) || p.types.contains(&Type::Steel)),
        _ => true,
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
    if !md.flag_contact {
        return vec![b];
    }
    let foe = side.other();
    let def_ab = b.state.side(foe).active().ability;
    let (target, status) = match def_ab {
        Ab::FlameBody => (side, Status::Burn),
        Ab::Static => (side, Status::Paralysis),
        Ab::PoisonPoint => (side, Status::Poison),
        _ if b.state.side(side).active().ability == Ab::PoisonTouch => (foe, Status::Poison),
        _ => return vec![b],
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
    if md.secondary_status != Status::None && status_applies(proc.state.side(foe).active(), md.secondary_status) {
        let slot = proc.state.side(foe).active_index;
        push(&mut proc, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.secondary_status });
    }
    vec![proc, noproc]
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
    }
}

/// Execute a status move from its data: self-heal, hazard, and/or target status, with an
/// accuracy hit/miss branch when the move can miss.
fn execute_status_move(b: Branch, side: SideId, md: &crate::data::MoveData) -> Vec<Branch> {
    let foe = side.other();

    // Protect family: set the Protect volatile on the user (blocks the foe's move).
    if is_protect_move(md.id) {
        let mut b = b;
        if !b.state.side(side).volatiles.contains(VolatileStatus::Protect) {
            push(&mut b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::Protect });
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

    let (hit_prob, miss_prob) = if md.accuracy == 0 || md.accuracy >= 100 {
        (1.0, 0.0)
    } else {
        (md.accuracy as f32 / 100.0, 1.0 - md.accuracy as f32 / 100.0)
    };
    let mut hit = scaled(&b, hit_prob);

    if md.heal.0 > 0 {
        let p = hit.state.side(side).active();
        let amount = ((p.max_hp as i32 * md.heal.0 as i32 / md.heal.1 as i32) as i16).min(p.max_hp - p.hp);
        if amount > 0 {
            let slot = hit.state.side(side).active_index;
            push(&mut hit, Instruction::Heal { side, slot, amount });
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
        apply_hazard(&mut hit, foe, sc);
    }
    if md.weather != Weather::None && hit.state.weather != md.weather {
        set_weather(&mut hit, md.weather, 5);
    }
    if md.status != Status::None && !foe_immune && status_applies(hit.state.side(foe).active(), md.status) {
        let slot = hit.state.side(foe).active_index;
        push(&mut hit, Instruction::ChangeStatus { side: foe, slot, previous: Status::None, new: md.status });
    }

    if miss_prob > 0.0 {
        vec![hit, scaled(&b, miss_prob)]
    } else {
        vec![hit]
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

// --- end of turn -------------------------------------------------------------

fn apply_end_of_turn(b: &mut Branch) {
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

        // Sandstorm chip.
        if b.state.weather == Weather::Sand && !magic_guard {
            let immune = p.types.contains(&Type::Rock)
                || p.types.contains(&Type::Ground)
                || p.types.contains(&Type::Steel);
            if !immune {
                let dmg = (maxhp / 16).max(1).min(b.state.side(side).active().hp);
                if dmg > 0 {
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

        // Salt Cure.
        let p = b.state.side(side).active();
        if p.is_alive() && !magic_guard && b.state.side(side).volatiles.contains(VolatileStatus::SaltCure) {
            let heavy = p.types.contains(&Type::Water) || p.types.contains(&Type::Steel);
            let frac = if heavy { 4 } else { 8 };
            let dmg = (maxhp / frac).max(1).min(p.hp);
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
    }
}
