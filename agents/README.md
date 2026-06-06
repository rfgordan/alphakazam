# agents

A small, readable **PPO** reinforcement-learning agent for the deep-showdown battle engine, with
a **PyO3 bridge** to the Rust engine, **greedy self-play**, and **live natural-language
commentary** so you can watch a battle unfold in the terminal.

- **~1M-parameter** actor-critic (shared MLP trunk → masked 9-action policy + value head).
- **Machine-agnostic**: CPU, Apple **MPS**, or CUDA (auto-detected).
- **Real engine**: `showdown_engine.Battle` exposes observations, legal-action masks, rewards,
  and per-turn commentary; the agent never touches the engine internals.

See [`DESIGN.md`](DESIGN.md) for the architecture and the training/inference/self-play diagrams.

## Layout

```
agents/
  ppo/
    config.py       # PPOConfig — hyperparameters
    model.py        # ActorCritic (~1M params) + action masking
    env.py          # placeholder env (DummyBattleEnv) for algorithm smoke tests
    engine_env.py   # EngineVecEnv — vectorized self-play over showdown_engine
    buffer.py       # RolloutBuffer + GAE
    train.py        # PPO loop vs the placeholder env
    selfplay.py     # frozen-snapshot self-play vs the Rust engine + live commentary  <-- main
    baselines.py    # Baseline protocol + PolicyBaseline/RandomBaseline + evaluate()
    run_logger.py   # runs/<ts>/ : metrics.jsonl, eval.jsonl, config.json, checkpoints
  DESIGN.md, pyproject.toml
```

## Logs & analysis

Each run writes to `runs/<timestamp>/`:
- `metrics.jsonl` — one line per PPO update (win-rate vs snapshot, losses, entropy, KL, sps)
- `eval.jsonl` — one line per baseline eval (win-rate, W/L/D, avg turns) — the **absolute** progress curve
- `config.json`, `ckpt_<update>.pt` — checkpoints are **rolling**: only the most recent
  `--keep-checkpoints` (default 3, ~4 MB each) are kept; older ones are pruned. `--keep-checkpoints 0`
  keeps all (e.g. for a future snapshot pool). `--snapshot-every` is in-memory only — it never
  writes to disk.

```python
import json
evals = [json.loads(l) for l in open("runs/<ts>/eval.jsonl")]
# win-rate vs the fixed anchor over time:
[(e["update"], e["win_rate"]) for e in evals if e["baseline"] == "anchor-init"]
```

Baselines are generic (`baselines.Baseline`: `actions(obs, mask) -> np.ndarray`) — add a heuristic
bot or a specific checkpoint to the `baselines` list in `selfplay.train_selfplay`.

### Weights & Biases

`--wandb` mirrors everything to W&B: training metrics under `train/*` (incl. the split aux losses
`aux_opp` / `aux_delta` / `aux_ko`) and evals under `eval/<baseline>/{win_rate,draws,avg_turns}`,
keyed by environment step.
```sh
uv run python -m ppo.selfplay --total-steps 0 --wandb --wandb-project deep-showdown
WANDB_MODE=offline uv run python -m ppo.selfplay --total-steps 50000 --wandb   # no login/network
```
The JSONL files are still written either way.

The Rust↔Python bridge lives in `../showdown-rs/crates/pybridge/` (a `pyo3` crate exposing the
`showdown_engine` module: `Battle.observe / legal_actions / step / render`). The natural-language
narration and observation encoder are render-layers in the engine crate (`narrate.rs`,
`encode.rs`), reading the canonical instruction stream — never in the hot path.

## Setup (uv + maturin)

```sh
cd agents
uv venv --python 3.12
uv pip install numpy "torch>=2.2" "maturin>=1.5,<2.0"

# Build the Rust bridge into the venv (rebuild after engine changes):
uv run maturin develop --release -m ../showdown-rs/crates/pybridge/Cargo.toml
```

## Run

```sh
# Watch one game with the (untrained) policy — live commentary:
uv run python -m ppo.selfplay --watch

# Frozen-snapshot self-play. The learner trains on a random side each episode; the opponent
# plays a frozen snapshot refreshed every --snapshot-every updates. Every --eval-every updates
# the learner is scored (greedily) vs fixed baselines (a random-init anchor + a random bot) for
# an absolute progress curve. Logs land in runs/<timestamp>/.
uv run python -m ppo.selfplay --total-steps 300000 --snapshot-every 10 --eval-every 25

# Train indefinitely from a clean slate (Ctrl-C to stop; final checkpoint is saved):
uv run python -m ppo.selfplay --total-steps 0 --device cpu

uv run python -m ppo.selfplay --snapshot-every 0    # live moving-target opponent (no freeze)
uv run python -m ppo.selfplay --render-every 10     # also watch a narrated game every 10 updates

# Algorithm-only smoke test against the learnable placeholder env (no engine):
uv run python -m ppo.train --total-steps 50000
```

## Evaluating vs poke-engine MCTS (validated baseline)

A strong, external reference opponent: pmariglia's [poke-engine](https://github.com/pmariglia/poke-engine)
Monte-Carlo Tree Search (the engine behind the [foul-play](https://github.com/pmariglia/foul-play)
bot). Our `MctsBaseline` builds a **perfect-information** poke-engine state from each battle (via
`Battle.state_json()` → `ppo/poke_engine_adapter.py`), searches, and maps its move back to our
action space — so MCTS plays at full strength. It's meant to be hard to beat; a random-init policy
should win ~0%.

Install poke-engine once (needs Rust; uses gen9/terastallization features like foul-play):
```sh
uv pip install "poke-engine==0.0.46" \
  --config-settings="build-args=--features poke-engine/terastallization --no-default-features"
```

Run an offline eval (greedy policy, side-balanced, with Wilson CIs):
```sh
uv run python -m ppo.eval --ckpt runs/<ts>/ckpt_000050.pt --baseline mcts --mcts-ms 100 --games 50
uv run python -m ppo.eval --baseline mcts --mcts-ms 50 --games 12      # random-init sanity (~0%)
```

### Baseline ladder

Three reference opponents of increasing strength — your policy should clear them in this order:

| baseline | what it is | speed |
|---|---|---|
| `random` | uniform over legal actions | instant |
| `heuristic` | port of poke-env's `SimpleHeuristicsPlayer` (matchup/switch/hazards/best-damage) | fast |
| `mcts` | poke-engine Monte-Carlo Tree Search (perfect info) | heavy |

`random` and `heuristic` run in the **default training evals** (logged to `eval.jsonl` every
`--eval-every`); `mcts` is opt-in (`--mcts-eval-ms`) since it's heavy. Standalone:
```sh
uv run python -m ppo.eval --baseline heuristic --games 100      # fast; no poke-engine needed
```

Notes: MCTS is heavy (sequential per env) — keep `--num-envs`/`--games` modest and sweep
`--mcts-ms` to get a win-rate-vs-search-budget curve. The poke-engine `MctsBaseline` slots into the
same `evaluate()`/`Baseline` machinery as `random`/`anchor`. Caveats live in
`poke_engine_adapter.py` (volatiles/substitute/Tera-intent are approximated).

`win_rate(vs snapshot)` traces a **sawtooth**: it climbs as the learner beats the frozen
opponent, then drops at each `[snapshot refreshed]` when the opponent catches up. Staying above
~0.5 after every refresh means each policy beats its predecessor (real improvement). Example
commentary:

```
── Turn 3 ──
Blue's Ragingbolt used Thunderclap!
Red's Greattusk used Headlongrush!
Blue's Ragingbolt lost 85% HP.
Blue's Ragingbolt fainted!
```

## Action space

`0..3` = the four move slots, `4..8` = switch to the k-th benched mon. Illegal actions are masked
before sampling. Special cases (forced two-turn locks, etc.) are out of scope for now.

## Status

Bridge + observation encoder + narration + team builder + greedy self-play trainer + live
terminal commentary all working. Next candidates: opponent snapshotting / league play, randomized
teams, reward shaping, determinization over hidden info, and a proper display-name table.
