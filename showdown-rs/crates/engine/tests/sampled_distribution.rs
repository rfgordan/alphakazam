//! Pins the sampled executor to the enumerated distribution: `generate_instructions_sampled`
//! must (a) only ever produce instruction lists that appear in the enumerated branch set, and
//! (b) produce each with its enumerated probability (5σ binomial tolerance).
//!
//! This is the standing guarantee that the training fast path plays the same game as the
//! verification path certified against Pokémon Showdown.

use engine::generate::{generate_instructions, generate_instructions_sampled, MoveChoice, Pivot};
use engine::instruction::Instruction;
use engine::team;

fn check(a1: MoveChoice, a2: MoveChoice, samples: usize, rng: &mut u64) {
    let state = team::default_matchup();
    let enumerated = generate_instructions(&state, a1, a2);

    // Coalesce enumerated leaves with identical instruction lists (distinct stochastic paths
    // can produce the same transcript; the sampled bin is the transcript, not the path).
    let mut groups: Vec<(Vec<Instruction>, f64)> = Vec::new();
    for e in &enumerated {
        match groups.iter_mut().find(|(ins, _)| *ins == e.instructions) {
            Some((_, mass)) => *mass += e.percentage as f64,
            None => groups.push((e.instructions.clone(), e.percentage as f64)),
        }
    }

    let mut counts = vec![0usize; groups.len()];
    for _ in 0..samples {
        let s = generate_instructions_sampled(&state, a1, a2, [Pivot::Stay; 2], [false, false], rng);
        let idx = groups
            .iter()
            .position(|(ins, _)| *ins == s.instructions)
            .unwrap_or_else(|| panic!("sampled transcript not in enumerated set ({:?} vs {} groups)", s.instructions.len(), groups.len()));
        counts[idx] += 1;
    }

    for (i, (_, mass)) in groups.iter().enumerate() {
        let p = mass / 100.0;
        let phat = counts[i] as f64 / samples as f64;
        let se = (p * (1.0 - p) / samples as f64).sqrt();
        assert!(
            (phat - p).abs() <= 5.0 * se + 1e-4,
            "group {i}: enumerated p={p:.5}, sampled phat={phat:.5} (n={samples})"
        );
    }
}

#[test]
fn sampled_matches_enumerated_distribution() {
    let mut rng = 0x00C0_FFEE_D15E_A5E5u64;
    // Both actives attack (damage rolls × crit × secondaries on both sides — the worst-case
    // cross-product this executor exists to avoid), plus a mixed attack/switch turn.
    check(MoveChoice::Move(0), MoveChoice::Move(0), 40_000, &mut rng);
    check(MoveChoice::Move(1), MoveChoice::Move(0), 40_000, &mut rng);
    check(MoveChoice::Move(0), MoveChoice::Switch(1), 20_000, &mut rng);
}
