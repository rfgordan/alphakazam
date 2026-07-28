"""Rollout storage + Generalized Advantage Estimation (GAE).

Holds one on-policy batch of `[rollout_steps, num_envs]` transitions as preallocated tensors,
then turns rewards/values/dones into advantages and returns. Everything lives on `device` so
the PPO update reads contiguous tensors with no per-step host<->device copies.
"""

import torch


class RolloutBuffer:
    def __init__(self, steps: int, num_envs: int, obs_dim: int, n_actions: int, device, id_dim: int = 0,
                 aux: bool = False, outcome: bool = False, belief: bool = False):
        self.steps = steps
        self.num_envs = num_envs
        self.device = device
        self.id_dim = id_dim
        self.aux = aux
        self.outcome = outcome
        self.belief = belief

        shape = (steps, num_envs)
        self.obs = torch.zeros(shape + (obs_dim,), device=device)
        # Categorical IDs for embedding (empty when id_dim == 0, e.g. the placeholder env).
        self.obs_ids = torch.zeros(shape + (id_dim,), dtype=torch.long, device=device) if id_dim > 0 else None
        self.masks = torch.ones(shape + (n_actions,), dtype=torch.bool, device=device)
        # Auxiliary-loss labels: opponent's action this turn, and [dmg_self, dmg_opp, ko_self, ko_opp].
        self.opp_actions = torch.zeros(shape, dtype=torch.long, device=device) if aux else None
        self.dyn_targets = torch.zeros(shape + (4,), device=device) if aux else None
        self.actions = torch.zeros(shape, dtype=torch.long, device=device)
        self.log_probs = torch.zeros(shape, device=device)
        self.values = torch.zeros(shape, device=device)
        self.rewards = torch.zeros(shape, device=device)
        self.dones = torch.zeros(shape, device=device)
        # "The learner actually chose here." Under the decision-point env (`flow_env`) a faint
        # replacement or pivot landing is single-sided, so the other side's stored action is a
        # no-op the engine discarded — real transition, no real decision. `ppo_update` masks the
        # policy/entropy terms with this; value regression and GAE run over every step. Defaults
        # to all-ones so the whole-turn envs are unaffected.
        self.active = torch.ones(shape, device=device)
        # Kickstart distillation target (long, -1 = no teacher opinion at this step). Allocated
        # lazily on the first add() that supplies one.
        self.teacher_actions = None

        # Belief-head labels: 11 hidden-identity targets + their is-hidden mask (see
        # pybridge belief_targets_all).
        self.belief_targets = torch.zeros(shape + (11,), dtype=torch.long, device=device) if belief else None
        self.belief_masks = torch.zeros(shape + (11,), device=device) if belief else None

        # Outcome-head channel (E2 branch-B fix): the UNSHAPED terminal reward, the head's own
        # rollout predictions (for bootstrapping), and its λ-returns — a second GAE pass whose
        # targets mean "who wins from here" on the ±1 scale.
        self.outcome_rewards = torch.zeros(shape, device=device) if outcome else None
        self.outcome_values = torch.zeros(shape, device=device) if outcome else None
        self.outcome_returns = torch.zeros(shape, device=device) if outcome else None

        # Filled by compute_gae().
        self.advantages = torch.zeros(shape, device=device)
        self.returns = torch.zeros(shape, device=device)

    def add(self, t: int, obs, mask, action, log_prob, value, reward, done, obs_ids=None,
            opp_action=None, dyn_target=None, active=None, teacher_action=None,
            outcome_reward=None, outcome_value=None, belief_target=None, belief_mask=None):
        self.obs[t] = obs
        self.active[t] = 1.0 if active is None else active
        if self.outcome and outcome_reward is not None:
            self.outcome_rewards[t] = outcome_reward
            self.outcome_values[t] = outcome_value
        if self.belief and belief_target is not None:
            self.belief_targets[t] = belief_target
            self.belief_masks[t] = belief_mask
        if teacher_action is not None:
            if self.teacher_actions is None:
                self.teacher_actions = torch.full((self.steps, self.num_envs), -1,
                                                  dtype=torch.long, device=self.device)
            self.teacher_actions[t] = teacher_action
        if self.obs_ids is not None and obs_ids is not None:
            self.obs_ids[t] = obs_ids
        if self.aux and opp_action is not None:
            self.opp_actions[t] = opp_action
            self.dyn_targets[t] = dyn_target
        self.masks[t] = mask
        self.actions[t] = action
        self.log_probs[t] = log_prob
        self.values[t] = value
        self.rewards[t] = reward
        self.dones[t] = done

    @torch.no_grad()
    def compute_gae(self, last_value, gamma: float, gae_lambda: float):
        """Standard truncated GAE. `last_value` bootstraps the step after the rollout ends.

        `dones[t]` marks that step t *terminated* the episode, so the bootstrap from t+1 is
        cut (auto-reset means t+1 is a new episode and must not leak backward).
        """
        last_gae = torch.zeros(self.num_envs, device=self.device)
        for t in reversed(range(self.steps)):
            next_nonterminal = 1.0 - self.dones[t]
            next_value = last_value if t == self.steps - 1 else self.values[t + 1]
            delta = self.rewards[t] + gamma * next_value * next_nonterminal - self.values[t]
            last_gae = delta + gamma * gae_lambda * next_nonterminal * last_gae
            self.advantages[t] = last_gae
        self.returns = self.advantages + self.values

    @torch.no_grad()
    def compute_gae_outcome(self, last_outcome_value, gamma: float, gae_lambda: float):
        """λ-returns for the outcome head over the UNSHAPED terminal reward — same recursion as
        `compute_gae`, bootstrapped by the outcome head's own predictions."""
        last_gae = torch.zeros(self.num_envs, device=self.device)
        for t in reversed(range(self.steps)):
            next_nonterminal = 1.0 - self.dones[t]
            next_value = last_outcome_value if t == self.steps - 1 else self.outcome_values[t + 1]
            delta = (self.outcome_rewards[t] + gamma * next_value * next_nonterminal
                     - self.outcome_values[t])
            last_gae = delta + gamma * gae_lambda * next_nonterminal * last_gae
            self.outcome_returns[t] = last_gae + self.outcome_values[t]

    def flat_view(self):
        """Flatten [steps, num_envs, ...] -> [steps*num_envs, ...] for minibatching."""
        d = dict(
            obs=self.obs.reshape(-1, self.obs.shape[-1]),
            masks=self.masks.reshape(-1, self.masks.shape[-1]),
            actions=self.actions.reshape(-1),
            log_probs=self.log_probs.reshape(-1),
            values=self.values.reshape(-1),
            advantages=self.advantages.reshape(-1),
            returns=self.returns.reshape(-1),
            active=self.active.reshape(-1),
        )
        if self.obs_ids is not None:
            d["obs_ids"] = self.obs_ids.reshape(-1, self.id_dim)
        if self.aux:
            d["opp_action"] = self.opp_actions.reshape(-1)
            d["dyn_target"] = self.dyn_targets.reshape(-1, 4)
        if self.teacher_actions is not None:
            d["teacher_action"] = self.teacher_actions.reshape(-1)
        if self.outcome:
            d["outcome_return"] = self.outcome_returns.reshape(-1)
        if self.belief:
            d["belief_target"] = self.belief_targets.reshape(-1, 11)
            d["belief_mask"] = self.belief_masks.reshape(-1, 11)
        return d
