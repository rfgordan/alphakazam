// Sample the *pinned* Pokémon Showdown gen9randombattle team generator into a reproducible pool.
//
// We do NOT reimplement PS's team generator — we drive it. `Teams.generate('gen9randombattle',
// {seed})` returns the exact distribution PS ladders on, so the pool is distribution-exact by
// construction and later doubles as an exact belief-prior label source.
//
// Each team i is generated with a deterministic seed derived from i, so the whole pool is
// reproducible: regenerate with the same N and you get byte-identical output.
//
// Output: gzipped JSONL, one team per line, each member serialized in the engine's
// MemberSpec-compatible `toID` form (see crates/pybridge team_from_pool_line). A stats sidecar
// (species frequency + per-species move marginals) is emitted for belief priors.
//
// Usage:
//   node gen-team-pool.mjs [N] [outfile.jsonl.gz]
//   node gen-team-pool.mjs 100000 team-pool/gen9randombattle-100k.jsonl.gz
//   node gen-team-pool.mjs 2000   team-pool/gen9randombattle-2k.jsonl.gz
//
// Run from showdown-rs/harness (the PS clone is resolved relative to this file).

import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { createGzip } from "node:zlib";
import { createWriteStream, mkdirSync, writeFileSync } from "node:fs";
import { dirname, resolve, join } from "node:path";

const __dirname = dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);

// The pinned PS clone lives at the worktree root under engines/pokemon-showdown.
const PS = resolve(__dirname, "../../engines/pokemon-showdown");
const { Teams, toID } = require(join(PS, "dist/sim/index.js"));

const FORMAT = "gen9randombattle";
const N = parseInt(process.argv[2] ?? "100000", 10);
const OUT = process.argv[3] ?? "team-pool/gen9randombattle-100k.jsonl.gz";
const STAT_STRIDE = ["hp", "atk", "def", "spa", "spd", "spe"];

// Deterministic per-team seed: PS's gen5-style PRNG takes four 16-bit words. Fold i across the
// words (plus two fixed salts to avoid degenerate all-zero seeds) so seed(i) is stable and
// distinct per team.
function seedFor(i) {
  return [(i >>> 16) & 0xffff, i & 0xffff, 0xa5a5, 0x1234];
}

// A PS random-battle set → engine MemberSpec-compatible record (all ids in toID form).
// - species: `speciesId` is already the toID (formes included, e.g. urshifurapidstrike).
// - moves: PS emits these already lowercased/toID.
// - ability/item/teraType: display strings → toID.
// - item may be empty (intentionally itemless mons, e.g. Acrobatics Jumpluff) → "" (Item::None).
// - nature: gen9 randombattle never sets one → neutral "serious".
// - evs/ivs: normalized to [hp,atk,def,spa,spd,spe] arrays (engine StatIndex order).
function toRecord(set) {
  const evs = STAT_STRIDE.map((k) => set.evs[k]);
  const ivs = STAT_STRIDE.map((k) => set.ivs[k]);
  return {
    species: set.speciesId || toID(set.species),
    level: set.level,
    ability: toID(set.ability),
    item: toID(set.item), // "" when the set holds no item
    nature: set.nature ? toID(set.nature) : "serious",
    tera: toID(set.teraType),
    gender: set.gender || "N",
    evs,
    ivs,
    moves: set.moves.map((m) => toID(m)),
  };
}

const outPath = resolve(__dirname, OUT);
mkdirSync(dirname(outPath), { recursive: true });

const gz = createGzip({ level: 9 });
const sink = createWriteStream(outPath);
gz.pipe(sink);

// Stats sidecar: species frequency + per-species move marginals (P(move | species)).
const speciesCount = new Map();
const moveBySpecies = new Map(); // species -> Map(move -> count)
let monTotal = 0;

function tally(rec) {
  monTotal++;
  speciesCount.set(rec.species, (speciesCount.get(rec.species) ?? 0) + 1);
  let mv = moveBySpecies.get(rec.species);
  if (!mv) {
    mv = new Map();
    moveBySpecies.set(rec.species, mv);
  }
  for (const m of rec.moves) mv.set(m, (mv.get(m) ?? 0) + 1);
}

async function writeLine(line) {
  if (!gz.write(line)) {
    await new Promise((r) => gz.once("drain", r));
  }
}

console.error(`Generating ${N} ${FORMAT} teams -> ${outPath}`);
const t0 = Date.now();
for (let i = 0; i < N; i++) {
  const team = Teams.generate(FORMAT, { seed: seedFor(i) });
  const rec = { team: team.map(toRecord) };
  for (const m of rec.team) tally(m);
  await writeLine(JSON.stringify(rec) + "\n");
  if ((i + 1) % 10000 === 0) {
    console.error(`  ${i + 1}/${N} (${((Date.now() - t0) / 1000).toFixed(1)}s)`);
  }
}

await new Promise((r) => gz.end(r));
await new Promise((r) => sink.once("close", r));

// Emit the stats sidecar next to the pool file.
const species = [...speciesCount.entries()]
  .sort((a, b) => b[1] - a[1])
  .map(([id, count]) => ({ id, count, freq: count / monTotal }));

const moveMarginals = {};
for (const [sp, mv] of moveBySpecies) {
  const spCount = speciesCount.get(sp);
  moveMarginals[sp] = Object.fromEntries(
    [...mv.entries()].sort((a, b) => b[1] - a[1]).map(([m, c]) => [m, c / spCount])
  );
}

const statsPath = resolve(__dirname, "team-pool/stats.json");
writeFileSync(
  statsPath,
  JSON.stringify(
    {
      format: FORMAT,
      ps_pin: "b9dc987d344635789116ae46c48f8e2480e0ddc2",
      teams: N,
      mons: monTotal,
      distinct_species: species.length,
      species_frequency: species,
      move_marginals_by_species: moveMarginals,
    },
    null,
    0
  ) + "\n"
);

console.error(
  `Done: ${N} teams, ${monTotal} mons, ${species.length} distinct species in ${(
    (Date.now() - t0) /
    1000
  ).toFixed(1)}s`
);
console.error(`  pool  -> ${outPath}`);
console.error(`  stats -> ${statsPath}`);
