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

/// PS type NAME for a serialized type slot: the capitalized form PS stores in `pokemon.types`
/// and compares at runtime (`hasType`/STAB use `"Poison"`, not the `"poison"` id — emitting the
/// id silently drops STAB). `Type::None` -> "" (PS's typeless slot). `convert` lowercases via
/// `to_id`, so the round-trip is unaffected.
fn type_name(t: Type) -> String {
    if t == Type::None {
        return String::new();
    }
    let id = t.to_id();
    let mut c = id.chars();
    match c.next() {
        Some(first) => first.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
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
                a.push(Value::String(type_name(*t)));
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
        // PS's choicelock stores the locked move (= the last move used under the Choice item) in
        // its effectState and re-disables the other slots from it each request. The engine derives
        // the lock from last_used_move, so emit that as the choicelock `move`; without it PS can't
        // reconstruct which slots to disable.
        v.insert("choicelock".into(), json!({ "move": side.last_used_move.to_id() }));
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
fn export_pokemon(p: &Pokemon, si: usize, slot: usize, arr_idx: usize, active: bool, side: &Side) -> Value {
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
    m.insert("position".into(), json!(arr_idx));
    m.insert("isActive".into(), json!(active));

    m.insert("gender".into(), json!(match p.gender { 1 => "M", 2 => "F", _ => "" }));

    // PS keeps the RAW (pre-tera) `types` array and applies Tera at lookup via `terastallized`.
    // `convert` overwrites `State.types` to the effective [tera] for a non-Stellar Tera, so the
    // raw types are recovered from `base_types` in that case (they coincide otherwise). `convert`
    // re-derives the effective typing from `terastallized` on the way back, so this round-trips.
    let raw_types = if p.terastallized && p.tera_type != Type::Stellar {
        p.base_types
    } else {
        p.types
    };
    m.insert("types".into(), types_array(raw_types));
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
    // Supreme Overlord snapshots `fallen = min(side.totalFainted, 5)` into abilityState AT
    // switch-in and boosts base power by it. The engine recomputes it from the live fainted count
    // (equal to the snapshot while no ally faints during this mon's stay — the corpus invariant),
    // so we derive it here from the current fainted party members. Only the active attacker uses it.
    if active && p.ability == engine::ids::Ability::SupremeOverlord {
        let fallen = side
            .pokemon
            .iter()
            .filter(|m| m.species != engine::ids::Species::None && m.hp <= 0)
            .count()
            .min(5);
        if fallen > 0 {
            ability_state.insert("fallen".into(), json!(fallen));
        }
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

    // Stats. `storedStats` has no `hp` key in PS; `convert` takes HP from `maxhp`. `baseMaxhp`
    // must be set too: PS's residual/heal maths (Leftovers, Recover, Leech Seed, ...) divide
    // `baseMaxhp`, and left unset it defaults to the SYNTHETIC set's 0-EV HP — wrong. For a
    // non-Dynamax singles mon baseMaxhp == maxhp.
    m.insert("maxhp".into(), json!(p.max_hp));
    m.insert("baseMaxhp".into(), json!(p.max_hp));
    m.insert("hp".into(), json!(p.hp));
    m.insert("storedStats".into(), stats_obj(&p.stats, false));
    m.insert("baseStoredStats".into(), stats_obj(&p.base_stats, true));

    // Tera. PS serializes `terastallized` as the tera-type NAME when active, absent otherwise.
    if p.terastallized {
        m.insert("terastallized".into(), json!(type_name(p.tera_type)));
    }
    m.insert("teraType".into(), json!(type_name(p.tera_type)));
    // `canTerastallize`: PS nulls it for the WHOLE side once any ally teras (and for a mon that
    // already tera'd). Without it the transplant's side reads as tera-available and can tera again
    // — a cascade. Emit the tera type when the side still has its Tera, else null.
    if side.tera_used || p.terastallized || p.tera_type == Type::None {
        m.insert("canTerastallize".into(), Value::Null);
    } else {
        m.insert("canTerastallize".into(), json!(type_name(p.tera_type)));
    }

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
        // lastMove: PS's `pokemon.lastMove` is an ActiveMove serialized as an object. It MUST carry
        // `hit` so PS's `isActiveMove` recognizes it on deserialize and rebuilds a Move-backed
        // ActiveMove (with `.flags`, `.id`, ...); without it PS keeps a bare object and effects like
        // Encore's onStart (`move.flags['failencore']`) crash. `hitTargets` empty encodes
        // `last_move_failed` (what convert reads).
        if side.last_used_move != MoveId::None {
            let mut lm = Map::new();
            lm.insert("hit".into(), json!(1));
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

/// (nature name, plus stat index, minus stat index). Indices: 1=atk 2=def 3=spa 4=spd 5=spe.
/// Neutral natures (plus==minus) are represented once as `Serious`.
const NATURES: &[(&str, usize, usize)] = &[
    ("Serious", 0, 0), // neutral
    ("Adamant", 1, 3), ("Lonely", 1, 2), ("Brave", 1, 5), ("Naughty", 1, 4),
    ("Bold", 2, 1), ("Impish", 2, 3), ("Relaxed", 2, 5), ("Lax", 2, 4),
    ("Modest", 3, 1), ("Mild", 3, 2), ("Quiet", 3, 5), ("Rash", 3, 4),
    ("Calm", 4, 1), ("Gentle", 4, 2), ("Sassy", 4, 5), ("Careful", 4, 3),
    ("Timid", 5, 1), ("Hasty", 5, 2), ("Jolly", 5, 3), ("Naive", 5, 4),
];

/// PS `spreadModify` for one stat: `nature_mod` is +1 (×1.1), 0 (×1.0), or -1 (×0.9). Mirrors
/// the exact truncation order in sim/pokemon.ts (`spreadModify`).
fn ps_stat(base: i32, ev: i32, level: i32, is_hp: bool, nature_mod: i32) -> i32 {
    let iv = 31;
    let inner = 2 * base + iv + ev / 4;
    if is_hp {
        if base == 1 {
            return 1; // Shedinja
        }
        return (inner + 100) * level / 100 + 10;
    }
    let v = inner * level / 100 + 5;
    match nature_mod {
        m if m > 0 => v * 11 / 10,
        m if m < 0 => v * 9 / 10,
        _ => v,
    }
}

/// Find an EV in 0..=252 that makes `ps_stat` equal `target` for this stat, or `None`.
fn solve_ev(base: i32, target: i32, level: i32, is_hp: bool, nature_mod: i32) -> Option<u8> {
    (0..=252).find(|&ev| ps_stat(base, ev, level, is_hp, nature_mod) == target).map(|ev| ev as u8)
}

/// Recover a (nature, EVs, ivs=31) spread that makes PS's `spreadModify(species_base, set)`
/// reproduce the exact target `stats` — so a switch-in / forme-change `setSpecies` (which
/// recomputes `storedStats` FROM THE SET off the SPECIES DEX base stats) yields the same values
/// the engine holds. The engine doesn't model the original spread (convert bakes it into
/// storedStats), so we reconstruct any spread that hits the targets. Returns `None` if no single
/// nature reproduces every stat (documented residual — falls back to a neutral 0-EV set, exact
/// only until the mon switches in).
fn solve_spread(species_base: &[u16; 6], stats: &[i16; 6], level: u8) -> Option<(&'static str, [u8; 6])> {
    let lvl = level as i32;
    'nat: for &(name, plus, minus) in NATURES {
        let mut evs = [0u8; 6];
        for s in 0..6 {
            let nm = if s == plus && plus != 0 { 1 } else if s == minus && minus != 0 { -1 } else { 0 };
            match solve_ev(species_base[s] as i32, stats[s] as i32, lvl, s == 0, nm) {
                Some(ev) => evs[s] = ev,
                None => continue 'nat,
            }
        }
        return Some((name, evs));
    }
    None
}

/// A valid `PokemonSet` whose spread reproduces the exact `storedStats`/`baseStoredStats`, so PS
/// `setSpecies` recompute (switch-in / forme change) matches the engine. See `solve_spread`.
fn export_set(p: &Pokemon) -> Value {
    let moves: Vec<Value> = p
        .moves
        .iter()
        .filter(|ms| ms.id != MoveId::None)
        .map(|ms| json!(ms.id.to_id()))
        .collect();
    // Solve against the SPECIES DEX base stats (what `setSpecies`/`spreadModify` uses), targeting
    // the engine's base-forme stats (which a switch-in `setSpecies(baseSpecies)` reproduces).
    let species_base = engine::data::base_stats(p.base_species);
    let (nature, evs) = solve_spread(&species_base, &p.base_stats, p.level).unwrap_or(("Serious", [0; 6]));
    let ev_obj = json!({
        "hp": evs[0], "atk": evs[1], "def": evs[2], "spa": evs[3], "spd": evs[4], "spe": evs[5],
    });
    json!({
        "name": p.base_species.to_id(),
        "species": p.base_species.to_id(),
        "item": if p.item == Item::None { String::new() } else { p.item.to_id().to_string() },
        "ability": p.ability.to_id(),
        "moves": moves,
        "nature": nature,
        // Explicit gender avoids a `sample(['M','F'])` construction draw for dual-gender species.
        "gender": match p.gender { 1 => "M", 2 => "F", _ => "N" },
        "evs": ev_obj,
        "ivs": { "hp": 31, "atk": 31, "def": 31, "spa": 31, "spd": 31, "spe": 31 },
        "level": p.level,
        "teraType": if p.tera_type == Type::None { String::from("Normal") } else { type_name(p.tera_type) },
    })
}

/// Roster slots in PS live-array order: ACTIVE mon first (field position 0), then the remaining
/// non-empty roster slots ascending. PS indexes `slotConditions[position]` and requires the
/// active mon at position 0, so the emitted array must be active-first (not roster order).
fn emit_order(state: &State, si: usize) -> Vec<usize> {
    let side = &state.sides[si];
    let active_index = side.active_index as usize;
    let has_active = side.active_index != u8::MAX;
    let mut order = Vec::new();
    if has_active && side.pokemon[active_index].species != engine::ids::Species::None {
        order.push(active_index);
    }
    for slot in 0..6usize {
        if has_active && slot == active_index {
            continue;
        }
        if side.pokemon[slot].species != engine::ids::Species::None {
            order.push(slot);
        }
    }
    order
}

/// Array index (field position) of `roster_slot` on side `si` in the emitted active-first order.
fn array_index_of(state: &State, si: usize, roster_slot: usize) -> usize {
    emit_order(state, si).iter().position(|&s| s == roster_slot).unwrap_or(0)
}

fn export_side(state: &State, si: usize) -> Value {
    let side = &state.sides[si];
    let active_index = side.active_index as usize;
    let has_active = side.active_index != u8::MAX;

    // Emit in active-first order; each mon's `position` is its array index, `rosterIndex` its
    // true (stable) roster slot — the field convert reads to place it back into the engine slot.
    let order = emit_order(state, si);
    let mut mons = Vec::new();
    for (arr_idx, &slot) in order.iter().enumerate() {
        let p = &side.pokemon[slot];
        let active = has_active && slot == active_index;
        mons.push(export_pokemon(p, si, slot, arr_idx, active, side));
    }

    // `team`: identity — the emitted array order is already the live order we want restored.
    let team: String = (1..=mons.len()).map(|n| n.to_string()).collect();

    let mut s = Map::new();
    s.insert("id".into(), json!(format!("p{}", si + 1)));
    s.insert("n".into(), json!(si));
    s.insert("name".into(), json!(if si == 0 { "Red" } else { "Blue" }));
    // Back-reference to the opposing side. `start()`/`restart()` wire `foe`, but deserializeBattle
    // runs `getRequests` (which reads `pokemon.foes()` -> `side.foe.allies()`) BEFORE the caller
    // restarts — so it must be serialized. A full serializeBattle includes it (the recorder's
    // snapshot drops it). Singles: no ally side.
    s.insert("foe".into(), json!(format!("[Side:p{}]", (si ^ 1) + 1)));
    s.insert("allySide".into(), Value::Null);
    // `side.active` — references to the active mon(s). Deserialize resolves these to the Pokemon
    // objects; without it PS leaves side.active = [null] and choice handling dereferences null.
    // The active mon is emitted first (array index 0 == position 'a') in singles.
    let active_ref = if has_active {
        json!([pokemon_ref(si, 0)])
    } else {
        json!([Value::Null])
    };
    s.insert("active".into(), active_ref);
    s.insert("pokemon".into(), Value::Array(mons));
    s.insert("team".into(), json!(team));
    s.insert("teraUsed".into(), json!(side.tera_used));
    // Faint accounting. `totalFainted` is load-bearing: a Supreme Overlord mon switching in during
    // a continuation reads `side.totalFainted` in `onStart` to snapshot its `fallen` boost, and PS
    // uses `pokemonLeft` for win detection. Cumulative `totalFainted` == current fainted count
    // absent revival (a documented edge). `faintedLastTurn`/`faintedThisTurn` reset at a clean
    // turn boundary.
    let fainted = side.pokemon.iter().filter(|m| m.species != engine::ids::Species::None && m.hp <= 0).count();
    let alive = side.pokemon.iter().filter(|m| m.species != engine::ids::Species::None && m.hp > 0).count();
    s.insert("pokemonLeft".into(), json!(alive));
    s.insert("totalFainted".into(), json!(fainted));
    s.insert("faintedThisTurn".into(), Value::Null);
    s.insert("faintedLastTurn".into(), Value::Null);
    // `deserializeChoice` iterates `side.choice`'s keys — it must exist. A clean, no-pending-action
    // choice (the canonical turn-start boundary) with an empty switchIns set.
    s.insert("choice".into(), json!({ "switchIns": [] }));

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
        // the Future Sight user, on the OPPOSITE side; resolve_future_caster reads pokemon[pos]'s
        // rosterIndex, so pos is the caster's array index in that side's active-first ordering.
        let caster_slot = side.future_sight.1 as usize;
        let caster_side = si ^ 1;
        let source = pokemon_ref(caster_side, array_index_of(state, caster_side, caster_slot));
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
    // PS uses "" for absent weather/terrain (the engine's `None.to_id()` is "none").
    let none_weather = state.weather == engine::ids::Weather::None;
    let weather_id = if none_weather { "" } else { state.weather.to_id() };
    field.insert("weather".into(), json!(weather_id));
    field.insert(
        "weatherState".into(),
        if none_weather {
            json!({ "id": "", "effectOrder": 0 })
        } else {
            json!({ "id": weather_id, "duration": state.weather_turns })
        },
    );
    let none_terrain = state.terrain == engine::ids::Terrain::None;
    let terrain_id = if none_terrain { "" } else { state.terrain.to_id() };
    field.insert("terrain".into(), json!(terrain_id));
    field.insert(
        "terrainState".into(),
        if none_terrain {
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
