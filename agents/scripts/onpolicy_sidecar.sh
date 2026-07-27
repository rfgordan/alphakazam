#!/usr/bin/env bash
# On-policy cosim sidecar (DETERMINISTIC path).
#
#   agents/scripts/onpolicy_sidecar.sh runs/scale1
#
# Why it exists: the parity corpora are PS-led with random-ish choices, so they certify the states
# *those* games visit — not the states a trained policy visits. RL adversarially searches for
# reward, so a divergence anywhere on the policy's own distribution is an exploit waiting to be
# farmed (RESEARCH_PLAN.md §P1.1). This runs that check continuously against the live checkpoint.
#
# Each sweep, per seed:
#   1. `harness/cosim.mjs --seed S --policy ...` plays a full gen9randombattle inside PINNED
#      Showdown with every choice supplied by the run's latest checkpoint (scripts/policy_server.py
#      converts each PS request through the certified `convert_state` + the training encoder, so
#      the policy sees exactly its training inputs). Output is a standard v2 trace.
#   2. `SEED_GATE=1 cosim <trace>` replays it: the engine's `Replicate` executor is driven off a
#      `PsPrng` seeded from the same battle seed, and the converted state is byte-compared after
#      EVERY decision.
#
# Deterministic end to end — one outcome per decision, no enumeration and no path cap. (An earlier
# transplant-based version had to enumerate PS's whole outcome tree and ask whether the engine's
# sampled result was *somewhere* in it, because the training env samples with its own splitmix RNG
# rather than PS's PRNG. That is what a `--max-paths` cap was for; this path has no such knob.)
#
# It only ever REPORTS. Divergence burn-down is a separate campaign.
#
# Env knobs: SIDECAR_EVERY (seconds between sweeps, default 1800)
#            SIDECAR_GAMES (battles per sweep, default 4)
#            SIDECAR_SEED_BASE (first battle seed; stays < 60000, PS seeds are u16 limbs)
#            SIDECAR_MAX_DECISIONS (per battle, default 400)
set -uo pipefail

cd "$(dirname "$0")/.." || exit 1
AGENTS=$(pwd)
RS="$AGENTS/../showdown-rs"

RUN_DIR=${1:?usage: onpolicy_sidecar.sh <run-dir>}
mkdir -p "$RUN_DIR/cosim"
# Absolute: the recorder and the gate both run with cwd=showdown-rs.
OUT=$(cd "$RUN_DIR/cosim" && pwd)
LOG="$OUT/sidecar.log"

# One sidecar per run directory. Two loops just halve each other's CPU and interleave verdicts.
if [ -f "$OUT/sidecar.pid" ] && kill -0 "$(cat "$OUT/sidecar.pid")" 2>/dev/null; then
	echo "sidecar already running for $RUN_DIR (pid $(cat "$OUT/sidecar.pid"))" >&2
	exit 1
fi
echo $$ > "$OUT/sidecar.pid"

EVERY=${SIDECAR_EVERY:-1800}
GAMES=${SIDECAR_GAMES:-4}
MAX_DECISIONS=${SIDECAR_MAX_DECISIONS:-400}
# PS battle seeds are four u16 limbs and the recorder derives them as [S, S+7, S+13, S+29], so S
# must stay well under 65535. Kept away from the committed seed-fixture range (rb1000+) so a
# sidecar trace is never confused with a corpus one.
SEED_BASE=${SIDECAR_SEED_BASE:-40000}
PY=${PY:-$AGENTS/.venv/bin/python}
NODE=${NODE:-node}
command -v "$NODE" >/dev/null || NODE="$HOME/.local/node-v22.14.0-linux-x64/bin/node"
COSIM="$RS/target/release/cosim"

stopping=0
trap 'stopping=1; [ -n "${napper:-}" ] && kill "$napper" 2>/dev/null' TERM INT

# Bash defers trap handling until the current foreground command returns, so a plain
# `sleep $EVERY` would swallow SIGTERM for up to EVERY seconds — `stop_train.sh` appeared to hang.
nap() {
	sleep "$1" &
	napper=$!
	wait "$napper" 2>/dev/null
	napper=""
	[ "$stopping" -eq 1 ] && { echo "[sidecar] stopping" >> "$LOG"; exit 0; }
	return 0
}

[ -x "$COSIM" ] || { echo "[sidecar] no cosim binary at $COSIM (cargo build --release -p cosim)" | tee -a "$LOG"; exit 1; }
echo "[sidecar] start $(date -Is) run=$RUN_DIR every=${EVERY}s games=$GAMES" >> "$LOG"

sweep=0
while true; do
	sweep=$((sweep + 1))
	# Newest weights-only checkpoint; none yet (fresh run) is fine — a random-init policy is still
	# a valid, if less interesting, on-policy distribution, so the sidecar never blocks on training.
	CKPT=""
	if [ -f "$RUN_DIR/LATEST" ]; then
		cand="$RUN_DIR/$(cat "$RUN_DIR/LATEST")"
		[ -f "$cand" ] && CKPT="$cand"
	fi
	[ -n "$CKPT" ] || CKPT=$(ls -1t "$RUN_DIR"/ckpt_*.pt 2>/dev/null | head -1)

	STAMP=$(date +%Y%m%d-%H%M%S)
	TDIR="$OUT/traces-$STAMP"
	mkdir -p "$TDIR"
	echo "[sidecar] $(date -Is) sweep=$sweep ckpt=${CKPT:-<random-init>}" >> "$LOG"

	recorded=0
	for g in $(seq 0 $((GAMES - 1))); do
		# Fresh seeds every sweep so the run keeps covering new games instead of re-checking one
		# fixed set. Wrapped to stay inside the u16 limb range.
		SEED=$(( (SEED_BASE + (sweep - 1) * GAMES + g) % 60000 + 1 ))
		# nice: the trainer saturates every core on this box, and this check has no deadline.
		if nice -n 19 "$NODE" "$RS/harness/cosim.mjs" --seed "$SEED" --format gen9randombattle \
			--max-decisions "$MAX_DECISIONS" --out "$TDIR/op-$SEED.json.gz" \
			--policy "$PY $AGENTS/scripts/policy_server.py ${CKPT:+--ckpt $CKPT} --device cpu --seed $SEED" \
			>>"$LOG" 2>&1; then
			recorded=$((recorded + 1))
		else
			echo "[sidecar] record FAILED seed=$SEED" >> "$LOG"
		fi
	done

	if [ "$recorded" -eq 0 ]; then
		echo "[sidecar] $(date -Is) NO-TRACES sweep=$sweep — see log" | tee -a "$OUT/verdicts.log" >> "$LOG"
		rm -rf "$TDIR"; nap "$EVERY"; continue
	fi

	REPORT="$OUT/gate-$STAMP.txt"
	( cd "$RS" && SEED_GATE=1 nice -n 19 "$COSIM" "$TDIR"/*.json.gz ) >"$REPORT" 2>>"$LOG"
	rc=$?

	EXACT=$(grep -m1 "FULL-GAME EXACT" "$REPORT" | sed 's/.*: //')
	DIVS=$(sed -n '/per-game first divergence/,/first-divergence category/p' "$REPORT" | grep -c "align=")
	VERDICT=$([ "${DIVS:-0}" -eq 0 ] && echo CLEAN || echo DIVERGENCE)
	echo "[sidecar] $(date -Is) $VERDICT sweep=$sweep ckpt=$(basename "${CKPT:-random-init}") games=$recorded exact=${EXACT:-?} diverged=${DIVS:-?} rc=$rc" \
		| tee -a "$OUT/verdicts.log" >> "$LOG"
	# The ranked first-divergence lines are the work queue; keep them next to the verdict.
	[ "${DIVS:-0}" -gt 0 ] && sed -n '/per-game first divergence/,$p' "$REPORT" >> "$LOG"

	# Traces are reproducible from (seed, checkpoint); keep only the last few sweeps' worth.
	ls -1dt "$OUT"/traces-* 2>/dev/null | tail -n +4 | xargs -r rm -rf
	ls -1t "$OUT"/gate-*.txt 2>/dev/null | tail -n +31 | xargs -r rm -f

	nap "$EVERY"
done
