#!/usr/bin/env bash
# The REAL-FORMAT rail: seed gate over the `[Gen 9] Random Battle` corpus.
#
#   101  randbats fixtures  harness/seed-fixtures-rb/*.fx.json.gz        (seeds 5001-5100 + 5139)
#   399  fresh   fixtures   harness/seed-fixtures-rb-fresh/*.fx.json.gz  (seeds 5101-5500 minus 5139)
#  1000  1000    fixtures   harness/seed-fixtures-rb-1000/*.fx.json.gz   (seeds 5501-6500)
#   = 1500 games total. The total is COUNTED from the globs, not hardcoded — add a dir, add a
#   `run` line, and the arithmetic follows.
#
# These are the ONLY committed recordings actually PLAYED under gen9randombattle — Sleep Clause
# Mod live, `Dex#trunc` (13-bit Speed / 16-bit damage), no team preview, percent HP. Every other
# corpus (`gate-912.sh`) is customgame however its `format` field is stamped; the two are not
# interchangeable and a fix must be judged on BOTH.
#
# Same set-not-count discipline as gate-912.sh:
#   bash harness/gate-rb.sh /tmp/rb-before.txt
#   ...fix...
#   bash harness/gate-rb.sh /tmp/rb-after.txt
#   comm -13 /tmp/rb-before.txt /tmp/rb-after.txt   # newly-non-exact — MUST BE EMPTY
#   comm -23 /tmp/rb-before.txt /tmp/rb-after.txt   # newly exact — the yield
#
# Usage: bash harness/gate-rb.sh [out-nonexact-set-file]
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
  # VERBOSE=1 is REQUIRED (seedgate.rs truncates the per-game listing at 45 rows without it).
  SEED_GATE=1 VERBOSE=1 "$COSIM" "$@" > "$TMP/$label.log" 2>&1 || true
  printf '%-8s %s\n' "$label" "$(grep -m1 'FULL-GAME EXACT' "$TMP/$label.log")"
  printf '%-8s %s\n' "" "$(grep -m1 'init-aligned' "$TMP/$label.log")"
  awk '/^per-game first divergence/{f=1;next} /^first-divergence category/{f=0} f && NF {print $1}' \
    "$TMP/$label.log" | sed 's/\.fx\.json\.gz$//' >> "$TMP/set"
}

TOT=0
count() { TOT=$(( TOT + $# )); }
count harness/seed-fixtures-rb/*.fx.json.gz
count harness/seed-fixtures-rb-fresh/*.fx.json.gz
count harness/seed-fixtures-rb-1000/*.fx.json.gz

run rb100 harness/seed-fixtures-rb/*.fx.json.gz
run rbfresh harness/seed-fixtures-rb-fresh/*.fx.json.gz
run rb1000 harness/seed-fixtures-rb-1000/*.fx.json.gz

sort -u "$TMP/set" -o "$TMP/set"
BAD=$(wc -l < "$TMP/set" | tr -d ' ')
echo "----"
echo "COMBINED-RB: $(( TOT - BAD )) / $TOT exact ; non-exact $BAD"
sed -n '/^first-divergence category/,/^exact games/p' "$TMP/rb1000.log" | head -24

if [ -n "$OUT" ]; then cp "$TMP/set" "$OUT"; echo "non-exact set -> $OUT"; fi
