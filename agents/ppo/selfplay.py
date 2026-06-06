"""Greedy self-play PPO training against the Rust engine.

Both sides are driven by the *same* network (shared-parameter self-play):
  - the **learner** (Red) samples actions (exploration) and its transitions feed PPO;
  - the **opponent** (Blue) acts **greedily** (argmax over the masked policy) — a steadily
    improving sparring partner as the shared weights update.

Reward is sparse: +1 / -1 to the learner on win / loss. Every `--render-every` updates we play
one full game to the terminal with natural-language commentary so you can watch progress.

Run:
    uv run python -m ppo.selfplay                      # defaults
    uv run python -m ppo.selfplay --total-steps 200000 --device cpu
    uv run python -m ppo.selfplay --watch              # just watch one game with the init policy
"""

from __future__ import annotations

import argparse
import time

import numpy as np
import torch

import showdown_engine as se

from .buffer import RolloutBuffer
from .config import PPOConfig
from .engine_env import BLUE, RED, EngineVecEnv
from .model import ActorCritic
from .train import ppo_update, resolve_device, set_seed


@torch.no_grad()
def greedy_actions(model, obs_np, mask_np, device) -> np.ndarray:
    """Argmax action under the masked policy (the greedy opponent / eval policy)."""
    obs = torch.as_tensor(obs_np, device=device)
    mask = torch.as_tensor(mask_np, device=device)
    logits, _ = model.forward(obs, mask)
    return logits.argmax(dim=-1).cpu().numpy()


def train_selfplay(
    cfg: PPOConfig,
    render_every: int = 20,
    max_games: int | None = None,
    snapshot_every: int = 10,
):
    """Frozen-snapshot self-play.

    The opponent (Blue) plays from a *frozen copy* of the learner refreshed every
    `snapshot_every` updates — so between refreshes it's a fixed reference and the
    win-rate-vs-snapshot curve is a meaningful (sawtooth) progress signal. `snapshot_every == 0`
    falls back to a *live* opponent (the moving-target variant).
    """
    set_seed(cfg.seed)
    device = resolve_device(cfg.device)

    envs = EngineVecEnv(cfg.num_envs, seed=cfg.seed)
    model = ActorCritic(envs.obs_dim, envs.n_actions, cfg.hidden_dim, cfg.n_hidden_layers).to(device)
    optimizer = torch.optim.Adam(model.parameters(), lr=cfg.lr, eps=1e-5)
    buffer = RolloutBuffer(cfg.rollout_steps, cfg.num_envs, envs.obs_dim, envs.n_actions, device)

    # The frozen opponent: same architecture, weights copied from the learner, no gradients.
    use_snapshot = snapshot_every > 0
    opponent = ActorCritic(envs.obs_dim, envs.n_actions, cfg.hidden_dim, cfg.n_hidden_layers).to(device)
    opponent.load_state_dict(model.state_dict())
    opponent.eval()
    for p in opponent.parameters():
        p.requires_grad_(False)
    blue_net = opponent if use_snapshot else model

    mode = f"frozen snapshot, refresh every {snapshot_every} updates" if use_snapshot else "live (moving target)"
    print(f"device={device}  params={model.num_params():,}  obs_dim={envs.obs_dim}  "
          f"batch={cfg.num_envs * cfg.rollout_steps}  (learner=Red samples, opponent=Blue greedy; {mode})")

    batch_size = cfg.num_envs * cfg.rollout_steps
    num_updates = cfg.total_steps // batch_size
    global_step = 0
    total_games = 0
    wins, losses, draws = 0, 0, 0
    window_results: list[int] = []  # since the last snapshot refresh; basis for the win-rate
    label = "snapshot" if use_snapshot else "live"
    start = time.time()

    for update in range(1, num_updates + 1):
        # --- collect a rollout of the learner's (Red's) transitions ---
        for t in range(cfg.rollout_steps):
            obs_r, mask_r = envs.observe(RED)
            obs_b, mask_b = envs.observe(BLUE)

            obs_t = torch.as_tensor(obs_r, device=device)
            mask_t = torch.as_tensor(mask_r, device=device)
            with torch.no_grad():
                action, log_prob, _, value = model.act(obs_t, mask_t)         # learner: sample
            blue_action = greedy_actions(blue_net, obs_b, mask_b, device)      # opponent: greedy (frozen)

            reward_np, done_np = envs.step(action.cpu().numpy(), blue_action, learner=RED)

            buffer.add(
                t,
                obs_t,
                mask_t,
                action,
                log_prob,
                value,
                torch.as_tensor(reward_np, device=device),
                torch.as_tensor(done_np, device=device),
            )
            global_step += cfg.num_envs

            for r, d in zip(reward_np, done_np):
                if d:
                    res = 1 if r > 0 else (-1 if r < 0 else 0)
                    total_games += 1
                    window_results.append(res)
                    wins += res == 1
                    losses += res == -1
                    draws += res == 0

        # --- advantages + PPO update ---
        with torch.no_grad():
            obs_r, mask_r = envs.observe(RED)
            _, last_value = model.forward(
                torch.as_tensor(obs_r, device=device), torch.as_tensor(mask_r, device=device)
            )
        buffer.compute_gae(last_value, cfg.gamma, cfg.gae_lambda)
        stats = ppo_update(model, optimizer, buffer.flat_view(), cfg, batch_size)

        win_rate = float(np.mean([r == 1 for r in window_results])) if window_results else float("nan")
        sps = int(global_step / (time.time() - start))
        print(f"update {update:4d}/{num_updates}  step {global_step:>8}  games {total_games:>4}  "
              f"win_rate(vs {label}) {win_rate:5.2f}  "
              f"pi {stats['policy_loss']:+.3f}  v {stats['value_loss']:.3f}  "
              f"ent {stats['entropy']:.3f}  kl {stats['approx_kl']:.4f}  {sps} sps")

        # --- refresh the frozen opponent: it catches up to the learner; win-rate resets ---
        if use_snapshot and update % snapshot_every == 0:
            opponent.load_state_dict(model.state_dict())
            window_results.clear()
            print(f"    [snapshot refreshed @ update {update} — opponent = current learner]")

        if render_every and update % render_every == 0:
            print()
            play_one_game(model, device, seed=1000 + update, max_turns=200)
            print()

        if max_games is not None and total_games >= max_games:
            print(f"\nReached {total_games} games (target {max_games}); stopping. "
                  f"record W/L/D = {wins}/{losses}/{draws}.")
            break

    return model


@torch.no_grad()
def play_one_game(model, device, seed: int = 0, max_turns: int = 200, sample: bool = True):
    """Play one full self-play game to the terminal with natural-language commentary."""
    b = se.Battle(seed=seed)
    print("=" * 64)
    print("  WATCH: one self-play game (both sides = current policy)")
    print("=" * 64)
    print(b.render())

    def pick(side):
        obs = torch.as_tensor(np.asarray(b.observe(side), dtype=np.float32), device=device).unsqueeze(0)
        mask = torch.as_tensor(np.asarray(b.legal_actions(side), dtype=bool), device=device).unsqueeze(0)
        if sample:
            action, _, _, _ = model.act(obs, mask)
            return int(action.item())
        logits, _ = model.forward(obs, mask)
        return int(logits.argmax(dim=-1).item())

    for _ in range(max_turns):
        ar, ab = pick(RED), pick(BLUE)
        done, winner, lines = b.step(ar, ab, narrate=True)
        for ln in lines:
            print(ln)
        if done:
            name = {0: "Red", 1: "Blue", -1: "Nobody (draw/timeout)"}[winner]
            print(f"\n>>> {name} wins on turn {b.turn}.")
            print(b.render())
            return
    print(f"\n>>> Game hit the {max_turns}-turn cap (draw).")
    print(b.render())


def main():
    parser = argparse.ArgumentParser(description="Greedy self-play PPO against the Rust engine.")
    parser.add_argument("--total-steps", type=int, default=300_000)
    parser.add_argument("--games", type=int, default=None, help="stop after this many completed games")
    parser.add_argument("--device", type=str, default=None, help="auto|cpu|mps|cuda")
    parser.add_argument("--seed", type=int, default=0)
    parser.add_argument("--render-every", type=int, default=20, help="watch a game every N updates (0 = never)")
    parser.add_argument("--snapshot-every", type=int, default=10,
                        help="refresh the frozen opponent every N updates (0 = live moving-target opponent)")
    parser.add_argument("--watch", action="store_true", help="just play one game with the untrained policy and exit")
    args = parser.parse_args()

    cfg = PPOConfig()
    cfg.total_steps = args.total_steps
    cfg.seed = args.seed
    if args.device is not None:
        cfg.device = args.device

    if args.watch:
        device = resolve_device(cfg.device)
        # obs_dim must match the engine; build a model sized to it.
        probe = se.Battle(seed=0)
        model = ActorCritic(probe.obs_dim, probe.n_actions, cfg.hidden_dim, cfg.n_hidden_layers).to(device)
        play_one_game(model, device, seed=args.seed)
        return

    train_selfplay(cfg, render_every=args.render_every, max_games=args.games, snapshot_every=args.snapshot_every)


if __name__ == "__main__":
    main()
