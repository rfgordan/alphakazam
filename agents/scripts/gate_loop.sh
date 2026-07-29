#!/usr/bin/env bash
# The goal gate, on a loop: every INTERVAL seconds, pause the hero run, evaluate the latest
# checkpoint + budget search vs mcts@100ms at matched time (300 games), append to gate.log,
# resume. Detached like the watchdog — survives the shell.
#
#   agents/scripts/gate_loop.sh runs/hero1 [interval_s] [search args...]
set -uo pipefail
cd "$(dirname "$0")/.."
RUN_DIR=${1:-runs/hero1}
INTERVAL=${2:-10800}
shift 2 2>/dev/null || shift $# 2>/dev/null || true
SEARCH_ARGS=${*:-"--depth 2 --topk 4 --samples 1 --det"}
LOG="$RUN_DIR/gate.log"
echo "[gate_loop] start $(date -Is) run=$RUN_DIR interval=${INTERVAL}s search='$SEARCH_ARGS'" >> "$LOG"
while true; do
  sleep "$INTERVAL"
  CK="$RUN_DIR/$(cat "$RUN_DIR/LATEST" 2>/dev/null)"
  [ -f "$CK" ] || { echo "[gate_loop] $(date -Is) no ckpt yet" >> "$LOG"; continue; }
  ./scripts/stop_train.sh "$RUN_DIR" >> "$LOG" 2>&1
  sleep 5
  echo "[gate_loop] $(date -Is) GATE EVAL ckpt=$(basename "$CK")" >> "$LOG"
  .venv/bin/python -m probes.value_search "$CK" --games 300 --envs 32 $SEARCH_ARGS \
      --opponent mcts --mcts-ms 100 >> "$LOG" 2>&1
  .venv/bin/python -m probes.eval_ckpt "$CK" --games 300 --baselines heuristic >> "$LOG" 2>&1
  NO_SIDECAR=1 ./scripts/resume.sh "$RUN_DIR" >> "$LOG" 2>&1 || \
      NO_SIDECAR=1 ./scripts/launch_train.sh "$RUN_DIR" >> "$LOG" 2>&1
done
