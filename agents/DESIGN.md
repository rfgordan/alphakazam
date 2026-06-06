# Agent system design

A small, readable **PPO** agent for the deep-showdown battle engine, now wired to the real Rust
engine via a PyO3 bridge with **greedy self-play** and **live natural-language commentary**.
Keep this in sync with the code in `ppo/` and the bridge in `../showdown-rs/crates/pybridge/`.

Design priorities (in order): **simple & readable**, **small model (~1M params)**,
**machine-agnostic** (CPU / Apple MPS / CUDA), **clean engine boundary** — the agent only ever
sees `(observation, action mask, reward, done)`, computed by render-layers around an unchanged
transition.

---

## 1. Components

```mermaid
flowchart LR
    subgraph RUST["showdown-rs (Rust)"]
        direction TB
        ENGINE["engine: generate_instructions\n(canonical transition -> instruction stream)"]
        TEAM["team: build playable State from MemberSpecs"]
        ENC["encode: State.observe(viewer) -> float vector"]
        NARR["narrate: (state, actions, instructions) -> text"]
        TEAM --> ENGINE
        ENGINE --> ENC
        ENGINE --> NARR
    end

    subgraph BRIDGE["pybridge (PyO3): class Battle"]
        STEP["step(a_red,a_blue) -> done,winner,lines\nobserve(side) / legal_actions(side) / render()"]
    end

    subgraph AGENT["agents/ppo (Python)"]
        AC["ActorCritic (~1M params)\nshared MLP trunk -> policy(9) + value(1)"]
    end

    ENC --> STEP
    NARR --> STEP
    ENGINE --> STEP
    STEP -- "obs[464], mask[9]" --> AC
    AC -- "masked action" --> STEP
    STEP -- "reward, done" --> AC
    STEP -- "commentary lines" --> TTY["terminal (watch a game)"]
```

The engine produces one canonical artifact per turn — the **instruction stream**. Three readers
project it: `encode` (for the network), `narrate` (for a human), and the bridge's outcome check
(reward/done). None of them live in the transition, so RL throughput is unaffected.

**Action space (9):** `0..3` = move slots, `4..8` = switch to the k-th benched mon. The mask
zeroes illegal actions (no-PP moves, fainted/active switch targets) before sampling.

---

## 2. The model (~1M parameters)

Shared trunk + two linear heads. With the engine's `obs_dim≈464`, `hidden_dim=608`,
`n_hidden_layers=2` it is **1,029,354** params (`ActorCritic.num_params()`).

```mermaid
flowchart TD
    OBS["obs [~464]"] --> L0["Linear ->608 + Tanh"]
    L0 --> L1["Linear 608->608 + Tanh"]
    L1 --> L2["Linear 608->608 + Tanh"]
    L2 --> H["trunk [608]"]
    H --> P["policy head -> 9"]
    H --> V["value head -> 1"]
    MASK["action_mask [9]"] --> P
    P --> LOGITS["masked logits -> Categorical"]
```

---

## 3. Frozen-snapshot self-play

The **learner (Red)** samples actions and trains via PPO. The **opponent (Blue)** plays greedily
(argmax) from a **frozen snapshot** of the learner — a *separate* network whose weights are
copied from the learner every `--snapshot-every` updates and otherwise held fixed (no gradients).
Between refreshes the opponent is a stable reference, so `win_rate(vs snapshot)` is a meaningful
**sawtooth** progress curve: it climbs as the learner beats the fixed opponent, then drops at each
refresh when the opponent catches up. Staying above ~0.5 after every refresh = monotonic
improvement (each policy beats its predecessor). `--snapshot-every 0` falls back to a *live*
moving-target opponent. Reward is sparse: +1 / -1 to the learner on win / loss.

```mermaid
flowchart TD
    subgraph COLLECT["1 · Collect rollout (per env, per step)"]
        direction TB
        O["obs_red, obs_blue <- env.observe"]
        SR["red  action ~ sample(learner)    (store for PPO)"]
        SB["blue action = argmax(snapshot)    (frozen opponent)"]
        ST["env.step(a_red, a_blue) -> reward, done; auto-reset"]
        O --> SR --> SB --> ST --> O
    end
    COLLECT --> GAE["2 · GAE over the learner's transitions"]
    GAE --> UPD["3 · PPO clipped update (learner weights only)"]
    UPD --> REF{"update % snapshot_every == 0?"}
    REF -- yes --> COPY["snapshot.load_state_dict(learner)\nwin-rate window resets"]
    REF -- no --> COLLECT
    COPY --> COLLECT
```

The PPO objective (clipped surrogate + value loss + entropy bonus, GAE, minibatch epochs) is
shared with the placeholder trainer — only data collection + the frozen opponent differ. Every
`--render-every` updates one full game is played to the terminal with live commentary.

---

## 4. Inference / watching a battle

`Battle.step(a_red, a_blue, narrate=True)` returns the turn's commentary; `Battle.render()`
prints an HP board. `python -m ppo.selfplay --watch` plays one game with the current policy.

```mermaid
sequenceDiagram
    participant Loop as selfplay
    participant Br as Battle (bridge)
    participant Eng as engine
    Loop->>Br: observe(side), legal_actions(side)
    Br->>Eng: encode(State.observe(side))
    Loop->>Loop: action = policy(obs, mask)
    Loop->>Br: step(a_red, a_blue, narrate=True)
    Br->>Eng: generate_instructions -> sample branch -> apply
    Br->>Eng: narrate(pre, actions, instructions)
    Br-->>Loop: done, winner, commentary lines
    Loop->>Loop: print lines (follow live)
```

---

## 5. Status & what's next

**Done:** PyO3 bridge (`showdown_engine.Battle`), observation encoder, narration layer, team
builder, engine-backed vector env, frozen-snapshot self-play trainer (with a live-opponent
fallback), live terminal commentary.

**Not yet (intentionally):** league / population play (a *pool* of past snapshots à la AlphaStar,
vs. the single snapshot here); richer reward shaping; determinization over hidden info; randomized
teams (one fixed matchup for now); special-case action types (forced two-turn locks, etc.); a
proper display-name table (commentary uses prettified PS ids). Each slots in behind the same
`Battle` interface.
