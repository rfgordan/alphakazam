//! Co-simulation verifier: does the Rust engine compute the same function as Pokémon Showdown?
//!
//! Replays v2 cosim traces (full serialized PS states + choices + RNG draws) and classifies
//! every turn unit Matched / Diverged / Unsupported with exact full-field comparison. Reports
//! ranked divergence categories, the unsupported frontier, legality mismatches, and verified
//! per-mechanic coverage.
//!
//! Usage: cosim harness/cosim-traces/*.json.gz
//!        VERBOSE=1 cosim trace.json.gz   # per-unit detail

mod convert;
mod diff;
mod drawdiff;
mod replay;
mod trace;

use std::collections::BTreeMap;
use std::process::ExitCode;

use replay::{replay_trace, Verdict};

#[derive(Default)]
struct Totals {
    matched: u32,
    diverged: u32,
    unsupported: u32,
    divergence_categories: BTreeMap<String, u32>,
    unsupported_reasons: BTreeMap<String, u32>,
    legality: BTreeMap<String, u32>,
    /// move id -> (times in matched units, times in non-matched units)
    move_coverage: BTreeMap<String, (u32, u32)>,
    /// Ability/item ids present in the serialized states of FULLY-matched traces
    /// (presence-in-exact-games accounting for the equivalence campaign).
    abilities_present: std::collections::BTreeSet<String>,
    items_present: std::collections::BTreeSet<String>,
    /// frontier id -> number of traces it appears in (the work queue, ranked by blockage)
    frontier_traces: BTreeMap<String, u32>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("usage: cosim <trace.json[.gz]> [more ...]");
        return ExitCode::FAILURE;
    }
    let verbose = std::env::var("VERBOSE").is_ok();

    // Draw-consumption differ mode: classify each unit DRAW-EXACT vs first-mismatch category,
    // and print the burn-down scoreboard. Leaves the state-verification path untouched.
    if std::env::var("DRAW_DIFF").is_ok() {
        return run_draw_diff(&args);
    }

    let mut totals = Totals::default();
    let mut ps_commit: Option<String> = None;

    for path in &args {
        let t = match trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::FAILURE;
            }
        };
        match &ps_commit {
            None => ps_commit = Some(t.ps_commit.clone()),
            Some(c) if *c != t.ps_commit => {
                eprintln!("{path}: PS commit {} differs from {} — corpus is mixed; refusing", t.ps_commit, c);
                return ExitCode::FAILURE;
            }
            _ => {}
        }

        // Full-frontier scan: every unmapped id anywhere in this trace, deduped.
        let mut frontier = std::collections::BTreeSet::new();
        for d in &t.decisions {
            convert::scan_frontier(&d.state_after, &mut frontier);
        }
        for f in frontier {
            *totals.frontier_traces.entry(f).or_insert(0) += 1;
        }

        let units = match replay_trace(&t) {
            Ok(u) => u,
            Err(u) => {
                println!("=== {path} === UNREPLAYABLE: {}", u.0);
                totals.unsupported += 1;
                *totals.unsupported_reasons.entry(u.0).or_insert(0) += 1;
                continue;
            }
        };

        let (mut m, mut dv, mut un) = (0, 0, 0);
        for unit in &units {
            for l in &unit.legality {
                let cat = l.split([':', '[']).next().unwrap_or(l).to_string();
                *totals.legality.entry(cat).or_insert(0) += 1;
                if verbose {
                    println!("  [legality t{}] {}", unit.turn, l);
                }
            }
            let matched = matches!(unit.verdict, Verdict::Matched);
            for mv in &unit.moves_used {
                let e = totals.move_coverage.entry(mv.clone()).or_insert((0, 0));
                if matched { e.0 += 1 } else { e.1 += 1 }
            }
            match &unit.verdict {
                Verdict::Matched => m += 1,
                Verdict::Diverged { closest, branches } => {
                    dv += 1;
                    for diff in closest {
                        *totals.divergence_categories.entry(diff.category.clone()).or_insert(0) += 1;
                    }
                    if verbose {
                        println!("  [diverged t{} | {} branches] {}| {}", unit.turn, branches,
                            unit.choice_summary,
                            closest.iter().take(6).map(|d| d.detail.as_str()).collect::<Vec<_>>().join(" | "));
                        if std::env::var("DRAWS").is_ok() {
                            println!("      draws: {}", unit.draws_summary);
                        }
                    }
                }
                Verdict::DistributionDiverged { detail, branches, outcomes } => {
                    dv += 1;
                    *totals.divergence_categories.entry("distribution".into()).or_insert(0) += 1;
                    if verbose {
                        println!("  [distribution-diverged t{} | {} rust branches, {} PS outcomes] {}| {}",
                            unit.turn, branches, outcomes, unit.choice_summary, detail);
                    }
                }
                Verdict::Unsupported(u) => {
                    un += 1;
                    *totals.unsupported_reasons.entry(u.0.clone()).or_insert(0) += 1;
                    if verbose {
                        println!("  [unsupported t{}] {}", unit.turn, u.0);
                    }
                }
            }
        }
        totals.matched += m;
        if dv == 0 && un == 0 {
            for d in &t.decisions {
                collect_ability_items(&d.state_after, &mut totals);
            }
        }
        totals.diverged += dv;
        totals.unsupported += un;
        println!("=== {path} === units: {} | matched {m} | diverged {dv} | unsupported {un}", units.len());
    }

    let supported = totals.matched + totals.diverged;
    let total = supported + totals.unsupported;
    println!("\n========== COSIM AGGREGATE (ps {}) ==========", ps_commit.as_deref().unwrap_or("?"));
    println!(
        "units: {total} | matched {} | diverged {} | unsupported {}",
        totals.matched, totals.diverged, totals.unsupported
    );
    if supported > 0 {
        println!(
            "EXACTNESS (matched / supported): {:.2}%   <- must be 100",
            100.0 * totals.matched as f64 / supported as f64
        );
    }
    if total > 0 {
        println!(
            "COVERAGE  (supported / total):   {:.2}%   <- grows as mechanics are modeled",
            100.0 * supported as f64 / total as f64
        );
    }
    print_ranked("divergence categories", &totals.divergence_categories);
    print_ranked("FRONTIER (work queue, by traces blocked)", &totals.frontier_traces);
    print_ranked("unsupported frontier", &totals.unsupported_reasons);
    print_ranked("legality mismatches", &totals.legality);

    let exercised = totals.move_coverage.iter().filter(|(_, (m, _))| *m > 0).count();
    let unverified: Vec<&String> = totals
        .move_coverage
        .iter()
        .filter(|(_, (m, n))| *m == 0 && *n > 0)
        .map(|(k, _)| k)
        .collect();
    println!("\nmove coverage: {exercised} move ids exercised in matched units");
    if !unverified.is_empty() {
        println!("  used but never in a matched unit: {unverified:?}");
    }
    // Strict coverage accounting for the equivalence campaign: EXERCISED_DUMP=1 prints the full
    // matched-unit move-id set (one line, sorted) for diffing against the randbats-eligible list
    // (harness/coverage-worklist.json).
    if std::env::var("EXERCISED_DUMP").is_ok() {
        let ids: Vec<&str> = totals
            .move_coverage
            .iter()
            .filter(|(_, (m, _))| *m > 0)
            .map(|(k, _)| k.as_str())
            .collect();
        println!("EXERCISED: {}", ids.join(" "));
        let ab: Vec<&str> = totals.abilities_present.iter().map(|s| s.as_str()).collect();
        println!("ABILITIES_EXACT: {}", ab.join(" "));
        let it: Vec<&str> = totals.items_present.iter().map(|s| s.as_str()).collect();
        println!("ITEMS_EXACT: {}", it.join(" "));
    }

    // Gate decision: any divergence is a hard failure. Unsupported units are ALSO a failure by
    // default — otherwise a converter regression that pushes units into Unsupported would make
    // the gate silently vacuous (it would exit 0 while verifying nothing). Set ALLOW_UNSUPPORTED=1
    // to restore the old behavior when deliberately growing coverage on a new corpus.
    let allow_unsupported = std::env::var("ALLOW_UNSUPPORTED").is_ok();
    if totals.diverged > 0 {
        ExitCode::FAILURE
    } else if totals.unsupported > 0 && !allow_unsupported {
        println!(
            "\nFAIL: {} unsupported unit(s) — gate would be vacuous. Set ALLOW_UNSUPPORTED=1 to permit while growing coverage.",
            totals.unsupported
        );
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn run_draw_diff(args: &[String]) -> ExitCode {
    let verbose = std::env::var("VERBOSE").is_ok();
    let mut board = drawdiff::DrawScoreboard::default();
    let mut ps_commit: Option<String> = None;
    for path in args {
        let t = match trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => { eprintln!("{e}"); return ExitCode::FAILURE; }
        };
        match &ps_commit {
            None => ps_commit = Some(t.ps_commit.clone()),
            Some(c) if *c != t.ps_commit => {
                eprintln!("{path}: PS commit differs — corpus is mixed; refusing");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
        match drawdiff::draw_diff_trace(&t) {
            Ok(units) => {
                let (mut ex, mut mm, mut un) = (0u32, 0u32, 0u32);
                for u in &units {
                    board.add(u);
                    match &u.class {
                        drawdiff::DrawClass::Exact => ex += 1,
                        drawdiff::DrawClass::Unsupported(_) => un += 1,
                        drawdiff::DrawClass::Mismatch { .. } => mm += 1,
                    }
                }
                if verbose {
                    println!("=== {path} === units: {} | draw-exact {ex} | mismatch {mm} | unsupported {un}", units.len());
                }
            }
            Err(u) => eprintln!("{path}: {}", u.0),
        }
    }
    if let Some(c) = ps_commit {
        println!("(ps {c})");
    }
    board.report();
    ExitCode::SUCCESS
}

fn print_ranked(label: &str, map: &BTreeMap<String, u32>) {
    if map.is_empty() {
        return;
    }
    let mut v: Vec<(&String, &u32)> = map.iter().collect();
    v.sort_by(|a, b| b.1.cmp(a.1));
    println!("{label}:");
    for (k, n) in v.iter().take(15) {
        println!("    {n:>5}  {k}");
    }
}

/// Walk a PS serialized battle state collecting pokemon ability/item ids. Presence in a
/// fully-matched trace means these ids participated in exactly-verified games.
fn collect_ability_items(v: &serde_json::Value, totals: &mut Totals) {
    let Some(sides) = v.get("sides").and_then(|s| s.as_array()) else { return };
    for side in sides {
        let Some(mons) = side.get("pokemon").and_then(|p| p.as_array()) else { continue };
        for mon in mons {
            for key in ["ability", "baseAbility"] {
                if let Some(a) = mon.get(key).and_then(|x| x.as_str()) {
                    if !a.is_empty() {
                        totals.abilities_present.insert(a.to_string());
                    }
                }
            }
            if let Some(i) = mon.get("item").and_then(|x| x.as_str()) {
                if !i.is_empty() {
                    totals.items_present.insert(i.to_string());
                }
            }
        }
    }
}
