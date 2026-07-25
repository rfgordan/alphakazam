//! The v2 co-simulation trace format (see harness/cosim.mjs).
//!
//! States are kept as raw `serde_json::Value` — they are PS's *complete* `serializeBattle`
//! output, and the converter walks them field-by-field against an explicit manifest rather
//! than deserializing into a hand-picked struct (hand-picking is how the old flow silently
//! dropped state).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
pub struct Trace {
    pub version: u32,
    #[serde(rename = "psCommit")]
    pub ps_commit: String,
    pub format: String,
    /// The battle seed the recorder built the game with (`[hi,..,lo]` u16 limbs). Present in v2
    /// traces; drives the seed-replay gate (seed a `PsPrng`, drive Replicate, byte-compare).
    #[serde(default)]
    pub seed: Option<[u16; 4]>,
    pub teamset: Option<String>,
    /// The PS packed team strings the recorder handed `new Battle` (p1, p2). Present in traces
    /// recorded after the seed-fixture work; empty for older ones.
    #[serde(rename = "packedTeams", default)]
    pub packed_teams: Vec<String>,
    pub decisions: Vec<Decision>,
    pub result: TraceResult,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct TraceResult {
    pub winner: Option<String>,
    pub ended: bool,
    pub turns: u32,
}

#[derive(Deserialize)]
pub struct Decision {
    pub index: u32,
    pub turn: u32,
    #[serde(rename = "requestState")]
    pub request_state: String,
    #[serde(rename = "midTurn", default)]
    pub mid_turn: bool,
    /// side id ("p1"/"p2") -> the request JSON PS sent (legality ground truth).
    #[serde(default)]
    pub requests: BTreeMap<String, Value>,
    /// side id -> the choice made.
    #[serde(default)]
    pub choices: BTreeMap<String, ChoiceRec>,
    /// PRNG draws consumed resolving this decision (the outcome alphabet).
    #[serde(default)]
    pub draws: Vec<Value>,
    /// Full serialized battle state after the battle advanced.
    #[serde(rename = "stateAfter")]
    pub state_after: Value,
    /// Optional exact PS transition distribution produced by `cosim.mjs --distributions`.
    /// Probabilities are in [0,1]; outcomes with byte-identical normalized PS snapshots are
    /// already coalesced by the recorder.
    #[serde(default)]
    pub distribution: Option<DecisionDistribution>,
}

#[derive(Deserialize)]
pub struct DecisionDistribution {
    pub paths: u64,
    pub outcomes: Vec<DistributionOutcome>,
    #[serde(default)]
    pub kernels: Vec<ActionKernel>,
}

#[derive(Deserialize)]
pub struct ActionKernel {
    pub action: KernelAction,
    pub input: Value,
    pub outcomes: Vec<DistributionOutcome>,
}

#[derive(Deserialize)]
pub struct KernelAction {
    pub choice: String,
    pub side: Option<String>,
    #[serde(rename = "moveId")]
    pub move_id: Option<String>,
    #[serde(rename = "foePendingMoveId")]
    pub foe_pending_move_id: Option<String>,
}

#[derive(Deserialize)]
pub struct DistributionOutcome {
    pub probability: f64,
    #[serde(rename = "requestState", default)]
    pub request_state: String,
    #[serde(rename = "midTurn", default)]
    pub mid_turn: bool,
    pub state: Value,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct ChoiceRec {
    /// The literal PS choice string ("move 2 terastallize", "switch 4", ...).
    pub choice: String,
    /// The unambiguous form recorded at choice time (ids, not positions).
    pub resolved: Resolved,
}

#[derive(Clone, Deserialize, Serialize)]
pub struct Resolved {
    pub action: String, // "move" | "switch" | "teampreview" | "pass" | "default"
    #[serde(rename = "moveId")]
    pub move_id: Option<String>,
    #[serde(default)]
    pub tera: bool,
    /// For switches: the PS ident ("p2: Slowking") and full details ("Slowking-Galar, M").
    pub ident: Option<String>,
    pub details: Option<String>,
    /// Stable battle-start roster slot (forme-proof identity); preferred over `details`.
    #[serde(rename = "rosterIndex")]
    pub roster_index: Option<u8>,
}

/// Read a possibly-gzipped UTF-8 file.
pub fn read_maybe_gz(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    if path.ends_with(".gz") {
        use std::io::Read;
        let mut s = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut s)
            .map_err(|e| format!("gunzip {path}: {e}"))?;
        Ok(s)
    } else {
        String::from_utf8(bytes).map_err(|e| format!("utf8 {path}: {e}"))
    }
}

pub fn load_trace(path: &str) -> Result<Trace, String> {
    let text = read_maybe_gz(path)?;
    let t: Trace = serde_json::from_str(&text).map_err(|e| format!("parse {path}: {e}"))?;
    if t.version != 2 {
        return Err(format!("{path}: unsupported trace version {}", t.version));
    }
    Ok(t)
}

/// Is PS's **Sleep Clause Mod** active for a trace recorded under `format`?
///
/// It never is. `harness/cosim.mjs` constructs every battle with
/// `formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT` — random-battle TEAMS are
/// pre-generated and the battle itself runs as a custom game so PS does not re-roll teams from the
/// battle seed. `gen9customgame` carries no ruleset, and Sleep Clause Mod lives in the `standard`
/// ruleset (data/rulesets.ts:1378, pulled in by gen9ou and friends, and listed explicitly on
/// "[Gen 9] Random Battle" — a format the harness never instantiates). The default `FORMAT` is
/// `gen9customgame`, so the other arm never fires either.
///
/// The old inference (`format.contains("randombattle")`) had it exactly backwards and made the
/// engine refuse a second foe-inflicted sleep that the pinned PS happily applies — rb1312 t13 has
/// Regice slept while the benched Iron Jugulis is still asleep from the same attacker, and PS rolls
/// the `random(2,5)` duration for it.
pub fn sleep_clause_for_format(format: &str) -> bool {
    let effective = if format.contains("random") { "gen9customgame" } else { format };
    effective != "gen9customgame"
}
