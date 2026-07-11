#!/bin/bash
# Exact distribution gate over bounded decision points. These seeds deliberately cover
# deterministic status/switch turns and tractable stochastic attacks without pivot/faint
# request boundaries. Each trace is generated from pinned Showdown and immediately compared
# against the Rust branch distribution.
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
echo "distribution smoke: ${#SEEDS[@]}/${#SEEDS[@]} matched"
