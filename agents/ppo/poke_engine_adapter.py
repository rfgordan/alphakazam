"""Adapter: our engine's state (from `Battle.state_json()`) -> poke-engine `State`, and
poke-engine `move_choice` strings -> our 9-action index.

Lets us run pmariglia's poke-engine MCTS as an opponent/baseline against our learned policy.
We construct a **perfect-information** poke-engine state (both full teams known), so MCTS is at
full strength and there's no set-inference noise — the strongest, cleanest reference opponent.

Approximations (fine for a baseline; noted so we know the limits):
  - volatile statuses / substitute / wish / future-sight are not transferred (passed empty),
  - Tera intent in a chosen move ("...-tera") collapses to the plain move (our 9-action space
    has no separate Tera action),
  - side conditions beyond the eight our engine models are 0.
"""

from __future__ import annotations

import poke_engine as pe

# our Status toID -> poke-engine status string
_STATUS = {"none": "None", "brn": "Burn", "par": "Paralyze", "slp": "Sleep",
           "frz": "Freeze", "psn": "Poison", "tox": "Toxic"}
# our Weather toID -> poke-engine weather string (StrongWinds has no poke-engine equiv -> none)
_WEATHER = {"none": "none", "sunnyday": "sun", "raindance": "rain", "sandstorm": "sand",
            "snowscape": "snow", "desolateland": "harshsun", "primordialsea": "heavyrain",
            "deltastream": "none"}
# our Terrain toID already matches poke-engine ("electricterrain", ...).


def _type(t: str) -> str:
    return "typeless" if t == "none" else t


def _pokemon(m: dict) -> "pe.Pokemon":
    st = m["stats"]
    return pe.Pokemon(
        id=m["species"],
        level=m["level"],
        types=(_type(m["types"][0]), _type(m["types"][1])),
        base_types=(_type(m["base_types"][0]), _type(m["base_types"][1])),
        hp=int(m["hp"]),
        maxhp=int(m["maxhp"]),
        ability=m["ability"],
        base_ability=m["base_ability"],
        item=m["item"],
        nature=m["nature"],
        evs=tuple(m["evs"]),
        attack=st["atk"], defense=st["def"], special_attack=st["spa"],
        special_defense=st["spd"], speed=st["spe"],
        status=_STATUS.get(m["status"], "None"),
        weight_kg=m["weight_kg"],
        moves=[pe.Move(id=mv["id"], disabled=mv["disabled"], pp=mv["pp"]) for mv in m["moves"]],
        tera_type=_type(m["tera_type"]),
        terastallized=m["terastallized"],
    )


def _side(side_dict: dict) -> "pe.Side":
    ai = side_dict["active_index"]
    mons = side_dict["pokemon"]
    order = [ai] + [i for i in range(6) if i != ai]  # poke-engine puts the active at index 0
    b = side_dict["boosts"]
    sc = side_dict["side_conditions"]

    # last_used_move as poke-engine's "move:N" (best effort; default none).
    lum = "move:none"
    active_moves = [mv["id"] for mv in mons[ai]["moves"]]
    lm = side_dict["last_used_move"]
    if lm != "none" and lm in active_moves:
        lum = f"move:{active_moves.index(lm)}"

    return pe.Side(
        pokemon=[_pokemon(mons[i]) for i in order],
        active_index="0",
        side_conditions=pe.SideConditions(
            stealth_rock=sc["stealth_rock"], spikes=sc["spikes"], toxic_spikes=sc["toxic_spikes"],
            sticky_web=int(sc["sticky_web"]), reflect=sc["reflect"], light_screen=sc["light_screen"],
            aurora_veil=sc["aurora_veil"], tailwind=sc["tailwind"],
        ),
        volatile_status_durations=pe.VolatileStatusDurations(),
        wish=(0, 0),
        future_sight=(0, "0"),
        volatile_statuses=set(),
        attack_boost=b["atk"], defense_boost=b["def"], special_attack_boost=b["spa"],
        special_defense_boost=b["spd"], speed_boost=b["spe"],
        accuracy_boost=b["accuracy"], evasion_boost=b["evasion"],
        last_used_move=lum,
        switch_out_move_second_saved_move="NONE",
    )


def state_to_poke_engine(state_dict: dict, mcts_side: int) -> "pe.State":
    """Build a poke-engine State where `side_one` is the side MCTS controls (`mcts_side`: 0/1)."""
    other = 1 - mcts_side
    sd = state_dict
    return pe.State(
        side_one=_side(sd["sides"][mcts_side]),
        side_two=_side(sd["sides"][other]),
        weather=_WEATHER.get(sd["weather"], "none"),
        weather_turns_remaining=max(0, sd["weather_turns"]),
        terrain=sd["terrain"],
        terrain_turns_remaining=max(0, sd["terrain_turns"]),
        trick_room=sd["trick_room"],
        trick_room_turns_remaining=max(0, sd["trick_room_turns"]),
        team_preview=False,
    )


def move_choice_to_action(move_choice: str, side_dict: dict) -> int | None:
    """Map a poke-engine `move_choice` ("switch <species>" / "<moveid>"[-tera]) to our 9-action
    index (0..3 = move slots, 4..8 = switch to the k-th non-active party slot). None if unmapped."""
    mc = move_choice
    if mc.endswith("-tera"):
        mc = mc[: -len("-tera")]
    elif mc.endswith("-mega") or mc.endswith("-dynamax"):
        mc = mc.rsplit("-", 1)[0]

    ai = side_dict["active_index"]
    mons = side_dict["pokemon"]
    if mc.startswith("switch "):
        target = mc.split("switch ", 1)[1]
        bench = [i for i in range(6) if i != ai]  # k-th bench -> switch action 4+k
        for k, slot in enumerate(bench):
            if mons[slot]["species"] == target:
                return 4 + k
        return None
    active_moves = [mv["id"] for mv in mons[ai]["moves"]]
    return active_moves.index(mc) if mc in active_moves else None
