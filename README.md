# alphakazam

A clean-slate **Rust Pokémon battle engine** built for fast reinforcement-learning
simulation, and **verified for correctness against Pokémon Showdown** by differential
testing.

## Why

Existing options trade off against each other: Pokémon Showdown (the reference simulator)
is correct but JS and slow for RL throughput; faster reimplementations are approximate.
This engine aims for both — a flat, `Copy`-able state designed for cheap snapshotting and
massively-parallel rollouts, with a rigorous, continuous parity check against Showdown so
correctness is *measured*, not assumed.

## Status

- **2-team gen9 slice: 100% per-turn parity** with Showdown across 250 sampled games
  (1673/1673 turns), 100% state-representation fidelity.
- **Sample gen9 OU team: 100% per-turn parity** across 50 games (1612/1612 turns).
- **Random battles (full roster): ~85%** turn parity and climbing — the remaining gap is
  the long tail of abilities/items/edge-case mechanics, each diagnosable and mechanical to
  add. See [`showdown-rs/WORKLOG.md`](showdown-rs/WORKLOG.md) for the full fix-by-fix
  history and the current breakdown.

## Layout

```
showdown-rs/
  crates/engine/        the engine: flat Copy state, reversible Instruction model,
                        data-driven turn resolution (generate_instructions), damage calc
  crates/engine/src/gen.rs   AUTO-GENERATED species + move tables (911 / 954) from PS
  crates/verify/        differential test runner (loads a trace, replays, compares)
  harness/              gen-trace.mjs (sample seeded battles from PS) and
                        gen-data.mjs (codegen the Rust data tables from PS data)
  WORKLOG.md            development log + parity progression
engines/                third-party reference clones (gitignored — see Setup)
```

## How it works

1. **`harness/gen-trace.mjs`** drives a seeded, deterministic battle in the real PS
   simulator with scripted choices and records a replayable JSON trace (per-turn state +
   the choices made). See `harness/TRACE_FORMAT.md`.
2. **`crates/verify`** parses each trace's state into the engine, replays the turn through
   `generate_instructions`, and checks that PS's actual next state is one of the engine's
   enumerated outcome branches (membership testing — no need to match PS's RNG).
3. **`harness/gen-data.mjs`** regenerates `crates/engine/src/gen.rs` (all gen9 species and
   moves) from PS's data files, so move *data* is never hand-written.

## Hidden information & stat spreads (EV/IV gaps)

The engine runs internally on **full ground truth** — that's what keeps the per-turn transition
fast — and exposes a player's partial view through `State::observe(viewer)`, which masks the
opponent's unrevealed item, ability, unused moves, and Tera type (a per-Pokémon `Reveal` bitmask
records what each side has shown). An agent acting under hidden information is expected to
*determinize*: sample concrete full states consistent with that view and run the perfect-info
engine on each (cheap, because the state is `Copy`).

Two **known gaps around stat spreads** worth calling out:

- **IVs are not modeled.** A `Pokemon` stores final *computed* `stats` (so the hot path never
  recomputes them) plus `evs` and `nature` for reference, but **not** IVs. The engine therefore
  trusts the stats supplied by the trace/harness rather than deriving them from
  species+level+IV+EV+nature. For RL self-play that builds its own teams, stat computation
  (including IVs) needs to live in the team builder that populates the state, not in the engine.
- **The opponent's spread is hidden and only *inferred*, never announced.** Unlike moves/item/
  ability (discrete one-shot reveals), EV/IV/nature are never shown in the battle log — they're
  narrowed from observed damage rolls. So `observe()` zeroes the foe's `evs`/`nature` (leaving
  base stats, which bound the rolls); recovering a concrete spread is a job for the
  determinization sampler, not the observation layer.

## Setup

The engine builds standalone. The harness needs the Pokémon Showdown clone for its `dist/sim`:

```sh
mkdir -p engines
git clone https://github.com/smogon/pokemon-showdown engines/pokemon-showdown
# (optional reference) git clone https://github.com/pmariglia/poke-engine engines/poke-engine
```

## Build & test

```sh
cd showdown-rs
cargo test                              # unit + apply/reverse property tests

# generate traces from PS, then verify the engine against them
node harness/gen-trace.mjs --seed 1 --max-turns 60 --out harness/traces/trace-seed1.json
cargo run --release -p verify -- harness/traces/*.json      # read the AGGREGATE block

# regenerate the species/move data tables from PS
node harness/gen-data.mjs
```

`VERIFY_DEBUG=1 cargo run -p verify -- <trace.json>` prints the first mismatch with the
offending Pokémon/move and a field-level diff — the loop used to drive parity upward.
