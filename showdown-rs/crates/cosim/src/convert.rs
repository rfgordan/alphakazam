//! PS `serializeBattle` JSON -> engine `State`, against an explicit manifest.
//!
//! The contract that makes the trichotomy honest: every gameplay-bearing value either maps
//! into the engine (MODELED), is provably re-derivable / turn-local bookkeeping (IGNORED),
//! or conversion FAILS LOUDLY with an `Unsupported` reason (UNMODELED). Nothing is silently
//! defaulted the way the old `verify` converter defaulted unknown abilities/items to `None`.

use std::collections::BTreeMap;

use engine::ids::{Ability, Item, MoveId, Nature, Species, Status, Terrain, Type, Weather};
use engine::state::{MoveSlot, PendingMove, Pokemon, Side, SideId, State};
use engine::volatile::VolatileStatus;
use serde_json::Value;

/// Why a state can't be represented in the engine. These are *coverage* findings, not errors:
/// the report aggregates them so the unmodeled frontier is explicit and ranked.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unsupported(pub String);

type Res<T> = Result<T, Unsupported>;

fn unsup(reason: impl Into<String>) -> Unsupported {
    Unsupported(reason.into())
}

// ---- json helpers ----------------------------------------------------------------

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(Value::as_str).unwrap_or("")
}
fn i(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(Value::as_i64).unwrap_or(0)
}
fn b(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// PS `toID`: lowercase alphanumerics only.
pub fn to_id(name: &str) -> String {
    name.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase()
}

/// Species id from a `details` string ("Slowking-Galar, M" -> "slowkinggalar").
pub fn species_id_of_details(details: &str) -> String {
    to_id(details.split(',').next().unwrap_or(details))
}

// ---- canonical party order ---------------------------------------------------------

/// Engine party slots are fixed; PS reorders its array on switch. The canonical order is the
/// order in the first snapshot, keyed by species id (teams must have unique species per side).
pub struct Canonical {
    /// per side: species id -> canonical slot
    pub slots: [BTreeMap<String, u8>; 2],
}

impl Canonical {
    pub fn from_first_state(state: &Value) -> Res<Canonical> {
        let mut slots = [BTreeMap::new(), BTreeMap::new()];
        for (si, side) in state["sides"].as_array().ok_or_else(|| unsup("state:no-sides"))?.iter().enumerate() {
            for (pi, p) in side["pokemon"].as_array().ok_or_else(|| unsup("side:no-pokemon"))?.iter().enumerate() {
                let id = species_id_of_details(s(p, "details"));
                if slots[si].insert(id.clone(), pi as u8).is_some() {
                    return Err(unsup(format!("duplicate-species:{id}")));
                }
            }
        }
        Ok(Canonical { slots })
    }

    pub fn slot(&self, side: usize, species_id: &str) -> Res<u8> {
        self.slots[side]
            .get(species_id)
            .copied()
            .ok_or_else(|| unsup(format!("unknown-species-ident:{species_id}")))
    }
}

// ---- the conversion ------------------------------------------------------------------

pub fn convert_state(v: &Value, canon: &Canonical) -> Res<State> {
    let mut state = State::EMPTY;
    let ended = b(v, "ended");
    let sides = v["sides"].as_array().ok_or_else(|| unsup("state:no-sides"))?;
    let turn = i(v, "turn") as u32;
    for (si, side_v) in sides.iter().enumerate() {
        state.sides[si] = convert_side(side_v, si, canon, ended, turn)?;
    }
    // Resolve each pending Future Sight's caster to its canonical party slot. PS serializes the
    // slotCondition's `source` as a position ref `[Pokemon:p{N}{letter}]` (the caster's CURRENT
    // position — PS tracks the specific mon across switches). The caster's canonical slot is the
    // `rosterIndex` of the mon at that position, which is what `future_sight_rolls` indexes.
    for si in 0..sides.len().min(2) {
        if state.sides[si].future_sight.0 == 0 {
            continue;
        }
        if let Some(slot) = resolve_future_caster(sides, si) {
            state.sides[si].future_sight.1 = slot;
        }
    }
    convert_field(&v["field"], &mut state)?;
    state.turn = turn;
    Ok(state)
}

/// Resolve the caster slot for the Future Sight pending on target side `target_si`. Returns the
/// caster's `rosterIndex` (canonical party slot), or `None` if it can't be located.
fn resolve_future_caster(sides: &[Value], target_si: usize) -> Option<u8> {
    let entries: Vec<&Value> = match sides.get(target_si)?.get("slotConditions") {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(Value::Object(o)) => o.values().collect(),
        _ => return None,
    };
    for conds in entries {
        let Some(fm) = conds.get("futuremove") else { continue };
        let src = fm.get("source").and_then(Value::as_str)?;
        // "[Pokemon:p1a]" -> side digit + position letter.
        let inner = src.trim_start_matches("[Pokemon:").trim_end_matches(']');
        let bytes = inner.as_bytes();
        if bytes.len() < 3 || bytes[0] != b'p' {
            return None;
        }
        let src_side = (bytes[1] as char).to_digit(10)? as usize - 1;
        let pos = (bytes[bytes.len() - 1] - b'a') as usize;
        let caster = sides.get(src_side)?.get("pokemon")?.as_array()?.get(pos)?;
        return caster.get("rosterIndex").and_then(Value::as_i64).map(|r| r as u8);
    }
    None
}

fn convert_field(f: &Value, state: &mut State) -> Res<()> {
    let weather = s(f, "weather");
    if !weather.is_empty() {
        state.weather = Weather::from_id(weather).ok_or_else(|| unsup(format!("weather:{weather}")))?;
        state.weather_turns = i(&f["weatherState"], "duration") as i8;
    }
    let terrain = s(f, "terrain");
    if !terrain.is_empty() {
        state.terrain = Terrain::from_id(terrain).ok_or_else(|| unsup(format!("terrain:{terrain}")))?;
        state.terrain_turns = i(&f["terrainState"], "duration") as i8;
    }
    if let Some(pw) = f["pseudoWeather"].as_object() {
        for (k, pv) in pw {
            match k.as_str() {
                "trickroom" => {
                    state.trick_room = true;
                    state.trick_room_turns = i(pv, "duration") as i8;
                }
                // The RULES are registered as field pseudo-weathers at construction time
                // (`sim/battle.ts:295-308`, "timing is early enough to hook into ModifySpecies").
                // `dex.conditions.getByID` short-circuits to the Format object for any id in
                // `data.Rulesets`, so the pseudo-weather's condition IS the rule, with
                // `effectType: 'Rule'`. For `gen9randombattle` exactly one rule qualifies —
                // every other rule in its table defines only lifecycle hooks, which are called
                // directly and never registered. It carries no duration and no state the engine
                // models (`addPseudoWeather` passes no `target`, so `initEffectState` does not
                // even bump `effectOrder`): it is pure handler discovery. The behaviour it
                // discovers is `Ruleset::sleep_clause`, which comes from the format stamp.
                "sleepclausemod" => {}
                other => return Err(unsup(format!("pseudoweather:{other}"))),
            }
        }
    }
    Ok(())
}

fn convert_side(v: &Value, si: usize, canon: &Canonical, ended: bool, turn: u32) -> Res<Side> {
    let mut side = Side::EMPTY;

    // Party, into canonical slots.
    let mons = v["pokemon"].as_array().ok_or_else(|| unsup("side:no-pokemon"))?;
    let mut active: Option<(u8, &Value)> = None;
    for p in mons {
        let id = species_id_of_details(s(p, "details"));
        // Stable identity: the recorder stamps each mon with its battle-start roster slot
        // (immune to PS array reordering and to forme changes renaming `details`).
        let slot = match p.get("rosterIndex").and_then(Value::as_i64) {
            Some(ri) if (0..6).contains(&ri) => ri as u8,
            _ => canon.slot(si, &id)?,
        };
        side.pokemon[slot as usize] = convert_pokemon(p, &id)?;
        if b(p, "isActive") {
            active = Some((slot, p));
        }
    }

    // PS's LIVE `side.pokemon` order, as canonical slots — `Side::roster`. The array index IS
    // `pokemon.position`, which is also what a `[Pokemon:pNx]` reference encodes, so this table is
    // what resolves the `illusion` pointers below. Missing `rosterIndex` (a pre-stamp recording)
    // leaves the identity order, which is what the engine assumes at battle start anyway.
    let mut order: Vec<u8> = Vec::new();
    for p in mons {
        let id = species_id_of_details(s(p, "details"));
        let slot = match p.get("rosterIndex").and_then(Value::as_i64) {
            Some(ri) if (0..6).contains(&ri) => ri as u8,
            _ => canon.slot(si, &id)?,
        };
        order.push(slot);
    }
    // Any party slot PS does not carry (a shorter team, or the terminal state where PS has dropped
    // the loser's active) is appended ascending, so `roster` is always a permutation of 0..6 —
    // `illusion_target` skips the empty ones by species.
    let mut full = order.clone();
    full.extend((0..6u8).filter(|s| !order.contains(s)));
    side.roster.copy_from_slice(&full[..6]);

    // `pokemon.illusion` serializes as `[Pokemon:pNx]` where `x` is the ARRAY index
    // (`sim/state.ts:380`, `POSITIONS = 'abcdef…'`). Resolve through the order table above.
    for p in mons {
        let Some(r) = p.get("illusion").and_then(Value::as_str) else { continue };
        let id = species_id_of_details(s(p, "details"));
        let slot = match p.get("rosterIndex").and_then(Value::as_i64) {
            Some(ri) if (0..6).contains(&ri) => ri as u8,
            _ => canon.slot(si, &id)?,
        };
        let arr = r
            .strip_prefix("[Pokemon:")
            .and_then(|x| x.strip_suffix(']'))
            .and_then(|x| x.chars().nth(2))
            .and_then(|c| "abcdefghijklmnopqrstuvwx".find(c))
            .ok_or_else(|| unsup(format!("illusion-ref:{r}")))?;
        let shown = *order.get(arr).ok_or_else(|| unsup(format!("illusion-ref-oob:{r}")))?;
        side.pokemon[slot as usize].illusion = Some(shown);
    }

    // Side-level tera-used: synthesized by the recorder from live objects (the serializer
    // drops `terastallized` from fainted mons, so the snapshot alone can't recover it).
    side.tera_used = b(v, "teraUsed") || side.pokemon.iter().any(|p| p.terastallized);

    // At a terminal or mid-turn faint/request boundary PS can have no active in the slot.
    // Mark it with a sentinel and let the differ skip active-only comparisons. Side-level state
    // (hazards/screens/slot conditions) still exists and must still be converted.
    let Some((active_slot, active_v)) = active else {
        let _ = ended;
        side.active_index = u8::MAX;
        convert_side_conditions(&v["sideConditions"], &mut side)?;
        convert_slot_conditions(v.get("slotConditions"), &mut side, si, canon, turn)?;
        return Ok(side);
    };
    side.active_index = active_slot;

    // Active-only state: boosts, volatiles, counters, pending move.
    if let Some(boosts) = active_v["boosts"].as_object() {
        for (bi, key) in ["atk", "def", "spa", "spd", "spe", "accuracy", "evasion"].iter().enumerate() {
            side.boosts[bi] = boosts.get(*key).and_then(Value::as_i64).unwrap_or(0) as i8;
        }
    }
    // PS: activeTurns = 0 on switch-in, +1 in every endTurn (battle.ts:1762) — the same
    // convention the engine uses, so convert raw. (PS leads start at 1 because PS runs one
    // endTurn before turn 1; that propagates consistently through both engines.)
    side.active_turns = i(active_v, "activeTurns") as u8;
    if let Some(lm) = active_v.get("lastMove") {
        // Serialized as {move: "[Move:gigadrain]", hit: 1, hitTargets: [...], ...}.
        if let Some(mref) = lm.get("move").and_then(Value::as_str) {
            let id = mref.trim_start_matches("[Move:").trim_end_matches(']');
            side.last_used_move = MoveId::from_id(id).unwrap_or(MoveId::None);
        }
        // PS strips `moveLastTurnResult`, but a last move that hit nothing (empty hitTargets)
        // is the failure case Stomping Tantrum's doubler keys on. Approximate it from lastMove.
        side.last_move_failed = lm
            .get("hitTargets")
            .and_then(Value::as_array)
            .is_some_and(|t| t.is_empty());
    }
    convert_volatiles(active_v, &mut side)?;
    // Roost is the second half of the effective-typing derivation. PS's `roost` condition
    // filters Flying out of `getTypes()` via `onType` (`data/moves.ts:15460`) and never writes
    // `pokemon.types`, so the serialized array still carries Flying; the engine stores the
    // RESOLVED typing, so apply the filter here. `live_types` deliberately keeps PS's array.
    // (`onStart` returns false for a terastallized target, so the volatile never coexists with
    // Tera and no extra guard is needed.)
    if side.volatiles.contains(VolatileStatus::Roosted) {
        let p = &mut side.pokemon[active_slot as usize];
        for t in p.types.iter_mut() {
            if *t == Type::Flying {
                *t = Type::None;
            }
        }
    }
    // `statsRaisedThisTurn` is a per-Pokemon field (not a PS volatile): only the active can
    // have raised a stat this turn, so it maps onto the engine's active-only volatile. Reset
    // by PS at every `endTurn`, so it is only ever set on mid-turn snapshots.
    if b(active_v, "statsRaisedThisTurn") {
        side.volatiles.insert(VolatileStatus::StatsRaisedThisTurn);
    }
    if b(active_v, "statsLoweredThisTurn") {
        side.volatiles.insert(VolatileStatus::StatsLoweredThisTurn);
    }
    // Protean/Libero once-per-switch-in marker lives in abilityState, not volatiles.
    let ast = &active_v["abilityState"];
    if b(ast, "libero") || b(ast, "protean") {
        side.volatiles.insert(VolatileStatus::TypeShifted);
    }

    // Bench mons must not carry volatiles (PS clears them on switch-out; anything left is a
    // mechanic like Baton Pass residue we don't model).
    for p in mons {
        if !b(p, "isActive") {
            if let Some(volatiles) = p["volatiles"].as_object() {
                if let Some((k, _)) = volatiles.iter().next() {
                    return Err(unsup(format!("bench-volatile:{k}")));
                }
            }
        }
    }

    convert_side_conditions(&v["sideConditions"], &mut side)?;
    convert_slot_conditions(v.get("slotConditions"), &mut side, si, canon, turn)?;
    Ok(side)
}

fn convert_pokemon(p: &Value, species_id: &str) -> Res<Pokemon> {
    // The working species is the "[Species:x]" ref whenever present: `details` lags behind
    // for transformed mons AND for non-permanent forme changes (Relic Song's Pirouette,
    // Hunger Switch's Morpeko-Hangry keep base-forme details — cosim caught the engine and
    // PS disagreeing one full forme cycle apart when this read `details`).
    let mut species_id = species_id.to_string();
    if let Some(r) = p.get("species").and_then(Value::as_str) {
        species_id = r.trim_start_matches("[Species:").trim_end_matches(']').to_string();
    }
    let species_id = species_id.as_str();
    let species = Species::from_id(species_id).ok_or_else(|| unsup(format!("species:{species_id}")))?;

    let details = s(p, "details");
    let level = details
        .split(", ")
        .find_map(|part| part.strip_prefix('L').and_then(|l| l.parse::<u8>().ok()))
        .unwrap_or(100);

    // PS's typeless "???" (Double Shock removing Electric, Burn Up, ...) has no alphanumerics, so
    // `to_id` yields "" — map that to the engine's `Type::None` (typeless slot).
    let parse_type = |t: &Value| -> Result<Type, Unsupported> {
        let tid = to_id(t.as_str().unwrap_or(""));
        if tid.is_empty() {
            return Ok(Type::None);
        }
        Type::from_id(&tid).ok_or_else(|| unsup(format!("type:{tid}")))
    };
    let mut types = [Type::None; 2];
    for (ti, t) in p["types"].as_array().into_iter().flatten().take(2).enumerate() {
        types[ti] = parse_type(t)?;
    }
    // `base_types` is what PS's `clearVolatile()` restores the array to — `setSpecies(baseSpecies)`
    // -> `setType(species.types, true)`. That is NOT simply the serialized `baseTypes` field:
    // `baseTypes` is frozen at CONSTRUCTION from `baseSpecies.types` (`sim/pokemon.ts:446-447`),
    // before `setSpecies`' `runEvent('ModifySpecies')` resolves an item-driven forme. A Rusted
    // Shield Zamazenta serializes `types` ["Fighting","Steel"] with `baseTypes` ["Fighting"], and
    // `setSpecies(baseSpecies)` re-runs ModifySpecies, so its restore target is the two-type array
    // (rb1114 / rb1123 / rb1137 / rb1318 / rb1320 / rb1356 / rb1366 all carry a Crowned Zamazenta).
    // So the live array supplies the LENGTH and `baseTypes` corrects the prefix.
    //
    // A TRANSFORMED mon is the one case where the live array carries no information about the base:
    // it is the COPY. There the field must be taken verbatim — a Ditto with `types`
    // ["Ghost","Fire"] and `baseTypes` ["Normal"] reverted to [Normal, Fire] on switch-out
    // (rb1359 d7) under the element-wise patch.
    let mut base_types = if b(p, "transformed") { [Type::None; 2] } else { types };
    for (ti, t) in p["baseTypes"].as_array().into_iter().flatten().take(2).enumerate() {
        base_types[ti] = parse_type(t)?;
    }
    if p.get("addedType").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
        return Err(unsup("pokemon:addedType"));
    }

    // Status. PS uses "fnt" on fainted mons; the engine derives faintedness from hp.
    let status_id = s(p, "status");
    let (status, status_counter) = match status_id {
        "" | "fnt" => (Status::None, 0u8),
        other => {
            let st = Status::from_id(other).ok_or_else(|| unsup(format!("status:{other}")))?;
            let ss = &p["statusState"];
            let counter = match st {
                Status::Sleep => i(ss, "time") as u8,
                Status::Toxic => i(ss, "stage") as u8,
                _ => 0,
            };
            (st, counter)
        }
    };

    let ability_id = s(p, "ability");
    let ability = Ability::from_id(ability_id).ok_or_else(|| unsup(format!("ability:{ability_id}")))?;
    let base_ability_id = s(p, "baseAbility");
    let base_ability = if base_ability_id.is_empty() {
        ability
    } else {
        Ability::from_id(base_ability_id).ok_or_else(|| unsup(format!("ability:{base_ability_id}")))?
    };
    let item_id = s(p, "item");
    let item = if item_id.is_empty() {
        Item::None
    } else {
        Item::from_id(item_id).ok_or_else(|| unsup(format!("item:{item_id}")))?
    };

    let mut moves = [MoveSlot::EMPTY; 4];
    for (mi, m) in p["moveSlots"].as_array().into_iter().flatten().take(4).enumerate() {
        let mid = s(m, "id");
        let id = MoveId::from_id(mid).ok_or_else(|| unsup(format!("move:{mid}")))?;
        moves[mi] = MoveSlot {
            id,
            pp: i(m, "pp") as u8,
            max_pp: i(m, "maxpp") as u8,
            disabled: m.get("disabled").and_then(Value::as_bool).unwrap_or(false),
        };
    }

    let stats_v = &p["storedStats"];
    let max_hp = i(p, "maxhp") as i16;
    let mut stats = [0i16; 6];
    stats[0] = max_hp;
    for (idx, key) in ["atk", "def", "spa", "spd", "spe"].iter().enumerate() {
        stats[idx + 1] = i(stats_v, key) as i16;
    }

    // Tera: `terastallized` is the tera type name when active, absent otherwise. PS keeps the
    // raw `types` array unchanged and applies tera at lookup time; the engine stores effective
    // types — so a terastallized mon's effective typing is [tera type].
    let terastallized = p.get("terastallized").and_then(Value::as_str).is_some_and(|t| !t.is_empty());
    let tera_id = to_id(s(p, "teraType"));
    let tera_type = if tera_id.is_empty() {
        Type::None
    } else {
        Type::from_id(&tera_id).ok_or_else(|| unsup(format!("type:{tera_id}")))?
    };
    // `types` as parsed above is PS's `pokemon.types` VERBATIM — the live, pre-tera array.
    // Keep it as `live_types` (the STAB / digest / diff field) and derive the engine's
    // EFFECTIVE typing from it. Roost's Flying strip is the other half of the derivation and
    // is applied in `convert_volatiles`, which is the only place the `roost` volatile is known.
    let live_types = types;
    if terastallized && tera_type != Type::Stellar {
        types = [tera_type, Type::None];
    }

    // `illusion` is a `[Pokemon:pNx]` STRING, resolved in `convert_side` where the array order is
    // known. (The old guard here was `b(p, "illusion")` — `as_bool` on a string is always `None`,
    // so the "unsupported" arm never fired and four Zoroark games were silently converted without
    // their disguises.)

    // Transform: the snapshot's moveSlots/storedStats/types/ability are already the copied
    // values; `baseStoredStats` holds the originals. PS doesn't serialize baseMoveSlots, so
    // for a transformed mon `base_moves` is unknown here — converted states are used for
    // comparison (base_* fields aren't diffed) and battle-start states are never transformed.
    let transformed = b(p, "transformed");
    let mut base_stats = stats;
    if let Some(bss) = p.get("baseStoredStats").and_then(Value::as_object) {
        for (idx, key) in ["atk", "def", "spa", "spd", "spe"].iter().enumerate() {
            if let Some(v) = bss.get(*key).and_then(Value::as_i64) {
                base_stats[idx + 1] = v as i16;
            }
        }
    }
    // `baseSpecies` serializes as a "[Species:x]" ref (like `species`), so strip that prefix
    // before resolving — otherwise a transformed mon's base species fails to parse and falls
    // back to its transformed species (breaking the revert-to-base on faint/switch-out).
    let base_species_raw = s(p, "baseSpecies");
    let base_species_id = to_id(base_species_raw.trim_start_matches("[Species:").trim_end_matches(']'));
    let base_species = if base_species_id.is_empty() {
        species
    } else {
        Species::from_id(&base_species_id).unwrap_or(species)
    };
    // Sleep Clause bookkeeping: a sleep whose statusState.source is on the other side.
    let slept_by_foe = status == Status::Sleep && {
        let src = p["statusState"].get("source").and_then(Value::as_str).unwrap_or("");
        let tgt = p["statusState"].get("target").and_then(Value::as_str).unwrap_or("");
        // refs look like "[Pokemon:p1a]"; different player prefix => foe-induced
        let pside = |r: &str| r.split(':').nth(1).map(|x| x.chars().take(2).collect::<String>()).unwrap_or_default();
        !src.is_empty() && !tgt.is_empty() && pside(src) != pside(tgt)
    };
    // Harvest bookkeeping: the eaten berry (PS lastItem + ateBerry).
    let last_berry = if b(p, "ateBerry") {
        Item::from_id(s(p, "lastItem")).unwrap_or(Item::None)
    } else {
        Item::None
    };
    // Cud Chew pending re-eat timer lives in abilityState.counter (absent = 0).
    let cudchew_turns = p["abilityState"].get("counter").and_then(Value::as_i64).unwrap_or(0).max(0) as u8;
    // Gender (Attract / Cute Charm legality): serialized as "M" / "F" / "" (or "N").
    let gender = match s(p, "gender") {
        "M" => 1,
        "F" => 2,
        _ => 0,
    };
    Ok(Pokemon {
        species,
        level,
        types,
        live_types,
        base_types,
        transformed,
        slept_by_foe,
        last_berry,
        cudchew_turns,
        gender,
        base_species,
        base_stats,
        base_moves: if transformed { [MoveSlot::EMPTY; 4] } else { moves },
        hp: i(p, "hp") as i16,
        max_hp,
        stats,
        status,
        status_counter,
        ability,
        base_ability,
        item,
        nature: Nature::Serious, // spreads are baked into storedStats
        evs: [0; 6],
        moves,
        tera_type,
        terastallized,
        ability_used: b(p, "swordBoost") || b(p, "shieldBoost"),
        times_hit: i(p, "timesAttacked") as u8,
        ..Pokemon::EMPTY
    })
}

/// Active-mon volatiles -> engine Side volatiles/counters/pending. Unknown ids are findings.
fn convert_volatiles(p: &Value, side: &mut Side) -> Res<()> {
    let Some(vols) = p["volatiles"].as_object() else { return Ok(()) };
    for (k, vv) in vols {
        let dur = i(vv, "duration") as u8;
        match k.as_str() {
            "confusion" => {
                side.volatiles.insert(VolatileStatus::Confusion);
                side.confusion_turns = i(vv, "time").max(dur as i64) as u8;
            }
            "substitute" => {
                side.volatiles.insert(VolatileStatus::Substitute);
                side.substitute_hp = i(vv, "hp") as i16;
            }
            "leechseed" => { side.volatiles.insert(VolatileStatus::LeechSeed); }
            "taunt" => {
                side.volatiles.insert(VolatileStatus::Taunt);
                side.taunt_turns = dur;
            }
            "encore" => {
                side.volatiles.insert(VolatileStatus::Encore);
                let mv = MoveId::from_id(s(vv, "move")).unwrap_or(MoveId::None);
                side.encore = (mv, dur);
            }
            "disable" => {
                side.volatiles.insert(VolatileStatus::Disable);
                let mv = MoveId::from_id(&to_id(s(vv, "move"))).unwrap_or(MoveId::None);
                side.disable = (mv, dur);
            }
            "yawn" => {
                side.volatiles.insert(VolatileStatus::Yawn);
                side.yawn_turns = dur;
            }
            "throatchop" => {
                side.volatiles.insert(VolatileStatus::ThroatChop);
                side.throat_chop_turns = dur;
            }
            "healblock" => {
                side.volatiles.insert(VolatileStatus::HealBlock);
                side.heal_block_turns = dur;
            }
            "magnetrise" => {
                side.volatiles.insert(VolatileStatus::MagnetRise);
                side.magnet_rise_turns = dur;
            }
            "perishsong" | "perish3" | "perish2" | "perish1" => {
                side.volatiles.insert(VolatileStatus::PerishSong);
                if let Some(n) = k.strip_prefix("perish").and_then(|n| n.parse::<u8>().ok()) {
                    side.perish_turns = n;
                } else {
                    side.perish_turns = dur;
                }
            }
            "stall" => {
                // PS stores the denominator (3^n, capped 729); the engine stores n.
                let c = i(vv, "counter").max(1);
                side.stall_counter = (c as f64).log(3.0).round() as u8;
                // The `stall` volatile's remaining `duration` (2 on the Protect turn, 1 the turn
                // after) drives the Residual handler-list length, INDEPENDENT of the counter (which
                // can be reset to 0 by a non-Protect move or rounded to 0 by log3 on the turn-after).
                side.stall_turns = (i(vv, "duration").max(1)) as u8;
            }
            "choicelock" => { side.volatiles.insert(VolatileStatus::ChoiceLock); }
            "saltcure" => { side.volatiles.insert(VolatileStatus::SaltCure); }
            "curse" => { side.volatiles.insert(VolatileStatus::Curse); }
            "nightmare" => { side.volatiles.insert(VolatileStatus::Nightmare); }
            "attract" => { side.volatiles.insert(VolatileStatus::Attract); }
            "torment" => { side.volatiles.insert(VolatileStatus::Torment); }
            "destinybond" => { side.volatiles.insert(VolatileStatus::DestinyBond); }
            "glaiverush" => { side.volatiles.insert(VolatileStatus::GlaiveRush); }
            "partiallytrapped" => {
                side.volatiles.insert(VolatileStatus::PartiallyTrapped);
                // `duration` = remaining turns (ticked each end of turn); `boundDivisor` = 6 with
                // Binding Band else 8 (snapshotted at application).
                side.partial_trap_turns = dur;
                side.partial_trap_div = i(vv, "boundDivisor").max(1) as u8;
            }
            "trapped" => { side.volatiles.insert(VolatileStatus::Trapped); }
            // `trapper` is PS's linkage marker on the mon DOING the trapping (Mean Look family); it
            // carries no engine-modeled state (the trap lives as `trapped` on the victim).
            "trapper" => {}
            "ingrain" => { side.volatiles.insert(VolatileStatus::Ingrain); }
            "noretreat" => { side.volatiles.insert(VolatileStatus::NoRetreat); }
            "octolock" => { side.volatiles.insert(VolatileStatus::Octolock); }
            "flashfire" => { side.volatiles.insert(VolatileStatus::FlashFire); }
            "truant" => { side.volatiles.insert(VolatileStatus::Truant); }
            // `fromBooster` marks a Booster-Energy-sourced boost: PS's `onWeatherChange` /
            // `onTerrainChange` removes a FIELD-sourced boost the moment the sun / Electric Terrain
            // lapses but keeps a Booster one, so the flag is load-bearing state, not cosmetics.
            "protosynthesis" => {
                side.volatiles.insert(VolatileStatus::Protosynthesis);
                if vv.get("fromBooster").and_then(|x| x.as_bool()).unwrap_or(false) {
                    side.volatiles.insert(VolatileStatus::ProtoBooster);
                }
            }
            "quarkdrive" => {
                side.volatiles.insert(VolatileStatus::QuarkDrive);
                if vv.get("fromBooster").and_then(|x| x.as_bool()).unwrap_or(false) {
                    side.volatiles.insert(VolatileStatus::ProtoBooster);
                }
            }
            "focusenergy" | "dragoncheer" => { side.volatiles.insert(VolatileStatus::FocusEnergy); }
            "unburden" => { side.volatiles.insert(VolatileStatus::Unburden); }
            "mustrecharge" => {
                side.volatiles.insert(VolatileStatus::MustRecharge);
                side.pending_move = PendingMove::Recharging;
            }
            "twoturnmove" => {
                let mv = MoveId::from_id(s(vv, "move")).unwrap_or(MoveId::None);
                // `twoturnmove` OUTLIVES the strike. PS's charge `onTryMove` is
                // `if (attacker.removeVolatile(move.id)) return;` (`data/moves.ts:1716`): it drops
                // the MOVE-SPECIFIC marker volatile and lets the strike through, leaving
                // `twoturnmove` (duration 2) standing until the end-of-turn duration tick. So
                // `twoturnmove` WITHOUT its marker means "already struck this turn", which is the
                // engine's `PendingMove::None` — the field means "committed to strike NEXT turn".
                // Only the pair means charging. Visible at rb1345 d42, a mid-turn faint request
                // right after Eternatus' charged Meteor Beam connects.
                let charging = mv != MoveId::None
                    && vols.contains_key(mv.to_id().as_ref() as &str);
                if charging {
                    side.pending_move = PendingMove::Charging(mv);
                }
            }
            "lockedmove" => {
                side.volatiles.insert(VolatileStatus::LockedMove);
                let mv = MoveId::from_id(s(vv, "move")).unwrap_or(MoveId::None);
                // `duration` resets to 2 on every use; `trueDuration` is the real number of
                // rampage turns remaining (1 = next use ends with confusion).
                let remaining = i(vv, "trueDuration").max(1) as u8;
                side.pending_move = PendingMove::Rampaging(mv, remaining);
            }
            // Single-turn flags that exist only inside a turn; at decision boundaries they may
            // linger in PS for the active mid-turn snapshot but carry no cross-turn state.
            "roost" => { side.volatiles.insert(VolatileStatus::Roosted); }
            "protect" => { side.volatiles.insert(VolatileStatus::Protect); }
            "endure" => { side.volatiles.insert(VolatileStatus::Endure); }
            "flinch" => { side.volatiles.insert(VolatileStatus::Flinch); }
            "charge" => { side.volatiles.insert(VolatileStatus::Charge); }
            // Chilly Reception's priority-charge marker: a duration-1 volatile that only drives
            // the "[premajor]" -prepare message, carrying no cross-turn state (the user switches
            // out the same turn, clearing it). No engine field to set.
            "chillyreception" => {}
            // Two-turn charge / semi-invulnerable moves add BOTH a `twoturnmove` volatile (which
            // sets PendingMove::Charging above) AND a move-specific marker volatile (`meteorbeam`,
            // `fly`, ...). The latter carries no extra cross-turn state — skip it.
            other if MoveId::from_id(other).is_some_and(engine::generate::is_two_turn_move) => {}
            other => return Err(unsup(format!("volatile:{other}"))),
        }
    }
    Ok(())
}

fn convert_side_conditions(v: &Value, side: &mut Side) -> Res<()> {
    let Some(conds) = v.as_object() else { return Ok(()) };
    let sc = &mut side.side_conditions;
    for (k, cv) in conds {
        let layers = i(cv, "layers").max(1) as u8;
        let dur = i(cv, "duration") as u8;
        match k.as_str() {
            "stealthrock" => sc.stealth_rock = true,
            "spikes" => sc.spikes = layers,
            "toxicspikes" => sc.toxic_spikes = layers,
            "stickyweb" => sc.sticky_web = true,
            "reflect" => sc.reflect = dur,
            "lightscreen" => sc.light_screen = dur,
            "auroraveil" => sc.aurora_veil = dur,
            "tailwind" => sc.tailwind = dur,
            other => return Err(unsup(format!("sidecondition:{other}"))),
        }
    }
    Ok(())
}

fn convert_slot_conditions(v: Option<&Value>, side: &mut Side, si: usize, canon: &Canonical, turn: u32) -> Res<()> {
    // Serialized as an array (one object per slot); accept the object form too.
    let entries: Vec<&Value> = match v {
        Some(Value::Array(a)) => a.iter().collect(),
        Some(Value::Object(o)) => o.values().collect(),
        _ => return Ok(()),
    };
    for conds in entries {
        let Some(conds) = conds.as_object() else { continue };
        for (k, cv) in conds {
            match k.as_str() {
                "wish" => {
                    // PS stores startingTurn rather than duration. Immediately after the Wish
                    // action `turn == startingTurn + 1` and two residual phases remain; at the
                    // next decision boundary turn has advanced again and one remains. Matured
                    // wishes that linger over an empty slot also map to one.
                    let starting = cv.get("startingTurn").and_then(Value::as_u64).unwrap_or(0) as u32;
                    // hp can be fractional (maxhp/2 of an odd max); PS truncates on heal.
                    let hp = cv.get("hp").and_then(Value::as_f64).unwrap_or(0.0) as i16;
                    side.wish = (if turn <= starting + 1 { 2 } else { 1 }, hp);
                }
                "futuremove" => {
                    // PS stores `endingTurn` (not a live duration): the strike lands 2 turns after
                    // `endingTurn`, so the engine's remaining-tick count is `endingTurn + 2 - turn`
                    // (matching its own end-of-turn countdown, which fires when it reaches 1). The
                    // caster slot (`future_sight.1`) is re-resolved from the source ref in
                    // `convert_state`; a placeholder 0 here is overwritten there.
                    let ending = i(cv, "endingTurn");
                    let remaining = (ending + 2 - turn as i64).max(1) as u8;
                    side.future_sight = (remaining, 0);
                }
                "healingwish" | "lunardance" => {
                    side.healing_wish = true;
                }
                other => return Err(unsup(format!("slotcondition:{other}"))),
            }
        }
    }
    Ok(())
}

/// Side index helper.
pub fn side_id(idx: usize) -> SideId {
    if idx == 0 { SideId::One } else { SideId::Two }
}

// ---- frontier scanning -----------------------------------------------------------------

/// Walk a full state JSON and report *every* id outside the engine's modeled set — not just
/// the first conversion failure. This is the work-queue generator for "implement the full
/// game": the aggregate report ranks these by how many traces they block.
pub fn scan_frontier(v: &Value, out: &mut std::collections::BTreeSet<String>) {
    let Some(sides) = v["sides"].as_array() else { return };
    for side in sides {
        for p in side["pokemon"].as_array().into_iter().flatten() {
            let species = species_id_of_details(s(p, "details"));
            if Species::from_id(&species).is_none() {
                out.insert(format!("species:{species}"));
            }
            let ability = s(p, "ability");
            if !ability.is_empty() && Ability::from_id(ability).is_none() {
                out.insert(format!("ability:{ability}"));
            }
            let item = s(p, "item");
            if !item.is_empty() && Item::from_id(item).is_none() {
                out.insert(format!("item:{item}"));
            }
            for m in p["moveSlots"].as_array().into_iter().flatten() {
                let mid = s(m, "id");
                if MoveId::from_id(mid).is_none() {
                    out.insert(format!("move:{mid}"));
                }
            }
            for (k, _) in p["volatiles"].as_object().into_iter().flatten() {
                // Two-turn moves add a per-move marker volatile next to `twoturnmove`
                // (meteorbeam, fly, ...) that `convert_volatiles` skips — keep in sync here.
                if MoveId::from_id(k).is_some_and(engine::generate::is_two_turn_move) {
                    continue;
                }
                if !KNOWN_VOLATILES.contains(&k.as_str()) {
                    out.insert(format!("volatile:{k}"));
                }
            }
            if false {
                out.insert("pokemon:transformed".into());
            }
            if p.get("addedType").and_then(Value::as_str).is_some_and(|t| !t.is_empty()) {
                out.insert("pokemon:addedType".into());
            }
        }
        for (k, _) in side["sideConditions"].as_object().into_iter().flatten() {
            if !KNOWN_SIDE_CONDITIONS.contains(&k.as_str()) {
                out.insert(format!("sidecondition:{k}"));
            }
        }
        if let Some(slots) = side.get("slotConditions").and_then(Value::as_object) {
            for conds in slots.values() {
                for (k, _) in conds.as_object().into_iter().flatten() {
                    if !matches!(k.as_str(), "wish" | "futuremove") {
                        out.insert(format!("slotcondition:{k}"));
                    }
                }
            }
        }
    }
    for (k, _) in v["field"]["pseudoWeather"].as_object().into_iter().flatten() {
        if k != "trickroom" {
            out.insert(format!("pseudoweather:{k}"));
        }
    }
    let weather = s(&v["field"], "weather");
    if !weather.is_empty() && Weather::from_id(weather).is_none() {
        out.insert(format!("weather:{weather}"));
    }
}

/// Volatile ids `convert_volatiles` knows how to map. Keep in sync with that match.
const KNOWN_VOLATILES: &[&str] = &[
    "confusion", "substitute", "leechseed", "taunt", "encore", "disable", "yawn", "perishsong",
    "perish3", "perish2", "perish1", "stall", "choicelock", "saltcure", "curse", "nightmare",
    "attract", "torment", "destinybond", "glaiverush", "partiallytrapped", "protosynthesis",
    "quarkdrive", "mustrecharge", "twoturnmove", "lockedmove", "roost", "protect", "endure",
    "flinch", "charge", "focusenergy", "dragoncheer", "unburden", "throatchop", "healblock",
    "magnetrise",
    "trapped", "trapper", "ingrain", "noretreat", "octolock", "chillyreception",
];

const KNOWN_SIDE_CONDITIONS: &[&str] = &[
    "stealthrock", "spikes", "toxicspikes", "stickyweb", "reflect", "lightscreen", "auroraveil",
    "tailwind",
];
