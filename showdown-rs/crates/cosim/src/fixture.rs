//! Slim seed-gate fixtures (`*.fx.json.gz`) — the extension-corpus format.
//!
//! A full v2 cosim trace carries PS's complete `serializeBattle` after EVERY decision. That is
//! the right format for the *mechanics* sweep (which diffs states field-by-field and needs the
//! PS side verbatim), but the seed gate only ever asks "did the engine's own carried-forward
//! state stay equal to PS's?", so it can run off a 128-bit digest per decision instead
//! (`digest.rs` defines the canonical encoding and why digest-equality ⟺ `diff_states`-empty).
//!
//! A fixture therefore keeps:
//!   - the battle seed, format, teamset and the PACKED TEAMS (resolved + set genders included),
//!     so the sidecar can be re-recorded from scratch and re-verified;
//!   - the FULL PS state of the first (teampreview) decision only — the gate's initial state and
//!     the init-verification anchor;
//!   - per decision: the choices (with `rosterIndex`), the recorded PRNG draws (1% of the bytes,
//!     and they carry the whole init-alignment check plus the first-divergence draw labels), the
//!     side `rosterOrder` (PS's live `side.pokemon` order, which Beat Up reads), a precomputed
//!     `activeFainted` pair (replacement-vs-pivot classification, from PS's request JSON), and
//!     the canonical state DIGEST.
//!
//! What it drops: every non-first serialized state and every request JSON — ~97% of the bytes.
//!
//! On a digest mismatch the gate can say WHICH decision and WHICH draw first diverged, but not
//! which state field. The full state lives in the **sidecar**: the original trace, gzipped, in
//! `harness/seed-sidecars/` (kept OUT of the default gate path and out of git — regenerable
//! byte-identically from the seed). Point the gate at the sidecar to get the field-level diff.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::convert::{convert_state, Canonical};
use crate::digest::{hex, state_digest};
use crate::trace::{ChoiceRec, Trace, TraceResult};

pub const FIXTURE_SUFFIX: &str = ".fx.json";

#[derive(Serialize, Deserialize)]
pub struct Fixture {
    pub version: u32,
    pub kind: String,
    #[serde(rename = "psCommit")]
    pub ps_commit: String,
    pub format: String,
    pub teamset: Option<String>,
    pub seed: Option<[u16; 4]>,
    /// PS packed team strings, p1 then p2 — every set's species/level/ability/item/moves/EVs/IVs
    /// AND its declared gender. Re-recording the sidecar from these + the seed reproduces the
    /// battle exactly (they are what the recorder handed `new Battle`).
    #[serde(rename = "packedTeams", default)]
    pub packed_teams: Vec<String>,
    /// Per-side, per-roster-slot RESOLVED battle gender ("M"/"F"/"N") — what `new Pokemon`
    /// settled on, after the set gender / species gender / `sample(['M','F'])` cascade.
    #[serde(default)]
    pub genders: Vec<Vec<String>>,
    /// Per-side, per-roster-slot SET gender (may be "") — the field that decides whether the mon
    /// burned a construction-time `sample` draw (see `seedgate::init_gender_rolls`).
    #[serde(rename = "setGenders", default)]
    pub set_genders: Vec<Vec<String>>,
    /// PS's full `serializeBattle` after the FIRST (teampreview) decision. The gate's initial
    /// state; also the init-verification anchor (`initDigest`).
    #[serde(rename = "initState")]
    pub init_state: Value,
    #[serde(rename = "initDigest")]
    pub init_digest: String,
    pub result: TraceResult,
    pub decisions: Vec<FxDecision>,
}

#[derive(Serialize, Deserialize)]
pub struct FxDecision {
    pub turn: u32,
    #[serde(rename = "requestState")]
    pub request_state: String,
    #[serde(rename = "midTurn", default)]
    pub mid_turn: bool,
    #[serde(default)]
    pub choices: BTreeMap<String, ChoiceRec>,
    #[serde(default)]
    pub draws: Vec<Value>,
    /// PS's live `side.pokemon` order as roster indices, per side, at THIS decision's state.
    /// The gate feeds decision *i-1*'s order to Beat Up when stepping decision *i*.
    #[serde(rename = "rosterOrder", default)]
    pub roster_order: [Option<Vec<u8>>; 2],
    /// Was each side's active fainted at this (switch) decision? Precomputed from PS's request
    /// JSON so the fixture need not ship requests. Meaningless on `move` decisions.
    #[serde(rename = "activeFainted", default)]
    pub active_fainted: [bool; 2],
    /// Canonical digest of `convert(stateAfter)`; absent iff the state did not convert.
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(rename = "digestErr", default)]
    pub digest_err: Option<String>,
}

pub fn is_fixture_path(path: &str) -> bool {
    path.contains(FIXTURE_SUFFIX)
}

pub fn load_fixture(path: &str) -> Result<Fixture, String> {
    let text = crate::trace::read_maybe_gz(path)?;
    let f: Fixture = serde_json::from_str(&text).map_err(|e| format!("parse {path}: {e}"))?;
    if f.version != 1 || f.kind != "seed-fixture" {
        return Err(format!("{path}: unsupported fixture {} / {}", f.version, f.kind));
    }
    Ok(f)
}

/// Per-side roster-index order of PS's live `side.pokemon` array in a serialized state.
fn roster_order(state: &Value) -> [Option<Vec<u8>>; 2] {
    let mut out: [Option<Vec<u8>>; 2] = [None, None];
    let Some(sides) = state.get("sides").and_then(Value::as_array) else { return out };
    for (si, side) in sides.iter().enumerate().take(2) {
        if let Some(mons) = side.get("pokemon").and_then(Value::as_array) {
            let order: Vec<u8> = mons
                .iter()
                .filter_map(|p| p.get("rosterIndex").and_then(Value::as_i64).map(|r| r as u8))
                .collect();
            if !order.is_empty() {
                out[si] = Some(order);
            }
        }
    }
    out
}

fn gender_columns(state: &Value, key: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    for side in state.get("sides").and_then(Value::as_array).into_iter().flatten() {
        let mut col: Vec<(u8, String)> = Vec::new();
        for mon in side.get("pokemon").and_then(Value::as_array).into_iter().flatten() {
            let ri = mon.get("rosterIndex").and_then(Value::as_i64).unwrap_or(-1);
            let g = mon.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            if ri >= 0 {
                col.push((ri as u8, g));
            }
        }
        col.sort_by_key(|(ri, _)| *ri);
        out.push(col.into_iter().map(|(_, g)| g).collect());
    }
    out
}

/// Build the slim fixture for a recorded trace. Digests are computed HERE (in Rust, through the
/// same `convert_state` the gate uses), so a fixture is only ever as good as the converter that
/// made it — regenerate fixtures whenever `convert.rs` changes.
pub fn build_fixture(t: &Trace) -> Result<Fixture, String> {
    let first = t.decisions.first().ok_or_else(|| "empty-trace".to_string())?;
    let canon = Canonical::from_first_state(&first.state_after).map_err(|u| format!("canon:{}", u.0))?;

    let mut decisions = Vec::with_capacity(t.decisions.len());
    for d in &t.decisions {
        let (digest, digest_err) = match convert_state(&d.state_after, &canon) {
            Ok(s) => (Some(hex(state_digest(&s))), None),
            Err(u) => (None, Some(u.0)),
        };
        let active_fainted = [
            crate::replay::active_fainted(&d.state_after, 0, d, "p1"),
            crate::replay::active_fainted(&d.state_after, 1, d, "p2"),
        ];
        decisions.push(FxDecision {
            turn: d.turn,
            request_state: d.request_state.clone(),
            mid_turn: d.mid_turn,
            choices: d.choices.clone(),
            draws: d.draws.clone(),
            roster_order: roster_order(&d.state_after),
            active_fainted,
            digest,
            digest_err,
        });
    }

    let init_digest = decisions[0].digest.clone().unwrap_or_default();
    Ok(Fixture {
        version: 1,
        kind: "seed-fixture".into(),
        ps_commit: t.ps_commit.clone(),
        format: t.format.clone(),
        teamset: t.teamset.clone(),
        seed: t.seed,
        packed_teams: t.packed_teams.clone(),
        genders: gender_columns(&first.state_after, "gender"),
        set_genders: gender_columns(&first.state_after, "setGender"),
        init_state: first.state_after.clone(),
        init_digest,
        result: TraceResult { winner: t.result.winner.clone(), ended: t.result.ended, turns: t.result.turns },
        decisions,
    })
}

/// `MAKE_FIXTURE=<outdir> cosim <traces...>` — write one `<name>.fx.json.gz` per trace.
pub fn run_make_fixture(args: &[String], outdir: &str) -> std::process::ExitCode {
    use rayon::prelude::*;
    use std::io::Write;

    if let Err(e) = std::fs::create_dir_all(outdir) {
        eprintln!("mkdir {outdir}: {e}");
        return std::process::ExitCode::FAILURE;
    }
    let rows: Vec<Result<(String, usize, usize), String>> = args
        .par_iter()
        .map(|path| {
            let t = crate::trace::load_trace(path)?;
            let f = build_fixture(&t).map_err(|e| format!("{path}: {e}"))?;
            let name = path.rsplit('/').next().unwrap_or(path);
            let stem = name.trim_end_matches(".gz").trim_end_matches(".json");
            let out = format!("{outdir}/{stem}{FIXTURE_SUFFIX}.gz");
            let body = serde_json::to_vec(&f).map_err(|e| format!("{path}: encode {e}"))?;
            let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
            enc.write_all(&body).map_err(|e| format!("{path}: gz {e}"))?;
            let gz = enc.finish().map_err(|e| format!("{path}: gz {e}"))?;
            let n = gz.len();
            std::fs::write(&out, gz).map_err(|e| format!("{out}: {e}"))?;
            Ok((out, body.len(), n))
        })
        .collect();

    let (mut raw, mut gz, mut ok, mut bad) = (0usize, 0usize, 0usize, 0usize);
    for r in &rows {
        match r {
            Ok((_, b, g)) => { raw += b; gz += g; ok += 1; }
            Err(e) => { eprintln!("{e}"); bad += 1; }
        }
    }
    println!("fixtures: {ok} written, {bad} failed -> {outdir}");
    println!("bytes: {raw} raw, {gz} gz  (mean {} gz/fixture)", gz / ok.max(1));
    if bad > 0 { std::process::ExitCode::FAILURE } else { std::process::ExitCode::SUCCESS }
}
