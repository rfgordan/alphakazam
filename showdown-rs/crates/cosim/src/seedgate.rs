//! Seed-driven full-battle Replicate gate (DRAW_EXACT Phase 3, deliverable #2).
//!
//! For each committed trace: construct the initial battle state (from the post-teampreview
//! snapshot), seed a `PsPrng` from the recorded battle seed, REPLICATE the unlogged
//! battle-construction draws (per-mon gender rolls — see `init_gender_rolls`), consume the
//! recorded teampreview draw *shapes*, then drive the engine's single-path `Replicate` executor
//! with the recorded choices — carrying the engine's OWN evolving state forward and
//! byte-comparing the converted state after EVERY decision. Any drift desyncs the PRNG stream
//! and shows immediately (that is the feature).
//!
//! Metric: % of FULL GAMES exact end-to-end. Per-game first-divergence labels form the Phase-3
//! burn-down queue. `SEED_GATE=1 cosim <traces...>`.
//!
//! ## Replicate executor (the filter)
//! The engine's `Enumerate` mode already emits, per outcome branch, the ordered PRNG draws (PS
//! call form) with their realized results (`generate_instructions_annotated`). `Replicate`
//! selects the single realized outcome by walking the draw positions: at each position it
//! consumes the real `PsPrng` with the branch's `(kind,args)` and keeps only the branches whose
//! recorded result matches the drawn value. Shuffles carry no result (order is state-neutral in
//! the engine's model) so they are consumed for their exact draw count but do not filter — the
//! one place this loses information is a move-order Speed tie (both order-branches share an
//! identical draw stream), reported as its own divergence class.

use std::process::ExitCode;

use engine::generate::{generate_instructions_annotated, AnnotatedOutcome, DrawEvent, MoveChoice};
use engine::psprng::PsPrng;
use engine::state::State;
use serde_json::Value;

use crate::convert::{convert_state, side_id, species_id_of_details, Canonical};
use crate::diff::diff_states;
use crate::replay::{active_fainted, resolve_choice};
use crate::trace::{Decision, Trace};

/// Species with a fixed `gender` field in PS's gen9 dex (genderless "N", or single-gender M/F).
/// A mon of such a species does NOT roll `sample(["M","F"])` at `new Pokemon`; every other
/// (dual-gender) species with an unspecified set gender rolls one `sample` draw at construction.
static FIXED_GENDER_IDS: &str = include_str!("fixed_gender.txt");

fn fixed_gender_set() -> std::collections::HashSet<&'static str> {
    FIXED_GENDER_IDS.lines().map(str::trim).filter(|s| !s.is_empty()).collect()
}

/// The number of unlogged battle-construction PRNG draws to burn to align the stream to turn 1.
///
/// PS's `new Pokemon` rolls `this.battle.sample(["M","F"])` (one draw) for every mon whose
/// species has no fixed gender AND whose set leaves gender unspecified, in side-then-roster
/// order (`Side.addPokemon`). Random-battle formats pre-generate teams whose sets carry an
/// explicit gender, so those mons don't roll — 0 construction draws. Custom-game corpora use
/// fixed sets with (mostly) empty gender, so dual-gender species roll.
///
/// A set that specifies its own gender (the recorded `setGender` roster field is non-empty)
/// suppresses the construction roll: PS's `new Pokemon` uses `set.gender || species.gender ||
/// sample(['M','F'])`, so an explicit set gender short-circuits before the sample. Directed
/// custom teams (the c5 batch) fix genders in the packed set for deterministic Attract/Cute
/// Charm legality, so those mons roll NOTHING at construction. Traces recorded before the
/// recorder captured `setGender` lack the field entirely; those are treated as empty, preserving
/// the original (empty-set-gender ⇒ roll for every dual-gender species) accounting.
fn init_gender_rolls(t: &Trace) -> u32 {
    if t.format.contains("random") {
        return 0;
    }
    let fixed = fixed_gender_set();
    let st = &t.decisions[0].state_after;
    let mut n = 0u32;
    for side in st["sides"].as_array().into_iter().flatten() {
        for mon in side["pokemon"].as_array().into_iter().flatten() {
            let det = mon.get("details").and_then(Value::as_str)
                .or_else(|| mon.get("speciesForme").and_then(Value::as_str))
                .or_else(|| mon.get("species").and_then(Value::as_str))
                .unwrap_or("");
            let sid = species_id_of_details(det);
            // Explicit set gender ⇒ no construction roll (PS short-circuits the sample).
            let set_gender_explicit = mon.get("setGender").and_then(Value::as_str)
                .map(|g| !g.is_empty()).unwrap_or(false);
            if !fixed.contains(sid.as_str()) && !set_gender_explicit {
                n += 1;
            }
        }
    }
    n
}

/// Consume one draw of the given PS call shape from `prng`, returning the realized result under
/// PS's interpretation (randomChance -> 0/1, random/sample -> value, shuffle -> -1).
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

/// Consume the recorded draws of a decision purely for their PRNG-stream shape (used for the
/// teampreview action, which the engine does not model — it only advances the stream).
fn consume_recorded(prng: &mut PsPrng, d: &Decision) {
    for dr in &d.draws {
        let kind = dr.get("kind").and_then(Value::as_str).unwrap_or("");
        let args: Vec<i32> = dr.get("args").and_then(Value::as_array).map(|a| {
            a.iter().filter_map(Value::as_i64).map(|x| x as i32).collect()
        }).unwrap_or_default();
        consume(prng, kind, &args);
    }
}

/// Per-side Beat Up participant order for a unit: the canonical party slots (`rosterIndex`) in the
/// order PS serializes `side.pokemon` at the given pre-state — PS's current array order, which
/// `switchIn` keeps active-first + swap-tracked. `beatup_calcs` iterates these to pair each
/// member's base power with its hit's roll exactly as PS does.
fn beatup_orders(pre: &Value) -> [Option<Vec<u8>>; 2] {
    let mut out: [Option<Vec<u8>>; 2] = [None, None];
    let Some(sides) = pre.get("sides").and_then(Value::as_array) else { return out };
    for (si, side) in sides.iter().enumerate().take(2) {
        if let Some(mons) = side.get("pokemon").and_then(Value::as_array) {
            let order: Vec<u8> = mons.iter()
                .filter_map(|p| p.get("rosterIndex").and_then(Value::as_i64).map(|r| r as u8))
                .collect();
            if !order.is_empty() {
                out[si] = Some(order);
            }
        }
    }
    out
}

/// Replicate: select the single realized outcome by consuming `prng` at the engine's draw sites.
/// Returns `(chosen_outcome_index, ambiguous)`. `ambiguous` is set when >1 outcome survives the
/// filter (an unfilterable shuffle fork — move-order Speed tie).
fn replicate_select(outcomes: &[AnnotatedOutcome], prng: &mut PsPrng) -> (usize, bool) {
    if outcomes.len() == 1 {
        for d in &outcomes[0].draws {
            consume(prng, d.kind, &d.args);
        }
        return (0, false);
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
            // Threshold-encoded proc split: the engine annotates each branch's result as the LOWER
            // BOUND of the drawn-value range that selects it (binary secondary: proc=0, noproc=chance
            // — drawn<chance -> proc; multi-way Effect Spore: slp=0, par=11, psn=21, none=30). The
            // realized `res` selects the branch with the LARGEST threshold <= res. (The differ
            // compares only kinds/args, so these representative results are safe to reinterpret.)
            let mut distinct: Vec<i64> = cands.iter().map(|&i| outcomes[i].draws[pos].result).collect();
            distinct.sort_unstable();
            distinct.dedup();
            let filtered: Vec<usize> = if distinct.len() >= 2 && distinct[0] == 0 {
                let want = *distinct.iter().rev().find(|&&t| t <= res).unwrap_or(&0);
                cands.iter().copied().filter(|&i| outcomes[i].draws[pos].result == want).collect()
            } else {
                // Single-branch draw-and-discard, or a multi-way split we can't threshold — try
                // exact match, else keep all (an ambiguity the state compare will surface).
                let f: Vec<usize> = cands.iter().copied().filter(|&i| outcomes[i].draws[pos].result == res).collect();
                if f.is_empty() { cands.clone() } else { f }
            };
            live = if filtered.is_empty() { cands } else { filtered };
        } else {
            let filtered: Vec<usize> = cands.iter().copied()
                .filter(|&i| outcomes[i].draws[pos].result == res)
                .collect();
            live = if filtered.is_empty() { cands } else { filtered };
        }
        pos += 1;
    }
    let ambiguous = live.len() > 1;
    let choice = *live.iter().max_by(|&&a, &&b| {
        outcomes[a].percentage.total_cmp(&outcomes[b].percentage).then(b.cmp(&a))
    }).unwrap_or(&0);
    (choice, ambiguous)
}

thread_local! {
    /// Debug: when set, `step_unit` dumps every generated outcome's draw stream. Enabled per
    /// (game, decision-index) via `DBG_GAME`/`DBG_I` env vars, set in `run_game`.
    static DBG_UNIT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct GameResult {
    name: String,
    exact: bool,
    decisions_ok: u32,
    total_decisions: u32,
    first_divergence: Option<String>,
    aligned: bool,
}

fn run_game(path: &str, t: &Trace) -> GameResult {
    let name = path.rsplit('/').next().unwrap_or(path).to_string();
    let mk_fail = |first: Option<String>, ok: u32, total: u32, aligned: bool| GameResult {
        name: name.clone(), exact: false, decisions_ok: ok, total_decisions: total,
        first_divergence: first, aligned,
    };

    let Some(limbs) = t.seed else {
        return mk_fail(Some("no-seed".into()), 0, 0, false);
    };
    let Some(first) = t.decisions.first() else {
        return mk_fail(Some("empty-trace".into()), 0, 0, false);
    };
    if first.request_state != "teampreview" {
        return mk_fail(Some(format!("first-{}", first.request_state)), 0, 0, false);
    }
    let canon = match Canonical::from_first_state(&first.state_after) {
        Ok(c) => c,
        Err(u) => return mk_fail(Some(format!("canon:{}", u.0)), 0, 0, false),
    };
    let sleep_clause = t.format.contains("randombattle");
    let aligned = alignment_ok(t, limbs);

    let mut prng = PsPrng::from_limbs(limbs);
    for _ in 0..init_gender_rolls(t) {
        let _ = prng.next();
    }
    consume_recorded(&mut prng, first);

    let mut state = match convert_state(&first.state_after, &canon) {
        Ok(mut s) => { s.sleep_clause = sleep_clause; s }
        Err(u) => return mk_fail(Some(format!("convert0:{}", u.0)), 0, 0, aligned),
    };

    let mut i = 1usize;
    let mut decisions_ok = 0u32;
    let mut total = 0u32;
    while i < t.decisions.len() {
        let dp = &t.decisions[i];
        if dp.request_state != "move" {
            return mk_fail(Some(format!("unexpected-{}", dp.request_state)), decisions_ok, total, aligned);
        }
        let mut unit: Vec<&Decision> = vec![dp];
        let mut j = i + 1;
        while j < t.decisions.len() && t.decisions[j].request_state == "switch" {
            unit.push(&t.decisions[j]);
            j += 1;
        }
        total += 1;
        let dbg_on = std::env::var("DBG_GAME").ok().is_some_and(|g| name.starts_with(&g))
            && std::env::var("DBG_I").ok().and_then(|v| v.parse::<usize>().ok()).is_none_or(|di| di == i);
        DBG_UNIT.with(|c| c.set(dbg_on));
        if dbg_on { eprintln!("=== {name} d{i} t{} ===", dp.turn); }
        // Beat Up pairs each participant's base power with a distinct per-hit roll, so its realized
        // total depends on PS's CURRENT side.pokemon array order (active-first, swap-tracked). The
        // engine stores a fixed canonical slot order, so feed it PS's array order (the recorded
        // pre-state's `rosterIndex` sequence) for this unit.
        engine::generate::set_beatup_order(beatup_orders(&t.decisions[i - 1].state_after));
        let (chosen_draws, ambiguous) = match step_unit(&mut state, &unit, &canon, sleep_clause, &mut prng) {
            Ok(x) => x,
            Err(label) => {
                return mk_fail(Some(format!("d{i}[t{}]:{label}", dp.turn)), decisions_ok, total, aligned);
            }
        };
        if std::env::var("DRAWCMP").is_ok() {
            let rec = rec_draw_labels(&unit);
            if let Some(m) = first_draw_mismatch(&chosen_draws, &rec) {
                let mid = unit.iter().any(|d| d.mid_turn);
                let rs: Vec<String> = chosen_draws.iter().map(|d| format!("{}{:?}@{}", d.kind, d.args, d.site)).collect();
                let ps: Vec<String> = rec.iter().map(|r| r.label.clone()).collect();
                eprintln!("[DRAWCMP] {name} d{i} t{} mid={mid}: {m}", dp.turn);
                eprintln!("    rust[{}]: {}", rs.len(), rs.join(" "));
                eprintln!("    ps  [{}]: {}", ps.len(), ps.join(" "));
            }
        }
        let target = &unit.last().unwrap().state_after;
        let state_target = match convert_state(target, &canon) {
            Ok(mut s) => { s.sleep_clause = sleep_clause; s }
            Err(u) => return mk_fail(Some(format!("d{i}:convert-target:{}", u.0)), decisions_ok, total, aligned),
        };
        let diffs = diff_states(&state, &state_target);
        if !diffs.is_empty() {
            if std::env::var("DBG_DIFF").is_ok() && dbg_on {
                for dd in &diffs {
                    eprintln!("  DIFF {}: {}", dd.category, dd.detail);
                }
            }
            // Attribute the divergence to its draw-class: the first point where the engine's
            // chosen-outcome draw stream diverges from PS's recorded draws for this unit (the
            // input matched by construction — every prior decision was state-exact).
            let rec = rec_draw_labels(&unit);
            let draw_label = first_draw_mismatch(&chosen_draws, &rec)
                .unwrap_or_else(|| if ambiguous {
                    "move-order-tie (ambiguous shuffle fork)".to_string()
                } else {
                    "draws-match/state-diff".to_string()
                });
            let d0 = &diffs[0];
            return mk_fail(Some(format!("d{i}[t{}]:{} | {}", dp.turn, draw_label, d0.category)),
                decisions_ok, total, aligned);
        }
        decisions_ok += 1;
        i = j;
    }

    GameResult { name, exact: true, decisions_ok, total_decisions: total, first_divergence: None, aligned }
}

/// Advance `state` in place through one move-unit using Replicate, applying trailing
/// replacement switches from the recorded choices. Errors return a short label.
fn step_unit(
    state: &mut State,
    unit: &[&Decision],
    canon: &Canonical,
    _sleep_clause: bool,
    prng: &mut PsPrng,
) -> Result<(Vec<DrawEvent>, bool), String> {
    let dp = unit[0];
    let mut mc = [MoveChoice::Move(0); 2];
    let mut tera = [false; 2];
    for (si, side_key) in ["p1", "p2"].iter().enumerate() {
        let Some(choice) = dp.choices.get(*side_key) else {
            return Err(format!("no-choice-{side_key}"));
        };
        match resolve_choice(state, si, choice, canon) {
            Ok((m, tr)) => { mc[si] = m; tera[si] = tr; }
            Err(u) => return Err(format!("resolve:{}", u.0)),
        }
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
                let Some(details) = &choice.resolved.details else {
                    return Err("switch-no-details".into());
                };
                match canon.slot(si, &species_id_of_details(details)) {
                    Ok(s) => s,
                    Err(u) => return Err(format!("switch-slot:{}", u.0)),
                }
            };
            if active_fainted(pending_state, si, sw, side_key) {
                replacements.push((si, slot));
            } else {
                pivots[si] = Some(slot);
            }
        }
    }

    // Move-order Speed tie: PS breaks it with a `commitChoices` shuffle[2,0,2] — the FIRST draw
    // of a both-move-tie unit (custap makes no draw). Peek that bit (random(0,2)) from the real
    // stream and force the realized order so Replicate follows a single unambiguous path; the
    // generation still emits+consumes the shuffle draw, keeping the stream aligned.
    let tie = engine::generate::move_order_tie(state, mc[0], mc[1]);
    if tie {
        let mut peek = *prng;
        // A both-move Speed tie's turn-start bracket is FOUR shuffles (all consuming one
        // `random(0,2)` each): [commit `queue.sort()` (b0), eachEvent('BeforeTurn') (b1),
        // runAction Update (b2), gen8 dynamic re-sort of the [move,move,residual] queue (b3)].
        // PS's executed order is set by composing the two queue sorts (b1/b2 are eachEvent actives
        // shuffles that don't touch the move queue): commitChoices `shuffle[2,0,2]` swaps the
        // committed [One,Two] pair iff b0==1, then the dynamic `shuffle[3,0,2]` re-shuffles the two
        // moves (its [0,2) tie-group) iff b3==1. speedSort composes to: side One executes first iff
        // b0 == b3. (Peeking only b0 — the pre-dynamic-resort order — mis-selects whenever b0 != b3;
        // that was the residual both-move-tie divergence.)
        // Each side that terastallizes queues a `terastallize` action (order 106) that runs before
        // the moves, adding one extra runAction Update shuffle between the beforeTurn Update and the
        // dynamic re-sort — so the dynamic bit sits `k` positions later (k = number of tera'ing
        // sides). The commit shuffle (b0) shifts its RANGE with the queue length but still consumes
        // exactly one `random` draw, so composing b0 vs the dynamic bit is unchanged.
        let k = tera[0] as u32 + tera[1] as u32;
        let b0 = peek.random_range(0, 2);
        let _b1 = peek.random_range(0, 2); // eachEvent('BeforeTurn')
        let _b2 = peek.random_range(0, 2); // runAction Update after beforeTurn
        for _ in 0..k {
            let _ = peek.random_range(0, 2); // runAction Update after each tera action
        }
        let b3 = peek.random_range(0, 2); // gen8 dynamic re-sort
        engine::generate::set_forced_tie_order(Some(b0 == b3));
    }
    // Install the realized multi-hit source: `*prng` is the PRNG state at this decision's start
    // (replicate_select consumes it only after generation). A variable multi-hit move realizes its
    // single branch by positioning a clone of this state past the branch's draws-so-far, then
    // drawing count + per-hit rolls — the DP path emits no per-hit stream and would desync.
    engine::generate::set_realized_source(Some(engine::generate::RealizedSource::Prng(*prng)));
    let outcomes = generate_instructions_annotated(state, mc[0], mc[1], pivots, tera);
    engine::generate::set_realized_source(None);
    engine::generate::set_forced_tie_order(None);
    if outcomes.is_empty() {
        return Err("no-branches".into());
    }
    let (choice, ambiguous) = replicate_select(&outcomes, prng);
    if DBG_UNIT.with(|c| c.get()) {
        eprintln!("--- unit: {} outcomes, chosen={} ambiguous={} ---", outcomes.len(), choice, ambiguous);
        let c = &outcomes[choice];
        let cs: Vec<String> = c.draws.iter().map(|d| format!("{}{:?}={}@{}", d.kind, d.args, d.result, d.site)).collect();
        eprintln!("  CHOSEN [{choice}] draws=[{}]", cs.join(" "));
    }
    let chosen_draws = outcomes[choice].draws.clone();
    state.apply_instructions(&outcomes[choice].instructions);

    let pre_end_turn = !replacements.is_empty() && unit.last().is_some_and(|d| d.turn == dp.turn);
    // A replacement whose incoming mon has Trace fires a `sample(1)@trace` draw on switch-in that
    // `switch_into` (state only) skips — detect it BEFORE applying the swap (afterwards the ability
    // is already copied). PS consumes it during the replacement's runSwitch (c3c2s82/s83).
    //
    // Trace is an `onUpdate` handler (abilities.ts), so it fires at the first `eachEvent('Update')`
    // where a valid foe EXISTS — which for a simultaneous both-sides replacement is AFTER the other
    // side's replacement has entered. Evaluating the foe on the PRE-swap board sees the foe slot
    // still fainted and wrongly skips the draw (c3c2s82 d31: Gardevoir and Phione replace two
    // simultaneous faints; PS samples Phione for Trace). So resolve each side's foe against the
    // POST-swap board: the foe's replacement mon if that side is also replacing, else its current
    // active. `switch_into_pair` already models the same "both enter, then abilities fire" order
    // for STATE; this brings the draw accounting in line.
    let trace_draws = replacements.iter()
        .filter(|&&(side, slot)| {
            let foe_replacement = replacements.iter().find(|&&(s2, _)| s2 != side).map(|&(_, sl)| sl);
            engine::generate::trace_replacement_sample(state, side_id(side), slot, foe_replacement)
        })
        .count();
    let mut replaced = [false; 2];
    if replacements.len() == 2 && replacements[0].0 != replacements[1].0 {
        engine::generate::switch_into_pair(state, [
            (side_id(replacements[0].0), replacements[0].1),
            (side_id(replacements[1].0), replacements[1].1),
        ]);
        replaced = [true, true];
    } else {
        for &(side, slot) in &replacements {
            engine::generate::switch_into(state, side_id(side), slot);
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
    // Post-turn (not same-turn) forced-replacement switches: `switch_into`/`switch_into_pair` apply
    // state only, but PS resolves each replacement as a `switch` action whose runAction fires a
    // 3-shuffle bracket — switch-action runAction Update (battle.ts:2882), `runSwitch` getAllActive
    // speedSort (battle-actions.ts:182), runSwitch runAction Update (2882) — each a `getAllActive()`
    // speed-tie shuffle on the POST-swap board. A bracket fires (3 draws) at the transition to a
    // both-actives-alive-and-Speed-tied board: for a single replacement, or for the SECOND of a
    // simultaneous both-sides replacement (the first runs while the other slot is still fainted, so
    // getAllActive has one active → no shuffle). Ground-truthed on c5a1 t11 (Primarina replaces a
    // fainted Alcremie vs a Speed-tied Grimmsnarl → exactly 3 shuffles; PS seed 46844→21739). The
    // engine's annotated switch bracket (generate.rs) covers only VOLUNTARY move+switch pivots; this
    // consumes the forced-replacement bracket the gate would otherwise skip (a state-neutral drift
    // that only surfaces at the next Speed-sensitive roll). Off a tie: 0 draws → no effect.
    if !pre_end_turn && !replacements.is_empty() && engine::generate::replacement_bracket_tied(state, replaced) {
        let brackets = if replacements.len() == 2 && replacements[0].0 != replacements[1].0 {
            1 // simultaneous both-sides double faint: only the second switch sees both actives alive
        } else {
            replacements.len()
        };
        for _ in 0..brackets {
            for _ in 0..3 {
                let _ = consume(prng, "shuffle", &[2, 0, 2]);
            }
        }
    }
    // Trace's switch-in `sample(1)` for each tracing replacement (after the switch bracket, in the
    // replacement's runSwitch onUpdate). Consumed here because `switch_into` applies the copied
    // ability to state but not the draw.
    for _ in 0..trace_draws {
        let _ = consume(prng, "sample", &[1]);
    }
    Ok((chosen_draws, ambiguous))
}

/// Recorded PS draws of a unit, reduced to (kind, args, semantic label) for draw-class triage.
struct RecLabel { kind: String, args: Vec<i64>, label: String }

fn rec_draw_labels(unit: &[&Decision]) -> Vec<RecLabel> {
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
            let ctx = if !effect.is_empty() { effect } else if !event.is_empty() { event }
                else if !mv.is_empty() { mv } else { "generic" };
            let label = format!("{kind}{args:?}@{ctx}");
            out.push(RecLabel { kind, args, label });
        }
    }
    out
}

/// First position where the engine's chosen draw stream diverges from PS's recorded draws.
/// Returns a compact label describing what PS did that the engine didn't (or vice versa).
fn first_draw_mismatch(rust: &[DrawEvent], rec: &[RecLabel]) -> Option<String> {
    let n = rust.len().max(rec.len());
    for i in 0..n {
        match (rust.get(i), rec.get(i)) {
            (Some(r), Some(p)) => {
                if r.kind != p.kind {
                    return Some(format!("PS {} (rust {}{:?}@{})", p.label, r.kind, r.args, r.site));
                }
                let ra: Vec<i64> = r.args.iter().map(|&x| x as i64).collect();
                if ra != p.args {
                    return Some(format!("args {} (rust {:?})", p.label, ra));
                }
            }
            (Some(r), None) => return Some(format!("rust-extra {}{:?}@{}", r.kind, r.args, r.site)),
            (None, Some(p)) => return Some(format!("PS-unconsumed {}", p.label)),
            (None, None) => unreachable!(),
        }
    }
    None
}

/// Diagnostic: does the recorded full strong-draw stream reproduce with the modeled init offset?
/// (Independent of the engine — pure PsPrng vs recorded results; localizes init misalignment.)
fn alignment_ok(t: &Trace, limbs: [u16; 4]) -> bool {
    let mut prng = PsPrng::from_limbs(limbs);
    for _ in 0..init_gender_rolls(t) { let _ = prng.next(); }
    for d in &t.decisions {
        for dr in &d.draws {
            let kind = dr.get("kind").and_then(Value::as_str).unwrap_or("");
            let args: Vec<i32> = dr.get("args").and_then(Value::as_array).map(|a| {
                a.iter().filter_map(Value::as_i64).map(|x| x as i32).collect()
            }).unwrap_or_default();
            let res = consume(&mut prng, kind, &args);
            if kind != "shuffle" {
                if let Some(want) = dr.get("result") {
                    let want_i = match want {
                        Value::Bool(b) => *b as i64,
                        Value::Number(n) => n.as_i64().unwrap_or(res),
                        _ => res,
                    };
                    if want_i != res { return false; }
                }
            }
        }
    }
    true
}

/// Reduce a first-draw-mismatch label to a stable category (drop numeric args / mon names).
fn normalize_draw_cat(s: &str) -> String {
    // Keep the leading token pattern + the @context; strip bracketed args.
    let no_args = {
        let mut out = String::new();
        let mut depth = 0i32;
        for ch in s.chars() {
            match ch {
                '[' => depth += 1,
                ']' => { if depth > 0 { depth -= 1; } }
                _ if depth == 0 => out.push(ch),
                _ => {}
            }
        }
        out
    };
    // Take up to the first '(' (drops the parenthetical rust-side detail).
    no_args.split('(').next().unwrap_or(&no_args).trim().to_string()
}

pub fn run_seed_gate(args: &[String]) -> ExitCode {
    let verbose = std::env::var("VERBOSE").is_ok();
    let mut results = Vec::new();
    let mut ps_commit: Option<String> = None;
    for path in args {
        let t = match crate::trace::load_trace(path) {
            Ok(t) => t,
            Err(e) => { eprintln!("{e}"); continue; }
        };
        match &ps_commit {
            None => ps_commit = Some(t.ps_commit.clone()),
            Some(c) if *c != t.ps_commit => {
                eprintln!("{path}: PS commit differs — refusing mixed corpus");
                return ExitCode::FAILURE;
            }
            _ => {}
        }
        results.push(run_game(path, &t));
    }

    let games = results.len();
    let exact = results.iter().filter(|r| r.exact).count();
    let aligned = results.iter().filter(|r| r.aligned).count();
    let exact_aligned = results.iter().filter(|r| r.exact && r.aligned).count();

    println!("\n========== SEED-DRIVEN FULL-BATTLE GATE ==========");
    if let Some(c) = &ps_commit { println!("(ps {c})"); }
    println!("games: {games}");
    println!("FULL-GAME EXACT: {exact} / {games} = {:.1}%", 100.0 * exact as f64 / games.max(1) as f64);
    println!("init-aligned (from-seed PRNG stream reproduces): {aligned} / {games}");
    println!("full-game exact among init-aligned: {exact_aligned} / {aligned}");

    let mut cat_counts: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
    let mut rows: Vec<&GameResult> = results.iter().filter(|r| !r.exact).collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    for r in &rows {
        let fd = r.first_divergence.as_deref().unwrap_or("?");
        // Category = the draw-class label (between the first ':' and the ' | '), normalized to
        // its @context so games sharing a draw-order cause aggregate.
        let after = fd.split_once(':').map(|(_, c)| c).unwrap_or(fd);
        let draw_part = after.split(" | ").next().unwrap_or(after).trim();
        let cat = normalize_draw_cat(draw_part);
        *cat_counts.entry(cat).or_default() += 1;
    }
    println!("\nper-game first divergence (non-exact games):");
    let show = if verbose { rows.len() } else { 45.min(rows.len()) };
    for r in rows.iter().take(show) {
        let fd = r.first_divergence.as_deref().unwrap_or("?");
        println!("  {:22} ok {}/{} align={}  {}", r.name, r.decisions_ok, r.total_decisions, r.aligned, fd);
    }
    println!("\nfirst-divergence category counts (ranked):");
    let mut cats: Vec<_> = cat_counts.iter().collect();
    cats.sort_by(|a, b| b.1.cmp(a.1));
    for (c, n) in cats { println!("  {n:4}  {c}"); }

    if verbose {
        println!("\nexact games:");
        for r in results.iter().filter(|r| r.exact) {
            println!("  {:22} {} decisions", r.name, r.decisions_ok);
        }
    }
    ExitCode::SUCCESS
}
