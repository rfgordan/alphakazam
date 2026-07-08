"""Training environment backed by **poke-env + a live Pokémon Showdown server** (NOT our Rust engine).

This is the "model on the poke-env engine" path: battles are simulated by the real Showdown server;
the agent perceives them through poke-env's `Battle` objects. We provide the three pieces poke-env's
`SinglesEnv` leaves abstract — a float observation (`embed_battle`), a reward (`calc_reward`) — plus
a legal-action mask so our masked-policy PPO never proposes an illegal order.

Action space is poke-env's native 26-way singles encoding (see `SinglesEnv.action_to_order`):
    0..5   switch to team slot k
    6..9   use move slot k
    22..25 use move slot k + terastallize
(10..21 are mega/z/dynamax — never legal in gen9, always masked off.) We keep the full 26-wide head
and mask, so tera is learnable for free and the mapping matches poke-env exactly.

Usage:
    server running:  (cd engines/pokemon-showdown && node pokemon-showdown start --no-security)
    env  = DeepShowdownSinglesEnv(battle_format="gen9randombattle")
    wrap = MaskedSingleAgentEnv(env, opponent=RandomPlayer(battle_format="gen9randombattle"))
    obs, mask = wrap.reset()
    obs, mask, reward, done = wrap.step(action)
"""

from __future__ import annotations

import numpy as np
from gymnasium.spaces import Box

from poke_env.data import GenData
from poke_env.calc import calculate_damage
from poke_env.stats import compute_raw_stats
from poke_env.battle.status import Status
from poke_env.battle.weather import Weather
from poke_env.battle.field import Field
from poke_env.battle.effect import Effect
from poke_env.battle.side_condition import SideCondition
from poke_env.environment import SinglesEnv
from poke_env.player.battle_order import DefaultBattleOrder

# ---- fixed vocabularies (order is frozen so obs columns are stable) -------------------------------
_GEN = GenData.from_gen(9)
_TYPE_CHART = _GEN.type_chart
TYPE_NAMES = sorted(_TYPE_CHART.keys())            # 18 canonical types
TYPE_INDEX = {t: i for i, t in enumerate(TYPE_NAMES)}
STATUSES = ["BRN", "FRZ", "PAR", "PSN", "SLP", "TOX"]          # (+ "none" -> all-zero)
STATUS_INDEX = {s: i for i, s in enumerate(STATUSES)}
BOOST_KEYS = ["atk", "def", "spa", "spd", "spe", "accuracy", "evasion"]
WEATHERS = ["RAINDANCE", "SUNNYDAY", "SANDSTORM", "SNOWSCAPE", "HAIL"]
TERRAINS = ["ELECTRIC_TERRAIN", "GRASSY_TERRAIN", "MISTY_TERRAIN", "PSYCHIC_TERRAIN"]
HAZARDS = ["STEALTH_ROCK", "SPIKES", "TOXIC_SPIKES", "STICKY_WEB",
           "REFLECT", "LIGHT_SCREEN", "AURORA_VEIL", "TAILWIND"]

N_ACTIONS = 26
_STAT_KEYS = ("hp", "atk", "def", "spa", "spd", "spe")

# ---- observation layout (sizes summed into OBS_DIM) ----------------------------------------------
_ACTIVE = 1 + 1 + len(STATUSES) + len(BOOST_KEYS) + len(TYPE_NAMES) + 6   # hp,lvl,status,boosts,types,base_stats = 40
_BENCH_PER_SIDE = 5 * 2                                                    # (hp_frac, fainted) x5
_FIELD = len(WEATHERS) + 1 + len(TERRAINS) + 1                            # weather(+none-implicit via count) + terrain
_SIDECOND = len(HAZARDS) * 2
_MOVES = 4 * 4                                                             # per move: bp, acc, is_status, effectiveness
_MISC = 4                                                                  # team_alive x2, trapped, force_switch
_DMG = 4 + 4 + 2   # per-move est-damage-fraction (x4) + would-KO (x4) + [speed_adv, speed_ratio]
_TEAM = 6 * 3      # per TEAM slot (aligned with switch actions 0-5): hp_frac, fainted, is_active

# ---- v3 append-only blocks (hidden-info fixes + type/matchup generalization) ----------------------
# Append-only so v2 checkpoints transfer exactly (old offsets unchanged; new columns zero-init).
_V2_DIM = _ACTIVE * 2 + _BENCH_PER_SIDE * 2 + _FIELD + _SIDECOND + _MOVES + _MISC + _DMG + _TEAM
_GLOBAL2 = 2        # [foe_alive_frac (bugfixed: unrevealed count as alive), foe_unrevealed_frac]
_INCOMING = 4       # revealed foe moves vs MY active: [max_dmg_frac, would_ko_me, n_revealed/4, max_eff/4]
_FOEBENCH = 1       # [max STAB type-eff of any revealed foe bench mon vs my active /4]
_MOVETYPE = 4 * len(TYPE_NAMES)   # raw type one-hot per my move (feeds move scorer, NOT trunk)
_MOVEBENCH = 4      # per move: [max type-eff vs revealed foe bench /4]
_SLOTMATCH = 6 * (len(TYPE_NAMES) + 3)   # per team slot: types(18) + [atk_eff, def_eff, speed_edge]
# v4: per-move MECHANICS the model previously couldn't see (a setup move looked identical to any
# 0-BP status move): [is_physical, self-boost off/def/spe stages, inflicts_status, heal_frac, priority]
_MOVEMECH = 4 * 7
# v5 (paper-inspired, Table A.1/A.2): field TIMERS (screens/tailwind/weather/trick-room elapsed —
# expiry is playable information) + volatile effects and status counters for both actives.
_FIELDTIME = 4 * 2 + 3            # [reflect, lightscreen, veil, tailwind] elapsed x2 sides;
                                   # weather elapsed; trick room present + elapsed
_VOLATILES = (6 + 1 + 1) * 2      # per active: [sub, confusion, leechseed, taunt, encore, yawn]
                                   # + protect_counter/5 + status_counter/8
# v6 "verb block": FULL per-move impact vocabulary (Rob: be thorough — every modified stat incl.
# accuracy/evasion, both directions; field/tempo verbs; risk/sustain; secondaries).
#   self stat deltas (7: atk,def,spa,spd,spe,acc,eva) + target stat deltas (7)
#   status kind one-hot (slp,par,brn,psn/tox,frz) + would-affect-this-foe (6)
#   verbs: self_switch, forces_foe_out, protect_like, substitute (4)
#   hazard-set one-hot (sr,spikes,tspikes,web) + hazard_removal + screen_set (6)
#   weather-set (sun,rain,sand,snow) + terrain-set (elec,grassy,misty,psychic) + trick_room (9)
#   recoil_frac, drain_frac, self_destruct (3) + [max_secondary_chance, secondary_has_status] (2)
#   + charge, recharge, pp_frac, is_ohko, expected_hits/5,
#     restricts_foe (encore/taunt/disable/torment), removes_item (knockoff/trick),
#     inflicts confusion / leech seed / yawn (10)
_MOVEVERB = 4 * 54
# v7 context/legality: decision-context flags [replacing_fainted, pivot_selection] + per-move
# LEGAL bit (4) + per-team-slot SWITCHABLE bit (6). The mask gates action selection but was
# never an input — the value function couldn't see e.g. that it's Taunt-locked or Choice-locked.
_CONTEXT = 2 + 4 + 6
OBS_DIM = (_V2_DIM + _GLOBAL2 + _INCOMING + _FOEBENCH + _MOVETYPE + _MOVEBENCH + _SLOTMATCH
           + _MOVEMECH + _FIELDTIME + _VOLATILES + _MOVEVERB + _CONTEXT)
_MV_OFF = (_V2_DIM + _GLOBAL2 + _INCOMING + _FOEBENCH + _MOVETYPE + _MOVEBENCH + _SLOTMATCH
           + _MOVEMECH + _FIELDTIME + _VOLATILES)
_CTX_OFF = _MV_OFF + _MOVEVERB

# ---- obs layout (single source of truth for slot-structured models) --------------------------------
_MOVES_OFF = _ACTIVE * 2 + _BENCH_PER_SIDE * 2 + _FIELD + _SIDECOND
_DMG_OFF = _MOVES_OFF + _MOVES + _MISC
_TEAM_OFF = _DMG_OFF + _DMG
_G2_OFF = _V2_DIM
_INC_OFF = _G2_OFF + _GLOBAL2
_FB_OFF = _INC_OFF + _INCOMING
_MT_OFF = _FB_OFF + _FOEBENCH
_MB_OFF = _MT_OFF + _MOVETYPE
_SM_OFF = _MB_OFF + _MOVEBENCH
_MM_OFF = _SM_OFF + _SLOTMATCH
_NT = len(TYPE_NAMES)

# Per move slot i: [bp, acc, is_status, eff, est_dmg, would_ko] + type one-hot(18) + [bench_eff]
#                  + mechanics [is_physical, boost_off, boost_def, boost_spe, inflicts, heal, prio]
MOVE_SLOT_IDX = [[_MOVES_OFF + 4 * i + k for k in range(4)] + [_DMG_OFF + i, _DMG_OFF + 4 + i]
                 + [_MT_OFF + _NT * i + k for k in range(_NT)] + [_MB_OFF + i]
                 + [_MM_OFF + 7 * i + k for k in range(7)]
                 + [_MV_OFF + 54 * i + k for k in range(54)]
                 + [_CTX_OFF + 2 + i]                                  # this move's LEGAL bit
                 for i in range(4)]
# Per team slot j (== switch action j): [hp, fainted, is_active] + types(18) + [atk_eff, def_eff, spe_edge]
SWITCH_SLOT_IDX = [[_TEAM_OFF + 3 * j + k for k in range(3)]
                   + [_SM_OFF + (_NT + 3) * j + k for k in range(_NT + 3)]
                   + [_CTX_OFF + 6 + j]                                # this slot's SWITCHABLE bit
                   for j in range(6)]
_FT_OFF = _MM_OFF + _MOVEMECH
_VO_OFF = _FT_OFF + _FIELDTIME

# Trunk input: everything EXCEPT the per-slot type one-hots (they only feed the shared scorers) —
# keeps the trunk's per-dim parameter cost off the 180 one-hot dims. Old v2 dims come first, in
# order, so v2 trunk weights transfer as an exact prefix.
TRUNK_IDX = (list(range(_V2_DIM))
             + list(range(_G2_OFF, _MT_OFF))                                   # global2+incoming+foebench
             + list(range(_MB_OFF, _SM_OFF))                                   # per-move bench_eff
             + [_SM_OFF + (_NT + 3) * j + _NT + k for j in range(6) for k in range(3)]  # slot scalars
             + list(range(_FT_OFF, _VO_OFF + _VOLATILES))                      # field timers + volatiles
             + list(range(_CTX_OFF, _CTX_OFF + _CONTEXT)))                     # context + legality


def _boost_mult(stage: int) -> float:
    """Stat multiplier for a boost stage (gen-6+ table), used for effective speed."""
    stage = max(-6, min(6, int(stage)))
    return (2 + stage) / 2 if stage >= 0 else 2 / (2 - stage)


_STAT_CACHE: dict = {}


def _est_opp_stats(mon) -> dict | None:
    """Estimate a fogged opponent's stats from PUBLIC info (species + level), neutral 85-EV/31-IV
    spread — the same assumption SimpleHeuristics uses. Cached by (species, level). No hidden info."""
    key = (mon.species, mon.level)
    if key not in _STAT_CACHE:
        try:
            raw = compute_raw_stats(mon.species, [85] * 6, [31] * 6, mon.level or 100, "hardy", _GEN)
            _STAT_CACHE[key] = dict(zip(_STAT_KEYS, raw))
        except Exception:
            _STAT_CACHE[key] = None
    return _STAT_CACHE[key]


def _damage_features(battle, me, foe) -> list:
    """Decision features the MLP can't cheaply synthesize from raw factors: per-move estimated
    damage (fraction of the foe's est. max HP), a would-KO flag, and a speed advantage.
    Uses poke-env's gen-9 damage calc against ESTIMATED opponent stats (public info only)."""
    dmg = [0.0] * 4
    ko = [0.0] * 4
    spd_adv, spd_ratio = 0.0, 0.5
    est = _est_opp_stats(foe) if foe is not None else None
    if me is not None and foe is not None and est is not None:
        foe._stats = est                                    # let calc read estimated defender stats
        foe_maxhp = max(1, est["hp"])
        foe_cur = foe_maxhp * (foe.current_hp_fraction or 0.0)
        mid, fid = me.identifier(battle.player_role), foe.identifier(battle.opponent_role)
        for i, mv in enumerate(list(me.moves.values())[:4]):
            try:
                lo, hi = calculate_damage(mid, fid, mv, battle, False)
                dmg[i] = min(((lo + hi) / 2) / foe_maxhp, 2.0)
                ko[i] = 1.0 if lo > 0 and lo >= foe_cur else 0.0
            except Exception:
                pass
        my_spe = ((me.stats or {}).get("spe") or 0) * _boost_mult(me.boosts.get("spe", 0))
        foe_spe = est["spe"] * _boost_mult(foe.boosts.get("spe", 0))
        if my_spe + foe_spe > 0:
            spd_ratio = my_spe / (my_spe + foe_spe)
        spd_adv = 1.0 if my_spe > foe_spe else 0.0
    return dmg + ko + [spd_adv, spd_ratio]


def _type_effectiveness(move, target) -> float:
    """Damage multiplier of `move`'s type against `target`'s type(s); 1.0 on any uncertainty."""
    if move is None or target is None or move.type is None:
        return 1.0
    return _teff(move.type, target)


def _teff(att_type, def_mon) -> float:
    """Raw-type damage multiplier of attacking type vs a mon's type(s); 1.0 on uncertainty."""
    if att_type is None or def_mon is None:
        return 1.0
    types = [t for t in def_mon.types if t is not None]
    if not types:
        return 1.0
    try:
        return att_type.damage_multiplier(*types[:2], type_chart=_TYPE_CHART)
    except Exception:
        return 1.0


def _encode_active(v: list, mon, foe_active) -> None:
    """Append the ~40-float active-mon block (relative to `foe_active` for typing context)."""
    if mon is None:
        v.extend([0.0] * _ACTIVE)
        return
    v.append(float(mon.current_hp_fraction or 0.0))
    v.append((mon.level or 100) / 100.0)
    st = [0.0] * len(STATUSES)
    if mon.status is not None and mon.status.name in STATUS_INDEX:
        st[STATUS_INDEX[mon.status.name]] = 1.0
    v.extend(st)
    v.extend((mon.boosts.get(k, 0)) / 6.0 for k in BOOST_KEYS)
    types = [0.0] * len(TYPE_NAMES)
    for t in mon.types:
        if t is not None and t.name in TYPE_INDEX:
            types[TYPE_INDEX[t.name]] = 1.0
    v.extend(types)
    bs = mon.base_stats or {}
    v.extend((bs.get(k, 0) or 0) / 255.0 for k in _STAT_KEYS)


def _encode_bench(v: list, team: dict, active) -> None:
    """Append 5 (hp_frac, fainted) pairs for non-active team members (public/own info)."""
    bench = [m for m in team.values() if m is not active]
    for i in range(5):
        if i < len(bench):
            m = bench[i]
            v.append(float(m.current_hp_fraction or 0.0))
            v.append(1.0 if m.fainted else 0.0)
        else:
            v.extend([0.0, 0.0])


def build_observation(battle) -> np.ndarray:
    """The 173-dim observation for a poke-env `Battle`. Standalone so live-play players and the
    training env share exactly one encoder (no drift between train and deploy)."""
    v: list[float] = []
    me, foe = battle.active_pokemon, battle.opponent_active_pokemon
    _encode_active(v, me, foe)
    _encode_active(v, foe, me)
    _encode_bench(v, battle.team, me)
    _encode_bench(v, battle.opponent_team, foe)

    # field: weather + terrain one-hots
    wx = [0.0] * len(WEATHERS)
    for w in battle.weather:
        if w.name in WEATHERS:
            wx[WEATHERS.index(w.name)] = 1.0
    v.extend(wx)
    v.append(1.0 if not battle.weather else 0.0)          # "no weather" flag
    tr = [0.0] * len(TERRAINS)
    for f in battle.fields:
        if f.name in TERRAINS:
            tr[TERRAINS.index(f.name)] = 1.0
    v.extend(tr)
    v.append(1.0 if not any(f.name in TERRAINS for f in battle.fields) else 0.0)

    # side conditions / hazards, both sides
    for conds in (battle.side_conditions, battle.opponent_side_conditions):
        names = {c.name: n for c, n in conds.items()}
        for h in HAZARDS:
            v.append(min(names.get(h, 0), 3) / 3.0 if h in names else 0.0)

    # my active's 4 moves: base power, accuracy, is-status, type-effectiveness vs foe
    moves = list(me.moves.values())[:4] if me is not None else []
    for i in range(4):
        if i < len(moves):
            mv = moves[i]
            v.append((mv.base_power or 0) / 150.0)
            acc = mv.accuracy
            v.append(1.0 if acc is True else float(acc or 0.0))
            v.append(1.0 if (mv.category is not None and mv.category.name == "STATUS") else 0.0)
            v.append(_type_effectiveness(mv, foe) / 4.0)
        else:
            v.extend([0.0, 0.0, 0.0, 0.0])

    # misc — NOTE: the second entry is the foe's *revealed*-alive fraction (kept as-is for v2
    # checkpoint transfer); the bug-fixed true-alive fraction is in the v3 global block below.
    v.append(sum(1 for m in battle.team.values() if not m.fainted) / 6.0)
    v.append(sum(1 for m in battle.opponent_team.values() if not m.fainted) / 6.0)
    v.append(1.0 if battle.trapped else 0.0)
    fs = battle.force_switch
    v.append(1.0 if (fs is True or (isinstance(fs, (list, tuple)) and any(fs))) else 0.0)

    # estimated-damage / KO / speed-advantage decision features (public-info damage calc)
    v.extend(_damage_features(battle, me, foe))

    # per-TEAM-slot features, aligned with switch actions 0-5 (action j = team slot j)
    team = list(battle.team.values())
    for j in range(6):
        if j < len(team):
            t = team[j]
            v.extend([float(t.current_hp_fraction or 0.0), 1.0 if t.fainted else 0.0,
                      1.0 if t is me else 0.0])
        else:
            v.extend([0.0, 1.0, 0.0])   # unknown slot: treat as unswitchable

    # ================= v3 append-only blocks =================
    revealed = list(battle.opponent_team.values())
    moves = list(me.moves.values())[:4] if me is not None else []
    bench_foes = [m for m in revealed if m is not foe and not m.fainted]

    # global2: bug-fixed foe-alive (unrevealed mons ARE alive) + explicit unrevealed count
    v.append((6 - sum(1 for m in revealed if m.fainted)) / 6.0)
    v.append((6 - len(revealed)) / 6.0)

    # incoming threat from the foe's REVEALED moves vs my active (their stats estimated,
    # mine exact): [max est damage frac, would-KO-me, revealed-move count, max type-eff]
    inc_dmg = inc_ko = inc_eff = 0.0
    n_rev = 0
    if foe is not None and me is not None and me.max_hp:
        rev_moves = [m for m in foe.moves.values()][:4]
        n_rev = len(rev_moves)
        fid, mid = foe.identifier(battle.opponent_role), me.identifier(battle.player_role)
        my_cur_abs = (me.current_hp_fraction or 0.0) * me.max_hp
        for mv in rev_moves:
            inc_eff = max(inc_eff, _type_effectiveness(mv, me))
            try:
                lo, hi = calculate_damage(fid, mid, mv, battle, False)
                inc_dmg = max(inc_dmg, min(((lo + hi) / 2) / me.max_hp, 2.0))
                if lo > 0 and lo >= my_cur_abs:
                    inc_ko = 1.0
            except Exception:
                pass
    v.extend([inc_dmg, inc_ko, n_rev / 4.0, inc_eff / 4.0])

    # revealed foe BENCH threat vs my active (aggregate: max STAB-type effectiveness)
    fb = 0.0
    for m in bench_foes:
        for t in m.types:
            fb = max(fb, _teff(t, me))
    v.append(fb / 4.0)

    # my moves: raw type one-hot (position-invariant; feeds the shared move scorer, not the trunk)
    for i in range(4):
        row = [0.0] * len(TYPE_NAMES)
        if i < len(moves) and moves[i].type is not None and moves[i].type.name in TYPE_INDEX:
            row[TYPE_INDEX[moves[i].type.name]] = 1.0
        v.extend(row)

    # my moves: max type-effectiveness vs the revealed foe bench (generalize beyond the active)
    for i in range(4):
        e = 0.0
        if i < len(moves) and moves[i].type is not None:
            for m in bench_foes:
                e = max(e, _teff(moves[i].type, m))
        v.append(e / 4.0)

    # team-slot matchup block: types + [atk_eff, def_eff, speed_edge] vs the current foe active.
    # atk_eff compresses the slot's KNOWN MOVESET: max over its damaging moves of
    # type-eff x STAB(1.5) -> "how hard can this mon hit their active" (max 6).
    foe_est = _est_opp_stats(foe) if foe is not None else None
    foe_spe = (foe_est["spe"] * _boost_mult(foe.boosts.get("spe", 0))) if foe_est else 0.0
    for j in range(6):
        if j < len(team):
            t = team[j]
            row = [0.0] * len(TYPE_NAMES)
            for tt in t.types:
                if tt is not None and tt.name in TYPE_INDEX:
                    row[TYPE_INDEX[tt.name]] = 1.0
            v.extend(row)
            atk = 0.0
            for mv in list(t.moves.values())[:4]:
                if (mv.base_power or 0) > 0 and mv.type is not None:
                    stab = 1.5 if mv.type in t.types else 1.0
                    atk = max(atk, _teff(mv.type, foe) * stab)
            dfe = max((_teff(ft, t) for ft in foe.types if ft is not None), default=1.0) \
                if foe is not None else 1.0
            spe = ((t.stats or {}).get("spe") or 0)
            v.extend([atk / 6.0, dfe / 4.0, 1.0 if spe > foe_spe else 0.0])
        else:
            v.extend([0.0] * len(TYPE_NAMES) + [0.0, 1.0, 0.0])

    # per-move MECHANICS: what the move DOES to stats/status/HP — previously invisible, so a
    # Swords Dance was indistinguishable from Defog at decision time. Signed stages /4.
    for i in range(4):
        if i < len(moves):
            mv = moves[i]
            cat = mv.category.name if mv.category is not None else "STATUS"
            if cat == "STATUS":
                tgt = str(getattr(mv, "target", "")).lower()
                b = mv.boosts if (mv.boosts and "self" in tgt) else {}
            else:
                b = mv.self_boost or {}
            v.extend([
                1.0 if cat == "PHYSICAL" else 0.0,
                (b.get("atk", 0) + b.get("spa", 0)) / 4.0,
                (b.get("def", 0) + b.get("spd", 0)) / 4.0,
                b.get("spe", 0) / 4.0,
                1.0 if mv.status is not None else 0.0,
                float(getattr(mv, "heal", 0) or 0.0),
                (mv.priority or 0) / 5.0,
            ])
        else:
            v.extend([0.0] * 7)

    # field TIMERS: elapsed turns since each timed effect was set (expiry is playable info).
    # poke-env stores the SET turn as the dict value for non-stackable conditions.
    turn = battle.turn or 0
    _timed = (SideCondition.REFLECT, SideCondition.LIGHT_SCREEN,
              SideCondition.AURORA_VEIL, SideCondition.TAILWIND)
    for conds in (battle.side_conditions, battle.opponent_side_conditions):
        for sc in _timed:
            v.append(min(max(turn - conds[sc], 0), 8) / 8.0 if sc in conds else 0.0)
    wx_start = min(battle.weather.values()) if battle.weather else None
    v.append(min(max(turn - wx_start, 0), 8) / 8.0 if wx_start is not None else 0.0)
    tr = battle.fields.get(Field.TRICK_ROOM)
    v.append(1.0 if tr is not None else 0.0)
    v.append(min(max(turn - tr, 0), 5) / 5.0 if tr is not None else 0.0)

    # volatiles + status counters for both actives
    _vols = (Effect.SUBSTITUTE, Effect.CONFUSION, Effect.LEECH_SEED,
             Effect.TAUNT, Effect.ENCORE, Effect.YAWN)
    for mon in (me, foe):
        if mon is None:
            v.extend([0.0] * 8)
            continue
        eff = mon.effects or {}
        v.extend([1.0 if e in eff else 0.0 for e in _vols])
        v.append(min(getattr(mon, "protect_counter", 0) or 0, 5) / 5.0)
        v.append(min(getattr(mon, "status_counter", 0) or 0, 8) / 8.0)

    for i in range(4):
        v.extend(_move_verbs(moves[i], foe) if i < len(moves) else [0.0] * 54)

    # decision context + observed legality (the mask gates actions; these make it VISIBLE)
    fs_any = fs is True or (isinstance(fs, (list, tuple)) and any(fs))
    me_fainted = me is None or me.fainted or (me.current_hp_fraction or 0) <= 0
    v.append(1.0 if (fs_any and me_fainted) else 0.0)      # replacing a fainted mon
    v.append(1.0 if (fs_any and not me_fainted) else 0.0)  # pivot selection (U-turn etc.)
    avail_ids = {m.id for m in (battle.available_moves or [])}
    for i in range(4):
        v.append(1.0 if (i < len(moves) and moves[i].id in avail_ids) else 0.0)
    avail_sw = {p.species for p in (battle.available_switches or [])}
    for j in range(6):
        v.append(1.0 if (j < len(team) and team[j].species in avail_sw) else 0.0)

    assert len(v) == OBS_DIM, f"obs len {len(v)} != OBS_DIM {OBS_DIM}"
    return np.asarray(v, dtype=np.float32)


_STATS7 = ("atk", "def", "spa", "spd", "spe", "accuracy", "evasion")
_HAZ_SET = {"stealthrock": 0, "spikes": 1, "toxicspikes": 2, "stickyweb": 3}
_HAZ_REMOVE = {"defog", "rapidspin", "courtchange", "tidyup", "mortalspin"}
_SCREEN_SET = {"reflect", "lightscreen", "auroraveil"}
_PROTECT = {"protect", "detect", "banefulbunker", "silktrap", "burningbulwark", "spikyshield",
            "kingsshield", "obstruct", "maxguard"}
_WX_SET = {"sunnyday": 0, "raindance": 1, "sandstorm": 2, "snowscape": 3, "chillyreception": 3}
_TER_SET = {"electricterrain": 0, "grassyterrain": 1, "mistyterrain": 2, "psychicterrain": 3}
_POWDER = {"spore", "sleeppowder", "stunspore", "poisonpowder"}


def _status_would_affect(mv, foe) -> float:
    """Pragmatic immunity check for a status-inflicting move vs the current foe (public info):
    already statused, canonical type immunities, T-Wave type-eff 0, powder vs Grass."""
    if foe is None:
        return 1.0
    if foe.status is not None:
        return 0.0
    st = str(mv.status)
    ft = {t.name for t in foe.types if t is not None}
    if "PAR" in st and ("ELECTRIC" in ft or (mv.id == "thunderwave" and _teff(mv.type, foe) == 0)):
        return 0.0
    if "BRN" in st and "FIRE" in ft:
        return 0.0
    if ("PSN" in st or "TOX" in st) and ({"STEEL", "POISON"} & ft):
        return 0.0
    if mv.id in _POWDER and "GRASS" in ft:
        return 0.0
    return 1.0


def _move_verbs(mv, foe) -> list:
    """The 44-dim full impact vocabulary for one move (see _MOVEVERB layout comment)."""
    out = []
    cat = mv.category.name if mv.category is not None else "STATUS"
    tgt = str(getattr(mv, "target", "")).lower()
    self_b = dict((mv.boosts if (cat == "STATUS" and mv.boosts and "self" in tgt) else None)
                  or mv.self_boost or {})
    opp_b = dict(mv.boosts) if (mv.boosts and "self" not in tgt) else {}
    # secondaries contribute EXPECTED stat deltas: chance x stages (Icy Wind, Moonblast, ...)
    secs0 = getattr(mv, "secondary", None) or []
    for sec in ([secs0] if isinstance(secs0, dict) else secs0):
        ch = sec.get("chance", 0) / 100.0
        for s, amt in (sec.get("boosts") or {}).items():
            opp_b[s] = opp_b.get(s, 0) + ch * amt
        for s, amt in ((sec.get("self") or {}).get("boosts") or {}).items():
            self_b[s] = self_b.get(s, 0) + ch * amt
    out.extend([self_b.get(s, 0) / 4.0 for s in _STATS7])          # 7 self stat deltas
    out.extend([opp_b.get(s, 0) / 4.0 for s in _STATS7])           # 7 target stat deltas
    st = str(mv.status) if mv.status is not None else ""
    out.extend([1.0 if k in st else 0.0 for k in ("SLP", "PAR", "BRN")])
    out.append(1.0 if ("PSN" in st or "TOX" in st) else 0.0)
    out.append(1.0 if "FRZ" in st else 0.0)
    out.append(_status_would_affect(mv, foe) if st else 0.0)       # 6 status detail
    out.append(1.0 if getattr(mv, "self_switch", False) else 0.0)  # pivot
    out.append(1.0 if getattr(mv, "force_switch", False) else 0.0)  # phaze
    out.append(1.0 if mv.id in _PROTECT else 0.0)
    out.append(1.0 if mv.id == "substitute" else 0.0)              # 4 tempo verbs
    hz = [0.0] * 4
    sc = str(getattr(mv, "side_condition", "") or "").lower().replace(" ", "")
    if mv.id in _HAZ_SET:
        hz[_HAZ_SET[mv.id]] = 1.0
    elif sc in ("stealthrock", "spikes", "toxicspikes", "stickyweb"):
        hz[_HAZ_SET[sc]] = 1.0
    out.extend(hz)
    out.append(1.0 if mv.id in _HAZ_REMOVE else 0.0)
    out.append(1.0 if mv.id in _SCREEN_SET else 0.0)               # 6 hazard/screen
    wx = [0.0] * 4
    if mv.id in _WX_SET:
        wx[_WX_SET[mv.id]] = 1.0
    out.extend(wx)
    ter = [0.0] * 4
    if mv.id in _TER_SET:
        ter[_TER_SET[mv.id]] = 1.0
    out.extend(ter)
    out.append(1.0 if mv.id == "trickroom" else 0.0)               # 9 field verbs
    out.append(float(getattr(mv, "recoil", 0) or 0))
    out.append(float(getattr(mv, "drain", 0) or 0))
    out.append(1.0 if getattr(mv, "self_destruct", None) else 0.0)  # 3 risk/sustain
    secs = getattr(mv, "secondary", None) or []
    if isinstance(secs, dict):
        secs = [secs]
    max_ch = max((s.get("chance", 0) for s in secs), default=0) / 100.0
    has_st = 1.0 if any("status" in s for s in secs) else 0.0
    out.extend([max_ch, has_st])                                    # 2 secondaries
    flags = (getattr(mv, "entry", None) or {}).get("flags", {})
    out.append(1.0 if flags.get("charge") else 0.0)                # charge turn (Solar Beam)
    out.append(1.0 if flags.get("recharge") else 0.0)              # recharge turn (Hyper Beam)
    maxpp = getattr(mv, "max_pp", 0) or 1
    out.append(min(max(getattr(mv, "current_pp", maxpp) or maxpp, 0), maxpp) / maxpp)  # pp frac
    out.append(1.0 if mv.id in ("sheercold", "fissure", "guillotine", "horndrill") else 0.0)
    eh = float(getattr(mv, "expected_hits", 1) or 1)
    out.append(min(eh, 5.0) / 5.0)                                 # multi-hit expectation
    out.append(1.0 if mv.id in ("encore", "taunt", "disable", "torment") else 0.0)
    out.append(1.0 if mv.id in ("knockoff", "trick", "switcheroo", "corrosivegas") else 0.0)
    vs = str(getattr(mv, "volatile_status", "") or "").lower()
    out.append(1.0 if "confusion" in vs else 0.0)
    out.append(1.0 if "leech" in vs else 0.0)                      # Effect str is "leech_seed"
    out.append(1.0 if "yawn" in vs else 0.0)                       # 10 extended verbs
    return out


def legal_action_mask(battle) -> np.ndarray:
    """Boolean length-26 legal-action mask, computed via poke-env's own action_to_order (so it
    matches exactly what the server will accept). Used by live-play players outside the env."""
    mask = np.zeros(N_ACTIONS, dtype=bool)
    for a in range(N_ACTIONS):
        try:
            SinglesEnv.action_to_order(np.int64(a), battle, fake=False, strict=True)
            mask[a] = True
        except Exception:
            pass
    if not mask.any():
        mask[0] = True
    return mask


class DeepShowdownSinglesEnv(SinglesEnv):
    """poke-env singles env with our float obs, shaped reward, and cached legal-action masks."""

    def __init__(self, hp_value: float = 0.5, fainted_value: float = 1.0,
                 victory_value: float = 30.0, boost_value: float = 0.0,
                 status_value: float = 0.0, **kwargs):
        super().__init__(**kwargs)
        space = Box(low=-np.inf, high=np.inf, shape=(OBS_DIM,), dtype=np.float32)
        self.observation_spaces = {agent: space for agent in self.possible_agents}
        # poke-env wraps embed_battle() into {'observation', 'action_mask'} and computes the
        # legal-action mask itself (from battle.valid_orders), so we don't stash our own.
        self._reward_prev: dict = {}
        self._last_parts: dict = {}          # key -> (shaping, victory) from the last calc_reward
        # Reward weights. Winning DOMINATES: victory_value is set well above the shaping range,
        # which telescopes over an episode to at most hp_value*6 + fainted_value*6 (=9 by default).
        # So the terminal win/loss (±30) is ~3x the largest possible cumulative shaping.
        self.hp_value = hp_value
        self.fainted_value = fainted_value
        self.victory_value = victory_value
        # Long-horizon terms in the SAME potential: stat stages and inflicted status carry
        # intermediate value, so setup/status moves earn dense credit the moment they land
        # (and give it back if squandered — boosts vanish on switch-out). PBRS-flavored:
        # everything still telescopes through one Φ.
        self.boost_value = boost_value
        self.status_value = status_value

    # -- observation -------------------------------------------------------------------------------
    def embed_battle(self, battle):
        return build_observation(battle)

    # -- reward ------------------------------------------------------------------------------------
    def calc_reward(self, battle) -> float:
        # poke-env calls this for BOTH perspectives each step; battle1/battle2 share a battle_tag,
        # so key the running baseline by (tag, username) to keep each side's potential separate.
        key = (battle.battle_tag, battle.player_username)
        my_hp = sum((m.current_hp_fraction or 0.0) for m in battle.team.values())
        # Unrevealed foe mons count as FULL HP: otherwise each reveal adds a healthy mon to the
        # foe's potential, i.e. a spurious negative reward for making the opponent switch.
        known = list(battle.opponent_team.values())
        opp_hp = (6 - len(known)) + sum((m.current_hp_fraction or 0.0) for m in known)
        my_ko = sum(1 for m in battle.team.values() if m.fainted)
        opp_ko = sum(1 for m in battle.opponent_team.values() if m.fainted)
        potential = (my_hp - opp_hp) * self.hp_value + (opp_ko - my_ko) * self.fainted_value
        if self.boost_value:
            me, foe = battle.active_pokemon, battle.opponent_active_pokemon
            my_b = sum(v for v in me.boosts.values() if v > 0) if me else 0
            foe_b = sum(v for v in foe.boosts.values() if v > 0) if foe else 0
            potential += (my_b - foe_b) * self.boost_value
        if self.status_value:
            my_st = sum(1 for m in battle.team.values() if m.status is not None and not m.fainted)
            foe_st = sum(1 for m in battle.opponent_team.values()
                         if m.status is not None and not m.fainted)
            potential += (foe_st - my_st) * self.status_value
        first = key not in self._reward_prev
        shaping = 0.0 if first else potential - self._reward_prev[key]   # dense HP/faint guidance
        self._reward_prev[key] = potential
        victory = 0.0                                                    # terminal win/loss (dominant)
        if battle.finished:
            victory = self.victory_value if battle.won else (
                -self.victory_value if battle.lost else 0.0)
        self._last_parts[key] = (shaping, victory)
        return shaping + victory

    def last_parts(self, battle_tag, username) -> tuple[float, float]:
        """(shaping, victory) components of the most recent calc_reward for this side."""
        return self._last_parts.get((battle_tag, username), (0.0, 0.0))


class MaskedSingleAgentEnv:
    """Single-agent driver over a `DeepShowdownSinglesEnv` vs a fixed poke-env `Player` opponent.

    Mirrors poke-env's `SingleAgentWrapper` (it auto-plays the opponent via `choose_move`) but returns
    the learner's legal-action mask alongside the observation, in the (obs, mask, reward, done) shape
    our PPO loop consumes. reset() -> (obs, mask); step(a) -> (obs, mask, reward, done).
    """

    def __init__(self, env: DeepShowdownSinglesEnv, opponent, frames: int = 1,
                 redistribute: bool = False):
        self.env = env
        self.opponent = opponent
        # Credit redistribution (v1, boosts only): when a boosted attack lands, report the boost's
        # share of realized damage — realized * (1 - 1/stat_mult) * hp_value — so the trainer can
        # MOVE that reward slice from the attack's timestep back to the setup action's timestep.
        # Guards: offensive stat of the move's category only; requires this mon's OWN setup action
        # this stint (cleared on switch/faint, so ability/item boosts never credit a phantom).
        self.redistribute = redistribute
        self._setup_mon: str | None = None
        self.last_is_setup = False
        self.last_redist = 0.0
        # frames=k stacks the previous k-1 observations AFTER the current one, so the current
        # frame keeps positions 0..OBS_DIM-1 and every slot/probe index stays valid.
        self.frames = frames
        self._prev: list = []
        self.obs_dim = OBS_DIM * frames
        self.n_actions = N_ACTIONS
        # Reward breakdown of the last step (learner side): shaping vs terminal victory.
        self.last_shaping = 0.0
        self.last_victory = 0.0

    def _stack(self, o, reset=False):
        if self.frames == 1:
            return o
        if reset or not self._prev:
            self._prev = [o] * (self.frames - 1)
        out = np.concatenate([o] + self._prev)
        self._prev = [o] + self._prev[:-1]
        return out

    @staticmethod
    def _split(agent_obs) -> tuple[np.ndarray, np.ndarray]:
        """poke-env hands back {'observation', 'action_mask'} per agent."""
        return (np.asarray(agent_obs["observation"], dtype=np.float32),
                np.asarray(agent_obs["action_mask"], dtype=bool))

    def reset(self):
        # Prune per-battle dicts (keyed by unique battle_tag) so multi-thousand-game runs
        # don't grow memory: only the CURRENT battle's entries are ever read.
        self.env._reward_prev.clear()
        self.env._last_parts.clear()
        obs, _ = self.env.reset()
        self.opponent.reset_battles()
        assert self.env.battle2 is not None
        self.opponent._battles[self.env.battle2.battle_tag] = self.env.battle2
        o, m = self._split(obs[self.env.agent1.username])
        return self._stack(o, reset=True), m

    @staticmethod
    def _foe_hp(b) -> float:
        known = list(b.opponent_team.values())
        return (6 - len(known)) + sum((m.current_hp_fraction or 0.0) for m in known)

    def step(self, action: int):
        env = self.env
        # opponent order -> action (mirrors SingleAgentWrapper, minus VGC teampreview)
        if env.battle2.wait:
            opp_order = DefaultBattleOrder()
        else:
            opp_order = self.opponent.choose_move(env.battle2)
        opp_action = env.order_to_action(opp_order, env.battle2, fake=env._fake, strict=env._strict)

        # -- pre-step snapshot for boost-credit redistribution ------------------------------------
        self.last_is_setup, self.last_redist = False, 0.0
        atk_stage = 0
        attacker = None
        if self.redistribute and action >= 6 and env.battle1.active_pokemon is not None:
            me = env.battle1.active_pokemon
            attacker = me.species
            moves = list(me.moves.values())[:4]
            idx = (action - 6) % 4
            if idx < len(moves):
                mv = moves[idx]
                is_status = mv.category is not None and mv.category.name == "STATUS"
                if is_status and (mv.boosts and any(v > 0 for v in mv.boosts.values())
                                  and "self" in str(getattr(mv, "target", "")).lower()):
                    self.last_is_setup = True
                    self._setup_mon = attacker
                elif not is_status and mv.category is not None:
                    stat = "atk" if mv.category.name == "PHYSICAL" else "spa"
                    atk_stage = max(0, me.boosts.get(stat, 0))
            pre_foe = self._foe_hp(env.battle1)

        a1, a2 = env.agent1.username, env.agent2.username
        obs, rewards, terms, truncs, _ = env.step({a1: np.int64(action), a2: opp_action})
        done = bool(terms[a1] or truncs[a1])
        self.last_shaping, self.last_victory = env.last_parts(env.battle1.battle_tag, a1)

        # -- attribute the boost's share of realized damage ----------------------------------------
        if self.redistribute:
            if atk_stage > 0 and self._setup_mon == attacker and attacker is not None:
                realized = max(0.0, pre_foe - self._foe_hp(env.battle1))
                mult = (2 + atk_stage) / 2
                self.last_redist = realized * (1.0 - 1.0 / mult) * env.hp_value
            cur = env.battle1.active_pokemon.species if env.battle1.active_pokemon else None
            if done or (self._setup_mon is not None and cur != self._setup_mon):
                self._setup_mon = None          # stint over: switch/faint clears provenance

        o, m = self._split(obs[a1])
        return self._stack(o), m, float(rewards[a1]), done
