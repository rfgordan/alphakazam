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

// ---- multi-hit coverage -------------------------------------------------------------------
//
// `default_matchup()` contains no multi-hit move, so the cases above never exercise the
// multi-hit executor at all — they passed identically before and after `Exec::Sample` started
// realizing those moves instead of enumerating their per-hit product. These cases close that
// gap. They are deliberately aggregate rather than per-transcript: Triple Axel enumerates 33,825
// leaves, so pinning each one at 5σ would need an unreasonable sample count. Instead:
//
//   1. SUPPORT — every sampled transcript must appear in the enumerated set. This is the
//      structural check: a realized path that produced an outcome enumeration cannot reach
//      (wrong hit count, wrong per-hit base power, a skipped effect) fails here.
//   2. HIT COUNT — the number of landed hits must follow the enumerated distribution (5σ per
//      bin). This is what pins the per-hit accuracy semantics ("a miss ends the move").
//   3. MEAN DAMAGE — pins the per-hit damage rolls against enumeration's exact mean.

use engine::ids::Nature;
use engine::state::{SideId, State};
use engine::team::MemberSpec;
use std::collections::{HashMap, HashSet};

const NO_EVS: [u8; 6] = [0, 0, 0, 0, 0, 0];

fn multihit_fixture(attack: &'static str, ability: &'static str) -> State {
    let atk = MemberSpec {
        species: "cinccino", ability, item: "none", tera: "normal",
        nature: Nature::Jolly, evs: NO_EVS, moves: [attack, "tailslap", "knockoff", "uturn"],
    };
    // Blissey: enough HP that no roll KOes it, so hit counts are never truncated by a faint.
    let def = MemberSpec {
        species: "blissey", ability: "naturalcure", item: "none", tera: "normal",
        nature: Nature::Bold, evs: NO_EVS, moves: ["softboiled", "toxic", "protect", "seismictoss"],
    };
    engine::team::build_state(&[atk], &[def], 100)
}

/// (hits landed on the foe, total damage dealt to the foe) for one transcript.
fn hits_and_damage(ins: &[Instruction]) -> (usize, i64) {
    let mut n = 0usize;
    let mut total = 0i64;
    for i in ins {
        if let Instruction::Damage { side: SideId::Two, amount, .. } = i {
            n += 1;
            total += *amount as i64;
        }
    }
    (n, total)
}

fn check_multihit(attack: &'static str, ability: &'static str, samples: usize, rng: &mut u64) {
    let state = multihit_fixture(attack, ability);
    // Side two passes (Protect would change the picture); side one uses the multi-hit move.
    let (a1, a2) = (MoveChoice::Move(0), MoveChoice::Move(0));
    let enumerated = generate_instructions(&state, a1, a2);
    assert!(!enumerated.is_empty(), "{attack}: no enumerated branches");

    let support: HashSet<String> = enumerated.iter().map(|e| format!("{:?}", e.instructions)).collect();
    let mut exp_hits: HashMap<usize, f64> = HashMap::new();
    let mut exp_damage = 0.0f64;
    for e in &enumerated {
        let (h, d) = hits_and_damage(&e.instructions);
        let p = e.percentage as f64 / 100.0;
        *exp_hits.entry(h).or_default() += p;
        exp_damage += p * d as f64;
    }

    let mut got_hits: HashMap<usize, usize> = HashMap::new();
    let mut got_damage = 0.0f64;
    for _ in 0..samples {
        let s = generate_instructions_sampled(&state, a1, a2, [Pivot::Stay; 2], [false, false], rng);
        assert!(
            support.contains(&format!("{:?}", s.instructions)),
            "{attack}: sampled a transcript the enumerator cannot produce"
        );
        let (h, d) = hits_and_damage(&s.instructions);
        *got_hits.entry(h).or_default() += 1;
        got_damage += d as f64;
    }

    for (h, p) in &exp_hits {
        let phat = got_hits.get(h).copied().unwrap_or(0) as f64 / samples as f64;
        // Clamp before the binomial SE: enumerated masses are summed from f32 percentages and
        // can land a hair above 1.0, which makes `(p*(1-p)).sqrt()` NaN — and every comparison
        // against NaN is false, so the assert fires with p and phat printing identical.
        let pc = p.clamp(0.0, 1.0);
        let se = (pc * (1.0 - pc) / samples as f64).sqrt();
        assert!(
            (phat - pc).abs() <= 5.0 * se + 1e-3,
            "{attack}: {h} hits — enumerated p={pc:.5}, sampled {phat:.5} (n={samples})"
        );
    }
    // Damage is bounded, so a 5σ band from its enumerated spread is a fair tolerance.
    let var: f64 = enumerated.iter().map(|e| {
        let (_, d) = hits_and_damage(&e.instructions);
        (e.percentage as f64 / 100.0) * (d as f64 - exp_damage).powi(2)
    }).sum();
    let se = (var / samples as f64).sqrt();
    let got_mean = got_damage / samples as f64;
    assert!(
        (got_mean - exp_damage).abs() <= 5.0 * se + 1e-6,
        "{attack}: mean damage enumerated {exp_damage:.3}, sampled {got_mean:.3} (5σ={:.3})",
        5.0 * se
    );
}

#[test]
fn sampled_multihit_matches_enumerated_distribution() {
    let mut rng = 0x5EED_1234_ABCD_0001u64;
    // Per-hit accuracy with ascending base power — the 33,825-branch case the realized path exists
    // for. Skill Link does NOT bypass its accuracy checks, so the hit-count distribution is live.
    check_multihit("tripleaxel", "cutecharm", 20_000, &mut rng);
    // Variable [2,5] count: previously the sumset-DP path in Sample mode, now realized.
    check_multihit("bulletseed", "cutecharm", 4_000, &mut rng);
    // Skill Link pins the count at 5 — checks the realized path honours count-forcing abilities.
    check_multihit("bulletseed", "skilllink", 4_000, &mut rng);
}
