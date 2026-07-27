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

use engine::generate::{generate_instructions_ex, generate_move_action, switch_into, switch_into_pair, MoveChoice};
use engine::state::State;
use serde_json::Value;

use crate::convert::{convert_state, species_id_of_details, to_id, Canonical, Unsupported};
use crate::diff::{diff_states, Diff};
use crate::trace::{Decision, Trace};

pub enum Verdict {
    Matched,
    Diverged { closest: Vec<Diff>, branches: usize },
    DistributionDiverged { detail: String, branches: usize, outcomes: usize },
    Unsupported(Unsupported),
}

pub struct UnitResult {
    pub turn: u32,
    pub verdict: Verdict,
    /// "p1:<choice> p2:<choice>" summary for diagnostics.
    pub choice_summary: String,
    /// Compact labeled-PRNG-draw log for diagnostics.
    pub draws_summary: String,
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

    let ruleset = crate::trace::ruleset_for(trace.ruleset.as_deref(), &trace.format)
        .map_err(Unsupported)?;
    let mut results = Vec::new();
    let mut i = 0;
    // The state each unit replays from: the previous decision boundary.
    let mut boundary: &Value = &first.state_after;
    if first.request_state != crate::trace::first_decision_state(&ruleset) {
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
        results.push(replay_unit(boundary, &unit, target, &canon, ruleset));
        boundary = target;
        i = j;
    }
    Ok(results)
}

fn replay_unit(before: &Value, unit: &[&Decision], target: &Value, canon: &Canonical, ruleset: engine::ruleset::Ruleset) -> UnitResult {
    let dp = unit[0];
    let mut moves_used = Vec::new();
    for c in dp.choices.values() {
        if let Some(m) = &c.resolved.move_id {
            moves_used.push(m.clone());
        }
    }
    let mut choice_summary = String::new();
    for d in unit {
        for sk in ["p1", "p2"] {
            if let Some(c) = d.choices.get(sk) {
                let what = c.resolved.move_id.clone()
                    .or_else(|| c.resolved.details.clone())
                    .unwrap_or_else(|| c.choice.clone());
                choice_summary.push_str(&format!("{sk}:{what} "));
            }
        }
    }
    let mut draws_summary = String::new();
    for d in unit {
        for v in &d.draws {
            let kind = v.get("kind").and_then(serde_json::Value::as_str).unwrap_or("");
            let eff = v.get("effect").and_then(serde_json::Value::as_str).unwrap_or("");
            let ev = v.get("event").and_then(serde_json::Value::as_str).unwrap_or("");
            let res = v.get("result").map(|r| r.to_string()).unwrap_or_default();
            let args = v.get("args").map(|r| r.to_string()).unwrap_or_default();
            draws_summary.push_str(&format!("{kind}{args}={res}[{eff}/{ev}] "));
        }
    }
    let turn = dp.turn;
    let mk = |verdict: Verdict, legality: Vec<String>| UnitResult { turn, verdict, moves_used: moves_used.clone(), legality, choice_summary: choice_summary.clone(), draws_summary: draws_summary.clone() };

    // Convert endpoint states; conversion failures are coverage findings.
    let state_before = match convert_state(before, canon) {
        Ok(mut s) => { s.ruleset = ruleset; s }
        Err(u) => return mk(Verdict::Unsupported(u), vec![]),
    };
    let state_target = match convert_state(target, canon) {
        Ok(mut s) => { s.ruleset = ruleset; s }
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
            let slot = if let Some(ri) = choice.resolved.roster_index {
                ri
            } else {
                let Some(details) = &choice.resolved.details else {
                    return mk(Verdict::Unsupported(Unsupported("switch:no-details".into())), legality);
                };
                match canon.slot(si, &species_id_of_details(details)) {
                    Ok(s) => s,
                    Err(u) => return mk(Verdict::Unsupported(u), legality),
                }
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

    // Strong mode: compare the complete probability distribution, not sampled-path
    // membership. Distribution snapshots currently describe one PS request resolution, so
    // only compare units without separately recorded trailing replacement requests.
    if let Some(dist) = &dp.distribution {
        for kernel in dist.kernels.iter().filter(|k| k.action.choice == "move") {
            let Some(side_key) = kernel.action.side.as_deref() else { continue };
            let Some(move_id) = kernel.action.move_id.as_deref() else { continue };
            let side_idx = if side_key == "p1" { 0 } else if side_key == "p2" { 1 } else { continue };
            let side = crate::convert::side_id(side_idx);
            let mut input = match convert_state(&kernel.input, canon) {
                Ok(s) => s,
                Err(u) => return mk(Verdict::Unsupported(u), legality),
            };
            input.ruleset = ruleset;
            if input.side(side).active_index == u8::MAX {
                let no_op = vec![engine::instruction::StateInstructions { percentage: 100.0, instructions: Vec::new() }];
                match compare_distribution(&input, &no_op, &kernel.outcomes, canon, ruleset) {
                    Ok(None) => continue,
                    Ok(Some(detail)) => return mk(Verdict::DistributionDiverged {
                        detail: format!("action {side_key}:{move_id} (no active): {detail}"),
                        branches: 1,
                        outcomes: kernel.outcomes.len(),
                    }, legality),
                    Err(u) => return mk(Verdict::Unsupported(u), legality),
                }
            }
            let Some(move_idx) = input.side(side).active().moves.iter().position(|m| m.id.to_id() == move_id) else {
                return mk(Verdict::Unsupported(Unsupported(format!("distribution:kernel-move-not-found:{move_id}"))), legality);
            };
            let foe_pending = kernel.action.foe_pending_move_id.as_deref().and_then(|id| {
                input.side(side.other()).active().moves.iter().find(|m| m.id.to_id() == id).map(|m| m.id)
            });
            let action_branches = generate_move_action(&input, side, move_idx as u8, None, foe_pending);
            match compare_distribution(&input, &action_branches, &kernel.outcomes, canon, ruleset) {
                Ok(None) => {}
                Ok(Some(detail)) => return mk(Verdict::DistributionDiverged {
                    detail: format!("action {side_key}:{move_id}: {detail}"),
                    branches: action_branches.len(),
                    outcomes: kernel.outcomes.len(),
                }, legality),
                Err(u) => return mk(Verdict::Unsupported(u), legality),
            }
        }
        if unit.len() != 1 {
            // A first-action pivot pauses before its replacement is selected. We can certify
            // that policy-neutral endpoint when the oracle contains exactly one move kernel
            // and its output distribution is exactly the terminal switch-request
            // distribution. Requiring a single kernel deliberately excludes slower pivots
            // (whose reach probability depends on an earlier action), and requiring every
            // outcome to stop at the request excludes miss/non-pivot continuation mixtures.
            let move_kernels = dist.kernels.iter()
                .filter(|k| k.action.choice == "move")
                .collect::<Vec<_>>();
            let immediate_pivot = pivots.iter().any(Option::is_some)
                && move_kernels.len() == 1
                && !dist.outcomes.is_empty()
                && dist.outcomes.iter().all(|o| o.request_state == "switch");
            if immediate_pivot {
                match compare_ps_distributions(
                    &move_kernels[0].outcomes,
                    &dist.outcomes,
                    canon,
                    ruleset,
                ) {
                    Ok(None) => return mk(Verdict::Matched, legality),
                    Ok(Some(detail)) => return mk(Verdict::DistributionDiverged {
                        detail: format!("immediate pivot endpoint: {detail}"),
                        branches: move_kernels[0].outcomes.len(),
                        outcomes: dist.outcomes.len(),
                    }, legality),
                    Err(u) => return mk(Verdict::Unsupported(u), legality),
                }
            }
            return mk(Verdict::Unsupported(Unsupported("distribution:trailing-request-boundary".into())), legality);
        }
        // With a decision cap the trace may end at the switch request, before recording the
        // eventual switch choice as a trailing decision. The request is still certifiable when
        // it is exactly the output of the sole move kernel. This criterion covers both pivots
        // and faints and does not need to infer which kind of switch PS requested.
        let move_kernels = dist.kernels.iter()
            .filter(|k| k.action.choice == "move")
            .collect::<Vec<_>>();
        if move_kernels.len() == 1
            && !dist.outcomes.is_empty()
            && dist.outcomes.iter().all(|o| o.request_state == "switch")
        {
            match compare_ps_distributions(
                &move_kernels[0].outcomes,
                &dist.outcomes,
                canon,
                ruleset,
            ) {
                Ok(None) => return mk(Verdict::Matched, legality),
                Ok(Some(detail)) => return mk(Verdict::DistributionDiverged {
                    detail: format!("immediate request endpoint: {detail}"),
                    branches: move_kernels[0].outcomes.len(),
                    outcomes: dist.outcomes.len(),
                }, legality),
                Err(u) => return mk(Verdict::Unsupported(u), legality),
            }
        }
        // A forced-switch request is a valid, policy-neutral endpoint: no replacement has
        // been chosen yet. `convert_state` represents PS's empty active slot with a sentinel,
        // and `diff_states` consequently compares the fainted roster member plus all shared
        // side/field state while omitting active-only fields. Do not continue through the
        // request (that would require assigning a switch policy); compare its exact support
        // and probability mass at the boundary instead.
        let mut boundary_kernel_states = Vec::new();
        for kernel in dist.kernels.iter().filter(|k| k.action.choice == "move") {
            for outcome in &kernel.outcomes {
                let mut state = match convert_state(&outcome.state, canon) {
                    Ok(s) => s,
                    Err(u) => return mk(Verdict::Unsupported(u), legality),
                };
                state.ruleset = ruleset;
                if state.sides.iter().any(|side| side.active_index == u8::MAX) {
                    boundary_kernel_states.push(state);
                }
            }
        }
        let mut ps_clusters: Vec<(State, f64, bool)> = Vec::new();
        for outcome in &dist.outcomes {
            let mut state = match convert_state(&outcome.state, canon) {
                Ok(s) => s,
                Err(u) => return mk(Verdict::Unsupported(u), legality),
            };
            state.ruleset = ruleset;
            if outcome.request_state == "switch"
                && state.sides.iter().all(|side| side.active_index != u8::MAX)
            {
                // An alive active at a switch request is a pivot-style decision. Its endpoint
                // depends on a replacement choice, unlike a forced replacement after faint.
                return mk(Verdict::Unsupported(Unsupported(
                    "distribution:pivot-request-boundary".into(),
                )), legality);
            }
            let boundary = outcome.request_state == "switch";
            if boundary && !boundary_kernel_states.iter().any(|s| diff_states(s, &state).is_empty()) {
                return mk(Verdict::Unsupported(Unsupported(
                    "distribution:uncovered-forced-switch-boundary".into(),
                )), legality);
            }
            if let Some((_, mass, _)) = ps_clusters.iter_mut()
                .find(|(s, _, b)| *b == boundary && diff_states(s, &state).is_empty())
            {
                *mass += outcome.probability;
            } else {
                ps_clusters.push((state, outcome.probability, boundary));
            }
        }
        let mut rust_mass = vec![0.0f64; ps_clusters.len()];
        let mut unmatched_rust = 0.0f64;
        for branch in &branches {
            let mut cand = state_before;
            cand.apply_instructions(&branch.instructions);
            let p = branch.percentage as f64 / 100.0;
            if let Some(i) = ps_clusters.iter().position(|(s, _, _)| diff_states(&cand, s).is_empty()) {
                rust_mass[i] += p;
            } else {
                unmatched_rust += p;
            }
        }
        const TOL: f64 = 2e-5;
        let mut errors = Vec::new();
        let ps_boundary_mass: f64 = ps_clusters.iter()
            .filter(|(_, _, boundary)| *boundary)
            .map(|(_, mass, _)| mass)
            .sum();
        let rust_boundary_mass = unmatched_rust + ps_clusters.iter().zip(&rust_mass)
            .filter(|((_, _, boundary), _)| *boundary)
            .map(|(_, mass)| *mass)
            .sum::<f64>();
        if (ps_boundary_mass - rust_boundary_mass).abs() > TOL {
            errors.push(format!("request-boundary mass: rust={rust_boundary_mass:.8} ps={ps_boundary_mass:.8} delta={:.8}", rust_boundary_mass - ps_boundary_mass));
        }
        for (i, ((_, ps, boundary), rs)) in ps_clusters.iter().zip(&rust_mass).enumerate() {
            if *boundary { continue; }
            if (ps - rs).abs() > TOL {
                errors.push(format!("outcome {i}: rust={rs:.8} ps={ps:.8} delta={:.8}", rs - ps));
            }
        }
        if errors.is_empty() {
            return mk(Verdict::Matched, legality);
        }
        return mk(Verdict::DistributionDiverged {
            detail: errors.into_iter().take(6).collect::<Vec<_>>().join(" | "),
            branches: branches.len(),
            outcomes: ps_clusters.len(),
        }, legality);
    }
    let mut closest: Option<Vec<Diff>> = None;
    let n = branches.len();
    for si in &branches {
        let mut cand = state_before;
        cand.apply_instructions(&si.instructions);
        let mut replaced = [false; 2];
        // `switch_into_pair` is for a SIMULTANEOUS double replacement — one fresh mon per side,
        // both fainted. Two replacements on the SAME side instead mean a cascade (the first
        // replacement fainted to entry hazards, the next came in); those are sequential and
        // must go through `switch_into` one at a time, marking only that side as replaced.
        if replacements.len() == 2 && replacements[0].0 != replacements[1].0 {
            switch_into_pair(&mut cand, [
                (crate::convert::side_id(replacements[0].0), replacements[0].1),
                (crate::convert::side_id(replacements[1].0), replacements[1].1),
            ]);
            replaced = [true, true];
        } else {
            for &(side, slot) in &replacements {
                switch_into(&mut cand, crate::convert::side_id(side), slot);
                replaced[side] = true;
            }
        }
        // activeTurns timing across a faint-replacement boundary. PS's endTurn does its per-mon
        // `activeTurns++` as the LAST step — and turn++ as its FIRST step. So the unit's target
        // snapshot sits relative to that increment exactly as its turn number reveals:
        //   - target turn > move turn: endTurn already ran (post-increment). Every mon is
        //     counted; the fresh replacement enters at 0 here but reads 1 in PS, so +1 it.
        //   - target turn == move turn: a mid-turn faint replacement captured BEFORE endTurn
        //     (pre-increment). The engine already counted this turn at end-of-turn, so the
        //     staying side is one ahead of the snapshot. (Mid-turn replacements snapshot at the
        //     request, before the fresh mon enters, so the replaced side isn't compared.)
        let pre_end_turn = !replacements.is_empty()
            && unit.last().is_some_and(|d| d.turn == dp.turn);
        for (side, was_replaced) in replaced.iter().enumerate() {
            if *was_replaced && !pre_end_turn {
                cand.sides[side].active_turns = cand.sides[side].active_turns.saturating_add(1);
            } else if !*was_replaced && pre_end_turn && cand.sides[side].active().is_alive() {
                cand.sides[side].active_turns = cand.sides[side].active_turns.saturating_sub(1);
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

/// Compare two Showdown-projected distributions. Used to prove that an action kernel itself
/// is the terminal request endpoint, without selecting or simulating the requested switch.
fn compare_ps_distributions(
    left: &[crate::trace::DistributionOutcome],
    right: &[crate::trace::DistributionOutcome],
    canon: &Canonical,
    ruleset: engine::ruleset::Ruleset,
) -> Result<Option<String>, Unsupported> {
    fn clusters(
        outcomes: &[crate::trace::DistributionOutcome],
        canon: &Canonical,
        ruleset: engine::ruleset::Ruleset,
    ) -> Result<Vec<(State, f64)>, Unsupported> {
        let mut out: Vec<(State, f64)> = Vec::new();
        for outcome in outcomes {
            let mut state = convert_state(&outcome.state, canon)?;
            state.ruleset = ruleset;
            if let Some((_, mass)) = out.iter_mut().find(|(s, _)| diff_states(s, &state).is_empty()) {
                *mass += outcome.probability;
            } else {
                out.push((state, outcome.probability));
            }
        }
        Ok(out)
    }

    let left = clusters(left, canon, ruleset)?;
    let right = clusters(right, canon, ruleset)?;
    const TOL: f64 = 2e-5;
    let mut errors = Vec::new();
    for (i, (state, mass)) in left.iter().enumerate() {
        let other = right.iter()
            .find(|(candidate, _)| diff_states(state, candidate).is_empty())
            .map_or(0.0, |(_, p)| *p);
        if (mass - other).abs() > TOL {
            errors.push(format!("kernel outcome {i}: kernel={mass:.8} terminal={other:.8}"));
        }
    }
    for (i, (state, mass)) in right.iter().enumerate() {
        if !left.iter().any(|(candidate, _)| diff_states(state, candidate).is_empty()) && *mass > TOL {
            errors.push(format!("terminal-only outcome {i}: mass={mass:.8}"));
        }
    }
    Ok((!errors.is_empty()).then(|| errors.into_iter().take(6).collect::<Vec<_>>().join(" | ")))
}

/// Compare a Rust conditional branch set with a PS action kernel after canonical projection.
/// Returns `Ok(None)` on exact support+mass equality, or a compact mismatch description.
fn compare_distribution(
    input: &State,
    branches: &[engine::instruction::StateInstructions],
    outcomes: &[crate::trace::DistributionOutcome],
    canon: &Canonical,
    ruleset: engine::ruleset::Ruleset,
) -> Result<Option<String>, Unsupported> {
    let mut ps_clusters: Vec<(State, f64)> = Vec::new();
    for outcome in outcomes {
        let mut state = convert_state(&outcome.state, canon)?;
        state.ruleset = ruleset;
        if let Some((_, mass)) = ps_clusters.iter_mut().find(|(s, _)| diff_states(s, &state).is_empty()) {
            *mass += outcome.probability;
        } else {
            ps_clusters.push((state, outcome.probability));
        }
    }
    let mut rust_mass = vec![0.0f64; ps_clusters.len()];
    let mut unmatched_rust = 0.0f64;
    let mut closest_unmatched: Option<Vec<Diff>> = None;
    for branch in branches {
        let mut cand = *input;
        cand.apply_instructions(&branch.instructions);
        let p = branch.percentage as f64 / 100.0;
        if let Some(i) = ps_clusters.iter().position(|(s, _)| diff_states(&cand, s).is_empty()) {
            rust_mass[i] += p;
        } else {
            unmatched_rust += p;
            for (state, _) in &ps_clusters {
                let diffs = diff_states(&cand, state);
                if closest_unmatched.as_ref().is_none_or(|old| diffs.len() < old.len()) {
                    closest_unmatched = Some(diffs);
                }
            }
        }
    }
    const TOL: f64 = 2e-5;
    let mut errors = Vec::new();
    if unmatched_rust > TOL {
        errors.push(format!("Rust-only mass {unmatched_rust:.8}"));
        if let Some(diffs) = &closest_unmatched {
            errors.extend(diffs.iter().take(3).map(|d| d.detail.clone()));
        }
    }
    for (i, ((_, ps), rs)) in ps_clusters.iter().zip(&rust_mass).enumerate() {
        if (ps - rs).abs() > TOL {
            errors.push(format!("outcome {i}: rust={rs:.8} ps={ps:.8} delta={:.8}", rs - ps));
        }
    }
    Ok((!errors.is_empty()).then(|| errors.into_iter().take(6).collect::<Vec<_>>().join(" | ")))
}

/// Did `side_key`'s active faint (making its pending switch request a faint replacement
/// rather than a pivot)? Decided from the request JSON itself: PS marks the slot in
/// `forceSwitch` and the side's active condition shows `fnt` for faints.
pub(crate) fn active_fainted(pending_state: &Value, si: usize, sw: &Decision, side_key: &str) -> bool {
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

pub(crate) fn resolve_choice(
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
            if mid == "recharge" {
                // PS's request for a `mustrecharge` mon offers the single pseudo-move
                // "recharge" (sim/pokemon.ts `getMoveRequestData`), which is not on the set.
                // The engine signals it through `pending_move == Recharging`: whatever slot is
                // chosen, `before_move` cancels the attempt and clears the flag.
                return Ok((MoveChoice::Move(0), r.tera));
            }
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
            if let Some(ri) = r.roster_index {
                return Ok((MoveChoice::Switch(ri), false));
            }
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
            .filter(|m| {
                m.id != engine::ids::MoveId::None
                    && m.pp > 0
                    && !m.disabled
                    // Gigaton Hammer / Blood Moon (cantusetwice) are locked out the turn after use.
                    && !engine::generate::cantusetwice_locked(state, crate::convert::side_id(si), m.id)
            })
            .map(|m| m.id.to_id().to_string())
            .collect();
        // A locked mon (mid-rampage Outrage/Thrash, charging a two-turn move, recharging, or
        // Encored) can only pick that one move — PS's request offers just it.
        let locked = match side.pending_move {
            engine::state::PendingMove::Rampaging(m, _)
            | engine::state::PendingMove::Charging(m) => Some(m),
            _ => None,
        }
        .or(if side.encore.1 > 0 { Some(side.encore.0) } else { None });
        if let Some(m) = locked {
            if m != engine::ids::MoveId::None {
                eng_moves = vec![m.to_id().to_string()];
            }
        }
        // A recharging mon's request is PS's single pseudo-move "recharge".
        if side.pending_move == engine::state::PendingMove::Recharging {
            eng_moves = vec!["recharge".to_string()];
        }
        eng_moves.sort();
        if ps_moves != eng_moves && ps_moves != vec!["struggle".to_string()] {
            out.push(format!("moves[{side_key}]: ps={ps_moves:?} engine={eng_moves:?}"));
        }

        // Trapping legality is a CERTIFIED property. PS reports volatile/locked traps as
        // `trapped: true` and ability traps (hidden `tryTrap(true)`: Arena Trap / Shadow Tag /
        // Magnet Pull) as `maybeTrapped: true`; with types public and the foe's real ability
        // known, both mean "switch rejected". PS omits both flags when the side has no one to
        // switch to (`canSwitchIn == 0`), so the comparison is gated on a live bench.
        // `committed` covers the rampage/charge/recharge lock (PendingMove), which PS also
        // reports as trapped.
        //
        // UNDER RANDBATS THE TWO FLAGS COME APART and must be compared separately: `nextTurn`
        // additionally sweeps every ability the foe's SPECIES could have
        // (`sim/battle.ts:1741-1752`), so a mon facing a Dugtrio with Sand Veil gets
        // `maybeTrapped: true` while the switch is perfectly legal. Conflating them was only
        // sound because customgame skips that sweep entirely.
        let sid = crate::convert::side_id(si);
        let committed = !matches!(side.pending_move, engine::state::PendingMove::None);
        let has_bench = side.pokemon.iter().enumerate().any(|(i, p)| {
            i as u8 != side.active_index && p.species != engine::ids::Species::None && p.is_alive()
        });
        let engine_trapped = committed || engine::generate::is_trapped(state, sid);
        if state.ruleset.infer_foe_trapping_abilities {
            let ps_hard = matches!(act.get("trapped"), Some(Value::Bool(true)));
            let ps_maybe = matches!(act.get("maybeTrapped"), Some(Value::Bool(true)));
            let eng_maybe = committed || engine::generate::maybe_trapped(state, sid);
            if has_bench && ps_hard != engine_trapped {
                out.push(format!("trapped[{side_key}]: ps={ps_hard} engine={engine_trapped}"));
            }
            if has_bench && ps_maybe != eng_maybe {
                out.push(format!("maybeTrapped[{side_key}]: ps={ps_maybe} engine={eng_maybe}"));
            }
        } else {
            let ps_trapped = matches!(act.get("trapped"), Some(Value::Bool(true)))
                || matches!(act.get("maybeTrapped"), Some(Value::Bool(true)));
            if has_bench && ps_trapped != engine_trapped {
                out.push(format!("trapped[{side_key}]: ps={ps_trapped} engine={engine_trapped}"));
            }
        }

        // Tera availability.
        let ps_tera = act.get("canTerastallize").and_then(Value::as_str).is_some_and(|t| !t.is_empty())
            || matches!(act.get("canTerastallize"), Some(Value::Bool(true)));
        // A committed multi-turn action cannot newly Terastallize on its continuation turn.
        let eng_tera = !side.tera_used && !committed;
        if ps_tera != eng_tera {
            out.push(format!("tera[{side_key}]: ps={ps_tera} engine={eng_tera}"));
        }
    }
    out
}
