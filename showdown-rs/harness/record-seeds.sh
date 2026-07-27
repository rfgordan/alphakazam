#!/usr/bin/env bash
# Record a range of full games as seed-gate SIDECARS (full v2 traces).
#
# FORMAT is the REAL formatid now — it is passed to `new Battle` verbatim and stamped on the
# trace as `ruleset`. `gen9randombattle` gets Sleep Clause Mod, the 13-bit Speed truncation, no
# team preview and percent HP; `gen9customgame` gets what the 912 committed games were recorded
# with. They are NOT interchangeable corpora: a game recorded under one is not a valid witness
# for the other.
#
#   bash harness/record-seeds.sh <first-seed> <last-seed> [outdir]
#
# One node process at a time (the recorder loads the whole pinned PS dist per run and the box is
# 16 GB — do NOT parallelize this, and do NOT run it next to a heavy cargo build). RESUMABLE: a
# seed whose sidecar already exists and gunzips clean is skipped, so re-running after an
# interruption picks up where it stopped.
#
# Full games — no --max-decisions cap, no --distributions (the distribution oracle enumerates
# branch trees and is orders of magnitude slower; the seed gate does not use it).
#
# Sidecars are gitignored. Turn them into committed slim fixtures with:
#   MAKE_FIXTURE=harness/seed-fixtures target/release/cosim harness/seed-sidecars/*.json.gz
set -uo pipefail

FIRST="${1:?usage: record-seeds.sh <first-seed> <last-seed> [outdir]}"
LAST="${2:?usage: record-seeds.sh <first-seed> <last-seed> [outdir]}"
OUTDIR="${3:-harness/seed-sidecars}"
FORMAT="${FORMAT:-gen9randombattle}"

cd "$(dirname "$0")/.." || exit 1
mkdir -p "$OUTDIR"

total=$((LAST - FIRST + 1))
done_n=0; skipped=0; failed=0; i=0
start=$(date +%s)

for seed in $(seq "$FIRST" "$LAST"); do
	i=$((i + 1))
	out="$OUTDIR/rb${seed}.json.gz"
	if [ -s "$out" ] && gzip -t "$out" 2>/dev/null; then
		skipped=$((skipped + 1))
		continue
	fi
	if node harness/cosim.mjs --seed "$seed" --format "$FORMAT" --out "$out" >/dev/null 2>"$OUTDIR/.err.$seed"; then
		done_n=$((done_n + 1))
		rm -f "$OUTDIR/.err.$seed"
	else
		failed=$((failed + 1))
		echo "FAIL seed=$seed: $(tail -n 2 "$OUTDIR/.err.$seed" | tr '\n' ' ')"
		rm -f "$out"
	fi
	if [ $((i % 25)) -eq 0 ]; then
		el=$(( $(date +%s) - start ))
		echo "[$i/$total] recorded=$done_n skipped=$skipped failed=$failed  ${el}s elapsed"
	fi
done

el=$(( $(date +%s) - start ))
echo "DONE $FIRST..$LAST -> $OUTDIR: recorded=$done_n skipped=$skipped failed=$failed in ${el}s"
[ "$failed" -eq 0 ]
