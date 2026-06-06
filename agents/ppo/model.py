"""The policy/value network.

A small actor-critic: optional **embedding tables** for the high-cardinality categorical IDs
(species, ability, item, tera type, moves) concatenated with the numeric float observation, then
a shared MLP trunk feeding a masked policy head (9 actions) and a scalar value head.

`embed=None` gives a pure-float MLP (used by the placeholder env). With the engine's `embed` spec
(vocab sizes + per-column table layout from the bridge) and the default config it is ~5M params —
call `ActorCritic(...).num_params()`.
"""

from __future__ import annotations

import torch
import torch.nn as nn
from torch.distributions import Categorical

# Finite stand-in for -inf when masking logits (keeps entropy finite for zeroed actions).
MASK_FILL = -1e8


class ActorCritic(nn.Module):
    def __init__(self, obs_dim, n_actions, hidden_dim=928, n_hidden_layers=2, embed: dict | None = None):
        """`embed`, if given: {n_mons, cols: [table-name per ID column], vocab: {table: size}, dim}."""
        super().__init__()
        self.embed_cfg = embed
        embed_total = 0
        if embed is not None:
            self.n_mons = embed["n_mons"]
            self.col_tables = list(embed["cols"])  # which table each ID column indexes
            dim = embed["dim"]
            # One embedding table per distinct categorical (columns may share, e.g. the 4 moves).
            # Keys are prefixed because some table names (e.g. "type") collide with nn.Module attrs.
            self.tables = nn.ModuleDict({f"e_{name}": nn.Embedding(size, dim) for name, size in embed["vocab"].items()})
            embed_total = self.n_mons * len(self.col_tables) * dim
        input_dim = obs_dim + embed_total

        layers = [nn.Linear(input_dim, hidden_dim), nn.Tanh()]
        for _ in range(n_hidden_layers):
            layers += [nn.Linear(hidden_dim, hidden_dim), nn.Tanh()]
        self.trunk = nn.Sequential(*layers)

        self.policy_head = nn.Linear(hidden_dim, n_actions)
        self.value_head = nn.Linear(hidden_dim, 1)

        self.apply(_orthogonal_init)
        _orthogonal_init(self.policy_head, gain=0.01)
        _orthogonal_init(self.value_head, gain=1.0)

    def _embed(self, obs_ids: torch.Tensor) -> torch.Tensor:
        """obs_ids [B, n_mons*n_cols] (long) -> flat embedding features [B, n_mons*n_cols*dim]."""
        b = obs_ids.shape[0]
        ncol = len(self.col_tables)
        ids = obs_ids.view(b, self.n_mons, ncol)
        cols = [self.tables[f"e_{table}"](ids[:, :, c]) for c, table in enumerate(self.col_tables)]  # each [B, n_mons, dim]
        return torch.stack(cols, dim=2).reshape(b, -1)

    def _features(self, obs, obs_ids):
        if self.embed_cfg is None:
            return obs
        return torch.cat([obs, self._embed(obs_ids)], dim=-1)

    def forward(self, obs, action_mask=None, obs_ids=None):
        """Return (logits, value). `action_mask` [..., n_actions] bool (True = legal)."""
        h = self.trunk(self._features(obs, obs_ids))
        logits = self.policy_head(h)
        if action_mask is not None:
            logits = logits.masked_fill(~action_mask, MASK_FILL)
        value = self.value_head(h).squeeze(-1)
        return logits, value

    def act(self, obs, action_mask=None, action=None, obs_ids=None):
        """Sample (or evaluate a given) action. Returns (action, log_prob, entropy, value)."""
        logits, value = self.forward(obs, action_mask, obs_ids)
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
