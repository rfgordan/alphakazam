//! Transition verification: replay a turn through `generate_instructions` and check
//! that PS's actual next state is among the engine's enumerated outcome branches.
//!
//! Because the engine enumerates every branch (each accuracy / crit / damage-roll
//! outcome), we don't need to know which roll PS picked — we just require membership.
//! Comparison is *relaxed* to the mechanics currently modeled (HP, status, active slot,
//! boosts, hazards, weather); volatiles and item/ability-driven fields are ignored until
//! those systems are implemented, so the signal reflects real coverage rather than noise.

use engine::generate::{generate_instructions_ex, switch_into, MoveChoice};
use engine::state::{SideId, State};

use crate::convert::{parse_state, project_state, Unmapped};
use crate::trace::{Choices, ReplacementList, TSide, TState};

/// Parse a PS choice string (`"move 1"`, `"switch 2"`) into a `MoveChoice`.
pub fn parse_choice(s: &str) -> Option<MoveChoice> {
    let mut it = s.split_whitespace();
    match it.next()? {
        "move" => {
            let n: u8 = it.next()?.parse().ok()?;
            Some(MoveChoice::Move(n.saturating_sub(1)))
        }
        "switch" => {
            let n: u8 = it.next()?.parse().ok()?;
            Some(MoveChoice::Switch(n.saturating_sub(1)))
        }
        _ => None, // default / pass / team — not a replayable single action
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TurnResult {
    /// PS's next state matched one of the engine's branches.
    Match,
    /// Both choices replayable, but no branch reproduced PS's state.
    Mismatch,
    /// Couldn't replay (a missing/non-action choice, e.g. a forced mid-turn switch).
    Skipped,
}

/// Parse a side's ordered replacement strings into canonical switch-target indices.
fn switch_list(strs: &[String]) -> Vec<u8> {
    strs.iter()
        .filter_map(|s| match parse_choice(s) {
            Some(MoveChoice::Switch(t)) => Some(t),
            _ => None,
        })
        .collect()
}

/// Generate every candidate resulting state for a turn. Replacements happen in order: a
/// U-turn pivot (mid-turn, inside the engine), then a faint replacement (post-turn) if
/// the side's active ends up fainted.
fn candidate_states(prev_state: &State, m1: MoveChoice, m2: MoveChoice, replacements: &ReplacementList) -> Vec<State> {
    let lists = [switch_list(&replacements.p1), switch_list(&replacements.p2)];
    // The first replacement is the U-turn pivot target if the side used a pivot move.
    let pivots = [
        if used_pivot_move(prev_state, SideId::One, m1) { lists[0].first().copied() } else { None },
        if used_pivot_move(prev_state, SideId::Two, m2) { lists[1].first().copied() } else { None },
    ];

    let mut out = Vec::new();
    for si in generate_instructions_ex(prev_state, m1, m2, pivots) {
        let mut cand: State = *prev_state; // Copy
        cand.apply_instructions(&si.instructions);
        // Faint replacement: index 1 if this side already used its first switch as a
        // pivot, else index 0.
        for (i, side) in [SideId::One, SideId::Two].into_iter().enumerate() {
            if !cand.side(side).active().is_alive() {
                let idx = if pivots[i].is_some() { 1 } else { 0 };
                if let Some(&target) = lists[i].get(idx) {
                    switch_into(&mut cand, side, target);
                }
            }
        }
        out.push(cand);
    }
    out
}

/// True if the side's chosen action is a pivot move (U-turn) that triggers a switch-out.
fn used_pivot_move(state: &State, side: SideId, mc: MoveChoice) -> bool {
    if let MoveChoice::Move(idx) = mc {
        let id = state.side(side).active().moves[idx as usize].id;
        engine::data::move_data(id).self_switch
    } else {
        false
    }
}

/// Like `check_turn`, but on a mismatch also returns a coarse category for the closest
/// candidate's first diverging field (for aggregating failure causes across many turns).
pub fn analyze_turn(prev: &TState, choices: &Choices, replacements: &ReplacementList, target: &TState) -> (TurnResult, Option<String>) {
    let r = check_turn(prev, choices, replacements, target);
    if r != TurnResult::Mismatch {
        return (r, None);
    }
    let (c1, c2) = match (&choices.p1, &choices.p2) {
        (Some(a), Some(b)) => (a, b),
        _ => return (r, None),
    };
    let (m1, m2) = match (parse_choice(c1), parse_choice(c2)) {
        (Some(a), Some(b)) => (a, b),
        _ => return (r, None),
    };
    let mut unmapped = Unmapped::default();
    let prev_state = parse_state(prev, &mut unmapped);
    let target_proj = project_state(&parse_state(target, &mut unmapped));
    // Closest candidate by active-HP distance, then categorize its first relaxed diff.
    let mut best: Option<(i32, String)> = None;
    for cand in candidate_states(&prev_state, m1, m2, replacements) {
        let proj = project_state(&cand);
        let dist = active_hp_distance(&proj, &target_proj);
        let reason = relaxed_diff(&proj, &target_proj).map(|d| categorize(&d)).unwrap_or_else(|| "match".into());
        if best.as_ref().map_or(true, |(d, _)| dist < *d) {
            best = Some((dist, reason));
        }
    }
    (r, best.map(|(_, c)| c))
}

/// Compact, tally-able context line for a mismatching turn (move/ability/item per side).
pub fn dump_context(prev: &TState, choices: &Choices, cause: &str) -> String {
    let mut unmapped = Unmapped::default();
    let st = parse_state(prev, &mut unmapped);
    let part = |side: SideId, choice: &Option<String>| -> String {
        let p = st.side(side).active();
        let mv = match choice.as_deref().and_then(parse_choice) {
            Some(MoveChoice::Move(i)) => p.moves[i as usize].id.to_id().to_string(),
            Some(MoveChoice::Switch(_)) => "switch".to_string(),
            None => "?".to_string(),
        };
        format!("m={mv} a={:?} i={:?}", p.ability, p.item)
    };
    format!("MM {cause} | p1 {} | p2 {}", part(SideId::One, &choices.p1), part(SideId::Two, &choices.p2))
}

/// Bucket a diff message into a coarse cause for aggregation.
fn categorize(diff: &str) -> String {
    if diff.contains("hp") { "hp".into() }
    else if diff.contains("status") { "status".into() }
    else if diff.contains("boosts") { "boosts".into() }
    else if diff.contains("hazards") { "hazards".into() }
    else if diff.contains("active_index") { "active_index".into() }
    else if diff.contains("weather") { "weather".into() }
    else { "other".into() }
}

/// Replay one turn: from `prev` state, apply both choices, test membership of `target`.
pub fn check_turn(prev: &TState, choices: &Choices, replacements: &ReplacementList, target: &TState) -> TurnResult {
    let (c1, c2) = match (&choices.p1, &choices.p2) {
        (Some(a), Some(b)) => (a, b),
        _ => return TurnResult::Skipped,
    };
    let (m1, m2) = match (parse_choice(c1), parse_choice(c2)) {
        (Some(a), Some(b)) => (a, b),
        _ => return TurnResult::Skipped,
    };

    let mut unmapped = Unmapped::default();
    let prev_state = parse_state(prev, &mut unmapped);
    let target_proj = project_state(&parse_state(target, &mut unmapped));

    for cand in candidate_states(&prev_state, m1, m2, replacements) {
        if relaxed_eq(&project_state(&cand), &target_proj) {
            return TurnResult::Match;
        }
    }
    TurnResult::Mismatch
}

/// Human-readable diagnostic for a mismatching turn: what PS produced vs the set of
/// outcomes the engine enumerated (active-Pokémon HP/status on both sides).
pub fn diagnose(prev: &TState, choices: &Choices, replacements: &ReplacementList, target: &TState) -> String {
    let mut unmapped = Unmapped::default();
    let prev_state = parse_state(prev, &mut unmapped);
    let (c1, c2) = match (&choices.p1, &choices.p2) {
        (Some(a), Some(b)) => (a.clone(), b.clone()),
        _ => return "  (not replayable)".into(),
    };
    let (m1, m2) = match (parse_choice(&c1), parse_choice(&c2)) {
        (Some(a), Some(b)) => (a, b),
        _ => return "  (unparseable choices)".into(),
    };

    let target_proj = project_state(&parse_state(target, &mut unmapped));
    let mut out = String::new();
    for (i, (side, mc)) in [(SideId::One, m1), (SideId::Two, m2)].into_iter().enumerate() {
        let s = prev_state.side(side);
        let p = s.active();
        let act = match mc {
            MoveChoice::Move(idx) => format!("uses {}", p.moves[idx as usize].id.to_id()),
            MoveChoice::Switch(t) => format!("switches to #{t}"),
        };
        out += &format!("  p{}: {} (item {:?}, ability {:?}) {act}\n", i + 1, p.species.to_id(), p.item, p.ability);
    }
    out += &format!("  PS    : {}\n", summarize(&target_proj));

    // Find the engine candidate closest to PS (min summed |hp diff| over actives), and
    // print the first relaxed field difference against it.
    let mut best: Option<(i32, TState)> = None;
    for cand in candidate_states(&prev_state, m1, m2, replacements) {
        let proj = project_state(&cand);
        let dist = active_hp_distance(&proj, &target_proj);
        if best.as_ref().map_or(true, |(d, _)| dist < *d) {
            best = Some((dist, proj));
        }
    }
    if let Some((_, proj)) = best {
        out += &format!("  engine: {}\n", summarize(&proj));
        out += &format!("  first relaxed diff: {}", relaxed_diff(&proj, &target_proj).unwrap_or_else(|| "<none — diff is outside relaxed set>".into()));
    }
    out
}

fn summarize(s: &TState) -> String {
    let mut parts = Vec::new();
    if s.weather != "none" {
        parts.push(format!("weather={}", s.weather));
    }
    for (i, side) in s.sides.iter().enumerate() {
        let hps: Vec<String> = side.pokemon.iter().map(|p| {
            if p.status == "none" { format!("{}", p.hp) } else { format!("{}({})", p.hp, p.status) }
        }).collect();
        let b = &side.boosts;
        let boostpart = if (b.atk, b.def, b.spa, b.spd, b.spe) != (0, 0, 0, 0, 0) {
            format!(" boosts[{},{},{},{},{}]", b.atk, b.def, b.spa, b.spd, b.spe)
        } else { String::new() };
        parts.push(format!("s{i}#{} [{}]{}", side.active_index, hps.join(","), boostpart));
    }
    parts.join("  ")
}

fn active_hp_distance(a: &TState, b: &TState) -> i32 {
    let mut d = 0;
    for (sa, sb) in a.sides.iter().zip(&b.sides) {
        for (pa, pb) in sa.pokemon.iter().zip(&sb.pokemon) {
            d += (pa.hp as i32 - pb.hp as i32).abs();
        }
    }
    d
}

/// Whether the side's active Pokémon is alive (boosts are meaningless once it faints).
fn active_alive(s: &TSide) -> bool {
    s.pokemon.get(s.active_index as usize).map_or(false, |p| p.hp > 0)
}

/// First relaxed field difference (engine vs PS), or None if relaxed-equal.
fn relaxed_diff(a: &TState, b: &TState) -> Option<String> {
    if a.weather != b.weather { return Some(format!("weather {} != {}", a.weather, b.weather)); }
    for (i, (sa, sb)) in a.sides.iter().zip(&b.sides).enumerate() {
        if sa.active_index != sb.active_index {
            return Some(format!("s{i} active_index {} != {}", sa.active_index, sb.active_index));
        }
        if active_alive(sa) && active_alive(sb)
            && (sa.boosts.atk, sa.boosts.def, sa.boosts.spa, sa.boosts.spd, sa.boosts.spe)
                != (sb.boosts.atk, sb.boosts.def, sb.boosts.spa, sb.boosts.spd, sb.boosts.spe) {
            return Some(format!("s{i} boosts differ"));
        }
        let (ca, cb) = (&sa.side_conditions, &sb.side_conditions);
        if ca.stealth_rock != cb.stealth_rock || ca.spikes != cb.spikes || ca.toxic_spikes != cb.toxic_spikes {
            return Some(format!("s{i} hazards differ (engine SR={} spikes={} / PS SR={} spikes={})", ca.stealth_rock, ca.spikes, cb.stealth_rock, cb.spikes));
        }
        for (j, (pa, pb)) in sa.pokemon.iter().zip(&sb.pokemon).enumerate() {
            if pa.hp != pb.hp { return Some(format!("s{i}#{j} hp {} != {}", pa.hp, pb.hp)); }
            // A fainted Pokémon's status is cleared by PS; skip it.
            if pa.hp > 0 && pa.status != pb.status { return Some(format!("s{i}#{j} status {} != {}", pa.status, pb.status)); }
        }
    }
    None
}

/// Compare two projected states on the mechanics the engine currently models.
fn relaxed_eq(a: &TState, b: &TState) -> bool {
    if a.weather != b.weather || a.terrain != b.terrain || a.trick_room != b.trick_room {
        return false;
    }
    if a.sides.len() != b.sides.len() {
        return false;
    }
    for (sa, sb) in a.sides.iter().zip(&b.sides) {
        if sa.active_index != sb.active_index {
            return false;
        }
        let ba = &sa.boosts;
        let bb = &sb.boosts;
        if active_alive(sa) && active_alive(sb)
            && (ba.atk, ba.def, ba.spa, ba.spd, ba.spe) != (bb.atk, bb.def, bb.spa, bb.spd, bb.spe) {
            return false;
        }
        let ca = &sa.side_conditions;
        let cb = &sb.side_conditions;
        if ca.stealth_rock != cb.stealth_rock || ca.spikes != cb.spikes || ca.toxic_spikes != cb.toxic_spikes {
            return false;
        }
        if sa.pokemon.len() != sb.pokemon.len() {
            return false;
        }
        for (pa, pb) in sa.pokemon.iter().zip(&sb.pokemon) {
            if pa.hp != pb.hp {
                return false;
            }
            // A fainted Pokémon's status is cleared by PS; only compare while alive.
            if pa.hp > 0 && pa.status != pb.status {
                return false;
            }
        }
    }
    true
}
