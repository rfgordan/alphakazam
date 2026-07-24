//! Engine protocol-log emitter over a recorded corpus game.
//!
//! Reconstructs a game the same way the seed gate does — seed a `PsPrng` from the recorded battle
//! seed, replicate the unlogged construction draws, then drive the recorded choices through the
//! engine's single-path Replicate executor — but instead of byte-comparing state, it feeds each
//! resolved turn `(pre-state, choices, chosen instructions)` to `engine::protocol::protocol_turn`
//! and accumulates the PS protocol lines. The output is a replayable `.log` (see
//! `harness/make-replay.mjs`) and the subject of the log-parity gate (`harness/protocol-parity.mjs`).
//!
//! This is a READ-ONLY parallel of `seedgate` (which is owned by the draw-exact campaign and must
//! not be edited); the Replicate logic is reimplemented here against the same public generate API.

use std::process::ExitCode;

use engine::generate::{generate_instructions_annotated, AnnotatedOutcome, MoveChoice};
use engine::protocol::{protocol_turn, HpStyle};
use engine::psprng::PsPrng;
use engine::state::State;
use serde_json::Value;

use crate::convert::{convert_state, side_id, species_id_of_details, Canonical};
use crate::replay::{active_fainted, resolve_choice};
use crate::trace::{Decision, Trace};

static FIXED_GENDER_IDS: &str = include_str!("fixed_gender.txt");

fn init_gender_rolls(t: &Trace) -> u32 {
    if t.format.contains("random") {
        return 0;
    }
    let fixed: std::collections::HashSet<&str> =
        FIXED_GENDER_IDS.lines().map(str::trim).filter(|s| !s.is_empty()).collect();
    let st = &t.decisions[0].state_after;
    let mut n = 0u32;
    for side in st["sides"].as_array().into_iter().flatten() {
        for mon in side["pokemon"].as_array().into_iter().flatten() {
            let det = mon.get("details").and_then(Value::as_str).unwrap_or("");
            if !fixed.contains(species_id_of_details(det).as_str()) {
                n += 1;
            }
        }
    }
    n
}

fn consume(prng: &mut PsPrng, kind: &str, args: &[i32]) -> i64 {
    match kind {
        "randomChance" => prng.random_chance(args[0] as u32, args[1] as u32) as i64,
        "random" => {
            if args.len() == 2 {
                prng.random_range(args[0] as u32, args[1] as u32) as i64
            } else {
                prng.random_n(args[0] as u32) as i64
            }
        }
        "sample" => prng.sample_index(args[0] as u32) as i64,
        "shuffle" => {
            let (start, end) = (args[1] as usize, args[2] as usize);
            let mut s = start;
            while s < end.saturating_sub(1) {
                let _ = prng.random_range(s as u32, end as u32);
                s += 1;
            }
            -1
        }
        _ => -1,
    }
}

fn consume_recorded(prng: &mut PsPrng, d: &Decision) {
    for dr in &d.draws {
        let kind = dr.get("kind").and_then(Value::as_str).unwrap_or("");
        let args: Vec<i32> = dr
            .get("args")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_i64).map(|x| x as i32).collect())
            .unwrap_or_default();
        consume(prng, kind, &args);
    }
}

/// Replicate: select the realized outcome by consuming `prng` at the engine's draw sites (a copy
/// of the seed-gate selection, sufficient for driving the recorded line to emit its protocol).
fn replicate_select(outcomes: &[AnnotatedOutcome], prng: &mut PsPrng) -> usize {
    if outcomes.len() == 1 {
        for d in &outcomes[0].draws {
            consume(prng, d.kind, &d.args);
        }
        return 0;
    }
    let mut live: Vec<usize> = (0..outcomes.len()).collect();
    let mut pos = 0usize;
    loop {
        let cands: Vec<usize> = live.iter().copied().filter(|&i| outcomes[i].draws.len() > pos).collect();
        if cands.is_empty() {
            break;
        }
        let rep = &outcomes[cands[0]].draws[pos];
        let res = consume(prng, rep.kind, &rep.args);
        if rep.kind == "shuffle" {
            live = cands;
        } else if rep.kind == "random" && rep.args == [100] {
            let mut distinct: Vec<i64> = cands.iter().map(|&i| outcomes[i].draws[pos].result).collect();
            distinct.sort_unstable();
            distinct.dedup();
            let filtered: Vec<usize> = if distinct.len() == 2 && distinct[0] == 0 && distinct[1] > 0 {
                let chance = distinct[1];
                let want = if res < chance { 0 } else { chance };
                cands.iter().copied().filter(|&i| outcomes[i].draws[pos].result == want).collect()
            } else {
                let f: Vec<usize> = cands.iter().copied().filter(|&i| outcomes[i].draws[pos].result == res).collect();
                if f.is_empty() { cands.clone() } else { f }
            };
            live = if filtered.is_empty() { cands } else { filtered };
        } else {
            let filtered: Vec<usize> =
                cands.iter().copied().filter(|&i| outcomes[i].draws[pos].result == res).collect();
            live = if filtered.is_empty() { cands } else { filtered };
        }
        pos += 1;
    }
    *live
        .iter()
        .max_by(|&&a, &&b| outcomes[a].percentage.total_cmp(&outcomes[b].percentage).then(b.cmp(&a)))
        .unwrap_or(&0)
}

/// Emit the full protocol log for one game, or an error label.
fn emit_game(t: &Trace, hp_style: HpStyle) -> Result<Vec<String>, String> {
    let Some(limbs) = t.seed else { return Err("no-seed".into()) };
    let Some(first) = t.decisions.first() else { return Err("empty".into()) };
    if first.request_state != "teampreview" {
        return Err(format!("first-{}", first.request_state));
    }
    let canon = Canonical::from_first_state(&first.state_after).map_err(|u| format!("canon:{}", u.0))?;
    let sleep_clause = t.format.contains("randombattle");

    let mut prng = PsPrng::from_limbs(limbs);
    for _ in 0..init_gender_rolls(t) {
        let _ = prng.next();
    }
    consume_recorded(&mut prng, first);

    let mut state = convert_state(&first.state_after, &canon).map_err(|u| format!("convert0:{}", u.0))?;
    state.sleep_clause = sleep_clause;

    let mut out = Vec::new();
    out.push("|start".to_string());
    // Initial lead switch-ins (PS emits |switch| for both leads at battle start).
    for side in [engine::state::SideId::One, engine::state::SideId::Two] {
        if state.side(side).active_index != u8::MAX {
            out.push(engine::protocol::switch_line(&state, side, hp_style));
        }
    }

    let mut i = 1usize;
    while i < t.decisions.len() {
        let dp = &t.decisions[i];
        if dp.request_state != "move" {
            return Err(format!("unexpected-{}", dp.request_state));
        }
        let mut unit: Vec<&Decision> = vec![dp];
        let mut j = i + 1;
        while j < t.decisions.len() && t.decisions[j].request_state == "switch" {
            unit.push(&t.decisions[j]);
            j += 1;
        }
        step_unit(&mut state, &unit, &canon, &mut prng, hp_style, &mut out)?;
        i = j;
    }
    if let Some(w) = &t.result.winner {
        out.push(format!("|win|{}", if w == "Red" || w.starts_with("p1") { "Red" } else { "Blue" }));
    } else if t.result.ended {
        out.push("|tie".to_string());
    }
    Ok(out)
}

fn step_unit(
    state: &mut State,
    unit: &[&Decision],
    canon: &Canonical,
    prng: &mut PsPrng,
    hp_style: HpStyle,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let dp = unit[0];
    let mut mc = [MoveChoice::Move(0); 2];
    let mut tera = [false; 2];
    for (si, side_key) in ["p1", "p2"].iter().enumerate() {
        let Some(choice) = dp.choices.get(*side_key) else { return Err("no-choice".into()) };
        let (m, tr) = resolve_choice(state, si, choice, canon).map_err(|u| format!("resolve:{}", u.0))?;
        mc[si] = m;
        tera[si] = tr;
    }

    let mut pivots: [Option<u8>; 2] = [None, None];
    let mut replacements: Vec<(usize, u8)> = Vec::new();
    for (k, sw) in unit.iter().enumerate().skip(1) {
        let pending_state = &unit[k - 1].state_after;
        for (side_key, choice) in &sw.choices {
            let si = if side_key == "p1" { 0 } else { 1 };
            let slot = if let Some(ri) = choice.resolved.roster_index {
                ri
            } else {
                let Some(details) = &choice.resolved.details else { return Err("switch-no-details".into()) };
                canon.slot(si, &species_id_of_details(details)).map_err(|u| format!("switch-slot:{}", u.0))?
            };
            if active_fainted(pending_state, si, sw, side_key) {
                replacements.push((si, slot));
            } else {
                pivots[si] = Some(slot);
            }
        }
    }

    // The engine carries state forward without a turn counter; stamp the recorded turn so the
    // `|turn|N` marker advances.
    state.turn = dp.turn;
    let pre = *state;

    let tie = engine::generate::move_order_tie(state, mc[0], mc[1]);
    if tie {
        let mut peek = *prng;
        let k = tera[0] as u32 + tera[1] as u32;
        let b0 = peek.random_range(0, 2);
        let _ = peek.random_range(0, 2);
        let _ = peek.random_range(0, 2);
        for _ in 0..k {
            let _ = peek.random_range(0, 2);
        }
        let b3 = peek.random_range(0, 2);
        engine::generate::set_forced_tie_order(Some(b0 == b3));
    }
    engine::generate::set_realized_source(Some(engine::generate::RealizedSource::Prng(*prng)));
    let outcomes = generate_instructions_annotated(state, mc[0], mc[1], pivots, tera);
    engine::generate::set_realized_source(None);
    engine::generate::set_forced_tie_order(None);
    if outcomes.is_empty() {
        return Err("no-branches".into());
    }
    let choice = replicate_select(&outcomes, prng);
    let instrs = &outcomes[choice].instructions;

    // Emit protocol for this turn from the PRE-state + chosen instructions.
    protocol_turn(&pre, mc[0], mc[1], instrs, hp_style, out);

    state.apply_instructions(instrs);

    // Replacement switches (fainted mon replaced this unit) — reflected in the log as |switch|.
    let pre_end_turn = !replacements.is_empty() && unit.last().is_some_and(|d| d.turn == dp.turn);
    let mut replaced = [false; 2];
    if replacements.len() == 2 && replacements[0].0 != replacements[1].0 {
        let pre = *state;
        let ins = engine::generate::switch_into_pair(
            state,
            [(side_id(replacements[0].0), replacements[0].1), (side_id(replacements[1].0), replacements[1].1)],
        );
        engine::protocol::emit_instructions(&pre, &ins, hp_style, out);
        replaced = [true, true];
    } else {
        for &(side, slot) in &replacements {
            let pre = *state;
            let ins = engine::generate::switch_into(state, side_id(side), slot);
            engine::protocol::emit_instructions(&pre, &ins, hp_style, out);
            replaced[side] = true;
        }
    }
    for (side, was_replaced) in replaced.iter().enumerate() {
        if *was_replaced && !pre_end_turn {
            state.sides[side].active_turns = state.sides[side].active_turns.saturating_add(1);
        } else if !*was_replaced && pre_end_turn && state.sides[side].active().is_alive() {
            state.sides[side].active_turns = state.sides[side].active_turns.saturating_sub(1);
        }
    }
    Ok(())
}

/// `PROTOCOL_EMIT=<outdir> cosim <traces>`: write `<name>.log` per game. `PROTOCOL_EXACT=1` for
/// exact HP fractions instead of the public `/100` form.
pub fn run_protocol_emit(args: &[String], outdir: &str) -> ExitCode {
    if let Err(e) = std::fs::create_dir_all(outdir) {
        eprintln!("mkdir {outdir}: {e}");
        return ExitCode::FAILURE;
    }
    let hp_style = if std::env::var("PROTOCOL_EXACT").is_ok() { HpStyle::Exact } else { HpStyle::Percent };
    let mut ok = 0u32;
    let mut fail = 0u32;
    for path in args {
        let t = match crate::trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                continue;
            }
        };
        let name = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".json.gz").trim_end_matches(".json");
        match emit_game(&t, hp_style) {
            Ok(lines) => {
                let dest = format!("{outdir}/{name}.log");
                if let Err(e) = std::fs::write(&dest, lines.join("\n")) {
                    eprintln!("write {dest}: {e}");
                    fail += 1;
                } else {
                    ok += 1;
                }
            }
            Err(label) => {
                eprintln!("{name}: emit failed: {label}");
                fail += 1;
            }
        }
    }
    println!("PROTOCOL_EMIT: wrote {ok} logs to {outdir} ({fail} failed)");
    ExitCode::SUCCESS
}
