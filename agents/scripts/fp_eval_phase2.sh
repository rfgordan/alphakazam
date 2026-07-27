#!/bin/bash
# Phase 2: certify the engine gates on Codex's final committed state, then finish the P0.1
# foul-play calibration (fast-arm top-up + full arm). Sequenced so mutation-suite rebuilds
# don't steal CPU from foul-play's wall-clock search budget.
# Run detached: nohup caffeinate -i scripts/fp_eval_phase2.sh > runs/fp_eval/phase2.out 2>&1 &
set -u
cd "$(dirname "$0")/.."
ROOT=$(cd .. && pwd)
GL=runs/fp_eval/gates
mkdir -p "$GL"

echo "=== GATES start: $(date) ==="
(
  . "$HOME/.cargo/env"
  cd "$ROOT/showdown-rs"
  cargo test --release -p engine  > "$OLDPWD/$GL/tests_engine.log" 2>&1 && echo "engine tests OK"
  cargo test --release -p cosim   > "$OLDPWD/$GL/tests_cosim.log"  2>&1 && echo "cosim tests OK"
  cargo build --release -p cosim >/dev/null 2>&1
  target/release/cosim harness/cosim-traces/*.json.gz > "$OLDPWD/$GL/sweep.log" 2>/dev/null \
    && echo "sampled sweep OK" || echo "sampled sweep FAILED"
  bash harness/run-distribution-smoke.sh > "$OLDPWD/$GL/smoke.log" 2>&1 \
    && echo "distribution smoke OK" || echo "distribution smoke FAILED"
  bash harness/run-mutations.sh > "$OLDPWD/$GL/mutations.log" 2>&1 \
    && echo "mutation suite done" || echo "mutation suite FAILED"
  # run-mutations leaves the last mutant's BINARY compiled; force a clean rebuild
  touch crates/engine/src/generate.rs
  cargo build --release -p cosim >/dev/null 2>&1
)
echo "=== GATES done: $(date) ==="

echo "=== PS server ==="
if ! curl -s -m 3 http://localhost:8000 >/dev/null; then
  (cd "$ROOT/engines/pokemon-showdown" && nohup node pokemon-showdown start --no-security > /tmp/ps-server.log 2>&1 &)
  for i in $(seq 1 30); do curl -s -m 2 http://localhost:8000 >/dev/null && break; sleep 2; done
fi
curl -s -m 3 http://localhost:8000 >/dev/null && echo "PS server UP" || { echo "PS SERVER FAILED"; exit 1; }

echo "=== FAST ARM top-up (100ms) start: $(date) ==="
scripts/fp_eval.sh F 5 10 100 > runs/fp_eval/pairF.out 2>&1 &
scripts/fp_eval.sh G 5 10 100 > runs/fp_eval/pairG.out 2>&1 &
scripts/fp_eval.sh H 5 10 100 > runs/fp_eval/pairH.out 2>&1 &
wait
echo "=== FAST ARM top-up done: $(date) ==="

echo "=== FULL ARM (2000ms) start: $(date) ==="
scripts/fp_eval.sh D 5 15 2000 > runs/fp_eval/pairD.out 2>&1 &
scripts/fp_eval.sh E 5 15 2000 > runs/fp_eval/pairE.out 2>&1 &
wait
echo "=== FULL ARM done: $(date) ==="

W=$(grep -h "Winner: N4E" runs/fp_eval/*_ms100/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
L=$(grep -h "Winner: FPE" runs/fp_eval/*_ms100/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
echo "ARM ms100 FINAL (incl. phase-1 A/B/C): model $W, foul-play $L"
W2=$(grep -h "Winner: N4E" runs/fp_eval/*_ms2000/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
L2=$(grep -h "Winner: FPE" runs/fp_eval/*_ms2000/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
echo "ARM ms2000 FINAL: model $W2, foul-play $L2"
echo "ALL DONE: $(date)"
