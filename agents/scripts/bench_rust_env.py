"""P1.4 throughput benchmark: vectorized Rust env (BattleVec) with batched numpy exchange.

Measures env-steps/s (1 env-step = 1 turn = both sides act) in three configurations:
  raw     — random legal actions chosen in numpy, no model
  model   — + batched torch forward for BOTH sides each turn (the training-loop shape)
  compile — model wrapped in torch.compile (what a real run would use)

Rebuild the bridge first after engine changes:
  cd agents && uv run maturin develop --release -m ../showdown-rs/crates/pybridge/Cargo.toml
Run:
  uv run python scripts/bench_rust_env.py --envs 512 --seconds 10
"""

from __future__ import annotations

import argparse
import time

import numpy as np


def random_legal(mask: np.ndarray, rng: np.random.Generator) -> np.ndarray:
    """Uniform-random legal action per row of a (N, A) bool mask."""
    noise = rng.random(mask.shape) * mask
    # rows with no legal action (already-lost side): argmax returns 0; the bridge coerces.
    return noise.argmax(axis=1).astype(np.int64)


def bench(envs: int, seconds: float, mode: str) -> tuple[float, int]:
    from showdown_engine import BattleVec

    vec = BattleVec(envs, seed=7)
    rng = np.random.default_rng(7)

    model = None
    if mode in ("model", "compile"):
        import torch

        obs_dim = vec.obs_dim
        model = torch.nn.Sequential(
            torch.nn.Linear(obs_dim, 256), torch.nn.Tanh(),
            torch.nn.Linear(256, 256), torch.nn.Tanh(),
            torch.nn.Linear(256, vec.n_actions),
        )
        model.eval()
        if mode == "compile":
            model = torch.compile(model)

        @torch.no_grad()
        def act(side: int) -> np.ndarray:
            obs = torch.from_numpy(vec.observe_all(side))
            mask = torch.from_numpy(vec.legal_all(side))
            logits = model(obs).masked_fill(~mask, float("-inf"))
            return logits.argmax(dim=-1).numpy()

    steps = 0
    episodes = 0
    start = time.perf_counter()
    while time.perf_counter() - start < seconds:
        if model is None:
            red = random_legal(vec.legal_all(0), rng)
            blue = random_legal(vec.legal_all(1), rng)
        else:
            red, blue = act(0), act(1)
        done, _winner = vec.step_all(red, blue)
        steps += envs
        episodes += int(done.sum())
    return steps / (time.perf_counter() - start), episodes


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--envs", type=int, default=512)
    ap.add_argument("--seconds", type=float, default=10.0)
    ap.add_argument("--modes", default="raw,model")
    args = ap.parse_args()

    for mode in args.modes.split(","):
        sps, eps = bench(args.envs, args.seconds, mode)
        print(f"{mode:>8}: {sps:>10.0f} env-steps/s (turns; x2 agent decisions)  [{eps} episodes]")


if __name__ == "__main__":
    main()
