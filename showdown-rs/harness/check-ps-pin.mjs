// Verify the Pokémon Showdown clone matches the pinned commit in ps.lock.
//
// Every harness entry point imports `assertPsPinned()` before touching the sim, so
// verification numbers can never silently come from an unpinned ground truth.

import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const HERE = path.dirname(fileURLToPath(import.meta.url));
export const PS_DIR = path.resolve(HERE, '../../engines/pokemon-showdown');
const LOCK = path.resolve(HERE, '../ps.lock');

export function pinnedCommit() {
	const line = readFileSync(LOCK, 'utf8').split('\n').find(l => l.startsWith('commit='));
	if (!line) throw new Error(`ps.lock has no commit= line (${LOCK})`);
	return line.slice('commit='.length).trim();
}

export function assertPsPinned() {
	const want = pinnedCommit();
	let got;
	try {
		got = execFileSync('git', ['-C', PS_DIR, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
	} catch (e) {
		throw new Error(`cannot read PS clone at ${PS_DIR}: ${e.message}\n` +
			`clone it with: git clone https://github.com/smogon/pokemon-showdown "${PS_DIR}" && ` +
			`git -C "${PS_DIR}" checkout ${want} && (cd "${PS_DIR}" && node build)`);
	}
	if (got !== want) {
		throw new Error(
			`PS clone is at ${got.slice(0, 12)} but ps.lock pins ${want.slice(0, 12)}.\n` +
			`Either: git -C "${PS_DIR}" checkout ${want} && (cd "${PS_DIR}" && node build)\n` +
			`Or (deliberate upgrade): update ps.lock and regenerate all corpora.`
		);
	}
	return want;
}

// Runnable directly: node harness/check-ps-pin.mjs
if (process.argv[1] === fileURLToPath(import.meta.url)) {
	console.log(`PS pinned OK: ${assertPsPinned()}`);
}
