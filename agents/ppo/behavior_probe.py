"""Long-horizon behavior probe: measures whether a checkpoint has learned DELAYED-PAYOFF play
(setup/stat-boost moves, hazards, status, recovery) rather than pure greedy damage.

Companion to agents/EVAL_RUBRIC.md — each metric here is a rubric row. Run manually when judging
a checkpoint (no fixed cadence):

    python -m ppo.behavior_probe runs/long-med/model_final.pt --opponent maxbp --games 30 \
        --out probes/longmed_vs_maxbp.json

Needs a running Showdown server. Flake-tolerant (rebuilds its env and keeps going).
"""

from __future__ import annotations

import argparse
import collections
import json
import sys
from pathlib import Path

import numpy as np
import torch

from poke_env.player import RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer

from .model_player import load_model
from .pokeenv_env import DeepShowdownSinglesEnv, MaskedSingleAgentEnv

OPPONENTS = {"random": RandomPlayer, "maxbp": MaxBasePowerPlayer, "heuristic": SimpleHeuristicsPlayer}

SETUP_EXTRA = {"bellydrum", "clangoroussoul", "filletaway", "noretreat", "victorydance",
               "tidyup", "curse", "shellsmash", "shiftgear", "quiverdance", "dragondance"}
HAZARD_IDS = {"stealthrock", "spikes", "toxicspikes", "stickyweb", "stoneaxe", "ceaselessedge"}
RECOVERY_IDS = {"recover", "roost", "slackoff", "softboiled", "moonlight", "morningsun",
                "synthesis", "shoreup", "milkdrink", "rest", "strengthsap"}


def classify(mv) -> str:
    if mv.id in HAZARD_IDS:
        return "hazard"
    if mv.category is not None and mv.category.name == "STATUS":
        boosts = mv.boosts or {}
        tgt = str(getattr(mv, "target", "")).lower()
        if mv.id in SETUP_EXTRA or (boosts and any(v > 0 for v in boosts.values()) and "self" in tgt):
            return "setup"
        if mv.status is not None:
            return "status"
        if (getattr(mv, "heal", 0) or 0) > 0 or mv.id in RECOVERY_IDS:
            return "recovery"
        return "other_status"
    return "attack"


def foe_team_hp(battle) -> float:
    """Known foe HP + unknown mons assumed full — a consistent chip-damage yardstick."""
    known = list(battle.opponent_team.values())
    return (6 - len(known)) + sum((m.current_hp_fraction or 0.0) for m in known)


def boost_sum(mon) -> int:
    return sum(v for v in mon.boosts.values() if v > 0) if mon else 0


def probe(ckpt: str, opponent: str, games: int, fmt: str, seed: int = 0) -> dict:
    model, arch, _ = load_model(ckpt)
    frames = getattr(model, "frames", 1)

    def make():
        env = DeepShowdownSinglesEnv(battle_format=fmt)
        return env, MaskedSingleAgentEnv(env, OPPONENTS[opponent](battle_format=fmt,
                                                                  start_listening=False),
                                         frames=frames)
    env, wrap = make()
    np.random.seed(seed)

    g = collections.Counter()          # global counters
    per_game_max_boost, first_hazard_turns, post_setup_ratios = [], [], []
    dmg_per_attack_all = []
    wins = flakes = 0
    played = 0

    while played < games:
        try:
            obs, mask = wrap.reset()
        except Exception:
            flakes += 1
            try: env.close()
            except Exception: pass
            env, wrap = make()
            continue
        done = False
        max_boost = 0
        hazard_turn = None
        # pending setup events: dicts {left, dmg, mon, attacked} — watch the boosted mon's next
        # few decisions to see whether the investment converts into damage or is wasted.
        pending: list[dict] = []

        def resolve(ev):
            g["setup_resolved"] += 1
            if not ev["attacked"]:
                g["setup_wasted"] += 1        # boosted, then left the field/fainted without attacking
            post_setup_ratios.append(ev["dmg"] / 3.0)

        try:
            while not done:
                b = env.battle1
                me, foe = b.active_pokemon, b.opponent_active_pokemon
                cur_species = me.species if me else None
                pre_foe_hp = foe_team_hp(b)
                pre_faints = sum(1 for m in b.opponent_team.values() if m.fainted)
                pre_boost = boost_sum(me)
                max_boost = max(max_boost, pre_boost)
                forced = bool(b.force_switch)

                with torch.no_grad():
                    lg, _ = model.forward(torch.tensor(obs).unsqueeze(0).float(),
                                          torch.tensor(mask).unsqueeze(0).bool())
                a = int(lg.argmax(-1).item())

                cat, mv = "switch", None
                if a >= 6:
                    moves = list(me.moves.values())[:4] if me else []
                    idx = (a - 6) % 4
                    if idx < len(moves):
                        mv = moves[idx]
                        cat = classify(mv)

                if not forced:
                    g["decisions"] += 1
                    g[f"cat_{cat}"] += 1
                    if cat == "setup":
                        g["setup_uses"] += 1
                        if (me.current_hp_fraction or 0) >= 0.5:
                            g["setup_safe"] += 1
                        # left=4: the same-step pass below consumes one, leaving 3 true
                        # post-setup decisions in the watch window.
                        pending.append(dict(left=4, dmg=0.0, mon=cur_species, attacked=False))
                    elif cat == "hazard" and hazard_turn is None:
                        hazard_turn = b.turn
                        g["hazard_games"] += 1
                    elif cat == "status" and foe is not None:
                        g["status_uses"] += 1
                        if foe.status is not None:
                            g["status_redundant"] += 1
                    elif cat == "recovery":
                        g["recovery_uses"] += 1
                        if 0.2 <= (me.current_hp_fraction or 0) <= 0.7:
                            g["recovery_well_timed"] += 1

                obs, mask, _r, done = wrap.step(a)

                # -- payoff attribution after the step ------------------------------------------
                b = env.battle1
                dealt = max(0.0, pre_foe_hp - foe_team_hp(b))
                if cat == "attack" and not forced:
                    dmg_per_attack_all.append(dealt)
                new_faints = sum(1 for m in b.opponent_team.values() if m.fainted) - pre_faints
                if new_faints > 0:
                    g["kos"] += new_faints
                    if pre_boost >= 2:
                        g["boosted_kos"] += new_faints
                still = []
                for ev in pending:                        # watch 3 decisions after each setup
                    if ev["mon"] != cur_species:          # the boosted mon left the field
                        resolve(ev)
                        continue
                    ev["dmg"] += dealt
                    ev["attacked"] |= (cat == "attack")
                    ev["left"] -= 1
                    (resolve(ev) if ev["left"] <= 0 else still.append(ev))
                pending = still
            for ev in pending:                            # game ended with a live watch window
                resolve(ev)
        except Exception:
            flakes += 1
            try: env.close()
            except Exception: pass
            env, wrap = make()
            continue
        played += 1
        wins += 1 if env.battle1.won else 0
        per_game_max_boost.append(max_boost)
        if hazard_turn is not None:
            first_hazard_turns.append(hazard_turn)
        g["turns_total"] += env.battle1.turn
    env.close()

    d = max(1, g["decisions"])
    su = max(1, g["setup_uses"])
    base_dmg = float(np.mean(dmg_per_attack_all)) if dmg_per_attack_all else 0.0
    out = {
        "checkpoint": ckpt, "arch": arch, "opponent": opponent, "games": played,
        "win_rate": round(wins / played, 3), "avg_turns": round(g["turns_total"] / played, 1),
        "flakes": flakes,
        "decision_mix": {k.replace("cat_", ""): round(g[k] / d, 3)
                         for k in sorted(g) if k.startswith("cat_")},
        "setup": {
            "uses_per_game": round(g["setup_uses"] / played, 2),
            "games_with_setup_frac": round(sum(1 for m in per_game_max_boost if m >= 2) / played, 2),
            "safe_use_frac": round(g["setup_safe"] / su, 2),
            "avg_max_boost": round(float(np.mean(per_game_max_boost)), 2),
            "boosted_kos_per_game": round(g["boosted_kos"] / played, 2),
            "wasted_frac": round(g["setup_wasted"] / max(1, g["setup_resolved"]), 2),
            "post_setup_dmg_per_decision": round(float(np.mean(post_setup_ratios)), 3)
                if post_setup_ratios else None,
            "baseline_dmg_per_attack": round(base_dmg, 3),
        },
        "hazards": {
            "games_with_hazards_frac": round(g["hazard_games"] / played, 2),
            "avg_first_hazard_turn": round(float(np.mean(first_hazard_turns)), 1)
                if first_hazard_turns else None,
        },
        "status": {
            "uses_per_game": round(g["status_uses"] / played, 2),
            "redundant_frac": round(g["status_redundant"] / max(1, g["status_uses"]), 2),
        },
        "recovery": {
            "uses_per_game": round(g["recovery_uses"] / played, 2),
            "well_timed_frac": round(g["recovery_well_timed"] / max(1, g["recovery_uses"]), 2),
        },
        "kos_per_game": round(g["kos"] / played, 2),
    }
    return out


def main():
    p = argparse.ArgumentParser(description="Long-horizon behavior probe (see agents/EVAL_RUBRIC.md).")
    p.add_argument("checkpoint")
    p.add_argument("--opponent", choices=list(OPPONENTS), default="maxbp")
    p.add_argument("--games", type=int, default=30)
    p.add_argument("--format", default="gen9randombattle")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--out", default=None, help="also write the JSON here")
    args = p.parse_args()
    res = probe(args.checkpoint, args.opponent, args.games, args.format, args.seed)
    print(json.dumps(res, indent=2))
    if args.out:
        Path(args.out).parent.mkdir(parents=True, exist_ok=True)
        Path(args.out).write_text(json.dumps(res, indent=2) + "\n")
        print(f"\nwrote {args.out}", file=sys.stderr)


if __name__ == "__main__":
    main()
