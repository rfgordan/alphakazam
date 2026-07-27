#!/usr/bin/env bash
# The REAL-FORMAT rail: seed gate over the `[Gen 9] Random Battle` corpus.
#
#   101  randbats fixtures  harness/seed-fixtures-rb/*.fx.json.gz  (seeds 5001-5100 + 5139)
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
#
# Usage: bash harness/gate-rb.sh [out-nonexact-set-file]
set -euo pipefail
cd "$(dirname "$0")/.."

COSIM=target/release/cosim
[ -x "$COSIM" ] || { echo "build first: cargo build --release -p cosim -j 2" >&2; exit 1; }

OUT="${1:-}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# VERBOSE=1 is REQUIRED (seedgate.rs truncates the per-game listing at 45 rows without it).
SEED_GATE=1 VERBOSE=1 "$COSIM" harness/seed-fixtures-rb/*.fx.json.gz > "$TMP/log" 2>&1 || true
grep -m1 'FULL-GAME EXACT' "$TMP/log"
grep -m1 'init-aligned' "$TMP/log"
awk '/^per-game first divergence/{f=1;next} /^first-divergence category/{f=0} f && NF {print $1}' \
  "$TMP/log" | sed 's/\.fx\.json\.gz$//' | sort -u > "$TMP/set"
echo "non-exact: $(wc -l < "$TMP/set" | tr -d ' ')"
sed -n '/^first-divergence category/,/^exact games/p' "$TMP/log" | head -20

if [ -n "$OUT" ]; then cp "$TMP/set" "$OUT"; echo "non-exact set -> $OUT"; fi
