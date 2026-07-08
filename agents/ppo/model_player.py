"""A poke-env `Player` backed by a trained checkpoint — usable anywhere poke-env players go:
battling baselines, laddering, or **accepting challenges from a human on the Showdown web client**.

Reuses the exact training encoder (`build_observation`) and legal-action mask, so the deployed policy
sees precisely what it trained on. Checkpoint architecture (flat-MLP `ActorCritic` vs slot-equivariant
`SlotActorCritic`) and hidden size are auto-detected from the state dict.

Optional human-readable logging: `log_dir` tees each battle's raw Showdown protocol to
`<log_dir>/<battle_tag>.log`; `save_replays` (poke-env native) writes viewable replay HTML.
"""

from __future__ import annotations

import logging
from pathlib import Path

import numpy as np
import torch

from poke_env.player import Player
from poke_env.environment import SinglesEnv

from .model import ActorCritic
from .pokeenv_env import build_observation, legal_action_mask, OBS_DIM, N_ACTIONS

_LOGGER = logging.getLogger("model_player")


_MODEL_CACHE: dict = {}


def load_model(ckpt_path: str):
    """Load a checkpoint, auto-detecting arch (mlp/slot) and hidden size from its weights.
    Cached by (path, mtime) so many opponent instances share one nn.Module (inference-only)."""
    key = (str(ckpt_path), Path(ckpt_path).stat().st_mtime_ns)
    if key in _MODEL_CACHE:
        return _MODEL_CACHE[key]
    sd = torch.load(ckpt_path, map_location="cpu")
    hidden = sd["trunk.0.weight"].shape[0]
    trunk_in = sd["trunk.0.weight"].shape[1]
    n_hidden = sum(1 for k in sd if k.startswith("trunk.") and k.endswith(".weight")) - 1
    is_slot = any("move_scorer" in k for k in sd)
    if is_slot:
        from .slot_model import SlotActorCritic, transfer_v2_state_dict, _V2_TRUNK_IN
        set_context = "move_embed.weight" in sd
        set_dim = sd["move_embed.weight"].shape[0] if set_context else 32
        from .pokeenv_env import TRUNK_IDX
        frames = max(1, (trunk_in - (2 * set_dim if set_context else 0)) // len(TRUNK_IDX))
        model = SlotActorCritic(OBS_DIM * frames, N_ACTIONS, hidden_dim=hidden,
                                n_hidden_layers=n_hidden, set_context=set_context,
                                set_dim=set_dim, frames=frames)
        if trunk_in == _V2_TRUNK_IN and trunk_in != len(model.trunk_idx):
            _LOGGER.info("v2-era checkpoint %s: transferring to v3 obs layout", ckpt_path)
            sd = transfer_v2_state_dict(sd, model)
    else:
        model = ActorCritic(OBS_DIM, N_ACTIONS, hidden, n_hidden, embed=None, aux=False)
        if trunk_in != OBS_DIM:
            raise ValueError(f"{ckpt_path}: legacy MLP checkpoint (obs {trunk_in} != {OBS_DIM}); "
                             "no transfer path — retrain or use a slot checkpoint")
    model.load_state_dict(sd)
    model.eval()
    out = (model, ("slot" if is_slot else "mlp"), hidden)
    _MODEL_CACHE[key] = out
    return out


class ModelPlayer(Player):
    def __init__(self, checkpoint: str, *args, greedy: bool = True, log_dir: str | None = None,
                 verbose: bool = False, **kwargs):
        super().__init__(*args, **kwargs)
        self.model, self.arch, self.hidden = load_model(checkpoint)
        self.greedy = greedy
        self.verbose = verbose
        self.checkpoint = checkpoint
        self._log_dir = Path(log_dir) if log_dir else None
        if self._log_dir:
            self._log_dir.mkdir(parents=True, exist_ok=True)
        self._log_bufs: dict[str, list[str]] = {}
        _LOGGER.info("ModelPlayer loaded %s (arch=%s hidden=%d)", checkpoint, self.arch, self.hidden)

    # -- decision ----------------------------------------------------------------------------------
    def _stacked_obs(self, battle):
        """Frame-stack per battle for frames>1 models (current obs first, then previous)."""
        obs = build_observation(battle)
        k = getattr(self.model, "frames", 1)
        if k <= 1:
            return obs
        if not hasattr(self, "_prev_obs"):
            self._prev_obs = {}
        prev = self._prev_obs.get(battle.battle_tag)
        if prev is None or len(prev) != k - 1:
            prev = [obs] * (k - 1)
        out = np.concatenate([obs] + prev)
        self._prev_obs[battle.battle_tag] = [obs] + prev[:-1]
        if len(self._prev_obs) > 64:      # bound memory across many battles
            self._prev_obs.pop(next(iter(self._prev_obs)))
        return out

    def choose_move(self, battle):
        try:
            obs = self._stacked_obs(battle)
            mask = legal_action_mask(battle)
            with torch.no_grad():
                logits, value = self.model.forward(torch.as_tensor(obs).unsqueeze(0).float(),
                                                   torch.as_tensor(mask).unsqueeze(0).bool())
            if self.greedy:
                a = int(logits.argmax(-1).item())
            else:
                a = int(torch.distributions.Categorical(logits=logits.squeeze(0)).sample().item())
            if self.verbose:
                self._explain(battle, logits.squeeze(0), value.item(), a, mask)
            order = SinglesEnv.action_to_order(np.int64(a), battle, fake=False, strict=False)
            # strict=False returns a default order if somehow illegal — fall back to a legal move.
            if order is None or "default" in str(order):
                return self.choose_random_move(battle)
            return order
        except Exception as e:  # never crash a live game on an encoder/edge-case hiccup
            _LOGGER.warning("choose_move fallback (%s: %s)", type(e).__name__, e)
            return self.choose_random_move(battle)

    # -- verbose narration -------------------------------------------------------------------------
    def _action_label(self, a: int, battle) -> str:
        if a < 6:
            team = list(battle.team.values())
            return f"switch {team[a].species}" if a < len(team) else f"switch#{a}"
        i = (a - 6) % 4
        moves = list(battle.active_pokemon.moves.values())[:4] if battle.active_pokemon else []
        name = moves[i].id if i < len(moves) else f"move#{i}"
        return name + (" +tera" if a >= 22 else "")

    def _explain(self, battle, logits, value, a, mask):
        probs = torch.softmax(logits, -1)
        legal = [i for i in range(N_ACTIONS) if mask[i]]
        top = sorted(legal, key=lambda i: -probs[i].item())[:3]
        me, foe = battle.active_pokemon, battle.opponent_active_pokemon
        def hp(m):
            return f"{m.species}({(m.current_hp_fraction or 0) * 100:.0f}%)" if m else "?"
        head = f"[T{battle.turn}] {hp(me)} vs {hp(foe)}  V={value:+.1f}"
        alts = "  ".join(f"{self._action_label(i, battle)} {probs[i].item():.2f}" for i in top)
        print(f"{head}\n   → {self._action_label(a, battle)} (p={probs[a].item():.2f})   "
              f"| top: {alts}", flush=True)

    # -- raw-protocol logging ----------------------------------------------------------------------
    async def _handle_battle_message(self, split_messages):
        if self._log_dir is not None:
            try:
                tag = split_messages[0][0].replace(">", "").strip() or "battle"
                buf = self._log_bufs.setdefault(tag, [])
                for m in split_messages[1:]:
                    if m and any(m):
                        buf.append("|".join(m))   # m[0] is already "" -> yields "|move|..."
                        if len(m) > 1 and m[1] in ("win", "tie"):
                            self._flush_log(tag)
            except Exception:
                pass
        return await super()._handle_battle_message(split_messages)

    def _flush_log(self, tag: str):
        buf = self._log_bufs.pop(tag, None)
        if not buf:
            return
        path = self._log_dir / f"{tag}.log"
        try:
            path.write_text("\n".join(buf) + "\n")
            _LOGGER.info("wrote game log %s (%d lines)", path, len(buf))
        except Exception:
            pass
