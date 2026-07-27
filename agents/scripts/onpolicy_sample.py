"""Sample **on-policy** decisions out of the Rust engine for differential replay in Showdown.

The parity corpora are PS-led: Showdown plays a game with random-ish choices and the engine has
to reproduce it. That certifies the states *those* games visit, which is not the distribution a
trained policy visits — and RL adversarially searches for reward, so any divergence in the states
the policy actually reaches is exploitable. `RESEARCH_PLAN.md` §P1.1 calls engine-led on-policy
cosim non-negotiable before a scale run; this is its sampler.

For each decision point along a self-play game driven by a checkpoint, it records:
  * `pre`     — the engine's true state as a PS `deserializeBattle`-loadable snapshot (the
                certified exporter), with the battle seed written in
  * `choices` — what each acting side chose, as PS choice strings (resolved in Rust against the
                same state the exporter serialized, so the switch indexing cannot drift)
  * `legal`   — the engine's legal-action mask per side, for the legality cross-check
  * `post`    — the same snapshot after the engine resolved the decision

`harness/onpolicy-gate.mjs` consumes the JSONL and does the Showdown half.

    python scripts/onpolicy_sample.py --ckpt runs/scale1/ckpt_...pt --games 20 --out samples.jsonl
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

import numpy as np
import torch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ppo.flow_env import BLUE, RED, FlowEnvVec  # noqa: E402
from ppo.model import ActorCritic  # noqa: E402

DEFAULT_POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"


def load_policy(ckpt_path: str | None, env, device):
    """The checkpoint's policy, or a random-init one when no checkpoint exists yet.

    A brand-new run has no checkpoint for the first few minutes; sampling a random policy is
    still a valid (if less interesting) on-policy distribution, so the sidecar never has to
    block on training.
    """
    embed = {"n_mons": env.n_mons, "cols": env.id_columns, "vocab": env.vocab, "dim": 32}
    hidden, layers = 256, 2
    state = None
    if ckpt_path:
        ck = torch.load(ckpt_path, map_location=device, weights_only=False)
        hidden = ck.get("hidden_dim", hidden)
        layers = ck.get("n_hidden_layers", layers)
        embed["dim"] = ck.get("embed_dim", 32)
        state = ck["model"]
    net = ActorCritic(env.obs_dim, env.n_actions, hidden, layers, embed=embed, aux=False).to(device)
    if state is not None:
        net.load_state_dict(state)
    net.eval()
    return net


@torch.no_grad()
def sample_actions(net, obs, ids, mask, device, greedy: bool):
    obs_t = torch.as_tensor(obs, device=device)
    ids_t = torch.as_tensor(ids, device=device)
    mask_t = torch.as_tensor(mask, device=device)
    if greedy:
        logits, _ = net.forward(obs_t, mask_t, obs_ids=ids_t)
        return logits.argmax(dim=-1).cpu().numpy()
    action, _, _, _ = net.act(obs_t, mask_t, obs_ids=ids_t)
    return action.cpu().numpy()


def main():
    p = argparse.ArgumentParser(description="Dump on-policy engine decisions for PS differential replay.")
    p.add_argument("--ckpt", type=str, default=None, help="policy checkpoint (default: random init)")
    p.add_argument("--games", type=int, default=20, help="battles to play concurrently")
    p.add_argument("--max-decisions", type=int, default=400, help="cap per battle")
    p.add_argument("--stride", type=int, default=1, help="record every Nth decision (1 = all)")
    p.add_argument("--team-pool", type=str, default=DEFAULT_POOL)
    p.add_argument("--out", type=str, required=True)
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--greedy", action="store_true",
                   help="argmax instead of sampling (records the exploitation distribution)")
    p.add_argument("--device", type=str, default="cpu",
                   help="cpu by default: the sidecar must not contend with the trainer for the GPU")
    args = p.parse_args()

    pool = args.team_pool
    if pool and not Path(pool).is_absolute():
        pool = str((Path(__file__).resolve().parents[1] / pool).resolve())

    device = torch.device(args.device)
    env = FlowEnvVec(args.games, seed=args.seed, team_pool=pool, max_requests=args.max_decisions)
    net = load_policy(args.ckpt, env, device)

    out = Path(args.out)
    out.parent.mkdir(parents=True, exist_ok=True)
    # A battle seed per env. It goes into the exported snapshot, so the PS side deserializes a
    # battle whose PRNG starts exactly where the engine says it does.
    rng = np.random.default_rng(args.seed ^ 0xC0FFEE)
    seeds = rng.integers(0, 0xFFFF, size=(args.games, 4)).astype(int)

    n_written, n_done, step = 0, 0, 0
    live = np.ones(args.games, dtype=bool)  # envs still on their FIRST battle (auto-reset ends it)

    with out.open("w") as fh:
        while live.any() and step < args.max_decisions:
            obs_r, ids_r, mask_r, act_r = env._sides()[RED]
            obs_b, ids_b, mask_b, act_b = env._sides()[BLUE]
            a_red = sample_actions(net, obs_r, ids_r, mask_r, device, args.greedy)
            a_blue = sample_actions(net, obs_b, ids_b, mask_b, device, args.greedy)

            record = (step % args.stride) == 0
            pending = []
            if record:
                for i in range(args.games):
                    if not live[i]:
                        continue
                    entry = {
                        "env": i, "step": step, "seed": [int(x) for x in seeds[i]],
                        "pre": json.loads(env.vec.export_state(i, [int(x) for x in seeds[i]])),
                        "legal": {"p1": [bool(x) for x in mask_r[i]], "p2": [bool(x) for x in mask_b[i]]},
                        "acting": {"p1": bool(act_r[i]), "p2": bool(act_b[i])},
                        "choices": {},
                    }
                    if act_r[i]:
                        entry["choices"]["p1"] = env.vec.choice_str(i, RED, int(a_red[i]))
                    if act_b[i]:
                        entry["choices"]["p2"] = env.vec.choice_str(i, BLUE, int(a_blue[i]))
                    pending.append(entry)

            # Drive the physical sides directly — this sampler has no learner/opponent split and
            # wants no reward bookkeeping, so it steps the bridge rather than FlowEnvVec.step.
            # `step_all` auto-resets a finished env in place, so its post-state would belong to a
            # brand-new battle: snapshot `post` only for envs that are still going.
            done_np, _ = env.vec.step_all(a_red.astype(np.int64), a_blue.astype(np.int64), True)
            env._cache = None
            done = np.asarray(done_np, dtype=bool)

            for entry in pending:
                i = entry["env"]
                if done[i]:
                    entry["terminal"] = True  # no post-state: the env was recycled
                else:
                    entry["post"] = json.loads(env.vec.export_state(i, [int(x) for x in seeds[i]]))
                fh.write(json.dumps(entry) + "\n")
                n_written += 1

            n_done += int((done & live).sum())
            live &= ~done
            step += 1

    print(f"wrote {n_written} on-policy decisions from {n_done} completed battles "
          f"({args.games} envs, {step} steps) -> {out}")


if __name__ == "__main__":
    main()
