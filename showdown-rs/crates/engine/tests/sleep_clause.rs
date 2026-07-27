//! Sleep Clause Mod — `data/rulesets.ts:1378`, `RULESET_SPEC.md` §1 / H1.
//!
//! The clause is a `Rule` pseudo-weather handler on `SetStatus`, subOrder 5, i.e. LAST: after
//! every ability immunity (speed > 0), after the terrains (subOrder 2) and after Safeguard (4).
//! Blocking there means the `slp` condition's `onStart` never runs, so the `random(2,5)` duration
//! roll is **not consumed** — the single biggest draw-shape delta between the two presets. The
//! move still pays for everything before `MoveHitLoop`, notably the accuracy roll.
//!
//! Both Rest exclusions live in `source?.isAlly(target)` and in the `statusState.source` test of
//! the party scan; the engine spells them as "Rest never consults the clause" and "Rest never
//! sets `slept_by_foe`".

use engine::generate::{generate_instructions_annotated, generate_move_action};
use engine::ids::{Item, MoveId, Species, Status, Type};
use engine::ruleset::Ruleset;
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::MoveChoice;

fn mon(species: &str, types: [Type; 2], stats: [i16; 6]) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = types;
    p.base_types = types;
    p.hp = stats[0];
    p.max_hp = stats[0];
    p.stats = stats;
    p
}

fn slot(m: &str) -> MoveSlot {
    MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 10, max_pp: 10, disabled: false }
}

/// p1 Amoonguss with Spore vs a p2 party whose slot 0 is awake. Spore never misses, so any
/// `random(2,5)` in the draw stream is unambiguously the sleep duration.
fn spore_board(rs: Ruleset) -> State {
    let mut s = State::EMPTY;
    s.ruleset = rs;
    s.sides[0].pokemon[0] = mon("amoonguss", [Type::Grass, Type::Poison], [400, 180, 200, 180, 200, 100]);
    s.sides[0].pokemon[0].moves[0] = slot("spore");
    for i in 0..2usize {
        s.sides[1].pokemon[i] = mon("dragapult", [Type::Dragon, Type::Ghost], [300, 250, 180, 180, 180, 250]);
        s.sides[1].pokemon[i].moves[0] = slot("splash");
    }
    s
}

fn sleep_duration_draws(state: &State) -> usize {
    generate_instructions_annotated(state, MoveChoice::Move(0), MoveChoice::Move(0), [None, None], [false, false])
        .iter()
        .map(|o| o.draws.iter().filter(|d| d.kind == "random" && d.args == [2, 5]).count())
        .max()
        .unwrap_or(0)
}

fn any_branch_sleeps(state: &State) -> bool {
    generate_move_action(state, SideId::One, 0, None, None).iter().any(|o| {
        let mut r = *state;
        r.apply_instructions(&o.instructions);
        r.side(SideId::Two).active().status == Status::Sleep
    })
}

fn emits_clause_marker(state: &State) -> bool {
    generate_move_action(state, SideId::One, 0, None, None).iter().any(|o| {
        o.instructions
            .iter()
            .any(|i| matches!(i, engine::Instruction::SleepClauseBlocked { .. }))
    })
}

// ---- the block itself ------------------------------------------------------------------------

#[test]
fn first_sleep_lands_under_both_presets() {
    for rs in [Ruleset::GEN9_CUSTOM_GAME, Ruleset::GEN9_RANDOM_BATTLE] {
        let s = spore_board(rs);
        assert!(any_branch_sleeps(&s), "{}: nothing is asleep yet", rs.format_id);
        assert!(!emits_clause_marker(&s), "{}: clause must be silent", rs.format_id);
        assert_eq!(sleep_duration_draws(&s), 1, "{}: one random(2,5)", rs.format_id);
    }
}

#[test]
fn a_second_foe_slept_party_member_blocks_and_consumes_no_duration_draw() {
    let mut s = spore_board(Ruleset::GEN9_RANDOM_BATTLE);
    // The BENCHED slot-1 mon is already asleep, put there by a foe. PS scans the whole party.
    s.sides[1].pokemon[1].status = Status::Sleep;
    s.sides[1].pokemon[1].slept_by_foe = true;

    assert!(!any_branch_sleeps(&s), "Sleep Clause Mod must refuse the sleep");
    assert!(emits_clause_marker(&s), "the |-message| / |-hint| pair must be emitted");
    // H1: the blocked path consumes NOTHING at the status site — not a draw-and-discard.
    assert_eq!(sleep_duration_draws(&s), 0, "no random(2,5) on the blocked path");

    // ...and the same board under customgame lets it through, with its duration roll.
    let mut cg = s;
    cg.ruleset = Ruleset::GEN9_CUSTOM_GAME;
    assert!(any_branch_sleeps(&cg));
    assert!(!emits_clause_marker(&cg));
    assert_eq!(sleep_duration_draws(&cg), 1);
}

#[test]
fn a_fainted_sleeper_does_not_occupy_the_clause_slot() {
    // PS requires `pokemon.hp` in the party scan.
    let mut s = spore_board(Ruleset::GEN9_RANDOM_BATTLE);
    s.sides[1].pokemon[1].status = Status::Sleep;
    s.sides[1].pokemon[1].slept_by_foe = true;
    s.sides[1].pokemon[1].hp = 0;
    assert!(any_branch_sleeps(&s), "a fainted sleeper is not counted");
    assert!(!emits_clause_marker(&s));
}

// ---- the two Rest exclusions -----------------------------------------------------------------

#[test]
fn rest_asleep_party_member_does_not_block_a_foe_sleep() {
    // `!pokemon.statusState.source?.isAlly(pokemon)` — a self-Rested sleeper is skipped by the
    // scan. The engine spells this as `slept_by_foe == false`.
    let mut s = spore_board(Ruleset::GEN9_RANDOM_BATTLE);
    s.sides[1].pokemon[1].status = Status::Sleep;
    s.sides[1].pokemon[1].slept_by_foe = false;
    assert!(any_branch_sleeps(&s), "a Rest sleeper does not hold the clause slot");
    assert!(!emits_clause_marker(&s));
    assert_eq!(sleep_duration_draws(&s), 1);
}

#[test]
fn rest_itself_is_never_blocked_and_never_takes_the_slot() {
    // `source?.isAlly(target)` returns before the clause is consulted.
    let mut s = State::EMPTY;
    s.ruleset = Ruleset::GEN9_RANDOM_BATTLE;
    s.sides[0].pokemon[0] = mon("snorlax", [Type::Normal, Type::None], [500, 250, 200, 180, 200, 60]);
    s.sides[0].pokemon[0].moves[0] = slot("rest");
    s.sides[0].pokemon[0].hp = 100;
    s.sides[0].pokemon[0].item = Item::None;
    // A party member of the RESTING side is already foe-slept — irrelevant to Rest.
    s.sides[0].pokemon[1] = mon("dragapult", [Type::Dragon, Type::Ghost], [300, 250, 180, 180, 180, 250]);
    s.sides[0].pokemon[1].status = Status::Sleep;
    s.sides[0].pokemon[1].slept_by_foe = true;
    s.sides[1].pokemon[0] = mon("dragapult", [Type::Dragon, Type::Ghost], [300, 250, 180, 180, 180, 250]);
    s.sides[1].pokemon[0].moves[0] = slot("splash");

    let outs = generate_move_action(&s, SideId::One, 0, None, None);
    assert!(!outs.is_empty());
    for o in &outs {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(r.side(SideId::One).active().status, Status::Sleep, "Rest is never clause-blocked");
        assert!(!r.side(SideId::One).active().slept_by_foe, "Rest does not take the clause slot");
        assert!(!o.instructions.iter().any(|i| matches!(i, engine::Instruction::SleepClauseBlocked { .. })));
    }
}

// ---- H4: the subOrder-5 collision assertion --------------------------------------------------

#[test]
fn no_preset_registers_two_suborder_5_set_status_rules() {
    // SPEC H4. Sleep Clause Mod's `speedSort` tuple in a `SetStatus` list is
    // (order ∞, priority 0, speed 0, subOrder 5, effectOrder 0) and NOTHING else in either
    // preset shares it — so the clause is shuffle-neutral. A second such Rule (Freeze Clause
    // Mod, Stadium Sleep Clause) would create a 2-element tie group and one extra `prng.shuffle`
    // that no draw-exact gate is currently modelling. Fail here, loudly, if one is ever added.
    for rs in [Ruleset::GEN9_CUSTOM_GAME, Ruleset::GEN9_RANDOM_BATTLE] {
        assert!(
            rs.set_status_rule_handlers() <= 1,
            "{}: {} subOrder-5 Rule handlers in a SetStatus list — that is a NEW prng.shuffle",
            rs.format_id,
            rs.set_status_rule_handlers()
        );
    }
}
