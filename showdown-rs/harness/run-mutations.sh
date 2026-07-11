#!/bin/bash
# Mutation kill suite: inject one-line engine bugs and require the cosim verifier to catch
# every one. This measures the verifier's sensitivity directly — the answer to "do I trust
# the verification?" is this suite's kill rate, not an assertion.
#
# Two mutant classes, each checked by the gate that is actually able to see the bug:
#   outcome  — changes the SET of reachable outcomes. Caught by the sampled-corpus sweep
#              (exact full-field membership). Several are invisible by construction to the old
#              relaxed verifier (PP errors, volatile bookkeeping, type restoration); the suite
#              documents that the new exact-comparison flow catches them.
#   prob     — leaves the outcome support identical but perturbs a branch PROBABILITY. These
#              PASS the sampled sweep (same support) and can only be killed by the exact
#              distribution gate (the "Mode 2" oracle). Checked against a fast subset of the
#              distribution-smoke seeds, pre-generated once (PS oracles are invariant to the
#              Rust-side mutation, so they need not be regenerated per mutant).
#
# Usage: harness/run-mutations.sh        (from showdown-rs/)
set -u
cd "$(dirname "$0")/.."

GEN=crates/engine/src/generate.rs
DMG=crates/engine/src/damage.rs
TRACES="harness/cosim-traces/*.json.gz"

if ! git diff --quiet -- crates/engine/src/; then
    echo "refusing to run: crates/engine/src has uncommitted changes"; exit 2
fi

declare -a NAMES FILES SEDS KINDS
# add NAME FILE SED KIND   (KIND = outcome | prob)
add() { NAMES+=("$1"); FILES+=("$2"); SEDS+=("$3"); KINDS+=("$4"); }

# --- outcome mutants: killed by the sampled-corpus sweep (membership) ---
# mutation name              file    | one-line bug injected via sed                                             | kind
add "crit-multiplier-2x"      "$DMG"  's|d = d \* 3 / 2;|d = d * 2;|'                                              outcome
add "leftovers-skipped"       "$GEN"  's|if p.item == Item::Leftovers \&\& p.hp < p.max_hp|if false \&\& p.hp < p.max_hp|' outcome
add "pressure-removed"        "$GEN"  's|&& pressure_affected(&md)|\&\& false|'                                    outcome
add "choicelock-not-set"      "$GEN"  's|push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ChoiceLock });|/* mutated out */|' outcome
add "roost-never-restores"    "$GEN"  's|if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {|if false {|' outcome
add "times-hit-once-per-move" "$GEN"  's|let new = cur.saturating_add(hits_landed).min(250);|let new = cur.saturating_add(1).min(250);|' outcome
add "pp-cost-doubled"         "$GEN"  's|let amount = if pressured { 2u8.min(pp) } else { 1 };|let amount = if pressured { 2u8.min(pp) } else { 2u8.min(pp) };|' outcome
add "stealth-rock-never-set"  "$GEN"  's|SideConditionId::StealthRock|SideConditionId::Spikes|'                    outcome

# --- probability mutants: identical support, wrong mass. Invisible to the sampled sweep;
#     killed ONLY by the exact distribution gate. ---
# confusion self-hit: PS uses randomChance(33,100); bump to 40/100 (same two outcomes, wrong split).
add "confusion-selfhit-40pct" "$GEN"  's|act \* 0.33|act * 0.40|'                                                  prob
# base critical-hit rate: 1/24 -> 1/16 (crit/non-crit support unchanged, wrong probabilities).
add "crit-base-rate-1in16"    "$GEN"  's|0 => 1.0 / 24.0,|0 => 1.0 / 16.0,|'                                       prob
# damage-roll weighting: each of the 16 rolls is 1/16; skew to 1/15 (same roll values, wrong mass).
add "damage-roll-weight-1in15" "$GEN" 's|prob \*= (1.0 / 16.0)|prob *= (1.0 / 15.0)|'                              prob

# Fast distribution kill-check: a subset of run-distribution-smoke.sh's seeds that between them
# exercise confusion (15,32), crits and damage rolls (all). Generated once from the clean engine;
# the PS oracle for a decision point does not depend on the Rust build under test.
DIST_SEEDS=(3 6 15 32 38)
DIST_OUT="${TMPDIR:-/tmp}/deep-showdown-mut-dist"
have_prob=0
for k in "${KINDS[@]}"; do [ "$k" = prob ] && have_prob=1; done
if [ "$have_prob" -eq 1 ]; then
    rm -rf "$DIST_OUT"; mkdir -p "$DIST_OUT"
    echo "pre-generating ${#DIST_SEEDS[@]} distribution oracles for probability mutants..."
    for s in "${DIST_SEEDS[@]}"; do
        node harness/cosim.mjs --seed "$s" --teamset diverse --max-decisions 2 \
            --distributions --max-dist-paths 100000 --out "$DIST_OUT/d$s.json" >/dev/null 2>&1 \
            || { echo "FATAL: could not generate distribution oracle for seed $s"; exit 2; }
    done
fi

# Kill check for a probability mutant: run the (already-built) mutant cosim against each cached
# distribution oracle. The mutant is KILLED if ANY seed's exact distribution diverges.
dist_kills() {
    for s in "${DIST_SEEDS[@]}"; do
        if ! ./target/release/cosim "$DIST_OUT/d$s.json" >/dev/null 2>&1; then return 0; fi
    done
    return 1
}

killed=0; survived=0; broken=0
for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"; target="${FILES[$i]}"; sedexpr="${SEDS[$i]}"; kind="${KINDS[$i]}"
    cp "$target" "$target.orig"
    sed -i '' -e "$sedexpr" "$target"
    if cmp -s "$target" "$target.orig"; then
        echo "BROKEN  $name (sed matched nothing — update the pattern)"
        broken=$((broken+1))
        mv "$target.orig" "$target"; continue
    fi
    if ! cargo build --release -p cosim >/dev/null 2>&1; then
        echo "BROKEN  $name (mutant does not compile)"
        broken=$((broken+1))
        mv "$target.orig" "$target"; continue
    fi

    if [ "$kind" = prob ]; then
        # Sanity: a pure-probability mutant should SURVIVE the sampled sweep (same support).
        # It must instead be killed by the exact distribution gate.
        if dist_kills; then
            echo "killed  $name  (distribution gate)"
            killed=$((killed+1))
        else
            echo "SURVIVED  $name  <-- distribution gate blind spot!"
            survived=$((survived+1))
        fi
    else
        if ./target/release/cosim $TRACES >/dev/null 2>&1; then
            echo "SURVIVED  $name  <-- verifier blind spot!"
            survived=$((survived+1))
        else
            echo "killed  $name"
            killed=$((killed+1))
        fi
    fi
    mv "$target.orig" "$target"
done

# restore a clean build of the unmutated engine
cargo build --release -p cosim >/dev/null 2>&1

echo
echo "mutation kill: $killed killed, $survived survived, $broken broken"
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
