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
    for (si, side_v) in sides.iter().enumerate() {
        state.sides[si] = convert_side(side_v, si, canon, ended)?;
    }
    convert_field(&v["field"], &mut state)?;
    state.turn = i(v, "turn") as u32;
    Ok(state)
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
                other => return Err(unsup(format!("pseudoweather:{other}"))),
            }
        }
    }
    Ok(())
}

fn convert_side(v: &Value, si: usize, canon: &Canonical, ended: bool) -> Res<Side> {
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

    // Side-level tera-used: synthesized by the recorder from live objects (the serializer
    // drops `terastallized` from fainted mons, so the snapshot alone can't recover it).
    side.tera_used = b(v, "teraUsed") || side.pokemon.iter().any(|p| p.terastallized);

    // A finished battle has no active on the losing side (PS drops the fainted active);
    // mark with a sentinel and let the differ skip active-only comparisons. Side-level state
    // (hazards/screens/slot conditions) still exists and must still be converted.
    let Some((active_slot, active_v)) = active else {
        if ended {
            side.active_index = u8::MAX;
            convert_side_conditions(&v["sideConditions"], &mut side)?;
            convert_slot_conditions(v.get("slotConditions"), &mut side, si, canon)?;
            return Ok(side);
        }
        return Err(unsup("side:no-active"));
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
        if let Some(id) = lm.get("id").and_then(Value::as_str) {
            side.last_used_move = MoveId::from_id(id).unwrap_or(MoveId::None);
        }
    }
    convert_volatiles(active_v, &mut side)?;

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
    convert_slot_conditions(v.get("slotConditions"), &mut side, si, canon)?;
    Ok(side)
}

fn convert_pokemon(p: &Value, species_id: &str) -> Res<Pokemon> {
    let species = Species::from_id(species_id).ok_or_else(|| unsup(format!("species:{species_id}")))?;

    let details = s(p, "details");
    let level = details
        .split(", ")
        .find_map(|part| part.strip_prefix('L').and_then(|l| l.parse::<u8>().ok()))
        .unwrap_or(100);

    let mut types = [Type::None; 2];
    for (ti, t) in p["types"].as_array().into_iter().flatten().take(2).enumerate() {
        let tid = to_id(t.as_str().unwrap_or(""));
        types[ti] = Type::from_id(&tid).ok_or_else(|| unsup(format!("type:{tid}")))?;
    }
    let mut base_types = types;
    for (ti, t) in p["baseTypes"].as_array().into_iter().flatten().take(2).enumerate() {
        let tid = to_id(t.as_str().unwrap_or(""));
        base_types[ti] = Type::from_id(&tid).ok_or_else(|| unsup(format!("type:{tid}")))?;
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
    if terastallized && tera_type != Type::Stellar {
        types = [tera_type, Type::None];
    }

    if b(p, "illusion") {
        return Err(unsup("pokemon:illusion"));
    }

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
    let base_species_id = to_id(s(p, "baseSpecies"));
    let base_species = if base_species_id.is_empty() {
        species
    } else {
        Species::from_id(&base_species_id).unwrap_or(species)
    };
    Ok(Pokemon {
        species,
        level,
        types,
        base_types,
        transformed,
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
            "perishsong" | "perish3" | "perish2" | "perish1" => {
                side.volatiles.insert(VolatileStatus::PerishSong);
                if let Some(n) = k.strip_prefix("perish").and_then(|n| n.parse::<u8>().ok()) {
                    side.perish_turns = n;
                } else {
                    side.perish_turns = dur;
                }
            }
            "stall" => {
                side.stall_counter = i(vv, "counter") as u8;
            }
            "choicelock" => { side.volatiles.insert(VolatileStatus::ChoiceLock); }
            "saltcure" => { side.volatiles.insert(VolatileStatus::SaltCure); }
            "curse" => { side.volatiles.insert(VolatileStatus::Curse); }
            "nightmare" => { side.volatiles.insert(VolatileStatus::Nightmare); }
            "attract" => { side.volatiles.insert(VolatileStatus::Attract); }
            "torment" => { side.volatiles.insert(VolatileStatus::Torment); }
            "destinybond" => { side.volatiles.insert(VolatileStatus::DestinyBond); }
            "glaiverush" => { side.volatiles.insert(VolatileStatus::GlaiveRush); }
            "partiallytrapped" => { side.volatiles.insert(VolatileStatus::PartiallyTrapped); }
            "protosynthesis" => { side.volatiles.insert(VolatileStatus::Protosynthesis); }
            "quarkdrive" => { side.volatiles.insert(VolatileStatus::QuarkDrive); }
            "focusenergy" | "dragoncheer" => { side.volatiles.insert(VolatileStatus::FocusEnergy); }
            "unburden" => { side.volatiles.insert(VolatileStatus::Unburden); }
            "mustrecharge" => {
                side.volatiles.insert(VolatileStatus::MustRecharge);
                side.pending_move = PendingMove::Recharging;
            }
            "twoturnmove" => {
                let mv = MoveId::from_id(s(vv, "move")).unwrap_or(MoveId::None);
                side.pending_move = PendingMove::Charging(mv);
            }
            "lockedmove" => {
                side.volatiles.insert(VolatileStatus::LockedMove);
                let mv = MoveId::from_id(s(vv, "move")).unwrap_or(MoveId::None);
                side.pending_move = PendingMove::Rampaging(mv, dur);
            }
            // Single-turn flags that exist only inside a turn; at decision boundaries they may
            // linger in PS for the active mid-turn snapshot but carry no cross-turn state.
            "roost" => { side.volatiles.insert(VolatileStatus::Roosted); }
            "protect" => { side.volatiles.insert(VolatileStatus::Protect); }
            "endure" => { side.volatiles.insert(VolatileStatus::Endure); }
            "flinch" => { side.volatiles.insert(VolatileStatus::Flinch); }
            "charge" => { side.volatiles.insert(VolatileStatus::Charge); }
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

fn convert_slot_conditions(v: Option<&Value>, side: &mut Side, si: usize, canon: &Canonical) -> Res<()> {
    let Some(slots) = v.and_then(Value::as_object) else { return Ok(()) };
    for (_slot, conds) in slots {
        let Some(conds) = conds.as_object() else { continue };
        for (k, cv) in conds {
            match k.as_str() {
                "wish" => {
                    side.wish = (i(cv, "duration") as u8, i(cv, "hp") as i16);
                }
                "futuremove" => {
                    // source pokemon decides the attack's stats; map to a canonical slot
                    let src = cv
                        .get("source")
                        .and_then(Value::as_str)
                        .map(|r| to_id(r.split(": ").last().unwrap_or("")))
                        .unwrap_or_default();
                    let slot = canon.slot(si, &src).unwrap_or(0);
                    side.future_sight = (i(cv, "duration") as u8, slot);
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
];

const KNOWN_SIDE_CONDITIONS: &[&str] = &[
    "stealthrock", "spikes", "toxicspikes", "stickyweb", "reflect", "lightscreen", "auroraveil",
    "tailwind",
];
