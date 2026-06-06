"""Fixed reference opponents + a generic periodic evaluation.

Unlike the *rolling* snapshot used for training (which only shows marginal improvement over a
near-equal recent self), an evaluation against a **fixed** baseline shows *absolute* progress.
The `Baseline` protocol is deliberately tiny — `actions(obs, mask) -> np.ndarray` — so anything
can be a baseline: a frozen anchor checkpoint, a random bot, a hand-written heuristic, or a
specific past model. `evaluate()` plays the learner (greedily) against any baseline over both
sides and reports a win-rate.
"""

from __future__ import annotations

from typing import Protocol

import numpy as np
import torch

from .engine_env import EngineVecEnv


class Baseline(Protocol):
    name: str

    def actions(self, obs: np.ndarray, ids: np.ndarray, mask: np.ndarray, battles=None, sides=None) -> np.ndarray:
        """Batched action indices for a stack of rows. `battles`/`sides` (the raw engine `Battle`
        handles and the side this baseline controls per env) are provided for baselines that need
        the full state (e.g. an external search); policy/random baselines ignore them."""


@torch.no_grad()
def greedy_actions(net, obs_np, ids_np, mask_np, device) -> np.ndarray:
    """Argmax action under a network's masked policy (used for the learner at eval and for any
    policy-backed baseline / the greedy training opponent)."""
    obs = torch.as_tensor(obs_np, device=device)
    ids = torch.as_tensor(ids_np, device=device)
    mask = torch.as_tensor(mask_np, device=device)
    logits, _ = net.forward(obs, mask, obs_ids=ids)
    return logits.argmax(dim=-1).cpu().numpy()


class PolicyBaseline:
    """A baseline backed by a (typically frozen) network — e.g. the random-init anchor or any
    saved checkpoint. Greedy by default."""

    def __init__(self, net, device, name: str, greedy: bool = True):
        self.net = net
        self.device = device
        self.name = name
        self.greedy = greedy

    @torch.no_grad()
    def actions(self, obs_np, ids_np, mask_np, battles=None, sides=None) -> np.ndarray:
        obs = torch.as_tensor(obs_np, device=self.device)
        ids = torch.as_tensor(ids_np, device=self.device)
        mask = torch.as_tensor(mask_np, device=self.device)
        if self.greedy:
            logits, _ = self.net.forward(obs, mask, obs_ids=ids)
            return logits.argmax(dim=-1).cpu().numpy()
        action, _, _, _ = self.net.act(obs, mask, obs_ids=ids)
        return action.cpu().numpy()


class RandomBaseline:
    """Uniform random over legal actions — the floor every policy should clear."""

    def __init__(self, seed: int = 0, name: str = "random"):
        self.rng = np.random.default_rng(seed)
        self.name = name

    def actions(self, obs_np, ids_np, mask_np, battles=None, sides=None) -> np.ndarray:
        out = np.zeros(len(mask_np), dtype=np.int64)
        for i, m in enumerate(mask_np):
            legal = np.flatnonzero(m)
            out[i] = self.rng.choice(legal) if legal.size else 0
        return out


class MctsBaseline:
    """pmariglia's poke-engine MCTS as an opponent — a strong, validated reference.

    For each battle it builds a *perfect-information* poke-engine state (full teams known, so MCTS
    plays at full strength), searches `time_ms`, and maps the top-visited move to our action index
    (falling back to the first legal action on any unmapped/illegal choice). Search is sequential
    per env and heavy, so keep num_envs/n_games modest for this baseline.
    """

    def __init__(self, time_ms: int = 100, name: str | None = None):
        self.time_ms = time_ms
        self.name = name or f"mcts@{time_ms}ms"

    def actions(self, obs_np, ids_np, mask_np, battles=None, sides=None) -> np.ndarray:
        import json
        import poke_engine as pe
        from .poke_engine_adapter import move_choice_to_action, state_to_poke_engine

        out = np.zeros(len(mask_np), dtype=np.int64)
        for i in range(len(battles)):
            mask = mask_np[i]
            side = int(sides[i])
            sd = json.loads(battles[i].state_json())
            try:
                st = state_to_poke_engine(sd, side)
                res = pe.monte_carlo_tree_search(st, self.time_ms)
                best = max(res.side_one, key=lambda x: x.visits)
                a = move_choice_to_action(best.move_choice, sd["sides"][side])
            except Exception:
                a = None
            if a is None or not mask[a]:
                legal = np.flatnonzero(mask)
                a = int(legal[0]) if legal.size else 0
            out[i] = a
        return out


class HeuristicBaseline:
    """A faithful port of poke-env's `SimpleHeuristicsPlayer.choose_move` over our `state_json`.

    Deterministic, no search: estimate the matchup (type advantage + speed + HP), switch out of a
    bad matchup into a better reserve, set hazards / clear our own, set up when safe at full HP,
    otherwise click the best-scoring damaging move (base_power × STAB × type-effectiveness ×
    accuracy). A strong-ish, very fast middle baseline between `random` and `mcts`.
    """

    ENTRY_HAZARDS = {"spikes": "spikes", "stealthrock": "stealth_rock",
                     "stickyweb": "sticky_web", "toxicspikes": "toxic_spikes"}
    ANTI_HAZARDS = {"rapidspin", "defog"}
    SPEED_COEF = 0.1
    HP_COEF = 0.4
    SWITCH_THRESHOLD = -2

    def __init__(self, name: str = "heuristic"):
        self.name = name

    def _matchup(self, te, mon: dict, opp: dict) -> float:
        mon_types = [t for t in mon["types"] if t != "none"]
        opp_types = [t for t in opp["types"] if t != "none"]
        off = max((te(t, opp["types"]) for t in mon_types), default=1.0)   # our best type vs them
        deff = max((te(t, mon["types"]) for t in opp_types), default=1.0)  # their best type vs us
        score = off - deff
        spe_m, spe_o = mon["stats"]["spe"], opp["stats"]["spe"]
        score += self.SPEED_COEF if spe_m > spe_o else (-self.SPEED_COEF if spe_o > spe_m else 0.0)
        score += self.HP_COEF * (mon["hp"] / max(1, mon["maxhp"]))
        score -= self.HP_COEF * (opp["hp"] / max(1, opp["maxhp"]))
        return score

    def _action_for(self, te, side: dict, opp_side: dict, mask) -> int:
        ai = side["active_index"]
        mons = side["pokemon"]
        active = mons[ai]
        opp = opp_side["pokemon"][opp_side["active_index"]]
        bench = [s for s in range(6) if s != ai]

        legal_moves = [i for i in range(4) if mask[i]]
        legal_switch_k = [k for k in range(5) if mask[4 + k]]
        switch_mon = lambda k: mons[bench[k]]

        # Should we switch out? (a good reserve exists AND we're in a bad spot)
        should_switch = False
        if legal_switch_k and any(self._matchup(te, switch_mon(k), opp) > 0 for k in legal_switch_k):
            b = active["boosts"]
            phys = active["stats"]["atk"] >= active["stats"]["spa"]
            should_switch = (
                b["def"] <= -3 or b["spd"] <= -3
                or (b["atk"] <= -3 and phys) or (b["spa"] <= -3 and not phys)
                or self._matchup(te, active, opp) < self.SWITCH_THRESHOLD
            )

        if legal_moves and not (should_switch and legal_switch_k):
            n_opp = sum(1 for p in opp_side["pokemon"] if p["species"] != "none" and p["hp"] > 0)
            n_self = sum(1 for p in mons if p["species"] != "none" and p["hp"] > 0)
            opp_sc = opp_side["side_conditions"]
            self_sc = side["side_conditions"]

            # Hazards: set them up early; clear our own if any exist.
            for i in legal_moves:
                mid = active["moves"][i]["id"]
                if mid in self.ENTRY_HAZARDS and not opp_sc.get(self.ENTRY_HAZARDS[mid]) and n_opp >= 3:
                    return i
                if mid in self.ANTI_HAZARDS and any(self_sc[h] for h in
                        ("stealth_rock", "spikes", "toxic_spikes", "sticky_web")) and n_self >= 2:
                    return i

            # Setup: boost when at full HP in a favorable matchup and not maxed.
            if active["hp"] >= active["maxhp"] and self._matchup(te, active, opp) > 0 \
                    and max(active["boosts"][s] for s in ("atk", "def", "spa", "spd", "spe")) < 6:
                for i in legal_moves:
                    mv = active["moves"][i]
                    if mv["self_boost_total"] >= 2 and mv["base_power"] == 0:
                        return i

            # Best damaging move.
            def score(i):
                mv = active["moves"][i]
                stab = 1.5 if mv["type"] in active["types"] else 1.0
                acc = mv["accuracy"] / 100.0 if mv["accuracy"] > 0 else 1.0
                return mv["base_power"] * stab * te(mv["type"], opp["types"]) * acc

            dmg = [i for i in legal_moves if active["moves"][i]["base_power"] > 0]
            if dmg:
                return max(dmg, key=score)

        # Switch to the best-matchup reserve.
        if legal_switch_k:
            return 4 + max(legal_switch_k, key=lambda k: self._matchup(te, switch_mon(k), opp))

        legal = [a for a in range(len(mask)) if mask[a]]
        return legal[0] if legal else 0

    def actions(self, obs_np, ids_np, mask_np, battles=None, sides=None) -> np.ndarray:
        import json

        out = np.zeros(len(mask_np), dtype=np.int64)
        for i in range(len(battles)):
            side = int(sides[i])
            sd = json.loads(battles[i].state_json())
            try:
                a = self._action_for(battles[i].type_effectiveness, sd["sides"][side], sd["sides"][1 - side], mask_np[i])
            except Exception:
                a = None
            if a is None or not mask_np[i][a]:
                legal = np.flatnonzero(mask_np[i])
                a = int(legal[0]) if legal.size else 0
            out[i] = int(a)
        return out


@torch.no_grad()
def evaluate(model, baseline: Baseline, device, n_games: int = 100, num_envs: int = 8,
             seed: int = 1234567, max_turns: int = 300) -> dict:
    """Play the (greedy) learner vs `baseline` over `n_games`, side-balanced, fixed eval seed."""
    envs = EngineVecEnv(num_envs, seed=seed, max_turns=max_turns)
    results: list[int] = []
    turns: list[int] = []
    while len(results) < n_games:
        obs_l, ids_l, mask_l = envs.learner_view()
        obs_o, ids_o, mask_o = envs.opponent_view()
        learner_a = greedy_actions(model, obs_l, ids_l, mask_l, device)
        opp_a = baseline.actions(obs_o, ids_o, mask_o, battles=envs.battles, sides=(1 - envs.learner_side))
        prev_len = envs._ep_len.copy()
        reward, done, _ = envs.step(learner_a, opp_a)
        for r, d, pl in zip(reward, done, prev_len):
            if d:
                results.append(1 if r > 0 else (-1 if r < 0 else 0))
                turns.append(int(pl) + 1)
    res = results[:n_games]
    wins = sum(r == 1 for r in res)
    losses = sum(r == -1 for r in res)
    draws = sum(r == 0 for r in res)
    return {
        "baseline": baseline.name,
        "n_games": n_games,
        "win_rate": wins / n_games,
        "wins": wins,
        "losses": losses,
        "draws": draws,
        "avg_turns": float(np.mean(turns[:n_games])) if turns else float("nan"),
    }
