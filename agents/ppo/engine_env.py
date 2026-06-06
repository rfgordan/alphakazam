"""Vectorized self-play environment backed by the Rust engine (`showdown_engine`).

Unlike the placeholder `DummyBattleEnv`, this is a real two-player game, so the env exposes
*both* sides' observations and masks; the training loop decides each side's action (the learner
samples, the greedy opponent argmaxes) and the env advances both at once. Rewards are sparse:
+1 / -1 to the learner on win / loss, 0 otherwise. Battles auto-reset on termination.
"""

from __future__ import annotations

import numpy as np

import showdown_engine as se

RED, BLUE = 0, 1


class EngineVecEnv:
    """`num_envs` Rust battles stepped in lockstep, auto-resetting on game over."""

    def __init__(self, num_envs: int, seed: int = 0, max_turns: int = 300):
        self.num_envs = num_envs
        self.max_turns = max_turns
        self._base_seed = seed
        self.battles = [se.Battle(seed=seed * 100003 + i) for i in range(num_envs)]
        self.obs_dim = self.battles[0].obs_dim
        self.n_actions = self.battles[0].n_actions
        self._ep_len = np.zeros(num_envs, dtype=np.int32)
        self._resets = np.zeros(num_envs, dtype=np.int64)  # vary the seed across episodes

    def observe(self, side: int) -> tuple[np.ndarray, np.ndarray]:
        """Return (obs[num_envs, obs_dim] float32, mask[num_envs, n_actions] bool) for `side`."""
        obs = np.empty((self.num_envs, self.obs_dim), dtype=np.float32)
        mask = np.empty((self.num_envs, self.n_actions), dtype=bool)
        for i, b in enumerate(self.battles):
            obs[i] = np.asarray(b.observe(side), dtype=np.float32)
            mask[i] = np.asarray(b.legal_actions(side), dtype=bool)
        return obs, mask

    def step(self, red_actions, blue_actions, learner: int = RED):
        """Advance every battle one turn. Returns (reward[num_envs], done[num_envs]) for `learner`."""
        rewards = np.zeros(self.num_envs, dtype=np.float32)
        dones = np.zeros(self.num_envs, dtype=np.float32)
        for i, b in enumerate(self.battles):
            done, winner, _ = b.step(int(red_actions[i]), int(blue_actions[i]))
            self._ep_len[i] += 1
            if not done and self._ep_len[i] >= self.max_turns:
                done, winner = True, -1  # timeout -> draw
            if done:
                rewards[i] = 1.0 if winner == learner else (-1.0 if winner == (1 - learner) else 0.0)
                dones[i] = 1.0
                self._resets[i] += 1
                b.reset(seed=self._base_seed * 100003 + i + self._resets[i] * 7919)
                self._ep_len[i] = 0
        return rewards, dones
