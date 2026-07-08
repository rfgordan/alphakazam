"""Slot-equivariant actor-critic for the poke-env 26-action space.

Why: a flat MLP policy head must independently learn, per move slot, that logit_i should track the
quality features of move i — and cold-start RL demonstrably fails at this (probe on the from-scratch
checkpoint: chosen slot tracks argmax-damage 19.8% — below uniform; logit/damage Spearman ~0; a
63%-slot-3 positional bias). This head shares ONE scorer across move slots and ONE across team
slots, so a positional prior is unrepresentable: only slot *content* can move a logit.

Action mapping (poke-env singles): 0-5 switch to team slot j; 6-9 move slot i; 22-25 move slot i
+ tera; 10-21 (mega/z/dyna) never legal in gen9 — constant logits, always masked.

Same interface as ActorCritic (forward/act), fewer params (~136K vs ~178K at hidden 256).
"""

from __future__ import annotations

import torch
import torch.nn as nn
from torch.distributions import Categorical

from .model import MASK_FILL, _orthogonal_init
from .pokeenv_env import MOVE_SLOT_IDX, SWITCH_SLOT_IDX, TRUNK_IDX, OBS_DIM as _BASE_OBS

N_MOVE_FEATS = len(MOVE_SLOT_IDX[0])       # 25 (v3: 6 base + 18 type one-hot + bench_eff)
N_SWITCH_FEATS = len(SWITCH_SLOT_IDX[0])   # 24 (v3: 3 base + 18 types + 3 matchup scalars)


class SlotActorCritic(nn.Module):
    def __init__(self, obs_dim, n_actions=26, hidden_dim=240, n_hidden_layers=2,
                 ctx_dim=48, scorer_dim=64, set_context=False, set_dim=32, frames=1):
        super().__init__()
        assert n_actions == 26
        # frames>1: obs is [current, prev, ...] stacked; scorers read the CURRENT frame's slots,
        # the trunk additionally reads each previous frame's TRUNK_IDX subset (short-term memory).
        self.frames = frames
        self.n_actions = n_actions
        # set_context: DeepSets-style pooled slot embeddings — mean over per-move and per-team-slot
        # embeddings — appended to the trunk input and to each scorer. Gives every decision
        # cross-slot COMBINATION information (e.g. "boost move + sweeper coverage exists")
        # while staying position-invariant (pooling is permutation-invariant by construction).
        self.set_context = set_context
        self.set_dim = set_dim if set_context else 0
        # The trunk reads TRUNK_IDX (everything except per-slot type one-hots — those feed only
        # the shared scorers), so the wide one-hot blocks don't pay the per-dim trunk cost.
        t_idx = list(TRUNK_IDX)
        for f in range(1, frames):
            t_idx += [f * _BASE_OBS + i for i in TRUNK_IDX]
        self.register_buffer("trunk_idx", torch.tensor(t_idx, dtype=torch.long))
        if set_context:
            self.move_embed = nn.Linear(N_MOVE_FEATS, set_dim)
            self.team_embed = nn.Linear(N_SWITCH_FEATS, set_dim)
        layers = [nn.Linear(len(t_idx) + 2 * self.set_dim, hidden_dim), nn.Tanh()]
        for _ in range(n_hidden_layers):
            layers += [nn.Linear(hidden_dim, hidden_dim), nn.Tanh()]
        self.trunk = nn.Sequential(*layers)
        self.ctx = nn.Linear(hidden_dim, ctx_dim)
        # Shared scorers: input = [slot features, is_tera (moves only), context(, pooled set)]
        self.move_scorer = nn.Sequential(
            nn.Linear(N_MOVE_FEATS + 1 + ctx_dim + self.set_dim, scorer_dim), nn.Tanh(),
            nn.Linear(scorer_dim, 1))
        self.switch_scorer = nn.Sequential(
            nn.Linear(N_SWITCH_FEATS + ctx_dim + self.set_dim, scorer_dim), nn.Tanh(),
            nn.Linear(scorer_dim, 1))
        self.value_head = nn.Linear(hidden_dim, 1)
        self.register_buffer("move_idx", torch.tensor(MOVE_SLOT_IDX, dtype=torch.long))
        self.register_buffer("switch_idx", torch.tensor(SWITCH_SLOT_IDX, dtype=torch.long))
        self.apply(_orthogonal_init)
        _orthogonal_init(self.move_scorer[-1], gain=0.01)
        _orthogonal_init(self.switch_scorer[-1], gain=0.01)
        _orthogonal_init(self.value_head, gain=1.0)

    def _logits_value(self, obs):
        mf = obs[:, self.move_idx]                                     # [B, 4, n_move_feats]
        sf = obs[:, self.switch_idx]                                   # [B, 6, n_switch_feats]
        trunk_in = obs[:, self.trunk_idx]
        if self.set_context:
            pm = torch.tanh(self.move_embed(mf)).mean(dim=1)           # [B, set] pooled moves
            pt = torch.tanh(self.team_embed(sf)).mean(dim=1)           # [B, set] pooled team
            trunk_in = torch.cat([trunk_in, pm, pt], dim=-1)
        h = self.trunk(trunk_in)
        c = torch.tanh(self.ctx(h))                                   # [B, ctx]
        c4 = c.unsqueeze(1).expand(-1, 4, -1)
        if self.set_context:
            c4 = torch.cat([c4, pm.unsqueeze(1).expand(-1, 4, -1)], -1)
        flag0 = torch.zeros(mf.shape[0], 4, 1, device=obs.device)
        base = self.move_scorer(torch.cat([mf, flag0, c4], -1)).squeeze(-1)       # [B, 4]
        tera = self.move_scorer(torch.cat([mf, flag0 + 1.0, c4], -1)).squeeze(-1)  # [B, 4]
        c6 = c.unsqueeze(1).expand(-1, 6, -1)
        if self.set_context:
            c6 = torch.cat([c6, pt.unsqueeze(1).expand(-1, 6, -1)], -1)
        sw = self.switch_scorer(torch.cat([sf, c6], -1)).squeeze(-1)   # [B, 6]
        dead = torch.zeros(mf.shape[0], 12, device=obs.device)         # actions 10-21, never legal
        logits = torch.cat([sw, base, dead, tera], dim=-1)             # [B, 26]
        return logits, self.value_head(h).squeeze(-1)

    def forward(self, obs, action_mask=None, obs_ids=None):
        logits, value = self._logits_value(obs)
        if action_mask is not None:
            logits = logits.masked_fill(~action_mask, MASK_FILL)
        return logits, value

    def act(self, obs, action_mask=None, action=None, obs_ids=None, return_aux=False):
        logits, value = self.forward(obs, action_mask)
        dist = Categorical(logits=logits)
        if action is None:
            action = dist.sample()
        return action, dist.log_prob(action), dist.entropy(), value

    def num_params(self) -> int:
        return sum(p.numel() for p in self.parameters())


# ---- v2 -> v3 checkpoint transfer -------------------------------------------------------------------
_V2_TRUNK_IN, _V2_MOVE_FEATS, _V2_SWITCH_FEATS, _V2_CTX = 173, 6, 3, 48


def transfer_v2_state_dict(old_sd: dict, model: "SlotActorCritic") -> dict:
    """Map a v2-era (173-dim obs) SlotActorCritic state dict onto the v3 layout.

    v3 is append-only: old features keep their positions (and lead TRUNK_IDX), new columns are
    zero-initialized — so the transferred model computes EXACTLY the same function as the v2
    checkpoint until training moves the new weights.
    """
    new_sd = {k: v.clone() for k, v in model.state_dict().items()}
    for k, old in old_sd.items():
        if k not in new_sd or k in ("move_idx", "switch_idx", "trunk_idx"):
            continue   # index buffers: keep the new model's own (v3) values
        new = new_sd[k]
        if old.shape == new.shape:
            new_sd[k] = old.clone()
        elif k == "trunk.0.weight":
            new_z = torch.zeros_like(new)
            new_z[:, :_V2_TRUNK_IN] = old          # v2 dims are the TRUNK_IDX prefix
            new_sd[k] = new_z
        elif k == "move_scorer.0.weight":
            # v2 input: [feats(6), is_tera, ctx(48)]  ->  v3: [feats(25), is_tera, ctx(48)]
            new_z = torch.zeros_like(new)
            new_z[:, :_V2_MOVE_FEATS] = old[:, :_V2_MOVE_FEATS]
            new_z[:, N_MOVE_FEATS] = old[:, _V2_MOVE_FEATS]                       # is_tera
            new_z[:, N_MOVE_FEATS + 1:] = old[:, _V2_MOVE_FEATS + 1:]             # ctx
            new_sd[k] = new_z
        elif k == "switch_scorer.0.weight":
            # v2 input: [feats(3), ctx(48)]  ->  v3: [feats(24), ctx(48)]
            new_z = torch.zeros_like(new)
            new_z[:, :_V2_SWITCH_FEATS] = old[:, :_V2_SWITCH_FEATS]
            new_z[:, N_SWITCH_FEATS:] = old[:, _V2_SWITCH_FEATS:]                 # ctx
            new_sd[k] = new_z
        else:
            raise ValueError(f"unexpected shape mismatch for {k}: {old.shape} vs {new.shape}")
    return new_sd
