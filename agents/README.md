# agents

A small, readable **PPO** reinforcement-learning agent for the deep-showdown battle engine.

- **~1M-parameter** actor-critic (shared MLP trunk → masked 9-action policy + value head).
- **Machine-agnostic**: runs on CPU, Apple **MPS**, or CUDA (auto-detected).
- **Decoupled from the engine**: trains today against a learnable placeholder env that has the
  exact I/O contract of the real simulator, so the Rust engine plugs in behind one interface.

See [`DESIGN.md`](DESIGN.md) for the architecture and the training/inference diagrams.

## Layout

```
agents/
  ppo/
    config.py   # PPOConfig — all hyperparameters
    model.py    # ActorCritic (~1M params) + action masking
    env.py      # BattleEnv interface, DummyBattleEnv placeholder, SyncVectorEnv
    buffer.py   # RolloutBuffer + GAE
    train.py    # the PPO training loop (entrypoint)
  DESIGN.md     # system design diagram (architecture, training, inference)
  requirements.txt
```

## Setup

```sh
cd agents
python -m venv .venv && source .venv/bin/activate
pip install -r requirements.txt
```

## Run

```sh
# from agents/
python -m ppo.train                       # defaults; prints param count + training progress
python -m ppo.train --total-steps 50000   # shorter run
python -m ppo.train --device cpu          # force CPU (auto picks MPS/CUDA if present)
```

On the placeholder task, mean `ep_return` should climb from ~0 (random) toward the optimum as
the policy learns — a quick end-to-end check that the loop works.

## Action space

`0..3` = the four move slots, `4..8` = switch to one of the five benched team members. Illegal
actions are masked out before sampling. Special cases (forced switches, two-turn locks, etc.)
are intentionally out of scope for now.

## Status

Scaffold + working PPO loop against the placeholder env. Next: implement a `BattleEnv` backed
by `showdown-rs` (encode `State.observe(viewer)` → obs; map actions → `MoveChoice`; reward on
win/loss). The algorithm above does not change when that lands.
