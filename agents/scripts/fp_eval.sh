#!/bin/bash
# P0.1 calibration: night4 checkpoint vs foul-play on the local PS server.
# One "pair" = an accept-mode ModelPlayer + a foul-play challenger, run in chunks
# with a hard per-chunk deadline so one hung battle can't stall the arm.
#
#   scripts/fp_eval.sh <PAIR> <CHUNKS> <GAMES_PER_CHUNK> <SEARCH_MS>
#
# Tally at the end parses foul-play's per-game "Winner:" lines (model names N4E*, fp FPE*).
set -u
PAIR=$1; CHUNKS=$2; GAMES=$3; MS=$4
CKPT=${CKPT:-runs/night4/model_27264000.pt}
cd "$(dirname "$0")/.."
LOGDIR=runs/fp_eval/${PAIR}_ms${MS}
mkdir -p "$LOGDIR"

for c in $(seq 1 "$CHUNKS"); do
  BOT="N4E${PAIR}c${c}"
  FP="FPE${PAIR}c${c}"
  uv run python -m ppo.play --mode accept --username "$BOT" --games "$GAMES" \
    --checkpoint "$CKPT" > "$LOGDIR/accept_$c.log" 2>&1 &
  APID=$!
  sleep 10
  ( cd foul-play && .venv/bin/python run.py \
      --websocket-uri ws://localhost:8000/showdown/websocket \
      --ps-username "$FP" --bot-mode challenge_user --user-to-challenge "$BOT" \
      --pokemon-format gen9randombattle --search-time-ms "$MS" \
      --run-count "$GAMES" --log-level INFO ) > "$LOGDIR/fp_$c.log" 2>&1 &
  FPID=$!
  # deadline: generous per-game budget + fixed overhead
  DEADLINE=$(( GAMES * (120 + MS / 10) + 300 ))
  SECS=0
  while kill -0 "$FPID" 2>/dev/null && [ "$SECS" -lt "$DEADLINE" ]; do
    sleep 30; SECS=$((SECS + 30))
  done
  kill "$FPID" 2>/dev/null; kill "$APID" 2>/dev/null
  wait 2>/dev/null
  W=$(grep -h "Winner: N4E" "$LOGDIR"/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
  L=$(grep -h "Winner: FPE" "$LOGDIR"/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
  echo "[pair $PAIR ms$MS] chunk $c/$CHUNKS done — running tally: model $W, foul-play $L"
done
echo "[pair $PAIR ms$MS] COMPLETE"
