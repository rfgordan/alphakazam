// PsPrng raw gate: dump the real PS PRNG (sim/prng.ts at the pin) output for several seeds
// and every call pattern the battle engine uses, so a Rust test can assert bit-identical
// output. The battle PRNG is the Gen-5 LCG (see psprng.rs header for why the array-seed the
// recorder uses lands on that branch).
//
// Emits a dep-free, pipe-delimited fixture (one call per line). To verify 100k draws per
// pattern without a 60MB fixture, each line carries the draw count, an FNV-1a-64 checksum
// over the FULL output stream, and the first 16 literal outputs (so a mismatch is debuggable):
//   call | s0,s1,s2,s3 | k=v,k=v | count | fnvhex | out0,..,out15
// The Rust test recomputes the identical FNV-1a over its own 100k draws and compares the hash
// AND the 16 literals — bit-identical over the whole stream.
// Usage: node harness/psprng-gate.mjs > crates/engine/tests/psprng_gate.txt

// FNV-1a 64-bit over a sequence of unsigned integers, each folded as 8 little-endian bytes.
// Uses BigInt so it is exact and matches the Rust u64 wrapping arithmetic.
const FNV_OFFSET = 0xcbf29ce484222325n;
const FNV_PRIME = 0x100000001b3n;
const MASK64 = 0xffffffffffffffffn;
function fnvStart() { return FNV_OFFSET; }
function fnvPush(h, value) {
	let v = BigInt(value) & MASK64;
	for (let i = 0; i < 8; i++) {
		const b = v & 0xffn;
		h = ((h ^ b) * FNV_PRIME) & MASK64;
		v >>= 8n;
	}
	return h;
}

import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';
import { assertPsPinned, PS_DIR } from './check-ps-pin.mjs';

assertPsPinned();
const __dirname = path.dirname(fileURLToPath(import.meta.url));
const require = createRequire(import.meta.url);
const { PRNG } = require(path.join(PS_DIR, 'dist/sim/prng.js'));

// Seeds: the recorder's shape [n, n+7, n+13, n+29] for a few n, plus edge/limb-stress seeds.
const SEEDS = [
	[1, 8, 14, 30],
	[2, 9, 15, 31],
	[7, 14, 20, 36],
	[123, 456, 789, 1011],
	[0, 0, 0, 1],
	[0xffff, 0xffff, 0xffff, 0xffff],
	[0x1234, 0x5678, 0x9abc, 0xdef0],
	[65535, 0, 32768, 1],
];

const N = 100000; // draws per (seed, pattern)
const PREVIEW = 16;
const lines = [];
let totalDraws = 0;

// `gen(prng)` yields one output at a time.
function emitStream(call, seed, params, count, gen) {
	const p = new PRNG(seed.join(','));
	let h = fnvStart();
	const preview = [];
	for (let i = 0; i < count; i++) {
		const v = gen(p);
		h = fnvPush(h, v);
		if (i < PREVIEW) preview.push(v);
	}
	totalDraws += count;
	lines.push(`${call} | ${seed.join(',')} | ${params} | ${count} | ${h.toString(16)} | ${preview.join(',')}`);
}

// Shuffle mutates in place; emit the resulting permutation (its FNV + preview).
function emitShuffle(seed, len) {
	const p = new PRNG(seed.join(','));
	const items = Array.from({ length: len }, (_, i) => i);
	p.shuffle(items);
	let h = fnvStart();
	for (const v of items) h = fnvPush(h, v);
	totalDraws += Math.max(0, len - 1);
	lines.push(`shuffle | ${seed.join(',')} | len=${len} | ${len} | ${h.toString(16)} | ${items.slice(0, PREVIEW).join(',')}`);
}

for (const seed of SEEDS) {
	emitStream('next', seed, '', N, p => p.rng.next());
	// random(n): damage roll 16, d100, d2..d10, big n.
	for (const n of [2, 3, 4, 5, 6, 8, 10, 16, 24, 100, 256, 1000]) {
		emitStream('random_n', seed, `n=${n}`, N, p => p.random(n));
	}
	// random(m, n).
	for (const [m, n] of [[2, 4], [1, 5], [3, 7], [5, 20], [0, 12], [10, 100]]) {
		emitStream('random_range', seed, `m=${m},n=${n}`, N, p => p.random(m, n));
	}
	// randomChance(num, den) — the accuracy/secondary/status coin.
	for (const [num, den] of [[1, 5], [1, 2], [3, 10], [1, 3], [30, 100], [100, 100], [85, 100], [1, 24]]) {
		emitStream('random_chance', seed, `num=${num},den=${den}`, N, p => (p.randomChance(num, den) ? 1 : 0));
	}
	// sample(items) — index into an array.
	for (const len of [2, 3, 4, 6, 12]) {
		const items = Array.from({ length: len }, (_, i) => i);
		emitStream('sample', seed, `len=${len}`, N, p => p.sample(items));
	}
	// shuffle(items) — full-array shuffle; out is the resulting permutation.
	for (const len of [2, 3, 6, 10, 12, 24]) emitShuffle(seed, len);
}

process.stdout.write(lines.join('\n') + '\n');
process.stderr.write(`psprng-gate: ${lines.length} fixture lines, ${SEEDS.length} seeds, ${totalDraws} draws verified\n`);
