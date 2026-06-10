// Co-simulation recorder (verification v2).
//
// Plays a PS battle through the *synchronous* Battle API and records, at every decision point:
//   - the request JSONs PS sent (the legality ground truth),
//   - the choices made (with a `resolved` form: absolute slots + move/species ids, so the
//     Rust side never re-interprets PS's positional choice strings),
//   - every PRNG draw consumed resolving those choices, with semantic labels
//     (the shared "outcome alphabet" both engines speak),
//   - the FULL serialized battle state (State.serializeBattle — nothing hand-projected).
//
// The Rust cosim crate replays these and demands exact equality on all modeled fields,
// classifying every turn matched / diverged / unsupported. Artifacts are stamped with the
// pinned PS commit; this script refuses to run against an unpinned clone.
//
// Usage: node harness/cosim.mjs --seed 1 --games 1 --out harness/cosim-traces/c1.json
//        node harness/cosim.mjs --seed 1 --teamset ou --max-decisions 400

import path from 'node:path';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

const PS_COMMIT = assertPsPinned();
const { Battle, Teams } = require(path.join(PS_DIR, 'dist/sim'));
const { State } = require(path.join(PS_DIR, 'dist/sim/state'));

// ---- args -------------------------------------------------------------------

const argv = process.argv.slice(2);
function arg(name, def) {
	const i = argv.indexOf(`--${name}`);
	return i >= 0 ? argv[i + 1] : def;
}
const SEED_NUM = Number(arg('seed', '1'));
const FORMAT = arg('format', 'gen9customgame');
const TEAMSET = arg('teamset', 'ou');
const MAX_DECISIONS = Number(arg('max-decisions', '600'));
const OUT = arg('out', path.join(__dirname, 'cosim-traces', `c${SEED_NUM}.json`));

// ---- teams (packed; unique species per side so idents are unambiguous) -------

const TEAM_OU_1 = [
	'Great Tusk||heavydutyboots|protosynthesis|headlongrush,closecombat,rapidspin,stealthrock|Jolly|,252,,,4,252|||||,,,,,Ground',
	'Gholdengo||choicescarf|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Flying',
	'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
	'Dragapult||heavydutyboots|clearbody|dragondarts,shadowball,uturn,dracometeor|Naive|,84,,172,,252|||||,,,,,Dragon',
	'Corviknight||leftovers|pressure|bravebird,bodypress,roost,uturn|Impish|248,,168,,92,|||||,,,,,Flying',
	'Garganacl||leftovers|purifyingsalt|saltcure,recover,bodypress,earthquake|Careful|252,,4,,252,|||||,,,,,Water',
].join(']');

const TEAM_OU_2 = [
	'Iron Valiant||leftovers|quarkdrive|moonblast,thunderbolt,closecombat,calmmind|Timid|,,,252,4,252|||||,,,,,Fairy',
	'Roaring Moon||heavydutyboots|protosynthesis|dragondance,knockoff,earthquake,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
	'Raging Bolt||leftovers|protosynthesis|thunderclap,dracometeor,thunderbolt,calmmind|Modest|64,,,252,,192|||||,,,,,Electric',
	'Slowking-Galar||leftovers|regenerator|sludgebomb,flamethrower,icebeam,slackoff|Calm|248,,8,,252,|||||,,,,,Water',
	'Dragonite||heavydutyboots|multiscale|dragondance,earthquake,firepunch,roost|Adamant|,252,,,4,252|||||,,,,,Flying',
	'Toxapex||leftovers|regenerator|surf,sludgebomb,recover,haze|Bold|248,252,8,,,|||||,,,,,Steel',
].join(']');

// Directed-coverage teams: deliberately exercise mechanics the OU pair never touches —
// sleep/yawn/para/burn status, screens + Light Clay, weather abilities, Trick Room, tailwind,
// spikes/toxic spikes/web stacks, Leech Seed, Substitute, Encore, Taunt, protect stalling.
const TEAM_DIVERSE_1 = [
	'Torkoal||heatrock|drought|lavaplume,yawn,stealthrock,rapidspin|Bold|248,,252,,8,|||||,,,,,Fire',
	'Amoonguss||rockyhelmet|regenerator|spore,gigadrain,sludgebomb,synthesis|Calm|248,,80,,180,|||||,,,,,Water',
	'Grimmsnarl||lightclay|prankster|reflect,lightscreen,spiritbreak,taunt|Careful|248,,8,,252,|||||,,,,,Steel',
	'Skarmory||rockyhelmet|sturdy|spikes,roost,bodypress,whirlwind|Impish|252,,252,,4,|||||,,,,,Water',
	'Slowking||leftovers|regenerator|trickroom,psychic,chillingwater,slackoff|Relaxed|252,,252,4,,|||||,,,,,Fairy',
	'Garchomp||leftovers|roughskin|substitute,earthquake,swordsdance,dragonclaw|Jolly|,252,,,4,252|||||,,,,,Steel',
].join(']');

const TEAM_DIVERSE_2 = [
	'Pelipper||damprock|drizzle|hydropump,hurricane,roost,uturn|Bold|248,,252,,8,|||||,,,,,Water',
	'Breloom||focussash|technician|machpunch,bulletseed,spore,swordsdance|Adamant|,252,,,4,252|||||,,,,,Fighting',
	'Klefki||lightclay|prankster|thunderwave,spikes,foulplay,lightscreen|Careful|248,,8,,252,|||||,,,,,Water',
	'Whimsicott||focussash|prankster|tailwind,encore,moonblast,memento|Timid|,,,252,4,252|||||,,,,,Fairy',
	'Ninetales-Alola||lightclay|snowwarning|auroraveil,blizzard,moonblast,encore|Timid|,,,252,4,252|||||,,,,,Ice',
	'Toxtricity||throatspray|punkrock|boomburst,overdrive,toxic,shiftgear|Modest|,,,252,4,252|||||,,,,,Normal',
].join(']');

// ---- deterministic choice RNG -------------------------------------------------

function makeRng(seed) {
	let s = BigInt(seed) + 0x9e3779b97f4a7c15n;
	return (n) => {
		s = (s + 0x9e3779b97f4a7c15n) & 0xffffffffffffffffn;
		let z = s;
		z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & 0xffffffffffffffffn;
		z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & 0xffffffffffffffffn;
		z = z ^ (z >> 31n);
		return Number(z % BigInt(n));
	};
}

// ---- PRNG instrumentation: the shared outcome alphabet -------------------------

function instrumentPrng(battle, draws) {
	const prng = battle.prng;
	const label = () => ({
		event: battle.event?.id ?? null,
		effect: battle.effect?.id ?? null,
		move: battle.activeMove?.id ?? null,
		pokemon: battle.activePokemon?.fullname ?? null,
	});
	// randomChance/sample call this.random internally; record only the outermost call so each
	// semantic draw appears exactly once.
	let depth = 0;
	const wrap = (name, fmtArgs) => {
		const orig = prng[name].bind(prng);
		prng[name] = (...args) => {
			depth++;
			let result;
			try {
				result = orig(...args);
			} finally {
				depth--;
			}
			if (depth === 0) {
				draws.push({ kind: name, args: fmtArgs(args), result: serializeResult(name, result, args), ...label() });
			}
			return result;
		};
	};
	wrap('random', (a) => a.filter(x => x !== undefined));
	wrap('randomChance', (a) => a);
	wrap('sample', (a) => [a[0]?.length]);          // record domain size, result index below
	wrap('shuffle', (a) => [a[0]?.length, a[1], a[2]]);
}

function serializeResult(kind, result, args) {
	if (kind === 'sample') {
		// store the index into the sampled list so the result is stable/serializable
		const idx = args[0].indexOf(result);
		return { index: idx, value: stringify(result) };
	}
	if (kind === 'shuffle') return null; // mutation in place; order divergence shows in outcomes
	return result;
}

function stringify(v) {
	if (v == null) return null;
	if (typeof v === 'object') return v.fullname ?? v.id ?? v.name ?? String(v);
	return v;
}

// ---- state snapshot (full serializeBattle, minus noise) -------------------------

const DROP_TOP = new Set([
	'log', 'messageLog', 'inputLog', 'sentRequests', 'hints', 'queue', 'events', 'eventDepth',
	'effect', 'effectState', 'event', 'formatData', 'sentLogPos', 'sentEnd', 'activeMove',
	'activePokemon', 'activeTarget', 'lastMoveLine', 'prng', 'prngSeed', 'speedOrder',
	'lastSuccessfulMoveThisTurn', 'lastDamage', 'effectOrder', 'quickClawRoll', 'formatid',
	'debugMode', 'forceRandomChance', 'strictChoices', 'gameType', 'activePerHalf', 'rated',
	'reportExactHP', 'reportPercentages', 'supportCancel', 'faintQueue', 'requestState', 'started',
]);
const DROP_SIDE = new Set(['foe', 'allySide', 'team', 'choice', 'avatar', 'activeRequest']);
const DROP_POKEMON = new Set(['m', 'set', 'speciesState', 'moveLastTurnResult']);

function snapshot(battle, roster) {
	const ser = State.serializeBattle(battle);
	const out = {};
	for (const k of Object.keys(ser)) {
		if (!DROP_TOP.has(k)) out[k] = ser[k];
	}
	out.sides = ser.sides.map((side, si) => {
		const s = {};
		for (const k of Object.keys(side)) {
			if (!DROP_SIDE.has(k)) s[k] = side[k];
		}
		s.pokemon = side.pokemon.map((p, pi) => {
			const q = {};
			for (const k of Object.keys(p)) {
				if (!DROP_POKEMON.has(k)) q[k] = p[k];
			}
			// Stable identity across PS's array reordering AND forme changes (Palafin-Hero
			// changes `details` mid-battle): index into the battle-start roster, keyed by the
			// live pokemon's immutable `set` object.
			q.rosterIndex = roster[si].indexOf(battle.sides[si].pokemon[pi].set);
			return q;
		});
		// Synthesized: has this side spent its Terastallization? PS deletes `terastallized`
		// from a mon when it faints (battle.ts `delete pokemon.terastallized`), so presence of
		// a tera'd mon is NOT a reliable signal. PS's own source of truth is per-mon
		// `canTerastallize`, nulled for the whole team when any ally teras.
		s.teraUsed = !battle.sides[si].pokemon.some(p => !!p.canTerastallize);
		return s;
	});
	out.winner = battle.winner ?? null;
	return JSON.parse(JSON.stringify(out)); // deep-copy + drop undefineds
}

// ---- choice policy ---------------------------------------------------------------

// Uniform-ish over legal options: mostly moves, sometimes a voluntary switch (the old
// harness never made voluntary switches — a coverage hole), tera ~25% when available.
function chooseFor(request, rand) {
	if (!request || request.wait) return null;
	if (request.teamPreview) return { choice: 'default', resolved: { action: 'teampreview' } };

	if (request.forceSwitch) {
		const mons = request.side.pokemon;
		const options = [];
		for (let i = 0; i < mons.length; i++) {
			if (!mons[i].active && !mons[i].condition.endsWith(' fnt') && mons[i].condition !== '0 fnt') {
				options.push(i);
			}
		}
		if (!options.length) return { choice: 'pass', resolved: { action: 'pass' } };
		const k = options[rand(options.length)];
		return {
			choice: `switch ${k + 1}`,
			resolved: { action: 'switch', ident: mons[k].ident, details: mons[k].details },
		};
	}

	if (request.active) {
		const act = request.active[0];
		const moves = [];
		for (let i = 0; i < act.moves.length; i++) {
			const m = act.moves[i];
			if (!m.disabled && (m.pp === undefined || m.pp > 0)) moves.push({ slot: i + 1, id: m.id });
		}
		const switches = [];
		if (!act.trapped) {
			const mons = request.side.pokemon;
			for (let i = 0; i < mons.length; i++) {
				if (!mons[i].active && !mons[i].condition.endsWith(' fnt') && mons[i].condition !== '0 fnt') {
					switches.push({ slot: i + 1, ident: mons[i].ident, details: mons[i].details });
				}
			}
		}
		// 15% voluntary switch when possible.
		if (switches.length && (!moves.length || rand(100) < 15)) {
			const s = switches[rand(switches.length)];
			return { choice: `switch ${s.slot}`, resolved: { action: 'switch', ident: s.ident, details: s.details } };
		}
		if (!moves.length) {
			// only Struggle remains; PS lists it as the lone move
			return { choice: 'move 1', resolved: { action: 'move', moveId: act.moves[0]?.id ?? 'struggle', tera: false } };
		}
		const mv = moves[rand(moves.length)];
		const tera = !!act.canTerastallize && rand(4) === 0;
		return {
			choice: `move ${mv.slot}${tera ? ' terastallize' : ''}`,
			resolved: { action: 'move', moveId: mv.id, tera },
		};
	}
	return { choice: 'default', resolved: { action: 'default' } };
}

// ---- main -----------------------------------------------------------------------

async function main() {
	// Teams: fixed packed sets, or PS-generated random-battle teams (deterministic per seed).
	let team1, team2;
	if (FORMAT.includes('random')) {
		team1 = Teams.pack(Teams.generate(FORMAT, { seed: [0, 0, 0, SEED_NUM * 2 + 1] }));
		team2 = Teams.pack(Teams.generate(FORMAT, { seed: [0, 0, 0, SEED_NUM * 2 + 2] }));
	} else {
		const [t1, t2] = TEAMSET === 'diverse' ? [TEAM_DIVERSE_1, TEAM_DIVERSE_2] : [TEAM_OU_1, TEAM_OU_2];
		team1 = Teams.pack(Teams.import(t1));
		team2 = Teams.pack(Teams.import(t2));
	}
	const battle = new Battle({
		// Random-battle TEAMS are pre-generated above; the battle itself runs as a custom game
		// so PS doesn't re-roll teams from the battle seed.
		formatid: FORMAT.includes('random') ? 'gen9customgame' : FORMAT,
		seed: [SEED_NUM, SEED_NUM + 7, SEED_NUM + 13, SEED_NUM + 29],
		p1: { name: 'Red', team: team1 },
		p2: { name: 'Blue', team: team2 },
	});

	const roster = battle.sides.map(side => side.pokemon.map(p => p.set));
	const draws = [];
	instrumentPrng(battle, draws);
	const rng = { p1: makeRng(SEED_NUM * 2 + 1), p2: makeRng(SEED_NUM * 2 + 2) };

	const decisions = [];
	let guard = 0;
	while (!battle.ended && guard++ < MAX_DECISIONS) {
		const requestState = battle.requestState;
		const requests = {};
		const choices = {};
		for (const side of [battle.p1, battle.p2]) {
			const req = side.activeRequest;
			if (!req || req.wait) continue;
			requests[side.id] = JSON.parse(JSON.stringify(req));
			const c = chooseFor(req, rng[side.id]);
			if (c) {
				// Switch choices: also resolve to the stable roster index (forme-proof identity).
				const m = c.choice.match(/^switch (\d+)$/);
				if (m) {
					const target = side.pokemon[Number(m[1]) - 1];
					c.resolved.rosterIndex = roster[side.n].indexOf(target.set);
				}
				choices[side.id] = c;
			}
		}
		if (!Object.keys(choices).length) {
			throw new Error(`no side can act at decision ${decisions.length} (requestState=${requestState})`);
		}
		// Submitting the final needed choice runs the battle forward synchronously.
		for (const [sideId, c] of Object.entries(choices)) {
			const ok = battle.choose(sideId, c.choice);
			if (!ok) {
				throw new Error(`PS rejected choice '${c.choice}' for ${sideId} at decision ${decisions.length}: ${battle.inputLog.slice(-3).join(' | ')}`);
			}
		}
		decisions.push({
			index: decisions.length,
			turn: battle.turn,
			requestState,
			midTurn: !!battle.midTurn,
			requests,
			choices,
			draws: draws.splice(0, draws.length),
			stateAfter: snapshot(battle, roster),
		});
	}

	const trace = {
		version: 2,
		psCommit: PS_COMMIT,
		format: FORMAT,
		seed: [SEED_NUM, SEED_NUM + 7, SEED_NUM + 13, SEED_NUM + 29],
		teamset: TEAMSET,
		decisions,
		result: { winner: battle.winner ?? null, ended: battle.ended, turns: battle.turn },
	};
	fs.mkdirSync(path.dirname(OUT), { recursive: true });
	const body = JSON.stringify(trace);
	if (OUT.endsWith('.gz')) {
		const { gzipSync } = await import('node:zlib');
		fs.writeFileSync(OUT, gzipSync(body));
	} else {
		fs.writeFileSync(OUT, body);
	}
	console.log(`wrote ${OUT}: ${decisions.length} decisions, ${trace.result.turns} turns, winner=${trace.result.winner}, ps=${PS_COMMIT.slice(0, 12)}`);
}

main();
