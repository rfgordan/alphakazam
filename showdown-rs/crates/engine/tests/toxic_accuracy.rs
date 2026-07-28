//! Toxic's Poison-type no-miss rule, and the accuracy roll behind a Substitute.
//!
//! `sim/battle-actions.ts:726` — `hitStepAccuracy` hard-codes
//! `move.alwaysHit || (move.id === 'toxic' && this.battle.gen >= 8 && pokemon.hasType('Poison'))`
//! into the `accuracy = true` arm. A `true` accuracy makes **no `randomChance` draw at all**,
//! which is a different thing from a numeric 100 (Close Combat still rolls `randomChance(100,100)`).
//!
//! The counterpart this file also pins: an already-statused target does NOT suppress the accuracy
//! roll. `setStatus` fails inside `moveHit`, long after step 4, and `hitStepTryImmunity`
//! (`battle-actions.ts:661-684`) has no status check. The engine used to skip the draw for a
//! status-only move into an already-statused, subbed target, justified by d6 t58-62 — where the
//! Toxic user is **Toxtricity, Electric/Poison**, i.e. the rule above wearing the wrong name.
//! rb5039 d46 and rb1642 d35 are the witnesses that PS rolls.

use engine::generate::generate_instructions_annotated;
use engine::ids::{MoveId, Species, Status, Type};
use engine::state::{MoveSlot, Pokemon, State};
use engine::volatile::VolatileStatus;
use engine::MoveChoice;

fn mon(species: &str, types: [Type; 2]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = types;
    p.base_types = types;
    p.hp = 300;
    p.max_hp = 300;
    p.stats = [300, 200, 200, 200, 200, 200];
    p
}

fn slot(m: &str) -> MoveSlot {
    MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 10, max_pp: 10, disabled: false }
}

/// p1 uses move 0 into p2's active. p1's typing is the variable under test.
fn board(user: &str, user_types: [Type; 2], move_id: &str) -> State {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon(user, user_types);
    s.sides[0].pokemon[0].moves[0] = slot(move_id);
    // Slower target with a harmless move, so the only accuracy draw in the stream is p1's.
    s.sides[1].pokemon[0] = mon("blissey", [Type::Normal, Type::None]);
    s.sides[1].pokemon[0].moves[0] = slot("splash");
    s.sides[1].pokemon[0].stats[5] = 1;
    s
}

/// Every accuracy-site `randomChance` arg list across all branches, deduped.
fn accuracy_draws(state: &State) -> Vec<Vec<i32>> {
    let mut v: Vec<Vec<i32>> = generate_instructions_annotated(
        state,
        MoveChoice::Move(0),
        MoveChoice::Move(0),
        [None, None],
        [false, false],
    )
    .iter()
    .flat_map(|o| o.draws.iter().filter(|d| d.site == "accuracy").map(|d| d.args.clone()))
    .collect();
    v.sort();
    v.dedup();
    v
}

#[test]
fn poison_type_toxic_never_rolls_accuracy() {
    let s = board("clodsire", [Type::Poison, Type::Ground], "toxic");
    assert!(accuracy_draws(&s).is_empty(), "Poison-type Toxic must make NO accuracy draw");
}

#[test]
fn non_poison_toxic_rolls_ninety() {
    let s = board("blissey", [Type::Normal, Type::None], "toxic");
    assert_eq!(accuracy_draws(&s), vec![vec![90, 100]]);
}

#[test]
fn poison_type_toxic_always_hits() {
    // The forced-`true` override is a probability rule too, not only a stream rule: no miss branch.
    let s = board("clodsire", [Type::Poison, Type::Ground], "toxic");
    let out = generate_instructions_annotated(
        &s,
        MoveChoice::Move(0),
        MoveChoice::Move(0),
        [None, None],
        [false, false],
    );
    let poisoned = out
        .iter()
        .filter(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            r.sides[1].pokemon[0].status == Status::Toxic
        })
        .map(|o| o.percentage)
        .sum::<f32>();
    assert!((poisoned - 100.0).abs() < 0.01, "expected a 100% poison, got {poisoned}");
}

#[test]
fn tera_poison_user_gets_the_override() {
    // `hasType` reads the LIVE types, so a Tera-Poison Blissey's Toxic stops rolling.
    let mut s = board("blissey", [Type::Normal, Type::None], "toxic");
    s.sides[0].pokemon[0].terastallized = true;
    s.sides[0].pokemon[0].tera_type = Type::Poison;
    s.sides[0].pokemon[0].types = [Type::Poison, Type::None];
    assert!(accuracy_draws(&s).is_empty());
}

#[test]
fn already_statused_subbed_target_still_rolls() {
    // rb5039 d46: Toxic into an already-badly-poisoned Keldeo behind its own Substitute.
    let mut s = board("blissey", [Type::Normal, Type::None], "toxic");
    s.sides[1].pokemon[0].status = Status::Toxic;
    s.sides[1].volatiles.insert(VolatileStatus::Substitute);
    s.sides[1].substitute_hp = 75;
    assert_eq!(accuracy_draws(&s), vec![vec![90, 100]]);
}

#[test]
fn already_statused_subbed_target_still_rolls_willowisp() {
    // rb1642 d35: Will-O-Wisp (85) into a statused, subbed target.
    let mut s = board("giratina", [Type::Ghost, Type::Dragon], "willowisp");
    s.sides[1].pokemon[0].status = Status::Poison;
    s.sides[1].volatiles.insert(VolatileStatus::Substitute);
    s.sides[1].substitute_hp = 75;
    assert_eq!(accuracy_draws(&s), vec![vec![85, 100]]);
}

#[test]
fn poison_type_toxic_behind_a_sub_still_makes_no_draw() {
    let mut s = board("clodsire", [Type::Poison, Type::Ground], "toxic");
    s.sides[1].volatiles.insert(VolatileStatus::Substitute);
    s.sides[1].substitute_hp = 75;
    assert!(accuracy_draws(&s).is_empty());
}
