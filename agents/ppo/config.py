"""Hyperparameters for PPO training.

One dataclass, all knobs in one place. Defaults are tuned to be sane for the small
placeholder task and to keep the model near ~1M parameters; the same config drives the
real battle engine once it's wired in.
"""

from dataclasses import dataclass


@dataclass
class PPOConfig:
    # --- interface (the contract with the environment / observation encoder) ---
    obs_dim: int = 128          # flat observation vector length (eventually = encoded State.observe)
    n_actions: int = 9          # 4 move slots + 5 switch targets

    # --- model (sized to land ~5M params with the engine's float obs + ID embeddings) ---
    hidden_dim: int = 928
    n_hidden_layers: int = 2    # hidden->hidden layers after the input projection
    embed_dim: int = 32         # embedding width for each categorical (species/move/item/...)

    # --- rollout collection ---
    num_envs: int = 8           # parallel environments stepped in lockstep
    rollout_steps: int = 128    # steps per env per update (batch = num_envs * rollout_steps)

    # --- PPO objective ---
    gamma: float = 0.99
    gae_lambda: float = 0.95
    clip_eps: float = 0.2
    entropy_coef: float = 0.01
    value_coef: float = 0.5
    max_grad_norm: float = 0.5
    lr: float = 3e-4
    update_epochs: int = 4
    minibatch_size: int = 256
    norm_advantages: bool = True

    # --- training run ---
    total_steps: int = 200_000  # total environment steps before stopping
    seed: int = 0
    device: str = "auto"        # "auto" | "cpu" | "mps" | "cuda"
    log_every_updates: int = 1
