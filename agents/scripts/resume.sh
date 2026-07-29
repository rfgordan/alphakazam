#!/usr/bin/env bash
# Resume the standing scale run with its canonical settings — so nobody has to paste a
# twelve-line command into tmux (multi-line paste through iTerm -> ssh -> tmux is exactly the
# thing that mangles backslash continuations).
#
#   agents/scripts/resume.sh                 # resume runs/hero2 (full night-era kit), detached, with W&B
#   agents/scripts/resume.sh runs/other      # a different run directory
#   NO_SIDECAR=1 agents/scripts/resume.sh    # trainer only
#   agents/scripts/resume.sh runs/scale2 --entropy-coef 0.01    # extra args are appended
#
# Everything here is the same set `RUNBOOK.md §1b` documents; change it in one place.
# Architecture flags ARE present (a fresh run needs them); on resume the trainer still
# restores hidden_dim / n_hidden_layers / embed_dim from the run's own checkpoint, so a
# mismatch on resume is corrected, not fatal.
set -euo pipefail
cd "$(dirname "$0")/.."

RUN_DIR=${1:-runs/hero2}
[ $# -gt 0 ] && shift || true

exec ./scripts/launch_train.sh "$RUN_DIR" \
	--num-envs 4096 \
	--rollout-steps 32 \
	--minibatch-size 16384 \
	--update-epochs 4 \
	--target-kl 0.03 \
	--shaping-coef 0.15 \
	--league-heuristic-weight 4.0 \
	--fog-species \
	--obs-version 2 \
	--frames 2 \
	--outcome-head \
	--setslot \
	--hidden-dim 1024 \
	--n-hidden-layers 3 \
	--embed-dim 48 \
	--lr-anneal-horizon 1500000000 \
	--shaping-anneal-horizon 500000000 \
	--team-pool ../showdown-rs/harness/team-pool/gen9randombattle-100k.jsonl.gz \
	--ckpt-every 10 \
	--keep-checkpoints 40 \
	--snapshot-every 10 \
	--pool-size 24 \
	--opponent-slots 4 \
	--pfsp-mode frontier \
	--eval-every 75 \
	--eval-games 300 \
	--wandb \
	--wandb-project deep-showdown \
	"$@"
