"""Promote significantly-improved hero2 checkpoints to the W&B model artifact (user directive).

Called by gate_loop.sh after each gate eval. Reads the newest powered heuristic eval from
gate.log, compares against the best already-uploaded win-rate (state in best_upload.json), and
logs a new version of "<run>-policy" when improved by >= MARGIN. W&B keeps every version;
the `latest` alias moves to the new one.

    .venv/bin/python scripts/upload_best.py runs/hero2
"""

from __future__ import annotations

import json
import os
import sys

MARGIN = 0.02  # points of powered (n=300) heuristic win-rate over the best uploaded so far


def main():
    run_dir = sys.argv[1] if len(sys.argv) > 1 else "runs/hero2"
    name = os.path.basename(run_dir.rstrip("/"))
    gate_log = os.path.join(run_dir, "gate.log")
    state_path = os.path.join(run_dir, "best_upload.json")

    last = None
    with open(gate_log) as f:
        for line in f:
            if '"baseline": "heuristic"' in line and '"ckpt"' in line:
                try:
                    last = json.loads(line)
                except json.JSONDecodeError:
                    pass
    if last is None:
        print("[upload_best] no heuristic eval in gate.log yet")
        return
    ckpt, wr = last["ckpt"], float(last["win_rate"])
    if not os.path.exists(ckpt):
        print(f"[upload_best] {ckpt} pruned; skipping")
        return

    state = {}
    if os.path.exists(state_path):
        try:
            state = json.loads(open(state_path).read())
        except Exception:
            state = {}
    best = float(state.get("best_wr", 0.72))  # seed: the manually-uploaded 2.2B ckpt's level

    if wr < best + MARGIN:
        print(f"[upload_best] wr {wr:.3f} < best {best:.3f} + {MARGIN} — no upload")
        return

    import wandb
    run = wandb.init(project="deep-showdown", id="model-artifacts", resume="allow",
                     job_type="artifact-upload")
    art = wandb.Artifact(f"{name}-policy", type="model",
                         description=f"{name} checkpoint, {wr:.3f} vs perfect-info heuristic "
                                     f"(n=300 powered eval, gate loop auto-promotion).",
                         metadata={"step": last.get("step"), "vs_heuristic": wr,
                                   "ci": [last.get("ci_low"), last.get("ci_high")]})
    art.add_file(ckpt)
    run.log_artifact(art)
    run.finish()
    state = {"best_wr": wr, "ckpt": ckpt, "step": last.get("step")}
    with open(state_path, "w") as f:
        json.dump(state, f)
    print(f"[upload_best] PROMOTED {ckpt} at wr {wr:.3f} (previous best {best:.3f})")


if __name__ == "__main__":
    main()
