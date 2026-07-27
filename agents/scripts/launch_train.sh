#!/usr/bin/env bash
# Kick off a long training run, fully detached, with the on-policy cosim sidecar alongside.
#
#   agents/scripts/launch_train.sh runs/scale1 [extra train_flow args...]
#
# Starts two detached processes and returns immediately:
#   * the trainer under `train_watchdog.sh` (auto-resumes on death)   -> <run>/train.log
#   * the on-policy cosim sidecar (`onpolicy_sidecar.sh`)             -> <run>/cosim/sidecar.log
#
# Both survive this shell exiting (setsid + nohup + stdin from /dev/null). Stop them with
#   agents/scripts/stop_train.sh runs/scale1
#
# Env knobs:
#   NO_SIDECAR=1     trainer only
#   SIDECAR_EVERY=N  seconds between sidecar sweeps (default 1800)
#   SIDECAR_GAMES=N  battles verified per sweep (default 20)
set -euo pipefail

cd "$(dirname "$0")/.." || exit 1
HERE=$(pwd)

RUN_DIR=${1:?usage: launch_train.sh <run-dir> [train_flow args...]}
shift || true
mkdir -p "$RUN_DIR"

if [ -f "$RUN_DIR/watchdog.pid" ] && kill -0 "$(cat "$RUN_DIR/watchdog.pid")" 2>/dev/null; then
	echo "refusing to launch: a watchdog is already running for $RUN_DIR (pid $(cat "$RUN_DIR/watchdog.pid"))" >&2
	echo "stop it first:  agents/scripts/stop_train.sh $RUN_DIR" >&2
	exit 1
fi

if [ ! -x .venv/bin/python ]; then
	echo "no .venv here — run ./setup.sh from the repo root first" >&2
	exit 1
fi

# Record exactly what produced this run: the launch args and the code they ran against.
{
	echo "launched_at=$(date -Is)"
	echo "args=$*"
	echo "git_commit=$(git -C "$HERE/.." rev-parse HEAD 2>/dev/null || echo unknown)"
	echo "git_dirty=$(git -C "$HERE/.." status --porcelain 2>/dev/null | wc -l) files"
	echo "host=$(hostname)"
	echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
} >> "$RUN_DIR/launches.txt"

setsid nohup ./scripts/train_watchdog.sh "$RUN_DIR" "$@" </dev/null >/dev/null 2>&1 &
sleep 2
echo "trainer  -> $RUN_DIR/train.log   (watchdog pid $(cat "$RUN_DIR/watchdog.pid" 2>/dev/null || echo '?'))"

if [ "${NO_SIDECAR:-0}" != "1" ]; then
	mkdir -p "$RUN_DIR/cosim"
	SIDECAR_EVERY=${SIDECAR_EVERY:-1800} SIDECAR_GAMES=${SIDECAR_GAMES:-20} \
		setsid nohup ./scripts/onpolicy_sidecar.sh "$RUN_DIR" </dev/null >/dev/null 2>&1 &
	sleep 1
	echo "sidecar  -> $RUN_DIR/cosim/sidecar.log (every ${SIDECAR_EVERY:-1800}s, ${SIDECAR_GAMES:-20} games)"
fi

echo
echo "follow:  tail -f $RUN_DIR/train.log"
echo "stop:    agents/scripts/stop_train.sh $RUN_DIR"
