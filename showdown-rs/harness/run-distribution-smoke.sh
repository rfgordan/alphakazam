#!/bin/bash
# Exact distribution gate over bounded decision points. These seeds deliberately cover
# deterministic status/switch turns and tractable stochastic attacks. A final sequential case
# covers a mixed distribution where only some damage rolls cause a forced-switch boundary.
# Each trace is generated from pinned Showdown and immediately compared against Rust.
set -euo pipefail
cd "$(dirname "$0")/.."

OUT=${TMPDIR:-/tmp}/deep-showdown-dist-smoke
mkdir -p "$OUT"
SEEDS=(1 3 4 6 8 11 15 16 20 21 23 25 32 36 37 38 40)

run_one() {
    local seed=$1
    node harness/cosim.mjs --seed "$seed" --teamset diverse --max-decisions 2 \
        --distributions --max-dist-paths 100000 --out "$OUT/d$seed.json" >/dev/null
    target/release/cosim "$OUT/d$seed.json" >/dev/null
    echo "distribution seed $seed matched"
}

export OUT
export -f run_one
# Four workers use the machine without reproducing the high-memory parallel load that crashed
# earlier development runs. Every individual oracle is capped independently.
printf '%s\n' "${SEEDS[@]}" | xargs -n1 -P4 bash -c 'run_one "$0"'

# Keep this 112MB randbattle oracle out of the parallel group: it verifies exact request-boundary
# mass without assigning any policy over the legal replacement choices.
node harness/cosim.mjs --seed 90 --format gen9randombattle --max-decisions 2 \
    --distributions --max-dist-paths 100000 --out "$OUT/request-boundary-90.json" >/dev/null
target/release/cosim "$OUT/request-boundary-90.json" >/dev/null
echo "distribution seed 90 forced-switch boundary matched"

TOTAL=$((${#SEEDS[@]} + 1))
echo "distribution smoke: $TOTAL/$TOTAL matched"
