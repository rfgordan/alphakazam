//! Draw-consumption differ (Phase 1 of DRAW_EXACT_PLAN).
//!
//! For each turn unit it replays the engine with draw annotation enabled
//! ([`generate_instructions_annotated`]), selects the outcome branch that reproduces PS's
//! recorded `stateAfter`, and compares that branch's ordered PRNG draw-request log against PS's
//! recorded draw log for the unit. Each unit is classified:
//!
//!   * `DRAW-EXACT`  — the engine requested exactly the recorded draw sequence (same count,
//!                     order, kinds, and args) AND some branch reproduces `stateAfter`. The
//!                     realized roll *values* are not compared: they are validated by the state.
//!   * first mismatch, one of:
//!       - `rust-requested-draw-not-next-in-log`  (kind differs, or the engine drew an extra)
//!       - `rust-finished-with-unconsumed-draws`  (PS drew more than the engine did)
//!       - `args-mismatch`                         (same kind, different call args)
//!       - `state-mismatch-despite-draw-match`     (draws align but no branch hits `stateAfter`)
//!   * `unsupported` — the unit couldn't be converted/resolved (a coverage finding, not a
//!                     draw finding). Reported separately; never counted as draw-exact.
//!
//! Enable via `DRAW_DIFF=1 cosim <traces...>`.

use serde_json::Value;

use engine::generate::{generate_instructions_annotated, DrawEvent, MoveChoice};
use engine::state::{SideId, State};

use crate::convert::{convert_state, species_id_of_details, Canonical, Unsupported};
use crate::diff::diff_states;
use crate::replay::{active_fainted, resolve_choice};
use crate::trace::{Decision, Trace};

/// One draw-diff verdict for a unit.
pub struct DrawUnit {
    pub turn: u32,
    pub class: DrawClass,
}

pub enum DrawClass {
    Exact,
    Unsupported(String),
    /// First-mismatch category (a `&'static str` from the fixed set) + a site label built from
    /// the recorded draw at the mismatch point, + a short human detail.
    Mismatch { category: &'static str, label: String, detail: String },
}

/// A recorded PS draw reduced to what the differ compares: kind, args, and a site label.
struct RecDraw {
    kind: String,
    args: Vec<i64>,
    label: String,
}

/// The unit's recorded draw RESULTS in order, as the realized multi-hit executor reads them
/// (sample → chosen index; randomChance → 0/1; random → value; shuffle/other → placeholder). Indexed
/// by the engine branch's draws-so-far length, which equals the PS draw position when aligned.
fn rec_results_of(unit: &[&Decision]) -> Vec<i64> {
    let mut out = Vec::new();
    for d in unit {
        for v in &d.draws {
            let kind = v.get("kind").and_then(Value::as_str).unwrap_or("");
            let r = v.get("result");
            let val = match kind {
                "sample" => r.and_then(|x| x.get("index")).and_then(Value::as_i64).unwrap_or(0),
                "randomChance" => r.and_then(Value::as_bool).map(|b| b as i64).unwrap_or(0),
                "random" => r.and_then(Value::as_i64).unwrap_or(0),
                _ => -1, // shuffle / unknown — position placeholder, never read by the executor
            };
            out.push(val);
        }
    }
    out
}

fn rec_draws_of(unit: &[&Decision]) -> Vec<RecDraw> {
    let mut out = Vec::new();
    for d in unit {
        for v in &d.draws {
            let kind = v.get("kind").and_then(Value::as_str).unwrap_or("").to_string();
            let args = v.get("args").and_then(Value::as_array).map(|a| {
                a.iter().filter_map(Value::as_i64).collect::<Vec<_>>()
            }).unwrap_or_default();
            let effect = v.get("effect").and_then(Value::as_str).unwrap_or("");
            let event = v.get("event").and_then(Value::as_str).unwrap_or("");
            let mv = v.get("move").and_then(Value::as_str).unwrap_or("");
            // Prefer the most specific semantic context; fall back to the move id.
            let ctx = if !effect.is_empty() { effect }
                else if !event.is_empty() { event }
                else if !mv.is_empty() { mv }
                else { "generic" };
            let label = format!("{kind}{args:?}@{ctx}");
            out.push(RecDraw { kind, args, label });
        }
    }
    out
}

fn draw_label(d: &DrawEvent) -> String {
    format!("{}{:?}@{}", d.kind, d.args, d.site)
}

/// Compare the engine's draw-request log against PS's recorded draws (kind + args + order +
/// count). Returns `None` when they match exactly, else the first-mismatch category and label.
fn compare_draws(rust: &[DrawEvent], rec: &[RecDraw]) -> Option<(&'static str, String, String)> {
    let n = rust.len().max(rec.len());
    for i in 0..n {
        match (rust.get(i), rec.get(i)) {
            (Some(r), Some(p)) => {
                if r.kind != p.kind {
                    return Some((
                        "rust-requested-draw-not-next-in-log",
                        p.label.clone(),
                        format!("pos {i}: rust {} vs ps {}", draw_label(r), p.label),
                    ));
                }
                let rargs: Vec<i64> = r.args.iter().map(|&x| x as i64).collect();
                if rargs != p.args {
                    return Some((
                        "args-mismatch",
                        p.label.clone(),
                        format!("pos {i}: rust {} args {:?} vs ps args {:?}", r.kind, rargs, p.args),
                    ));
                }
            }
            (Some(r), None) => {
                return Some((
                    "rust-requested-draw-not-next-in-log",
                    format!("{}{:?}@{}", r.kind, r.args, r.site),
                    format!("pos {i}: rust extra {}", draw_label(r)),
                ));
            }
            (None, Some(p)) => {
                return Some((
                    "rust-finished-with-unconsumed-draws",
                    p.label.clone(),
                    format!("pos {i}: ps unconsumed {}", p.label),
                ));
            }
            (None, None) => unreachable!(),
        }
    }
    None
}

pub fn draw_diff_trace(trace: &Trace) -> Result<Vec<DrawUnit>, Unsupported> {
    let first = trace.decisions.first().ok_or_else(|| Unsupported("trace:empty".into()))?;
    let canon = Canonical::from_first_state(&first.state_after)?;
    let ruleset = crate::trace::ruleset_for(trace.ruleset.as_deref(), &trace.format)
        .map_err(Unsupported)?;

    let mut results = Vec::new();
    if first.request_state != crate::trace::first_decision_state(&ruleset) {
        return Err(Unsupported(format!("trace:first-decision-{}", first.request_state)));
    }
    let mut boundary: &Value = &first.state_after;
    let mut i = 1;
    while i < trace.decisions.len() {
        let dp = &trace.decisions[i];
        if dp.request_state != "move" {
            return Err(Unsupported(format!("trace:unexpected-{}-at-{}", dp.request_state, dp.index)));
        }
        let mut unit = vec![dp];
        let mut j = i + 1;
        while j < trace.decisions.len() && trace.decisions[j].request_state == "switch" {
            unit.push(&trace.decisions[j]);
            j += 1;
        }
        let target = &unit.last().unwrap().state_after;
        results.push(diff_unit(boundary, &unit, target, &canon, ruleset));
        boundary = target;
        i = j;
    }
    Ok(results)
}

fn diff_unit(before: &Value, unit: &[&Decision], target: &Value, canon: &Canonical, ruleset: engine::ruleset::Ruleset) -> DrawUnit {
    let dp = unit[0];
    let turn = dp.turn;
    let mk_unsupported = |s: String| DrawUnit { turn, class: DrawClass::Unsupported(s) };

    let state_before = match convert_state(before, canon) {
        Ok(mut s) => { s.ruleset = ruleset; s }
        Err(u) => return mk_unsupported(u.0),
    };
    let state_target = match convert_state(target, canon) {
        Ok(mut s) => { s.ruleset = ruleset; s }
        Err(u) => return mk_unsupported(u.0),
    };

    // Resolve both sides' move choices.
    let mut mc = [MoveChoice::Move(0); 2];
    let mut tera = [false; 2];
    for (si, side_key) in ["p1", "p2"].iter().enumerate() {
        let Some(choice) = dp.choices.get(*side_key) else {
            return mk_unsupported(format!("unit:no-choice-{side_key}"));
        };
        match resolve_choice(&state_before, si, choice, canon) {
            Ok((m, t)) => { mc[si] = m; tera[si] = t; }
            Err(u) => return mk_unsupported(u.0),
        }
    }

    // Trailing switch decisions: pivots vs faint replacements (mirrors replay_unit).
    let mut pivots: [Option<u8>; 2] = [None, None];
    let mut replacements: Vec<(usize, u8)> = Vec::new();
    for (k, sw) in unit.iter().enumerate().skip(1) {
        let pending_state = if k == 1 { before } else { &unit[k - 1].state_after };
        for (side_key, choice) in &sw.choices {
            let si = if side_key == "p1" { 0 } else { 1 };
            let slot = if let Some(ri) = choice.resolved.roster_index {
                ri
            } else {
                let Some(details) = &choice.resolved.details else {
                    return mk_unsupported("switch:no-details".into());
                };
                match canon.slot(si, &species_id_of_details(details)) {
                    Ok(s) => s,
                    Err(u) => return mk_unsupported(u.0),
                }
            };
            if active_fainted(pending_state, si, sw, side_key) {
                replacements.push((si, slot));
            } else {
                pivots[si] = Some(slot);
            }
        }
    }

    // Install the realized multi-hit source (recorded results) so a variable multi-hit move
    // realizes the single branch PS's stream dictates instead of the state-only DP fold (which
    // emits no per-hit draws). Indexed by draws-so-far; the executor only reads it at the count +
    // per-hit positions, which align with PS's recorded stream on units exact up to that point.
    engine::generate::set_realized_source(Some(engine::generate::RealizedSource::Recorded(
        std::rc::Rc::new(rec_results_of(unit)),
    )));
    let outcomes = generate_instructions_annotated(&state_before, mc[0], mc[1], pivots, tera);
    engine::generate::set_realized_source(None);
    if outcomes.is_empty() {
        return mk_unsupported("engine:no-branches".into());
    }

    let rec = rec_draws_of(unit);
    let pre_end_turn = !replacements.is_empty() && unit.last().is_some_and(|d| d.turn == dp.turn);

    if let Ok(want) = std::env::var("DUMP_TURN") {
        if want.parse::<u32>() == Ok(turn) {
            eprintln!("=== DUMP turn {turn} mc={:?} outcomes={} ===", mc, outcomes.len());
            eprintln!("  ps  ={:?}", rec.iter().map(|r| r.label.clone()).collect::<Vec<_>>());
            for (oi, o) in outcomes.iter().enumerate() {
                let mut cand = state_before;
                cand.apply_instructions(&o.instructions);
                for &(side, slot) in &replacements {
                    engine::generate::switch_into(&mut cand, crate::convert::side_id(side), slot);
                }
                let sd = diff_states(&cand, &state_target);
                let sm = sd.is_empty();
                if oi < 4 && !sm { eprintln!("      state_diff[{oi}]={:?}", sd.iter().map(|x| x.detail.clone()).collect::<Vec<_>>()); }
                eprintln!("  [{oi}] p={:.3} state_match={sm} draws={:?}", o.percentage,
                    o.draws.iter().map(|d| format!("{}{:?}@{}", d.kind, d.args, d.site)).collect::<Vec<_>>());
            }
        }
    }

    // Materialize each outcome's final state exactly as replay does; find one matching `target`.
    // Draw *shapes* are invariant across the roll values that lead to a given state, so the
    // first state-matching outcome's draw log is representative.
    //
    // On a Speed tie both move orderings enumerate to the SAME `stateAfter` but with DIFFERENT
    // (interleaved) draw streams; PS realized exactly one order (its commitChoices shuffle bit).
    // The engine's enumeration produces both, so the honest question is whether ANY state-matching
    // branch reproduces PS's recorded draw sequence — not whichever ordering happens to enumerate
    // first. So collect every state-matching outcome: report Exact if one reproduces the draws,
    // else report the first such outcome's mismatch as the representative (the DBG dump too).
    let mut draw_matched_but_state_off = false;
    let mut first_state_match: Option<&engine::generate::AnnotatedOutcome> = None;
    for o in &outcomes {
        let mut cand = state_before;
        cand.apply_instructions(&o.instructions);
        let mut replaced = [false; 2];
        if replacements.len() == 2 && replacements[0].0 != replacements[1].0 {
            engine::generate::switch_into_pair(&mut cand, [
                (crate::convert::side_id(replacements[0].0), replacements[0].1),
                (crate::convert::side_id(replacements[1].0), replacements[1].1),
            ]);
            replaced = [true, true];
        } else {
            for &(side, slot) in &replacements {
                engine::generate::switch_into(&mut cand, crate::convert::side_id(side), slot);
                replaced[side] = true;
            }
        }
        for (side, was_replaced) in replaced.iter().enumerate() {
            if *was_replaced && !pre_end_turn {
                cand.sides[side].active_turns = cand.sides[side].active_turns.saturating_add(1);
            } else if !*was_replaced && pre_end_turn && cand.sides[side].active().is_alive() {
                cand.sides[side].active_turns = cand.sides[side].active_turns.saturating_sub(1);
            }
        }
        if diff_states(&cand, &state_target).is_empty() {
            // A branch reproducing BOTH stateAfter AND the exact recorded draw sequence means the
            // engine reproduced PS — return Exact regardless of which ordering enumerated first.
            if compare_draws(&o.draws, &rec).is_none() {
                return DrawUnit { turn, class: DrawClass::Exact };
            }
            if first_state_match.is_none() {
                first_state_match = Some(o);
            }
        } else if compare_draws(&o.draws, &rec).is_none() {
            draw_matched_but_state_off = true;
        }
    }
    // No state-matching branch reproduced the draws: report the first state-matching outcome's
    // mismatch as the representative (with the DBG dump, as before).
    if let Some(o) = first_state_match {
        let res = compare_draws(&o.draws, &rec);
        if let Ok(filter) = std::env::var("DRAW_DBG") {
            if let Some((cat, label, detail)) = &res {
                if (filter.is_empty() && label.contains("accuracy") && cat.contains("not-next"))
                    || (!filter.is_empty() && label.contains(&filter)) {
                    let p1 = state_before.sides[0].active();
                    let p2 = state_before.sides[1].active();
                    eprintln!("DBG turn {turn} mc={:?} p1alive={} p2alive={} | {detail}\n   rust={:?}\n   ps  ={:?}",
                        mc, p1.is_alive(), p2.is_alive(),
                        o.draws.iter().map(|d| format!("{}{:?}@{}", d.kind, d.args, d.site)).collect::<Vec<_>>(),
                        rec.iter().map(|r| r.label.clone()).collect::<Vec<_>>());
                }
            }
        }
        return match res {
            None => DrawUnit { turn, class: DrawClass::Exact },
            Some((category, label, detail)) => DrawUnit { turn, class: DrawClass::Mismatch { category, label, detail } },
        };
    }

    // No branch reproduced PS's state (should be rare: the corpus is 100% state-matched by the
    // main verifier). Report honestly.
    if draw_matched_but_state_off {
        DrawUnit {
            turn,
            class: DrawClass::Mismatch {
                category: "state-mismatch-despite-draw-match",
                label: rec.first().map(|r| r.label.clone()).unwrap_or_else(|| "none".into()),
                detail: "draws align but no branch reproduces stateAfter".into(),
            },
        }
    } else {
        // Fall back to the highest-probability branch's draw comparison as the representative
        // first mismatch (still honest: it's a real engine path).
        let rep = outcomes.iter().max_by(|a, b| a.percentage.total_cmp(&b.percentage)).unwrap();
        match compare_draws(&rep.draws, &rec) {
            None => DrawUnit {
                turn,
                class: DrawClass::Mismatch {
                    category: "state-mismatch-despite-draw-match",
                    label: rec.first().map(|r| r.label.clone()).unwrap_or_else(|| "none".into()),
                    detail: "no state-matching branch".into(),
                },
            },
            Some((category, label, detail)) => DrawUnit { turn, class: DrawClass::Mismatch { category, label, detail } },
        }
    }
}

/// Aggregate scoreboard over many traces.
#[derive(Default)]
pub struct DrawScoreboard {
    pub units: u64,
    pub exact: u64,
    pub unsupported: u64,
    /// category -> count
    pub categories: std::collections::BTreeMap<&'static str, u64>,
    /// label -> count (first-mismatch label frequency)
    pub labels: std::collections::BTreeMap<String, u64>,
    /// label -> one representative detail
    pub label_detail: std::collections::BTreeMap<String, String>,
}

impl DrawScoreboard {
    pub fn add(&mut self, u: &DrawUnit) {
        self.units += 1;
        match &u.class {
            DrawClass::Exact => self.exact += 1,
            DrawClass::Unsupported(_) => self.unsupported += 1,
            DrawClass::Mismatch { category, label, detail } => {
                *self.categories.entry(category).or_default() += 1;
                *self.labels.entry(label.clone()).or_default() += 1;
                self.label_detail.entry(label.clone()).or_insert_with(|| detail.clone());
            }
        }
    }

    pub fn report(&self) {
        let supported = self.units - self.unsupported;
        let pct = if supported > 0 { self.exact as f64 / supported as f64 * 100.0 } else { 0.0 };
        println!("\n========== DRAW-CONSUMPTION SCOREBOARD ==========");
        println!("units: {}  supported: {}  unsupported: {}", self.units, supported, self.unsupported);
        println!("DRAW-EXACT: {} / {} supported = {:.2}%", self.exact, supported, pct);
        println!("\nmismatch categories (ranked):");
        let mut cats: Vec<_> = self.categories.iter().collect();
        cats.sort_by(|a, b| b.1.cmp(a.1));
        for (cat, c) in cats {
            println!("  {c:6}  {cat}");
        }
        println!("\ntop-20 first-mismatch labels (the burn-down queue):");
        let mut labs: Vec<_> = self.labels.iter().collect();
        labs.sort_by(|a, b| b.1.cmp(a.1));
        for (label, c) in labs.into_iter().take(20) {
            let detail = self.label_detail.get(label).map(String::as_str).unwrap_or("");
            println!("  {c:6}  {label}");
            println!("            e.g. {detail}");
        }
    }
}
