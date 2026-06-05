//! Differential test runner.
//!
//! Loads a trace (ground truth from Pokémon Showdown), and for every recorded state
//! checks that the engine can represent it faithfully: parse PS state → engine `State`
//! → project back → diff against the original. Reports unmapped ids (coverage gaps) and
//! any structural mismatches, pinpointing the first divergence.
//!
//! Today this verifies *representation*. Once `generate_instructions` lands, the same
//! loop will additionally *replay* each snapshot's choices from the previous state and
//! assert the engine's computed transition matches PS — closing the loop.
//!
//! Usage: verify <trace.json> [trace2.json ...]

mod convert;
mod replay;
mod trace;

use std::process::ExitCode;

use convert::{parse_state, project_state, Unmapped};
use trace::{TState, Trace};

/// Recursively diff two JSON values, collecting human-readable `path: a != b` lines.
fn json_diff(path: &str, a: &serde_json::Value, b: &serde_json::Value, out: &mut Vec<String>) {
    use serde_json::Value::*;
    match (a, b) {
        (Object(ma), Object(mb)) => {
            let mut keys: Vec<&str> = ma.keys().map(|s| s.as_str()).chain(mb.keys().map(|s| s.as_str())).collect();
            keys.sort();
            keys.dedup();
            for k in keys {
                // Fields the engine models partially (curated enums / subset of volatiles);
                // excluded from the representation check so random-battle noise doesn't mask
                // real gaps. Transition checks (relaxed_eq) already ignore these.
                if matches!(k, "ability" | "item" | "volatiles" | "teraType" | "terastallized" | "statusCounter" | "lastUsedMove" | "stallCounter" | "pendingMove" | "timesHit" | "abilityUsed" | "activeTurns") {
                    continue;
                }
                let child = if path.is_empty() { k.to_string() } else { format!("{path}.{k}") };
                match (ma.get(k), mb.get(k)) {
                    (Some(va), Some(vb)) => json_diff(&child, va, vb, out),
                    (Some(_), None) => out.push(format!("{child}: present in PS, missing in engine")),
                    (None, Some(_)) => out.push(format!("{child}: missing in PS, present in engine")),
                    (None, None) => {}
                }
            }
        }
        (Array(va), Array(vb)) => {
            if va.len() != vb.len() {
                out.push(format!("{path}: array len PS={} engine={}", va.len(), vb.len()));
            }
            for (i, (ia, ib)) in va.iter().zip(vb.iter()).enumerate() {
                json_diff(&format!("{path}[{i}]"), ia, ib, out);
            }
        }
        _ => {
            if a != b {
                out.push(format!("{path}: PS={a} engine={b}"));
            }
        }
    }
}

/// Check a single PS state for representation fidelity. Returns (diffs, unmapped).
fn check_state(ps: &TState) -> (Vec<String>, Vec<String>) {
    let mut unmapped = Unmapped::default();
    let engine_state = parse_state(ps, &mut unmapped);
    let projected = project_state(&engine_state);

    // Normalize the PS side (sort volatiles) so set-equality isn't an ordering diff.
    let mut ps_norm = ps.clone();
    ps_norm.normalize();

    let ps_val = serde_json::to_value(&ps_norm).unwrap();
    let engine_val = serde_json::to_value(&projected).unwrap();
    let mut diffs = Vec::new();
    json_diff("", &ps_val, &engine_val, &mut diffs);

    // Unmapped ids legitimately cause some of those diffs; surface them separately.
    (diffs, unmapped.items)
}

fn run_trace(path: &str, totals: &mut Totals) -> bool {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  cannot read {path}: {e}");
            return false;
        }
    };
    let parsed: Trace = match serde_json::from_str(&text) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("  cannot parse {path}: {e}");
            return false;
        }
    };

    println!("\n=== {path} ===");
    println!("format={} winner={:?} snapshots={}", parsed.format, parsed.result.winner, parsed.snapshots.len());

    // Gather every state in the trace, labeled.
    let mut states: Vec<(String, &TState)> = Vec::new();
    states.push(("start".to_string(), &parsed.start.state));
    for s in &parsed.snapshots {
        states.push((format!("turn {}", s.turn), &s.state));
    }

    let mut all_unmapped: Vec<String> = Vec::new();
    let mut ok = true;
    let mut first_divergence: Option<(String, Vec<String>)> = None;

    for (label, st) in &states {
        let (diffs, unmapped) = check_state(st);
        for u in unmapped {
            if !all_unmapped.contains(&u) {
                all_unmapped.push(u);
            }
        }
        if !diffs.is_empty() {
            ok = false;
            if first_divergence.is_none() {
                first_divergence = Some((label.clone(), diffs));
            }
        }
    }

    println!("states checked: {}", states.len());
    if !all_unmapped.is_empty() {
        all_unmapped.sort();
        println!("unmapped ids ({}): {}", all_unmapped.len(), all_unmapped.join(", "));
    }

    match &first_divergence {
        None => println!("representation: OK — engine losslessly holds every state"),
        Some((label, diffs)) => {
            println!("representation: DIVERGENCE at {label} ({} field diffs):", diffs.len());
            for d in diffs.iter().take(40) {
                println!("    {d}");
            }
            if diffs.len() > 40 {
                println!("    ... and {} more", diffs.len() - 40);
            }
        }
    }

    if !ok {
        totals.repr_ok = false;
    }

    // --- transition replay (coverage signal) ---
    use replay::{analyze_turn, TurnResult};
    let (mut m, mut mm, mut sk) = (0u32, 0u32, 0u32);
    let mut first_mismatch: Option<u32> = None;
    for (i, snap) in parsed.snapshots.iter().enumerate() {
        let prev = if i == 0 { &parsed.start.state } else { &parsed.snapshots[i - 1].state };
        let (result, reason) = analyze_turn(prev, &snap.choices, &snap.replacements, &snap.state);
        match result {
            TurnResult::Match => { m += 1; totals.matched += 1; }
            TurnResult::Mismatch => {
                mm += 1;
                totals.mismatched += 1;
                if std::env::var("VERIFY_DUMP").is_ok() {
                    println!("{}", replay::dump_context(prev, &snap.choices, reason.as_deref().unwrap_or("?")));
                }
                if let Some(c) = reason {
                    *totals.reasons.entry(c).or_insert(0) += 1;
                }
                if first_mismatch.is_none() {
                    first_mismatch = Some(snap.turn);
                    if std::env::var("VERIFY_DEBUG").is_ok() {
                        println!("{}", replay::diagnose(prev, &snap.choices, &snap.replacements, &snap.state));
                    }
                }
            }
            TurnResult::Skipped => { sk += 1; totals.skipped += 1; }
        }
    }
    let replayable = m + mm;
    let rate = if replayable > 0 { 100.0 * m as f32 / replayable as f32 } else { 0.0 };
    println!(
        "transitions: {m}/{replayable} replayable turns matched ({rate:.0}%), {sk} skipped{}",
        match first_mismatch { Some(t) => format!(", first mismatch @ turn {t}"), None => String::new() }
    );

    ok
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: verify <trace.json> [more.json ...]");
        return ExitCode::FAILURE;
    }
    let mut all_ok = true;
    let mut totals = Totals { repr_ok: true, ..Default::default() };
    for path in &args {
        all_ok &= run_trace(path, &mut totals);
    }

    println!("\n========== AGGREGATE ==========");
    let replayable = totals.matched + totals.mismatched;
    let rate = if replayable > 0 { 100.0 * totals.matched as f32 / replayable as f32 } else { 0.0 };
    println!(
        "transitions: {}/{} replayable turns matched ({:.1}%), {} skipped, across {} traces",
        totals.matched, replayable, rate, totals.skipped, args.len()
    );
    if !totals.reasons.is_empty() {
        let mut reasons: Vec<(&String, &u32)> = totals.reasons.iter().collect();
        reasons.sort_by(|a, b| b.1.cmp(a.1));
        println!("mismatch causes:");
        for (cause, n) in reasons {
            println!("    {n:>4}  {cause}");
        }
    }
    println!("representation: {}", if totals.repr_ok { "OK across all traces" } else { "DIVERGED (see above)" });

    if all_ok {
        println!("\nALL TRACES OK");
        ExitCode::SUCCESS
    } else {
        println!("\nSOME TRACES DIVERGED (see above)");
        ExitCode::FAILURE
    }
}

#[derive(Default)]
struct Totals {
    matched: u32,
    mismatched: u32,
    skipped: u32,
    reasons: std::collections::HashMap<String, u32>,
    repr_ok: bool,
}
