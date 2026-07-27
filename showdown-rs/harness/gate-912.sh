#!/usr/bin/env bash
# The 912-game seed gate: the campaign's single regression rail.
#
#   111  audited traces      harness/cosim-traces/*.json.gz           (ABSOLUTE INVARIANT: 111/111)
#   401  pinned fixtures     harness/seed-fixtures/*.fx.json.gz       (seeds 1000-1400)
#   400  fresh fixtures      harness/seed-fixtures-fresh/*.fx.json.gz (seeds 1401-1800)
#
# Prints the per-corpus exact counts and writes the NON-EXACT game set to a file, so the
# regression judgment is made on the SET, never on the count:
#
#   bash harness/gate-912.sh /tmp/before.txt      # at the parent commit
#   ...make the fix...
#   bash harness/gate-912.sh /tmp/after.txt
#   comm -13 /tmp/before.txt /tmp/after.txt       # newly-non-exact — MUST BE EMPTY
#   comm -23 /tmp/before.txt /tmp/after.txt       # newly exact — the yield
#
# Usage: bash harness/gate-912.sh [out-nonexact-set-file]
set -euo pipefail
cd "$(dirname "$0")/.."

COSIM=target/release/cosim
[ -x "$COSIM" ] || { echo "build first: cargo build --release -p cosim -j 2" >&2; exit 1; }

OUT="${1:-}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT
: > "$TMP/set"

run() { # <label> <files...>
  local label="$1"; shift
  # VERBOSE=1 is REQUIRED: without it the per-game divergence listing is truncated at 45 rows
  # (seedgate.rs:973) and the set is silently short.
  SEED_GATE=1 VERBOSE=1 "$COSIM" "$@" > "$TMP/$label.log" 2>&1 || true
  printf '%-8s %s\n' "$label" "$(grep -m1 'FULL-GAME EXACT' "$TMP/$label.log")"
  # the gate lists ONLY non-exact games under "per-game first divergence"
  awk '/^per-game first divergence/{f=1;next} /^first-divergence category/{f=0} f && NF {print $1}' \
    "$TMP/$label.log" | sed 's/\.fx\.json\.gz$//; s/\.json\.gz$//' >> "$TMP/set"
}

run audited harness/cosim-traces/*.json.gz
run pinned  harness/seed-fixtures/*.fx.json.gz
run fresh   harness/seed-fixtures-fresh/*.fx.json.gz

sort -u "$TMP/set" -o "$TMP/set"
TOT=912
BAD=$(wc -l < "$TMP/set" | tr -d ' ')
echo "----"
echo "COMBINED: $(( TOT - BAD )) / $TOT exact ; non-exact $BAD"

if [ -n "$OUT" ]; then cp "$TMP/set" "$OUT"; echo "non-exact set -> $OUT"; fi
