#!/usr/bin/env bash
# Stop a detached training run cleanly: SIGTERM the watchdog (so it won't relaunch) and the
# trainer (which finishes the update it's in, checkpoints, and exits), plus the sidecar.
#
#   agents/scripts/stop_train.sh runs/scale1
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

RUN_DIR=${1:?usage: stop_train.sh <run-dir>}

stop() { # <pidfile> <label>
	local f=$1 label=$2
	[ -f "$f" ] || { echo "no $label pidfile"; return; }
	local pid
	pid=$(cat "$f")
	if kill -0 "$pid" 2>/dev/null; then
		kill -TERM "$pid" 2>/dev/null
		echo "sent TERM to $label (pid $pid)"
	else
		echo "$label (pid $pid) not running"
	fi
}

# Watchdog first, so it observes `stopping` before the trainer's exit would trigger a relaunch.
stop "$RUN_DIR/watchdog.pid" watchdog
stop "$RUN_DIR/train.pid" trainer
stop "$RUN_DIR/cosim/sidecar.pid" sidecar

# The trainer checkpoints on SIGTERM; give it a moment before reporting.
for _ in $(seq 1 30); do
	if [ -f "$RUN_DIR/train.pid" ] && kill -0 "$(cat "$RUN_DIR/train.pid")" 2>/dev/null; then
		sleep 1
	else
		break
	fi
done
echo "final: $(tail -2 "$RUN_DIR/train.log" 2>/dev/null | head -1)"
