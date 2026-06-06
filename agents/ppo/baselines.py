"""Fixed reference opponents + a generic periodic evaluation.

Unlike the *rolling* snapshot used for training (which only shows marginal improvement over a
near-equal recent self), an evaluation against a **fixed** baseline shows *absolute* progress.
The `Baseline` protocol is deliberately tiny — `actions(obs, mask) -> np.ndarray` — so anything
can be a baseline: a frozen anchor checkpoint, a random bot, a hand-written heuristic, or a
specific past model. `evaluate()` plays the learner (greedily) against any baseline over both
sides and reports a win-rate.
"""

from __future__ import annotations

from typing import Protocol

import numpy as np
import torch

from .engine_env import EngineVecEnv


class Baseline(Protocol):
    name: str

    def actions(self, obs: np.ndarray, mask: np.ndarray) -> np.ndarray:
        """Batched action indices for a stack of (obs, legal-mask) rows."""


@torch.no_grad()
def greedy_actions(net, obs_np, mask_np, device) -> np.ndarray:
    """Argmax action under a network's masked policy (used for the learner at eval and for any
    policy-backed baseline / the greedy training opponent)."""
    obs = torch.as_tensor(obs_np, device=device)
    mask = torch.as_tensor(mask_np, device=device)
    logits, _ = net.forward(obs, mask)
    return logits.argmax(dim=-1).cpu().numpy()


class PolicyBaseline:
    """A baseline backed by a (typically frozen) network — e.g. the random-init anchor or any
    saved checkpoint. Greedy by default."""

    def __init__(self, net, device, name: str, greedy: bool = True):
        self.net = net
        self.device = device
        self.name = name
        self.greedy = greedy

    @torch.no_grad()
    def actions(self, obs_np, mask_np) -> np.ndarray:
        obs = torch.as_tensor(obs_np, device=self.device)
        mask = torch.as_tensor(mask_np, device=self.device)
        if self.greedy:
            logits, _ = self.net.forward(obs, mask)
            return logits.argmax(dim=-1).cpu().numpy()
        action, _, _, _ = self.net.act(obs, mask)
        return action.cpu().numpy()


class RandomBaseline:
    """Uniform random over legal actions — the floor every policy should clear."""

    def __init__(self, seed: int = 0, name: str = "random"):
        self.rng = np.random.default_rng(seed)
        self.name = name

    def actions(self, obs_np, mask_np) -> np.ndarray:
        out = np.zeros(len(mask_np), dtype=np.int64)
        for i, m in enumerate(mask_np):
            legal = np.flatnonzero(m)
            out[i] = self.rng.choice(legal) if legal.size else 0
        return out


@torch.no_grad()
def evaluate(model, baseline: Baseline, device, n_games: int = 100, num_envs: int = 8,
             seed: int = 1234567, max_turns: int = 300) -> dict:
    """Play the (greedy) learner vs `baseline` over `n_games`, side-balanced, fixed eval seed."""
    envs = EngineVecEnv(num_envs, seed=seed, max_turns=max_turns)
    results: list[int] = []
    turns: list[int] = []
    while len(results) < n_games:
        obs_l, mask_l = envs.learner_view()
        obs_o, mask_o = envs.opponent_view()
        learner_a = greedy_actions(model, obs_l, mask_l, device)
        opp_a = baseline.actions(obs_o, mask_o)
        prev_len = envs._ep_len.copy()
        reward, done = envs.step(learner_a, opp_a)
        for r, d, pl in zip(reward, done, prev_len):
            if d:
                results.append(1 if r > 0 else (-1 if r < 0 else 0))
                turns.append(int(pl) + 1)
    res = results[:n_games]
    wins = sum(r == 1 for r in res)
    losses = sum(r == -1 for r in res)
    draws = sum(r == 0 for r in res)
    return {
        "baseline": baseline.name,
        "n_games": n_games,
        "win_rate": wins / n_games,
        "wins": wins,
        "losses": losses,
        "draws": draws,
        "avg_turns": float(np.mean(turns[:n_games])) if turns else float("nan"),
    }
