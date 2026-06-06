"""PPO training loop.

The whole algorithm in one readable place:

    repeat:
        1. collect a rollout (num_envs * rollout_steps on-policy transitions)
        2. compute advantages + returns with GAE
        3. for update_epochs, over shuffled minibatches:
               clipped policy loss + value loss - entropy bonus  ->  Adam step

Run:  python -m ppo.train               # uses defaults in config.py
      python -m ppo.train --total-steps 50000 --device cpu
"""

from __future__ import annotations

import argparse
import time

import numpy as np
import torch
import torch.nn as nn

from .buffer import RolloutBuffer
from .config import PPOConfig
from .env import make_dummy_vector_env
from .model import ActorCritic


def resolve_device(name: str) -> torch.device:
    if name != "auto":
        return torch.device(name)
    if torch.backends.mps.is_available():
        return torch.device("mps")
    if torch.cuda.is_available():
        return torch.device("cuda")
    return torch.device("cpu")


def set_seed(seed: int):
    np.random.seed(seed)
    torch.manual_seed(seed)


def train(cfg: PPOConfig, make_env=make_dummy_vector_env):
    set_seed(cfg.seed)
    device = resolve_device(cfg.device)

    envs = make_env(cfg, cfg.seed)
    model = ActorCritic(envs.obs_dim, envs.n_actions, cfg.hidden_dim, cfg.n_hidden_layers).to(device)
    optimizer = torch.optim.Adam(model.parameters(), lr=cfg.lr, eps=1e-5)
    buffer = RolloutBuffer(cfg.rollout_steps, cfg.num_envs, envs.obs_dim, envs.n_actions, device)

    print(f"device={device}  params={model.num_params():,}  "
          f"batch={cfg.num_envs * cfg.rollout_steps}  obs_dim={envs.obs_dim}  actions={envs.n_actions}")

    # Persistent env state across rollouts (PPO collects continuing trajectories).
    obs_np, mask_np = envs.reset()
    obs = torch.as_tensor(obs_np, device=device)
    mask = torch.as_tensor(mask_np, device=device)

    # Running tally of finished-episode returns, for logging.
    ep_returns = np.zeros(cfg.num_envs, dtype=np.float32)
    recent_returns: list[float] = []

    batch_size = cfg.num_envs * cfg.rollout_steps
    num_updates = cfg.total_steps // batch_size
    global_step = 0
    start = time.time()

    for update in range(1, num_updates + 1):
        # --- 1. collect rollout ------------------------------------------------------
        for t in range(cfg.rollout_steps):
            with torch.no_grad():
                action, log_prob, _, value = model.act(obs, mask)
            next_obs_np, next_mask_np, reward_np, done_np = envs.step(action.cpu().numpy())

            reward = torch.as_tensor(reward_np, device=device)
            done = torch.as_tensor(done_np, device=device)
            buffer.add(t, obs, mask, action, log_prob, value, reward, done)

            obs = torch.as_tensor(next_obs_np, device=device)
            mask = torch.as_tensor(next_mask_np, device=device)
            global_step += cfg.num_envs

            # Track episodic return for logging.
            ep_returns += reward_np
            for i in range(cfg.num_envs):
                if done_np[i]:
                    recent_returns.append(float(ep_returns[i]))
                    ep_returns[i] = 0.0

        # --- 2. advantages + returns -------------------------------------------------
        with torch.no_grad():
            _, last_value = model.forward(obs, mask)
        buffer.compute_gae(last_value, cfg.gamma, cfg.gae_lambda)
        data = buffer.flat_view()

        # --- 3. PPO update -----------------------------------------------------------
        stats = ppo_update(model, optimizer, data, cfg, batch_size)

        if update % cfg.log_every_updates == 0:
            window = recent_returns[-100:]
            mean_ret = float(np.mean(window)) if window else float("nan")
            sps = int(global_step / (time.time() - start))
            print(f"update {update:4d}/{num_updates}  step {global_step:>8}  "
                  f"ep_return {mean_ret:6.3f}  "
                  f"pi_loss {stats['policy_loss']:+.3f}  v_loss {stats['value_loss']:.3f}  "
                  f"entropy {stats['entropy']:.3f}  approx_kl {stats['approx_kl']:.4f}  {sps} sps")

    return model


def ppo_update(model, optimizer, data, cfg: PPOConfig, batch_size: int) -> dict:
    idx = np.arange(batch_size)
    advantages = data["advantages"]
    if cfg.norm_advantages:
        advantages = (advantages - advantages.mean()) / (advantages.std() + 1e-8)

    last = {}
    for _ in range(cfg.update_epochs):
        np.random.shuffle(idx)
        for start in range(0, batch_size, cfg.minibatch_size):
            mb = torch.as_tensor(idx[start:start + cfg.minibatch_size], device=data["obs"].device)

            _, new_log_prob, entropy, new_value = model.act(
                data["obs"][mb], data["masks"][mb], data["actions"][mb]
            )

            # Clipped policy (surrogate) objective.
            ratio = (new_log_prob - data["log_probs"][mb]).exp()
            adv = advantages[mb]
            unclipped = ratio * adv
            clipped = torch.clamp(ratio, 1 - cfg.clip_eps, 1 + cfg.clip_eps) * adv
            policy_loss = -torch.min(unclipped, clipped).mean()

            # Value regression toward GAE returns.
            value_loss = 0.5 * (new_value - data["returns"][mb]).pow(2).mean()

            entropy_loss = entropy.mean()
            loss = policy_loss + cfg.value_coef * value_loss - cfg.entropy_coef * entropy_loss

            optimizer.zero_grad()
            loss.backward()
            nn.utils.clip_grad_norm_(model.parameters(), cfg.max_grad_norm)
            optimizer.step()

            with torch.no_grad():
                approx_kl = (data["log_probs"][mb] - new_log_prob).mean()
            last = dict(
                policy_loss=policy_loss.item(),
                value_loss=value_loss.item(),
                entropy=entropy_loss.item(),
                approx_kl=approx_kl.item(),
            )
    return last


def main():
    parser = argparse.ArgumentParser(description="Small, readable PPO (battle-agent scaffold).")
    parser.add_argument("--total-steps", type=int, default=None)
    parser.add_argument("--device", type=str, default=None, help="auto|cpu|mps|cuda")
    parser.add_argument("--seed", type=int, default=None)
    args = parser.parse_args()

    cfg = PPOConfig()
    if args.total_steps is not None:
        cfg.total_steps = args.total_steps
    if args.device is not None:
        cfg.device = args.device
    if args.seed is not None:
        cfg.seed = args.seed

    train(cfg)


if __name__ == "__main__":
    main()
