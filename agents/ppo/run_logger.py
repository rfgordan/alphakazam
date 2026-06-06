"""Tiny run logger: a timestamped directory with JSONL metrics, eval results, config, and checkpoints.

    runs/<timestamp>/
        config.json     the PPOConfig + CLI args for this run
        metrics.jsonl   one line per PPO update (training stats)
        eval.jsonl      one line per periodic baseline evaluation
        ckpt_<update>.pt model checkpoints

JSONL is append-and-flush so a run can be tailed live or analyzed after the fact:
    import json; [json.loads(l) for l in open("runs/<ts>/eval.jsonl")]
"""

from __future__ import annotations

import json
import os
import time
from datetime import datetime


class RunLogger:
    def __init__(self, root: str = "runs", name: str | None = None):
        ts = datetime.now().strftime("%Y%m%d-%H%M%S")
        self.dir = os.path.join(root, name or ts)
        os.makedirs(self.dir, exist_ok=True)
        self._metrics = open(os.path.join(self.dir, "metrics.jsonl"), "a", buffering=1)
        self._eval = open(os.path.join(self.dir, "eval.jsonl"), "a", buffering=1)
        self.start = time.time()
        print(f"logging to {self.dir}/")

    def config(self, d: dict):
        with open(os.path.join(self.dir, "config.json"), "w") as f:
            json.dump(d, f, indent=2, default=str)

    def metrics(self, d: dict):
        self._metrics.write(json.dumps({"wall": round(time.time() - self.start, 1), **d}) + "\n")

    def eval(self, d: dict):
        self._eval.write(json.dumps({"wall": round(time.time() - self.start, 1), **d}) + "\n")

    def checkpoint_path(self, update: int) -> str:
        return os.path.join(self.dir, f"ckpt_{update:06d}.pt")

    def close(self):
        self._metrics.close()
        self._eval.close()
