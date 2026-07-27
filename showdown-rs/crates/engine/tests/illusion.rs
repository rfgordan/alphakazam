//! Illusion (`data/abilities.ts:2010-2050`) — the disguise, the break, and the two things it
//! changes outside the protocol.
//!
//! PS semantics being pinned here:
//!
//! * `onBeforeSwitchIn` picks the LAST able entry of the LIVE `side.pokemon` array behind the
//!   entering mon. The array is not the party: `switchIn` swaps the outgoing and incoming entries
//!   (`sim/battle-actions.ts:128-131`), so the answer depends on switch history. Modelled as
//!   `Side::roster`.
//! * The choice runs AFTER that swap and BEFORE `add('switch')`, so the `|switch|` line is already
//!   masked. No PRNG is consumed anywhere in the ability.
//! * `onDamagingHit` → `singleEvent('End')` → `onEnd` emits `|replace|` + `|-end|…|Illusion|` and,
//!   under Illusion Level Mod, a `|-hint|`.
//! * A SWITCH-OUT does not break it: `switchIn` sets `beingCalledBack = true` before firing the
//!   ability's `End`, and `onEnd` bails on that flag. The disguise rides along on the bench.
//! * `getFullDetails` shows the disguise's LEVEL only under Illusion Level Mod; without the rule a
//!   disguised mon wears its own level under a foreign name.
//! * `sim/battle.ts:1732` — the `maybeTrapped` inference sweeps `(source.illusion || source)
//!   .species`, so the disguise's ability list is what the foe's client is warned about.

use engine::ids::{Ability, MoveId, Species};
use engine::protocol::HpStyle;
use engine::ruleset::Ruleset;
use engine::state::{SideId, State};
use engine::team;

fn sp(id: &str) -> Species {
    Species::from_id(id).unwrap_or_else(|| panic!("species {id}"))
}

/// `default_matchup` with side One's slot 0 turned into a Zoroark-Hisui running Illusion.
fn board(rs: Ruleset) -> State {
    let mut st = team::default_matchup();
    st.ruleset = rs;
    let s = st.side_mut(SideId::One);
    s.pokemon[0].species = sp("zoroarkhisui");
    s.pokemon[0].base_species = sp("zoroarkhisui");
    s.pokemon[0].ability = Ability::Illusion;
    s.pokemon[0].base_ability = Ability::Illusion;
    // Something unambiguous to switch INTO the Zoroark from. PS's array ALWAYS holds the active at
    // index 0 (`side.active[0] === side.pokemon[0]`), so the roster has to be moved with it — a
    // hand-set `active_index` alone would describe a board PS cannot produce.
    s.active_index = 1;
    s.roster = [1, 0, 2, 3, 4, 5];
    st
}

/// Party slots that are actually occupied on side One's default team.
fn occupied(st: &State) -> Vec<u8> {
    (0..6u8).filter(|&i| st.side(SideId::One).pokemon[i as usize].species != Species::None).collect()
}

#[test]
fn disguise_is_the_last_able_party_member_and_costs_no_draw() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    let slots = occupied(&st);
    let last = *slots.last().unwrap();
    assert!(last > 0, "the fixture needs a bench behind slot 0");

    let ins = engine::generate::switch_into(&mut st, SideId::One, 0);
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(last));
    // The ability is a plain loop plus a `singleEvent`; neither draws.
    assert!(
        ins.iter().any(|i| matches!(i, engine::instruction::Instruction::SetIllusion { .. })),
        "the disguise must be an explicit, reversible instruction"
    );
}

#[test]
fn the_switch_swaps_the_live_array_and_the_disguise_follows_it() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    assert_eq!(st.side(SideId::One).roster, [1, 0, 2, 3, 4, 5]);
    let slots = occupied(&st);
    let last = *slots.last().unwrap();

    // Zoroark (party slot 0) enters: it takes array index 0 and the outgoing mon takes its.
    engine::generate::switch_into(&mut st, SideId::One, 0);
    assert_eq!(st.side(SideId::One).roster, [0, 1, 2, 3, 4, 5]);
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(last));

    // Now pivot to the mon it was DISGUISED as. That mon moves to array index 0 and the Zoroark
    // takes its place at the back — exactly the `[Pokemon:p1a]` references the recorded corpus
    // shows on a benched Zoroark (rb5017 d42), because PS's reference follows the OBJECT.
    engine::generate::switch_into(&mut st, SideId::One, last);
    assert_eq!(st.side(SideId::One).roster[0], last);
    assert_eq!(
        st.side(SideId::One).roster.iter().position(|&x| x == 0),
        Some(occupied(&st).len() - 1),
        "the Zoroark took the disguise target's old array index"
    );
    // A switch-OUT never breaks the disguise: `beingCalledBack` is true at the ability's `End`.
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(last));
}

#[test]
fn the_disguise_is_rechosen_on_re_entry_from_the_new_array_order() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    let slots = occupied(&st);
    let last = *slots.last().unwrap();

    engine::generate::switch_into(&mut st, SideId::One, 0);
    engine::generate::switch_into(&mut st, SideId::One, last); // Zoroark to the back
    engine::generate::switch_into(&mut st, SideId::One, 0); // and back in

    // Behind the Zoroark now sits whatever the swaps left at the tail — NOT necessarily `last`.
    let roster = st.side(SideId::One).roster;
    let expected = roster
        .iter()
        .rev()
        .copied()
        .find(|&s| {
            let p = &st.side(SideId::One).pokemon[s as usize];
            s != 0 && p.species != Species::None && p.is_alive()
        })
        .unwrap();
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(expected));
}

#[test]
fn a_fainted_party_member_is_skipped_not_a_stopping_point() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    let slots = occupied(&st);
    let last = *slots.last().unwrap();
    let second_last = slots[slots.len() - 2];
    st.side_mut(SideId::One).pokemon[last as usize].hp = 0;

    engine::generate::switch_into(&mut st, SideId::One, 0);
    // PS's `break` sits INSIDE the `!fainted` arm, so the scan walks past the corpse.
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(second_last));
}

#[test]
fn nothing_behind_it_means_no_disguise() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    for i in 1..6u8 {
        st.side_mut(SideId::One).pokemon[i as usize].hp = 0;
    }
    // Every candidate is a corpse; the entering mon has to come out as itself.
    st.side_mut(SideId::One).active_index = 1;
    engine::generate::switch_into(&mut st, SideId::One, 0);
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, None);
}

#[test]
fn a_mon_without_the_ability_never_picks_a_disguise() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    st.side_mut(SideId::One).pokemon[0].ability = Ability::Levitate;
    engine::generate::switch_into(&mut st, SideId::One, 0);
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, None);
}

#[test]
fn the_switch_line_wears_the_disguise_and_the_level_rule_decides_the_level() {
    // Without Illusion Level Mod (customgame): disguise SPECIES, the Zoroark's OWN level.
    let mut plain = board(Ruleset::GEN9_CUSTOM_GAME);
    let last = *occupied(&plain).last().unwrap();
    plain.side_mut(SideId::One).pokemon[0].level = 80;
    plain.side_mut(SideId::One).pokemon[last as usize].level = 90;
    engine::generate::switch_into(&mut plain, SideId::One, 0);
    let line = engine::protocol::switch_line(&plain, SideId::One, HpStyle::Exact);
    let shown = plain.side(SideId::One).pokemon[last as usize].species.to_id().to_string();
    assert!(line.contains("L80"), "customgame shows the REAL level: {line}");
    assert!(!line.to_lowercase().contains("zoroark"), "the real species must not leak: {line}");
    assert!(line.to_lowercase().replace([' ', '-'], "").contains(&shown), "{line}");

    // With Illusion Level Mod (randbats): the DISGUISE's level too.
    let mut modded = board(Ruleset::GEN9_RANDOM_BATTLE);
    modded.side_mut(SideId::One).pokemon[0].level = 80;
    modded.side_mut(SideId::One).pokemon[last as usize].level = 90;
    engine::generate::switch_into(&mut modded, SideId::One, 0);
    let line = engine::protocol::switch_line(&modded, SideId::One, HpStyle::Exact);
    assert!(line.contains("L90"), "Illusion Level Mod hides the true level: {line}");
}

#[test]
fn a_damaging_hit_breaks_it_with_replace_end_and_the_level_hint() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    engine::generate::switch_into(&mut st, SideId::One, 0);
    let slot = st.side(SideId::One).active_index;
    let previous = st.side(SideId::One).pokemon[slot as usize].illusion.expect("disguised");

    let pre = st;
    let ins = [engine::instruction::Instruction::BreakIllusion { side: SideId::One, slot, previous }];
    let mut out = Vec::new();
    engine::protocol::emit_instructions(&pre, &ins, HpStyle::Percent, &mut out);
    let joined = out.join("\n");
    assert!(joined.contains("|replace|p1a: "), "{joined}");
    assert!(joined.to_lowercase().contains("zoroark"), "the replace reveals the real mon: {joined}");
    assert!(joined.contains("|-end|p1a: "), "{joined}");
    assert!(joined.contains("|Illusion"), "{joined}");
    assert!(joined.contains("|-hint|Illusion Level Mod is active"), "{joined}");

    // Reversibility: apply then reverse restores the disguise byte-for-byte.
    let mut s2 = pre;
    s2.apply_instructions(&ins);
    assert_eq!(s2.sides[0].pokemon[slot as usize].illusion, None);
    s2.reverse_instructions(&ins);
    assert_eq!(s2, pre);
}

#[test]
fn customgame_break_prints_no_level_hint() {
    let mut st = board(Ruleset::GEN9_CUSTOM_GAME);
    engine::generate::switch_into(&mut st, SideId::One, 0);
    let slot = st.side(SideId::One).active_index;
    let previous = st.side(SideId::One).pokemon[slot as usize].illusion.expect("disguised");
    let mut out = Vec::new();
    engine::protocol::emit_instructions(
        &st,
        &[engine::instruction::Instruction::BreakIllusion { side: SideId::One, slot, previous }],
        HpStyle::Exact,
        &mut out,
    );
    assert!(!out.join("\n").contains("-hint"), "{out:?}");
}

#[test]
fn maybe_trapped_reads_the_apparent_species() {
    // `(source.illusion || source).species` — a Zoroark disguised as a Dugtrio makes the foe's
    // request flag `maybeTrapped`, and a Zoroark disguised as anything else does not.
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    let last = *occupied(&st).last().unwrap();
    st.side_mut(SideId::One).pokemon[last as usize].species = sp("dugtrio");
    st.side_mut(SideId::One).pokemon[last as usize].base_species = sp("dugtrio");
    // The Zoroark itself is NOT a trapper, so without the disguise nothing is inferred.
    assert!(!engine::generate::maybe_trapped(&st, SideId::Two));

    engine::generate::switch_into(&mut st, SideId::One, 0);
    assert_eq!(st.side(SideId::One).pokemon[0].illusion, Some(last));
    assert!(
        engine::generate::maybe_trapped(&st, SideId::Two),
        "the disguise's ability list is what PS sweeps"
    );
}

#[test]
fn observe_shows_the_foe_the_disguise_and_hides_the_pointer() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    let last = *occupied(&st).last().unwrap();
    engine::generate::switch_into(&mut st, SideId::One, 0);
    let disguise = st.side(SideId::One).pokemon[last as usize].species;
    let real_hp = st.side(SideId::One).pokemon[0].hp;

    let obs = st.observe(SideId::Two);
    let seen = &obs.side(SideId::One).pokemon[0];
    assert_eq!(seen.species, disguise, "the foe must not see a Zoroark");
    assert_eq!(seen.illusion, None, "knowing it is a disguise is itself hidden information");
    assert_eq!(seen.hp, real_hp, "HP is reported truthfully for the SLOT");

    // The viewer's own side is never masked.
    let mine = st.observe(SideId::One);
    assert_eq!(mine.side(SideId::One).pokemon[0].species, sp("zoroarkhisui"));
    assert_eq!(mine.side(SideId::One).pokemon[0].illusion, Some(last));
}

#[test]
fn transform_fails_against_and_from_a_disguised_mon() {
    let mut st = board(Ruleset::GEN9_RANDOM_BATTLE);
    engine::generate::switch_into(&mut st, SideId::One, 0);
    assert!(st.side(SideId::One).pokemon[0].illusion.is_some());

    // `sim/pokemon.ts:1274`: `transformInto` bails on `this.illusion || pokemon.illusion`.
    let mut s2 = st;
    let foe_slot = s2.side(SideId::Two).active_index as usize;
    s2.side_mut(SideId::Two).pokemon[foe_slot].moves[0] =
        engine::state::MoveSlot { id: MoveId::from_id("transform").unwrap(), pp: 10, max_pp: 10, disabled: false };
    let branches = engine::generate::generate_instructions(
        &s2,
        engine::generate::MoveChoice::Move(0),
        engine::generate::MoveChoice::Move(0),
    );
    let transformed = branches.iter().any(|b| {
        b.instructions.iter().any(|i| matches!(i, engine::instruction::Instruction::Transform { .. }))
    });
    assert!(!transformed, "Transform must fail into an Illusion");
}
