"""Behavior probe: what does the trained model actually DO in games?

Plays a checkpoint vs the scripted heuristic, classifies every learner action from the engine's
true state (setup / hazard / recovery / status / damaging move by category / switch / tera), and
tallies per-game rates plus outcome-conditioned splits. Optionally dumps full PS-protocol
transcripts of one won and one lost game for qualitative review (the standing hero-run
directive: look at actual games, not just win rates).

    .venv/bin/python -m probes.behavior_probe <ckpt> [--games 200]
"""

from __future__ import annotations

import argparse
import json
from collections import Counter

import numpy as np
import torch

from ppo.flow_env import FlowEnvVec
from ppo.flow_eval import _policy_actions, make_scripted_heuristic
from probes.mcts_calib import POOL, load_ckpt

RECOVERY = {"recover", "softboiled", "roost", "slackoff", "moonlight", "morningsun",
            "synthesis", "shoreup", "milkdrink", "strengthsap", "healorder", "rest", "wish",
            "junglehealing", "lunarblessing"}
HAZARDS = {"spikes", "stealthrock", "stickyweb", "toxicspikes"}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("ckpt")
    ap.add_argument("--games", type=int, default=200)
    ap.add_argument("--envs", type=int, default=64)
    ap.add_argument("--transcripts", type=int, default=2)
    args = ap.parse_args()

    device = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    net, step = load_ckpt(args.ckpt, device)
    env = FlowEnvVec(args.envs, seed=777_001, team_pool=POOL,
                     fog_species=net.fog_species, obs_version=net.obs_version,
                     frames=net.frames, capture_protocol=True)
    heur = make_scripted_heuristic()
    rng = np.random.default_rng(7)
    learner_side = (np.arange(args.envs) % 2).astype(np.int64)

    stats = Counter()
    per_game = []                      # (won, Counter) per finished game
    game_acc = [Counter() for _ in range(args.envs)]
    boosts_up = np.zeros(args.envs)    # learner's current positive boost stages
    finished = 0
    transcripts = []

    while finished < args.games:
        acts = {}
        for side in (0, 1):
            obs, ids, mask, acting = env._sides()[side]
            is_l = learner_side == side
            act = np.zeros(args.envs, dtype=np.int64)
            if is_l.any():
                act[is_l] = _policy_actions(net, obs[is_l], ids[is_l], mask[is_l], device)
                # classify learner actions on acting rows
                for e in np.flatnonzero(is_l & acting):
                    a = int(act[e])
                    g = game_acc[e]
                    g["decisions"] += 1
                    if 4 <= a <= 8:
                        g["switch"] += 1
                        continue
                    st = json.loads(env.vec.state_json(int(e)))
                    me = st["sides"][side]
                    mv = me["pokemon"][me["active_index"]]["moves"][a % 4 if a < 4 else (a - 9)]
                    if a >= 9:
                        g["tera"] += 1
                    mid = mv["id"]
                    if mid in HAZARDS:
                        g["hazard"] += 1
                    elif mid in RECOVERY:
                        g["recovery"] += 1
                    elif mv["base_power"] == 0 and mv["self_boost_total"] >= 2:
                        g["setup"] += 1
                    elif mv["base_power"] == 0:
                        g["status"] += 1
                    else:
                        g["attack"] += 1
                        g[f'attack_{mv["category"]}'] += 1
                        # boosted KO bookkeeping: attack while at +2 or more total boosts
                        b = me["boosts"]
                        if b["atk"] + b["spa"] >= 2:
                            g["boosted_attack"] += 1
            opp = ~is_l
            if opp.any():
                envs_list = [(int(e), side) for e in np.flatnonzero(opp)]
                act[opp] = heur(env.vec, envs_list, mask[opp], rng)
            acts[side] = act
        done_np, win_np = env.vec.step_all(acts[0], acts[1], True)
        env._cache = None
        done = np.asarray(done_np, dtype=bool)
        winner = np.asarray(win_np, dtype=np.int64)
        for e in np.flatnonzero(done):
            won = winner[e] == learner_side[e]
            per_game.append((bool(won), game_acc[e]))
            if len(transcripts) < args.transcripts * 4:
                try:
                    transcripts.append((bool(won), env.vec.protocol_log(int(e))))
                except Exception:
                    pass
            game_acc[e] = Counter()
            learner_side[e] = rng.integers(0, 2)
            finished += 1

    won_games = [c for w, c in per_game if w]
    lost_games = [c for w, c in per_game if not w]

    def rates(games, label):
        if not games:
            return
        n = len(games)
        keys = ["decisions", "switch", "attack", "attack_physical", "attack_special",
                "setup", "boosted_attack", "hazard", "recovery", "status", "tera"]
        print(f"{label} (n={n}): " + "  ".join(
            f"{k}={sum(g[k] for g in games)/n:.2f}" for k in keys))

    print(f"ckpt {args.ckpt} @ {step:,} steps — {finished} games vs heuristic, "
          f"win {len(won_games)}/{finished}")
    rates([c for _, c in per_game], "ALL /game")
    rates(won_games, "WON /game")
    rates(lost_games, "LOST/game")

    picked_w = picked_l = False
    for won, log in transcripts:
        if won and not picked_w:
            print("\n===== SAMPLE WON GAME (learner perspective varies) =====")
            print("\n".join(log[-120:]))
            picked_w = True
        if not won and not picked_l:
            print("\n===== SAMPLE LOST GAME =====")
            print("\n".join(log[-120:]))
            picked_l = True


if __name__ == "__main__":
    main()
