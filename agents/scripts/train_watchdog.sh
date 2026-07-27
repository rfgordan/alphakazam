#!/usr/bin/env bash
# Detached, auto-resuming trainer for long GPU runs (Linux).
#
# Supersedes run_watchdog.sh, which was macOS-only (zsh + `caffeinate`) and drove the poke-env
# trainer. This one drives `ppo.train_flow` (the Rust decision-point engine) and resumes the run
# directory in place after any death.
#
#   agents/scripts/train_watchdog.sh runs/scale1 [extra train_flow args...]
#
# Launch DETACHED so nothing in the parent session can reap it:
#   setsid nohup agents/scripts/train_watchdog.sh runs/scale1 --num-envs 1024 \
#       </dev/null >/dev/null 2>&1 &
# (or just use `agents/scripts/launch_train.sh`, which does that for you).
#
# Exits 0 when the trainer prints "done." (target steps reached), 1 on a crash loop.
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1

RUN_DIR=${1:?usage: train_watchdog.sh <run-dir> [train_flow args...]}
shift

mkdir -p "$RUN_DIR"
LOG="$RUN_DIR/train.log"
PY=${PY:-.venv/bin/python}
MAX_ITERS=${MAX_ITERS:-500}
# Deaths faster than this count toward the crash-loop trip.
MIN_HEALTHY_SECS=${MIN_HEALTHY_SECS:-120}
CRASH_LOOP_TRIPS=${CRASH_LOOP_TRIPS:-3}

echo "[watchdog] start $(date -Is) run=$RUN_DIR args=$* pid=$$" | tee -a "$LOG"
echo $$ > "$RUN_DIR/watchdog.pid"

# Forward a stop request to the trainer, which checkpoints on SIGTERM before exiting.
child=""
stopping=0
trap 'stopping=1; [ -n "$child" ] && kill -TERM "$child" 2>/dev/null' TERM INT

rapid=0
for ((i = 1; i <= MAX_ITERS; i++)); do
	start=$(date +%s)
	PYTHONUNBUFFERED=1 "$PY" -m ppo.train_flow --resume "$RUN_DIR" "$@" >>"$LOG" 2>&1 &
	child=$!
	echo "$child" > "$RUN_DIR/train.pid"
	wait "$child"
	code=$?
	child=""
	dur=$(($(date +%s) - start))
	echo "[watchdog] iter=$i exit=$code after ${dur}s at $(date -Is)" | tee -a "$LOG"

	if [ "$stopping" -eq 1 ]; then
		echo "[watchdog] stop requested — not restarting" | tee -a "$LOG"
		exit 0
	fi
	if tail -20 "$LOG" | grep -q "train_flow\] done\."; then
		echo "[watchdog] run COMPLETE" | tee -a "$LOG"
		exit 0
	fi

	if [ "$dur" -lt "$MIN_HEALTHY_SECS" ]; then
		rapid=$((rapid + 1))
	else
		rapid=0
	fi
	if [ "$rapid" -ge "$CRASH_LOOP_TRIPS" ]; then
		echo "[watchdog] crash loop ($rapid rapid deaths) — giving up. Last output:" | tee -a "$LOG"
		tail -30 "$LOG"
		exit 1
	fi
	sleep 15
done

echo "[watchdog] hit MAX_ITERS=$MAX_ITERS — giving up" | tee -a "$LOG"
exit 1
