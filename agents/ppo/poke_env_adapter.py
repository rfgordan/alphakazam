"""Adapter: our engine's state (from `Battle.state_json()`) -> a real **poke-env** `Battle`, and a
poke-env `BattleOrder` -> our 9-action index.

This lets any `poke_env.player.Player` (RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer,
...) act as an opponent/baseline against our learned policy — using poke-env's *own*, battle
representation and decision code rather than a hand re-implementation. It mirrors the poke-engine
adapter (`poke_engine_adapter.py`): our engine stays the simulator; poke-env only supplies the
opponent brain.

How it works (offline, no Showdown server):
  - The controlled side is populated by driving poke-env's real `Battle.parse_request(request)`
    with a synthesized Showdown "request" JSON built from our perfect-info state — so poke-env
    computes `active_pokemon`, `available_moves`, `available_switches`, and the full team exactly
    as it would live.
  - The opponent (our policy's) side is revealed the way a real game would expose it: the active
    mon (species/HP/status/boosts) plus any already-fainted mons (public info) go into
    `opponent_team`; the rest stay fogged. poke-env fills base stats/types from its gen-9 dex.
  - Side conditions / hazards are copied into poke-env's `SideCondition` dicts.

Approximations (fine for a baseline): opponent EVs/IVs/exact stats are poke-env's dex estimates
(fog of war, as intended); volatile statuses beyond status/boosts are not transferred; Tera /
Dynamax intent is ignored (our 9-action space has no separate Tera action).
"""

from __future__ import annotations

import logging

_LOGGER = logging.getLogger("poke_env_adapter")
_LOGGER.addHandler(logging.NullHandler())

# our boost keys, in poke-env's stat-name spelling
_BOOSTS = ("atk", "def", "spa", "spd", "spe", "accuracy", "evasion")


def _condition(m: dict) -> str:
    """Showdown 'condition' string for a mon: 'hp/maxhp[ status]' or '0 fnt'."""
    hp, maxhp = int(m["hp"]), int(m["maxhp"])
    if hp <= 0:
        return "0 fnt"
    status = m["status"]
    return f"{hp}/{maxhp}" if status == "none" else f"{hp}/{maxhp} {status}"


def _mon_request(m: dict, prefix: str, active: bool) -> dict:
    """A Showdown request-side pokemon entry built from one of our state mons."""
    species = m["species"]
    item = m["item"]
    return {
        "ident": f"{prefix}: {species}",
        "details": f"{species}, L{m['level']}",
        "active": active,
        "condition": _condition(m),
        "stats": {k: m["stats"][k] for k in ("atk", "def", "spa", "spd", "spe")},
        "moves": [mv["id"] for mv in m["moves"] if mv["id"] != "none"],
        "baseAbility": m["base_ability"],
        "ability": m["ability"],
        "item": "" if item == "none" else item,
    }


def state_to_poke_env_battle(state_dict: dict, bot_side: int, mask=None):
    """Build a poke-env `Battle` for the side `bot_side` (0/1) controls, from our perfect-info state.

    The controlled side is `p1` (fully known); the opponent is `p2` (active + fainted revealed).
    If `mask` (our length-9 legal-action mask) is given, the request only exposes moves/switches
    that are legal in our engine — so the bot never picks an action we'd have to override (this
    matters for move-lock states like encore/rampage that poke-env doesn't model).
    Returns a `Battle` ready to hand to any `poke_env.player.Player.choose_move`.
    """
    from poke_env.battle.battle import Battle
    from poke_env.battle.side_condition import SideCondition

    sd = state_dict
    bot = sd["sides"][bot_side]
    opp = sd["sides"][1 - bot_side]
    bot_ai = bot["active_index"]

    battle = Battle("battle-gen9customgame-adapter", "bot", _LOGGER, gen=9)

    # --- the controlled side, via poke-env's real request parser ------------------------------
    pokemon = [_mon_request(bot["pokemon"][i], "p1", i == bot_ai) for i in range(6)]
    active_mon = bot["pokemon"][bot_ai]
    enabled = [
        {"id": mv["id"], "move": mv["id"], "pp": mv["pp"], "maxpp": max(1, mv["pp"]),
         "target": "normal",
         "disabled": bool(mv["disabled"]) or mv["pp"] <= 0 or (mask is not None and not mask[j])}
        for j, mv in enumerate(active_mon["moves"]) if mv["id"] != "none"
    ]
    any_move = any(not mv["disabled"] for mv in enabled)
    request: dict = {"side": {"name": "p1", "id": "p1", "pokemon": pokemon},
                     "forceSwitch": [not any_move]}
    if any_move:
        request["active"] = [{"moves": enabled}]
    battle.parse_request(request)

    # Prune switches to those our mask allows (a mon can be switch-legal to poke-env yet trapped in
    # our engine). Switch action 4+k targets the k-th non-active party slot, in slot order.
    if mask is not None and battle._available_switches:
        bench = [i for i in range(6) if i != bot_ai]
        legal_species = {bot["pokemon"][slot]["species"]
                         for k, slot in enumerate(bench) if mask[4 + k]}
        battle._available_switches = [p for p in battle._available_switches
                                      if p.species in legal_species]

    # parse_request doesn't carry boosts (they arrive via messages) — set them on the active mon.
    if battle.active_pokemon is not None:
        for stat in _BOOSTS:
            amt = bot["boosts"].get(stat, 0)
            if amt:
                battle.active_pokemon.boost(stat, amt)

    # --- the opponent side: reveal the active mon + any fainted mons (public info) -------------
    opp_ai = opp["active_index"]
    for i in range(6):
        om = opp["pokemon"][i]
        if om["species"] == "none":
            continue
        fainted = int(om["hp"]) <= 0
        if not (i == opp_ai or fainted):
            continue  # fog of war: only active + fainted are known
        details = f"{om['species']}, L{om['level']}"
        mon = battle.get_pokemon(f"p2: {om['species']}", details=details)
        if i == opp_ai:
            mon.switch_in(details=details)  # the mon on the field
            mon.set_hp_status(_condition(om))
            for stat in _BOOSTS:
                amt = opp["boosts"].get(stat, 0)
                if amt:
                    mon.boost(stat, amt)
        else:
            mon.set_hp_status("0 fnt")  # a revealed, already-fainted mon (stays off the field)

    # --- side conditions / hazards into poke-env's SideCondition dicts -------------------------
    sc_map = {
        "stealth_rock": SideCondition.STEALTH_ROCK, "spikes": SideCondition.SPIKES,
        "toxic_spikes": SideCondition.TOXIC_SPIKES, "sticky_web": SideCondition.STICKY_WEB,
        "reflect": SideCondition.REFLECT, "light_screen": SideCondition.LIGHT_SCREEN,
        "aurora_veil": SideCondition.AURORA_VEIL, "tailwind": SideCondition.TAILWIND,
    }
    for name, sd_side, target in (("bot", bot, battle._side_conditions),
                                  ("opp", opp, battle._opponent_side_conditions)):
        for key, cond in sc_map.items():
            val = sd_side["side_conditions"].get(key, 0)
            if val:
                target[cond] = int(val) if not isinstance(val, bool) else 1
    return battle


def order_to_action(order, side_dict: dict) -> int | None:
    """Map a poke-env `BattleOrder` to our 9-action index (0..3 = move slots, 4..8 = switch to the
    k-th non-active party slot). Returns None if it can't be mapped (caller falls back to legal)."""
    from poke_env.battle.move import Move
    from poke_env.battle.pokemon import Pokemon

    o = getattr(order, "order", None)
    ai = side_dict["active_index"]
    mons = side_dict["pokemon"]
    if isinstance(o, Move):
        active_ids = [mv["id"] for mv in mons[ai]["moves"]]
        return active_ids.index(o.id) if o.id in active_ids else None
    if isinstance(o, Pokemon):
        target = o.species  # base-species toID, same convention as our state ids
        bench = [i for i in range(6) if i != ai]
        for k, slot in enumerate(bench):
            if mons[slot]["species"] == target:
                return 4 + k
        return None
    return None  # DefaultBattleOrder / PassBattleOrder / unknown
