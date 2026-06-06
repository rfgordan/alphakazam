//! Python bridge to the Rust battle engine (`showdown_engine` extension module).
//!
//! Exposes a single `Battle` class that owns a `State` and lets Python push **actions** in and
//! read **observations / legal-action masks / rewards / natural-language commentary** out — the
//! clean two-way channel the RL self-play loop needs. The engine stays untouched; this is a thin
//! adapter: encode (observation), sample one weighted outcome branch (the engine returns them
//! all), apply it, and optionally render the turn via the narration layer.
//!
//! Action space (9): `0..=3` = move slots, `4..=8` = switch to the k-th benched mon (the five
//! non-active party slots, in slot order). The mask makes illegal actions un-pickable.

use engine::generate::{generate_instructions, MoveChoice};
use engine::instruction::StateInstructions;
use engine::narrate::narrate_turn;
use engine::state::{SideId, State};
use engine::team;
use pyo3::prelude::*;

const N_ACTIONS: usize = 9;
const N_MOVES: usize = 4;

/// Small deterministic PRNG (splitmix64) so battles are reproducible from a seed without pulling
/// in a dependency (the engine core is dep-free; we keep RNG on this side of the boundary).
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_unit(&mut self) -> f32 {
        // 24 random bits -> [0, 1).
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

#[pyclass]
pub struct Battle {
    state: State,
    rng: Rng,
    seed: u64,
}

#[pymethods]
impl Battle {
    /// Start the default self-play matchup (red_team vs blue_team), seeded.
    #[new]
    #[pyo3(signature = (seed = 0))]
    fn new(seed: u64) -> Self {
        Battle { state: team::default_matchup(), rng: Rng(seed.wrapping_add(1)), seed }
    }

    /// Reset to a fresh battle. A new seed reseeds outcome sampling.
    #[pyo3(signature = (seed = None))]
    fn reset(&mut self, seed: Option<u64>) {
        if let Some(s) = seed {
            self.seed = s;
        }
        self.state = team::default_matchup();
        self.rng = Rng(self.seed.wrapping_add(1));
    }

    /// Length of the observation vector (the model's input dim).
    #[getter]
    fn obs_dim(&self) -> usize {
        engine::encode::OBS_DIM
    }

    /// Number of discrete actions (4 moves + 5 switches).
    #[getter]
    fn n_actions(&self) -> usize {
        N_ACTIONS
    }

    #[getter]
    fn turn(&self) -> u32 {
        self.state.turn
    }

    /// Encoded float observation from `side`'s perspective (0 = Red, 1 = Blue).
    fn observe(&self, side: u8) -> Vec<f32> {
        engine::encode::encode(&self.state, sid(side))
    }

    /// Categorical IDs from `side`'s perspective: `n_mons * ids_per_mon` integers (row-major per
    /// mon, columns = `id_columns()`), for the model's embedding tables.
    fn observe_ids(&self, side: u8) -> Vec<i64> {
        engine::encode::encode_ids(&self.state, sid(side))
    }

    #[getter]
    fn id_dim(&self) -> usize {
        engine::encode::ID_DIM
    }

    #[getter]
    fn n_mons(&self) -> usize {
        engine::encode::N_MONS
    }

    #[getter]
    fn ids_per_mon(&self) -> usize {
        engine::encode::IDS_PER_MON
    }

    /// Which embedding table each ID column indexes into (length `ids_per_mon`).
    fn id_columns(&self) -> Vec<String> {
        ["species", "ability", "item", "type", "move", "move", "move", "move"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Vocabulary size (number of embedding rows needed) for each table named in `id_columns()`.
    fn vocab_sizes(&self) -> std::collections::HashMap<String, usize> {
        use engine::ids::{Ability, Item, Type};
        let mut m = std::collections::HashMap::new();
        m.insert("species".to_string(), engine::gen::SPECIES_NAMES.len());
        m.insert("move".to_string(), engine::gen::MOVE_NAMES.len());
        m.insert("item".to_string(), Item::Unknown as usize + 1);
        m.insert("ability".to_string(), Ability::Unknown as usize + 1);
        m.insert("type".to_string(), Type::Stellar as usize + 1);
        m
    }

    /// Boolean legal-action mask of length 9 for `side`.
    fn legal_actions(&self, side: u8) -> Vec<bool> {
        let s = self.state.side(sid(side));
        let active = s.active();
        let mut mask = vec![false; N_ACTIONS];
        if active.is_alive() {
            for i in 0..N_MOVES {
                let m = active.moves[i];
                mask[i] = m.id != engine::ids::MoveId::None && m.pp > 0;
            }
        }
        for (k, slot) in self.bench(sid(side)).into_iter().enumerate() {
            if let Some(slot) = slot {
                let p = &s.pokemon[slot as usize];
                mask[N_MOVES + k] = p.is_alive();
            }
        }
        mask
    }

    /// True once either side has no Pokémon left.
    fn is_over(&self) -> bool {
        self.side_lost(SideId::One) || self.side_lost(SideId::Two)
    }

    /// Winner code: -1 ongoing or draw, 0 = Red won, 1 = Blue won.
    fn winner(&self) -> i64 {
        let (l1, l2) = (self.side_lost(SideId::One), self.side_lost(SideId::Two));
        match (l1, l2) {
            (false, true) => 0,
            (true, false) => 1,
            _ => -1,
        }
    }

    /// Advance one turn given both sides' action indices. Illegal actions are coerced to the
    /// first legal one (defensive; callers should pass masked samples).
    ///
    /// Returns `(done, winner, lines)`:
    ///   done   — battle finished this turn
    ///   winner — -1 ongoing/draw, 0 Red, 1 Blue
    ///   lines  — natural-language commentary for the turn (empty unless `narrate=True`)
    #[pyo3(signature = (action_red, action_blue, narrate = false))]
    fn step(&mut self, action_red: u8, action_blue: u8, narrate: bool) -> (bool, i64, Vec<String>) {
        let c1 = self.resolve(SideId::One, action_red);
        let c2 = self.resolve(SideId::Two, action_blue);

        let branches = generate_instructions(&self.state, c1, c2);
        if branches.is_empty() {
            // Should not happen for legal actions; do nothing rather than panic.
            return (self.is_over(), self.winner(), Vec::new());
        }
        let idx = self.sample(&branches);
        let lines = if narrate {
            narrate_turn(&self.state, c1, c2, &branches[idx].instructions)
        } else {
            Vec::new()
        };
        self.state.apply_instructions(&branches[idx].instructions);
        self.state.turn += 1;

        (self.is_over(), self.winner(), lines)
    }

    /// The full *true* battle state (both sides, perfect information) as a JSON string, for
    /// adapters that build a foreign engine's state (e.g. poke-engine for an MCTS baseline).
    /// All identifiers are PS `toID` strings; the caller maps them to the target engine's format.
    fn state_json(&self) -> String {
        use serde_json::{json, Value};
        let s = &self.state;
        let ty = |t: engine::ids::Type| t.to_id();

        let side_json = |side: SideId| -> Value {
            let sd = s.side(side);
            let mons: Vec<Value> = (0..6)
                .map(|i| {
                    let p = &sd.pokemon[i];
                    let moves: Vec<Value> = (0..4)
                        .map(|m| {
                            let mv = p.moves[m];
                            json!({"id": mv.id.to_id(), "pp": mv.pp, "disabled": mv.disabled})
                        })
                        .collect();
                    json!({
                        "species": p.species.to_id(),
                        "level": p.level,
                        "types": [ty(p.types[0]), ty(p.types[1])],
                        "base_types": [ty(p.base_types[0]), ty(p.base_types[1])],
                        "hp": p.hp, "maxhp": p.max_hp,
                        "ability": p.ability.to_id(), "base_ability": p.base_ability.to_id(),
                        "item": p.item.to_id(),
                        "nature": nature_str(p.nature),
                        "evs": p.evs,
                        "stats": {"atk": p.stats[1], "def": p.stats[2], "spa": p.stats[3], "spd": p.stats[4], "spe": p.stats[5]},
                        "status": p.status.to_id(), "status_counter": p.status_counter,
                        "tera_type": ty(p.tera_type), "terastallized": p.terastallized,
                        "weight_kg": engine::data::species_weight_hg(p.species) as f64 / 10.0,
                        "moves": moves,
                    })
                })
                .collect();
            let sc = &sd.side_conditions;
            json!({
                "active_index": sd.active_index,
                "last_used_move": sd.last_used_move.to_id(),
                "boosts": {"atk": sd.boosts[0], "def": sd.boosts[1], "spa": sd.boosts[2],
                           "spd": sd.boosts[3], "spe": sd.boosts[4], "accuracy": sd.boosts[5], "evasion": sd.boosts[6]},
                "side_conditions": {
                    "stealth_rock": sc.stealth_rock, "spikes": sc.spikes, "toxic_spikes": sc.toxic_spikes,
                    "sticky_web": sc.sticky_web, "reflect": sc.reflect, "light_screen": sc.light_screen,
                    "aurora_veil": sc.aurora_veil, "tailwind": sc.tailwind,
                },
                "pokemon": mons,
            })
        };

        json!({
            "weather": s.weather.to_id(), "weather_turns": s.weather_turns,
            "terrain": s.terrain.to_id(), "terrain_turns": s.terrain_turns,
            "trick_room": s.trick_room, "trick_room_turns": s.trick_room_turns,
            "turn": s.turn,
            "sides": [side_json(SideId::One), side_json(SideId::Two)],
        })
        .to_string()
    }

    /// A compact text snapshot of the board (active + bench HP%), for following along.
    fn render(&self) -> String {
        let mut out = String::new();
        for (label, side) in [("Red ", SideId::One), ("Blue", SideId::Two)] {
            let s = self.state.side(side);
            let a = s.active();
            out.push_str(&format!(
                "{}: *{} {}%*",
                label,
                cap(a.species.to_id()),
                hp_pct(a.hp, a.max_hp)
            ));
            let bench: Vec<String> = (0..6)
                .filter(|&i| i as u8 != s.active_index && s.pokemon[i].species != engine::ids::Species::None)
                .map(|i| {
                    let p = &s.pokemon[i];
                    format!("{} {}%", cap(p.species.to_id()), hp_pct(p.hp, p.max_hp))
                })
                .collect();
            out.push_str(&format!("  | {}\n", bench.join(", ")));
        }
        out
    }
}

impl Battle {
    /// The five non-active party slots, in slot order (index k -> switch action 4+k).
    fn bench(&self, side: SideId) -> Vec<Option<u8>> {
        let s = self.state.side(side);
        let mut out = Vec::with_capacity(5);
        for i in 0..6u8 {
            if i != s.active_index {
                out.push(if s.pokemon[i as usize].species != engine::ids::Species::None { Some(i) } else { None });
            }
        }
        out
    }

    /// Map an action index to a `MoveChoice`, coercing anything illegal to the first legal action.
    fn resolve(&self, side: SideId, action: u8) -> MoveChoice {
        let legal = {
            let mask = self.legal_actions(side.index() as u8);
            mask
        };
        let a = action as usize;
        if a < N_ACTIONS && legal[a] {
            return self.choice_for(side, action);
        }
        // Fallback: first legal action (forced switches etc.).
        for (i, ok) in legal.iter().enumerate() {
            if *ok {
                return self.choice_for(side, i as u8);
            }
        }
        // No legal action (side already lost) — return a harmless move; the turn won't matter.
        MoveChoice::Move(0)
    }

    fn choice_for(&self, side: SideId, action: u8) -> MoveChoice {
        let a = action as usize;
        if a < N_MOVES {
            MoveChoice::Move(action)
        } else {
            let k = a - N_MOVES;
            match self.bench(side).get(k).copied().flatten() {
                Some(slot) => MoveChoice::Switch(slot),
                None => MoveChoice::Move(0),
            }
        }
    }

    fn side_lost(&self, side: SideId) -> bool {
        !self
            .state
            .side(side)
            .pokemon
            .iter()
            .any(|p| p.species != engine::ids::Species::None && p.is_alive())
    }

    /// Sample one outcome branch by its percentage weight.
    fn sample(&mut self, branches: &[StateInstructions]) -> usize {
        let total: f32 = branches.iter().map(|b| b.percentage).sum::<f32>().max(1e-6);
        let mut r = self.rng.next_unit() * total;
        for (i, b) in branches.iter().enumerate() {
            r -= b.percentage;
            if r <= 0.0 {
                return i;
            }
        }
        branches.len() - 1
    }
}

/// PS `toID` string for a nature (the engine has no `to_id` for `Nature`).
fn nature_str(n: engine::ids::Nature) -> &'static str {
    use engine::ids::Nature::*;
    match n {
        Hardy => "hardy", Lonely => "lonely", Brave => "brave", Adamant => "adamant", Naughty => "naughty",
        Bold => "bold", Docile => "docile", Relaxed => "relaxed", Impish => "impish", Lax => "lax",
        Timid => "timid", Hasty => "hasty", Serious => "serious", Jolly => "jolly", Naive => "naive",
        Modest => "modest", Mild => "mild", Quiet => "quiet", Bashful => "bashful", Rash => "rash",
        Calm => "calm", Gentle => "gentle", Sassy => "sassy", Careful => "careful", Quirky => "quirky",
    }
}

fn sid(side: u8) -> SideId {
    if side == 0 {
        SideId::One
    } else {
        SideId::Two
    }
}

fn hp_pct(hp: i16, max_hp: i16) -> i32 {
    let m = max_hp.max(1) as i32;
    (hp.max(0) as i32 * 100 / m).clamp(0, 100)
}

fn cap(id: &str) -> String {
    let mut c = id.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[pymodule]
fn showdown_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Battle>()?;
    Ok(())
}
