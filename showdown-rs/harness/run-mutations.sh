#!/bin/bash
# Mutation kill suite: inject one-line engine bugs and require the cosim verifier to catch
# every one. This measures the verifier's sensitivity directly — the answer to "do I trust
# the verification?" is this suite's kill rate, not an assertion.
#
# Several of these mutations are *invisible by construction* to the old relaxed-membership
# verifier (PP errors, volatile bookkeeping, type restoration); the suite documents that the
# new exact-comparison flow catches them.
#
# Usage: harness/run-mutations.sh        (from showdown-rs/)
set -u
cd "$(dirname "$0")/.."

GEN=crates/engine/src/generate.rs
TRACES="harness/cosim-traces/*.json.gz"

if ! git diff --quiet -- "$GEN"; then
    echo "refusing to run: $GEN has uncommitted changes"; exit 2
fi

declare -a NAMES SEDS
add() { NAMES+=("$1"); SEDS+=("$2"); }

# mutation name                          | one-line bug injected via sed
add "crit-multiplier-2x"                  's|rolls_crit\[i\] as i32, (1.0 / 16.0) \* CRIT|rolls_crit[i] as i32 * 2, (1.0 / 16.0) * CRIT|'
add "leftovers-skipped"                   's|Item::Leftovers =>|Item::ChestoBerry =>|'
add "pressure-removed"                    's|&& pressure_affected(&md)|\&\& false|'
add "choicelock-not-set"                  's|push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ChoiceLock });|/* mutated out */|'
add "roost-never-restores"                's|if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {|if false {|'
add "times-hit-once-per-move"             's|let new = cur.saturating_add(hits_landed).min(250);|let new = cur.saturating_add(1).min(250);|'
add "pp-cost-doubled"                     's|let amount = if pressured { 2u8.min(pp) } else { 1 };|let amount = if pressured { 2u8.min(pp) } else { 2u8.min(pp) };|'
add "stealth-rock-never-set"              's|SideConditionId::StealthRock|SideConditionId::Spikes|'

killed=0; survived=0; broken=0
for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"; sedexpr="${SEDS[$i]}"
    cp "$GEN" "$GEN.orig"
    sed -i '' -e "$sedexpr" "$GEN"
    if cmp -s "$GEN" "$GEN.orig"; then
        echo "BROKEN  $name (sed matched nothing — update the pattern)"
        broken=$((broken+1))
        mv "$GEN.orig" "$GEN"; continue
    fi
    if ! cargo build --release -p cosim >/dev/null 2>&1; then
        echo "BROKEN  $name (mutant does not compile)"
        broken=$((broken+1))
        mv "$GEN.orig" "$GEN"; continue
    fi
    if ./target/release/cosim $TRACES >/dev/null 2>&1; then
        echo "SURVIVED  $name  <-- verifier blind spot!"
        survived=$((survived+1))
    else
        echo "killed  $name"
        killed=$((killed+1))
    fi
    mv "$GEN.orig" "$GEN"
done

# restore a clean build of the unmutated engine
cargo build --release -p cosim >/dev/null 2>&1

echo
echo "mutation kill: $killed killed, $survived survived, $broken broken"
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
