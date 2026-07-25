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
mod digest;
mod drawdiff;
mod fixture;
mod export;
mod protocol_emit;
mod replay;
mod seedgate;
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

    // Raw seed→draw-stream gate: seed a PsPrng from the recorded battle seed and replay the
    // recorded draw log in order, checking every non-shuffle draw's result reproduces. Validates
    // the seed alignment (incl. the pre-turn-1 offset) and the PsPrng port at the call level,
    // independent of the engine. RAW_DRAW_GATE=1 cosim <traces...>.
    if std::env::var("RAW_DRAW_GATE").is_ok() {
        return run_raw_draw_gate(&args);
    }

    // Slim seed-fixture builder: convert full v2 traces into `*.fx.json.gz` gate fixtures
    // (per-decision state DIGESTS instead of full serialized states). MAKE_FIXTURE=<outdir>.
    if let Ok(outdir) = std::env::var("MAKE_FIXTURE") {
        return crate::fixture::run_make_fixture(&args, &outdir);
    }

    // Seed-driven full-battle Replicate gate (Phase 3 deliverable #2).
    if std::env::var("SEED_GATE").is_ok() {
        return crate::seedgate::run_seed_gate(&args);
    }

    // Exporter round-trip gate: for every corpus decision state S = convert(ps_snapshot), assert
    // convert(export(S)) == S exactly. Certifies the State exporter is a right-inverse of convert.
    if std::env::var("ROUNDTRIP_GATE").is_ok() {
        return run_roundtrip_gate(&args);
    }

    // Transplant sampler: for each trace, export the engine State at a mid-game turn-start `move`
    // decision as a `deserializeBattle`-loadable snapshot, alongside the transplant decision index
    // so `harness/transplant-gate.mjs` can drive the recorded remainder in pinned PS.
    if let Ok(outdir) = std::env::var("EXPORT_SAMPLE") {
        return run_export_sample(&args, &outdir);
    }

    // Protocol-log emitter: replay each recorded game through the engine and write its PS protocol
    // log (for the replay player / log-parity gate). PROTOCOL_EMIT=<outdir> cosim <traces>.
    if let Ok(outdir) = std::env::var("PROTOCOL_EMIT") {
        return protocol_emit::run_protocol_emit(&args, &outdir);
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
    let diff_label = std::env::var("DIFF_LABEL").ok();
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
                        drawdiff::DrawClass::Mismatch { label, detail, .. } => {
                            mm += 1;
                            if let Some(f) = &diff_label {
                                if label.contains(f.as_str()) {
                                    println!("MM {path} t{} | {label} | {detail}", u.turn);
                                }
                            }
                        }
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

/// Consume one recorded draw from `prng` and return whether its result reproduces (shuffles are
/// consumed but never checked — PS logs their order as null). `None` for an unknown kind.
fn replay_recorded_draw(prng: &mut engine::psprng::PsPrng, dr: &serde_json::Value) -> Option<bool> {
    use serde_json::Value;
    let kind = dr.get("kind").and_then(Value::as_str)?;
    let args: Vec<i64> = dr.get("args").and_then(Value::as_array)?
        .iter().filter_map(Value::as_i64).collect();
    let result = dr.get("result");
    match kind {
        "randomChance" => {
            let got = prng.random_chance(args[0] as u32, args[1] as u32);
            Some(result.and_then(Value::as_bool).map_or(true, |r| r == got))
        }
        "random" => {
            let got = if args.len() == 2 {
                prng.random_range(args[0] as u32, args[1] as u32)
            } else {
                prng.random_n(args[0] as u32)
            };
            Some(result.and_then(Value::as_i64).map_or(true, |r| r as u32 == got))
        }
        "sample" => {
            let got = prng.sample_index(args[0] as u32);
            Some(result.and_then(Value::as_i64).map_or(true, |r| r as u32 == got))
        }
        "shuffle" => {
            // args [len,start,end]: Fisher-Yates over [start,end) consumes end-1-start draws.
            let (start, end) = (args[1] as usize, args[2] as usize);
            let mut s = start;
            while s < end.saturating_sub(1) {
                let _ = prng.random_range(s as u32, end as u32);
                s += 1;
            }
            Some(true)
        }
        _ => None,
    }
}

/// Replay a game's full recorded draw stream through `prng` after burning `init` leading draws.
/// Returns (all_strong_draws_ok, strong_draws_checked). Strong = non-shuffle (result checkable).
fn replay_stream_with_init(t: &trace::Trace, limbs: [u16; 4], init: u32) -> (bool, u64) {
    let mut prng = engine::psprng::PsPrng::from_limbs(limbs);
    for _ in 0..init { let _ = prng.next(); }
    let mut strong = 0u64;
    for d in &t.decisions {
        for dr in &d.draws {
            match replay_recorded_draw(&mut prng, dr) {
                Some(true) => { if dr.get("kind").and_then(|v| v.as_str()) != Some("shuffle") { strong += 1; } }
                _ => return (false, strong),
            }
        }
    }
    (true, strong)
}

fn run_raw_draw_gate(args: &[String]) -> ExitCode {
    // INIT_SCAN=1: report the per-game unlogged init-draw offset (find minimal `init` in 0..64
    // reproducing the whole strong stream) to characterize the pre-turn-1 alignment convention.
    if std::env::var("INIT_SCAN").is_ok() {
        let mut hist: BTreeMap<i64, u32> = BTreeMap::new();
        let mut none = 0u32;
        for path in args {
            let t = match trace::load_trace(path) { Ok(t) => t, Err(_) => continue };
            let Some(limbs) = t.seed else { continue };
            let mut found = None;
            for init in 0..64u32 {
                let (ok, n) = replay_stream_with_init(&t, limbs, init);
                if ok && n >= 1 { found = Some(init); break; }
            }
            match found {
                Some(init) => { *hist.entry(init as i64).or_default() += 1; }
                None => { none += 1; println!("  NO-INIT-FITS: {path}"); }
            }
        }
        println!("INIT-OFFSET histogram (init draws -> #games):");
        for (init, c) in &hist { println!("  init={init:3}  {c} games"); }
        println!("  no-fit: {none}");
        return ExitCode::SUCCESS;
    }
    let mut games_ok = 0u32;
    let mut games = 0u32;
    let mut total_draws = 0u64;
    let mut fails: Vec<String> = Vec::new();
    for path in args {
        let t = match trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => { eprintln!("{e}"); return ExitCode::FAILURE; }
        };
        let Some(limbs) = t.seed else {
            fails.push(format!("{path}: no seed in trace"));
            continue;
        };
        games += 1;
        let mut prng = engine::psprng::PsPrng::from_limbs(limbs);
        let mut ok = true;
        let mut n = 0u64;
        'outer: for d in &t.decisions {
            for dr in &d.draws {
                match replay_recorded_draw(&mut prng, dr) {
                    Some(true) => { n += 1; }
                    Some(false) => {
                        ok = false;
                        fails.push(format!("{path}: draw #{n} mismatch: {}", serde_json::to_string(dr).unwrap_or_default()));
                        break 'outer;
                    }
                    None => {
                        ok = false;
                        fails.push(format!("{path}: draw #{n} unknown kind: {}", serde_json::to_string(dr).unwrap_or_default()));
                        break 'outer;
                    }
                }
            }
        }
        total_draws += n;
        if ok { games_ok += 1; }
    }
    println!("RAW DRAW-STREAM GATE: {games_ok}/{games} games reproduce the full recorded draw stream ({total_draws} draws checked)");
    for f in fails.iter().take(30) { println!("  {f}"); }
    if games_ok == games { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// Round-trip certification: `convert(export(convert(state_after))) == convert(state_after)` for
/// every convertible decision state in the corpus. Headline is the `move`-request unit count.
fn run_roundtrip_gate(args: &[String]) -> ExitCode {
    let verbose = std::env::var("VERBOSE").is_ok();
    let mut ps_commit: Option<String> = None;
    // (converted, exact) tallied per request-state class.
    let mut per_class: BTreeMap<String, (u32, u32)> = BTreeMap::new();
    let mut convert_failed: BTreeMap<String, u32> = BTreeMap::new();
    let mut mismatch_cats: BTreeMap<String, u32> = BTreeMap::new();
    let mut mismatches: Vec<String> = Vec::new();

    for path in args {
        let t = match trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => { eprintln!("{e}"); return ExitCode::FAILURE; }
        };
        match &ps_commit {
            None => ps_commit = Some(t.ps_commit.clone()),
            Some(c) if *c != t.ps_commit => {
                eprintln!("{path}: PS commit differs — refusing mixed corpus");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
        let Some(first) = t.decisions.first() else { continue };
        let canon = match convert::Canonical::from_first_state(&first.state_after) {
            Ok(c) => c,
            Err(u) => { *convert_failed.entry(format!("canon:{}", u.0)).or_insert(0) += 1; continue; }
        };
        let name = path.rsplit('/').next().unwrap_or(path);
        for d in &t.decisions {
            let class = d.request_state.clone();
            // Convert PS snapshot -> engine State (the input to the round-trip identity).
            let state = match convert::convert_state(&d.state_after, &canon) {
                Ok(s) => s,
                Err(u) => {
                    *convert_failed.entry(u.0.split([':', '[']).next().unwrap_or("?").to_string()).or_insert(0) += 1;
                    continue;
                }
            };
            // export -> convert back.
            let json = export::export_state(&state, t.seed.unwrap_or([1, 2, 3, 4]));
            let canon2 = match convert::Canonical::from_first_state(&json) {
                Ok(c) => c,
                Err(u) => {
                    let e = per_class.entry(class.clone()).or_insert((0, 0));
                    e.0 += 1;
                    *mismatch_cats.entry(format!("export-canon:{}", u.0)).or_insert(0) += 1;
                    mismatches.push(format!("{name} d{} t{} [{class}] export-canon: {}", d.index, d.turn, u.0));
                    continue;
                }
            };
            let back = match convert::convert_state(&json, &canon2) {
                Ok(s) => s,
                Err(u) => {
                    let e = per_class.entry(class.clone()).or_insert((0, 0));
                    e.0 += 1;
                    *mismatch_cats.entry(format!("export-convert:{}", u.0.split([':', '[']).next().unwrap_or("?"))).or_insert(0) += 1;
                    mismatches.push(format!("{name} d{} t{} [{class}] export-convert-failed: {}", d.index, d.turn, u.0));
                    continue;
                }
            };
            let e = per_class.entry(class.clone()).or_insert((0, 0));
            e.0 += 1;
            if back == state {
                e.1 += 1;
            } else {
                let diffs = diff::diff_states(&state, &back);
                for dd in &diffs {
                    *mismatch_cats.entry(dd.category.clone()).or_insert(0) += 1;
                }
                let detail = diffs.iter().take(4).map(|d| d.detail.as_str()).collect::<Vec<_>>().join(" | ");
                mismatches.push(format!("{name} d{} t{} [{class}] {detail}", d.index, d.turn));
            }
        }
    }

    println!("\n========== EXPORTER ROUND-TRIP GATE ==========");
    if let Some(c) = &ps_commit { println!("(ps {c})"); }
    let (mut tot_c, mut tot_e) = (0u32, 0u32);
    for (class, (c, e)) in &per_class {
        println!("  {class:12} converted {c:5}  exact {e:5}");
        tot_c += c;
        tot_e += e;
    }
    if let Some((c, e)) = per_class.get("move") {
        println!("\nROUND-TRIP (move units): {e} / {c} exact");
    }
    println!("ROUND-TRIP (all states): {tot_e} / {tot_c} exact");
    if !convert_failed.is_empty() {
        println!("\nconvert-unsupported states (excluded from round-trip denominator):");
        print_ranked("  reasons", &convert_failed);
    }
    if tot_e != tot_c {
        println!("\nMISMATCH categories:");
        print_ranked("  ", &mismatch_cats);
        println!("\nfirst {} mismatches:", 30.min(mismatches.len()));
        for m in mismatches.iter().take(30) {
            println!("  {m}");
        }
        if verbose {
            for m in mismatches.iter().skip(30) {
                println!("  {m}");
            }
        }
        ExitCode::FAILURE
    } else {
        println!("\nPASS: every convertible corpus state round-trips byte-exact.");
        ExitCode::SUCCESS
    }
}

/// Dump one transplant sample per trace: the exported (deserializeBattle-loadable) engine State
/// at a mid-game turn-start `move` decision, plus the decision index so the harness can drive the
/// recorded remainder. Writes `<outdir>/<name>.json` = {trace, name, decisionIndex, turn, seed,
/// exported}. Skips games with no eligible clean boundary.
fn run_export_sample(args: &[String], outdir: &str) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(outdir) {
        eprintln!("mkdir {outdir}: {e}");
        return ExitCode::FAILURE;
    }
    let mut written = 0u32;
    for path in args {
        let t = match trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => { eprintln!("{e}"); continue; }
        };
        let Some(first) = t.decisions.first() else { continue };
        let canon = match convert::Canonical::from_first_state(&first.state_after) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // Transplant point: a turn-start (non-midTurn) `move` decision at/after the game's
        // midpoint whose state converts cleanly, with recorded choices for BOTH sides (so the
        // continuation is drivable) and at least one decision of remainder.
        let mid_turn_threshold = t.result.turns / 2;
        let mut chosen: Option<usize> = None;
        for (i, d) in t.decisions.iter().enumerate() {
            if d.request_state != "move" || d.mid_turn { continue; }
            if d.turn < mid_turn_threshold { continue; }
            if i + 1 >= t.decisions.len() { continue; }
            if !d.choices.contains_key("p1") || !d.choices.contains_key("p2") { continue; }
            if convert::convert_state(&d.state_after, &canon).is_err() { continue; }
            chosen = Some(i);
            break;
        }
        let Some(i) = chosen else {
            eprintln!("{path}: no eligible transplant boundary");
            continue;
        };
        // Export the PRE-decision state: the recorded state_after of the decision BEFORE i is the
        // state at which the transplant decision is being made. Decision i's own state_after is
        // AFTER i resolves. So transplant from decision (i-1).state_after and replay from i.
        // Guard: i>=1 (i is a move decision at/after midpoint, so a prior decision exists).
        let pre = &t.decisions[i - 1];
        let state = match convert::convert_state(&pre.state_after, &canon) {
            Ok(s) => s,
            Err(_) => { eprintln!("{path}: pre-state convert failed"); continue; }
        };
        let exported = export::export_state(&state, t.seed.unwrap_or([1, 2, 3, 4]));
        let name = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".json.gz").trim_end_matches(".json");
        let out = serde_json::json!({
            "trace": path,
            "name": name,
            "transplantDecisionIndex": i,
            "preDecisionIndex": i - 1,
            "turn": t.decisions[i].turn,
            "seed": t.seed,
            "exported": exported,
        });
        let dest = format!("{outdir}/{name}.json");
        match std::fs::write(&dest, serde_json::to_string(&out).unwrap()) {
            Ok(_) => { written += 1; }
            Err(e) => eprintln!("write {dest}: {e}"),
        }
    }
    println!("EXPORT_SAMPLE: wrote {written} transplant snapshots to {outdir}");
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
