"""Serve one policy's choices to `harness/cosim.mjs` over stdio, for deterministic on-policy cosim.

The deterministic sidecar runs the battle **inside real Showdown** from a seed, with our policy
picking the moves; the recorded trace then goes through the existing seed gate, which drives the
engine's `Replicate` executor off the same `PsPrng` and byte-compares state after every decision.
One outcome, no enumeration, no path cap — versus the transplant gate, which could only ask
whether the engine's sampled outcome was *somewhere* in PS's reachable set.

The policy was trained on `encode()` of an engine `State`, so each request is converted through
the certified `convert_state` (`showdown_engine.encode_ps_state`) — the policy sees exactly the
inputs it saw in training.

Protocol (line-delimited JSON on stdin/stdout, one exchange per decision):
    <- {"state": <serialized battle>, "side": "p1"|"p2", "format": "...",
        "requestState": "move"|"switch"|"teampreview", "roster": [rosterIndex per live slot]}
    -> {"action": <0..12>, "rosterIndex": <int|null>, "tera": <bool>}

The caller owns the translation from `rosterIndex` to a PS `switch N` position: PS's live array
reorders as mons switch, the engine's roster does not, and rosterIndex is the only stable bridge.
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

import showdown_engine as se  # noqa: E402

from ppo.model import ActorCritic  # noqa: E402

N_ACTIONS = 13


def build(ckpt_path: str | None, device):
    meta = se.Battle(seed=0)
    embed = {"n_mons": meta.n_mons, "cols": meta.id_columns(), "vocab": meta.vocab_sizes(), "dim": 32}
    hidden, layers, state = 256, 2, None
    if ckpt_path:
        ck = torch.load(ckpt_path, map_location=device, weights_only=False)
        hidden = ck.get("hidden_dim", hidden)
        layers = ck.get("n_hidden_layers", layers)
        embed["dim"] = ck.get("embed_dim", 32)
        state = ck["model"]
    net = ActorCritic(meta.obs_dim, N_ACTIONS, hidden, layers, embed=embed, aux=False).to(device)
    if state is not None:
        net.load_state_dict(state)
    net.eval()
    return net


def main():
    p = argparse.ArgumentParser(description="stdio policy server for on-policy cosim recording")
    p.add_argument("--ckpt", type=str, default=None)
    p.add_argument("--device", type=str, default="cpu")
    p.add_argument("--greedy", action="store_true")
    p.add_argument("--seed", type=int, default=0)
    args = p.parse_args()

    device = torch.device(args.device)
    torch.manual_seed(args.seed)
    net = build(args.ckpt, device)
    # Ready marker: the recorder waits for this before starting the battle, so a slow torch
    # import can't be mistaken for a hung policy.
    print(json.dumps({"ready": True}), flush=True)

    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        req = json.loads(line)
        side_idx = 0 if req.get("side", "p1") == "p1" else 1
        try:
            obs, ids, mask, roster = se.encode_ps_state(
                json.dumps(req["state"]), side_idx, req.get("format", "gen9randombattle"),
                req.get("requestState", "move"))
        except BaseException as e:  # pyo3 surfaces engine panics as PanicException (not Exception)
            # Fall back to "let the recorder pick" rather than killing the run: a convert failure
            # is itself a finding, and the recorder logs it.
            print(json.dumps({"error": str(e)}), flush=True)
            continue

        # PS's own request is the authority on legality; the engine mask is only a second
        # opinion. Intersecting means a *disagreement* costs us an action rather than a rejected
        # choice that aborts the recording — and the divergence still shows up in the seed gate,
        # which is the thing actually being measured.
        #
        # Switch targets arrive as stable ROSTER indices (PS's live array reorders as mons switch
        # out; roster indices don't), so they are matched through `roster[a]` from the encoder.
        m = np.asarray(mask, dtype=bool)
        am = np.zeros(N_ACTIONS, dtype=bool)
        allowed_moves = req.get("allowedMoves", [])
        allowed_roster = set(req.get("allowedRoster", []))
        can_tera = bool(req.get("canTera", False))
        for i in allowed_moves:
            am[i] = True
            if can_tera:
                am[9 + i] = True
        for k in range(5):
            if int(roster[4 + k]) in allowed_roster:
                am[4 + k] = True
        m &= am
        if not m.any():
            print(json.dumps({"action": None}), flush=True)
            continue

        with torch.no_grad():
            o = torch.as_tensor(np.asarray(obs, np.float32)[None], device=device)
            i = torch.as_tensor(np.asarray(ids, np.int64)[None], device=device)
            k = torch.as_tensor(m[None], device=device)
            if args.greedy:
                logits, _ = net.forward(o, k, obs_ids=i)
                a = int(logits.argmax(-1).item())
            else:
                act, _, _, _ = net.act(o, k, obs_ids=i)
                a = int(act.item())

        ri = int(roster[a]) if 4 <= a <= 8 else None
        print(json.dumps({"action": a, "rosterIndex": ri, "tera": a >= 9}), flush=True)


if __name__ == "__main__":
    main()
