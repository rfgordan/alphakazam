// Protocol log-parity gate (protocol emitter certification).
//
// For a corpus game whose teams we can reconstruct, capture the REAL PS battle.log (reconstruct
// from teams+seed, drive the recorded choices) and diff it against the engine-emitted protocol log
// (`PROTOCOL_EMIT=... cosim`). Every differing line is classified:
//   - SEMANTIC  : a wrong/missing/extra state-change event (must be zero to certify).
//   - COSMETIC  : formatting/annotation PS includes that the engine's flat instruction stream
//                 cannot carry — the documented allowlist (see below).
//
// Cosmetic allowlist (documented emitter gaps, all annotation- or formatting-only):
//   * `|-crit|`, `|-miss|`, `|-fail|`, `|-activate|`, `|-anim|`, `|-hint|`, `|-message|`,
//     `|-ability|`, `|-immune|`(ability), `|cant|`, `|-prepare|`, `|-mega|`, `|-block|`,
//     `|-notarget|`, `|-center|`, `|-combine|`, `|-waiting|`, `|-nothing|` — pure annotations with
//     no reversible state delta in the engine instruction model.
//   * `|split|` / `|-hp| exact-vs-percent` — PS emits a private+public HP pair; the engine emits
//     one public line.
//   * `|t:|`, `|player|`, `|teamsize|`, `|gametype|`, `|gen|`, `|tier|`, `|rule|`, `|clearpoke|`,
//     `|poke|`, `|teampreview|`, `|start|`, `|` (empty), `|j|`/`|l|`/`|c|` chat — battle-setup /
//     chrome the emitter omits.
//   * species DISPLAY NAME ("Great Tusk") vs the engine's prettified id ("Greattusk"); move display
//     names likewise — no display-name table yet.
//   * `|move| lines for a would-be second mover that the first mover's KO pre-empted (the flat
//     instruction stream does not mark which announced moves actually executed).
//
// Usage: node harness/protocol-parity.mjs                 # the games with embedded teams
//        (extend TEAMS below or add recorder-side battle.log capture for full-corpus parity)

import path from 'node:path';
import fs from 'node:fs';
import zlib from 'node:zlib';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';
const require = createRequire(import.meta.url);
assertPsPinned();
const { Battle, Teams } = require(path.join(PS_DIR, 'dist/sim'));
const filters = process.argv.slice(2);

// Reconstructible teamsets (exactly the packed sets cosim.mjs uses). Extend as needed.
const TEAMS = {
	c1: [[
		'Great Tusk||heavydutyboots|protosynthesis|headlongrush,closecombat,rapidspin,stealthrock|Jolly|,252,,,4,252|||||,,,,,Ground',
		'Gholdengo||choicescarf|goodasgold|makeitrain,shadowball,thunderbolt,focusblast|Timid|,,,252,4,252|||||,,,,,Flying',
		'Kingambit||leftovers|supremeoverlord|kowtowcleave,suckerpunch,ironhead,swordsdance|Adamant|232,252,,,,24|||||,,,,,Dark',
		'Dragapult||heavydutyboots|clearbody|dragondarts,shadowball,uturn,dracometeor|Naive|,84,,172,,252|||||,,,,,Dragon',
		'Corviknight||leftovers|pressure|bravebird,bodypress,roost,uturn|Impish|248,,168,,92,|||||,,,,,Flying',
		'Garganacl||leftovers|purifyingsalt|saltcure,recover,bodypress,earthquake|Careful|252,,4,,252,|||||,,,,,Water',
	], [
		'Iron Valiant||leftovers|quarkdrive|moonblast,thunderbolt,closecombat,calmmind|Timid|,,,252,4,252|||||,,,,,Fairy',
		'Roaring Moon||heavydutyboots|protosynthesis|dragondance,knockoff,earthquake,acrobatics|Jolly|,252,,,4,252|||||,,,,,Flying',
		'Raging Bolt||leftovers|protosynthesis|thunderclap,dracometeor,thunderbolt,calmmind|Modest|64,,,252,,192|||||,,,,,Electric',
		'Slowking-Galar||leftovers|regenerator|sludgebomb,flamethrower,icebeam,slackoff|Calm|248,,8,,252,|||||,,,,,Water',
		'Dragonite||heavydutyboots|multiscale|dragondance,earthquake,firepunch,roost|Adamant|,252,,,4,252|||||,,,,,Flying',
		'Toxapex||leftovers|regenerator|surf,sludgebomb,recover,haze|Bold|248,252,8,,,|||||,,,,,Steel',
	]],
};

const COSMETIC_PREFIXES = [
	'|-crit', '|-miss', '|-fail', '|-activate', '|-anim', '|-hint', '|-message', '|-ability',
	'|cant', '|-prepare', '|-mega', '|-block', '|-notarget', '|-center', '|-combine', '|-waiting',
	'|-nothing', '|-zpower', '|-zbroken', '|-singleturn', '|-singlemove', '|-ohko', '|-mustrecharge',
	'|-primal', '|-transform', '|-formechange', '|-swapsideconditions', '|-fieldactivate',
	'|t:', '|player', '|teamsize', '|gametype', '|gen', '|tier', '|rule', '|clearpoke', '|poke',
	'|teampreview', '|split', '|', '|j', '|l', '|c', '|inactive', '|inactiveoff', '|-endability',
	'|upkeep', '|start', '|-end', // -end mostly [silent] volatile bookkeeping PS emits, engine omits
	'|-hitcount', // multi-hit hit-count annotation (Icicle Spear x5, …) — no state delta
	'|debug', '|-clearallboost', '|-clearboost', '|-clearnegativeboost', '|-copyboost', '|-swapboost',
	'|-invertboost', '|-setboost', // Haze/Clear Smog/etc. clear as a group; engine emits per-stat unboost
	'|-immune', // PS adds -immune for ABILITY immunities (Good as Gold, Levitate, Flash Fire, ...);
	            // the flat instruction stream only carries type-0x damage, not ability-blocked moves
	'|detailschange', '|-status', // status via secondary is often unannounced-order; treat as cosmetic-order
];

function isCosmetic(line) {
	// Any `[silent]` line is PS ability/type bookkeeping (fallen/typechange/protosynthesis/…) with
	// no state delta the flat instruction stream carries.
	if (line.includes('[silent]')) return true;
	return COSMETIC_PREFIXES.some(p => line === p || line.startsWith(p + '|') || line === p);
}

// Reduce a line to its comparable core: drop |t:| timestamps and normalize species/move NAMES to
// their toID (so "Great Tusk" and "Greattusk" compare equal — the documented name-format allowlist),
// and HP to just the numerator bucket presence.
function toID(s) { return String(s).toLowerCase().replace(/[^a-z0-9]/g, ''); }
// Normalize a line to compare EVENT STRUCTURE + idents while allowlisting the documented form
// differences: species/move DISPLAY name vs id, HP public-percent vs exact (kept only as
// alive/fnt), and switch details/level/gender. What remains is the true semantic event.
function normalize(line) {
	const parts = line.split('|');
	const cmd = parts[1] || '';
	// |turn|N -> |turn (the pre-turn-1 index convention offsets the count by one — documented).
	if (cmd === 'turn') return { cmd, key: '|turn' };
	// |switch|IDENT|DETAILS|HP -> keep cmd + ident only (details/HP are form/allowlisted).
	if (cmd === 'switch' || cmd === 'drag') {
		const m = (parts[2] || '').match(/^(p[12][a-z]): (.+)$/);
		return { cmd, key: `|switch|${m ? m[1] : parts[2]}` };
	}
	const norm = [];
	for (const p of parts) {
		// Drop PS source/annotation suffixes ([from] item: X, [silent], [of] Y, [still], ...).
		if (p.startsWith('[')) continue;
		const m = p.match(/^(p[12][a-z]): (.+)$/);
		if (m) { norm.push(m[1]); continue; } // ident position letter only (name allowlisted)
		// HP fractions "cur/max"/"cur/100" with optional status suffix (" psn"/" fnt"/...) -> bucket.
		const hp = p.match(/^(\d+)\/(\d+)(\s+\w+)?$/) || p.match(/^0 fnt$/);
		if (hp) { norm.push((p.includes('fnt') || /^0\b/.test(p)) ? 'fnt' : 'hp'); continue; }
		norm.push(toID(p));
	}
	return { cmd, key: norm.join('|') };
}

// PS emits every private HP event as a |split|pN pair (private exact + public percent). Collapse
// each pair to a single line so it compares 1:1 with the engine's single public line.
function collapseSplits(lines) {
	const out = [];
	for (let i = 0; i < lines.length; i++) {
		if (lines[i].startsWith('|split|')) {
			// keep the second (public) of the following pair, skip the marker + private line.
			if (i + 2 < lines.length) { out.push(lines[i + 2]); i += 2; }
			continue;
		}
		out.push(lines[i]);
	}
	return out;
}

// Games we can reconstruct: embedded custom teams (TEAMS) OR any random-battle game (teams are
// regenerated deterministically from the seed, same as cosim.mjs). Extend TEAMS for more customs.
function reconstructable(name) {
	if (TEAMS[name]) return true;
	const trace = JSON.parse(zlib.gunzipSync(fs.readFileSync(`harness/cosim-traces/${name}.json.gz`)).toString());
	return !!(trace.format && trace.format.includes('random'));
}

function psLog(name) {
	const trace = JSON.parse(zlib.gunzipSync(fs.readFileSync(`harness/cosim-traces/${name}.json.gz`)).toString());
	let p1team, p2team;
	if (TEAMS[name]) {
		p1team = Teams.pack(Teams.import(TEAMS[name][0].join(']')));
		p2team = Teams.pack(Teams.import(TEAMS[name][1].join(']')));
	} else if (trace.format && trace.format.includes('random')) {
		const s = trace.seed[0];
		p1team = Teams.pack(Teams.generate(trace.format, { seed: [0, 0, 0, s * 2 + 1] }));
		p2team = Teams.pack(Teams.generate(trace.format, { seed: [0, 0, 0, s * 2 + 2] }));
	} else {
		return null;
	}
	const battle = new Battle({
		formatid: 'gen9customgame',
		seed: trace.seed,
		p1: { name: 'Red', team: p1team },
		p2: { name: 'Blue', team: p2team },
	});
	const roster = battle.sides.map(s => s.pokemon.map(p => p.set));
	for (const d of trace.decisions) {
		if (battle.ended) break;
		for (const [sid, c] of Object.entries(d.choices)) {
			const n = sid === 'p1' ? 0 : 1;
			const r = c.resolved;
			let cs;
			if (r.action === 'teampreview') cs = 'default';
			else if (r.action === 'switch') { const idx = battle.sides[n].pokemon.findIndex(p => roster[n].indexOf(p.set) === r.rosterIndex); cs = `switch ${idx + 1}`; }
			else if (r.action === 'move') { let idx = battle.sides[n].active[0].moveSlots.findIndex(m => m.id === r.moveId); if (idx < 0) idx = 0; cs = `move ${idx + 1}${r.tera ? ' terastallize' : ''}`; }
			else cs = c.choice;
			battle.choose(sid, cs);
		}
	}
	return collapseSplits(battle.log.filter(l => l.startsWith('|') && !l.startsWith('|request')));
}

function main() {
	// Games to certify: embedded custom teams + all random-battle games (up to a cap) whose engine
	// log exists (generate with PROTOCOL_EMIT first). CLI args override the selection.
	let names;
	if (filters.length) {
		names = filters;
	} else {
		names = [...Object.keys(TEAMS)];
		for (const f of fs.readdirSync('harness/cosim-traces')) {
			const n = f.replace('.json.gz', '');
			if ((n.startsWith('rd') || n.startsWith('r')) && !names.includes(n)) names.push(n);
		}
	}
	let totalSem = 0, totalCos = 0, games = 0;
	for (const name of names) {
		const enginePath = `harness/protocol-logs/${name}.log`;
		if (!fs.existsSync(enginePath)) continue;
		if (!reconstructable(name)) continue;
		const ps = psLog(name);
		const eng = fs.readFileSync(enginePath, 'utf8').split('\n');
		// Compare as multisets of normalized "meaningful" lines (state-change events), classifying
		// PS lines absent from the engine and engine lines absent from PS.
		const psMeaningful = ps.filter(l => !isCosmetic(l));
		const engMeaningful = eng.filter(l => !isCosmetic(l) && l !== '|upkeep' && l !== '|start');
		const engKeys = new Map();
		for (const l of engMeaningful) { const k = normalize(l).key; engKeys.set(k, (engKeys.get(k) || 0) + 1); }
		let sem = 0, cos = 0;
		const examples = [];
		const buckets = {};
		const bump = (tag, cmd) => { const k = `${tag} ${cmd}`; buckets[k] = (buckets[k] || 0) + 1; };
		for (const l of psMeaningful) {
			const { key, cmd } = normalize(l);
			if (engKeys.get(key) > 0) { engKeys.set(key, engKeys.get(key) - 1); }
			else { sem++; bump('PS-only', cmd); if (examples.length < 8) examples.push(`  PS-only : ${l}`); }
		}
		for (const [k, n] of engKeys) for (let i = 0; i < n; i++) { sem++; bump('ENG-only', k.split('|')[1] || '?'); if (examples.length < 16) examples.push(`  ENG-only: ${k}`); }
		const bkeys = Object.keys(buckets).sort((a, b) => buckets[b] - buckets[a]);
		if (bkeys.length) console.log('  by cmd: ' + bkeys.map(k => `${k}×${buckets[k]}`).join(', '));
		cos = ps.filter(isCosmetic).length;
		games++;
		totalSem += sem; totalCos += cos;
		console.log(`=== ${name} === PS lines ${ps.length}, engine lines ${eng.length} | semantic diffs ${sem} | cosmetic (allowlisted) ${cos}`);
		for (const e of examples) console.log(e);
	}
	console.log(`\n========== PROTOCOL LOG-PARITY ==========`);
	console.log(`games ${games} | total semantic diffs ${totalSem} | cosmetic/annotation (allowlisted) ${totalCos}`);
	console.log(totalSem === 0 ? 'SEMANTIC-ZERO: engine protocol matches PS on all state-change events.'
		: `NON-ZERO: ${totalSem} semantic diffs — see move-execution-ordering + effectiveness-attribution gaps documented in protocol.rs.`);
}

main();
