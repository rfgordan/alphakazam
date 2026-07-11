use engine::generate::generate_move_action;
use engine::ids::{Ability, MoveId, Species, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::volatile::VolatileStatus;

fn ice_face_state(move_name: &str) -> State {
    let mut state = State::EMPTY;
    let attacker = &mut state.sides[0].pokemon[0];
    *attacker = Pokemon::EMPTY;
    attacker.species = Species::from_id("ambipom").unwrap();
    attacker.level = 100;
    attacker.hp = 300;
    attacker.max_hp = 300;
    attacker.types = [Type::Normal, Type::None];
    attacker.base_types = attacker.types;
    attacker.stats = [300, 250, 180, 180, 180, 250];
    attacker.moves[0] = MoveSlot {
        id: MoveId::from_id(move_name).unwrap(), pp: 10, max_pp: 10, disabled: false,
    };

    let defender = &mut state.sides[1].pokemon[0];
    *defender = Pokemon::EMPTY;
    defender.species = Species::from_id("eiscue").unwrap();
    defender.level = 100;
    defender.hp = 400;
    defender.max_hp = 400;
    defender.types = [Type::Ice, Type::None];
    defender.base_types = defender.types;
    defender.ability = Ability::IceFace;
    defender.base_ability = Ability::IceFace;
    defender.stats = [400, 180, 300, 180, 220, 130];
    state
}

fn transformed_results(move_name: &str) -> Vec<(f32, State, Vec<engine::Instruction>)> {
    let state = ice_face_state(move_name);
    generate_move_action(&state, SideId::One, 0, None, None)
        .into_iter()
        .filter_map(|outcome| {
            let mut result = state;
            result.apply_instructions(&outcome.instructions);
            (result.side(SideId::Two).active().species == Species::from_id("eiscuenoice").unwrap())
                .then_some((outcome.percentage, result, outcome.instructions))
        })
        .collect()
}

#[test]
fn fixed_multihit_breaks_face_then_damages_noice_and_reverses() {
    let original = ice_face_state("doublehit");
    let results = transformed_results("doublehit");
    assert!(!results.is_empty());
    for (_, result, instructions) in results {
        let target = result.side(SideId::Two).active();
        assert!(target.hp < target.max_hp, "the second hit must damage Noice Form");
        assert_eq!(target.times_hit, 2, "both the blocked and damaging hit count");

        let mut restored = result;
        restored.reverse_instructions(&instructions);
        assert_eq!(restored, original, "forme, stats, HP, and hit count must all reverse");
    }
}

#[test]
fn variable_multihit_distribution_preserves_original_hit_counts() {
    let results = transformed_results("bulletseed");
    assert!(!results.is_empty());
    let mut probability_by_hits = [0.0f32; 6];
    for (probability, result, _) in results {
        let target = result.side(SideId::Two).active();
        assert!(target.hp < target.max_hp, "hits after the blocked first hit must deal damage");
        assert!((2..=5).contains(&target.times_hit));
        probability_by_hits[target.times_hit as usize] += probability;
    }
    // Gen 5+ weighting is 2/6, 2/6, 1/6, 1/6 for 2/3/4/5 hits.
    for (hits, expected) in [(2, 100.0 / 3.0), (3, 100.0 / 3.0), (4, 100.0 / 6.0), (5, 100.0 / 6.0)] {
        assert!((probability_by_hits[hits] - expected).abs() < 0.01,
            "hit-count {hits}: got {}, expected {expected}", probability_by_hits[hits]);
    }
}

#[test]
fn variable_multihit_interleaves_substitute_then_ice_face_then_damage() {
    let mut original = ice_face_state("bulletseed");
    original.sides[1].substitute_hp = 1;
    original.sides[1].volatiles.insert(VolatileStatus::Substitute);
    let outcomes = generate_move_action(&original, SideId::One, 0, None, None);
    assert!(!outcomes.is_empty());

    let mut probability_by_mon_hits = [0.0f32; 5];
    for outcome in outcomes {
        let mut result = original;
        result.apply_instructions(&outcome.instructions);
        let target = result.side(SideId::Two).active();
        assert_eq!(result.side(SideId::Two).substitute_hp, 0);
        assert!(!result.side(SideId::Two).volatiles.contains(VolatileStatus::Substitute));
        assert_eq!(target.species, Species::from_id("eiscuenoice").unwrap());
        // Hit 1 breaks the 1 HP Substitute, hit 2 breaks Ice Face, and only hits 3+ damage.
        assert!((1..=4).contains(&target.times_hit));
        assert_eq!(target.hp < target.max_hp, target.times_hit >= 2,
            "hp={}, max={}, times={}, probability={}", target.hp, target.max_hp, target.times_hit, outcome.percentage);
        probability_by_mon_hits[target.times_hit as usize] += outcome.percentage;

        let mut restored = result;
        restored.reverse_instructions(&outcome.instructions);
        assert_eq!(restored, original);
    }
    // Original 2/3/4/5 hit counts map to 1/2/3/4 hits on the Pokémon.
    for (hits, expected) in [(1, 100.0 / 3.0), (2, 100.0 / 3.0), (3, 100.0 / 6.0), (4, 100.0 / 6.0)] {
        assert!((probability_by_mon_hits[hits] - expected).abs() < 0.01,
            "Pokémon-hit count {hits}: got {}, expected {expected}", probability_by_mon_hits[hits]);
    }
}
