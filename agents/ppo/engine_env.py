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
        b0 = self.battles[0]
        self.obs_dim = b0.obs_dim
        self.n_actions = b0.n_actions
        # Categorical-ID layout for the model's embedding tables.
        self.id_dim = b0.id_dim
        self.n_mons = b0.n_mons
        self.id_columns = b0.id_columns()
        self.vocab = b0.vocab_sizes()
        self._ep_len = np.zeros(num_envs, dtype=np.int32)
        self._resets = np.zeros(num_envs, dtype=np.int64)
        self._rng = np.random.default_rng(seed ^ 0xA5A5_5A5A)
        # Which physical side (RED/BLUE) the learner controls in each env this episode.
        self.learner_side = self._rng.integers(0, 2, size=num_envs).astype(np.int64)

    def _view(self, sides: np.ndarray) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
        obs = np.empty((self.num_envs, self.obs_dim), dtype=np.float32)
        ids = np.empty((self.num_envs, self.id_dim), dtype=np.int64)
        mask = np.empty((self.num_envs, self.n_actions), dtype=bool)
        for i, b in enumerate(self.battles):
            s = int(sides[i])
            obs[i] = np.asarray(b.observe(s), dtype=np.float32)
            ids[i] = np.asarray(b.observe_ids(s), dtype=np.int64)
            mask[i] = np.asarray(b.legal_actions(s), dtype=bool)
        return obs, ids, mask

    def learner_view(self) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
        """(obs, ids, mask) from the learner's perspective in each env."""
        return self._view(self.learner_side)

    def opponent_view(self) -> tuple[np.ndarray, np.ndarray, np.ndarray]:
        """(obs, ids, mask) from the opponent's perspective in each env."""
        return self._view(1 - self.learner_side)

    def step(self, learner_actions, opponent_actions):
        """Advance every battle. Returns (reward[N], done[N], dyn[N,4]) for the learner.

        `dyn` is the world-model auxiliary label for the turn just played, learner-relative:
        [signed HP-Δ of my active, signed HP-Δ of foe's active, my active KO'd, foe active KO'd].
        The HP deltas are signed fractions (negative = took damage, positive = healed) so the world
        model learns healing/residuals too, not just damage. Read from the specific active mons (by
        slot) *before* any auto-reset.
        """
        rewards = np.zeros(self.num_envs, dtype=np.float32)
        dones = np.zeros(self.num_envs, dtype=np.float32)
        dyn = np.zeros((self.num_envs, 4), dtype=np.float32)
        for i, b in enumerate(self.battles):
            ls = int(self.learner_side[i])
            opp_s = 1 - ls
            # Pre-turn HP of each side's active (read the same slot after the turn).
            ai = (b.active_index(0), b.active_index(1))
            pre = (b.hp_fraction(0, ai[0]), b.hp_fraction(1, ai[1]))

            la, oa = int(learner_actions[i]), int(opponent_actions[i])
            red_a, blue_a = (la, oa) if ls == RED else (oa, la)
            done, winner, _ = b.step(red_a, blue_a)

            post = (b.hp_fraction(0, ai[0]), b.hp_fraction(1, ai[1]))
            dyn[i, 0] = post[ls] - pre[ls]        # my active's signed HP-Δ (neg = damage, pos = heal)
            dyn[i, 1] = post[opp_s] - pre[opp_s]  # foe active's signed HP-Δ
            dyn[i, 2] = 1.0 if pre[ls] > 0 and post[ls] <= 0 else 0.0      # my active fainted
            dyn[i, 3] = 1.0 if pre[opp_s] > 0 and post[opp_s] <= 0 else 0.0  # foe active fainted

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
        return rewards, dones, dyn
