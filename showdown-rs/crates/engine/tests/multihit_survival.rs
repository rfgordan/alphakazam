use engine::generate::generate_move_action;
use engine::ids::{Ability, Item, MoveId, Species, Type};
use engine::state::{MoveSlot, Pokemon, SideId, State};

fn state_with_survival(item: Item, ability: Ability) -> State {
    let mut state = State::EMPTY;
    let attacker = &mut state.sides[0].pokemon[0];
    *attacker = Pokemon::EMPTY;
    attacker.species = Species::from_id("breloom").unwrap();
    attacker.level = 100;
    attacker.hp = 300;
    attacker.max_hp = 300;
    attacker.types = [Type::Grass, Type::Fighting];
    attacker.base_types = attacker.types;
    attacker.stats = [300, 600, 180, 180, 180, 250];
    attacker.moves[0] = MoveSlot {
        id: MoveId::from_id("bulletseed").unwrap(), pp: 10, max_pp: 10, disabled: false,
    };

    let defender = &mut state.sides[1].pokemon[0];
    *defender = Pokemon::EMPTY;
    defender.species = Species::from_id("whimsicott").unwrap();
    defender.level = 100;
    defender.hp = 100;
    defender.max_hp = 100;
    defender.types = [Type::Fairy, Type::None];
    defender.base_types = defender.types;
    defender.ability = ability;
    defender.base_ability = ability;
    defender.item = item;
    defender.stats = [100, 100, 40, 100, 40, 100];
    state
}

#[test]
fn variable_multihit_consumes_sash_then_stops_counting_after_faint() {
    let original = state_with_survival(Item::FocusSash, Ability::None);
    let outcomes = generate_move_action(&original, SideId::One, 0, None, None);
    assert!(!outcomes.is_empty());
    let mut total = 0.0;
    for outcome in outcomes {
        total += outcome.percentage;
        let mut result = original;
        result.apply_instructions(&outcome.instructions);
        let target = result.side(SideId::Two).active();
        assert_eq!(target.hp, 0, "hit one activates Sash and hit two must KO");
        assert_eq!(target.item, Item::None, "Focus Sash must be consumed");
        assert_eq!(target.times_hit, 2, "nominal hits after the KO do not connect");
        let mut restored = result;
        restored.reverse_instructions(&outcome.instructions);
        assert_eq!(restored, original);
    }
    assert!((total - 100.0).abs() < 0.01);
}

#[test]
fn variable_multihit_sturdy_survives_only_the_first_hit() {
    let original = state_with_survival(Item::None, Ability::Sturdy);
    for outcome in generate_move_action(&original, SideId::One, 0, None, None) {
        let mut result = original;
        result.apply_instructions(&outcome.instructions);
        let target = result.side(SideId::Two).active();
        assert_eq!(target.hp, 0);
        assert_eq!(target.times_hit, 2);
        assert_eq!(target.item, Item::None);
    }
}

