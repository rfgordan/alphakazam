/**
 * Code generator: dump PS species + move data into a Rust source file of static tables
 * (engine/src/gen.rs). Species and moves are addressed by a dense u16 id; index 0 is the
 * "none" sentinel. Effects that the engine models (boosts, secondary, status, hazard,
 * heal, self-switch, target volatile) are translated to the engine's enums; unmodeled
 * effects are dropped (the differential harness surfaces those gaps).
 *
 * Usage: node gen-data.mjs   (writes ../crates/engine/src/gen.rs)
 */
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';
import { createRequire } from 'node:module';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const PS_DIR = path.resolve(__dirname, '../../engines/pokemon-showdown');
const { Dex } = require(path.join(PS_DIR, 'dist/sim'));
const dex = Dex.forFormat('gen9customgame');
const OUT = path.resolve(__dirname, '../crates/engine/src/gen.rs');

const TYPES = new Set(['Normal','Fire','Water','Electric','Grass','Ice','Fighting','Poison','Ground','Flying','Psychic','Bug','Rock','Ghost','Dragon','Dark','Steel','Fairy','Stellar']);
const typeRs = (t) => TYPES.has(t) ? `Type::${t}` : 'Type::None';

const STATUS = { brn:'Burn', par:'Paralysis', slp:'Sleep', frz:'Freeze', psn:'Poison', tox:'Toxic' };
const statusRs = (s) => s && STATUS[s] ? `Status::${STATUS[s]}` : 'Status::None';

const SIDECOND = { stealthrock:'StealthRock', spikes:'Spikes', toxicspikes:'ToxicSpikes', stickyweb:'StickyWeb', reflect:'Reflect', lightscreen:'LightScreen', auroraveil:'AuroraVeil', tailwind:'Tailwind' };
const sideCondRs = (s) => s && SIDECOND[s] ? `Some(SideConditionId::${SIDECOND[s]})` : 'None';

const WEATHER = { sunnyday:'Sun', raindance:'Rain', sandstorm:'Sand', snow:'Snow', snowscape:'Snow', hail:'Snow' };
const weatherRs = (s) => s && WEATHER[s] ? `Weather::${WEATHER[s]}` : 'Weather::None';

// Boost effects PS implements via onHit callbacks (invisible to field extraction).
const MANUAL_TARGET_BOOSTS = { partingshot: { atk: -1, spa: -1 } };
const VOLATILE = { confusion:'Confusion', substitute:'Substitute', leechseed:'LeechSeed', taunt:'Taunt', encore:'Encore', disable:'Disable', protect:'Protect', endure:'Endure', flinch:'Flinch', roost:'Roost', charge:'Charge', yawn:'Yawn', perishsong:'PerishSong', destinybond:'DestinyBond', curse:'Curse', nightmare:'Nightmare', attract:'Attract', torment:'Torment', saltcure:'SaltCure', glaiverush:'GlaiveRush', partiallytrapped:'PartiallyTrapped', focusenergy:'FocusEnergy', dragoncheer:'FocusEnergy', throatchop:'ThroatChop', healblock:'HealBlock' };
const volatileRs = (s) => s && VOLATILE[s] ? `Some(VolatileStatus::${VOLATILE[s]})` : 'None';

// boosts object -> [atk,def,spa,spd,spe,accuracy,evasion]
const boostsRs = (b) => {
	b = b || {};
	const a = [b.atk||0, b.def||0, b.spa||0, b.spd||0, b.spe||0, b.accuracy||0, b.evasion||0];
	return `[${a.join(',')}]`;
};

const TARGETS = { normal:'Normal', self:'User', adjacentAlly:'AdjacentAlly', adjacentAllyOrSelf:'AdjacentAllyOrSelf', adjacentFoe:'AdjacentFoe', allAdjacent:'AllAdjacent', allAdjacentFoes:'AllAdjacentFoes', all:'All', allies:'Allies', allySide:'AllySide', allyTeam:'AllyTeam', any:'Any', foeSide:'FoeSide', randomNormal:'RandomNormal', scripted:'Scripted' };
const targetRs = (t) => `MoveTarget::${TARGETS[t] || 'Normal'}`;

const esc = (s) => s.replace(/\\/g, '\\\\').replace(/"/g, '\\"');

// ---- moves ----
const moves = dex.moves.all().filter(m => m.exists && m.id && m.realMove !== false);
// Stable order by id; dense index starts at 1.
moves.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
const moveRows = ['    MoveData::none(),'];
const moveNames = ['""'];
const moveByName = [];
moves.forEach((m, i) => {
	const idx = i + 1;
	moveNames.push(`"${esc(m.id)}"`);
	moveByName.push([m.id, idx]);
	const cat = m.category === 'Physical' ? 'Physical' : m.category === 'Special' ? 'Special' : 'Status';
	const acc = m.accuracy === true ? 0 : (m.accuracy || 0);
	// Fixed multihit is a number (Dragon Darts = 2, Population Bomb = 10); variable is a
		// [min, max] pair (Bullet Seed / Rock Blast / Icicle Spear = [2, 5]). `hits` is the
		// guaranteed minimum; `hits_max` the maximum (== hits when fixed).
		const hits = typeof m.multihit === 'number' ? m.multihit : (Array.isArray(m.multihit) ? m.multihit[0] : 1);
		const hitsMax = typeof m.multihit === 'number' ? m.multihit : (Array.isArray(m.multihit) ? m.multihit[1] : 1);
	const sec = m.secondary || (m.secondaries && m.secondaries[0]) || null;
	const heal = Array.isArray(m.heal) ? `(${m.heal[0]},${m.heal[1]})` : '(0,1)';
	const recoil = Array.isArray(m.recoil) ? `(${m.recoil[0]},${m.recoil[1]})` : '(0,1)';
	const drain = Array.isArray(m.drain) ? `(${m.drain[0]},${m.drain[1]})` : '(0,1)';
	// A boost/status secondary is probabilistic; a pure-volatile secondary (e.g. Salt
	// Cure: 100% volatileStatus) is folded into `target_volatile` instead.
	const secHasBoostOrStatus = !!(sec && (sec.boosts || sec.status));
	// A chance-based volatile secondary (Hurricane 30% confusion, Dire Claw, ...) — flinch is
	// handled separately; 100%-chance volatiles fold into target_volatile below.
	const secVol = sec && sec.volatileStatus && sec.volatileStatus !== 'flinch'
		&& sec.chance && sec.chance < 100 ? sec.volatileStatus : null;
	const secChance = (secHasBoostOrStatus || secVol) ? (sec.chance || 0) : 0;
	// Flinch is its own chance-based secondary (Iron Head 30%, Fake Out 100%), handled
	// separately so it can interrupt a not-yet-moved target rather than being a plain volatile.
	const flinchChance = (sec && sec.volatileStatus === 'flinch') ? (sec.chance || 100)
		: (m.volatileStatus === 'flinch' ? 100 : 0);
	let targetVol = m.volatileStatus === 'flinch' ? undefined : m.volatileStatus;
	if (!targetVol && sec && sec.volatileStatus && sec.volatileStatus !== 'flinch' && (sec.chance === 100 || sec.chance === undefined)) {
		targetVol = sec.volatileStatus;
	}
	// Top-level `boosts` apply to the user when target is 'self' (Swords Dance, Dragon
	// Dance, ...) or to the foe otherwise (Growl, Charm, ...). Also fold in `self.boosts`.
	const selfTarget = m.target === 'self';
	// Some moves boost the user via a 100%-chance self-secondary (Rapid Spin, Trailblaze,
	// Meteor Mash, ...) — fold those into the deterministic self-boosts too.
	const secSelfBoosts = sec && sec.self && (sec.chance === 100 || sec.chance === undefined) ? sec.self.boosts : null;
	// `self.boosts` (most), `selfBoost.boosts` (Scale Shot's post-hit +Spe/−Def), top-level
	// boosts when target is self, and a 100%-chance self-secondary all feed the user's boosts.
	const selfBoostsObj = Object.assign({}, m.self && m.self.boosts, m.selfBoost && m.selfBoost.boosts, selfTarget ? m.boosts : null, secSelfBoosts);
	const targetBoostsObj = !selfTarget ? m.boosts : null;
	const fields = [
		`id: MoveId(${idx})`,
		`typ: ${typeRs(m.type)}`,
		`category: MoveCategory::${cat}`,
		`base_power: ${m.basePower || 0}`,
		`accuracy: ${acc}`,
		`priority: ${m.priority || 0}`,
		`hits: ${hits}`,
			`hits_max: ${hitsMax}`,
			`flinch_chance: ${flinchChance}`,
		`uses_defense_as_attack: ${m.overrideOffensiveStat === 'def'}`,
		`self_switch: ${!!m.selfSwitch}`,
		`force_switch: ${!!m.forceSwitch}`,
		`self_boosts: ${boostsRs(selfBoostsObj)}`,
		`target_boosts: ${boostsRs(MANUAL_TARGET_BOOSTS[m.id] || targetBoostsObj)}`,
		`secondary_chance: ${secChance}`,
		`secondary_boosts: ${boostsRs(sec && sec.boosts)}`,
		`secondary_status: ${statusRs(sec && sec.status)}`,
		`secondary_volatile: ${volatileRs(secVol)}`,
		`status: ${statusRs(m.status)}`,
		`side_condition: ${sideCondRs(m.sideCondition)}`,
		`heal: ${heal}`,
		`recoil: ${recoil}`,
		`drain: ${drain}`,
		`target_volatile: ${volatileRs(targetVol)}`,
		`weather: ${weatherRs(m.weather)}`,
		`flag_contact: ${!!(m.flags && m.flags.contact)}`,
		`flag_sound: ${!!(m.flags && m.flags.sound)}`,
		`flag_punch: ${!!(m.flags && m.flags.punch)}`,
		`flag_bite: ${!!(m.flags && m.flags.bite)}`,
		`flag_slicing: ${!!(m.flags && m.flags.slicing)}`,
		`flag_bullet: ${!!(m.flags && m.flags.bullet)}`,
		`flag_pulse: ${!!(m.flags && m.flags.pulse)}`,
		`flag_heal: ${!!(m.flags && m.flags.heal)}`,
		`flag_powder: ${!!(m.flags && m.flags.powder)}`,
		`pp: ${m.pp || 0}`,
		`target: ${targetRs(m.target)}`,
		`crit_ratio: ${m.critRatio || 1}`,
		`always_crit: ${m.willCrit === true}`,
		`self_destruct: ${!!m.selfdestruct}`,
	];
	moveRows.push(`    MoveData { ${fields.join(', ')} },`);
});

// ---- species ----
const species = dex.species.all().filter(s => s.exists && s.num > 0 && s.baseStats && !s.isNonstandard);
species.sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
const specTypes = ['    [Type::None, Type::None],'];
const specStats = ['    [0, 0, 0, 0, 0, 0],'];
const specWeight = ['    0,'];
const specNfe = ['    false,']; // not-fully-evolved (can still evolve) — Eviolite applies
const specNames = ['""'];
const specByName = [];
species.forEach((s, i) => {
	const idx = i + 1;
	specNames.push(`"${esc(s.id)}"`);
	specByName.push([s.id, idx]);
	const t = s.types || [];
	const t0 = typeRs(t[0]);
	const t1 = t[1] ? typeRs(t[1]) : 'Type::None';
	specTypes.push(`    [${t0}, ${t1}],`);
	const bs = s.baseStats;
	specStats.push(`    [${bs.hp}, ${bs.atk}, ${bs.def}, ${bs.spa}, ${bs.spd}, ${bs.spe}],`);
	specWeight.push(`    ${Math.round((s.weightkg || 0) * 10)},`); // hectograms (kg*10)
	specNfe.push(`    ${!!(s.evos && s.evos.length > 0)},`); // has further evolutions
	// Cosmetic formes (Florges-Yellow, Vivillon patterns we filtered, ...) are mechanically
	// identical: alias their ids to the base species' dense id.
	for (const cf of s.cosmeticFormes || []) {
		const cfid = String(cf).toLowerCase().replace(/[^a-z0-9]/g, '');
		if (!species.some(sp => sp.id === cfid)) specByName.push([cfid, idx]);
	}
});

const byNameRs = (pairs) => pairs.slice().sort((a, b) => (a[0] < b[0] ? -1 : 1)).map(([n, i]) => `    ("${esc(n)}", ${i}),`).join('\n');

// ---- abilities & items: full id coverage as generated enums --------------------------------
// Variant names are PascalCase of the display name (non-alphanumerics stripped). The engine's
// effect code matches on specific variants; unimplemented ones simply fall through `_ => {}`.
const pascal = (name) => {
	let v = name.replace(/['.\u2019]/g, '').split(/[^A-Za-z0-9]+/).filter(Boolean)
		.map(w => w[0].toUpperCase() + w.slice(1)).join('');
	if (/^[0-9]/.test(v)) v = 'X' + v;
	return v;
};

function genIdEnum(kind, entries) {
	const seen = new Map();
	for (const e of entries) {
		const v = pascal(e.name);
		if (seen.has(v)) throw new Error(`${kind} variant collision: ${v} (${e.id} vs ${seen.get(v)})`);
		seen.set(v, e.id);
	}
	const variants = entries.map(e => `    ${pascal(e.name)},`).join('\n');
	const fromArms = entries.map(e => `            "${e.id}" => Some(${kind}::${pascal(e.name)}),`).join('\n');
	const toArms = entries.map(e => `            ${kind}::${pascal(e.name)} => "${e.id}",`).join('\n');
	return `#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum ${kind} {
    None = 0,
${variants}
    /// Fog-of-war sentinel (see State::observe); never a real PS id.
    Unknown,
}

impl ${kind} {
    pub const COUNT: usize = ${entries.length + 2};
    /// Map a PS toID string to this enum, or None if unknown.
    pub fn from_id(s: &str) -> Option<${kind}> {
        match s {
            "none" => Some(${kind}::None),
            "unknown" => Some(${kind}::Unknown),
${fromArms}
            _ => None,
        }
    }
    /// The PS toID string for this value.
    pub fn to_id(self) -> &'static str {
        match self {
            ${kind}::None => "none",
            ${kind}::Unknown => "unknown",
${toArms}
        }
    }
}
`;
}

const NONSTD_EXCLUDE = new Set(['CAP', 'Custom', 'Future']);
const abilities = dex.abilities.all()
	.filter(a => a.exists && a.id && !NONSTD_EXCLUDE.has(a.isNonstandard || ''))
	.sort((a, b) => (a.id < b.id ? -1 : 1));
const items = dex.items.all()
	.filter(i => i.exists && i.id && !NONSTD_EXCLUDE.has(i.isNonstandard || ''))
	.sort((a, b) => (a.id < b.id ? -1 : 1));
const abilityEnum = genIdEnum('Ability', abilities);
const itemEnum = genIdEnum('Item', items);

const out = `//! AUTO-GENERATED by harness/gen-data.mjs — do not edit by hand.
//!
//! Static species and move tables extracted from Pokémon Showdown. Species and moves are
//! addressed by a dense u16 id (index 0 = none). \`*_BY_NAME\` are sorted for binary search.
#![allow(clippy::all)]

use crate::data::{MoveData, MoveTarget};
use crate::ids::{MoveCategory, MoveId, Status, Type, Weather};
use crate::instruction::SideConditionId;
use crate::volatile::VolatileStatus;

${abilityEnum}
${itemEnum}

pub static SPECIES_COUNT: usize = ${species.length + 1};
pub static SPECIES_NAMES: &[&str] = &[${specNames.join(', ')}];
pub static SPECIES_TYPES: &[[Type; 2]] = &[
${specTypes.join('\n')}
];
pub static SPECIES_BASE_STATS: &[[u16; 6]] = &[
${specStats.join('\n')}
];
/// Weight in hectograms (kg × 10).
pub static SPECIES_WEIGHT_HG: &[u32] = &[
${specWeight.join('\n')}
];
/// Not-fully-evolved: the species can still evolve (Eviolite boosts its defenses).
pub static SPECIES_NFE: &[bool] = &[
${specNfe.join('\n')}
];
pub static SPECIES_BY_NAME: &[(&str, u16)] = &[
${byNameRs(specByName)}
];

pub static MOVE_COUNT: usize = ${moves.length + 1};
pub static MOVE_NAMES: &[&str] = &[${moveNames.join(', ')}];
pub static MOVES: &[MoveData] = &[
${moveRows.join('\n')}
];
pub static MOVE_BY_NAME: &[(&str, u16)] = &[
${byNameRs(moveByName)}
];
`;

fs.writeFileSync(OUT, out);
console.log(`wrote ${OUT}: ${species.length} species, ${moves.length} moves, ${abilities.length} abilities, ${items.length} items`);
