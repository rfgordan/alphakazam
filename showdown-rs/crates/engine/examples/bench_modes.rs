//! Cost of the three execution modes on the SAME position, for a normal move vs a multi-hit one.
//!
//! The three modes are not three engines — they share one mechanics body and differ only in the
//! *outcome policy*:
//!
//!   Enumerate  — expand every stochastic fork with exact probabilities (verification, search).
//!   Sample     — follow one weighted path, but prune AT STAGE SEAMS: a stage's branch set is
//!                still built, then reduced to one survivor. This is the training path.
//!   Annotated  — Enumerate plus a per-branch PRNG draw log; the seed gate runs this and then
//!                filters to the single branch matching a real `PsPrng` (`replicate_select`).
//!                Measured here without the filter, which is the cheap half.
//!
//! The point of the benchmark is that `Sample` is NOT single-branch: for a move whose per-hit
//! product is large (Triple Axel: (16 damage rolls x 2 crit)^k for k=1..3 = 33,824 branches) it
//! pays nearly the full enumeration cost and then throws all of it away.
//!
//!     cargo run --release -p engine --example bench_modes

use std::time::Instant;

use engine::generate::{generate_instructions, generate_instructions_annotated, generate_instructions_sampled, MoveChoice, Pivot};
use engine::ids::Nature;
use engine::state::State;
use engine::team::{self, MemberSpec};

const N: [u8; 6] = [0, 0, 0, 0, 0, 0];

/// Attacker holding `attack` in slot 0; both sides otherwise identical and bulky enough that
/// nothing faints mid-benchmark (a KO would truncate the very branching we are measuring).
fn fixture(attack: &'static str) -> State {
    let atk = MemberSpec {
        species: "cinccino", ability: "skilllink", item: "lifeorb", tera: "normal",
        nature: Nature::Jolly, evs: N, moves: [attack, "tailslap", "knockoff", "uturn"],
    };
    let def = MemberSpec {
        species: "blissey", ability: "naturalcure", item: "leftovers", tera: "normal",
        nature: Nature::Bold, evs: N, moves: ["seismictoss", "softboiled", "toxic", "protect"],
    };
    team::build_state(&[atk], &[def], 100)
}

fn bench(label: &str, state: &State, seconds: f64) {
    let stay = [Pivot::Stay; 2];
    let tera = [false; 2];

    // Enumerate: full fork expansion.
    let t = Instant::now();
    let mut n_enum = 0u64;
    let mut branches = 0usize;
    while t.elapsed().as_secs_f64() < seconds {
        let out = generate_instructions(state, MoveChoice::Move(0), MoveChoice::Move(0));
        branches = out.len();
        n_enum += 1;
        std::hint::black_box(&out);
    }
    let enum_s = n_enum as f64 / t.elapsed().as_secs_f64();

    // Sample: one weighted path, pruned at stage seams.
    let t = Instant::now();
    let mut n_s = 0u64;
    let mut rng = 0x1234_5678u64;
    while t.elapsed().as_secs_f64() < seconds {
        let out = generate_instructions_sampled(state, MoveChoice::Move(0), MoveChoice::Move(0), stay, tera, &mut rng);
        n_s += 1;
        std::hint::black_box(&out);
    }
    let samp_s = n_s as f64 / t.elapsed().as_secs_f64();

    // Annotated (the seed gate's generation half).
    let t = Instant::now();
    let mut n_a = 0u64;
    while t.elapsed().as_secs_f64() < seconds {
        let out = generate_instructions_annotated(state, MoveChoice::Move(0), MoveChoice::Move(0), [None, None], tera);
        n_a += 1;
        std::hint::black_box(&out);
    }
    let annot_s = n_a as f64 / t.elapsed().as_secs_f64();

    println!(
        "{label:14}  enumerate {enum_s:9.1}/s   sample {samp_s:9.1}/s   annotated {annot_s:9.1}/s   \
         (enumerate produced {branches} branches)"
    );
    println!(
        "{:14}  sample is {:.2}x enumerate  ({:.3} ms/decision sampled)",
        "", samp_s / enum_s.max(1e-9), 1000.0 / samp_s.max(1e-9)
    );
}

fn main() {
    let seconds: f64 = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(2.0);
    println!("decisions/s, single-threaded, per move (higher is better)\n");
    // `bulletseed` is a fixed-BP multi-hit that the sumset DP already compresses — the control.
    for (label, mv) in [("thunderbolt", "thunderbolt"), ("bulletseed", "bulletseed"),
                        ("dualwingbeat", "dualwingbeat"), ("tripleaxel", "tripleaxel")] {
        bench(label, &fixture(mv), seconds);
    }
}
