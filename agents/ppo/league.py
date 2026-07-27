"""Frozen-checkpoint league for the Rust decision-point trainer, with PFSP opponent sampling.

Training only against the *most recent* frozen self is the classic self-play failure mode: the
pair co-adapts, the policy forgets how to beat anything it stopped seeing, and the win-rate against
that one opponent equilibrates to ~0.5 while telling you nothing. A league keeps a reservoir of
past checkpoints and keeps drawing from all of it.

The weighting is deliberately the same as `fleet.py`'s `OpponentPool` (the poke-env path's proven
implementation) so results stay comparable across the two trainers:

    hard      w ∝ base * (1 - wr + 0.1)^p          exploit-the-gap: time flows to whoever beats us
    frontier  w ∝ base * (wr * (1 - wr) + 0.1)^p   learnable-first: contested opponents get the time

What differs is the mechanics, because this env is vectorized: opponents are policy *networks*
rather than poke-env `Player`s, and every env steps in lockstep. Rather than a forward pass per
env, the envs are partitioned into `n_slots` blocks and each block is played by one sampled
opponent — so a rollout costs `n_slots` opponent forwards instead of one, for `n_slots` distinct
opponents per rollout. Slots are resampled every rollout, so over an update the learner sees a
fresh draw from the reservoir.
"""

from __future__ import annotations

import random as pyrandom
from collections import deque
from pathlib import Path

import numpy as np
import torch


class SnapshotLeague:
    """A reservoir of frozen self checkpoints plus the scripted `random` opponent.

    `pool_dir` holds `snap_<step>.pt` state dicts. `keep` bounds the reservoir (oldest pruned) —
    unbounded would be better league-theoretically but the disk and the sampling both get silly.
    """

    RANDOM = "random"

    def __init__(self, pool_dir, keep: int = 20, pfsp_power: float = 2.0,
                 window: int = 512, mode: str = "frontier", random_weight: float = 0.25,
                 scripted_weights: dict[str, float] | None = None):
        self.pool_dir = Path(pool_dir)
        self.pool_dir.mkdir(parents=True, exist_ok=True)
        self.keep = keep
        self.pfsp_power = pfsp_power
        self.mode = mode
        self.random_weight = random_weight
        # Scripted (non-checkpoint) opponents beyond `random`, key -> base weight. The proven
        # poke-env curriculum trained against the heuristic directly (base weight 2 vs self 0.5)
        # — win-rate against an opponent the agent never faces in training is pure transfer, and
        # scale1 showed how slow that is (0.18 vs heuristic at 26M steps). Keys here must have a
        # matching callable in `OpponentSlots.scripted`.
        self.scripted_weights = dict(scripted_weights or {})
        self.results: dict[str, deque] = {}
        self._window = window

    # ---- reservoir ------------------------------------------------------------------------

    def snapshots(self) -> list[Path]:
        return sorted(self.pool_dir.glob("snap_*.pt"))

    def add(self, model, step: int):
        torch.save(model.state_dict(), self.pool_dir / f"snap_{step:012d}.pt")
        for old in self.snapshots()[:-self.keep] if self.keep > 0 else []:
            old.unlink(missing_ok=True)
            self.results.pop(old.name, None)

    # ---- PFSP -----------------------------------------------------------------------------

    def _win(self, key: str) -> float:
        d = self.results.get(key)
        return sum(d) / len(d) if d else 0.5

    def _pfsp(self, key: str) -> float:
        wr = self._win(key)
        if self.mode == "frontier":
            return (wr * (1.0 - wr) + 0.1) ** self.pfsp_power
        return (1.0 - wr + 0.1) ** self.pfsp_power

    def weights(self) -> dict[str, float]:
        w: dict[str, float] = {p.name: self._pfsp(p.name) for p in self.snapshots()}
        # `random` is weighted by the SAME PFSP term as any other opponent, scaled by
        # `random_weight`. Applying it as a flat share instead (which is what this did first) does
        # not decay as the agent masters random: with one snapshot in the pool and a 0.94 win rate
        # against random, a flat 0.25 worked out to 67% of all training spent on an opponent the
        # agent already beats 19 times in 20 — almost pure wasted gradient. Under PFSP the same
        # 0.25 base lands near 5%, and shrinks further as the reservoir grows.
        if self.random_weight > 0 or not w:
            w[self.RANDOM] = (self.random_weight * self._pfsp(self.RANDOM)) if w else 1.0
        for k, base in self.scripted_weights.items():
            if base > 0:
                w[k] = base * self._pfsp(k)
        tot = sum(w.values())
        return {k: v / tot for k, v in w.items()} if tot > 0 else {self.RANDOM: 1.0}

    def sample(self, n: int) -> list[str]:
        w = self.weights()
        keys = list(w)
        p = np.array([w[k] for k in keys], dtype=np.float64)
        p /= p.sum()
        return [keys[i] for i in np.random.choice(len(keys), size=n, p=p)]

    def record(self, key: str, won: float):
        """`won` in {1.0 win, 0.0 loss, 0.5 draw} from the LEARNER's perspective."""
        self.results.setdefault(key, deque(maxlen=self._window)).append(float(won))

    def load_state(self, saved: dict):
        for k, vals in (saved or {}).items():
            self.results[k] = deque(vals, maxlen=self._window)

    def save_state(self) -> dict:
        return {k: list(v) for k, v in self.results.items()}

    def stats(self) -> dict:
        w = self.weights()
        return {k: {"wr": round(self._win(k), 3), "n": len(self.results.get(k, [])),
                    "w": round(w.get(k, 0.0), 3)} for k in w}


class OpponentSlots:
    """`n_slots` loaded opponent networks; env `i` is played by slot `i % n_slots`.

    Holding the networks resident and only swapping *weights* keeps resampling cheap — a rollout
    reassignment is `keep` small state-dict loads, not `num_envs` of them.
    """

    def __init__(self, n_slots: int, make_net, device, scripted: dict | None = None):
        self.n = n_slots
        self.device = device
        # Scripted opponents: key -> callable `(vec, envs, mask_rows, rng) -> actions`, where
        # `envs` is [(env_idx, side), ...] — the signature `flow_eval._heuristic_actions` has.
        self.scripted = dict(scripted or {})
        self.nets = []
        for _ in range(n_slots):
            net = make_net()
            net.eval()
            for prm in net.parameters():
                prm.requires_grad_(False)
            self.nets.append(net)
        self.keys = [SnapshotLeague.RANDOM] * n_slots

    def assign(self, league: SnapshotLeague, fallback_state_dict):
        """Resample each slot's opponent. Scripted slots (`random`, heuristic, …) load no net."""
        self.keys = league.sample(self.n)
        for i, k in enumerate(self.keys):
            if k == SnapshotLeague.RANDOM or k in self.scripted:
                continue
            path = league.pool_dir / k
            try:
                self.nets[i].load_state_dict(torch.load(path, map_location=self.device,
                                                        weights_only=True))
            except Exception as e:
                # A corrupt or half-written snapshot must never take down a multi-day run; fall
                # back to the current policy's weights and mark the slot as `random`-keyed so the
                # bad file stops attracting PFSP weight.
                print(f"[league] snapshot {k} failed to load ({type(e).__name__}: {e}); "
                      f"slot {i} falls back to the live policy")
                self.nets[i].load_state_dict(fallback_state_dict)
                self.keys[i] = SnapshotLeague.RANDOM

    def env_key(self, env_idx: int) -> str:
        return self.keys[env_idx % self.n]

    @torch.no_grad()
    def actions(self, obs, ids, mask, rng: np.random.Generator,
                vec=None, sides=None) -> np.ndarray:
        """Opponent action for every env, one forward per slot over that slot's env block.

        `vec`/`sides` (the engine vector and each env's opponent side) are only needed when a
        scripted opponent is in the league — it plays from the engine's true state, not the
        encoded obs.
        """
        n_envs = obs.shape[0]
        out = np.zeros(n_envs, dtype=np.int64)
        idx = np.arange(n_envs)
        for s in range(self.n):
            sel = idx[idx % self.n == s]
            if sel.size == 0:
                continue
            if self.keys[s] == SnapshotLeague.RANDOM:
                m = mask[sel]
                out[sel] = (rng.random(m.shape) * m).argmax(axis=1)
                continue
            if self.keys[s] in self.scripted:
                envs = [(int(e), int(sides[e])) for e in sel]
                out[sel] = self.scripted[self.keys[s]](vec, envs, mask[sel], rng)
                continue
            logits, _ = self.nets[s].forward(
                torch.as_tensor(obs[sel], device=self.device),
                torch.as_tensor(mask[sel], device=self.device),
                obs_ids=torch.as_tensor(ids[sel], device=self.device))
            out[sel] = logits.argmax(dim=-1).cpu().numpy()
        return out
