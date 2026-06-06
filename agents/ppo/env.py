"""Environment interface + a placeholder standing in for the Rust battle engine.

The agent only ever sees a flat observation vector, a boolean action mask, a scalar reward,
and a done flag. That keeps the PPO code completely decoupled from where those come from —
today a learnable toy task, tomorrow `showdown-rs` via an FFI/PyO3 bridge whose observation
is the encoded `State.observe(viewer)` and whose 9 actions are (4 moves + 5 switches).

`SyncVectorEnv` steps several envs in lockstep and auto-resets on episode end, which is all
PPO needs from a vector env.
"""

from __future__ import annotations

from typing import Protocol

import numpy as np


class BattleEnv(Protocol):
    obs_dim: int
    n_actions: int

    def reset(self) -> tuple[np.ndarray, np.ndarray]:
        """Return (obs[obs_dim] float32, action_mask[n_actions] bool)."""

    def step(self, action: int) -> tuple[np.ndarray, np.ndarray, float, bool]:
        """Return (next_obs, next_mask, reward, done)."""


class DummyBattleEnv:
    """A learnable smoke-test task with the exact shape of the real battle interface.

    Each step a fresh random observation is drawn; a fixed (hidden) linear projection
    defines the single "best" action for that observation. Reward is +1 for choosing it and
    -0.1 otherwise, and an episode lasts `episode_len` steps. A working PPO run drives mean
    episodic reward from ~0 (random) toward the optimum, so this doubles as an end-to-end
    test of the training loop. It is **not** Pokémon — it only mimics the I/O contract.
    """

    def __init__(self, obs_dim: int, n_actions: int, episode_len: int = 32, seed: int = 0):
        self.obs_dim = obs_dim
        self.n_actions = n_actions
        self.episode_len = episode_len
        self._rng = np.random.default_rng(seed)
        # Fixed hidden reward structure (shared across episodes; this is what the policy learns).
        self._W = self._rng.standard_normal((obs_dim, n_actions)).astype(np.float32)
        self._t = 0
        self._obs = self._draw_obs()

    def _draw_obs(self) -> np.ndarray:
        self._obs = self._rng.standard_normal(self.obs_dim).astype(np.float32)
        return self._obs

    def _best_action(self) -> int:
        return int(np.argmax(self._obs @ self._W))

    def reset(self) -> tuple[np.ndarray, np.ndarray]:
        self._t = 0
        obs = self._draw_obs()
        mask = np.ones(self.n_actions, dtype=bool)  # all actions legal in the placeholder
        return obs, mask

    def step(self, action: int) -> tuple[np.ndarray, np.ndarray, float, bool]:
        reward = 1.0 if action == self._best_action() else -0.1
        self._t += 1
        done = self._t >= self.episode_len
        obs = self._draw_obs()
        mask = np.ones(self.n_actions, dtype=bool)
        return obs, mask, reward, done


class SyncVectorEnv:
    """Run `num_envs` envs in lockstep; auto-reset any env that finishes its episode.

    `step` returns batched arrays plus the per-env reward/done for *this* transition. When an
    env is done, its returned obs/mask are already those of the freshly reset next episode
    (standard auto-reset), and `done[i]` flags that the bootstrap value must be cut there.
    """

    def __init__(self, make_env, num_envs: int):
        self.envs: list[BattleEnv] = [make_env(i) for i in range(num_envs)]
        self.num_envs = num_envs
        self.obs_dim = self.envs[0].obs_dim
        self.n_actions = self.envs[0].n_actions

    def reset(self) -> tuple[np.ndarray, np.ndarray]:
        obs, masks = zip(*(e.reset() for e in self.envs))
        return np.stack(obs), np.stack(masks)

    def step(self, actions) -> tuple[np.ndarray, np.ndarray, np.ndarray, np.ndarray]:
        obs, masks, rewards, dones = [], [], [], []
        for env, a in zip(self.envs, actions):
            o, m, r, d = env.step(int(a))
            if d:
                o, m = env.reset()  # auto-reset; o/m belong to the next episode
            obs.append(o)
            masks.append(m)
            rewards.append(r)
            dones.append(d)
        return (
            np.stack(obs),
            np.stack(masks),
            np.asarray(rewards, dtype=np.float32),
            np.asarray(dones, dtype=np.float32),
        )


def make_dummy_vector_env(cfg, seed: int) -> SyncVectorEnv:
    return SyncVectorEnv(
        lambda i: DummyBattleEnv(cfg.obs_dim, cfg.n_actions, seed=seed + i),
        cfg.num_envs,
    )
