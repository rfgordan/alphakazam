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
  (1558/1558 turns), 100% state-representation fidelity.
- **Random battles (full roster): ~55%** turn parity and climbing — the remaining gap is
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
