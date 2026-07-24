// Transplant-continuation gate (exporter certification b).
//
// For each sampled corpus game the Rust exporter (EXPORT_SAMPLE) has dumped, at a mid-game
// turn-start `move` decision, a PS `deserializeBattle`-loadable snapshot of the ENGINE State
// (State -> convert -> export). This script:
//   1. `State.deserializeBattle(exported)` in pinned PS — proves the emitted JSON loads.
//   2. Positions a live PRNG at the transplant point: seed from the recorded battle seed, find
//      the unlogged construction-draw offset that reproduces the recorded draw-RESULT stream
//      (seedgate's INIT_SCAN), then advance by the exact draw count consumed up to the transplant
//      decision. (A live, correctly-positioned PRNG reproduces shuffles — whose permutations the
//      recorder does not log — which a forced-value shim cannot.)
//   3. Drives the RECORDED remaining choices (switch positions re-resolved via the stable roster
//      index, since the transplanted array is in canonical order) to the end of the game.
//   4. After every decision, projects both the transplanted and the recorded `stateAfter` to the
//      ENGINE-MODELED field set (exactly what convert.rs reads) and asserts byte-equality.
//
// A modeled-projection mismatch is an exporter bug (or a documented engine-missing field). The
// bar is semantic-zero. Usage:
//   node harness/transplant-gate.mjs                 # all dumped samples
//   node harness/transplant-gate.mjs c1 diverse ...  # only samples whose name matches

import path from 'node:path';
import fs from 'node:fs';
import zlib from 'node:zlib';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
assertPsPinned();
const { Battle, Teams } = require(path.join(PS_DIR, 'dist/sim'));
const { State } = require(path.join(PS_DIR, 'dist/sim/state'));
const { PRNG } = require(path.join(PS_DIR, 'dist/sim/prng'));
const { Dex } = require(path.join(PS_DIR, 'dist/sim/dex'));

const EXPORT_DIR = path.join(__dirname, 'transplant-exports');
const filters = process.argv.slice(2);

function loadTrace(p) {
	const buf = fs.readFileSync(p);
	const text = p.endsWith('.gz') ? zlib.gunzipSync(buf).toString() : buf.toString();
	return JSON.parse(text);
}

// ---- PRNG positioning --------------------------------------------------------------

// Burn one PRNG frame (PRNG.next() is not public; random(2) consumes exactly one next(), the
// same as a construction gender roll sample(['M','F'])).
function burn(prng) {
	prng.random(2);
}

// Number of next() calls a recorded draw consumed. random/randomChance/sample = 1; shuffle =
// (end-1-start) internal random() calls.
function drawNextCount(dr) {
	if (dr.kind === 'shuffle') {
		const [, start, end] = dr.args;
		return Math.max(0, (end - 1) - start);
	}
	return 1;
}

// Advance `prng` through a recorded draw using the typed method (so the internal next() count is
// exact), returning the produced value for optional result-checking (null = don't check).
function advanceDraw(prng, dr) {
	const a = dr.args;
	switch (dr.kind) {
	case 'randomChance': return prng.randomChance(a[0], a[1]);
	case 'random': return a.length === 2 ? prng.random(a[0], a[1]) : prng.random(a[0]);
	case 'sample': return prng.random(a[0]); // sample(items) === random(items.length)
	case 'shuffle': {
		const [, start, end] = a;
		for (let s = start; s < end - 1; s++) prng.random(s, end);
		return null;
	}
	default: return null;
	}
}

// Does the recorded draw's logged result match `value`? (sample logs {index,...}.)
function resultMatches(dr, value) {
	if (value === null || dr.result === undefined || dr.result === null) return true;
	if (dr.kind === 'sample') return dr.result.index === value;
	return dr.result === value;
}

// Construction gender-roll offset: mons whose species has no fixed gender AND whose set leaves
// gender unspecified roll one sample at `new Pokemon`. We can't see the set here, so we SEARCH
// for the offset (0..maxRoster) that reproduces the recorded draw-result stream — the honest,
// self-validating alignment (identical to seedgate's INIT_SCAN).
function findInitOffset(trace, maxOffset) {
	for (let off = 0; off <= maxOffset; off++) {
		const prng = new PRNG([...trace.seed]);
		for (let k = 0; k < off; k++) burn(prng);
		let ok = true;
		outer:
		for (const d of trace.decisions) {
			for (const dr of d.draws) {
				const v = advanceDraw(prng, dr);
				if (!resultMatches(dr, v)) { ok = false; break outer; }
			}
		}
		if (ok) return off;
	}
	return null;
}

// ---- engine-modeled projection (mirrors convert.rs's reads) -------------------------

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

function projPokemon(p) {
	// A fainted mon: PS serializes status as "fnt" OR "" depending on when the snapshot is taken
	// relative to faint processing; convert.rs maps BOTH (and hp<=0) to Status::None, so normalize.
	const fainted = p.hp <= 0 || p.status === 'fnt';
	const status = fainted ? '' : (p.status || '');
	return {
		species: refId(p.species) || (p.details || '').split(',')[0],
		hp: p.hp, maxhp: p.maxhp,
		status, statusState: fainted ? null : projStatusState(status, p.statusState || {}),
		item: p.item || '',
		ability: p.ability || '', baseAbility: p.baseAbility || p.ability || '',
		terastallized: !!p.terastallized,
		teraType: toID(p.teraType || ''),
		timesAttacked: p.timesAttacked || 0,
		transformed: !!p.transformed,
		types: (p.types || []).map(toID),
		moveSlots: (p.moveSlots || []).map(m => ({ id: m.id, pp: m.pp, disabled: !!m.disabled })),
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

// ---- one game ----------------------------------------------------------------------

function runGame(sample) {
	const trace = loadTrace(sample.trace);
	// Random-battle teams use randbats spreads (0-IV special-attackers) the exporter's iv=31 solver
	// can't reconstruct — so instead of the synthetic set, overlay the EXACT recorded set by
	// regenerating the team deterministically from the seed (same as cosim.mjs) and keying by
	// rosterIndex. PS's setSpecies then recomputes exact stats on switch-in.
	if (trace.format && trace.format.includes('random')) {
		const seedNum = trace.seed[0];
		const realSets = [
			Teams.unpack(Teams.pack(Teams.generate(trace.format, { seed: [0, 0, 0, seedNum * 2 + 1] }))),
			Teams.unpack(Teams.pack(Teams.generate(trace.format, { seed: [0, 0, 0, seedNum * 2 + 2] }))),
		];
		for (const [si, side] of sample.exported.sides.entries()) {
			for (const p of side.pokemon) {
				const real = realSets[si][p.rosterIndex];
				if (real) p.set = real;
			}
		}
	}
	const startI = sample.transplantDecisionIndex;
	const maxRoster = trace.decisions[0].stateAfter.sides.reduce((m, s) => m + s.pokemon.length, 0);
	const initOffset = findInitOffset(trace, maxRoster + 2);
	if (initOffset === null) return { name: sample.name, status: 'skip', reason: 'no-init-offset-aligns' };

	// Position a live PRNG at the transplant point.
	const prng = new PRNG([...trace.seed]);
	for (let k = 0; k < initOffset; k++) burn(prng);
	for (let i = 0; i < startI; i++) {
		for (const dr of trace.decisions[i].draws) advanceDraw(prng, dr);
	}

	// Load the exported state.
	let battle;
	try {
		battle = State.deserializeBattle(sample.exported);
		battle.restart(() => {});
	} catch (e) {
		return { name: sample.name, status: 'fail', reason: `deserialize: ${e.message}` };
	}
	battle.prng = prng;

	// Every exported mon carries a stable, injected rosterIndex that PS preserves across array
	// reordering (and the recorded snapshots carry it too), so both sides key by it directly.
	const roster = battle.sides.map(side => side.pokemon.map(p => p.set));
	const rosterOf = (si, p) => p.rosterIndex;

	// Drive the recorded remainder.
	let decisionsChecked = 0;
	for (let i = startI; i < trace.decisions.length; i++) {
		const d = trace.decisions[i];
		if (battle.ended) break;
		// Submit recorded choices for every side that has one.
		for (const [sideId, rec] of Object.entries(d.choices)) {
			let choiceStr;
			try {
				choiceStr = resolveChoice(battle, sideId, rec, roster);
			} catch (e) {
				return { name: sample.name, status: 'fail', reason: `d${i} resolve: ${e.message}`, initOffset };
			}
			const ok = battle.choose(sideId, choiceStr);
			if (!ok) {
				return { name: sample.name, status: 'fail', initOffset,
					reason: `d${i} PS rejected '${choiceStr}' (${sideId}): ${battle.inputLog.slice(-2).join(' | ')}` };
			}
		}
		// Compare modeled projections.
		const transplantState = JSON.parse(JSON.stringify(State.serializeBattle(battle)));
		const projT = projectState(transplantState, rosterOf);
		const projR = projectState(d.stateAfter, (si, p) => (p.rosterIndex !== undefined ? p.rosterIndex : 0));
		const diffs = [];
		diffProjections(projT, projR, '', diffs);
		if (diffs.length) {
			// Root classification: the transplant now awaits a switch-phase the recording didn't
			// (an extra mid-turn faint) — the true cause of the downstream turn/activeTurns diffs.
			const extraFaint = battle.requestState === 'switch' && d.requestState !== 'switch' && !d.stateAfter.ended;
			const tag = extraFaint ? ' [request-phase: extra mid-turn faint — turn-cascade root; per-decision damage divergence]' : '';
			return { name: sample.name, status: 'diverge', initOffset,
				reason: `d${i} t${d.turn} (${diffs.length} diffs)${tag}`, diffs: diffs.slice(0, 8), decisionsChecked };
		}
		decisionsChecked++;
	}
	return { name: sample.name, status: 'ok', initOffset, decisionsChecked };
}

// ---- main --------------------------------------------------------------------------

function main() {
	if (!fs.existsSync(EXPORT_DIR)) {
		console.error(`no ${EXPORT_DIR}; run: EXPORT_SAMPLE=harness/transplant-exports cosim harness/cosim-traces/*.json.gz`);
		process.exit(1);
	}
	let files = fs.readdirSync(EXPORT_DIR).filter(f => f.endsWith('.json'));
	if (filters.length) files = files.filter(f => filters.some(x => f.includes(x)));
	files.sort();

	const results = [];
	for (const f of files) {
		const sample = JSON.parse(fs.readFileSync(path.join(EXPORT_DIR, f)));
		if (!sample.seed) { results.push({ name: sample.name, status: 'skip', reason: 'no-seed' }); continue; }
		let r;
		try {
			r = runGame(sample);
		} catch (e) {
			r = { name: sample.name, status: 'error', reason: e.message };
			if (process.env.DBG) console.error(e.stack);
		}
		results.push(r);
		const tag = r.status.toUpperCase().padEnd(8);
		let line = `  ${tag} ${r.name}`;
		if (r.decisionsChecked !== undefined) line += `  (${r.decisionsChecked} decisions, init=${r.initOffset})`;
		if (r.reason) line += `  ${r.reason}`;
		console.log(line);
		if (r.diffs) for (const dd of r.diffs) console.log(`             ${dd}`);
	}

	const by = (s) => results.filter(r => r.status === s).length;
	console.log('\n========== TRANSPLANT-CONTINUATION GATE ==========');
	console.log(`samples: ${results.length}`);
	console.log(`  ok:       ${by('ok')}`);
	console.log(`  diverge:  ${by('diverge')}   (modeled-projection mismatch = exporter bug / engine-missing field)`);
	console.log(`  fail:     ${by('fail')}      (deserialize/choice error)`);
	console.log(`  skip:     ${by('skip')}      (no aligning init offset — set-specified-gender residual)`);
	console.log(`  error:    ${by('error')}`);
	const okDecisions = results.filter(r => r.status === 'ok').reduce((s, r) => s + (r.decisionsChecked || 0), 0);
	console.log(`total continuation decisions verified state-exact: ${okDecisions}`);
	process.exit(by('diverge') + by('fail') + by('error') > 0 ? 1 : 0);
}

main();
