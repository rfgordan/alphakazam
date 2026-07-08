#!/bin/zsh
# Detached auto-resuming trainer for long runs. LAUNCH DETACHED so the session's task layer
# cannot reap it (tracked background tasks were repeatedly killed mid-run; detached ran 6.7h clean):
#   (nohup agents/scripts/run_watchdog.sh runs/night4 [extra train_long args] > /tmp/watchdog.out 2>&1 &)
# Auto-resumes on unexpected death; exits on completion ("done.") or a crash loop.
cd "$(dirname "$0")/.."
RUN_DIR=$1; shift
LOG=/tmp/$(basename "$RUN_DIR").log
rapid=0
for i in {1..60}; do
  start=$(date +%s)
  PYTHONUNBUFFERED=1 caffeinate -is .venv/bin/python -m ppo.train_long --resume "$RUN_DIR" "$@" >> "$LOG" 2>&1
  code=$?; dur=$(( $(date +%s) - start ))
  echo "[watchdog] iter=$i exit=$code after ${dur}s at $(date '+%H:%M:%S')" >> "$LOG"
  tail -5 "$LOG" | grep -q "done\." && { echo "[watchdog] run COMPLETE" >> "$LOG"; exit 0; }
  if (( dur < 120 )); then rapid=$((rapid+1)); else rapid=0; fi
  (( rapid >= 3 )) && { echo "[watchdog] crash loop — giving up" >> "$LOG"; exit 1; }
  sleep 15
done
