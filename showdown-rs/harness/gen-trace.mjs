/**
 * Battle trace generator.
 *
 * Drives a seeded, deterministic gen9customgame battle in the real Pokémon Showdown
 * simulator, with reproducible scripted choices, and records a replayable trace
 * (see TRACE_FORMAT.md). This is the ground truth the Rust engine is verified against.
 *
 * Usage:
 *   node gen-trace.mjs [--seed N] [--max-turns N] [--out path]
 *
 * The PS sim is loaded from the sibling clone at ../../engines/pokemon-showdown/dist/sim.
 */

import { fileURLToPath } from 'node:url';
import path from 'node:path';
import fs from 'node:fs';
import { createRequire } from 'node:module';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const PS_DIR = path.resolve(__dirname, '../../engines/pokemon-showdown');
const sim = require(path.join(PS_DIR, 'dist/sim'));
const { BattleStream, getPlayerStreams, Teams } = sim;

// ---- args -------------------------------------------------------------------
const argv = process.argv.slice(2);
function arg(name, def) {
	const i = argv.indexOf(`--${name}`);
	return i >= 0 ? argv[i + 1] : def;
}
const SEED_NUM = Number(arg('seed', '1'));
const FORMAT = arg('format', 'gen9customgame');
const MAX_TURNS = Number(arg('max-turns', '50'));
const OUT = arg('out', path.join(__dirname, 'traces', `trace-seed${SEED_NUM}.json`));

// A simple deterministic PRNG for *choice* selection (independent of the battle PRNG, which
// is seeded separately). Each player gets its OWN stream so the async interleaving of p1/p2
// request handling can't change the draw order — that's what makes the trace reproducible
// from --seed alone (a shared stream drifted with scheduling).
function makeRng(seed) {
	let st = (BigInt(seed) * 0x9e3779b97f4a7c15n + 1n) & 0xffffffffffffffffn;
	return (n) => {
		st = (st * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
		return Number((st >> 33n) % BigInt(n));
	};
}
const choiceRng = { p1: makeRng(SEED_NUM * 2 + 1), p2: makeRng(SEED_NUM * 2 + 2) };

// PS PRNG seed (32 bytes hex) derived deterministically from --seed.
function psSeed(n) {
	let hex = '';
	let x = BigInt(n) ^ 0xa5a5a5a5a5a5a5a5n;
	for (let i = 0; i < 8; i++) {
		x = (x * 6364136223846793005n + 1442695040888963407n) & 0xffffffffffffffffn;
		hex += x.toString(16).padStart(16, '0').slice(0, 8);
	}
	return `sodium,${hex}`;
}

// ---- teams ------------------------------------------------------------------
// Built from species/moves that exist in the Rust engine's id slice so traces are
// parseable on the Rust side. Hand-picked legal-ish gen9 sets (customgame is lenient).
const TEAM_1 = `
Great Tusk @ Booster Energy
Ability: Protosynthesis
Tera Type: Ground
EVs: 252 Atk / 4 Def / 252 Spe
Jolly Nature
- Earthquake
- Close Combat
- Headlong Rush
- Body Press

Gholdengo @ Choice Specs
Ability: Good as Gold
Tera Type: Steel
EVs: 252 SpA / 4 Def / 252 Spe
Timid Nature
- Make It Rain
- Shadow Ball
- Thunderbolt
- Draco Meteor

Garganacl @ Leftovers
Ability: Purifying Salt
Tera Type: Water
EVs: 252 HP / 4 Atk / 252 SpD
Careful Nature
- Salt Cure
- Recover
- Earthquake
- Body Press
`;

const TEAM_2 = `
Kingambit @ Leftovers
Ability: Supreme Overlord
Tera Type: Dark
EVs: 252 Atk / 4 Def / 252 Spe
Adamant Nature
- Kowtow Cleave
- Sucker Punch
- Close Combat
- Body Press

Dragapult @ Choice Band
Ability: Clear Body
Tera Type: Dragon
EVs: 252 Atk / 4 Def / 252 Spe
Jolly Nature
- Dragon Darts
- U-turn
- Shadow Ball
- Earthquake

Glimmora @ Leftovers
Ability: Toxic Debris
Tera Type: Grass
EVs: 252 SpA / 4 Def / 252 Spe
Timid Nature
- Make It Rain
- Stealth Rock
- Spikes
- Thunderbolt
`;

// ---- state projection -------------------------------------------------------
// Project the live Battle object into the normalized State shape (see TRACE_FORMAT.md).
// PS reorders side.pokemon on switch, so we pin a canonical order (by object identity)
// captured at the first snapshot and emit pokemon in that stable order throughout.
const canonical = [null, null]; // canonical[sideIdx] = Map<PokemonObject, index>

function ensureCanonical(battle) {
	for (let i = 0; i < 2; i++) {
		if (!canonical[i]) {
			canonical[i] = new Map();
			battle.sides[i].pokemon.forEach((p, idx) => canonical[i].set(p, idx));
		}
	}
}

function projType(t) {
	return String(t).toLowerCase();
}

function projPokemon(p) {
	return {
		species: p.species.id,
		level: p.level,
		types: p.getTypes().map(projType),
		hp: p.hp,
		maxHp: p.maxhp,
		status: p.status || 'none',
		statusCounter: p.statusState ? (p.statusState.time ?? p.statusState.stage ?? 0) : 0,
		ability: p.ability || 'none',
		item: p.item || 'none',
		// Unboosted final stats PS computed at team build (HP is maxHp above).
		stats: {
			atk: p.storedStats.atk,
			def: p.storedStats.def,
			spa: p.storedStats.spa,
			spd: p.storedStats.spd,
			spe: p.storedStats.spe,
		},
		terastallized: !!p.terastallized,
		teraType: projType(p.teraType || 'none'),
		moves: p.moveSlots.map(m => ({
			id: m.id,
			pp: m.pp,
			maxPp: m.maxpp,
			disabled: !!m.disabled,
		})),
	};
}

function projSide(side, sideIdx) {
	const active = side.active[0];
	const order = canonical[sideIdx];
	// Emit pokemon in canonical order.
	const inOrder = new Array(order.size);
	for (const p of side.pokemon) {
		const idx = order.get(p);
		if (idx !== undefined) inOrder[idx] = p;
	}
	const sc = side.sideConditions || {};
	const layers = (k) => (sc[k] ? (sc[k].layers ?? 1) : 0);
	const dur = (k) => (sc[k] ? (sc[k].duration ?? 0) : 0);
	const b = active ? active.boosts : { atk: 0, def: 0, spa: 0, spd: 0, spe: 0, accuracy: 0, evasion: 0 };
	return {
		activeIndex: active ? order.get(active) : 0,
		boosts: {
			atk: b.atk, def: b.def, spa: b.spa, spd: b.spd, spe: b.spe,
			accuracy: b.accuracy, evasion: b.evasion,
		},
		volatiles: active ? Object.keys(active.volatiles) : [],
		substituteHp: active && active.volatiles.substitute ? (active.volatiles.substitute.hp ?? 0) : 0,
		sideConditions: {
			stealthRock: !!sc.stealthrock,
			spikes: layers('spikes'),
			toxicSpikes: layers('toxicspikes'),
			stickyWeb: !!sc.stickyweb,
			reflect: dur('reflect'),
			lightScreen: dur('lightscreen'),
			auroraVeil: dur('auroraveil'),
			tailwind: dur('tailwind'),
		},
		pokemon: inOrder.map(projPokemon),
		// Consecutive-use tracking of the active mon: last executed move and the Protect
		// "stall" failure divisor (PS stores it on the `stall` volatile's counter).
		lastUsedMove: active && active.lastMove ? active.lastMove.id : '',
		stallCounter: active && active.volatiles.stall ? (active.volatiles.stall.counter || 0) : 0,
		// Multi-turn move commitment, encoded as "", "charge:<move>", "recharge", or
		// "rampage:<move>:<turns>" (PS volatiles twoturnmove / mustrecharge / lockedmove).
		pendingMove: (() => {
			if (!active) return '';
			const lm = active.lastMove ? active.lastMove.id : '';
			if (active.volatiles.twoturnmove) return 'charge:' + lm;
			if (active.volatiles.mustrecharge) return 'recharge';
			if (active.volatiles.lockedmove) return 'rampage:' + lm + ':' + (active.volatiles.lockedmove.duration || 0);
			return '';
		})(),
	};
}

function projState(battle) {
	const field = battle.field;
	return {
		turn: battle.turn,
		weather: field.weather || 'none',
		weatherTurns: field.weatherState ? (field.weatherState.duration ?? 0) : 0,
		terrain: field.terrain || 'none',
		terrainTurns: field.terrainState ? (field.terrainState.duration ?? 0) : 0,
		trickRoom: !!(field.pseudoWeather && field.pseudoWeather.trickroom),
		trickRoomTurns: field.pseudoWeather && field.pseudoWeather.trickroom ? (field.pseudoWeather.trickroom.duration ?? 0) : 0,
		sides: [projSide(battle.sides[0], 0), projSide(battle.sides[1], 1)],
	};
}

// ---- choice selection -------------------------------------------------------
// Parse a |request| JSON and pick a reproducible legal choice; return the >pN string.
function chooseFor(request, rand) {
	if (!request || request.wait) return null;
	if (request.teamPreview) return 'default';
	if (request.forceSwitch) {
		// Switch to a random living, non-active benched mon.
		const mons = request.side.pokemon;
		const options = [];
		for (let i = 0; i < mons.length; i++) {
			if (!mons[i].active && mons[i].condition !== '0 fnt' && !mons[i].condition.endsWith(' fnt')) {
				options.push(i + 1);
			}
		}
		if (!options.length) return 'pass';
		return `switch ${options[rand(options.length)]}`;
	}
	if (request.active) {
		const act = request.active[0];
		const moves = act.moves;
		const legal = [];
		for (let i = 0; i < moves.length; i++) {
			if (!moves[i].disabled && (moves[i].pp === undefined || moves[i].pp > 0)) legal.push(i + 1);
		}
		if (!legal.length) return 'move 1'; // Struggle fallback
		const mv = legal[rand(legal.length)];
		// Terastallize when available (~50%) for Tera coverage — the defining gen9 mechanic.
		if (act.canTerastallize && rand(2) === 0) {
			return `move ${mv} terastallize`;
		}
		return `move ${mv}`;
	}
	return 'default';
}

// ---- run --------------------------------------------------------------------
async function main() {
	const battleStream = new BattleStream({ debug: true }); // exact HP + |debug| channel
	const streams = getPlayerStreams(battleStream);
	const seed = psSeed(SEED_NUM);

	// Random formats: generate each side's team *explicitly* with its own deterministic seed
	// and pass it in. (Letting BattleStream auto-generate from the spec seed is NOT reproducible
	// — it rolls a fresh seed for team generation — but Teams.generate(format, {seed}) is.)
	const isRandom = FORMAT.includes('random');
	const spec = { formatid: FORMAT, seed };
	const team1 = isRandom ? Teams.generate(FORMAT, { seed: psSeed(SEED_NUM * 2 + 1) }) : Teams.import(TEAM_1);
	const team2 = isRandom ? Teams.generate(FORMAT, { seed: psSeed(SEED_NUM * 2 + 2) }) : Teams.import(TEAM_2);
	const p1 = { name: 'Bot Red', team: Teams.pack(team1) };
	const p2 = { name: 'Bot Blue', team: Teams.pack(team2) };

	const trace = {
		format: spec.formatid,
		seed,
		players: { p1, p2 },
		start: null,
		snapshots: [],
		result: { winner: null },
	};

	// Per-turn choices, keyed by the turn the player acted on. The turn *action* (from
	// an `active` request) and any post-faint *replacement* switch (from a `forceSwitch`
	// request) are tracked separately — otherwise a forced switch would clobber the real
	// turn action, corrupting the trace.
	const actionsByTurn = new Map();      // turn -> { p1, p2 }            (single action)
	const replacementsByTurn = new Map(); // turn -> { p1: [...], p2: [...] } (ordered)
	function record(map, slot, choice) {
		const t = battleStream.battle ? battleStream.battle.turn : 0;
		if (!map.has(t)) map.set(t, {});
		map.get(t)[slot] = choice;
	}
	// Replacements can occur multiple times per turn (U-turn pivot, then a faint
	// replacement). Record them as an ordered list per side.
	function recordReplacement(slot, choice) {
		const t = battleStream.battle ? battleStream.battle.turn : 0;
		if (!replacementsByTurn.has(t)) replacementsByTurn.set(t, {});
		const m = replacementsByTurn.get(t);
		if (!m[slot]) m[slot] = [];
		m[slot].push(choice);
	}

	// Rewrite a "switch N" choice (N = position in PS's current, possibly-reordered
	// pokemon array) into "switch M" where M is our stable canonical index + 1. Move
	// choices and non-switches pass through unchanged.
	function remapSwitch(slot, choice) {
		if (!choice.startsWith('switch ')) return choice;
		const battle = battleStream.battle;
		if (!battle) return choice;
		ensureCanonical(battle);
		const sideIdx = slot === 'p1' ? 0 : 1;
		const k = parseInt(choice.split(' ')[1], 10); // 1-based PS index
		const livePoke = battle.sides[sideIdx].pokemon[k - 1];
		const canonIdx = canonical[sideIdx].get(livePoke);
		if (canonIdx === undefined) return choice;
		return `switch ${canonIdx + 1}`;
	}

	// Player loops: respond to each request with a scripted choice.
	for (const slot of ['p1', 'p2']) {
		(async () => {
			for await (const chunk of streams[slot]) {
				for (const line of chunk.split('\n')) {
					if (!line.startsWith('|request|')) continue;
					const json = line.slice('|request|'.length);
					if (!json) continue;
					const request = JSON.parse(json);
					const choice = chooseFor(request, choiceRng[slot]);
					if (choice && choice !== 'pass') {
							const recorded = remapSwitch(slot, choice);
							if (request.forceSwitch) {
								recordReplacement(slot, recorded);
							} else {
								record(actionsByTurn, slot, recorded);
							}
						void streams[slot].write(choice);
					} else if (choice === 'pass') {
						void streams[slot].write('pass');
					}
				}
			}
		})();
	}

	// Omniscient loop: snapshot at each turn boundary.
	let lastSnapshotTurn = 0;
	const omniscientDone = (async () => {
		for await (const chunk of streams.omniscient) {
			const battle = battleStream.battle;
			if (!battle) continue;
			for (const line of chunk.split('\n')) {
				const parts = line.split('|');
				const cmd = parts[1];
				if (cmd === 'turn') {
					const turn = Number(parts[2]);
					ensureCanonical(battle);
					if (turn === 1) {
						trace.start = { turn: 1, state: projState(battle) };
					} else {
						// State now reflects the resolution of turn `turn-1`.
						const prev = turn - 1;
						trace.snapshots.push({
							turn: prev,
							choices: actionsByTurn.get(prev) || {},
							replacements: replacementsByTurn.get(prev) || {},
							outcomes: [],
							state: projState(battle),
						});
						lastSnapshotTurn = prev;
					}
				} else if (cmd === 'win' || cmd === 'tie') {
					ensureCanonical(battle);
					const t = battle.turn;
					if (t > lastSnapshotTurn) {
						trace.snapshots.push({
							turn: t,
							choices: actionsByTurn.get(t) || {},
							replacements: replacementsByTurn.get(t) || {},
							outcomes: [],
							state: projState(battle),
						});
					}
					trace.result.winner = cmd === 'tie' ? 'tie' : (parts[2] === p1.name ? 'p1' : 'p2');
				}
			}
		}
	})();

	void streams.omniscient.write(
		`>start ${JSON.stringify(spec)}\n` +
		`>player p1 ${JSON.stringify(p1)}\n` +
		`>player p2 ${JSON.stringify(p2)}`
	);

	// Safety cap: stop on game end or the turn limit (both deterministic per seed). The only
	// other exit is a *stall* — the turn number not advancing for many polls, which means the
	// scripted AI didn't answer some request. We detect that by lack of turn progress (not by
	// wall-clock), so the trace length is reproducible regardless of machine speed.
	const cap = (async () => {
		let lastTurn = -1, stalled = 0;
		while (true) {
			await new Promise(r => setTimeout(r, 5));
			const b = battleStream.battle;
			if (!b) continue;
			if (b.ended || b.turn > MAX_TURNS) break;
			if (b.turn === lastTurn) stalled++;
			else { stalled = 0; lastTurn = b.turn; }
			if (stalled > 600) { // ~3s without a single turn advancing → genuinely stuck
				try { void streams.omniscient.write('>forcetie'); } catch {}
				break;
			}
		}
	})();

	await Promise.race([omniscientDone, cap]);
	// Give the streams a tick to flush the final messages.
	await new Promise(r => setTimeout(r, 20));

	fs.mkdirSync(path.dirname(OUT), { recursive: true });
	fs.writeFileSync(OUT, JSON.stringify(trace, null, 2));
	console.log(`wrote ${OUT}: ${trace.snapshots.length} snapshots, winner=${trace.result.winner}`);
}

main().then(() => process.exit(0)).catch(e => { console.error(e); process.exit(1); });
