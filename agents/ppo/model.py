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
import torch.nn.functional as F
from torch.distributions import Categorical

# Finite stand-in for -inf when masking logits (keeps entropy finite for zeroed actions).
MASK_FILL = -1e8

# --- obs-v2 layout offsets (MUST mirror crates/engine/src/encode.rs; asserted in __init__) ----
PER_MON = 35            # hp,faint,active,tera + status7 + types18 + stats5 + level
MOVE_FEATS = 10         # present,pp,disabled,te,stab,bp,cat3,acc
V2_BASE = 643           # v1 OBS_DIM
V2_DIM = 703            # 643 + 28 dmg-calc + 32 mechanics appendix
MON_BLOCK_OFF = 0       # viewer's 6 mons first
ACTIVE_FLAG_IDX = 2     # within a mon block
MY_MOVES_OFF = 560      # viewer's active-move block (4 x MOVE_FEATS)
MY_MOVES_V2_OFF = 643   # per-move [est_dmg, ko] x4 (viewer)
V2_INCOMING_OFF = 659   # per party slot: worst revealed incoming hit
V2_BEST_OFF = 665       # per party slot: own best hit vs foe active
MY_MOVES_MECH_OFF = 671  # per-move [priority, self_boost_total, heal, status] x4 (viewer)


class ActorCritic(nn.Module):
    def __init__(self, obs_dim, n_actions, hidden_dim=928, n_hidden_layers=2,
                 embed: dict | None = None, aux: bool = False, outcome: bool = False,
                 belief: bool = False, setslot: bool = False):
        """`embed`, if given: {n_mons, cols: [table-name per ID column], vocab: {table: size}, dim}.
        `aux=True` adds prediction heads (opponent move + turn dynamics) that share the trunk but
        NOT the policy head — they enrich the representation without biasing the policy.
        `outcome=True` adds a second value head trained on the UNSHAPED terminal outcome —
        "who wins from here" on the ±1 scale, commensurate with terminal leaves. The GAE critic
        stays the PPO baseline; this head exists to be a search evaluator (E2 branch-B fix)."""
        super().__init__()
        self.embed_cfg = embed
        self.has_aux = aux
        self.has_outcome = outcome
        self.has_belief = belief
        self.has_setslot = setslot
        self.n_actions = n_actions
        embed_total = 0
        if embed is not None:
            self.n_mons = embed["n_mons"]
            self.col_tables = list(embed["cols"])  # which table each ID column indexes
            dim = embed["dim"]
            # One embedding table per distinct categorical (columns may share, e.g. the 4 moves).
            # Keys are prefixed because some table names (e.g. "type") collide with nn.Module attrs.
            # `sorted`: the table order fixes this module's parameter order, and Adam's state is
            # keyed by parameter *position*. An unordered vocab mapping (the bridge used to hand
            # back a Rust HashMap) made that order vary per process, so `--resume` loaded the
            # weights fine (state dicts are name-keyed) and then died in the optimizer.
            self.tables = nn.ModuleDict({f"e_{name}": nn.Embedding(size, dim)
                                         for name, size in sorted(embed["vocab"].items())})
            embed_total = self.n_mons * len(self.col_tables) * dim
        input_dim = obs_dim + embed_total

        layers = [nn.Linear(input_dim, hidden_dim), nn.Tanh()]
        for _ in range(n_hidden_layers):
            layers += [nn.Linear(hidden_dim, hidden_dim), nn.Tanh()]
        self.trunk = nn.Sequential(*layers)

        self.policy_head = nn.Linear(hidden_dim, n_actions)
        self.value_head = nn.Linear(hidden_dim, 1)

        if setslot:
            # Slot-shared action scorers (the night-era `setslot` arch, ported): one shared MLP
            # scores each MOVE from [trunk, that move's feature slice, tera?], one scores each
            # SWITCH from [trunk, that bench mon's block, its matchup scalars]. Positional bias
            # is unrepresentable by construction — the flat head's documented failure mode.
            assert obs_dim in (V2_DIM, 2 * V2_DIM), \
                f"setslot expects obs v2 (671 or 1342 with frames), got {obs_dim}"
            assert n_actions == 13
            move_in = hidden_dim + MOVE_FEATS + 2 + 4 + 1  # slice + [dmg,ko] + mechanics + tera
            switch_in = hidden_dim + PER_MON + 2        # mon block + [incoming, best]
            self.move_scorer = nn.Sequential(
                nn.Linear(move_in, 256), nn.Tanh(), nn.Linear(256, 1))
            self.switch_scorer = nn.Sequential(
                nn.Linear(switch_in, 256), nn.Tanh(), nn.Linear(256, 1))

        if aux:
            # Auxiliary *prediction* heads (training-time only). Off the trunk, never the policy
            # head — so they shape the representation without prescribing behavior.
            #   opp: predict the opponent's action this turn — from the state alone.
            #   dyn: predict the turn's outcome [dmg_self, dmg_opp, ko_self, ko_opp] — CONDITIONED
            #        on both players' actions (one-hot), since damage/KO depend on what was done.
            self.aux_opp_head = nn.Linear(hidden_dim, n_actions)
            self.aux_dyn_head = nn.Linear(hidden_dim + 2 * n_actions, 4)

        if outcome:
            self.outcome_head = nn.Linear(hidden_dim, 1)

        if belief:
            # Belief heads (W9): predict the FOE's hidden identity from the public obs — 6 party
            # species + the active's item + its 4 move slots. Supervised from the true state
            # (labels are free during self-play), trained only on HIDDEN entries. Off the trunk,
            # never the policy head: they shape representation and later steer determinization.
            assert embed is not None, "belief heads need the embedding vocab for output sizes"
            v = embed["vocab"]
            self.belief_species = nn.Linear(hidden_dim, 6 * v["species"])
            self.belief_item = nn.Linear(hidden_dim, v["item"])
            self.belief_moves = nn.Linear(hidden_dim, 4 * v["move"])
            self._belief_sizes = (v["species"], v["item"], v["move"])

        self.apply(_orthogonal_init)
        _orthogonal_init(self.policy_head, gain=0.01)
        _orthogonal_init(self.value_head, gain=1.0)
        if outcome:
            _orthogonal_init(self.outcome_head, gain=1.0)

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

    def _trunk_features(self, obs, obs_ids):
        return self.trunk(self._features(obs, obs_ids))

    def _setslot_logits(self, obs, h):
        """Slot-shared scorers over the CURRENT frame's feature slices (first V2_DIM cols)."""
        b = obs.shape[0]
        cur = obs[:, :V2_DIM]
        # Moves: [B, 4, MOVE_FEATS] + v2 [est_dmg, ko] pairs.
        mf = cur[:, MY_MOVES_OFF:MY_MOVES_OFF + 4 * MOVE_FEATS].view(b, 4, MOVE_FEATS)
        dk = cur[:, MY_MOVES_V2_OFF:MY_MOVES_V2_OFF + 8].view(b, 4, 2)
        mech = cur[:, MY_MOVES_MECH_OFF:MY_MOVES_MECH_OFF + 16].view(b, 4, 4)
        moves = torch.cat([mf, dk, mech], dim=2)                 # [B, 4, 16]
        h4 = h.unsqueeze(1).expand(-1, 4, -1)
        zeros = torch.zeros(b, 4, 1, device=obs.device)
        ones = torch.ones(b, 4, 1, device=obs.device)
        move_logits = self.move_scorer(torch.cat([h4, moves, zeros], 2)).squeeze(-1)   # a0..3
        tera_logits = self.move_scorer(torch.cat([h4, moves, ones], 2)).squeeze(-1)    # a9..12
        # Switches: bench = the 5 non-active party slots in order (stable argsort on the
        # active flag reproduces engine bench ordering).
        mons = cur[:, MON_BLOCK_OFF:MON_BLOCK_OFF + 6 * PER_MON].view(b, 6, PER_MON)
        active_flag = mons[:, :, ACTIVE_FLAG_IDX]
        order = torch.argsort(active_flag, dim=1, stable=True)   # non-active first, party order
        bench_idx = order[:, :5]
        bench = torch.gather(mons, 1, bench_idx.unsqueeze(-1).expand(-1, -1, PER_MON))
        inc = torch.gather(cur[:, V2_INCOMING_OFF:V2_INCOMING_OFF + 6], 1, bench_idx)
        bst = torch.gather(cur[:, V2_BEST_OFF:V2_BEST_OFF + 6], 1, bench_idx)
        h5 = h.unsqueeze(1).expand(-1, 5, -1)
        sw = torch.cat([h5, bench, inc.unsqueeze(-1), bst.unsqueeze(-1)], 2)
        switch_logits = self.switch_scorer(sw).squeeze(-1)                              # a4..8
        return torch.cat([move_logits, switch_logits, tera_logits], dim=1)

    def _logits(self, obs, h):
        return self._setslot_logits(obs, h) if self.has_setslot else self.policy_head(h)

    def forward(self, obs, action_mask=None, obs_ids=None):
        """Return (logits, value). `action_mask` [..., n_actions] bool (True = legal).

        When the outcome head exists, its prediction is left on `self.outcome_pred` (same
        stash pattern as `teacher_log_prob`) — callers that want it read it after the call.
        """
        h = self._trunk_features(obs, obs_ids)
        logits = self._logits(obs, h)
        if action_mask is not None:
            logits = logits.masked_fill(~action_mask, MASK_FILL)
        value = self.value_head(h).squeeze(-1)
        self.outcome_pred = self.outcome_head(h).squeeze(-1) if self.has_outcome else None
        return logits, value

    def act(self, obs, action_mask=None, action=None, obs_ids=None, return_aux=False,
            teacher_action=None):
        """Sample (or evaluate a given) action. Returns (action, log_prob, entropy, value[, aux]).

        `teacher_action` (long, -1 = no teacher): also compute the policy's log-prob of the
        teacher's action — the kickstart distillation term. Invalid rows evaluate at index 0 and
        must be masked out by the caller (they carry no gradient there anyway once masked).
        """
        h = self._trunk_features(obs, obs_ids)
        logits = self._logits(obs, h)
        if action_mask is not None:
            logits = logits.masked_fill(~action_mask, MASK_FILL)
        value = self.value_head(h).squeeze(-1)
        dist = Categorical(logits=logits)
        if action is None:
            action = dist.sample()
        self.teacher_log_prob = None
        if teacher_action is not None:
            self.teacher_log_prob = dist.log_prob(teacher_action.clamp(min=0))
        self.outcome_pred = self.outcome_head(h).squeeze(-1) if self.has_outcome else None
        self.trunk_h = h if self.has_belief else None
        self.log_probs_full = torch.log_softmax(logits, dim=-1)  # soft-target distillation reads this
        out = (action, dist.log_prob(action), dist.entropy(), value)
        if return_aux:
            # Return the trunk features so the dynamics head can be conditioned on the actions
            # (which we only have in the update loop). `opp` predicts the opponent move from state.
            aux = {"opp": self.aux_opp_head(h), "h": h}
            return (*out, aux)
        return out

    def belief_logits(self, h):
        """(species [B,6,Vs], item [B,Vi], moves [B,4,Vm]) from trunk features."""
        vs, vi, vm = self._belief_sizes
        return (self.belief_species(h).view(-1, 6, vs),
                self.belief_item(h),
                self.belief_moves(h).view(-1, 4, vm))

    def predict_dynamics(self, h, a_self, a_opp):
        """World-model prediction [hp_delta_self, hp_delta_opp, ko_self, ko_opp] conditioned on both
        actions. Deltas are signed (tanh -> [-1,1]); KO are logits."""
        a1 = F.one_hot(a_self, self.n_actions).float()
        a2 = F.one_hot(a_opp, self.n_actions).float()
        return self.aux_dyn_head(torch.cat([h, a1, a2], dim=-1))

    def num_params(self) -> int:
        return sum(p.numel() for p in self.parameters())


def _orthogonal_init(module, gain: float = 2 ** 0.5):
    if isinstance(module, nn.Linear):
        nn.init.orthogonal_(module.weight, gain=gain)
        nn.init.zeros_(module.bias)
