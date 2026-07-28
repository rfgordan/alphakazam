"""Vectorized self-play environment over the Rust engine's **decision-point** API (`FlowVec`).

This is the training-grade env: unlike `engine_env.EngineVecEnv` (which drives the legacy
whole-turn `Battle` bridge MDP with 9 actions and a fixed matchup), `FlowVec` advances a battle
request-by-request exactly as Showdown does — real faint replacements, pivot landings as their
own decision, Terastallization in the action space (13 actions), and teams drawn from the pinned
PS random-battle generator pool. `DECISION_POINTS.md` calls the bridge MDP's deviations
disqualifying for anything past P1, so long runs should use this env.

The interface mirrors `EngineVecEnv` (`learner_view` / `opponent_view` / `step`) so the PPO
machinery is shared, with one addition the decision-point model forces:

**Not every side acts at every request.** A faint replacement or a pivot landing is a
*single-sided* decision — the other side submits nothing and the engine ignores whatever action
it was handed. Those steps are real environment transitions (they carry reward and advance time)
but the learner's action at them is a no-op, so attributing an advantage to it would be wrong.
`step()` therefore also returns an `active` mask — "the learner actually acted here" — which the
buffer stores and `ppo_update` uses to zero the policy/entropy terms on inactive steps. Value
regression and GAE still run over every step, because the *dynamics* are real either way.

Observations are egocentric ("me" then "foe"), so one policy plays both sides; the learner is
assigned a random physical side per episode and the reported win-rate averages over both.
"""

from __future__ import annotations

import numpy as np

import showdown_engine as se

RED, BLUE = 0, 1


class FlowEnvVec:
    """`num_envs` independent gen9randombattle battles stepped in lockstep, one request at a time.

    `team_pool` is a path to `harness/gen-team-pool.mjs` output (gzipped JSONL of real PS
    random-battle sets). Without it every env replays the same fixed debug matchup, which is
    useless for training — the launcher always passes the pool.
    """

    def __init__(self, num_envs: int, seed: int = 0, team_pool: str | None = None,
                 max_requests: int = 1000, shaping_coef: float = 0.0, gamma: float = 0.99,
                 fog_species: bool = False):
        self.num_envs = num_envs
        self.shaping_coef = shaping_coef
        self.gamma = gamma
        self.fog_species = fog_species
        self.vec = se.FlowVec(num_envs, seed=seed, max_requests_per_episode=max_requests,
                              team_pool=team_pool, fog_species=fog_species)
        self.obs_dim = self.vec.obs_dim
        self.n_actions = self.vec.n_actions
        self.id_dim = self.vec.id_dim
        self.pool_size = self.vec.pool_size

        # The categorical-ID layout is a property of the encoder, not of a battle; read it off a
        # throwaway `Battle` (FlowVec doesn't re-export the accessors).
        meta = se.Battle(seed=0)
        self.n_mons = meta.n_mons
        self.id_columns = meta.id_columns()
        self.vocab = meta.vocab_sizes()

        self._rng = np.random.default_rng(seed ^ 0xA5A5_5A5A)
        # Which physical side (RED/BLUE) the learner controls in each env this episode.
        self.learner_side = self._rng.integers(0, 2, size=num_envs).astype(np.int64)
        self._cache = None  # per-request encode cache; invalidated by step()

    # ---- views ---------------------------------------------------------------------------

    def _sides(self):
        """Both physical sides' (obs, ids, mask, acting), encoded once per request.

        `FlowVec`'s accessors are per-*side* and encode all N envs in one Rust call, so a full
        step needs exactly two of each. Cached because `learner_view`, `opponent_view` and
        `step` all read the same request — without it each step re-encoded both sides three
        times over, and the encode is the CPU-bound part of the loop.
        """
        if self._cache is None:
            per_side = []
            for s in (RED, BLUE):
                mask = np.asarray(self.vec.legal_all(s), dtype=bool)
                # A non-acting side gets an all-false mask from the engine; the policy still has
                # to produce *something* sampleable, so hand it a uniform mask (the engine
                # discards the action anyway).
                dead = ~mask.any(axis=1)
                if dead.any():
                    mask = mask.copy()
                    mask[dead] = True
                per_side.append((
                    np.asarray(self.vec.observe_all(s), dtype=np.float32),
                    np.asarray(self.vec.observe_ids_all(s), dtype=np.int64),
                    mask,
                    np.asarray(self.vec.acting_all(s), dtype=bool),
                ))
            self._cache = per_side
        return self._cache

    def _view(self, sides: np.ndarray):
        """(obs, ids, mask, acting) for a per-env side selection."""
        red = (sides == RED)[:, None]
        (o_r, i_r, m_r, a_r), (o_b, i_b, m_b, a_b) = self._sides()
        return (np.where(red, o_r, o_b), np.where(red, i_r, i_b),
                np.where(red, m_r, m_b), np.where(red[:, 0], a_r, a_b))

    def learner_view(self):
        return self._view(self.learner_side)

    def opponent_view(self):
        return self._view(1 - self.learner_side)

    # ---- stepping ------------------------------------------------------------------------

    # Faint-count differential weight inside Φ. team_hp is a [0,1] team fraction while faints are
    # counts, so 0.5 makes a full 6-faint sweep worth 3× a full HP lead — the same 3:1
    # faints-to-HP ratio as the proven poke-env recipe (Φ = 0.5·HPdiff + 1.5·faints). A faint
    # already zeroes its mon's HP contribution; this term adds the discontinuity on top, because
    # "barely alive" and "gone" differ by far more than the last sliver of HP.
    FAINT_W = 0.5

    def _potential(self, sides: np.ndarray) -> np.ndarray:
        """Φ = learner-relative team-HP differential + faint differential (the PBRS potential)."""
        red = sides == RED
        hp_r, hp_b = self.vec.team_hp_all(RED), self.vec.team_hp_all(BLUE)
        f_r = np.asarray(self.vec.faints_all(RED), dtype=np.float32)
        f_b = np.asarray(self.vec.faints_all(BLUE), dtype=np.float32)
        mine = np.where(red, hp_r, hp_b)
        theirs = np.where(red, hp_b, hp_r)
        f_mine = np.where(red, f_r, f_b)
        f_theirs = np.where(red, f_b, f_r)
        return ((mine - theirs) + self.FAINT_W * (f_theirs - f_mine)).astype(np.float32)

    def _team_hp_pair(self, sides: np.ndarray):
        red = sides == RED
        mine = np.where(red, self.vec.team_hp_all(RED), self.vec.team_hp_all(BLUE))
        theirs = np.where(red, self.vec.team_hp_all(BLUE), self.vec.team_hp_all(RED))
        f_mine = np.where(red, self.vec.faints_all(RED), self.vec.faints_all(BLUE))
        f_theirs = np.where(red, self.vec.faints_all(BLUE), self.vec.faints_all(RED))
        return mine, theirs, f_mine, f_theirs

    def step(self, learner_actions, opponent_actions):
        """Answer the pending request in every env.

        Returns `(reward, done, active, dyn, outcome)`:
          reward  — sparse ±1/0 at terminal, plus PBRS shaping when `shaping_coef > 0`
          done    — episode ended this step (engine auto-resets in place)
          active  — the learner was actually acting at this request (policy-loss mask)
          dyn     — world-model labels [Δteam_hp_self, Δteam_hp_opp, ko_self, ko_opp]
          outcome — the *true* win/loss (+1/-1/0), for unbiased win-rate logging
        """
        ls = self.learner_side
        _, _, _, acting = self._view(ls)
        shaping = self.shaping_coef > 0.0
        phi_pre = self._potential(ls) if shaping else None
        hp_pre, hpo_pre, f_pre, fo_pre = self._team_hp_pair(ls)

        la = np.asarray(learner_actions, dtype=np.int64)
        oa = np.asarray(opponent_actions, dtype=np.int64)
        red_a = np.where(ls == RED, la, oa)
        blue_a = np.where(ls == RED, oa, la)

        done_np, winner_np = self.vec.step_all(red_a, blue_a, True)
        self._cache = None  # the board moved; every cached encode is stale
        done = np.asarray(done_np, dtype=bool)
        winner = np.asarray(winner_np, dtype=np.int64)

        # `winner` is a physical side index (0/1) or -1 for a draw/truncation.
        true_r = np.where(done & (winner == ls), 1.0,
                          np.where(done & (winner >= 0) & (winner != ls), -1.0, 0.0)).astype(np.float32)

        reward = true_r.copy()
        if shaping:
            # PBRS: the terminal state has potential 0, which preserves policy invariance. Envs
            # that finished were auto-reset in place, so their post-step Φ belongs to a *new*
            # episode and must not be read.
            phi_post = np.where(done, 0.0, self._potential(ls)).astype(np.float32)
            reward = reward + self.shaping_coef * (self.gamma * phi_post - phi_pre)

        hp_post, hpo_post, f_post, fo_post = self._team_hp_pair(ls)
        dyn = np.zeros((self.num_envs, 4), dtype=np.float32)
        live = ~done  # a reset env's post-state is a different battle; leave its labels at 0
        dyn[live, 0] = (hp_post - hp_pre)[live]
        dyn[live, 1] = (hpo_post - hpo_pre)[live]
        dyn[live, 2] = (f_post > f_pre)[live].astype(np.float32)
        dyn[live, 3] = (fo_post > fo_pre)[live].astype(np.float32)

        # Re-roll the learner's side for every env that just started a fresh episode.
        if done.any():
            self.learner_side = np.where(done, self._rng.integers(0, 2, size=self.num_envs), ls)

        return reward, done.astype(np.float32), acting, dyn, true_r
