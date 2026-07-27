#!/usr/bin/env bash
# Resume the standing scale run with its canonical settings — so nobody has to paste a
# twelve-line command into tmux (multi-line paste through iTerm -> ssh -> tmux is exactly the
# thing that mangles backslash continuations).
#
#   agents/scripts/resume.sh                 # resume runs/scale1, detached, with W&B
#   agents/scripts/resume.sh runs/other      # a different run directory
#   NO_SIDECAR=1 agents/scripts/resume.sh    # trainer only
#   agents/scripts/resume.sh runs/scale1 --entropy-coef 0.01    # extra args are appended
#
# Everything here is the same set `RUNBOOK.md §1b` documents; change it in one place.
set -euo pipefail
cd "$(dirname "$0")/.."

RUN_DIR=${1:-runs/scale1}
[ $# -gt 0 ] && shift || true

exec ./scripts/launch_train.sh "$RUN_DIR" \
	--num-envs 4096 \
	--rollout-steps 32 \
	--minibatch-size 16384 \
	--update-epochs 2 \
	--ckpt-every 10 \
	--snapshot-every 10 \
	--pool-size 24 \
	--opponent-slots 4 \
	--pfsp-mode frontier \
	--eval-every 25 \
	--eval-games 200 \
	--wandb \
	--wandb-project deep-showdown \
	"$@"
