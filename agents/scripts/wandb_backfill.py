"""One-shot: rebuild runs/<run>'s full W&B history as a single run (id='scale2').

Replays metrics.jsonl + eval.jsonl under the nightX naming scheme (bare training keys,
eval/<name> scalars, pool/<opp>/{wr,weight}), sorted by step so wandb's monotonic-step
requirement holds. Run with the trainer STOPPED; the trainer then resumes onto the same id.
"""
import json
import sys

import wandb

RUN_DIR = sys.argv[1] if len(sys.argv) > 1 else "runs/scale2"
rows = []
for path, kind in ((f"{RUN_DIR}/metrics.jsonl", "m"), (f"{RUN_DIR}/eval.jsonl", "e")):
    with open(path) as f:
        for line in f:
            d = json.loads(line)
            rows.append((d.get("step", 0), kind, d))
rows.sort(key=lambda r: r[0])

NAME = RUN_DIR.rstrip("/").split("/")[-1]
cfg = json.load(open(f"{RUN_DIR}/config.json"))
run = wandb.init(project="deep-showdown", name=NAME, id=NAME, resume="allow",
                 dir=RUN_DIR, config={"backfilled": True})

n = 0
for step, kind, d in rows:
    if kind == "m":
        payload = {k: v for k, v in d.items()
                   if isinstance(v, (int, float)) and not k.startswith("league_")}
        st = d.get("league")
        if isinstance(st, dict):
            scripted = [k for k in ("random", "heuristic") if k in st]
            for k in scripted:
                payload[f"pool/{k}/wr"] = st[k]["wr"]
                payload[f"pool/{k}/weight"] = st[k]["w"]
            snaps = {k: v for k, v in st.items() if k.startswith("snap_")}
            played = [v["wr"] for v in snaps.values() if v["n"] > 0]
            payload["pool/self/wr"] = (sum(played) / len(played)) if played else 0.5
            payload["pool/self/weight"] = sum(v["w"] for v in snaps.values())
    else:
        bl = d["baseline"]
        payload = {f"eval/{bl}": d["win_rate"],
                   f"eval/{bl}/draws": d["draws"],
                   f"eval/{bl}/avg_turns": d.get("avg_turns", d.get("avg_decisions", 0.0)),
                   f"eval/{bl}/ci_low": d.get("ci_low", 0.0),
                   f"eval/{bl}/ci_high": d.get("ci_high", 1.0)}
    run.log(payload, step=step)
    n += 1
run.finish()
print(f"backfilled {n} rows into run id 'scale2'")
