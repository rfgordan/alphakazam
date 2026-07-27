//! The v2 co-simulation trace format (see harness/cosim.mjs).
//!
//! States are kept as raw `serde_json::Value` — they are PS's *complete* `serializeBattle`
//! output, and the converter walks them field-by-field against an explicit manifest rather
//! than deserializing into a hand-picked struct (hand-picking is how the old flow silently
//! dropped state).

use std::collections::BTreeMap;

use engine::ruleset::Ruleset;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
pub struct Trace {
    pub version: u32,
    #[serde(rename = "psCommit")]
    pub ps_commit: String,
    /// The `--format` the RECORDER was invoked with. Not the rules the battle was played under —
    /// see [`ruleset_for`].
    pub format: String,
    /// The formatid actually handed to `new Battle`. Absent on every legacy recording (which were
    /// all played as `gen9customgame`, whatever `format` says).
    #[serde(default)]
    pub ruleset: Option<String>,
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

/// The [`Ruleset`] a recording was actually PLAYED under.
///
/// **This does not key off `trace.format`, and the reason is a landmine.** `trace.format` is the
/// `--format` the recorder was *invoked* with, which for the whole seed corpus is
/// `gen9randombattle` — but until the ruleset work, `harness/cosim.mjs` then constructed the
/// battle with `formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT`. Random-battle
/// TEAMS were pre-generated and the BATTLE ran as a custom game, so PS would not re-roll teams
/// from the battle seed. So all 912 committed games say `gen9randombattle` and were played with
/// **no** Sleep Clause Mod, `Math.trunc`, exact HP and a team-preview first decision.
///
/// Recordings made after that rewrite was deleted carry an explicit `ruleset` field naming the
/// formatid handed to `new Battle`. That field — and nothing else — decides:
///
/// * present   → [`Ruleset::from_format`], erroring loudly on an unknown id;
/// * absent    → `gen9customgame`, which is what every legacy recording was played under
///   regardless of what its `format` field claims.
///
/// The old inference (`format.contains("randombattle")`) had it exactly backwards and made the
/// engine refuse a second foe-inflicted sleep that the pinned PS happily applies — rb1312 t13 has
/// Regice slept while the benched Iron Jugulis is still asleep from the same attacker, and PS rolls
/// the `random(2,5)` duration for it.
pub fn ruleset_for(explicit: Option<&str>, _format: &str) -> Result<Ruleset, String> {
    match explicit {
        None => Ok(Ruleset::GEN9_CUSTOM_GAME),
        Some(id) => Ruleset::from_format(id)
            .ok_or_else(|| format!("unknown ruleset formatid {id:?} — add a preset to engine::ruleset")),
    }
}

/// The ENTRY CONTRACT shared by `replay.rs`, `seedgate.rs`, `drawdiff.rs` and
/// `protocol_emit.rs`: what `requestState` decision 0 of a recording must carry.
///
/// Decision 0 is never a battle transition — it is the setup that ends at the first `move`
/// request, recorded so its PRNG draws can be consumed for stream shape and its `stateAfter`
/// used as the gate's initial board. Under Team Preview that setup IS a decision (the team
/// order choice, whose resolution runs the `'start'` queue action). Without Team Preview
/// (`runPickTeam` is a complete no-op — `RULESET_SPEC.md` §5) there is no decision at all, so
/// `cosim.mjs` records a SYNTHETIC decision 0 with `requestState: "start"`, no choices, and the
/// draws `battle.start()` consumed. Everything downstream is then shape-identical.
pub fn first_decision_state(rs: &Ruleset) -> &'static str {
    if rs.team_preview {
        "teampreview"
    } else {
        "start"
    }
}
