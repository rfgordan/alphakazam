//! Replay recorded decision points through the engine and classify every turn unit:
//!
//!   Matched     — some engine branch is *exactly* equal to PS's next state on all modeled fields
//!   Diverged    — all branches differ; the closest branch's field diffs are reported
//!   Unsupported — the states/choices touch something outside the engine's modeled set
//!                 (the explicit coverage frontier, with a named reason)
//!
//! A "turn unit" is one `move` decision plus its trailing `switch` decisions (mid-turn pivots
//! and post-turn faint replacements), replayed as one engine transition. Comparison happens at
//! full decision boundaries only.

use std::collections::BTreeMap;

use engine::generate::{generate_instructions_ex, switch_into, MoveChoice};
use engine::state::State;
use serde_json::Value;

use crate::convert::{convert_state, species_id_of_details, to_id, Canonical, Unsupported};
use crate::diff::{diff_states, Diff};
use crate::trace::{Decision, Trace};

pub enum Verdict {
    Matched,
    Diverged { closest: Vec<Diff>, branches: usize },
    Unsupported(Unsupported),
}

pub struct UnitResult {
    pub turn: u32,
    pub verdict: Verdict,
    /// move ids chosen this unit (for coverage accounting)
    pub moves_used: Vec<String>,
    /// legality mismatches observed at this unit's requests
    pub legality: Vec<String>,
}

pub fn replay_trace(trace: &Trace) -> Result<Vec<UnitResult>, Unsupported> {
    let first = trace
        .decisions
        .first()
        .ok_or_else(|| Unsupported("trace:empty".into()))?;
    let canon = Canonical::from_first_state(&first.state_after)?;

    let mut results = Vec::new();
    let mut i = 0;
    // The state each unit replays from: the previous decision boundary.
    let mut boundary: &Value = &first.state_after;
    if first.request_state != "teampreview" {
        return Err(Unsupported(format!("trace:first-decision-{}", first.request_state)));
    }
    i += 1;

    while i < trace.decisions.len() {
        let dp = &trace.decisions[i];
        if dp.request_state != "move" {
            return Err(Unsupported(format!("trace:unexpected-{}-at-{}", dp.request_state, dp.index)));
        }
        // Collect this unit: the move decision plus trailing switch decisions.
        let mut unit = vec![dp];
        let mut j = i + 1;
        while j < trace.decisions.len() && trace.decisions[j].request_state == "switch" {
            unit.push(&trace.decisions[j]);
            j += 1;
        }
        let target = &unit.last().unwrap().state_after;
        results.push(replay_unit(boundary, &unit, target, &canon));
        boundary = target;
        i = j;
    }
    Ok(results)
}

fn replay_unit(before: &Value, unit: &[&Decision], target: &Value, canon: &Canonical) -> UnitResult {
    let dp = unit[0];
    let mut moves_used = Vec::new();
    for c in dp.choices.values() {
        if let Some(m) = &c.resolved.move_id {
            moves_used.push(m.clone());
        }
    }
    let turn = dp.turn;
    let mk = |verdict: Verdict, legality: Vec<String>| UnitResult { turn, verdict, moves_used: moves_used.clone(), legality };

    // Convert endpoint states; conversion failures are coverage findings.
    let state_before = match convert_state(before, canon) {
        Ok(s) => s,
        Err(u) => return mk(Verdict::Unsupported(u), vec![]),
    };
    let state_target = match convert_state(target, canon) {
        Ok(s) => s,
        Err(u) => return mk(Verdict::Unsupported(u), vec![]),
    };

    let legality = check_legality(&state_before, &dp.requests);

    // Choices for both sides.
    let mut mc = [MoveChoice::Move(0); 2];
    let mut tera = [false; 2];
    for (si, side_key) in ["p1", "p2"].iter().enumerate() {
        let Some(choice) = dp.choices.get(*side_key) else {
            return mk(Verdict::Unsupported(Unsupported(format!("unit:no-choice-{side_key}"))), legality);
        };
        match resolve_choice(&state_before, si, choice, canon) {
            Ok((m, t)) => {
                mc[si] = m;
                tera[si] = t;
            }
            Err(u) => return mk(Verdict::Unsupported(u), legality),
        }
    }

    // Trailing switch decisions: mid-turn pivots (active alive when asked) vs faint
    // replacements (active fainted), in recorded order.
    let mut pivots: [Option<u8>; 2] = [None, None];
    let mut replacements: Vec<(usize, u8)> = Vec::new();
    for (k, sw) in unit.iter().enumerate().skip(1) {
        // State when this switch request was pending = previous decision's stateAfter.
        let pending_state = if k == 1 { before } else { &unit[k - 1].state_after };
        for (side_key, choice) in &sw.choices {
            let si = if side_key == "p1" { 0 } else { 1 };
            let Some(details) = &choice.resolved.details else {
                return mk(Verdict::Unsupported(Unsupported("switch:no-details".into())), legality);
            };
            let slot = match canon.slot(si, &species_id_of_details(details)) {
                Ok(s) => s,
                Err(u) => return mk(Verdict::Unsupported(u), legality),
            };
            // NOTE: `pending_state` for k==1 is the pre-turn state; the request actually arose
            // mid-resolution. Distinguish pivot vs faint by the *requesting* side's active HP in
            // the state where the request appears: for k==1 we must look at the request itself.
            let fainted = active_fainted(pending_state, si, sw, side_key);
            if fainted {
                replacements.push((si, slot));
            } else {
                pivots[si] = Some(slot);
            }
        }
    }

    // Replay.
    let debug = std::env::var("DEBUG_TURN").ok().and_then(|v| v.parse::<u32>().ok()) == Some(turn);
    if debug {
        eprintln!("[debug t{turn}] mc={mc:?} tera={tera:?} pivots={pivots:?} repl(before-compute)");
        eprintln!("  before: s0.at={} s1.at={} s0.ai={} s1.ai={}",
            state_before.sides[0].active_turns, state_before.sides[1].active_turns,
            state_before.sides[0].active_index, state_before.sides[1].active_index);
        eprintln!("  target: s0.at={} s1.at={} s0.ai={} s1.ai={}",
            state_target.sides[0].active_turns, state_target.sides[1].active_turns,
            state_target.sides[0].active_index, state_target.sides[1].active_index);
    }
    let branches = generate_instructions_ex(&state_before, mc[0], mc[1], pivots, tera);
    if branches.is_empty() {
        return mk(Verdict::Unsupported(Unsupported("engine:no-branches".into())), legality);
    }
    let mut closest: Option<Vec<Diff>> = None;
    let n = branches.len();
    for si in &branches {
        let mut cand = state_before;
        cand.apply_instructions(&si.instructions);
        let mut replaced = [false; 2];
        for &(side, slot) in &replacements {
            switch_into(&mut cand, crate::convert::side_id(side), slot);
            replaced[side] = true;
        }
        // PS increments activeTurns in nextTurn — *after* faint replacements enter — while the
        // engine increments at end-of-turn, before the caller applies replacements. Same
        // convention except across a replacement boundary: the fresh mon is 1 in PS, 0 here.
        for (side, was_replaced) in replaced.iter().enumerate() {
            if *was_replaced {
                cand.sides[side].active_turns = cand.sides[side].active_turns.saturating_add(1);
            }
        }
        let diffs = diff_states(&cand, &state_target);
        if diffs.is_empty() {
            return mk(Verdict::Matched, legality);
        }
        if closest.as_ref().is_none_or(|c| diffs.len() < c.len()) {
            closest = Some(diffs);
        }
    }
    mk(
        Verdict::Diverged { closest: closest.unwrap_or_default(), branches: n },
        legality,
    )
}

/// Did `side_key`'s active faint (making its pending switch request a faint replacement
/// rather than a pivot)? Decided from the request JSON itself: PS marks the slot in
/// `forceSwitch` and the side's active condition shows `fnt` for faints.
fn active_fainted(pending_state: &Value, si: usize, sw: &Decision, side_key: &str) -> bool {
    // Prefer the request: side.pokemon[active].condition endswith " fnt".
    if let Some(req) = sw.requests.get(side_key) {
        if let Some(mons) = req["side"]["pokemon"].as_array() {
            for m in mons {
                if m.get("active").and_then(Value::as_bool).unwrap_or(false) {
                    let cond = m.get("condition").and_then(Value::as_str).unwrap_or("");
                    return cond == "0 fnt" || cond.ends_with(" fnt");
                }
            }
        }
    }
    // Fallback: the pending state's active hp.
    pending_state["sides"][si]["pokemon"]
        .as_array()
        .and_then(|mons| mons.iter().find(|p| p.get("isActive").and_then(Value::as_bool).unwrap_or(false)))
        .map(|p| p.get("hp").and_then(Value::as_i64).unwrap_or(1) <= 0)
        .unwrap_or(false)
}

fn resolve_choice(
    state: &State,
    si: usize,
    choice: &crate::trace::ChoiceRec,
    canon: &Canonical,
) -> Result<(MoveChoice, bool), Unsupported> {
    let r = &choice.resolved;
    match r.action.as_str() {
        "move" => {
            let side = &state.sides[si];
            let active = &side.pokemon[side.active_index as usize];
            let mid = r.move_id.as_deref().unwrap_or("");
            if mid == "struggle" {
                // engine signals Struggle via a 0-pp chosen slot
                let slot = active.moves.iter().position(|m| m.pp == 0).unwrap_or(0);
                return Ok((MoveChoice::Move(slot as u8), r.tera));
            }
            let id = to_id(mid);
            let slot = active
                .moves
                .iter()
                .position(|m| m.id.to_id() == id)
                .ok_or_else(|| Unsupported(format!("choice:move-not-on-set:{id}")))?;
            Ok((MoveChoice::Move(slot as u8), r.tera))
        }
        "switch" => {
            let details = r.details.as_deref().ok_or_else(|| Unsupported("choice:switch-no-details".into()))?;
            let slot = canon.slot(si, &species_id_of_details(details))?;
            Ok((MoveChoice::Switch(slot), false))
        }
        other => Err(Unsupported(format!("choice:{other}"))),
    }
}

/// Diff PS's request JSON (the legality ground truth) against the engine's view.
fn check_legality(state: &State, requests: &BTreeMap<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    for (side_key, req) in requests {
        let si = if side_key == "p1" { 0 } else { 1 };
        let side = &state.sides[si];
        let active = &side.pokemon[side.active_index as usize];
        let Some(acts) = req.get("active").and_then(Value::as_array) else { continue };
        let Some(act) = acts.first() else { continue };

        // Legal move set: PS's view vs engine's (pp>0 and not disabled).
        let mut ps_moves: Vec<String> = Vec::new();
        for m in act["moves"].as_array().into_iter().flatten() {
            let disabled = matches!(m.get("disabled"), Some(Value::Bool(true)));
            let pp_ok = m.get("pp").and_then(Value::as_i64).map_or(true, |pp| pp > 0);
            if !disabled && pp_ok {
                ps_moves.push(m.get("id").and_then(Value::as_str).unwrap_or("").to_string());
            }
        }
        ps_moves.sort();
        let mut eng_moves: Vec<String> = active
            .moves
            .iter()
            .filter(|m| m.id != engine::ids::MoveId::None && m.pp > 0 && !m.disabled)
            .map(|m| m.id.to_id().to_string())
            .collect();
        eng_moves.sort();
        if ps_moves != eng_moves && ps_moves != vec!["struggle".to_string()] {
            out.push(format!("moves[{side_key}]: ps={ps_moves:?} engine={eng_moves:?}"));
        }

        // Trapping: the engine doesn't model it; surface when PS traps a side.
        if matches!(act.get("trapped"), Some(Value::Bool(true))) {
            out.push(format!("trapped[{side_key}]"));
        }

        // Tera availability.
        let ps_tera = act.get("canTerastallize").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
            || matches!(act.get("canTerastallize"), Some(Value::Bool(true)));
        let eng_tera = !side.tera_used;
        if ps_tera != eng_tera {
            out.push(format!("tera[{side_key}]: ps={ps_tera} engine={eng_tera}"));
        }
    }
    out
}
