//! Two residual/hit-order rules found in the Exeggutor-Alola / Harvest bucket.
//!
//! 1. **Harvest rolls in SPEED order.** Two Harvest holders make two consecutive, identically
//!    shaped `randomChance[1,2]` draws, so a draw differ cannot tell them apart — but the seed
//!    gate's selector hands the first recorded result to whichever holder the engine rolls first.
//!    PS `speedSort`s the Residual handler list, so the faster holder rolls first (rb5073 d51).
//! 2. **`thawsTarget` cures the freeze AFTER the secondaries.** `frz.onAfterMoveSecondary`
//!    (`data/conditions.ts:111-115`) fires at `hitStepMoveHitLoop`'s trailing
//!    `afterMoveSecondaryEvent` (`battle-actions.ts:1026`), so a frozen target hit by Scald is
//!    still frozen when the 30% burn is tried, `setStatus` fails on an already-statused mon, and
//!    the thaw leaves it with NO status (rb1711 d14). The Fire-type arm is a different handler
//!    (`frz.onDamagingHit`) and stays inside the hit.

use engine::generate::{generate_instructions, generate_instructions_annotated};
use engine::ids::{Ability, Item, MoveId, Species, Status, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};
use engine::MoveChoice;

fn mon(species: &str, ability: &str, m: &str) -> Pokemon {
    let mut p = Pokemon::EMPTY;
    p.species = Species::from_id(species).unwrap();
    p.level = 100;
    p.types = engine::data::species_types(p.species);
    p.base_types = p.types;
    p.hp = 400;
    p.max_hp = 400;
    p.stats = [400, 200, 200, 200, 200, 200];
    p.ability = Ability::from_id(ability).unwrap();
    p.moves[0] = MoveSlot { id: MoveId::from_id(m).unwrap(), pp: 16, max_pp: 16, disabled: false };
    p
}

/// Both actives are Harvest holders with an eaten Sitrus; only the SLOWER one is at low enough HP
/// for the regrown berry to be eaten again, so the two branches are distinguishable by HP.
fn two_harvest_holders(fast_is_side_two: bool) -> State {
    let mut s = State::EMPTY;
    for i in 0..2usize {
        s.sides[i].pokemon[0] = mon("exeggutoralola", "harvest", "splash");
        s.sides[i].pokemon[0].item = Item::None;
        s.sides[i].pokemon[0].last_berry = Item::SitrusBerry;
    }
    let (fast, slow) = if fast_is_side_two { (1, 0) } else { (0, 1) };
    s.sides[fast].pokemon[0].stats[5] = 300;
    s.sides[slow].pokemon[0].stats[5] = 100;
    s
}

/// Which side regrew its berry on the branch whose FIRST `@harvest` draw came up 1 and whose
/// second came up 0 — i.e. the side that rolls first.
fn first_harvest_roller(state: &State) -> SideId {
    for o in generate_instructions_annotated(
        state,
        MoveChoice::Move(0),
        MoveChoice::Move(0),
        [None, None],
        [false, false],
    ) {
        let h: Vec<i64> = o.draws.iter().filter(|d| d.site == "harvest").map(|d| d.result).collect();
        if h != [1, 0] {
            continue;
        }
        let mut r = *state;
        r.apply_instructions(&o.instructions);
        let regrew: Vec<SideId> = [SideId::One, SideId::Two]
            .into_iter()
            .filter(|&sd| r.side(sd).active().last_berry == Item::None)
            .collect();
        assert_eq!(regrew.len(), 1, "exactly one holder regrew on the [1,0] branch");
        return regrew[0];
    }
    panic!("no branch with harvest draws [1, 0]");
}

#[test]
fn harvest_rolls_fastest_first() {
    assert_eq!(first_harvest_roller(&two_harvest_holders(true)), SideId::Two);
    assert_eq!(first_harvest_roller(&two_harvest_holders(false)), SideId::One);
}

/// Scald into a frozen target: thawed, and NOT burned, on every branch.
#[test]
fn scald_into_a_frozen_target_thaws_and_never_burns() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("lanturn", "voltabsorb", "scald");
    s.sides[0].pokemon[0].stats[5] = 300;
    s.sides[1].pokemon[0] = mon("bellibolt", "static", "splash");
    s.sides[1].pokemon[0].stats[5] = 1;
    s.sides[1].pokemon[0].status = Status::Freeze;

    let out = generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0));
    assert!(!out.is_empty());
    for o in &out {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_eq!(
            r.sides[1].pokemon[0].status,
            Status::None,
            "the freeze is cured after the secondary, so the burn cannot stick"
        );
    }
}

/// The control: an UNFROZEN target still catches Scald's burn on the proc branch.
#[test]
fn scald_still_burns_an_unfrozen_target() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("lanturn", "voltabsorb", "scald");
    s.sides[0].pokemon[0].stats[5] = 300;
    s.sides[1].pokemon[0] = mon("bellibolt", "static", "splash");
    s.sides[1].pokemon[0].stats[5] = 1;

    let burned = generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0))
        .iter()
        .any(|o| {
            let mut r = s;
            r.apply_instructions(&o.instructions);
            r.sides[1].pokemon[0].status == Status::Burn
        });
    assert!(burned, "Scald's 30% burn must still have a branch");
}

/// A Fire-type damaging move is `frz.onDamagingHit`, a different handler, and still thaws.
#[test]
fn a_fire_move_still_thaws_inside_the_hit() {
    let mut s = State::EMPTY;
    s.sides[0].pokemon[0] = mon("lanturn", "voltabsorb", "flamethrower");
    s.sides[0].pokemon[0].stats[5] = 300;
    s.sides[1].pokemon[0] = mon("bellibolt", "static", "splash");
    s.sides[1].pokemon[0].stats[5] = 1;
    s.sides[1].pokemon[0].status = Status::Freeze;
    s.sides[1].pokemon[0].types = [Type::Electric, Type::None];

    for o in &generate_instructions(&s, MoveChoice::Move(0), MoveChoice::Move(0)) {
        let mut r = s;
        r.apply_instructions(&o.instructions);
        assert_ne!(r.sides[1].pokemon[0].status, Status::Freeze, "a Fire hit thaws");
    }
}
