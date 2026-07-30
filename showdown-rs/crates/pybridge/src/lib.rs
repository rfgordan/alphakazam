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
use engine::protocol::{protocol_turn, HpStyle};
use engine::state::{SideId, State};
use engine::team;
use pyo3::prelude::*;

// Turn resolution allocates hard (a `Vec<Instruction>` per branch, grown per push), and that
// traffic is what limits multi-core scaling far more than the arithmetic does.
#[cfg(feature = "mimalloc-alloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

    /// Active party slot for a side (0 = Red, 1 = Blue) — for reading a specific mon over a turn.
    fn active_index(&self, side: u8) -> u8 {
        self.state.side(sid(side)).active_index
    }

    /// HP fraction in [0, 1] of `side`'s party slot `slot` — used to build world-model labels
    /// (damage dealt / KOs) without re-encoding the whole state.
    fn hp_fraction(&self, side: u8, slot: u8) -> f32 {
        let p = &self.state.side(sid(side)).pokemon[slot as usize];
        if p.max_hp <= 0 {
            0.0
        } else {
            (p.hp.max(0) as f32) / (p.max_hp as f32)
        }
    }

    /// Mean HP fraction across a side's 6 party slots (fainted/empty count as 0) — the potential
    /// for reward shaping (a side's "team health").
    fn team_hp_fraction(&self, side: u8) -> f32 {
        let s = self.state.side(sid(side));
        let mut sum = 0.0f32;
        for p in &s.pokemon {
            if p.species != engine::ids::Species::None && p.max_hp > 0 {
                sum += (p.hp.max(0) as f32) / (p.max_hp as f32);
            }
        }
        sum / 6.0
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

    /// Which embedding table each ID column indexes into (length `ids_per_mon`). Last = last-used
    /// move.
    fn id_columns(&self) -> Vec<String> {
        ["species", "ability", "item", "type", "move", "move", "move", "move", "move"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// Vocabulary size (number of embedding rows needed) for each table named in `id_columns()`.
    /// `BTreeMap`, not `HashMap`: the caller builds one embedding table per entry **in iteration
    /// order**, so a randomized order makes the model's parameter list differ run-to-run. State
    /// dicts survive that (they are keyed by name) but Adam's state is keyed by parameter
    /// *position*, so resuming a run crashed with a shape mismatch. Keep this deterministic.
    fn vocab_sizes(&self) -> std::collections::BTreeMap<String, usize> {
        use engine::ids::{Ability, Item, Type};
        let mut m = std::collections::BTreeMap::new();
        m.insert("species".to_string(), engine::gen::SPECIES_NAMES.len());
        m.insert("move".to_string(), engine::gen::MOVE_NAMES.len());
        m.insert("item".to_string(), Item::Unknown as usize + 1);
        m.insert("ability".to_string(), Ability::Unknown as usize + 1);
        m.insert("type".to_string(), Type::Stellar as usize + 1);
        m
    }

    /// Boolean legal-action mask of length 9 for `side`.
    fn legal_actions(&self, side: u8) -> Vec<bool> {
        legal_mask_of(&self.state, sid(side)).to_vec()
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
    ///   lines  — English commentary if `narrate=True`, else PS **protocol** lines if
    ///            `protocol=True`, else empty (both off = zero cost on the hot path).
    #[pyo3(signature = (action_red, action_blue, narrate = false, protocol = false))]
    fn step(&mut self, action_red: u8, action_blue: u8, narrate: bool, protocol: bool) -> (bool, i64, Vec<String>) {
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
        } else if protocol {
            let mut out = Vec::new();
            protocol_turn(&self.state, c1, c2, &branches[idx].instructions, HpStyle::Percent, &mut out);
            out
        } else {
            Vec::new()
        };
        self.state.apply_instructions(&branches[idx].instructions);
        self.state.turn += 1;

        (self.is_over(), self.winner(), lines)
    }

    /// Spot-check entry point (Rob's directive): serialize the current TRUE battle state as a PS
    /// `State.deserializeBattle`-loadable snapshot (the same exporter the round-trip/transplant
    /// gates certify). Drop the returned JSON into pinned Showdown to inspect the position live.
    /// `seed` is the 4-limb PRNG seed written into the snapshot (default `[1,2,3,4]`).
    #[pyo3(signature = (seed = None))]
    fn export_state(&self, seed: Option<Vec<u16>>) -> String {
        let s = seed
            .and_then(|v| <[u16; 4]>::try_from(v).ok())
            .unwrap_or([1, 2, 3, 4]);
        cosim::export::export_state(&self.state, s).to_string()
    }

    /// The full *true* battle state (both sides, perfect information) as a JSON string, for
    /// adapters that build a foreign engine's state (e.g. poke-engine for an MCTS baseline).
    /// All identifiers are PS `toID` strings; the caller maps them to the target engine's format.
    fn state_json(&self) -> String {
        state_json_of(&self.state)
    }

    /// Type effectiveness multiplier of `attacking` (a type id) against one or two `defending`
    /// type ids — the data a heuristic player needs for matchup/move scoring.
    fn type_effectiveness(&self, attacking: String, defending: Vec<String>) -> f64 {
        use engine::ids::Type;
        let at = match Type::from_id(&attacking) {
            Some(t) => t,
            None => return 1.0,
        };
        let mut d = [Type::None, Type::None];
        for (i, s) in defending.iter().take(2).enumerate() {
            d[i] = Type::from_id(s).unwrap_or(Type::None);
        }
        engine::damage::type_multiplier(at, d) as f64
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
        bench_slots(&self.state, side).to_vec()
    }

    /// Map an action index to a `MoveChoice`, coercing anything illegal to the first legal action.
    fn resolve(&self, side: SideId, action: u8) -> MoveChoice {
        resolve_of(&self.state, side, action)
    }

    fn side_lost(&self, side: SideId) -> bool {
        lost_of(&self.state, side)
    }

    /// Sample one outcome branch by its percentage weight.
    fn sample(&mut self, branches: &[StateInstructions]) -> usize {
        sample_of(&mut self.rng, branches)
    }
}

fn cat_str(c: engine::ids::MoveCategory) -> &'static str {
    use engine::ids::MoveCategory::*;
    match c {
        Physical => "physical",
        Special => "special",
        Status => "status",
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

/// The full *true* battle state as JSON. Free function so both `Battle` and `FlowVec` expose
/// it from the same source — the heuristic baseline needs it from the decision-point env, and
/// a second copy of this mapping would rot out of sync with the first.
pub fn state_json_of(state: &State) -> String {
        use serde_json::{json, Value};
        let s = state;
        let ty = |t: engine::ids::Type| t.to_id();

        let side_json = |side: SideId| -> Value {
            let sd = s.side(side);
            let mons: Vec<Value> = (0..6)
                .map(|i| {
                    let p = &sd.pokemon[i];
                    let moves: Vec<Value> = (0..4)
                        .map(|m| {
                            let mv = p.moves[m];
                            let md = engine::data::move_data(mv.id);
                            json!({
                                "id": mv.id.to_id(), "pp": mv.pp, "disabled": mv.disabled,
                                "type": md.typ.to_id(), "base_power": md.base_power, "accuracy": md.accuracy,
                                "category": cat_str(md.category),
                                "self_boost_total": md.self_boosts.iter().map(|&x| x as i32).sum::<i32>(),
                            })
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

// ---- shared state-level helpers (used by both Battle and BattleVec) ------------------------

/// The five non-active party slots in slot order (switch action 4+k -> k-th entry).
///
/// `k < 5` guard: a `Flow` always keeps a fainted mon as the active, so exactly one of the six
/// slots is skipped and `k` tops out at 5. A state CONVERTED from a PS snapshot need not —
/// at a forced-switch request PS has already taken the fainted mon off the field and `convert`
/// records `active_index = u8::MAX`, so no slot is skipped, `k` reaches 5, and this wrote past
/// the array. With no active there is no sixth switch action to name anyway.
fn bench_slots(state: &State, side: SideId) -> [Option<u8>; 5] {
    let s = state.side(side);
    let mut out = [None; 5];
    let mut k = 0;
    for i in 0..6u8 {
        if k >= out.len() {
            break;
        }
        if i != s.active_index {
            if s.pokemon[i as usize].species != engine::ids::Species::None {
                out[k] = Some(i);
            }
            k += 1;
        }
    }
    out
}

fn legal_mask_of(state: &State, side: SideId) -> [bool; N_ACTIONS] {
    let s = state.side(side);
    let active = s.active();
    let mut mask = [false; N_ACTIONS];
    if active.is_alive() {
        for i in 0..N_MOVES {
            let m = active.moves[i];
            mask[i] = m.id != engine::ids::MoveId::None
                && m.pp > 0
                && !engine::generate::cantusetwice_locked(state, side, m.id);
        }
    }
    for (k, slot) in bench_slots(state, side).into_iter().enumerate() {
        if let Some(slot) = slot {
            mask[N_MOVES + k] = state.side(side).pokemon[slot as usize].is_alive();
        }
    }
    mask
}

fn choice_of(state: &State, side: SideId, action: u8) -> MoveChoice {
    let a = action as usize;
    if a < N_MOVES {
        MoveChoice::Move(action)
    } else {
        match bench_slots(state, side).get(a - N_MOVES).copied().flatten() {
            Some(slot) => MoveChoice::Switch(slot),
            None => MoveChoice::Move(0),
        }
    }
}

/// Action index -> MoveChoice, coercing anything illegal to the first legal action.
fn resolve_of(state: &State, side: SideId, action: u8) -> MoveChoice {
    let legal = legal_mask_of(state, side);
    let a = action as usize;
    if a < N_ACTIONS && legal[a] {
        return choice_of(state, side, action);
    }
    for (i, ok) in legal.iter().enumerate() {
        if *ok {
            return choice_of(state, side, i as u8);
        }
    }
    MoveChoice::Move(0)
}

fn lost_of(state: &State, side: SideId) -> bool {
    !state
        .side(side)
        .pokemon
        .iter()
        .any(|p| p.species != engine::ids::Species::None && p.is_alive())
}

fn sample_of(rng: &mut Rng, branches: &[StateInstructions]) -> usize {
    let total: f32 = branches.iter().map(|b| b.percentage).sum::<f32>().max(1e-6);
    let mut r = rng.next_unit() * total;
    for (i, b) in branches.iter().enumerate() {
        r -= b.percentage;
        if r <= 0.0 {
            return i;
        }
    }
    branches.len() - 1
}

/// Advance one turn in place via the sampled executor (single weighted path — no branch
/// enumeration; distribution-pinned to the enumerate path by the engine's test suite).
fn step_of(state: &mut State, rng: &mut Rng, action_red: u8, action_blue: u8) {
    let c1 = resolve_of(state, SideId::One, action_red);
    let c2 = resolve_of(state, SideId::Two, action_blue);
    let si = engine::generate::generate_instructions_sampled(
        state, c1, c2, [engine::generate::Pivot::Stay; 2], [false, false], &mut rng.0,
    );
    state.apply_instructions(&si.instructions);
    state.turn += 1;
}

fn winner_of(state: &State) -> i64 {
    match (lost_of(state, SideId::One), lost_of(state, SideId::Two)) {
        (false, true) => 0,
        (true, false) => 1,
        _ => -1,
    }
}

// ---- vectorized bridge ----------------------------------------------------------------------

use numpy::ndarray::{Array1, Array2};
use numpy::{IntoPyArray, PyArray1, PyArray2, PyReadonlyArray1};
use rayon::prelude::*;

/// A batch of independent battles stepped/encoded in parallel in Rust, exchanged with Python as
/// numpy arrays in a single GIL crossing per call — the throughput path for vectorized PPO.
/// Same MDP and 9-action space as `Battle` (self-play: both sides act every call).
#[pyclass]
pub struct BattleVec {
    states: Vec<State>,
    rngs: Vec<Rng>,
    /// Per-env team-draw RNG (separate stream from outcome sampling, so drawing a fresh matchup
    /// never perturbs battle outcomes). `None` (no pool) leaves battles on the default matchup.
    draw_rngs: Vec<Rng>,
    pool: Option<Arc<Vec<PoolTeam>>>,
    /// Episode turn counters; battles hitting `max_turns` are truncated (done, winner -1).
    steps: Vec<u32>,
    max_turns: u32,
}

#[pymethods]
impl BattleVec {
    /// `team_pool`: path to a gzipped JSONL pool (harness/gen-team-pool.mjs output). When set, each
    /// env draws two random real-PS random-battle teams per reset; otherwise the fixed matchup.
    #[new]
    #[pyo3(signature = (num_envs, seed = 0, max_turns = 500, team_pool = None))]
    fn new(num_envs: usize, seed: u64, max_turns: u32, team_pool: Option<String>) -> PyResult<Self> {
        let pool = load_pool_opt(team_pool)?;
        let mut draw_rngs: Vec<Rng> = (0..num_envs)
            .map(|i| Rng(seed.wrapping_add(0x51_ED_27_09).wrapping_add((i as u64) << 32)))
            .collect();
        let states = (0..num_envs).map(|i| draw_state(&pool, &mut draw_rngs[i])).collect();
        Ok(BattleVec {
            states,
            rngs: (0..num_envs)
                .map(|i| Rng(seed.wrapping_add(1).wrapping_add((i as u64) << 32)))
                .collect(),
            draw_rngs,
            pool,
            steps: vec![0; num_envs],
            max_turns,
        })
    }

    /// Number of teams in the loaded pool (0 when running the fixed default matchup).
    #[getter]
    fn pool_size(&self) -> usize {
        self.pool.as_ref().map(|p| p.len()).unwrap_or(0)
    }

    #[getter]
    fn num_envs(&self) -> usize {
        self.states.len()
    }
    #[getter]
    fn obs_dim(&self) -> usize {
        engine::encode::OBS_DIM
    }
    #[getter]
    fn n_actions(&self) -> usize {
        N_ACTIONS
    }
    #[getter]
    fn id_dim(&self) -> usize {
        engine::encode::ID_DIM
    }

    /// (N, OBS_DIM) f32 observations from `side`'s perspective.
    fn observe_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<f32>> {
        let n = self.states.len();
        let dim = engine::encode::OBS_DIM;
        let mut flat = vec![0f32; n * dim];
        py.allow_threads(|| {
            flat.par_chunks_mut(dim).zip(self.states.par_iter()).for_each(|(dst, st)| {
                dst.copy_from_slice(&engine::encode::encode(st, sid(side)));
            });
        });
        Array2::from_shape_vec((n, dim), flat).unwrap().into_pyarray_bound(py)
    }

    /// (N, ID_DIM) i64 categorical IDs from `side`'s perspective (embedding-table inputs).
    fn observe_ids_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<i64>> {
        let n = self.states.len();
        let dim = engine::encode::ID_DIM;
        let mut flat = vec![0i64; n * dim];
        py.allow_threads(|| {
            flat.par_chunks_mut(dim).zip(self.states.par_iter()).for_each(|(dst, st)| {
                dst.copy_from_slice(&engine::encode::encode_ids(st, sid(side)));
            });
        });
        Array2::from_shape_vec((n, dim), flat).unwrap().into_pyarray_bound(py)
    }

    /// (N, 9) bool legal-action masks for `side`.
    fn legal_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<bool>> {
        let n = self.states.len();
        let mut flat = vec![false; n * N_ACTIONS];
        for (dst, st) in flat.chunks_mut(N_ACTIONS).zip(self.states.iter()) {
            dst.copy_from_slice(&legal_mask_of(st, sid(side)));
        }
        Array2::from_shape_vec((n, N_ACTIONS), flat).unwrap().into_pyarray_bound(py)
    }

    /// (N,) f32 mean team HP fraction for `side` — the reward-shaping potential.
    fn team_hp_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray1<f32>> {
        let v: Vec<f32> = self
            .states
            .iter()
            .map(|st| {
                let s = st.side(sid(side));
                s.pokemon
                    .iter()
                    .filter(|p| p.species != engine::ids::Species::None && p.max_hp > 0)
                    .map(|p| (p.hp.max(0) as f32) / (p.max_hp as f32))
                    .sum::<f32>()
                    / 6.0
            })
            .collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// (N,) i64 fainted-mon count for `side` — the other Φ term.
    fn faints_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray1<i64>> {
        let v: Vec<i64> = self
            .states
            .iter()
            .map(|st| {
                st.side(sid(side))
                    .pokemon
                    .iter()
                    .filter(|p| p.species != engine::ids::Species::None && !p.is_alive())
                    .count() as i64
            })
            .collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// Step every battle one turn. Returns `(done, winner)` as (N,) arrays describing the step
    /// just taken; battles that finished (or hit `max_turns`) are reset in place when
    /// `auto_reset` (their next observation is the fresh battle).
    #[pyo3(signature = (action_red, action_blue, auto_reset = true))]
    fn step_all<'py>(
        &mut self,
        py: Python<'py>,
        action_red: PyReadonlyArray1<'py, i64>,
        action_blue: PyReadonlyArray1<'py, i64>,
        auto_reset: bool,
    ) -> PyResult<(Bound<'py, PyArray1<bool>>, Bound<'py, PyArray1<i64>>)> {
        let red = action_red.as_slice()?.to_vec();
        let blue = action_blue.as_slice()?.to_vec();
        let n = self.states.len();
        if red.len() != n || blue.len() != n {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "action arrays must have length {n} (got {} / {})",
                red.len(),
                blue.len()
            )));
        }
        let max_turns = self.max_turns;
        let pool = &self.pool;
        let (dones, winners): (Vec<bool>, Vec<i64>) = py.allow_threads(|| {
            self.states
                .par_iter_mut()
                .zip(self.rngs.par_iter_mut())
                .zip(self.draw_rngs.par_iter_mut())
                .zip(self.steps.par_iter_mut())
                .enumerate()
                .map(|(i, (((st, rng), draw_rng), steps))| {
                    step_of(st, rng, red[i] as u8, blue[i] as u8);
                    *steps += 1;
                    let over = lost_of(st, SideId::One) || lost_of(st, SideId::Two);
                    let truncated = *steps >= max_turns;
                    let done = over || truncated;
                    let winner = if over { winner_of(st) } else { -1 };
                    if done && auto_reset {
                        *st = draw_state(pool, draw_rng);
                        *steps = 0;
                    }
                    (done, winner)
                })
                .unzip()
        });
        Ok((
            Array1::from_vec(dones).into_pyarray_bound(py),
            Array1::from_vec(winners).into_pyarray_bound(py),
        ))
    }
}

// ---- real-PS team pool loader ---------------------------------------------------------------
//
// We do NOT reimplement PS's random-team generator; harness/gen-team-pool.mjs *drives* the pinned
// PS generator and serializes each team as MemberSpec-compatible JSON (toID ids). Here we parse a
// gzipped JSONL pool into resolved engine teams. Every id is resolved through the engine's tables
// and an unknown id is a LOUD error — never a silent default — so a pool that outran the engine's
// coverage fails fast and visibly rather than training on corrupted teams.

use std::sync::Arc;

use engine::ids::{Ability, Item, MoveId, Nature, Species, Type};
use engine::team::ResolvedMember;

/// One resolved 6-mon pool team.
type PoolTeam = [ResolvedMember; 6];

fn resolve_species(s: &str) -> Result<Species, String> {
    Species::from_id(s).ok_or_else(|| format!("unknown species id {s:?}"))
}
fn resolve_ability(s: &str) -> Result<Ability, String> {
    Ability::from_id(s).ok_or_else(|| format!("unknown ability id {s:?}"))
}
/// Empty / "none" item id means the set holds no item (legitimate, e.g. Acrobatics users).
fn resolve_item(s: &str) -> Result<Item, String> {
    if s.is_empty() || s == "none" {
        return Ok(Item::None);
    }
    Item::from_id(s).ok_or_else(|| format!("unknown item id {s:?}"))
}
fn resolve_type(s: &str) -> Result<Type, String> {
    Type::from_id(s).ok_or_else(|| format!("unknown type id {s:?}"))
}
fn resolve_nature(s: &str) -> Result<Nature, String> {
    Nature::from_id(s).ok_or_else(|| format!("unknown nature id {s:?}"))
}
/// Empty / "none" move id means an empty move slot.
fn resolve_move(s: &str) -> Result<MoveId, String> {
    if s.is_empty() || s == "none" {
        return Ok(MoveId::None);
    }
    MoveId::from_id(s).ok_or_else(|| format!("unknown move id {s:?}"))
}

fn stat_array(v: &serde_json::Value, field: &str) -> Result<[u8; 6], String> {
    let arr = v.get(field).and_then(|x| x.as_array()).ok_or_else(|| format!("missing {field}[] array"))?;
    if arr.len() != 6 {
        return Err(format!("{field} must have 6 entries, got {}", arr.len()));
    }
    let mut out = [0u8; 6];
    for (i, x) in arr.iter().enumerate() {
        out[i] = x.as_u64().ok_or_else(|| format!("{field}[{i}] not an integer"))? as u8;
    }
    Ok(out)
}

fn resolve_member(m: &serde_json::Value) -> Result<ResolvedMember, String> {
    let species = resolve_species(m.get("species").and_then(|x| x.as_str()).ok_or("member missing species")?)?;
    let level = m.get("level").and_then(|x| x.as_u64()).ok_or("member missing level")? as u8;
    let ability = resolve_ability(m.get("ability").and_then(|x| x.as_str()).ok_or("member missing ability")?)?;
    let item = resolve_item(m.get("item").and_then(|x| x.as_str()).unwrap_or(""))?;
    let tera = resolve_type(m.get("tera").and_then(|x| x.as_str()).ok_or("member missing tera")?)?;
    let nature = resolve_nature(m.get("nature").and_then(|x| x.as_str()).ok_or("member missing nature")?)?;
    let evs = stat_array(m, "evs")?;
    let ivs = stat_array(m, "ivs")?;

    let moves_json = m.get("moves").and_then(|x| x.as_array()).ok_or("member missing moves[]")?;
    let mut moves = [MoveId::None; 4];
    for (i, mv) in moves_json.iter().take(4).enumerate() {
        moves[i] = resolve_move(mv.as_str().ok_or("move not a string")?)?;
    }

    Ok(ResolvedMember { species, level, ability, item, tera, nature, evs, ivs, moves })
}

/// Parse one JSONL pool line (a `{"team":[6 members]}` object) into a resolved 6-mon team.
/// Any unknown species/ability/item/move/type/nature id is a loud error.
pub fn team_from_pool_line(json: &str) -> Result<PoolTeam, String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("invalid JSON: {e}"))?;
    let team = v.get("team").and_then(|x| x.as_array()).ok_or("line missing team[] array")?;
    if team.len() != 6 {
        return Err(format!("team must have 6 members, got {}", team.len()));
    }
    let mut out = [ResolvedMember {
        species: Species::None,
        level: 0,
        ability: Ability::None,
        item: Item::None,
        tera: Type::None,
        nature: Nature::Serious,
        evs: [0; 6],
        ivs: [31; 6],
        moves: [MoveId::None; 4],
    }; 6];
    for (i, m) in team.iter().enumerate() {
        out[i] = resolve_member(m).map_err(|e| format!("member {i}: {e}"))?;
    }
    Ok(out)
}

/// Load a gzipped JSONL pool file into resolved teams. Errors carry the offending line number.
fn load_pool(path: &str) -> Result<Vec<PoolTeam>, String> {
    use std::io::BufRead;
    let f = std::fs::File::open(path).map_err(|e| format!("open pool {path}: {e}"))?;
    let gz = flate2::read::GzDecoder::new(f);
    let reader = std::io::BufReader::new(gz);
    let mut out = Vec::new();
    for (ln, line) in reader.lines().enumerate() {
        let line = line.map_err(|e| format!("read {path} line {ln}: {e}"))?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(team_from_pool_line(&line).map_err(|e| format!("{path} line {ln}: {e}"))?);
    }
    if out.is_empty() {
        return Err(format!("pool {path} contained no teams"));
    }
    Ok(out)
}

/// Optionally load a pool from an `Option<path>`, mapping errors to a Python `ValueError`.
fn load_pool_opt(team_pool: Option<String>) -> PyResult<Option<Arc<Vec<PoolTeam>>>> {
    match team_pool {
        None => Ok(None),
        Some(path) => load_pool(&path)
            .map(|v| Some(Arc::new(v)))
            .map_err(pyo3::exceptions::PyValueError::new_err),
    }
}

/// Draw two distinct-index pool teams with `rng` and assemble a fresh battle `State`. Falls back to
/// the fixed default matchup when there is no pool.
fn draw_state(pool: &Option<Arc<Vec<PoolTeam>>>, rng: &mut Rng) -> State {
    match pool {
        None => team::default_matchup(),
        Some(p) => {
            let a = (rng.next_u64() % p.len() as u64) as usize;
            let b = (rng.next_u64() % p.len() as u64) as usize;
            team::build_state_resolved(&p[a], &p[b])
        }
    }
}

/// Version-dispatched encoding for the flow bridge (v2 = +damage-calc block).
fn enc_v(version: u8, st: &engine::state::State, side: SideId) -> Vec<f32> {
    if version == 2 { engine::encode::encode_v2(st, side) } else { engine::encode::encode(st, side) }
}
fn obs_dim_v(version: u8) -> usize {
    if version == 2 { engine::encode::OBS_DIM_V2 } else { engine::encode::OBS_DIM }
}

// ---- request-driven vectorized bridge (13-action decision-point MDP) ------------------------

use engine::request::{Flow, PlayerChoice, Request};

/// Number of actions in the decision-point space: 4 moves + 5 switches + 4 tera-moves.
const N_ACTIONS_FLOW: usize = 13;

/// Whether `side` must answer the pending `req` (Turn: both; Replace: flagged; PivotLanding: the
/// named side; Terminal: neither).
fn acting_for(req: Request, side: SideId) -> bool {
    match req {
        Request::Turn => true,
        Request::Replace { sides } => sides[side.index()],
        Request::PivotLanding { side: s } => s == side,
        Request::Revive { side: s } => s == side,
        Request::Terminal { .. } => false,
    }
}

/// Party index of the k-th non-active slot (0..5), independent of whether it is filled/alive —
/// so a switch action always maps to *some* slot and `Flow::submit` coerces empty/dead targets.
fn bench_party_slot(state: &State, side: SideId, k: usize) -> u8 {
    let ai = state.side(side).active_index;
    let mut j = 0;
    for i in 0..6u8 {
        if i != ai {
            if j == k {
                return i;
            }
            j += 1;
        }
    }
    ai
}

/// Map an action int to a `PlayerChoice` (0-3 move, 4-8 switch to k-th bench slot, 9-12 tera-move).
fn flow_choice(state: &State, side: SideId, action: u8) -> PlayerChoice {
    match action {
        0..=3 => PlayerChoice::Move { slot: action, tera: false },
        4..=8 => PlayerChoice::Switch { slot: bench_party_slot(state, side, (action - 4) as usize) },
        9..=12 => PlayerChoice::Move { slot: action - 9, tera: true },
        _ => PlayerChoice::Move { slot: 0, tera: false },
    }
}

/// Phase-aware (N=13) legal mask for `side` under the pending `req`.
/// `_matchup` from `agents/ppo/baselines.py::HeuristicBaseline`, arithmetic-identical: f32
/// type-chart values widened to f64 exactly where the Python side crosses `type_effectiveness`,
/// same operation order, raw (unclamped) HP like the JSON path exposes.
fn heuristic_matchup(mon: &engine::state::Pokemon, opp: &engine::state::Pokemon) -> f64 {
    use engine::damage::type_multiplier;
    const SPEED_COEF: f64 = 0.1;
    const HP_COEF: f64 = 0.4;
    let best = |att: &[engine::ids::Type], def: [engine::ids::Type; 2]| -> f64 {
        att.iter()
            .filter(|&&t| t != engine::ids::Type::None)
            .map(|&t| type_multiplier(t, def) as f64)
            .fold(f64::NEG_INFINITY, f64::max)
    };
    let off = best(&mon.types, opp.types);
    let off = if off.is_finite() { off } else { 1.0 };
    let deff = best(&opp.types, mon.types);
    let deff = if deff.is_finite() { deff } else { 1.0 };
    let mut score = off - deff;
    score += if mon.stats[5] > opp.stats[5] {
        SPEED_COEF
    } else if opp.stats[5] > mon.stats[5] {
        -SPEED_COEF
    } else {
        0.0
    };
    score += HP_COEF * (mon.hp as f64 / mon.max_hp.max(1) as f64);
    score -= HP_COEF * (opp.hp as f64 / opp.max_hp.max(1) as f64);
    score
}

/// Faithful port of `agents/ppo/baselines.py::HeuristicBaseline._action_for` (itself a port of
/// poke-env's SimpleHeuristicsPlayer), evaluated directly on engine state. The Python version
/// costs ~400µs/env (a full `state_json` serialize + `json.loads` per env per step) and was
/// measured at 83% of trainer wall time when league slots draw the heuristic; this one is
/// rayon-parallel and three orders of magnitude cheaper. Action-exactness against the Python
/// implementation is enforced by `agents/probes/heuristic_parity.py` — any change here must keep
/// the two in lockstep, or the training opponent silently diverges from the eval opponent.
///
/// Returns -1 when the heuristic has no opinion (non-acting request, no legal action, or a state
/// the Python port would have raised on) — the caller falls back to a random legal action and
/// counts the event.
fn heuristic_action_of(state: &State, req: Request, side: SideId) -> i64 {
    use engine::damage::type_multiplier;
    const SWITCH_THRESHOLD: f64 = -2.0;

    let mask = flow_legal_mask(state, req, side);
    if !mask.iter().any(|&b| b) {
        return -1;
    }
    let sd = state.side(side);
    let od = state.side(side.other());
    let (ai, oi) = (sd.active_index as usize, od.active_index as usize);
    if ai >= 6 || oi >= 6 {
        return -1; // the Python port IndexErrors here and falls back; mirror that
    }
    let active = &sd.pokemon[ai];
    let opp = &od.pokemon[oi];
    let bench: Vec<usize> = (0..6).filter(|&s| s != ai).collect();
    let legal_moves: Vec<usize> = (0..4).filter(|&i| mask[i]).collect();
    let legal_switch: Vec<usize> = (0..5).filter(|&k| mask[4 + k]).collect();

    // Should we switch out? (a good reserve exists AND we're in a bad spot). Boosts live on the
    // SIDE in this engine; boosts[..5] = [atk, def, spa, spd, spe].
    let mut should_switch = false;
    if !legal_switch.is_empty()
        && legal_switch
            .iter()
            .any(|&k| heuristic_matchup(&sd.pokemon[bench[k]], opp) > 0.0)
    {
        let b = &sd.boosts;
        let phys = active.stats[1] >= active.stats[3];
        should_switch = b[1] <= -3
            || b[3] <= -3
            || (b[0] <= -3 && phys)
            || (b[2] <= -3 && !phys)
            || heuristic_matchup(active, opp) < SWITCH_THRESHOLD;
    }

    if !legal_moves.is_empty() && !(should_switch && !legal_switch.is_empty()) {
        let alive =
            |p: &engine::state::Pokemon| p.species != engine::ids::Species::None && p.hp > 0;
        let n_opp = od.pokemon.iter().filter(|p| alive(p)).count();
        let n_self = sd.pokemon.iter().filter(|p| alive(p)).count();

        // Hazards: set them up early; clear our own if any exist.
        for &i in &legal_moves {
            let id = active.moves[i].id.to_id();
            let hazard_set = match id {
                "spikes" => Some(od.side_conditions.spikes != 0),
                "stealthrock" => Some(od.side_conditions.stealth_rock),
                "stickyweb" => Some(od.side_conditions.sticky_web),
                "toxicspikes" => Some(od.side_conditions.toxic_spikes != 0),
                _ => None,
            };
            if let Some(already) = hazard_set {
                if !already && n_opp >= 3 {
                    return i as i64;
                }
            }
            if (id == "rapidspin" || id == "defog")
                && (sd.side_conditions.stealth_rock
                    || sd.side_conditions.spikes != 0
                    || sd.side_conditions.toxic_spikes != 0
                    || sd.side_conditions.sticky_web)
                && n_self >= 2
            {
                return i as i64;
            }
        }

        // Setup: boost when at full HP in a favorable matchup and not maxed.
        if active.hp >= active.max_hp
            && heuristic_matchup(active, opp) > 0.0
            && sd.boosts[..5].iter().copied().max().unwrap_or(0) < 6
        {
            for &i in &legal_moves {
                let md = engine::data::move_data(active.moves[i].id);
                let boost_total: i32 = md.self_boosts.iter().map(|&x| x as i32).sum();
                if boost_total >= 2 && md.base_power == 0 {
                    return i as i64;
                }
            }
        }

        // Best damaging move — strict `>` keeps Python `max()`'s first-of-ties semantics.
        let mut best: Option<(usize, f64)> = None;
        for &i in &legal_moves {
            let md = engine::data::move_data(active.moves[i].id);
            if md.base_power == 0 {
                continue;
            }
            let stab = if active.types.contains(&md.typ) { 1.5 } else { 1.0 };
            let acc = if md.accuracy > 0 { md.accuracy as f64 / 100.0 } else { 1.0 };
            let sc = md.base_power as f64 * stab * type_multiplier(md.typ, opp.types) as f64 * acc;
            if best.is_none_or(|(_, b)| sc > b) {
                best = Some((i, sc));
            }
        }
        if let Some((i, _)) = best {
            return i as i64;
        }
    }

    // Switch to the best-matchup reserve (again first-of-ties, ascending k).
    if !legal_switch.is_empty() {
        let mut best = (legal_switch[0], f64::NEG_INFINITY);
        for &k in &legal_switch {
            let m = heuristic_matchup(&sd.pokemon[bench[k]], opp);
            if m > best.1 {
                best = (k, m);
            }
        }
        return 4 + best.0 as i64;
    }
    // Python's terminal fallback: the first legal action (e.g. a Struggle-fallback move slot, or
    // only zero-power moves legal with an empty bench).
    match mask.iter().position(|&b| b) {
        Some(i) => i as i64,
        None => -1,
    }
}

fn flow_legal_mask(state: &State, req: Request, side: SideId) -> [bool; N_ACTIONS_FLOW] {
    let mut mask = [false; N_ACTIONS_FLOW];
    if !acting_for(req, side) {
        return mask;
    }
    let s = state.side(side);
    match req {
        Request::Turn => {
            let alive = s.active().is_alive();
            if alive {
                let active = s.active();
                for i in 0..N_MOVES {
                    let m = active.moves[i];
                    let ok = m.id != engine::ids::MoveId::None
                        && m.pp > 0
                        && !engine::generate::cantusetwice_locked(state, side, m.id);
                    mask[i] = ok;
                    if ok && !s.tera_used {
                        mask[9 + i] = true;
                    }
                }
            }
            // Trapping (Arena Trap / Shadow Tag / Magnet Pull / partial-trap / Mean Look / …)
            // forbids a voluntary switch on a Turn request. Faint-replacement and pivot-landing
            // phases are never trapped (PS always lets you pick a replacement / landing).
            let trapped = engine::generate::is_trapped(state, side);
            for (k, slot) in bench_slots(state, side).into_iter().enumerate() {
                if let Some(slot) = slot {
                    mask[N_MOVES + k] = !trapped && s.pokemon[slot as usize].is_alive();
                }
            }
            // PP-stalled active with nowhere to go: expose move 0 so the engine Struggle-detects it.
            if alive && mask.iter().all(|&b| !b) {
                mask[0] = true;
            }
        }
        Request::Replace { .. } | Request::PivotLanding { .. } => {
            for (k, slot) in bench_slots(state, side).into_iter().enumerate() {
                if let Some(slot) = slot {
                    mask[N_MOVES + k] = s.pokemon[slot as usize].is_alive();
                }
            }
        }
        Request::Revive { .. } => {
            // Revival Blessing: only a FAINTED party member is a legal revive target.
            for (k, slot) in bench_slots(state, side).into_iter().enumerate() {
                if let Some(slot) = slot {
                    let p = &s.pokemon[slot as usize];
                    mask[N_MOVES + k] = p.species != engine::ids::Species::None && !p.is_alive();
                }
            }
        }
        Request::Terminal { .. } => {}
    }
    mask
}

/// A batch of independent decision-point battles (`request::Flow`) stepped/encoded in parallel,
/// exchanged with Python as numpy in a single GIL crossing per call. The request-driven training
/// path with the real rules (faint replacements, pivots, tera) and the 13-action space.
#[pyclass]
pub struct FlowVec {
    flows: Vec<Flow>,
    /// Per-env request (decision-point) counters; episodes hitting `max_requests` are truncated.
    reqs: Vec<u32>,
    max_requests: u32,
    seed: u64,
    /// Per-env team-draw RNG (separate stream from Flow's internal outcome sampling).
    draw_rngs: Vec<Rng>,
    pool: Option<Arc<Vec<PoolTeam>>>,
    /// When set, each Flow accumulates PS protocol lines (fetched per-env via `protocol_log`).
    capture_protocol: bool,
    /// Fog of war: `State::observe` blanks never-seen foe species (see `State::fog_species`).
    fog_species: bool,
    /// Observation version: 1 = classic 643-dim, 2 = +damage-calc block (`encode_v2`, honest).
    obs_version: u8,
    /// Determinization donors: every pool member as a ready `Pokemon`, and the same indexed by
    /// species id — hidden attributes of seen foes and whole unseen slots are sampled from here.
    donors_all: Arc<Vec<engine::state::Pokemon>>,
    donors_by_species: Arc<std::collections::HashMap<u16, Vec<engine::state::Pokemon>>>,
}

/// The reproducible outcome-sampling seed for env `i` (matches the pre-pool behaviour).
fn flow_seed(seed: u64, i: usize) -> u64 {
    seed.wrapping_add(1).wrapping_add((i as u64) << 32)
}

/// A fresh `Flow`: pool teams drawn with `draw_rng` (or the fixed matchup when there is no pool),
/// seeded for outcome sampling by `outcome_seed`.
fn fresh_flow(pool: &Option<Arc<Vec<PoolTeam>>>, draw_rng: &mut Rng, outcome_seed: u64, capture: bool, fog: bool) -> Flow {
    let mut f = Flow::new(draw_state(pool, draw_rng), outcome_seed);
    f.state.fog_species = fog;
    // Leads are visible from turn 0; later entrances reveal via the switch instruction.
    f.state.reveal_leads();
    f.set_protocol_capture(capture);
    f
}

/// splitmix64 step for the determinization sampler.
fn mix64(z: &mut u64) -> u64 {
    *z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut x = *z;
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Resample everything the viewer cannot know about the foe (EXPLORATION_PLAN W8): unseen party
/// slots are replaced wholesale by pool donors (species-clause respected), and seen mons keep
/// their battle state while unrevealed moves/item/ability/tera are spliced from a same-species
/// donor. The viewer's own side and all global state are untouched. Falls back to the true
/// attribute where the pool has no donor (rare) — a bounded perfect-info leak, never a crash.
fn determinize_foe(
    state: &State,
    viewer: SideId,
    donors_all: &[engine::state::Pokemon],
    donors_by_species: &std::collections::HashMap<u16, Vec<engine::state::Pokemon>>,
    z: &mut u64,
) -> State {
    use engine::state::{MoveSlot, Reveal};
    let mut st = *state;
    let foe = viewer.other();
    let mut used: Vec<u16> = st
        .side(foe)
        .pokemon
        .iter()
        .filter(|p| p.species != engine::ids::Species::None && p.reveal.has(Reveal::SPECIES))
        .map(|p| p.species.0)
        .collect();
    let sd = st.side_mut(foe);
    for slot in 0..6 {
        let p = &mut sd.pokemon[slot];
        if p.species == engine::ids::Species::None {
            continue;
        }
        let r = p.reveal;
        if !r.has(Reveal::SPECIES) {
            if donors_all.is_empty() {
                continue;
            }
            for _ in 0..12 {
                let cand = &donors_all[(mix64(z) % donors_all.len() as u64) as usize];
                if !used.contains(&cand.species.0) {
                    let keep = p.reveal;
                    *p = *cand;
                    p.reveal = keep;
                    used.push(cand.species.0);
                    break;
                }
            }
            continue;
        }
        let Some(cands) = donors_by_species.get(&p.species.0) else { continue };
        let d = &cands[(mix64(z) % cands.len() as u64) as usize];
        if !r.has(Reveal::ITEM) {
            p.item = d.item;
        }
        if !r.has(Reveal::ABILITY) {
            p.ability = d.ability;
            p.base_ability = d.base_ability;
        }
        if !r.has(Reveal::TERA) {
            p.tera_type = d.tera_type;
        }
        let seen: Vec<engine::ids::MoveId> = (0..4u8)
            .filter(|&i| r.move_seen(i))
            .map(|i| p.moves[i as usize].id)
            .collect();
        let mut dpool: Vec<MoveSlot> = d
            .moves
            .iter()
            .copied()
            .filter(|m| m.id != engine::ids::MoveId::None && !seen.contains(&m.id))
            .collect();
        for i in 0..4u8 {
            if !r.move_seen(i) {
                p.moves[i as usize] = dpool.pop().unwrap_or(MoveSlot::EMPTY);
            }
        }
    }
    st
}

#[pymethods]
impl FlowVec {
    /// `team_pool`: path to a gzipped JSONL pool (harness/gen-team-pool.mjs output). When set, each
    /// env draws two random real-PS random-battle teams per reset; otherwise the fixed matchup.
    #[new]
    #[pyo3(signature = (num_envs, seed = 0, max_requests_per_episode = 1000, team_pool = None, capture_protocol = false, fog_species = false, obs_version = 1))]
    fn new(
        num_envs: usize,
        seed: u64,
        max_requests_per_episode: u32,
        team_pool: Option<String>,
        capture_protocol: bool,
        fog_species: bool,
        obs_version: u8,
    ) -> PyResult<Self> {
        if !(obs_version == 1 || obs_version == 2) {
            return Err(pyo3::exceptions::PyValueError::new_err("obs_version must be 1 or 2"));
        }
        let pool = load_pool_opt(team_pool)?;
        let mut draw_rngs: Vec<Rng> = (0..num_envs)
            .map(|i| Rng(seed.wrapping_add(0x51_ED_27_09).wrapping_add((i as u64) << 32)))
            .collect();
        let flows = (0..num_envs)
            .map(|i| fresh_flow(&pool, &mut draw_rngs[i], flow_seed(seed, i), capture_protocol, fog_species))
            .collect();
        // Determinization donors, built once: every pool member as a ready Pokemon.
        let mut donors_all = Vec::new();
        let mut donors_by_species: std::collections::HashMap<u16, Vec<engine::state::Pokemon>> =
            std::collections::HashMap::new();
        if let Some(p) = &pool {
            for team in p.iter() {
                for m in team.iter() {
                    let mon = engine::team::build_member_resolved(m);
                    donors_by_species.entry(mon.species.0).or_default().push(mon);
                    donors_all.push(mon);
                }
            }
        }
        Ok(FlowVec {
            flows,
            reqs: vec![0; num_envs],
            max_requests: max_requests_per_episode,
            seed,
            draw_rngs,
            pool,
            capture_protocol,
            fog_species,
            obs_version,
            donors_all: Arc::new(donors_all),
            donors_by_species: Arc::new(donors_by_species),
        })
    }

    /// Drain env `i`'s accumulated PS protocol lines (empty unless constructed with
    /// `capture_protocol=True`). Call after `step_all` to log/replay the turns just resolved;
    /// pair with `harness/make-replay.mjs` for a replay HTML.
    fn protocol_log(&mut self, env: usize) -> PyResult<Vec<String>> {
        let n = self.flows.len();
        let f = self
            .flows
            .get_mut(env)
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err(format!("env {env} out of range (num_envs={n})")))?;
        Ok(f.take_protocol_log())
    }

    /// Number of teams in the loaded pool (0 when running the fixed default matchup).
    #[getter]
    fn pool_size(&self) -> usize {
        self.pool.as_ref().map(|p| p.len()).unwrap_or(0)
    }

    /// Spot-check: env `i`'s current TRUE state as a PS `deserializeBattle`-loadable snapshot
    /// (the certified exporter). Drop it into pinned Showdown to inspect the position live.
    #[pyo3(signature = (env, seed = None))]
    fn export_state(&self, env: usize, seed: Option<Vec<u16>>) -> PyResult<String> {
        let f = self.flows.get(env).ok_or_else(|| {
            pyo3::exceptions::PyIndexError::new_err(format!("env {env} out of range (num_envs={})", self.flows.len()))
        })?;
        let s = seed.and_then(|v| <[u16; 4]>::try_from(v).ok()).unwrap_or([1, 2, 3, 4]);
        Ok(cosim::export::export_state(&f.state, s).to_string())
    }

    #[getter]
    fn num_envs(&self) -> usize {
        self.flows.len()
    }
    #[getter]
    fn obs_dim(&self) -> usize {
        obs_dim_v(self.obs_version)
    }
    #[getter]
    fn n_actions(&self) -> usize {
        N_ACTIONS_FLOW
    }
    #[getter]
    fn id_dim(&self) -> usize {
        engine::encode::ID_DIM
    }

    /// (N,) i8 phase: 0=Turn, 1=Replace, 2=PivotLanding, 3=Revive (Revival Blessing). Terminal (4)
    /// is never observed when `step_all` auto-resets.
    fn phase_all<'py>(&self, py: Python<'py>) -> Bound<'py, PyArray1<i8>> {
        let v: Vec<i8> = self
            .flows
            .iter()
            .map(|f| match f.request() {
                Request::Turn => 0,
                Request::Replace { .. } => 1,
                Request::PivotLanding { .. } => 2,
                Request::Revive { .. } => 3,
                Request::Terminal { .. } => 4,
            })
            .collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// (N,) bool: whether `side` must act for the current pending request.
    fn acting_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray1<bool>> {
        let sd = sid(side);
        let v: Vec<bool> = self.flows.iter().map(|f| acting_for(f.request(), sd)).collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// (N, 13) bool phase-aware legal mask for `side`.
    fn legal_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<bool>> {
        let sd = sid(side);
        let n = self.flows.len();
        let mut flat = vec![false; n * N_ACTIONS_FLOW];
        for (dst, f) in flat.chunks_mut(N_ACTIONS_FLOW).zip(self.flows.iter()) {
            dst.copy_from_slice(&flow_legal_mask(&f.state, f.request(), sd));
        }
        Array2::from_shape_vec((n, N_ACTIONS_FLOW), flat).unwrap().into_pyarray_bound(py)
    }

    /// One-ply joint-action lookahead for env `e` from `side`'s perspective (EXPLORATION_PLAN
    /// E2). Only valid at a Turn request (both sides acting). For every joint pair
    /// (a_self, a_opp) in the 13×13 action grid with both actions legal, a CLONE of the battle
    /// is advanced one request with an rng derived from `seed` and the pair index, and the
    /// successor is returned encoded from `side`'s view.
    ///
    /// Returns `(obs [169, OBS_DIM], ids [169, ID_DIM], done [169], outcome [169], valid [169])`
    /// — row `a_self * 13 + a_opp`; `outcome` is ±1/0 from `side`'s view when `done`, else 0;
    /// invalid pairs are zero rows. Call again with a different `seed` for a fresh stochastic
    /// draw per pair (the caller averages samples).
    #[pyo3(signature = (env, side, seed, det_seed = None))]
    fn lookahead_obs<'py>(
        &self,
        py: Python<'py>,
        env: usize,
        side: u8,
        seed: u64,
        det_seed: Option<u64>,
    ) -> PyResult<(
        Bound<'py, PyArray2<f32>>,
        Bound<'py, PyArray2<i64>>,
        Bound<'py, PyArray1<bool>>,
        Bound<'py, PyArray1<f32>>,
        Bound<'py, PyArray1<bool>>,
    )> {
        let f = self
            .flows
            .get(env)
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("env out of range"))?;
        if !matches!(f.request(), Request::Turn) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "lookahead_obs is only valid at a Turn request",
            ));
        }
        let me = sid(side);
        // det_seed: search under the viewer's INFORMATION SET — the foe's hidden attributes are
        // resampled from the pool (W8 realistic search) instead of read from the true state.
        let mut base = f.clone();
        if let Some(ds) = det_seed {
            let mut z = ds;
            base.state = determinize_foe(&f.state, me, &self.donors_all, &self.donors_by_species, &mut z);
        }
        let f = &base;
        let ver = self.obs_version;
        let my_mask = flow_legal_mask(&f.state, f.request(), me);
        let opp_mask = flow_legal_mask(&f.state, f.request(), me.other());
        const P: usize = N_ACTIONS_FLOW * N_ACTIONS_FLOW;
        let obs_dim = obs_dim_v(ver);
        let id_dim = engine::encode::ID_DIM;
        let mut obs = vec![0f32; P * obs_dim];
        let mut ids = vec![0i64; P * id_dim];
        let mut done = vec![false; P];
        let mut outcome = vec![0f32; P];
        let mut valid = vec![false; P];
        py.allow_threads(|| {
            obs.par_chunks_mut(obs_dim)
                .zip(ids.par_chunks_mut(id_dim))
                .zip(done.par_iter_mut())
                .zip(outcome.par_iter_mut())
                .zip(valid.par_iter_mut())
                .enumerate()
                .for_each(|(p, ((((o, idr), d), out), v))| {
                    let (ai, aj) = (p / N_ACTIONS_FLOW, p % N_ACTIONS_FLOW);
                    if !my_mask[ai] || !opp_mask[aj] {
                        return;
                    }
                    let mut sim = f.clone();
                    // Distinct, deterministic stream per (seed, pair) — splitmix64 mix.
                    let mut z = seed ^ (0x9E37_79B9_7F4A_7C15u64.wrapping_mul(p as u64 + 1));
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    sim.rng = z ^ (z >> 27);
                    let c_me = flow_choice(&sim.state, me, ai as u8);
                    let c_opp = flow_choice(&sim.state, me.other(), aj as u8);
                    let (c0, c1) = if me == SideId::One { (c_me, c_opp) } else { (c_opp, c_me) };
                    let next = sim.submit([Some(c0), Some(c1)]);
                    o.copy_from_slice(&enc_v(ver, &sim.state, me));
                    idr.copy_from_slice(&engine::encode::encode_ids(&sim.state, me));
                    if let Request::Terminal { winner } = next {
                        *d = true;
                        *out = if winner < 0 {
                            0.0
                        } else if winner == me.index() as i64 {
                            1.0
                        } else {
                            -1.0
                        };
                    }
                    *v = true;
                });
        });
        Ok((
            Array2::from_shape_vec((P, obs_dim), obs).unwrap().into_pyarray_bound(py),
            Array2::from_shape_vec((P, id_dim), ids).unwrap().into_pyarray_bound(py),
            Array1::from_vec(done).into_pyarray_bound(py),
            Array1::from_vec(outcome).into_pyarray_bound(py),
            Array1::from_vec(valid).into_pyarray_bound(py),
        ))
    }

    /// Depth-2 support (EXPLORATION_PLAN W7): advance a CLONE by the root joint pair
    /// (`a_self`, `a_opp`), then expand the SUCCESSOR. Three shapes, tagged by `kind`:
    ///   0 = terminal   — `obs[0]` unused, `outcome[0]` is the ±1/0 result
    ///   1 = leaf       — successor is not a Turn (single-sided pause/replace): `obs[0]`/`ids[0]`
    ///                    hold the successor encoded from `side`'s view
    ///   2 = expanded   — successor is a Turn: rows are ITS 13×13 joint grid, exactly like
    ///                    [`lookahead_obs`] (invalid pairs zero, `valid` flags set)
    /// The caller solves the child matrix game (kind 2) and backs its value up to the root —
    /// depth-limited subgame solving with equilibrium backups at simultaneous nodes.
    #[allow(clippy::type_complexity)]
    #[pyo3(signature = (env, side, seed, a_self, a_opp, det_seed = None))]
    fn lookahead_pair_obs<'py>(
        &self,
        py: Python<'py>,
        env: usize,
        side: u8,
        seed: u64,
        a_self: u8,
        a_opp: u8,
        det_seed: Option<u64>,
    ) -> PyResult<(
        u8,
        Bound<'py, PyArray2<f32>>,
        Bound<'py, PyArray2<i64>>,
        Bound<'py, PyArray1<bool>>,
        Bound<'py, PyArray1<f32>>,
        Bound<'py, PyArray1<bool>>,
    )> {
        let f = self
            .flows
            .get(env)
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("env out of range"))?;
        if !matches!(f.request(), Request::Turn) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "lookahead_pair_obs is only valid at a Turn request",
            ));
        }
        let me = sid(side);
        let mut root = f.clone();
        if let Some(ds) = det_seed {
            let mut dz = ds;
            root.state = determinize_foe(&f.state, me, &self.donors_all, &self.donors_by_species, &mut dz);
        }
        let mut z = seed ^ 0xD1B5_4A32_D192_ED03u64;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        root.rng = z ^ (z >> 27);
        let c_me = flow_choice(&root.state, me, a_self);
        let c_opp = flow_choice(&root.state, me.other(), a_opp);
        let (c0, c1) = if me == SideId::One { (c_me, c_opp) } else { (c_opp, c_me) };
        let next = root.submit([Some(c0), Some(c1)]);

        let ver = self.obs_version;
        let obs_dim = obs_dim_v(ver);
        let id_dim = engine::encode::ID_DIM;
        const P: usize = N_ACTIONS_FLOW * N_ACTIONS_FLOW;
        let pack = |kind: u8,
                    obs: Vec<f32>,
                    ids: Vec<i64>,
                    done: Vec<bool>,
                    outcome: Vec<f32>,
                    valid: Vec<bool>,
                    rows: usize| {
            Ok((
                kind,
                Array2::from_shape_vec((rows, obs_dim), obs).unwrap().into_pyarray_bound(py),
                Array2::from_shape_vec((rows, id_dim), ids).unwrap().into_pyarray_bound(py),
                Array1::from_vec(done).into_pyarray_bound(py),
                Array1::from_vec(outcome).into_pyarray_bound(py),
                Array1::from_vec(valid).into_pyarray_bound(py),
            ))
        };

        match next {
            Request::Terminal { winner } => {
                let out = if winner < 0 {
                    0.0
                } else if winner == me.index() as i64 {
                    1.0
                } else {
                    -1.0
                };
                pack(0, vec![0.0; obs_dim], vec![0; id_dim], vec![true], vec![out], vec![true], 1)
            }
            Request::Turn => {
                let my_mask = flow_legal_mask(&root.state, Request::Turn, me);
                let opp_mask = flow_legal_mask(&root.state, Request::Turn, me.other());
                let mut obs = vec![0f32; P * obs_dim];
                let mut ids = vec![0i64; P * id_dim];
                let mut done = vec![false; P];
                let mut outcome = vec![0f32; P];
                let mut valid = vec![false; P];
                py.allow_threads(|| {
                    obs.par_chunks_mut(obs_dim)
                        .zip(ids.par_chunks_mut(id_dim))
                        .zip(done.par_iter_mut())
                        .zip(outcome.par_iter_mut())
                        .zip(valid.par_iter_mut())
                        .enumerate()
                        .for_each(|(p, ((((o, idr), d), out), v))| {
                            let (ai, aj) = (p / N_ACTIONS_FLOW, p % N_ACTIONS_FLOW);
                            if !my_mask[ai] || !opp_mask[aj] {
                                return;
                            }
                            let mut sim = root.clone();
                            let mut z2 =
                                seed ^ (0x9E37_79B9_7F4A_7C15u64.wrapping_mul(p as u64 + 7));
                            z2 = (z2 ^ (z2 >> 30)).wrapping_mul(0x94D0_49BB_1331_11EB);
                            sim.rng = z2 ^ (z2 >> 27);
                            let m = flow_choice(&sim.state, me, ai as u8);
                            let o2 = flow_choice(&sim.state, me.other(), aj as u8);
                            let (cc0, cc1) =
                                if me == SideId::One { (m, o2) } else { (o2, m) };
                            let nn = sim.submit([Some(cc0), Some(cc1)]);
                            o.copy_from_slice(&enc_v(ver, &sim.state, me));
                            idr.copy_from_slice(&engine::encode::encode_ids(&sim.state, me));
                            if let Request::Terminal { winner } = nn {
                                *d = true;
                                *out = if winner < 0 {
                                    0.0
                                } else if winner == me.index() as i64 {
                                    1.0
                                } else {
                                    -1.0
                                };
                            }
                            *v = true;
                        });
                });
                pack(2, obs, ids, done, outcome, valid, P)
            }
            _ => {
                // Single-sided pause (pivot landing / replace / revive): evaluate as a leaf.
                let o = enc_v(ver, &root.state, me);
                let i = engine::encode::encode_ids(&root.state, me).to_vec();
                pack(1, o, i, vec![false], vec![0.0], vec![true], 1)
            }
        }
    }

    /// `state_json` of env `e` as seen from `side`'s INFORMATION SET: `State::observe` with
    /// species fog forced on (regardless of the env's own flag). For building public-info
    /// referees — e.g. the fog-heuristic that answers "how much of the heuristic's strength is
    /// perfect information?"
    fn state_json_observed(&self, env: usize, side: u8) -> PyResult<String> {
        let f = self
            .flows
            .get(env)
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("env out of range"))?;
        let mut st = f.state;
        st.fog_species = true;
        Ok(state_json_of(&st.observe(sid(side))))
    }

    /// Batched depth-2 expansion: ALL root joint pairs of one env in a single call (16 bridge
    /// crossings become 1), under ONE shared determinization — matrix entries must be scored in
    /// the same sampled world, which the per-pair API couldn't guarantee. Returns per pair p:
    /// `kinds[p]` (0 terminal / 1 leaf / 2 expanded), and P=169 rows each of obs/ids/done/
    /// outcome/valid stacked at `p*169` (kind 0/1 use row 0 only, like `lookahead_pair_obs`).
    #[allow(clippy::type_complexity)]
    #[pyo3(signature = (env, side, seed, a_selfs, a_opps, det_seed = None, child_whitelist = None))]
    fn lookahead_pairs_env<'py>(
        &self,
        py: Python<'py>,
        env: usize,
        side: u8,
        seed: u64,
        a_selfs: Vec<u8>,
        a_opps: Vec<u8>,
        det_seed: Option<u64>,
        child_whitelist: Option<Vec<u8>>,
    ) -> PyResult<(
        Bound<'py, PyArray1<u8>>,
        Bound<'py, PyArray2<f32>>,
        Bound<'py, PyArray2<i64>>,
        Bound<'py, PyArray1<bool>>,
        Bound<'py, PyArray1<f32>>,
        Bound<'py, PyArray1<bool>>,
    )> {
        let f = self
            .flows
            .get(env)
            .ok_or_else(|| pyo3::exceptions::PyIndexError::new_err("env out of range"))?;
        if !matches!(f.request(), Request::Turn) {
            return Err(pyo3::exceptions::PyValueError::new_err(
                "lookahead_pairs_env is only valid at a Turn request",
            ));
        }
        if a_selfs.len() != a_opps.len() {
            return Err(pyo3::exceptions::PyValueError::new_err("pair arrays must match"));
        }
        let me = sid(side);
        let mut world = f.clone();
        if let Some(ds) = det_seed {
            let mut dz = ds;
            world.state =
                determinize_foe(&f.state, me, &self.donors_all, &self.donors_by_species, &mut dz);
        }
        let ver = self.obs_version;
        let obs_dim = obs_dim_v(ver);
        let id_dim = engine::encode::ID_DIM;
        // Row stride per pair: the full 13x13 grid, or |whitelist|^2 in compact (pruned) mode —
        // the fixed-stride zero-fill was ~1/3 of wall time at 169 rows per pair.
        let wl: Option<Vec<u8>> = child_whitelist;
        let stride: usize = match &wl {
            Some(w) => (w.len() * w.len()).max(1),
            None => N_ACTIONS_FLOW * N_ACTIONS_FLOW,
        };
        let m = a_selfs.len();
        let mut kinds = vec![0u8; m];
        let mut obs = vec![0f32; m * stride * obs_dim];
        let mut ids = vec![0i64; m * stride * id_dim];
        let mut done = vec![false; m * stride];
        let mut outcome = vec![0f32; m * stride];
        let mut valid = vec![false; m * stride];
        py.allow_threads(|| {
            kinds
                .par_iter_mut()
                .zip(obs.par_chunks_mut(stride * obs_dim))
                .zip(ids.par_chunks_mut(stride * id_dim))
                .zip(done.par_chunks_mut(stride))
                .zip(outcome.par_chunks_mut(stride))
                .zip(valid.par_chunks_mut(stride))
                .enumerate()
                .for_each(|(pi, (((((kind, o_c), i_c), d_c), out_c), v_c))| {
                    let mut root = world.clone();
                    let mut z = seed
                        ^ (0xD1B5_4A32_D192_ED03u64.wrapping_mul(pi as u64 + 1));
                    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
                    root.rng = z ^ (z >> 27);
                    let c_me = flow_choice(&root.state, me, a_selfs[pi]);
                    let c_op = flow_choice(&root.state, me.other(), a_opps[pi]);
                    let (c0, c1) = if me == SideId::One { (c_me, c_op) } else { (c_op, c_me) };
                    let next = root.submit([Some(c0), Some(c1)]);
                    match next {
                        Request::Terminal { winner } => {
                            *kind = 0;
                            d_c[0] = true;
                            out_c[0] = if winner < 0 {
                                0.0
                            } else if winner == me.index() as i64 {
                                1.0
                            } else {
                                -1.0
                            };
                            v_c[0] = true;
                        }
                        Request::Turn => {
                            *kind = 2;
                            let my_mask = flow_legal_mask(&root.state, Request::Turn, me);
                            let op_mask =
                                flow_legal_mask(&root.state, Request::Turn, me.other());
                            // Child grid: compact wl x wl in pruned mode, full 13x13 otherwise.
                            let full: Vec<u8> = (0..N_ACTIONS_FLOW as u8).collect();
                            let axis: &[u8] = match &wl {
                                Some(w) => w.as_slice(),
                                None => full.as_slice(),
                            };
                            let n_ax = axis.len();
                            for p in 0..stride.min(n_ax * n_ax) {
                                let ai = axis[p / n_ax] as usize;
                                let aj = axis[p % n_ax] as usize;
                                if !my_mask[ai] || !op_mask[aj] {
                                    continue;
                                }
                                let mut sim = root.clone();
                                let mut z2 = seed
                                    ^ (0x9E37_79B9_7F4A_7C15u64
                                        .wrapping_mul((pi * 4096 + p) as u64 + 7));
                                z2 = (z2 ^ (z2 >> 30)).wrapping_mul(0x94D0_49BB_1331_11EB);
                                sim.rng = z2 ^ (z2 >> 27);
                                let mm = flow_choice(&sim.state, me, ai as u8);
                                let oo = flow_choice(&sim.state, me.other(), aj as u8);
                                let (cc0, cc1) =
                                    if me == SideId::One { (mm, oo) } else { (oo, mm) };
                                let nn = sim.submit([Some(cc0), Some(cc1)]);
                                o_c[p * obs_dim..(p + 1) * obs_dim]
                                    .copy_from_slice(&enc_v(ver, &sim.state, me));
                                i_c[p * id_dim..(p + 1) * id_dim]
                                    .copy_from_slice(&engine::encode::encode_ids(&sim.state, me));
                                if let Request::Terminal { winner } = nn {
                                    d_c[p] = true;
                                    out_c[p] = if winner < 0 {
                                        0.0
                                    } else if winner == me.index() as i64 {
                                        1.0
                                    } else {
                                        -1.0
                                    };
                                }
                                v_c[p] = true;
                            }
                        }
                        _ => {
                            *kind = 1;
                            o_c[..obs_dim].copy_from_slice(&enc_v(ver, &root.state, me));
                            i_c[..id_dim]
                                .copy_from_slice(&engine::encode::encode_ids(&root.state, me));
                            v_c[0] = true;
                        }
                    }
                });
        });
        Ok((
            Array1::from_vec(kinds).into_pyarray_bound(py),
            Array2::from_shape_vec((m * stride, obs_dim), obs).unwrap().into_pyarray_bound(py),
            Array2::from_shape_vec((m * stride, id_dim), ids).unwrap().into_pyarray_bound(py),
            Array1::from_vec(done).into_pyarray_bound(py),
            Array1::from_vec(outcome).into_pyarray_bound(py),
            Array1::from_vec(valid).into_pyarray_bound(py),
        ))
    }

    /// Multi-turn-pattern potential features for PBRS shaping, from `side`'s perspective:
    /// `[my positive boost stages, foe-side hazard layers, my screens/tailwind up, statused
    /// foe mons]` — each a STATE function, so any weighted sum is a valid PBRS potential.
    fn phi_features_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<f32>> {
        let me = sid(side);
        let n = self.flows.len();
        let mut flat = vec![0f32; n * 4];
        for (i, f) in self.flows.iter().enumerate() {
            let sd = f.state.side(me);
            let od = f.state.side(me.other());
            let row = &mut flat[i * 4..(i + 1) * 4];
            row[0] = sd.boosts.iter().take(5).map(|&b| b.max(0) as f32).sum();
            let sc = &od.side_conditions;
            row[1] = sc.stealth_rock as u8 as f32 + sc.spikes as f32 + sc.toxic_spikes as f32
                + sc.sticky_web as u8 as f32;
            let mc = &sd.side_conditions;
            row[2] = (mc.reflect > 0) as u8 as f32 + (mc.light_screen > 0) as u8 as f32
                + (mc.aurora_veil > 0) as u8 as f32 + (mc.tailwind > 0) as u8 as f32;
            row[3] = od
                .pokemon
                .iter()
                .filter(|p| {
                    p.species != engine::ids::Species::None
                        && p.is_alive()
                        && p.status != engine::ids::Status::None
                })
                .count() as f32;
        }
        Array2::from_shape_vec((n, 4), flat).unwrap().into_pyarray_bound(py)
    }

    /// Belief-head training targets (EXPLORATION_PLAN W9), from the TRUE state — free labels.
    /// Per env, about the FOE of `side`: `targets [N, 11]` = [species of party slot 0..5,
    /// active item, active move slot 0..3] as embedding ids; `mask [N, 11]` = 1 where the entry
    /// is HIDDEN from `side` (a real prediction target) and present, 0 where revealed/absent.
    fn belief_targets_all<'py>(
        &self,
        py: Python<'py>,
        side: u8,
    ) -> (Bound<'py, PyArray2<i64>>, Bound<'py, PyArray2<f32>>) {
        use engine::state::Reveal;
        const K: usize = 11;
        let n = self.flows.len();
        let me = sid(side);
        let mut tgt = vec![0i64; n * K];
        let mut msk = vec![0f32; n * K];
        for (i, f) in self.flows.iter().enumerate() {
            let foe = f.state.side(me.other());
            let t = &mut tgt[i * K..(i + 1) * K];
            let m = &mut msk[i * K..(i + 1) * K];
            for slot in 0..6 {
                let p = &foe.pokemon[slot];
                if p.species == engine::ids::Species::None {
                    continue;
                }
                t[slot] = p.species.0 as i64;
                m[slot] = if p.reveal.has(Reveal::SPECIES) { 0.0 } else { 1.0 };
            }
            let ai = foe.active_index as usize;
            if ai < 6 {
                let p = &foe.pokemon[ai];
                t[6] = p.item as i64;
                m[6] = if p.reveal.has(Reveal::ITEM) { 0.0 } else { 1.0 };
                for mv in 0..4usize {
                    let ms = p.moves[mv];
                    if ms.id == engine::ids::MoveId::None {
                        continue;
                    }
                    t[7 + mv] = ms.id.0 as i64;
                    m[7 + mv] = if p.reveal.move_seen(mv as u8) { 0.0 } else { 1.0 };
                }
            }
        }
        (
            Array2::from_shape_vec((n, K), tgt).unwrap().into_pyarray_bound(py),
            Array2::from_shape_vec((n, K), msk).unwrap().into_pyarray_bound(py),
        )
    }

    /// (N,) scripted-heuristic action for every env, from `sides[i]`'s perspective (0/1).
    /// -1 where the heuristic has no opinion (non-acting request, or a state the Python
    /// implementation would raise on) — see [`heuristic_action_of`]. The Python-side wrapper
    /// falls back to a random legal action there and counts the event.
    fn heuristic_actions_all<'py>(
        &self,
        py: Python<'py>,
        sides: PyReadonlyArray1<'py, i64>,
    ) -> PyResult<Bound<'py, PyArray1<i64>>> {
        let sv = sides.as_slice()?.to_vec();
        let n = self.flows.len();
        if sv.len() != n {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "sides must have length {n} (got {})",
                sv.len()
            )));
        }
        let acts: Vec<i64> = py.allow_threads(|| {
            self.flows
                .par_iter()
                .zip(sv.par_iter())
                .map(|(f, &s)| heuristic_action_of(&f.state, f.request(), sid(s as u8)))
                .collect()
        });
        Ok(Array1::from_vec(acts).into_pyarray_bound(py))
    }

    /// (N, obs_dim) f32 observations from `side`'s perspective (layout per `obs_version`).
    fn observe_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<f32>> {
        let n = self.flows.len();
        let ver = self.obs_version;
        let dim = obs_dim_v(ver);
        let mut flat = vec![0f32; n * dim];
        py.allow_threads(|| {
            flat.par_chunks_mut(dim).zip(self.flows.par_iter()).for_each(|(dst, f)| {
                dst.copy_from_slice(&enc_v(ver, &f.state, sid(side)));
            });
        });
        Array2::from_shape_vec((n, dim), flat).unwrap().into_pyarray_bound(py)
    }

    /// (N, ID_DIM) i64 categorical IDs from `side`'s perspective (embedding-table inputs).
    fn observe_ids_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray2<i64>> {
        let n = self.flows.len();
        let dim = engine::encode::ID_DIM;
        let mut flat = vec![0i64; n * dim];
        py.allow_threads(|| {
            flat.par_chunks_mut(dim).zip(self.flows.par_iter()).for_each(|(dst, f)| {
                dst.copy_from_slice(&engine::encode::encode_ids(&f.state, sid(side)));
            });
        });
        Array2::from_shape_vec((n, dim), flat).unwrap().into_pyarray_bound(py)
    }

    /// (N,) f32 mean team HP fraction for `side` — the reward-shaping potential.
    fn team_hp_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray1<f32>> {
        let v: Vec<f32> = self
            .flows
            .iter()
            .map(|f| {
                let s = f.state.side(sid(side));
                s.pokemon
                    .iter()
                    .filter(|p| p.species != engine::ids::Species::None && p.max_hp > 0)
                    .map(|p| (p.hp.max(0) as f32) / (p.max_hp as f32))
                    .sum::<f32>()
                    / 6.0
            })
            .collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// Env `i`'s full *true* state as JSON — the input the scripted heuristic baseline reads.
    /// Same mapping `Battle.state_json` uses (see [`state_json_of`]).
    fn state_json(&self, env: usize) -> PyResult<String> {
        let f = self.flows.get(env).ok_or_else(|| {
            pyo3::exceptions::PyIndexError::new_err(format!("env {env} out of range (num_envs={})", self.flows.len()))
        })?;
        Ok(state_json_of(&f.state))
    }

    /// The **PS choice string** env `i`'s `action` resolves to for `side`, against the snapshot
    /// `export_state` would emit right now ("move 2", "move 1 terastallize", "switch 3").
    ///
    /// The on-policy cosim sidecar replays engine decisions inside real Showdown, and the two
    /// sides index switches differently: the engine's action `4..=8` picks the k-th *bench party
    /// slot*, while a PS `switch N` indexes the exported **active-first** array. Resolving here,
    /// off the same state the exporter serializes, makes them agree by construction instead of
    /// by two copies of the same off-by-ordering reasoning.
    fn choice_str(&self, env: usize, side: u8, action: i64) -> PyResult<String> {
        let f = self.flows.get(env).ok_or_else(|| {
            pyo3::exceptions::PyIndexError::new_err(format!("env {env} out of range (num_envs={})", self.flows.len()))
        })?;
        let sd = sid(side);
        Ok(match flow_choice(&f.state, sd, action as u8) {
            PlayerChoice::Move { slot, tera } => {
                format!("move {}{}", slot + 1, if tera { " terastallize" } else { "" })
            }
            PlayerChoice::Switch { slot } => {
                format!("switch {}", cosim::export::array_index_of(&f.state, sd.index(), slot as usize) + 1)
            }
        })
    }

    /// (N,) i64 fainted-mon count for `side` — the other Φ term.
    fn faints_all<'py>(&self, py: Python<'py>, side: u8) -> Bound<'py, PyArray1<i64>> {
        let v: Vec<i64> = self
            .flows
            .iter()
            .map(|f| {
                f.state
                    .side(sid(side))
                    .pokemon
                    .iter()
                    .filter(|p| p.species != engine::ids::Species::None && !p.is_alive())
                    .count() as i64
            })
            .collect();
        Array1::from_vec(v).into_pyarray_bound(py)
    }

    /// Answer the pending request in every env. A side contributes a choice iff it is acting for
    /// that env's request (else `None`). Returns `(done, winner)` for the step just taken; envs
    /// that reached Terminal (or hit `max_requests`, truncation with winner -1) are reset in place
    /// when `auto_reset`.
    #[pyo3(signature = (action_red, action_blue, auto_reset = true))]
    fn step_all<'py>(
        &mut self,
        py: Python<'py>,
        action_red: PyReadonlyArray1<'py, i64>,
        action_blue: PyReadonlyArray1<'py, i64>,
        auto_reset: bool,
    ) -> PyResult<(Bound<'py, PyArray1<bool>>, Bound<'py, PyArray1<i64>>)> {
        let red = action_red.as_slice()?.to_vec();
        let blue = action_blue.as_slice()?.to_vec();
        let n = self.flows.len();
        if red.len() != n || blue.len() != n {
            return Err(pyo3::exceptions::PyValueError::new_err(format!(
                "action arrays must have length {n} (got {} / {})",
                red.len(),
                blue.len()
            )));
        }
        let max_requests = self.max_requests;
        let seed = self.seed;
        let pool = &self.pool;
        let capture = self.capture_protocol;
        let fog = self.fog_species;
        let (dones, winners): (Vec<bool>, Vec<i64>) = py.allow_threads(|| {
            self.flows
                .par_iter_mut()
                .zip(self.reqs.par_iter_mut())
                .zip(self.draw_rngs.par_iter_mut())
                .enumerate()
                .map(|(i, ((flow, reqs), draw_rng))| {
                    let req = flow.request();
                    let c0 = if acting_for(req, SideId::One) {
                        Some(flow_choice(&flow.state, SideId::One, red[i] as u8))
                    } else {
                        None
                    };
                    let c1 = if acting_for(req, SideId::Two) {
                        Some(flow_choice(&flow.state, SideId::Two, blue[i] as u8))
                    } else {
                        None
                    };
                    let next = flow.submit([c0, c1]);
                    *reqs += 1;
                    let (done, winner) = match next {
                        Request::Terminal { winner } => (true, winner),
                        _ if *reqs >= max_requests => (true, -1),
                        _ => (false, -1),
                    };
                    if done && auto_reset {
                        *flow = fresh_flow(pool, draw_rng, flow_seed(seed, i), capture, fog);
                        *reqs = 0;
                    }
                    (done, winner)
                })
                .unzip()
        });
        Ok((
            Array1::from_vec(dones).into_pyarray_bound(py),
            Array1::from_vec(winners).into_pyarray_bound(py),
        ))
    }
}

/// Encode a **PS serialized battle state** the way the training env would see it.
///
/// This is the hinge of the deterministic on-policy sidecar. There, Showdown drives the battle
/// from a seed and our policy supplies the choices, so the policy has to act on a *PS* state —
/// but it was trained on `encode()` of an engine `State`. This runs the certified
/// `convert_state` (the same converter every parity gate uses) and then the same encoder, so
/// the policy sees byte-identical inputs to training.
///
/// `state_json` is one snapshot in the recorder's projection (`harness/cosim.mjs`'s `snapshot`).
/// Returns `(obs, ids, legal_mask, roster_of_action)` for `side`, where `roster_of_action[a]` is
/// the battle-start roster index a switch action targets (`-1` for move actions) — the caller
/// translates that into PS's live array position, which is the only place the two orderings meet.
#[pyfunction]
fn encode_ps_state(
    py: Python<'_>,
    state_json: &str,
    side: u8,
    format: &str,
    request_state: &str,
) -> PyResult<(Vec<f32>, Vec<i64>, Vec<bool>, Vec<i64>)> {
    let v: serde_json::Value = serde_json::from_str(state_json)
        .map_err(|e| pyo3::exceptions::PyValueError::new_err(format!("state json: {e}")))?;
    let canon = cosim::convert::Canonical::from_first_state(&v)
        .map_err(|u| pyo3::exceptions::PyValueError::new_err(format!("canonical: {}", u.0)))?;
    let mut state = cosim::convert::convert_state(&v, &canon)
        .map_err(|u| pyo3::exceptions::PyValueError::new_err(format!("convert: {}", u.0)))?;
    // Ruleset selection. "gen9randombattle" is AMBIGUOUS at this boundary: every recording and
    // the whole training pipeline to date labels battles with the *team* format while actually
    // running them as gen9customgame (pre-generated teams, no ruleset) — so that spelling keeps
    // its historical meaning and maps to the customgame ruleset. The REAL ladder ruleset (sleep
    // clause, 16-bit truncation arm, maybeTrapped, percent HP) is an explicit opt-in via
    // "gen9randombattle-real", matching recordings whose trace `ruleset` field says
    // gen9randombattle (harness/seed-fixtures-rb/). Migrating the default is a deliberate
    // future step, to be taken together with the sidecar's recordings.
    state.ruleset = match format {
        f if f.contains("random") && !f.ends_with("-real") => {
            engine::ruleset::Ruleset::GEN9_CUSTOM_GAME
        }
        f => engine::ruleset::Ruleset::from_format(f.trim_end_matches("-real"))
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err(format!("unknown format: {f}")))?,
    };

    let sd = sid(side);
    let _ = py;
    // At a forced-switch request PS has already taken the fainted mon off the field, so `convert`
    // records `active_index = u8::MAX` — a board the encoder cannot describe ("my active" has no
    // referent) and `Side::active()` panics on. The engine's OWN replacement state keeps the
    // fainted mon as the active until the replacement enters, so restore that shape: adopt the
    // fainted party member as the active. This is the state the policy was trained on at a
    // replacement decision, and the only thing it is used for here is choosing who comes in.
    for s in [SideId::One, SideId::Two] {
        let sref = state.side_mut(s);
        if sref.active_index == u8::MAX {
            let fainted = (0..6u8).find(|&i| {
                let p = &sref.pokemon[i as usize];
                p.species != engine::ids::Species::None && !p.is_alive()
            });
            // Nothing fainted and still no active means an empty side (battle already decided);
            // slot 0 keeps the encoder total without claiming anything about the position.
            sref.active_index = fainted.unwrap_or(0);
        }
    }
    // A serialized PS state carries no pending-request marker (`requestState` is not part of
    // serializeBattle's modeled output), so the caller passes PS's own request phase in. It
    // matters beyond legality: at a forced switch the fainted mon is off the field, `convert`
    // sets `active_index = u8::MAX`, and the `Turn` arm's `s.active()` indexes past the party
    // (panic: "len is 6 but the index is 255").
    let phase = match request_state {
        "switch" => Request::Replace { sides: [true, true] },
        _ => Request::Turn,
    };
    // Belt and braces: a turn-phase state whose active is genuinely absent is still unsafe to
    // read as `Turn`, whatever PS called the request.
    let phase = if state.side(sd).active_index == u8::MAX {
        Request::Replace { sides: [true, true] }
    } else {
        phase
    };
    let mask = flow_legal_mask(&state, phase, sd);
    let mut roster = vec![-1i64; N_ACTIONS_FLOW];
    for k in 0..5usize {
        roster[4 + k] = bench_party_slot(&state, sd, k) as i64;
    }
    Ok((
        engine::encode::encode(&state, sd).to_vec(),
        engine::encode::encode_ids(&state, sd).to_vec(),
        mask.to_vec(),
        roster,
    ))
}

#[pymodule]
fn showdown_engine(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Battle>()?;
    m.add_class::<BattleVec>()?;
    m.add_class::<FlowVec>()?;
    m.add_function(wrap_pyfunction!(encode_ps_state, m)?)?;
    Ok(())
}

// ---- pool loader validation (equivalence-relevant) ------------------------------------------

#[cfg(test)]
mod pool_tests {
    use super::*;
    use engine::request::{Flow, PlayerChoice, Request};

    fn pool_path() -> String {
        format!(
            "{}/../../harness/team-pool/gen9randombattle-2k.jsonl.gz",
            env!("CARGO_MANIFEST_DIR")
        )
    }

    /// Every committed pool team must resolve (no unknown ids), build with real stats, and carry
    /// 6 real mons with moves. `load_pool` already resolves every id loudly — a single unknown
    /// species/move/ability/item/type/nature id fails the load here.
    #[test]
    fn every_committed_pool_team_builds_and_resolves() {
        let pool = load_pool(&pool_path()).expect("2k pool loads and every id resolves");
        assert!(pool.len() >= 2000, "expected >= 2000 teams, got {}", pool.len());
        for (ti, team) in pool.iter().enumerate() {
            for (mi, m) in team.iter().enumerate() {
                assert_ne!(m.species, Species::None, "team {ti} member {mi}: empty species");
                assert!(m.level > 0, "team {ti} member {mi}: zero level");
                let p = engine::team::build_member_resolved(m);
                assert!(
                    p.max_hp > 0,
                    "team {ti} member {mi} ({}): computed 0 HP",
                    m.species.to_id()
                );
                assert!(
                    p.moves.iter().any(|mv| mv.id != MoveId::None),
                    "team {ti} member {mi} ({}): no moves",
                    m.species.to_id()
                );
            }
        }
    }

    /// Pool teams must be playable: assemble a battle and drive several decision points with the
    /// first legal action for each acting side, without panicking.
    #[test]
    fn pool_teams_are_playable() {
        let pool = load_pool(&pool_path()).unwrap();
        for pair in pool.chunks(2).take(300) {
            if pair.len() < 2 {
                break;
            }
            let state = engine::team::build_state_resolved(&pair[0], &pair[1]);
            let mut flow = Flow::new(state, 0x1234_5678);
            for _ in 0..16 {
                let req = flow.request();
                if let Request::Terminal { .. } = req {
                    break;
                }
                let c0 = first_legal_choice(&flow.state, req, SideId::One);
                let c1 = first_legal_choice(&flow.state, req, SideId::Two);
                flow.submit([c0, c1]);
            }
        }
    }

    fn first_legal_choice(state: &State, req: Request, side: SideId) -> Option<PlayerChoice> {
        if !acting_for(req, side) {
            return None;
        }
        let mask = flow_legal_mask(state, req, side);
        mask.iter()
            .position(|&ok| ok)
            .map(|a| flow_choice(state, side, a as u8))
    }
}
