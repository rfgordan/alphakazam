"""Run logger: a timestamped directory with JSONL metrics, eval results, config, checkpoints —
and optional Weights & Biases mirroring.

    runs/<timestamp>/
        config.json     the PPOConfig + CLI args for this run
        metrics.jsonl   one line per PPO update (training stats)
        eval.jsonl      one line per periodic baseline evaluation
        ckpt_<update>.pt model checkpoints

JSONL is append-and-flush so a run can be tailed live or analyzed after the fact:
    import json; [json.loads(l) for l in open("runs/<ts>/eval.jsonl")]

If `wandb_project` is given (and `wandb` is installed/logged-in), every training metric is mirrored
under `train/*` and every eval under `eval/<baseline>/*`, keyed by environment step. Set
`WANDB_MODE=offline` to log without a network/login.
"""

from __future__ import annotations

import hashlib
import json
import os
import time
from datetime import datetime


class RunLogger:
    def __init__(self, root: str = "runs", name: str | None = None, wandb_project: str | None = None):
        ts = datetime.now().strftime("%Y%m%d-%H%M%S")
        self.dir = os.path.join(root, name or ts)
        os.makedirs(self.dir, exist_ok=True)
        self._metrics = open(os.path.join(self.dir, "metrics.jsonl"), "a", buffering=1)
        self._eval = open(os.path.join(self.dir, "eval.jsonl"), "a", buffering=1)
        self.start = time.time()
        print(f"logging to {self.dir}/")

        self._wandb = None
        if wandb_project:
            try:
                import wandb
                # A stable id derived from the run directory, so every resume/relaunch CONTINUES
                # one W&B run instead of minting a new id per process (scale2 shattered into four
                # W&B runs in an afternoon of restarts before this).
                run_name = os.path.basename(self.dir)
                stable_id = hashlib.sha1(os.path.abspath(self.dir).encode()).hexdigest()[:16]
                wandb.init(project=wandb_project, name=run_name, dir=self.dir,
                           id=stable_id, resume="allow")
                self._wandb = wandb
                print(f"wandb: project '{wandb_project}' run '{os.path.basename(self.dir)}'")
            except Exception as e:  # not installed / not logged in -> fall back to files only
                print(f"wandb disabled ({type(e).__name__}: {e})")

    def config(self, d: dict):
        with open(os.path.join(self.dir, "config.json"), "w") as f:
            json.dump(d, f, indent=2, default=str)
        if self._wandb:
            self._wandb.config.update(_flatten(d), allow_val_change=True)

    def metrics(self, d: dict):
        self._metrics.write(json.dumps({"wall": round(time.time() - self.start, 1), **d}) + "\n")
        if self._wandb:
            step = d.get("step")
            payload = {f"train/{k}": v for k, v in d.items()
                       if isinstance(v, (int, float)) and k not in ("step", "update")}
            self._wandb.log(payload, step=step)

    def eval(self, d: dict):
        self._eval.write(json.dumps({"wall": round(time.time() - self.start, 1), **d}) + "\n")
        if self._wandb:
            bl = d["baseline"]
            self._wandb.log({
                f"eval/{bl}/win_rate": d["win_rate"],
                f"eval/{bl}/draws": d["draws"],
                f"eval/{bl}/avg_turns": d["avg_turns"],
            }, step=d.get("step"))

    def checkpoint_path(self, update: int) -> str:
        return os.path.join(self.dir, f"ckpt_{update:06d}.pt")

    def close(self):
        self._metrics.close()
        self._eval.close()
        if self._wandb:
            self._wandb.finish()


def _flatten(d: dict, prefix: str = "") -> dict:
    """Flatten one level of nested dicts (e.g. {'cfg': {...}}) for wandb.config."""
    out = {}
    for k, v in d.items():
        if isinstance(v, dict):
            out.update(_flatten(v, f"{prefix}{k}."))
        else:
            out[f"{prefix}{k}"] = v
    return out
