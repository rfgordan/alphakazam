// On-policy cosim gate: replay engine-sampled decisions inside pinned Pokémon Showdown.
//
// `agents/scripts/onpolicy_sample.py` plays self-play battles on the Rust engine with a training
// checkpoint and dumps, per decision, the engine's true state as a PS-loadable snapshot plus the
// choices it made and the state it landed in. This script does the Showdown half:
//
//   1. LOADS      — `State.deserializeBattle(pre)` must succeed. The exporter is certified
//                   against corpus states; on-policy states are a different distribution.
//   2. LEGALITY   — PS's own request for that position must offer exactly the actions the engine's
//                   mask marks legal. This is the sharpest cheap check there is: an action the
//                   engine allows and PS forbids is a free exploit for the policy to farm.
//   3. MEMBERSHIP — drive PS from `pre` with the same choices and enumerate the PRNG tree; the
//                   engine's realized `post` must be one of the outcomes PS can produce. Bounded
//                   by --max-paths; hitting the bound without a match reports INCONCLUSIVE, never
//                   a divergence (the point is zero false alarms in CI).
//
// Usage:
//   node harness/onpolicy-gate.mjs samples.jsonl [--max-paths 20000] [--json report.json]
//
// Exit code 0 when nothing diverged (inconclusive is not a failure), 1 otherwise.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const PS_COMMIT = assertPsPinned();
const { State } = require(path.join(PS_DIR, 'dist/sim/state'));

const argv = process.argv.slice(2);
const arg = (name, def) => {
	const i = argv.indexOf(`--${name}`);
	return i >= 0 ? argv[i + 1] : def;
};
const SAMPLES = argv.find(a => !a.startsWith('--') && (argv.indexOf(a) === 0 || !argv[argv.indexOf(a) - 1]?.startsWith('--')));
const MAX_PATHS = Number(arg('max-paths', '20000'));
const JSON_OUT = arg('json', null);
const VERBOSE = argv.includes('--verbose');

if (!SAMPLES) {
	console.error('usage: node harness/onpolicy-gate.mjs <samples.jsonl> [--max-paths N] [--json out.json]');
	process.exit(2);
}

// ---- engine-modeled projection --------------------------------------------------------
//
// COPIED VERBATIM from harness/transplant-gate.mjs (its "engine-modeled projection" block, which
// mirrors convert.rs's reads). Duplicated rather than shared because transplant-gate.mjs is a
// certified gate whose exports this box cannot re-verify; if you change convert.rs's modeled
// field set, change BOTH. The projection is keyed by the stable injected `rosterIndex`, so PS's
// live active-first array order and the engine's canonical roster order compare equal.
const VOL_KEYS = {
	confusion: ['time', 'duration'], substitute: ['hp'], taunt: ['duration'],
	encore: ['move', 'duration'], disable: ['move', 'duration'], yawn: ['duration'],
	throatchop: ['duration'], healblock: ['duration'], stall: ['counter', 'duration'],
	partiallytrapped: ['duration', 'boundDivisor'], twoturnmove: ['move'],
	lockedmove: ['move', 'trueDuration'],
};
// Volatiles convert maps but that carry no compared internals (presence only).
const VOL_PRESENCE = new Set([
	'leechseed', 'perish1', 'perish2', 'perish3', 'perishsong', 'choicelock', 'saltcure', 'curse',
	'nightmare', 'attract', 'torment', 'destinybond', 'glaiverush', 'trapped', 'trapper', 'ingrain',
	'noretreat', 'octolock', 'flashfire', 'truant', 'protosynthesis', 'quarkdrive', 'focusenergy',
	'dragoncheer', 'unburden', 'mustrecharge', 'roost', 'protect', 'endure', 'flinch', 'charge',
	'chillyreception',
]);
const TWO_TURN_MARKERS = new Set([
	'fly', 'dig', 'dive', 'bounce', 'phantomforce', 'shadowforce', 'skydrop', 'solarbeam',
	'solarblade', 'meteorbeam', 'electroshot', 'skullbash', 'skyattack', 'razorwind', 'freezeshock',
	'iceburn', 'geomancy',
]);

function pickKeys(obj, keys) {
	const o = {};
	for (const k of keys) if (obj[k] !== undefined) o[k] = obj[k];
	return o;
}

function projVolatiles(volatiles) {
	const out = {};
	for (const [k, v] of Object.entries(volatiles || {})) {
		if (VOL_KEYS[k]) out[k] = pickKeys(v, VOL_KEYS[k]);
		else if (VOL_PRESENCE.has(k)) out[k] = 1;
		else if (TWO_TURN_MARKERS.has(k)) { /* per-move charge marker: convert skips it */ }
		else out[k] = pickKeys(v, VOL_KEYS[k] || []); // unknown -> presence, no internals
	}
	return out;
}

function refId(s) {
	// "[Species:x]" / "[Move:x]" -> "x"; passthrough otherwise.
	if (typeof s !== 'string') return s;
	const m = s.match(/^\[[A-Za-z]+:(.+)\]$/);
	return m ? m[1] : s;
}

function toID(s) {
	return String(s || '').toLowerCase().replace(/[^a-z0-9]/g, '');
}

function projStatusState(status, ss) {
	if (!status || status === 'fnt') return null;
	const o = { id: status };
	if (status === 'slp' && ss.time !== undefined) o.time = ss.time;
	if (status === 'tox' && ss.stage !== undefined) o.stage = ss.stage;
	// sleep-clause side encoding: only which SIDE source/target sit on matters.
	if (status === 'slp') {
		const side = (r) => (typeof r === 'string' ? (r.match(/p[12]/) || [''])[0] : '');
		o.foeSourced = !!(ss.source && ss.target && side(ss.source) !== side(ss.target));
	}
	return o;
}

// Cosmetic-only formes (identical base stats/types/abilities — flower colour, pattern, sweet, …)
// that the engine collapses to their base species. The recorded PS snapshot keeps the colour; the
// transplant (via the engine's exported state) uses the base. Gameplay-identical → normalize.
const COSMETIC_FORME_BASE = [
	'florges', 'vivillon', 'alcremie', 'minior', 'sinistcha', 'poltchageist', 'squawkabilly',
];
function normSpecies(id) {
	for (const base of COSMETIC_FORME_BASE) if (id.startsWith(base)) return base;
	return id;
}

function projPokemon(p) {
	// A fainted mon: PS serializes status as "fnt" OR "" depending on when the snapshot is taken
	// relative to faint processing; convert.rs maps BOTH (and hp<=0) to Status::None, so normalize.
	const fainted = p.hp <= 0 || p.status === 'fnt';
	const status = fainted ? '' : (p.status || '');
	return {
		species: normSpecies(toID(refId(p.species) || (p.details || '').split(',')[0])),
		hp: p.hp, maxhp: p.maxhp,
		status, statusState: fainted ? null : projStatusState(status, p.statusState || {}),
		item: p.item || '',
		ability: p.ability || '', baseAbility: p.baseAbility || p.ability || '',
		terastallized: !!p.terastallized,
		teraType: toID(p.teraType || ''),
		timesAttacked: p.timesAttacked || 0,
		transformed: !!p.transformed,
		types: (p.types || []).map(toID),
		// `disabled` is dropped relative to transplant-gate's copy of this projection: it is a
		// REQUEST-time PS flag (choice lock, Encore, Torment) recomputed when PS builds a request,
		// and the exporter never emits it. transplant-gate compares two *recorded PS* snapshots so
		// both carry it; here one side is exporter output, so comparing it is pure noise.
		moveSlots: (p.moveSlots || []).map(m => ({ id: m.id, pp: m.pp })),
		ateBerry: !!p.ateBerry,
		lastItem: p.ateBerry ? (p.lastItem || '') : '',
	};
}

function sideConditionsProj(sc) {
	const out = {};
	for (const [k, v] of Object.entries(sc || {})) {
		if (['reflect', 'lightscreen', 'auroraveil', 'tailwind'].includes(k)) out[k] = { duration: v.duration };
		else if (['spikes', 'toxicspikes'].includes(k)) out[k] = { layers: v.layers };
		else out[k] = 1;
	}
	return out;
}

function slotConditionsProj(slotConds, turn) {
	const entries = Array.isArray(slotConds) ? slotConds : [slotConds];
	const out = {};
	for (const sc of entries) {
		for (const [k, v] of Object.entries(sc || {})) {
			if (k === 'wish') out.wish = { turns: (turn <= (v.startingTurn || 0) + 1) ? 2 : 1, hp: Math.trunc(v.hp) };
			else if (k === 'futuremove') out.futuremove = { remaining: Math.max(1, (v.endingTurn + 2 - turn)) };
			else if (k === 'healingwish' || k === 'lunardance') out.healingwish = 1;
			else out[k] = 1;
		}
	}
	return out;
}

// Full engine-modeled projection of a serialized battle state, keyed by stable roster index.
// `rosterOf(side, pokemon)` yields the roster index (recorded snapshots carry `rosterIndex`;
// the transplanted battle supplies a closure over its start-of-continuation set array).
function projectState(state, rosterOf) {
	const f = state.field || {};
	const proj = {
		turn: state.turn,
		weather: f.weather || '', weatherDur: f.weather ? (f.weatherState.duration ?? null) : 0,
		terrain: f.terrain || '', terrainDur: f.terrain ? (f.terrainState.duration ?? null) : 0,
		trickroom: !!(f.pseudoWeather && f.pseudoWeather.trickroom),
		trickroomDur: (f.pseudoWeather && f.pseudoWeather.trickroom) ? f.pseudoWeather.trickroom.duration : 0,
		sides: [],
	};
	for (let si = 0; si < state.sides.length; si++) {
		const side = state.sides[si];
		const mons = {};
		let activeRoster = -1;
		for (const p of side.pokemon) {
			if (!p || (refId(p.species) === '' && !p.details)) continue;
			const ri = rosterOf(si, p);
			mons[ri] = projPokemon(p);
			if (p.isActive) {
				activeRoster = ri;
				mons[ri].active = true;
				mons[ri].boosts = p.boosts || {};
				mons[ri].activeTurns = p.activeTurns || 0;
				mons[ri].volatiles = projVolatiles(p.volatiles);
				mons[ri].statsRaisedThisTurn = !!p.statsRaisedThisTurn;
				mons[ri].statsLoweredThisTurn = !!p.statsLoweredThisTurn;
				mons[ri].lastMoveFailed = Array.isArray(p.lastMove?.hitTargets) && p.lastMove.hitTargets.length === 0;
				mons[ri].lastUsedMove = p.lastMove ? refId(p.lastMove.move) : '';
			}
		}
		proj.sides.push({
			activeRoster,
			// Derive teraUsed from canTerastallize (PS nulls it side-wide once anyone teras) for BOTH
			// sides: the exported `side.teraUsed` is a static recorder-synthesized field PS never
			// updates, so it goes stale in the transplant after an in-continuation tera. Recorded
			// snapshots keep per-mon canTerastallize too, so this is consistent.
			teraUsed: !side.pokemon.some(p => !!p.canTerastallize),
			mons,
			sideConditions: sideConditionsProj(side.sideConditions),
			slotConditions: slotConditionsProj(side.slotConditions, state.turn),
		});
	}
	return proj;
}

// ---- choice re-resolution ----------------------------------------------------------

// The transplanted battle's pokemon array is in canonical (roster) order; recorded positional
// switch choices are relative to PS's live order at record time. Re-resolve to the CURRENT array
// position via the stable roster set identity.
function resolveChoice(battle, sideId, rec, roster) {
	const n = sideId === 'p1' ? 0 : 1;
	const r = rec.resolved;
	if (r.action === 'switch') {
		// Match by the stable, injected rosterIndex (the array is in active-first order and
		// reorders as mons switch, so positional indices drift; rosterIndex does not).
		const ri = r.rosterIndex;
		const pos = battle.sides[n].pokemon.findIndex(p => p.rosterIndex === ri);
		if (pos < 0) throw new Error(`switch target roster#${ri} not found`);
		return `switch ${pos + 1}`;
	}
	if (r.action === 'move') {
		// Resolve the move index against the REQUEST's offered move list (what PS actually accepts),
		// not the full moveSlots: when the mon is locked (rampage/recharge/two-turn) PS restricts the
		// list to the single locked move, which is "move 1" regardless of its original slot.
		const req = battle[sideId].activeRequest;
		const offered = req && req.active && req.active[0] && req.active[0].moves;
		let idx = offered ? offered.findIndex(m => m.id === r.moveId) : -1;
		if (idx < 0) {
			idx = battle.sides[n].active[0].moveSlots.findIndex(m => m.id === r.moveId);
		}
		if (idx < 0) idx = 0; // struggle / lost slot
		return `move ${idx + 1}${r.tera ? ' terastallize' : ''}`;
	}
	if (r.action === 'pass') return 'pass';
	return rec.choice; // default
}

// ---- diff walk ---------------------------------------------------------------------

function diffProjections(a, b, pathStr, out) {
	if (JSON.stringify(a) === JSON.stringify(b)) return;
	if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) {
		out.push(`${pathStr}: transplant=${JSON.stringify(a)} recorded=${JSON.stringify(b)}`);
		return;
	}
	const keys = new Set([...Object.keys(a), ...Object.keys(b)]);
	for (const k of keys) {
		diffProjections(a[k], b[k], pathStr ? `${pathStr}.${k}` : k, out);
	}
}


// Canonical (key-sorted) serialization. Plain JSON.stringify is key-ORDER sensitive, and the two
// sides of this comparison build their projections by walking objects PS and the exporter
// populated in different orders — so structurally identical states compared unequal and every
// deterministic decision looked like a divergence.
function key(v) {
	if (v === null || typeof v !== 'object') return JSON.stringify(v);
	if (Array.isArray(v)) return `[${v.map(key).join(',')}]`;
	return `{${Object.keys(v).sort().map(k => `${JSON.stringify(k)}:${key(v[k])}`).join(',')}}`;
}

// Both sides of the comparison are exporter output driven through PS, and the exporter injects a
// stable `rosterIndex` on every mon that survives serialize/deserialize — so one accessor serves
// the engine's snapshot and PS's continuation alike.
const rosterOf = (_si, p) => (p.rosterIndex !== undefined ? p.rosterIndex : 0);

// ---- forced PRNG (outcome-tree enumeration) ----------------------------------------------

class NeedRandom extends Error {
	constructor(kind, args, options) {
		super(`need ${kind}`);
		this.kind = kind;
		this.args = args;
		this.options = options;
	}
}

/// Replace the battle's PRNG with one that replays `prefix` and then throws on the first
/// unforced draw, naming every value it could have taken. DFS over those options enumerates the
/// exact outcome tree.
function installForcedPrng(battle, prefix) {
	let cursor = 0;
	const take = (kind, args, options) => {
		if (cursor < prefix.length) {
			const f = prefix[cursor++];
			return f.value;
		}
		throw new NeedRandom(kind, args, options);
	};
	battle.prng = {
		next: (from, to) => {
			if (from === undefined) return take('next', [], [0.5]);
			if (to === undefined) { to = from; from = 0; }
			return take('random', [from, to], Array.from({ length: to - from }, (_, i) => from + i));
		},
		random: (from, to) => {
			if (from === undefined) return take('next', [], [0.5]);
			if (to === undefined) { to = from; from = 0; }
			return take('random', [from, to], Array.from({ length: to - from }, (_, i) => from + i));
		},
		randomChance: (num, den) => take('randomChance', [num, den], [true, false]),
		sample: items => items[take('sample', [items.length], items.map((_, i) => i))],
		shuffle: (items, start = 0, end = items.length) => {
			while (start < end - 1) {
				const nextIndex = take('random', [start, end], Array.from({ length: end - start }, (_, i) => start + i));
				[items[start], items[nextIndex]] = [items[nextIndex], items[start]];
				start++;
			}
		},
		get startingSeed() { return [0, 0, 0, 0]; },
		getSeed: () => [0, 0, 0, 0],
		clone() { return this; },
	};
}

/// Every distinct projected state PS can reach from `pre` under `choices`. Stops early once
/// `want` is found. Returns {found, paths, capped, sample}.
function enumerateOutcomes(pre, choices, want, maxPaths) {
	const seen = new Set();
	let paths = 0;
	let capped = false;
	let sample = null;
	const stack = [[]];

	while (stack.length) {
		if (paths >= maxPaths) { capped = true; break; }
		const prefix = stack.pop();
		let battle;
		try {
			battle = State.deserializeBattle(JSON.parse(JSON.stringify(pre)));
			battle.restart(() => {});
		} catch (e) {
			return { found: false, paths, capped: false, error: `deserialize: ${e.message}` };
		}
		installForcedPrng(battle, prefix);
		try {
			// PS commits by itself once every side that owes a choice has given one — an explicit
			// commitChoices() here fires early and throws "Not all choices done".
			for (const [sideId, choice] of Object.entries(choices)) {
				if (!battle.choose(sideId, choice)) {
					return { found: false, paths, capped: false,
						rejected: `PS rejected '${choice}' for ${sideId}` };
				}
			}
			paths++;
			const proj = key(projectState(State.serializeBattle(battle), rosterOf));
			if (!sample) sample = proj;
			if (proj === want) return { found: true, paths, capped: false };
			seen.add(proj);
		} catch (e) {
			if (e instanceof NeedRandom) {
				// Branch: one continuation per possible value of this draw.
				for (const v of e.options) stack.push([...prefix, { kind: e.kind, args: e.args, value: v }]);
				continue;
			}
			return { found: false, paths, capped: false, error: `${e.message}` };
		}
	}
	// "Nearest" = the reachable outcome with the fewest projected-field differences; it turns an
	// unreachable-state finding into a field-level diff.
	let nearest = null, best = Infinity;
	for (const cand of seen) {
		const d = [];
		diffProjections(JSON.parse(cand), JSON.parse(want), '', d);
		if (d.length < best) { best = d.length; nearest = cand; }
	}
	return { found: false, paths, capped, distinct: seen.size, sample, nearest };
}

// ---- legality cross-check ----------------------------------------------------------------

/// The engine's 13-action mask -> the PS choice strings it claims are legal.
function engineLegalChoices(mask, battle, sideIdx) {
	const side = battle.sides[sideIdx];
	const active = side.pokemon[0];
	const out = new Set();
	for (let i = 0; i < 4; i++) if (mask[i]) out.add(`move ${i + 1}`);
	for (let k = 0; k < 5; k++) {
		if (!mask[4 + k]) continue;
		// engine action 4+k = the k-th *bench* slot; against the exported (active-first) array
		// that is array index k+1.
		out.add(`switch ${k + 2}`);
	}
	for (let i = 0; i < 4; i++) if (mask[9 + i]) out.add(`move ${i + 1} terastallize`);
	void active;
	return out;
}

/// What PS's own request says is legal at this position.
function psLegalChoices(battle, sideIdx) {
	const side = battle.sides[sideIdx];
	const req = side.activeRequest;
	const out = new Set();
	if (!req || req.wait) return out;
	if (req.forceSwitch) {
		for (let i = 0; i < side.pokemon.length; i++) {
			const p = side.pokemon[i];
			if (!p.isActive && p.hp > 0) out.add(`switch ${i + 1}`);
		}
		return out;
	}
	const act = req.active?.[0];
	if (!act) return out;
	for (let i = 0; i < act.moves.length; i++) {
		const m = act.moves[i];
		if (m.disabled || (m.pp !== undefined && m.pp <= 0)) continue;
		out.add(`move ${i + 1}`);
		if (act.canTerastallize) out.add(`move ${i + 1} terastallize`);
	}
	if (!act.trapped && !act.maybeTrapped) {
		for (let i = 0; i < side.pokemon.length; i++) {
			const p = side.pokemon[i];
			if (!p.isActive && p.hp > 0) out.add(`switch ${i + 1}`);
		}
	}
	return out;
}

// ---- main ---------------------------------------------------------------------------------

const lines = fs.readFileSync(SAMPLES, 'utf8').split('\n').filter(Boolean);
const stats = {
	psCommit: PS_COMMIT, samples: lines.length,
	loaded: 0, loadFailed: 0, phaseUnrepresentable: 0,
	legalityOk: 0, legalityDiverged: 0,
	memberOk: 0, memberDiverged: 0, memberInconclusive: 0, memberSkipped: 0,
};
const findings = [];

for (const [n, line] of lines.entries()) {
	let rec;
	try {
		rec = JSON.parse(line);
	} catch (e) {
		findings.push({ n, kind: 'parse', detail: e.message });
		continue;
	}

	// 1) LOADS
	let battle;
	try {
		battle = State.deserializeBattle(JSON.parse(JSON.stringify(rec.pre)));
		battle.restart(() => {});
		stats.loaded++;
	} catch (e) {
		stats.loadFailed++;
		findings.push({ n, env: rec.env, step: rec.step, kind: 'load', detail: e.message });
		continue;
	}

	// A SINGLE-SIDED decision (forced faint replacement, pivot landing) is not representable in
	// the exported snapshot: PS's `requestState` is not part of serializeBattle's modeled output,
	// so a deserialized battle always resumes believing it is at a normal turn. Both checks below
	// would then compare an engine mid-turn replacement against a PS full turn and report a
	// divergence on every such decision — 100% false positives, and enough of them to bury real
	// signal. Count them and move on; covering replacement phases needs the seed-exact
	// (Replicate-mode) sidecar, not this transplant.
	if (!(rec.acting?.p1 && rec.acting?.p2)) {
		stats.phaseUnrepresentable++;
		continue;
	}

	// 2) LEGALITY
	for (const [sideId, idx] of [['p1', 0], ['p2', 1]]) {
		if (!rec.acting?.[sideId]) continue;
		const eng = engineLegalChoices(rec.legal[sideId], battle, idx);
		const ps = psLegalChoices(battle, idx);
		if (!ps.size) continue; // PS issued no request for this side at this position
		const engineOnly = [...eng].filter(c => !ps.has(c));
		const psOnly = [...ps].filter(c => !eng.has(c));
		if (engineOnly.length || psOnly.length) {
			stats.legalityDiverged++;
			findings.push({
				n, env: rec.env, step: rec.step, kind: 'legality', side: sideId,
				engineOnly, psOnly,
			});
		} else {
			stats.legalityOk++;
		}
	}

	// 3) MEMBERSHIP
	if (!rec.post || !Object.keys(rec.choices ?? {}).length) {
		stats.memberSkipped++;
		continue;
	}
	const want = key(projectState(rec.post, rosterOf));
	const res = enumerateOutcomes(rec.pre, rec.choices, want, MAX_PATHS);
	if (res.error || res.rejected) {
		stats.memberDiverged++;
		findings.push({ n, env: rec.env, step: rec.step, kind: res.rejected ? 'rejected' : 'drive',
			detail: res.rejected ?? res.error, choices: rec.choices });
	} else if (res.found) {
		stats.memberOk++;
	} else if (res.capped) {
		stats.memberInconclusive++;
	} else {
		stats.memberDiverged++;
		// The field-level story: diff the engine's post-state against the PS outcome closest to
		// it. Without this a finding says only "not reachable", which is not actionable.
		const diffs = [];
		if (res.nearest) diffProjections(JSON.parse(res.nearest), projectState(rec.post, rosterOf), '', diffs);
		findings.push({
			n, env: rec.env, step: rec.step, kind: 'membership', choices: rec.choices,
			paths: res.paths, distinctOutcomes: res.distinct,
			diff: diffs.slice(0, 6),
			detail: 'engine post-state is not reachable in PS from this position',
		});
	}
	if (VERBOSE && n % 5 === 0) process.stderr.write(`  ..${n}/${lines.length}\n`);
}

const diverged = stats.loadFailed + stats.legalityDiverged + stats.memberDiverged;
console.log('========== ON-POLICY COSIM GATE ==========');
console.log(`(ps ${PS_COMMIT})`);
console.log(`samples:            ${stats.samples}`);
console.log(`exported states:    ${stats.loaded} loaded, ${stats.loadFailed} FAILED`);
console.log(`skipped (phase):    ${stats.phaseUnrepresentable} single-sided requests ` +
	`(replacement / pivot landing — not representable in a snapshot)`);
console.log(`legality:           ${stats.legalityOk} match, ${stats.legalityDiverged} DIVERGED`);
console.log(`outcome membership: ${stats.memberOk} in PS support, ${stats.memberDiverged} DIVERGED, ` +
	`${stats.memberInconclusive} inconclusive (path cap), ${stats.memberSkipped} skipped`);
// Rank by the differing FIELD, not by sample: one engine/PS disagreement shows up on every
// decision that touches it, so the field histogram is the work queue and the sample list is not.
if (findings.length) {
	const cats = new Map();
	for (const f of findings) {
		let cat;
		if (f.kind === 'membership') {
			// The leaf field name, with roster/side indices stripped, is the divergence class.
			cat = f.diff?.length
				? `membership: ${f.diff[0].split(':')[0].replace(/\.\d+/g, '.*')}`
				: 'membership: (no nearest outcome)';
		} else if (f.kind === 'legality') {
			cat = `legality: ${f.engineOnly.length ? 'engine-only ' + f.engineOnly.join('/') : ''}` +
				`${f.psOnly.length ? 'ps-only ' + f.psOnly.map(c => c.split(' ')[0]).join('/') : ''}`;
		} else {
			cat = `${f.kind}: ${String(f.detail).slice(0, 60)}`;
		}
		cats.set(cat, (cats.get(cat) ?? 0) + 1);
	}
	console.log('\nfinding classes (ranked):');
	for (const [cat, n] of [...cats].sort((a, b) => b[1] - a[1])) {
		console.log(`  ${String(n).padStart(4)}  ${cat}`);
	}
	console.log('\nfirst findings:');
	for (const f of findings.slice(0, 8)) console.log(`  ${JSON.stringify(f)}`);
	if (findings.length > 8) console.log(`  ... and ${findings.length - 8} more (--json for all)`);
}
if (JSON_OUT) {
	fs.writeFileSync(JSON_OUT, JSON.stringify({ stats, findings }, null, 1));
	console.log(`\nreport -> ${JSON_OUT}`);
}
process.exit(diverged ? 1 : 0);
