//! Raw self-play throughput benchmark: one battle stepped with uniform-random legal actions,
//! auto-reset on completion. Single-threaded by design — envs are independent, so the parallel
//! ceiling is ~(this number × physical cores). Reports steps/s for the bare transition and for
//! transition + both-sides observation encode (the training-loop reality).
//!
//!     cargo run --release -p engine --example bench_steps [seconds-per-phase]

use engine::generate::{generate_instructions, MoveChoice};
use engine::instruction::StateInstructions;
use engine::state::{SideId, State};
use engine::team;
use std::time::Instant;

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
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

fn legal_mask(state: &State, side: SideId) -> [bool; 9] {
    let s = state.side(side);
    let active = s.active();
    let mut mask = [false; 9];
    if active.is_alive() {
        for i in 0..4 {
            let m = active.moves[i];
            mask[i] = m.id != engine::ids::MoveId::None && m.pp > 0;
        }
    }
    let mut k = 0;
    for i in 0..6u8 {
        if i != s.active_index {
            let p = &s.pokemon[i as usize];
            mask[4 + k] = p.species != engine::ids::Species::None && p.is_alive();
            k += 1;
        }
    }
    mask
}

fn random_action(rng: &mut Rng, mask: &[bool; 9]) -> Option<u8> {
    let n = mask.iter().filter(|&&b| b).count();
    if n == 0 {
        return None;
    }
    let mut pick = (rng.next_u64() % n as u64) as usize;
    for (i, &ok) in mask.iter().enumerate() {
        if ok {
            if pick == 0 {
                return Some(i as u8);
            }
            pick -= 1;
        }
    }
    None
}

fn choice_for(state: &State, side: SideId, action: u8) -> MoveChoice {
    if action < 4 {
        return MoveChoice::Move(action);
    }
    let s = state.side(side);
    let mut k = action - 4;
    for i in 0..6u8 {
        if i != s.active_index {
            if k == 0 {
                return MoveChoice::Switch(i);
            }
            k -= 1;
        }
    }
    MoveChoice::Move(0)
}

fn sample_idx(rng: &mut Rng, branches: &[StateInstructions]) -> usize {
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

fn battle_over(state: &State) -> bool {
    [SideId::One, SideId::Two].iter().any(|&sd| {
        !state
            .side(sd)
            .pokemon
            .iter()
            .any(|p| p.species != engine::ids::Species::None && p.is_alive())
    })
}

fn run_phase_sampled(seconds: u64) -> (u64, u64) {
    let mut state = team::default_matchup();
    let mut rng = Rng(0xDEADBEEF);
    let mut gen_rng = 0xBADC_0FFEu64;
    let (mut steps, mut episodes) = (0u64, 0u64);
    let start = Instant::now();
    while start.elapsed().as_secs() < seconds {
        for _ in 0..256 {
            let a1 = random_action(&mut rng, &legal_mask(&state, SideId::One)).unwrap_or(0);
            let a2 = random_action(&mut rng, &legal_mask(&state, SideId::Two)).unwrap_or(0);
            let c1 = choice_for(&state, SideId::One, a1);
            let c2 = choice_for(&state, SideId::Two, a2);
            let si = engine::generate::generate_instructions_sampled(
                &state, c1, c2, [None, None], [false, false], &mut gen_rng,
            );
            state.apply_instructions(&si.instructions);
            state.turn += 1;
            steps += 1;
            if branches_done(&state) {
                episodes += 1;
                state = team::default_matchup();
            }
        }
    }
    (steps / start.elapsed().as_secs().max(1), episodes)
}

fn run_phase(seconds: u64, encode: bool) -> (u64, u64) {
    let mut state = team::default_matchup();
    let mut rng = Rng(0xDEADBEEF);
    let (mut steps, mut episodes) = (0u64, 0u64);
    let start = Instant::now();
    while start.elapsed().as_secs() < seconds {
        // batch the clock check
        for _ in 0..256 {
            let a1 = random_action(&mut rng, &legal_mask(&state, SideId::One)).unwrap_or(0);
            let a2 = random_action(&mut rng, &legal_mask(&state, SideId::Two)).unwrap_or(0);
            let c1 = choice_for(&state, SideId::One, a1);
            let c2 = choice_for(&state, SideId::Two, a2);
            let branches = generate_instructions(&state, c1, c2);
            if !branches.is_empty() {
                let idx = sample_idx(&mut rng, &branches);
                state.apply_instructions(&branches[idx].instructions);
                state.turn += 1;
            }
            if encode {
                let _o1 = engine::encode::encode(&state, SideId::One);
                let _o2 = engine::encode::encode(&state, SideId::Two);
                let _i1 = engine::encode::encode_ids(&state, SideId::One);
                let _i2 = engine::encode::encode_ids(&state, SideId::Two);
            }
            steps += 1;
            if branches_done(&state) {
                episodes += 1;
                state = team::default_matchup();
            }
        }
    }
    (steps / start.elapsed().as_secs().max(1), episodes)
}

fn branches_done(state: &State) -> bool {
    battle_over(state) || state.turn > 500
}

fn main() {
    let secs: u64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let (sps_bare, eps) = run_phase(secs, false);
    println!("enumerate+sample:       {sps_bare:>8} steps/s  ({eps} episodes)");
    let (sps_enc, eps) = run_phase(secs, true);
    println!("enumerate + 2x encode:  {sps_enc:>8} steps/s  ({eps} episodes)");
    let (sps_sampled, eps) = run_phase_sampled(secs);
    println!("SAMPLED executor:       {sps_sampled:>8} steps/s  ({eps} episodes)");
    let cores = std::thread::available_parallelism().map(|c| c.get()).unwrap_or(1);
    println!(
        "parallel ceiling estimate ({} cores): ~{} steps/s bare, ~{} with encode",
        cores,
        sps_bare * cores as u64,
        sps_enc * cores as u64
    );
}
