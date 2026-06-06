"""A small, readable PPO agent for the deep-showdown battle engine.

Modules:
    config  — PPOConfig hyperparameters
    model   — ActorCritic (~1M params, masked 9-action policy + value head)
    env     — BattleEnv interface + DummyBattleEnv placeholder + SyncVectorEnv
    buffer  — RolloutBuffer + GAE
    train   — the PPO training loop
"""

from .config import PPOConfig
from .model import ActorCritic

__all__ = ["PPOConfig", "ActorCritic"]
