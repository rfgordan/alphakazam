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

if ! git diff --quiet -- crates/engine/src/; then
    echo "refusing to run: crates/engine/src has uncommitted changes"; exit 2
fi

declare -a NAMES FILES SEDS
add() { NAMES+=("$1"); FILES+=("$2"); SEDS+=("$3"); }

GEN=crates/engine/src/generate.rs
DMG=crates/engine/src/damage.rs

# mutation name              file    | one-line bug injected via sed
add "crit-multiplier-2x"     "$DMG"  's|d = d \* 3 / 2;|d = d * 2;|'
add "leftovers-skipped"      "$GEN"  's|if p.item == Item::Leftovers \&\& p.hp < p.max_hp|if false \&\& p.hp < p.max_hp|'
add "pressure-removed"       "$GEN"  's|&& pressure_affected(&md)|\&\& false|'
add "choicelock-not-set"     "$GEN"  's|push(b, Instruction::ApplyVolatile { side, volatile: VolatileStatus::ChoiceLock });|/* mutated out */|'
add "roost-never-restores"   "$GEN"  's|if b.state.side(side).volatiles.contains(VolatileStatus::Roosted) {|if false {|'
add "times-hit-once-per-move" "$GEN" 's|let new = cur.saturating_add(hits_landed).min(250);|let new = cur.saturating_add(1).min(250);|'
add "pp-cost-doubled"        "$GEN"  's|let amount = if pressured { 2u8.min(pp) } else { 1 };|let amount = if pressured { 2u8.min(pp) } else { 2u8.min(pp) };|'
add "stealth-rock-never-set" "$GEN"  's|SideConditionId::StealthRock|SideConditionId::Spikes|'

killed=0; survived=0; broken=0
for i in "${!NAMES[@]}"; do
    name="${NAMES[$i]}"; target="${FILES[$i]}"; sedexpr="${SEDS[$i]}"
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
    if ./target/release/cosim $TRACES >/dev/null 2>&1; then
        echo "SURVIVED  $name  <-- verifier blind spot!"
        survived=$((survived+1))
    else
        echo "killed  $name"
        killed=$((killed+1))
    fi
    mv "$target.orig" "$target"
done

# restore a clean build of the unmutated engine
cargo build --release -p cosim >/dev/null 2>&1

echo
echo "mutation kill: $killed killed, $survived survived, $broken broken"
[ "$survived" -eq 0 ] && [ "$broken" -eq 0 ]
