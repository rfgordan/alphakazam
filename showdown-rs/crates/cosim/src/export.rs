//! Engine `State` -> PS `serializeBattle`-shaped JSON (the inverse of `convert.rs`).
//!
//! `convert.rs` is the Rosetta stone: everything it READS out of a PS snapshot, this module
//! WRITES back, using the same serialization conventions (the `startingTurn`/`endingTurn`
//! wish/futuremove encodings, the `stall` counter = 3^n form, the `[Move:x]`/`[Species:x]`/
//! `[Pokemon:pNl]` reference syntax, the sleep-clause source/target side encoding, ...). The
//! certification bar is the round-trip identity `convert(export(convert(x))) == convert(x)` on
//! every corpus decision state (see the ROUNDTRIP_GATE mode in `main.rs`).
//!
//! Two consumers:
//!   1. Round-trip gate — pure Rust, needs only the fields `convert` reads.
//!   2. Transplant / spot-check — the emitted JSON is loadable by PS `State.deserializeBattle`,
//!      so a Rust state can be dropped into pinned PS and driven forward. That path needs the
//!      structural scaffolding too (per-mon `set`, the `team` ordering string, side identity,
//!      `prng`/`prngSeed`, `formatid`, an empty `queue`).
//!
//! ## Decision-boundary restriction
//! Export is only defined at a CLEAN decision boundary — a request point where PS's action
//! queue is empty/canonical (turn-start `move` requests; also the terminal state). We emit
//! `queue: []`. Mid-turn `switch`/replace boundaries carry a non-empty pending queue that the
//! engine `State` does not model, so a transplant there would resume with a truncated queue;
//! the round-trip gate (a pure state compare) is unaffected and still runs over every state.
//!
//! ## Engine-missing field: the EV/IV/nature spread
//! `convert.rs` bakes spreads into `storedStats` (nature := Serious, evs := 0) — the original
//! spread is genuinely not modeled. The exported per-mon `set` therefore carries a PLACEHOLDER
//! spread (evs 0 / ivs 31 / Serious) with the true `storedStats`/`baseStoredStats` written
//! alongside (PS overwrites the computed stats with these on deserialize). This is exact for
//! everything that reads `storedStats` at runtime; the only values that re-read the raw spread
//! are a mid-battle forme-change stat recompute and Transform-from-set (there are none of the
//! latter) — see the transplant harness for how it is handled/documented.

use engine::ids::{Item, MoveId, Type};
use engine::state::{PendingMove, Pokemon, Side, State};
use engine::volatile::VolatileStatus;
use serde_json::{json, Map, Value};

const POSITIONS: &[u8] = b"abcdefghijklmnopqrstuvwx";

/// A `[Pokemon:pNl]` reference: side digit (0-based `si`) + a position letter for `pos`.
fn pokemon_ref(si: usize, pos: usize) -> String {
    format!("[Pokemon:p{}{}]", si + 1, POSITIONS[pos] as char)
}

/// The PS type id for a serialized `types`/`teraType` slot. `Type::None` -> "" (PS's typeless
/// slot; `convert`'s `to_id` maps "" back to `Type::None`).
fn type_id(t: Type) -> &'static str {
    if t == Type::None { "" } else { t.to_id() }
}

/// Emit `types` the way PS's serialized array reads under `convert` (which walks slots
/// positionally with `.take(2).enumerate()`). Slot POSITION is significant: Burn Up / Double
/// Shock leave a typeless FIRST slot (`[None, Fighting]` -> `["", "fighting"]`), so we cannot
/// compact out `None`. We keep every slot up to the last non-`None` (mapping `None` -> ""),
/// trimming only trailing `None` — matching PS's one-element array for a plain single-type mon.
fn types_array(types: [Type; 2]) -> Value {
    let last = types.iter().rposition(|t| *t != Type::None);
    let mut a = Vec::new();
    match last {
        None => a.push(Value::String(String::new())), // fully typeless (???): one empty slot
        Some(n) => {
            for t in types.iter().take(n + 1) {
                a.push(Value::String(type_id(*t).to_string()));
            }
        }
    }
    Value::Array(a)
}

fn stats_obj(stats: &[i16; 6], with_hp: bool) -> Value {
    let mut m = Map::new();
    if with_hp {
        m.insert("hp".into(), json!(stats[0]));
    }
    for (i, k) in ["atk", "def", "spa", "spd", "spe"].iter().enumerate() {
        m.insert((*k).into(), json!(stats[i + 1]));
    }
    Value::Object(m)
}

/// Build the active mon's `volatiles` object from the side's volatile bitset + payload fields,
/// mirroring `convert_volatiles` exactly (each PS key with the internals `convert` reads back).
fn volatiles_obj(side: &Side) -> Value {
    let mut v = Map::new();
    let vs = side.volatiles;
    let simple = |m: &mut Map<String, Value>, k: &str| {
        m.insert(k.into(), json!({}));
    };
    use VolatileStatus::*;
    if vs.contains(Confusion) {
        v.insert("confusion".into(), json!({ "time": side.confusion_turns }));
    }
    if vs.contains(Substitute) {
        v.insert("substitute".into(), json!({ "hp": side.substitute_hp }));
    }
    if vs.contains(LeechSeed) {
        simple(&mut v, "leechseed");
    }
    if vs.contains(Taunt) {
        v.insert("taunt".into(), json!({ "duration": side.taunt_turns }));
    }
    if vs.contains(Encore) {
        v.insert("encore".into(), json!({ "move": side.encore.0.to_id(), "duration": side.encore.1 }));
    }
    if vs.contains(Disable) {
        v.insert("disable".into(), json!({ "move": side.disable.0.to_id(), "duration": side.disable.1 }));
    }
    if vs.contains(Yawn) {
        v.insert("yawn".into(), json!({ "duration": side.yawn_turns }));
    }
    if vs.contains(ThroatChop) {
        v.insert("throatchop".into(), json!({ "duration": side.throat_chop_turns }));
    }
    if vs.contains(HealBlock) {
        v.insert("healblock".into(), json!({ "duration": side.heal_block_turns }));
    }
    if vs.contains(PerishSong) {
        // `convert` maps `perish{n}` -> perish_turns = n (the 3->1 countdown). A bare
        // `perishsong` with `duration` is the other accepted form; the numbered key is canonical.
        let n = side.perish_turns;
        if (1..=3).contains(&n) {
            simple(&mut v, &format!("perish{n}"));
        } else {
            v.insert("perishsong".into(), json!({ "duration": n }));
        }
    }
    // PS `stall` volatile: present for as long as its residual-handler lifetime (`duration` =
    // stall_turns), carrying the success denominator `counter` = 3^stall_counter. No engine
    // volatile bit — `convert` keys it purely on the presence of the `stall` volatile.
    if side.stall_turns > 0 {
        let counter = 3u32.pow(side.stall_counter as u32);
        v.insert("stall".into(), json!({ "counter": counter, "duration": side.stall_turns }));
    }
    if vs.contains(ChoiceLock) {
        simple(&mut v, "choicelock");
    }
    if vs.contains(SaltCure) {
        simple(&mut v, "saltcure");
    }
    if vs.contains(Curse) {
        simple(&mut v, "curse");
    }
    if vs.contains(Nightmare) {
        simple(&mut v, "nightmare");
    }
    if vs.contains(Attract) {
        simple(&mut v, "attract");
    }
    if vs.contains(Torment) {
        simple(&mut v, "torment");
    }
    if vs.contains(DestinyBond) {
        simple(&mut v, "destinybond");
    }
    if vs.contains(GlaiveRush) {
        simple(&mut v, "glaiverush");
    }
    if vs.contains(PartiallyTrapped) {
        v.insert(
            "partiallytrapped".into(),
            json!({ "duration": side.partial_trap_turns, "boundDivisor": side.partial_trap_div }),
        );
    }
    if vs.contains(Trapped) {
        simple(&mut v, "trapped");
    }
    if vs.contains(Ingrain) {
        simple(&mut v, "ingrain");
    }
    if vs.contains(NoRetreat) {
        simple(&mut v, "noretreat");
    }
    if vs.contains(Octolock) {
        simple(&mut v, "octolock");
    }
    if vs.contains(FlashFire) {
        simple(&mut v, "flashfire");
    }
    if vs.contains(Truant) {
        simple(&mut v, "truant");
    }
    if vs.contains(Protosynthesis) {
        simple(&mut v, "protosynthesis");
    }
    if vs.contains(QuarkDrive) {
        simple(&mut v, "quarkdrive");
    }
    if vs.contains(FocusEnergy) {
        simple(&mut v, "focusenergy");
    }
    if vs.contains(Unburden) {
        simple(&mut v, "unburden");
    }
    if vs.contains(Roost) {
        simple(&mut v, "roost");
    }
    if vs.contains(Protect) {
        simple(&mut v, "protect");
    }
    if vs.contains(Endure) {
        simple(&mut v, "endure");
    }
    if vs.contains(Flinch) {
        simple(&mut v, "flinch");
    }
    if vs.contains(Charge) {
        simple(&mut v, "charge");
    }
    if vs.contains(Roosted) {
        simple(&mut v, "roost");
    }
    // Multi-turn commitment. The `MustRecharge` / `LockedMove` volatile bits are set in lockstep
    // with `pending_move` by `convert` (its `mustrecharge`/`lockedmove` arms set both), so we
    // drive them from `pending_move` here and do NOT emit them from the bitset above.
    match side.pending_move {
        PendingMove::None => {}
        PendingMove::Recharging => {
            simple(&mut v, "mustrecharge");
        }
        PendingMove::Charging(mv) => {
            // Two-turn charge/semi-invuln: PS adds `twoturnmove` (which `convert` reads for the
            // charging move) plus a per-move marker volatile which `convert` skips — omit it.
            v.insert("twoturnmove".into(), json!({ "move": mv.to_id() }));
        }
        PendingMove::Rampaging(mv, remaining) => {
            v.insert(
                "lockedmove".into(),
                json!({ "move": mv.to_id(), "trueDuration": remaining, "duration": 2 }),
            );
        }
    }
    Value::Object(v)
}

/// Serialize one Pokemon. `si`/`slot` locate it for reference synthesis; `active` marks the
/// side's active mon; `foe_ref` is a `[Pokemon:...]` on the other side used for the sleep-clause
/// source encoding.
fn export_pokemon(p: &Pokemon, si: usize, slot: usize, active: bool, side: &Side) -> Value {
    let mut m = Map::new();
    let species_id = p.species.to_id();
    let base_species_id = p.base_species.to_id();

    // `details` — PS uses the BASE forme + level. `convert` reads only the level (`, L{n}`) and,
    // as a fallback identity, `species_id_of_details`. The working species comes from the
    // `[Species:x]` ref below.
    let mut details = base_species_id.to_string();
    if p.level != 100 {
        details.push_str(&format!(", L{}", p.level));
    }
    m.insert("details".into(), json!(details));
    m.insert("species".into(), json!(format!("[Species:{species_id}]")));
    m.insert("baseSpecies".into(), json!(format!("[Species:{base_species_id}]")));
    m.insert("rosterIndex".into(), json!(slot));
    m.insert("position".into(), json!(slot));
    m.insert("isActive".into(), json!(active));

    m.insert("gender".into(), json!(match p.gender { 1 => "M", 2 => "F", _ => "" }));

    m.insert("types".into(), types_array(p.types));
    m.insert("baseTypes".into(), types_array(p.base_types));

    if p.transformed {
        m.insert("transformed".into(), json!(true));
    }

    // Status + statusState (sleep `time`, toxic `stage`, and the sleep-clause source/target
    // side encoding `convert` reads: foe-sourced sleep <=> source & target on different sides).
    let mut status_state = Map::new();
    match p.status {
        engine::ids::Status::None => {
            m.insert("status".into(), json!(""));
        }
        st => {
            m.insert("status".into(), json!(st.to_id()));
            status_state.insert("id".into(), json!(st.to_id()));
            if st == engine::ids::Status::Sleep {
                status_state.insert("time".into(), json!(p.status_counter));
            }
            if st == engine::ids::Status::Toxic {
                status_state.insert("stage".into(), json!(p.status_counter));
            }
            // Sleep-clause bookkeeping is only read for a Sleep status.
            if st == engine::ids::Status::Sleep {
                let self_ref = pokemon_ref(si, slot);
                status_state.insert("target".into(), json!(self_ref));
                let src = if p.slept_by_foe {
                    pokemon_ref(si ^ 1, 0) // any position on the other side; convert reads only "pN"
                } else {
                    self_ref.clone()
                };
                status_state.insert("source".into(), json!(src));
            }
        }
    }
    m.insert("statusState".into(), Value::Object(status_state));

    m.insert("ability".into(), json!(p.ability.to_id()));
    m.insert("baseAbility".into(), json!(p.base_ability.to_id()));
    m.insert("item".into(), json!(if p.item == Item::None { "" } else { p.item.to_id() }));

    // abilityState: Protean/Libero once-per-switch marker (-> TypeShifted) and Cud Chew counter.
    let mut ability_state = Map::new();
    if side.volatiles.contains(VolatileStatus::TypeShifted) && active {
        if p.ability == engine::ids::Ability::Libero {
            ability_state.insert("libero".into(), json!(true));
        } else {
            ability_state.insert("protean".into(), json!(true));
        }
    }
    if p.cudchew_turns > 0 {
        ability_state.insert("counter".into(), json!(p.cudchew_turns));
    }
    if !ability_state.is_empty() {
        m.insert("abilityState".into(), Value::Object(ability_state));
    }

    // Move slots.
    let mut slots = Vec::new();
    for ms in p.moves.iter() {
        if ms.id == MoveId::None {
            continue;
        }
        slots.push(json!({
            "id": ms.id.to_id(),
            "move": ms.id.to_id(),
            "pp": ms.pp,
            "maxpp": ms.max_pp,
            "disabled": ms.disabled,
        }));
    }
    m.insert("moveSlots".into(), Value::Array(slots));

    // Stats. `storedStats` has no `hp` key in PS; `convert` takes HP from `maxhp`.
    m.insert("maxhp".into(), json!(p.max_hp));
    m.insert("hp".into(), json!(p.hp));
    m.insert("storedStats".into(), stats_obj(&p.stats, false));
    m.insert("baseStoredStats".into(), stats_obj(&p.base_stats, true));

    // Tera. PS serializes `terastallized` as the tera-type NAME when active, absent otherwise.
    if p.terastallized {
        m.insert("terastallized".into(), json!(p.tera_type.to_id()));
    }
    m.insert("teraType".into(), json!(type_id(p.tera_type)));

    // Battle-long per-mon history.
    if p.ability_used {
        m.insert("swordBoost".into(), json!(true));
    }
    m.insert("timesAttacked".into(), json!(p.times_hit));
    if p.last_berry != Item::None {
        m.insert("ateBerry".into(), json!(true));
        m.insert("lastItem".into(), json!(p.last_berry.to_id()));
    }

    // Active-only per-mon flags `convert` reads off the active.
    if active {
        m.insert("activeTurns".into(), json!(side.active_turns));
        if side.volatiles.contains(VolatileStatus::StatsRaisedThisTurn) {
            m.insert("statsRaisedThisTurn".into(), json!(true));
        }
        if side.volatiles.contains(VolatileStatus::StatsLoweredThisTurn) {
            m.insert("statsLoweredThisTurn".into(), json!(true));
        }
        // boosts
        let mut b = Map::new();
        for (i, k) in ["atk", "def", "spa", "spd", "spe", "accuracy", "evasion"].iter().enumerate() {
            b.insert((*k).into(), json!(side.boosts[i]));
        }
        m.insert("boosts".into(), Value::Object(b));
        // lastMove: the id + a hitTargets that encodes `last_move_failed` (empty == failed).
        if side.last_used_move != MoveId::None {
            let mut lm = Map::new();
            lm.insert("move".into(), json!(format!("[Move:{}]", side.last_used_move.to_id())));
            if side.last_move_failed {
                lm.insert("hitTargets".into(), json!([]));
            }
            m.insert("lastMove".into(), Value::Object(lm));
        }
        m.insert("volatiles".into(), volatiles_obj(side));
    } else {
        m.insert("volatiles".into(), json!({}));
    }

    // --- transplant scaffolding: a self-contained PokemonSet (placeholder spread) --------------
    m.insert("set".into(), export_set(p));

    Value::Object(m)
}

/// A minimal, valid `PokemonSet` synthesized from modeled fields (placeholder EV/IV/nature —
/// see the module header). Used by `deserializeBattle` to instantiate the mon; every stat is
/// overwritten by the serialized `storedStats`/`baseStoredStats` immediately afterward.
fn export_set(p: &Pokemon) -> Value {
    let moves: Vec<Value> = p
        .moves
        .iter()
        .filter(|ms| ms.id != MoveId::None)
        .map(|ms| json!(ms.id.to_id()))
        .collect();
    json!({
        "name": p.base_species.to_id(),
        "species": p.base_species.to_id(),
        "item": if p.item == Item::None { String::new() } else { p.item.to_id().to_string() },
        "ability": p.ability.to_id(),
        "moves": moves,
        "nature": "Serious",
        // Explicit gender avoids a `sample(['M','F'])` construction draw for dual-gender species.
        "gender": match p.gender { 1 => "M", 2 => "F", _ => "N" },
        "evs": { "hp": 0, "atk": 0, "def": 0, "spa": 0, "spd": 0, "spe": 0 },
        "ivs": { "hp": 31, "atk": 31, "def": 31, "spa": 31, "spd": 31, "spe": 31 },
        "level": p.level,
        "teraType": if p.tera_type == Type::None { String::from("Normal") } else { p.tera_type.to_id().to_string() },
    })
}

fn export_side(state: &State, si: usize) -> Value {
    let side = &state.sides[si];
    let active_index = side.active_index as usize;
    let has_active = side.active_index != u8::MAX;

    // Emit non-empty roster slots in slot order; track the array index of each slot for refs.
    let mut mons = Vec::new();
    let mut slot_to_arrayidx = [None; 6];
    for slot in 0..6usize {
        let p = &side.pokemon[slot];
        if p.species == engine::ids::Species::None {
            continue;
        }
        slot_to_arrayidx[slot] = Some(mons.len());
        let active = has_active && slot == active_index;
        mons.push(export_pokemon(p, si, slot, active, side));
    }

    // `team`: the /team-style ordering string PS uses to restore array order on deserialize.
    // We already emit in canonical slot order, so it is the identity permutation.
    let team: String = (1..=mons.len()).map(|n| n.to_string()).collect();

    let mut s = Map::new();
    s.insert("id".into(), json!(format!("p{}", si + 1)));
    s.insert("n".into(), json!(si));
    s.insert("name".into(), json!(if si == 0 { "Red" } else { "Blue" }));
    s.insert("pokemon".into(), Value::Array(mons));
    s.insert("team".into(), json!(team));
    s.insert("teraUsed".into(), json!(side.tera_used));

    // Side conditions.
    let sc = &side.side_conditions;
    let mut conds = Map::new();
    let side_ref = format!("[Side:p{}]", si + 1);
    let cond = |conds: &mut Map<String, Value>, id: &str, extra: Value| {
        let mut o = json!({ "id": id, "target": side_ref });
        if let (Some(base), Value::Object(ex)) = (o.as_object_mut(), &extra) {
            for (k, v) in ex {
                base.insert(k.clone(), v.clone());
            }
        }
        conds.insert(id.into(), o);
    };
    if sc.stealth_rock {
        cond(&mut conds, "stealthrock", json!({}));
    }
    if sc.spikes > 0 {
        cond(&mut conds, "spikes", json!({ "layers": sc.spikes }));
    }
    if sc.toxic_spikes > 0 {
        cond(&mut conds, "toxicspikes", json!({ "layers": sc.toxic_spikes }));
    }
    if sc.sticky_web {
        cond(&mut conds, "stickyweb", json!({}));
    }
    if sc.reflect > 0 {
        cond(&mut conds, "reflect", json!({ "duration": sc.reflect }));
    }
    if sc.light_screen > 0 {
        cond(&mut conds, "lightscreen", json!({ "duration": sc.light_screen }));
    }
    if sc.aurora_veil > 0 {
        cond(&mut conds, "auroraveil", json!({ "duration": sc.aurora_veil }));
    }
    if sc.tailwind > 0 {
        cond(&mut conds, "tailwind", json!({ "duration": sc.tailwind }));
    }
    s.insert("sideConditions".into(), Value::Object(conds));

    // Slot conditions (position 0 slot in singles).
    let mut slotcond = Map::new();
    if side.wish.0 > 0 {
        // `convert`: turns == 2 <=> turn <= startingTurn + 1. Pick startingTurn to reproduce it.
        let starting = if side.wish.0 >= 2 { state.turn } else { state.turn.saturating_sub(2) };
        slotcond.insert(
            "wish".into(),
            json!({ "id": "wish", "target": side_ref, "isSlotCondition": true, "hp": side.wish.1, "startingTurn": starting }),
        );
    }
    if side.future_sight.0 > 0 {
        // `convert`: remaining = endingTurn + 2 - turn -> endingTurn = turn + remaining - 2.
        let ending = state.turn as i64 + side.future_sight.0 as i64 - 2;
        // The caster ref must point at the mon whose `rosterIndex` == future_sight.1. That mon is
        // the Future Sight user, on the OPPOSITE side; find its array index for the position letter.
        let caster_slot = side.future_sight.1 as usize;
        let caster_side = si ^ 1;
        let caster_arrayidx = state.sides[caster_side]
            .pokemon
            .iter()
            .take(caster_slot + 1)
            .enumerate()
            .filter(|(_, p)| p.species != engine::ids::Species::None)
            .count()
            .saturating_sub(1);
        let source = pokemon_ref(caster_side, caster_arrayidx);
        slotcond.insert(
            "futuremove".into(),
            json!({ "id": "futuremove", "target": side_ref, "isSlotCondition": true, "endingTurn": ending, "source": source, "move": "futuresight" }),
        );
    }
    if side.healing_wish {
        slotcond.insert(
            "healingwish".into(),
            json!({ "id": "healingwish", "target": side_ref, "isSlotCondition": true }),
        );
    }
    // PS serializes slotConditions as an array (one entry per active slot).
    s.insert("slotConditions".into(), json!([Value::Object(slotcond)]));

    Value::Object(s)
}

/// Export a full engine `State` as a PS `serializeBattle`-shaped JSON object. `seed` is written
/// verbatim into both `prng` (the live PRNG state) and `prngSeed` (the construction seed); a
/// caller replaying with a forced-PRNG shim overrides the live PRNG anyway.
pub fn export_state(state: &State, seed: [u16; 4]) -> Value {
    let mut field = Map::new();
    let (weather_id, weather_dur) = (state.weather.to_id(), state.weather_turns);
    field.insert("weather".into(), json!(weather_id));
    field.insert(
        "weatherState".into(),
        if state.weather == engine::ids::Weather::None {
            json!({ "id": "", "effectOrder": 0 })
        } else {
            json!({ "id": weather_id, "duration": weather_dur })
        },
    );
    let terrain_id = state.terrain.to_id();
    field.insert("terrain".into(), json!(terrain_id));
    field.insert(
        "terrainState".into(),
        if state.terrain == engine::ids::Terrain::None {
            json!({ "id": "", "effectOrder": 0 })
        } else {
            json!({ "id": terrain_id, "duration": state.terrain_turns })
        },
    );
    let mut pseudo = Map::new();
    if state.trick_room {
        pseudo.insert("trickroom".into(), json!({ "id": "trickroom", "duration": state.trick_room_turns }));
    }
    field.insert("pseudoWeather".into(), Value::Object(pseudo));

    let seed_arr = json!([seed[0], seed[1], seed[2], seed[3]]);
    json!({
        "formatid": "gen9customgame",
        "turn": state.turn,
        "ended": false,
        "winner": null,
        "field": Value::Object(field),
        "sides": [export_side(state, 0), export_side(state, 1)],
        // Transplant scaffolding.
        "prng": seed_arr,
        "prngSeed": seed_arr,
        "queue": [],
        "hints": [],
        "log": [],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{convert_state, Canonical};

    /// Round-trip a State through export -> convert and assert byte-identity.
    fn assert_roundtrip(state: &State) {
        let json = export_state(state, [1, 2, 3, 4]);
        let canon = Canonical::from_first_state(&json).expect("canonical");
        let back = convert_state(&json, &canon).expect("convert exported json");
        assert_eq!(&back, state, "round-trip mismatch");
    }

    fn sp(id: &str) -> engine::ids::Species {
        engine::ids::Species::from_id(id).unwrap()
    }
    fn mv(id: &str) -> MoveId {
        MoveId::from_id(id).unwrap()
    }

    fn base_mon(species: &str, mvid: &str) -> Pokemon {
        let mut p = Pokemon::EMPTY;
        p.species = sp(species);
        p.base_species = p.species;
        p.level = 100;
        p.types = [Type::Normal, Type::None];
        p.base_types = p.types;
        p.hp = 200;
        p.max_hp = 200;
        p.stats = [200, 150, 120, 110, 100, 130];
        p.base_stats = p.stats;
        p.ability = engine::ids::Ability::None;
        p.base_ability = p.ability;
        p.moves[0] = engine::state::MoveSlot { id: mv(mvid), pp: 10, max_pp: 16, disabled: false };
        // convert sets base_moves == moves for a non-transformed mon; mirror that invariant so
        // the fixture is a valid convert-image (the domain the round-trip identity is defined on).
        p.base_moves = p.moves;
        p
    }

    #[test]
    fn roundtrip_minimal() {
        let mut s = State::EMPTY;
        s.turn = 3;
        s.sides[0].pokemon[0] = base_mon("blissey", "softboiled");
        s.sides[0].active_index = 0;
        s.sides[1].pokemon[0] = base_mon("corviknight", "roost");
        s.sides[1].active_index = 0;
        assert_roundtrip(&s);
    }

    #[test]
    fn roundtrip_rich() {
        let mut s = State::EMPTY;
        s.turn = 12;
        s.weather = engine::ids::Weather::Snow;
        s.weather_turns = 4;
        s.trick_room = true;
        s.trick_room_turns = 3;

        let mut a = base_mon("blissey", "softboiled");
        a.status = engine::ids::Status::Toxic;
        a.status_counter = 3;
        a.hp = 90;
        a.terastallized = true;
        a.tera_type = Type::Water;
        a.types = [Type::Water, Type::None];
        s.sides[0].pokemon[0] = a;
        s.sides[0].active_index = 0;
        s.sides[0].boosts = [1, -2, 0, 0, 3, 0, 0];
        s.sides[0].volatiles.insert(VolatileStatus::Confusion);
        s.sides[0].confusion_turns = 2;
        s.sides[0].volatiles.insert(VolatileStatus::Substitute);
        s.sides[0].substitute_hp = 60;
        s.sides[0].volatiles.insert(VolatileStatus::LeechSeed);
        s.sides[0].side_conditions.stealth_rock = true;
        s.sides[0].side_conditions.spikes = 2;
        s.sides[0].side_conditions.reflect = 5;
        s.sides[0].tera_used = true;
        s.sides[0].active_turns = 4;
        s.sides[0].last_used_move = mv("softboiled");
        s.sides[0].wish = (2, 100);

        let mut b = base_mon("corviknight", "roost");
        b.status = engine::ids::Status::Sleep;
        b.status_counter = 2;
        b.slept_by_foe = true;
        s.sides[1].pokemon[0] = b;
        s.sides[1].pokemon[1] = base_mon("toxapex", "recover");
        s.sides[1].active_index = 0;
        s.sides[1].encore = (mv("roost"), 3);
        s.sides[1].volatiles.insert(VolatileStatus::Encore);
        s.sides[1].pending_move = PendingMove::Rampaging(mv("outrage"), 2);
        s.sides[1].volatiles.insert(VolatileStatus::LockedMove);
        s.sides[1].partial_trap_turns = 4;
        s.sides[1].partial_trap_div = 8;
        s.sides[1].volatiles.insert(VolatileStatus::PartiallyTrapped);

        assert_roundtrip(&s);
    }
}
