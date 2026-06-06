"""The policy/value network.

A deliberately small, readable actor-critic: a shared MLP trunk feeding two linear heads
(a policy head over the 9 discrete actions, and a scalar value head). Action masking is
baked in so illegal actions (no-PP moves, switching to a fainted/active mon) get zero
probability.

At obs_dim=128, hidden_dim=672, n_hidden_layers=2 this is ~1.0M parameters — call
`ActorCritic(...).num_params()` to print the exact count.
"""

from __future__ import annotations

import torch
import torch.nn as nn
from torch.distributions import Categorical

# Finite stand-in for -inf when masking logits: large enough that softmax gives ~0
# probability, but finite so entropy (p * log p) stays 0 instead of becoming NaN.
MASK_FILL = -1e8


class ActorCritic(nn.Module):
    def __init__(self, obs_dim: int, n_actions: int, hidden_dim: int = 672, n_hidden_layers: int = 2):
        super().__init__()

        layers = [nn.Linear(obs_dim, hidden_dim), nn.Tanh()]
        for _ in range(n_hidden_layers):
            layers += [nn.Linear(hidden_dim, hidden_dim), nn.Tanh()]
        self.trunk = nn.Sequential(*layers)

        self.policy_head = nn.Linear(hidden_dim, n_actions)
        self.value_head = nn.Linear(hidden_dim, 1)

        # Orthogonal init is the PPO default; a tiny gain on the policy head keeps the
        # initial action distribution near-uniform so early exploration is unbiased.
        self.apply(_orthogonal_init)
        _orthogonal_init(self.policy_head, gain=0.01)
        _orthogonal_init(self.value_head, gain=1.0)

    def forward(self, obs: torch.Tensor, action_mask: torch.Tensor | None = None):
        """Return (logits, value). `action_mask` is a bool tensor [..., n_actions]; True = legal."""
        h = self.trunk(obs)
        logits = self.policy_head(h)
        if action_mask is not None:
            logits = logits.masked_fill(~action_mask, MASK_FILL)
        value = self.value_head(h).squeeze(-1)
        return logits, value

    def act(self, obs, action_mask=None, action=None):
        """Sample (or evaluate a given) action. Returns (action, log_prob, entropy, value)."""
        logits, value = self.forward(obs, action_mask)
        dist = Categorical(logits=logits)
        if action is None:
            action = dist.sample()
        return action, dist.log_prob(action), dist.entropy(), value

    def num_params(self) -> int:
        return sum(p.numel() for p in self.parameters())


def _orthogonal_init(module, gain: float = 2 ** 0.5):
    if isinstance(module, nn.Linear):
        nn.init.orthogonal_(module.weight, gain=gain)
        nn.init.zeros_(module.bias)
