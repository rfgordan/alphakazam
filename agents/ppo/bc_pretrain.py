"""Behavior-clone SimpleHeuristicsPlayer (~0.98 vs random) into our policy net AND pretrain the value
head on the teacher's discounted returns, so PPO can fine-tune from BOTH a good policy and a good
critic (no random-value-head advantage churn wrecking the policy). Then --init-from this checkpoint.

    python -m ppo.bc_pretrain --samples 60000 --epochs 8 --out runs/bc/model_bc.pt
    python -m ppo.pokeenv_train --init-from runs/bc/model_bc.pt ...   # value head already warm

Reward weights + gamma here MUST match the fine-tune's, so the pretrained value function is on the
same scale as the returns PPO will compute.
"""
from __future__ import annotations
import argparse, resource, time
from pathlib import Path
import numpy as np, torch
import torch.nn as nn

from poke_env.player import RandomPlayer, SimpleHeuristicsPlayer
from .model import ActorCritic
from .pokeenv_env import DeepShowdownSinglesEnv, MaskedSingleAgentEnv, OBS_DIM, N_ACTIONS


def collect(env, wrap, teacher, n_samples, gamma):
    """Teacher plays vs random; record every step's (obs, action[-1 if not teacher], mask, reward, done),
    then compute discounted returns-to-go. Policy trains on teacher steps; value trains on ALL steps."""
    obs_l, act_l, mask_l, rew_l, done_l = [], [], [], [], []
    obs, mask = wrap.reset(); n_teacher = 0
    while n_teacher < n_samples:
        b1 = env.battle1
        act = None
        if not b1.force_switch:
            try:
                a = int(env.order_to_action(teacher.choose_move(b1), b1, fake=env._fake, strict=False))
                if 0 <= a < N_ACTIONS and mask[a]:
                    act = a
            except Exception:
                pass
        step_a = act if act is not None else int(np.random.choice(np.flatnonzero(mask)))
        obs_l.append(obs); act_l.append(act if act is not None else -1); mask_l.append(mask)
        obs, mask, r, done = wrap.step(step_a)
        rew_l.append(r); done_l.append(done); n_teacher += (act is not None)
        if done:
            obs, mask = wrap.reset()
    # discounted returns-to-go, reset at episode boundaries (done[t] = terminal step t)
    N = len(rew_l); returns = np.zeros(N, np.float32); R = 0.0
    for t in range(N - 1, -1, -1):
        if done_l[t]:
            R = 0.0
        R = rew_l[t] + gamma * R
        returns[t] = R
    return (np.array(obs_l, np.float32), np.array(act_l, np.int64),
            np.array(mask_l, bool), returns)


@torch.no_grad()
def winrate(model, env2, wrap2, episodes):
    wins = 0
    for _ in range(episodes):
        obs, mask = wrap2.reset(); done = False
        while not done:
            logits, _ = model.forward(torch.tensor(obs).unsqueeze(0).float(),
                                      torch.tensor(mask).unsqueeze(0).bool())
            obs, mask, _, done = wrap2.step(int(logits.argmax(-1).item()))
        wins += 1 if env2.battle1.won else 0
    return wins / episodes


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--samples", type=int, default=60000)
    p.add_argument("--epochs", type=int, default=8)
    p.add_argument("--batch-size", type=int, default=512)
    p.add_argument("--lr", type=float, default=1e-3)
    p.add_argument("--value-coef", type=float, default=0.5)
    p.add_argument("--gamma", type=float, default=0.99)
    p.add_argument("--hidden-dim", type=int, default=256)
    p.add_argument("--hp-value", type=float, default=0.5)
    p.add_argument("--fainted-value", type=float, default=1.5)
    p.add_argument("--victory-value", type=float, default=20.0)
    p.add_argument("--format", default="gen9randombattle")
    p.add_argument("--eval-episodes", type=int, default=200)
    p.add_argument("--out", default="runs/bc/model_bc.pt")
    args = p.parse_args()

    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, min(hard, 8192)), hard))

    def mkenv():
        e = DeepShowdownSinglesEnv(battle_format=args.format, hp_value=args.hp_value,
                                   fainted_value=args.fainted_value, victory_value=args.victory_value)
        return e, MaskedSingleAgentEnv(e, RandomPlayer(battle_format=args.format, start_listening=False))

    env, wrap = mkenv()
    teacher = SimpleHeuristicsPlayer(battle_format=args.format, start_listening=False)
    print(f"collecting {args.samples} teacher decisions (reward hp={args.hp_value} "
          f"faint={args.fainted_value} win={args.victory_value} gamma={args.gamma})...", flush=True)
    t0 = time.time()
    obs, act, mask, returns = collect(env, wrap, teacher, args.samples, args.gamma)
    is_teacher = act >= 0
    print(f"  {len(obs)} steps ({is_teacher.sum()} teacher) over {time.time()-t0:.0f}s; "
          f"return mean={returns.mean():.1f} std={returns.std():.1f}", flush=True)

    model = ActorCritic(OBS_DIM, N_ACTIONS, args.hidden_dim, 2, embed=None, aux=False)
    opt = torch.optim.Adam(model.parameters(), lr=args.lr)
    obs_t = torch.tensor(obs); act_t = torch.tensor(act); mask_t = torch.tensor(mask)
    ret_t = torch.tensor(returns); trow = torch.tensor(is_teacher)
    ret_var = float(returns.var())
    N = len(obs); idx = np.arange(N)
    for ep in range(args.epochs):
        np.random.shuffle(idx); ce_s = v_s = 0.0; correct = nb = nt = 0
        for s in range(0, N, args.batch_size):
            mb = idx[s:s + args.batch_size]
            logits, value = model.forward(obs_t[mb], mask_t[mb])
            tr = trow[mb]
            ce = nn.functional.cross_entropy(logits[tr], act_t[mb][tr]) if tr.any() else torch.zeros(())
            vloss = nn.functional.mse_loss(value, ret_t[mb])
            loss = ce + args.value_coef * vloss
            opt.zero_grad(); loss.backward(); opt.step()
            ce_s += float(ce) * int(tr.sum()); v_s += float(vloss) * len(mb)
            correct += int((logits[tr].argmax(-1) == act_t[mb][tr]).sum()); nt += int(tr.sum()); nb += len(mb)
        # value explained variance over the whole set
        with torch.no_grad():
            _, vall = model.forward(obs_t, mask_t)
            ev = float(1 - (ret_t - vall).var() / (ret_var + 1e-8))
        print(f"  epoch {ep+1}/{args.epochs}  ce={ce_s/nt:.3f} acc={correct/nt:.2%}  "
              f"vloss={v_s/nb:.1f} value_ev={ev:+.2f}", flush=True)

    Path(args.out).parent.mkdir(parents=True, exist_ok=True)
    torch.save(model.state_dict(), args.out)
    env2, wrap2 = mkenv()
    wr = winrate(model, env2, wrap2, args.eval_episodes)
    print(f"\nBC policy greedy win-rate vs random: {wr:.3f} ({args.eval_episodes} eps)\nsaved {args.out}")
    env.close(); env2.close()


if __name__ == "__main__":
    main()
