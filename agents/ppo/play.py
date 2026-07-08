"""Play games with a trained checkpoint on a (local) Pokemon Showdown server.

Prereq: a running server —  (cd engines/pokemon-showdown && node pokemon-showdown start --no-security)

Two modes:

  vs      Model plays N games vs a scripted baseline, logging human-readable games.
            python -m ppo.play --mode vs --opponent heuristic --games 5 \
                --log-dir games/ --replays

  accept  Model connects as a named bot and ACCEPTS challenges — so you can play it from the
          Showdown web client at http://localhost:8000 :
            python -m ppo.play --mode accept --username FableBot --games 10 --log-dir games/
          then in the browser: pick any name, search "FableBot", Challenge -> gen9randombattle.

Default checkpoint is Fable's latest slot-arch model; override with --checkpoint.
"""

from __future__ import annotations

import argparse
import asyncio
import logging
from pathlib import Path

from poke_env.player import RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer
from poke_env.ps_client.account_configuration import AccountConfiguration
from poke_env.ps_client.server_configuration import LocalhostServerConfiguration

from .model_player import ModelPlayer

DEFAULT_CKPT = "runs/slot2/model_139264.pt"
OPPONENTS = {"random": RandomPlayer, "maxbp": MaxBasePowerPlayer, "heuristic": SimpleHeuristicsPlayer}


def render_log(raw_path: Path):
    """Turn a raw Showdown protocol .log into a readable turn-by-turn .txt narration."""
    lines = raw_path.read_text().splitlines()
    out, turn = [], 0
    for ln in lines:
        p = ln.split("|")[1:]
        if not p:
            continue
        tag = p[0]
        if tag == "turn":
            turn = p[1]; out.append(f"\n--- Turn {turn} ---")
        elif tag == "move" and len(p) >= 4:
            out.append(f"  {p[1]} used {p[2]}" + (f" -> {p[3]}" if p[3] else ""))
        elif tag == "switch" and len(p) >= 3:
            out.append(f"  {p[1].split(':')[0]} sent out {p[2].split(',')[0]}")
        elif tag == "-damage" and len(p) >= 3:
            out.append(f"      {p[1]} -> {p[2].split(' ')[0]}")
        elif tag == "-heal" and len(p) >= 3:
            out.append(f"      {p[1]} healed -> {p[2].split(' ')[0]}")
        elif tag == "faint":
            out.append(f"  {p[1]} fainted")
        elif tag in ("-status", "-boost", "-unboost", "-crit", "-supereffective", "-immune",
                     "-miss", "-terastallize") and len(p) >= 2:
            out.append(f"      ({tag[1:]}: {' '.join(p[1:])})")
        elif tag == "win":
            out.append(f"\n== {p[1]} won ==")
        elif tag == "tie":
            out.append("\n== tie ==")
    txt = raw_path.with_suffix(".txt")
    txt.write_text("\n".join(out) + "\n")
    return txt


async def run_vs(args):
    server = LocalhostServerConfiguration
    # vs mode: let poke-env auto-generate a unique username (a fixed one collides across runs /
    # with a running accept-mode bot and hangs on |nametaken|). The name only matters for accept.
    model = ModelPlayer(args.checkpoint, battle_format=args.format, greedy=not args.stochastic,
                        verbose=args.verbose, log_dir=args.log_dir,
                        save_replays=(args.replay_dir or False),
                        server_configuration=server)
    opp = OPPONENTS[args.opponent](battle_format=args.format, server_configuration=server)
    print(f"[{model.arch} @ {args.checkpoint}] vs {args.opponent} x{args.games} ...", flush=True)
    await model.battle_against(opp, n_battles=args.games)
    print(f"model record: {model.n_won_battles}/{args.games} wins", flush=True)
    if args.log_dir:  # prettify each raw log we wrote
        for raw in sorted(Path(args.log_dir).glob("*.log")):
            print("  game log:", render_log(raw))


async def run_accept(args):
    model = ModelPlayer(args.checkpoint, battle_format=args.format, greedy=not args.stochastic,
                        verbose=args.verbose, log_dir=args.log_dir,
                        save_replays=(args.replay_dir or False),
                        server_configuration=LocalhostServerConfiguration,
                        account_configuration=AccountConfiguration(args.username, None))
    print("=" * 66)
    print(f"  Bot '{args.username}' [{model.arch} @ {args.checkpoint}] is online.")
    print(f"  Open http://localhost:8000  ->  pick any name  ->  search '{args.username}'")
    print(f"  ->  Challenge  ->  format '{args.format}'.  Accepting {args.games} game(s).")
    print("=" * 66, flush=True)
    await model.accept_challenges(None, args.games)
    print(f"done — played {args.games} game(s); record {model.n_won_battles} wins.", flush=True)
    if args.log_dir:
        for raw in sorted(Path(args.log_dir).glob("*.log")):
            render_log(raw)


def main():
    p = argparse.ArgumentParser(description="Play/observe a checkpoint on a Showdown server.")
    p.add_argument("--mode", choices=["vs", "accept"], default="vs")
    p.add_argument("--checkpoint", default=DEFAULT_CKPT)
    p.add_argument("--format", default="gen9randombattle")
    p.add_argument("--username", default="FableBot")
    p.add_argument("--opponent", choices=list(OPPONENTS), default="heuristic")
    p.add_argument("--games", type=int, default=5)
    p.add_argument("--log-dir", default=None, help="write human-readable game logs here")
    p.add_argument("--replay-dir", default=None, help="save poke-env replay HTML here")
    p.add_argument("--stochastic", action="store_true", help="sample actions instead of greedy argmax")
    p.add_argument("--verbose", action="store_true", help="print per-turn decisions (chosen action, probs, value)")
    args = p.parse_args()
    logging.basicConfig(level=logging.WARNING)
    asyncio.run((run_accept if args.mode == "accept" else run_vs)(args))


if __name__ == "__main__":
    main()
