//! The v2 co-simulation trace format (see harness/cosim.mjs).
//!
//! States are kept as raw `serde_json::Value` — they are PS's *complete* `serializeBattle`
//! output, and the converter walks them field-by-field against an explicit manifest rather
//! than deserializing into a hand-picked struct (hand-picking is how the old flow silently
//! dropped state).

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
pub struct Trace {
    pub version: u32,
    #[serde(rename = "psCommit")]
    pub ps_commit: String,
    pub format: String,
    pub teamset: Option<String>,
    pub decisions: Vec<Decision>,
    pub result: TraceResult,
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
pub struct ChoiceRec {
    /// The literal PS choice string ("move 2 terastallize", "switch 4", ...).
    pub choice: String,
    /// The unambiguous form recorded at choice time (ids, not positions).
    pub resolved: Resolved,
}

#[derive(Deserialize)]
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

pub fn load_trace(path: &str) -> Result<Trace, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
    let text = if path.ends_with(".gz") {
        use std::io::Read;
        let mut s = String::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .read_to_string(&mut s)
            .map_err(|e| format!("gunzip {path}: {e}"))?;
        s
    } else {
        String::from_utf8(bytes).map_err(|e| format!("utf8 {path}: {e}"))?
    };
    let t: Trace = serde_json::from_str(&text).map_err(|e| format!("parse {path}: {e}"))?;
    if t.version != 2 {
        return Err(format!("{path}: unsupported trace version {}", t.version));
    }
    Ok(t)
}
