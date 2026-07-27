#!/bin/bash
# P0.1 master: fast arm (3 pairs x 100 games @ 100ms, parallel), then full arm
# (2 pairs x 75 games @ 2000ms, parallel) after fast finishes to avoid CPU contention.
# Run detached: nohup caffeinate -i scripts/fp_eval_master.sh > runs/fp_eval/master.out 2>&1 &
set -u
cd "$(dirname "$0")/.."
mkdir -p runs/fp_eval

echo "=== FAST ARM (100ms) start: $(date) ==="
scripts/fp_eval.sh A 10 10 100 > runs/fp_eval/pairA.out 2>&1 &
scripts/fp_eval.sh B 10 10 100 > runs/fp_eval/pairB.out 2>&1 &
scripts/fp_eval.sh C 10 10 100 > runs/fp_eval/pairC.out 2>&1 &
wait
echo "=== FAST ARM done: $(date) ==="

echo "=== FULL ARM (2000ms) start: $(date) ==="
scripts/fp_eval.sh D 5 15 2000 > runs/fp_eval/pairD.out 2>&1 &
scripts/fp_eval.sh E 5 15 2000 > runs/fp_eval/pairE.out 2>&1 &
wait
echo "=== FULL ARM done: $(date) ==="

for arm in "ms100:A B C" "ms2000:D E"; do
  ms=${arm%%:*}; pairs=${arm#*:}
  W=0; L=0
  for p in $pairs; do
    w=$(grep -h "Winner: N4E" runs/fp_eval/${p}_${ms}/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
    l=$(grep -h "Winner: FPE" runs/fp_eval/${p}_${ms}/fp_*.log 2>/dev/null | wc -l | tr -d ' ')
    W=$((W + w)); L=$((L + l))
  done
  echo "ARM $ms FINAL: model $W, foul-play $L"
done
echo "ALL DONE: $(date)"
