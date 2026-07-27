"""E1 behavior cloning: fit the ActorCritic to a decision dataset, save a train_flow-loadable ckpt.

Policy head: masked cross-entropy on the teacher's actions. Value head: MSE to the discounted
terminal outcome. Runs entirely on the GPU — fine to run while scale2 trains (CPU untouched).

    .venv/bin/python -m probes.bc_train runs/probes/bc-heur.npz --out runs/probes/bc-heur-init.pt
"""

from __future__ import annotations

import argparse
import time

import numpy as np
import torch
import torch.nn.functional as F

import showdown_engine as se

from ppo.model import ActorCritic


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("data")
    ap.add_argument("--out", required=True)
    ap.add_argument("--hidden-dim", type=int, default=1024)
    ap.add_argument("--n-hidden-layers", type=int, default=3)
    ap.add_argument("--embed-dim", type=int, default=48)
    ap.add_argument("--epochs", type=int, default=4)
    ap.add_argument("--batch", type=int, default=16384)
    ap.add_argument("--lr", type=float, default=3e-4)
    ap.add_argument("--value-coef", type=float, default=0.5)
    ap.add_argument("--holdout", type=float, default=0.02)
    args = ap.parse_args()

    torch.set_float32_matmul_precision("high")
    dev = torch.device("cuda" if torch.cuda.is_available() else "cpu")
    d = np.load(args.data)
    n = len(d["action"])
    print(f"{n:,} decisions from {args.data}")

    meta = se.Battle(seed=0)
    embed = {"n_mons": meta.n_mons, "cols": meta.id_columns(), "vocab": meta.vocab_sizes(),
             "dim": args.embed_dim}
    net = ActorCritic(int(d["obs_dim"]), 13, args.hidden_dim, args.n_hidden_layers,
                      embed=embed, aux=False).to(dev)
    opt = torch.optim.Adam(net.parameters(), lr=args.lr, fused=(dev.type == "cuda"))

    perm = np.random.default_rng(0).permutation(n)
    n_hold = int(n * args.holdout)
    hold, train = perm[:n_hold], perm[n_hold:]
    t = {k: torch.as_tensor(d[k]) for k in ("obs", "ids", "mask", "action", "ret")}

    def batch_of(idx):
        return (t["obs"][idx].to(dev), t["ids"][idx].to(dev), t["mask"][idx].to(dev),
                t["action"][idx].to(dev), t["ret"][idx].to(dev))

    @torch.no_grad()
    def holdout_acc():
        correct = tot = 0
        for s in range(0, len(hold), args.batch):
            obs, ids, mask, act, _ = batch_of(hold[s:s + args.batch])
            logits, _ = net.forward(obs, mask, obs_ids=ids)
            correct += (logits.argmax(-1) == act).sum().item()
            tot += len(act)
        return correct / max(1, tot)

    t0 = time.time()
    for ep in range(args.epochs):
        np.random.default_rng(ep).shuffle(train)
        tot_pi = tot_v = nb = 0
        for s in range(0, len(train), args.batch):
            obs, ids, mask, act, ret = batch_of(train[s:s + args.batch])
            logits, value = net.forward(obs, mask, obs_ids=ids)
            pi_loss = F.cross_entropy(logits, act)
            v_loss = F.mse_loss(value, ret)
            opt.zero_grad()
            (pi_loss + args.value_coef * v_loss).backward()
            opt.step()
            tot_pi += pi_loss.item(); tot_v += v_loss.item(); nb += 1
        print(f"epoch {ep}: pi {tot_pi/nb:.4f}  v {tot_v/nb:.4f}  "
              f"holdout_acc {holdout_acc():.4f}  ({time.time()-t0:,.0f}s)", flush=True)

    torch.save({"model": net.state_dict(), "global_step": 0,
                "obs_dim": int(d["obs_dim"]), "n_actions": 13,
                "hidden_dim": args.hidden_dim, "n_hidden_layers": args.n_hidden_layers,
                "embed_dim": args.embed_dim, "bc_teacher": args.data}, args.out)
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
