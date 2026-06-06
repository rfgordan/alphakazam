"""Rollout storage + Generalized Advantage Estimation (GAE).

Holds one on-policy batch of `[rollout_steps, num_envs]` transitions as preallocated tensors,
then turns rewards/values/dones into advantages and returns. Everything lives on `device` so
the PPO update reads contiguous tensors with no per-step host<->device copies.
"""

import torch


class RolloutBuffer:
    def __init__(self, steps: int, num_envs: int, obs_dim: int, n_actions: int, device, id_dim: int = 0):
        self.steps = steps
        self.num_envs = num_envs
        self.device = device
        self.id_dim = id_dim

        shape = (steps, num_envs)
        self.obs = torch.zeros(shape + (obs_dim,), device=device)
        # Categorical IDs for embedding (empty when id_dim == 0, e.g. the placeholder env).
        self.obs_ids = torch.zeros(shape + (id_dim,), dtype=torch.long, device=device) if id_dim > 0 else None
        self.masks = torch.ones(shape + (n_actions,), dtype=torch.bool, device=device)
        self.actions = torch.zeros(shape, dtype=torch.long, device=device)
        self.log_probs = torch.zeros(shape, device=device)
        self.values = torch.zeros(shape, device=device)
        self.rewards = torch.zeros(shape, device=device)
        self.dones = torch.zeros(shape, device=device)

        # Filled by compute_gae().
        self.advantages = torch.zeros(shape, device=device)
        self.returns = torch.zeros(shape, device=device)

    def add(self, t: int, obs, mask, action, log_prob, value, reward, done, obs_ids=None):
        self.obs[t] = obs
        if self.obs_ids is not None and obs_ids is not None:
            self.obs_ids[t] = obs_ids
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
        )
        if self.obs_ids is not None:
            d["obs_ids"] = self.obs_ids.reshape(-1, self.id_dim)
        return d
