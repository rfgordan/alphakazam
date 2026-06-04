//! Serde mirror of the trace JSON (see `harness/TRACE_FORMAT.md`).
//!
//! These structs are the comparison currency: PS state deserializes into `TState`, and
//! the engine's `State` is *projected back* into the same `TState` shape, so the two can
//! be diffed as plain JSON values with matching key casing.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Trace {
    pub format: String,
    pub seed: String,
    pub start: StartWrap,
    pub snapshots: Vec<Snapshot>,
    #[serde(default)]
    pub result: ResultInfo,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct StartWrap {
    pub turn: u32,
    pub state: TState,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ResultInfo {
    #[serde(default)]
    pub winner: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Snapshot {
    pub turn: u32,
    #[serde(default)]
    pub choices: Choices,
    /// Switch-outs forced during this turn, in order: a U-turn pivot, then any faint
    /// replacements. Multiple per side are possible (pivot in, then the new mon faints).
    #[serde(default)]
    pub replacements: ReplacementList,
    #[serde(default)]
    pub outcomes: Vec<serde_json::Value>,
    pub state: TState,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ReplacementList {
    #[serde(default)]
    pub p1: Vec<String>,
    #[serde(default)]
    pub p2: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Choices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p1: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p2: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TState {
    pub turn: u32,
    pub weather: String,
    pub weather_turns: i8,
    pub terrain: String,
    pub terrain_turns: i8,
    pub trick_room: bool,
    pub trick_room_turns: i8,
    pub sides: Vec<TSide>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TSide {
    pub active_index: u8,
    pub boosts: TBoosts,
    pub volatiles: Vec<String>,
    pub substitute_hp: i16,
    pub side_conditions: TSideConditions,
    pub pokemon: Vec<TPokemon>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TBoosts {
    pub atk: i8,
    pub def: i8,
    pub spa: i8,
    pub spd: i8,
    pub spe: i8,
    pub accuracy: i8,
    pub evasion: i8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TSideConditions {
    pub stealth_rock: bool,
    pub spikes: u8,
    pub toxic_spikes: u8,
    pub sticky_web: bool,
    pub reflect: u8,
    pub light_screen: u8,
    pub aurora_veil: u8,
    pub tailwind: u8,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TPokemon {
    pub species: String,
    pub level: u8,
    pub types: Vec<String>,
    pub hp: i16,
    pub max_hp: i16,
    pub status: String,
    pub status_counter: u8,
    pub ability: String,
    pub item: String,
    pub stats: TStats,
    pub terastallized: bool,
    pub tera_type: String,
    pub moves: Vec<TMove>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TStats {
    pub atk: i16,
    pub def: i16,
    pub spa: i16,
    pub spd: i16,
    pub spe: i16,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TMove {
    pub id: String,
    pub pp: u8,
    pub max_pp: u8,
    pub disabled: bool,
}

impl TState {
    /// Sort volatile lists so set-equality isn't masked by ordering when diffing.
    pub fn normalize(&mut self) {
        for side in &mut self.sides {
            side.volatiles.sort();
        }
    }
}
