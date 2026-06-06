"""Vectorized self-play environment backed by the Rust engine (`showdown_engine`).

The observation/action interface is **egocentric** (each side sees "me" then "foe"), so one
policy plays both sides. To avoid a degenerate agent that only learns one side, the learner is
assigned a **random side per episode** — it trains on both `red_team`-as-me and `blue_team`-as-me
states, and the win-rate it reports is averaged over both perspectives. The env exposes the
learner's view and the opponent's view abstractly (which physical side each is changes per env
and per episode); the trainer never has to track sides.

Rewards are sparse: +1 / -1 to the learner on win / loss, 0 on draw/timeout. Auto-resets on
game over and re-rolls the learner's side.
"""

from __future__ import annotations

import numpy as np

import showdown_engine as se

RED, BLUE = 0, 1


class EngineVecEnv:
    def __init__(self, num_envs: int, seed: int = 0, max_turns: int = 300):
        self.num_envs = num_envs
        self.max_turns = max_turns
        self._base_seed = seed
        self.battles = [se.Battle(seed=seed * 100003 + i) for i in range(num_envs)]
        self.obs_dim = self.battles[0].obs_dim
        self.n_actions = self.battles[0].n_actions
        self._ep_len = np.zeros(num_envs, dtype=np.int32)
        self._resets = np.zeros(num_envs, dtype=np.int64)
        self._rng = np.random.default_rng(seed ^ 0xA5A5_5A5A)
        # Which physical side (RED/BLUE) the learner controls in each env this episode.
        self.learner_side = self._rng.integers(0, 2, size=num_envs).astype(np.int64)

    def _view(self, sides: np.ndarray) -> tuple[np.ndarray, np.ndarray]:
        obs = np.empty((self.num_envs, self.obs_dim), dtype=np.float32)
        mask = np.empty((self.num_envs, self.n_actions), dtype=bool)
        for i, b in enumerate(self.battles):
            s = int(sides[i])
            obs[i] = np.asarray(b.observe(s), dtype=np.float32)
            mask[i] = np.asarray(b.legal_actions(s), dtype=bool)
        return obs, mask

    def learner_view(self) -> tuple[np.ndarray, np.ndarray]:
        """(obs, mask) from the learner's perspective in each env."""
        return self._view(self.learner_side)

    def opponent_view(self) -> tuple[np.ndarray, np.ndarray]:
        """(obs, mask) from the opponent's perspective in each env."""
        return self._view(1 - self.learner_side)

    def step(self, learner_actions, opponent_actions):
        """Advance every battle. Returns (reward[num_envs], done[num_envs]) for the learner.

        Each env routes the learner's action to whichever physical side it controls this episode.
        """
        rewards = np.zeros(self.num_envs, dtype=np.float32)
        dones = np.zeros(self.num_envs, dtype=np.float32)
        for i, b in enumerate(self.battles):
            ls = int(self.learner_side[i])
            la, oa = int(learner_actions[i]), int(opponent_actions[i])
            red_a, blue_a = (la, oa) if ls == RED else (oa, la)

            done, winner, _ = b.step(red_a, blue_a)
            self._ep_len[i] += 1
            if not done and self._ep_len[i] >= self.max_turns:
                done, winner = True, -1  # timeout -> draw
            if done:
                rewards[i] = 1.0 if winner == ls else (-1.0 if winner == (1 - ls) else 0.0)
                dones[i] = 1.0
                self._resets[i] += 1
                b.reset(seed=self._base_seed * 100003 + i + self._resets[i] * 7919)
                self._ep_len[i] = 0
                self.learner_side[i] = self._rng.integers(0, 2)  # re-roll side each episode
        return rewards, dones
