# Runbook: long training runs on a GPU box

Everything needed to bring a bare Linux GPU machine from a fresh checkout to a multi-day
self-play run with continuous engine-vs-Showdown verification alongside it.

## 0. One-time setup

```sh
./setup.sh                 # ~10 min: rust, node, PS @ pin, venv+torch, pyo3 bridge, gates
./setup.sh --no-gates      # same, skipping the verification sweeps
```

Idempotent — re-run it after pulling. It is also the fix for the two things that bite on a new
box:

* **`engines/` may be a stale symlink** to another machine's checkout (it is gitignored). The
  script replaces anything that isn't a real directory and clones PS at the `ps.lock` pin.
* **torch must match the driver's CUDA, not the newest wheel.** A cu130 wheel on a 12.8 driver
  imports fine and then quietly reports `cuda.is_available() == False`. The script reads the
  version off `nvidia-smi` and picks the index accordingly.

Afterwards, put the toolchains on your PATH:

```sh
export PATH="$HOME/.cargo/bin:$HOME/.local/node-v22.14.0-linux-x64/bin:$PATH"
```

## 1. Start a run

```sh
agents/scripts/launch_train.sh runs/scale1 --num-envs 4096 --rollout-steps 32 \
    --minibatch-size 16384 --update-epochs 2
```

Detached (`setsid` + `nohup`), so it survives your shell, your SSH session, and this agent's
process tree. It starts two things:

| process | what | log |
|---|---|---|
| `train_watchdog.sh` → `ppo.train_flow` | the trainer, auto-resumed on any death | `runs/scale1/train.log` |
| `onpolicy_sidecar.sh` | engine-vs-PS verification of the live policy | `runs/scale1/cosim/sidecar.log` |

```sh
tail -f runs/scale1/train.log            # progress
cat runs/scale1/cosim/verdicts.log       # one line per verification sweep
agents/scripts/stop_train.sh runs/scale1 # clean stop (checkpoints first)
```

`NO_SIDECAR=1` launches the trainer alone. `SIDECAR_EVERY` / `SIDECAR_GAMES` tune the sweep
cadence and size.

**Stopping is safe at any point.** The trainer traps SIGTERM, finishes the update it is in,
writes `training_state.pt`, and exits; the watchdog sees the stop request and does not relaunch.
Relaunching the same run directory resumes weights, optimizer, opponent snapshot, and counters.

## 1b. Resuming in your own terminal

The run directory is the unit of resumption: weights, optimizer, counters and the league's
win-rate history all live in `runs/<name>/training_state.pt`, so relaunching against the same
directory continues exactly where it stopped. Stopping is always safe — SIGTERM finishes the
current update and checkpoints first.

Detached, auto-resuming, with the cosim sidecar (survives closing the terminal):

```sh
cd /home/user/alphakazam/agents
agents/scripts/launch_train.sh runs/scale1 \
    --num-envs 4096 --rollout-steps 32 --minibatch-size 16384 --update-epochs 2 \
    --ckpt-every 10 --snapshot-every 10 --pool-size 24 --opponent-slots 4 \
    --pfsp-mode frontier --eval-every 25 --eval-games 200 \
    --wandb --wandb-project deep-showdown
```

In the foreground instead, if you want the output live (no watchdog, no sidecar — Ctrl-C
checkpoints and exits):

```sh
cd /home/user/alphakazam/agents
.venv/bin/python -m ppo.train_flow --resume runs/scale1 \
    --num-envs 4096 --rollout-steps 32 --minibatch-size 16384 --update-epochs 2 \
    --ckpt-every 10 --snapshot-every 10 --pool-size 24 --opponent-slots 4 \
    --pfsp-mode frontier --eval-every 25 --eval-games 200 \
    --wandb --wandb-project deep-showdown
```

### Weights & Biases

`wandb` is already installed in the venv (it is not on the global `PATH`; use `.venv/bin/wandb`).
The credential must exist **on this box, for this user** — either `~/.netrc` (what
`.venv/bin/wandb login` writes) or `WANDB_API_KEY` exported in the shell that launches training.
Logging in on a laptop does not help a run executing here.

`--wandb` is safe to pass unconditionally: with no key the logger prints
`wandb disabled (UsageError: No API key configured...)` and carries on writing `metrics.jsonl` /
`eval.jsonl`. To capture now and upload later, launch with `WANDB_MODE=offline` and afterwards run
`.venv/bin/wandb sync runs/scale1/wandb/offline-run-*`.

Set `WANDB_ENTITY` if the run should land in a team rather than a personal account —
`train_flow.py` has no `--wandb-entity` flag and takes the default from the login.

## 2. What the trainer is

`ppo/train_flow.py` on `ppo/flow_env.py` → the Rust engine's **decision-point** API (`FlowVec`).

This matters: the older `ppo/selfplay.py` drives the legacy whole-turn `Battle` bridge MDP, whose
deviations from real Showdown (a faint replacement burns a whole turn, U-turn never pivots, no
Tera in the action space) `DECISION_POINTS.md` calls disqualifying — a policy trained on those
rules learns exploits that don't transfer. `train_flow` trains on the real rules: 13 actions
(4 moves / 5 switches / 4 move+Terastallize), faint replacements and pivot landings as their own
requests, and teams drawn from the pinned PS random-battle generator pool (2000 real teams).

One consequence worth knowing: replacements and pivot landings are **single-sided** requests, so
at those steps one side's stored action is a no-op the engine discarded. The buffer carries an
`active` mask and `ppo_update` zeroes the policy/entropy terms there — value regression and GAE
still run over every step, because the dynamics are real either way.

## 3. Throughput: where it actually goes

Measured on this box (AMD EPYC 7513, 8 cores, A100-80GB). End-to-end training:

| config | env-steps/s |
|---|---|
| 512 envs | 1.7k |
| 4096 envs | 4.5k |
| 8192 envs | 5.3k |

**The run is CPU-bound, not GPU-bound.** 97% of a rollout step is inside the Rust engine and the
A100 sits near 0%. But the reason is much more specific than "8 cores is not many", and it is
fixable.

### One move costs ~72% of all engine time

Per-decision cost on real random-battle teams is extremely heavy-tailed:

| pool | p50 | p90 | p99 | p99.9 | max | share of time in slowest 1% |
|---|---|---|---|---|---|---|
| fixed debug matchup | 103 µs | 252 µs | 530 µs | 6.8 ms | 7.4 ms | 32% |
| real randbats teams | **11.9 µs** | 243 µs | 510 µs | **252 ms** | **372 ms** | **88.8%** |

The median real-team decision takes 11.9 µs — about 84k decisions/s. The problem is a tail where
one decision in a thousand costs ~30,000× the median. Ranking slow decisions by total time
attributes ~72% of *all* engine runtime to **Triple Axel**, in 16 decisions out of 8,000.

`generate.rs`'s `tripleaxel | triplekick` arm enumerates `HitCombos::new(k)` for k=1..3 — every
combination of (16 damage rolls × 2 crit) per landed hit, so 32 + 32² + 32³ = **33,824 branches**,
each cloning a 1,520-byte `State` plus its instruction vector (~51 MB of copies per decision).
`Exec::Sample` then prunes all of it to one. Dual Wing Beat (2 hits, 1,024 branches, ~5.7 ms) and
Surging Strikes are the same shape, two orders of magnitude smaller.

Removing Triple Axel from the team pool (10.2% of teams) — as a measurement, not a fix:

| pool | 1 thread | 8 threads | scaling |
|---|---|---|---|
| full | 1,256 dec/s | 1,775 dec/s | 1.41× |
| no Triple Axel | 8,777 dec/s | **35,081 dec/s** | **4.0×** |

**7× single-thread, 19.8× on 8 cores.** Note 35k decisions/s is *above* the 21.4k the docs quote
for the trivial matchup — with real teams. The engine is not slow.

It also explains the **parallel scaling collapse**: `step_all` is a barrier, so a single 372 ms
decision stalls all 8 threads while 255 other envs sit finished. Fixing the tail is what restores
scaling; adding cores without fixing it buys almost nothing (1→8 threads is currently 1.41×).

### The fix already exists in the tree

`apply_multihit_realized_ma` resolves a multi-hit move *sequentially* — one accuracy roll, one crit
roll, one damage roll per hit — producing a single branch. It is PS-faithful and is what the seed
gate certifies at 386/401. It is reachable only when a `RealizedCursor` is present, and the only
two `RealizedSource`s are `Prng` (seed gate) and `Recorded` (differ). **The training path supplies
neither, so `Exec::Sample` takes the enumerate-then-prune path.**

The change is to give `Exec::Sample` a realized source of its own (a splitmix variant alongside
`Prng`/`Recorded`) so sampling realizes hits instead of enumerating them. `Exec::Sample` is already
distribution-pinned against `Enumerate` by `tests/sampled_distribution.rs`, which is exactly the
guard that would catch a mistake. Enumerate-mode semantics must not move — that is what every
parity gate depends on.

### Status: fixed on a branch

Landed on **`sample-multihit-realize`** (worktree at `/home/user/mh-fix`, commit `93a63c0`),
kept off `main` so it can merge independently of the parity campaign running elsewhere:

    move           enumerate     sample (before)   sample (after)
    thunderbolt        662/s          3851/s           3812/s
    bulletseed         200/s           453/s            456/s
    dualwingbeat        40/s            92/s              96/s
    tripleaxel         1.1/s             2.4/s        48,452/s

Gates on that branch: engine suite green; seed gate **386/401, identical to main**; Enumerate
corpus byte-identical to main over a 282-unit subset at 100% exactness.

Scope is deliberately narrow — only Triple Axel / Triple Kick realize under Sample. Attempting it
for the variable [2,5] moves, Population Bomb and Beat Up broke the sampled transcript: Enumerate
compresses those with the sumset DP into a SINGLE aggregated `Damage { amount: total }`, while the
realized executor emits `Damage` per hit. Same final HP, different instruction stream — and the
protocol emitter, narrator, apply/reverse roundtrip and the distribution support check all read
that stream.

Still open (deliberately not taken on that branch):
* **The exact-hits arm** (`hits_min == hits_max <= MAX_EXACT_HITS`) still enumerates —
  Dual Wing Beat is 1,025 branches / ~10 ms, Surging Strikes similar. Routing it through
  realization would need the seed gate re-verified, since that arm is shared with Replicate.
* **Whether the DP path should emit per-hit damage** in Enumerate too. That would make the
  transcripts agree and unlock realization for the whole multi-hit family, but it changes what
  every consumer of the instruction stream sees — a design decision, not a perf tweak.
* The **11.5% of runtime** in decisions with no move in the protocol log (~336 ms each), still
  unidentified.

### It gets worse as the agent improves

Instantaneous throughput over the first 61 updates of `runs/scale1`, against policy entropy:

| update | sps | entropy |
|---|---|---|
| 2 | 4,423 | 1.93 |
| 18 | 3,410 | 1.49 |
| 39 | 2,652 | 1.29 |
| 55 | 1,896 | 1.16 |

Throughput fell **2.15×** as the policy sharpened. Triple Axel is a strong move, so a learning
agent selects it more often than the uniform-random policy the benchmarks use — the pathological
path gets hit *more* as training progresses. Any step-budget estimate taken early in a run is
therefore optimistic.

### Secondary: allocator

Turn resolution allocates hard (a cloned `State` and a growing `Vec<Instruction>` per branch).
Swapping glibc malloc for **mimalloc** is +25% single-thread and +37% at 8 threads, with no
semantic change. It is now the default in `crates/pybridge/Cargo.toml`
(`--no-default-features` to A/B it). It does *not* fix the scaling collapse — that is the tail.

### Two documented numbers that do not reproduce

* `RESEARCH_PLAN.md` §P1.4 quotes **21.4k turns/s single-thread**; `bench_steps` gives **5.9k**
  here. Part is a slower core (EPYC 7513 vs a laptop).
* More importantly that benchmark runs `team::default_matchup()`, and every headline throughput
  figure in the docs was measured on it. It is not representative: it lacks the moves that
  dominate real cost.

## 4. The on-policy cosim sidecar

The parity corpora are **PS-led with random-ish choices**: Showdown plays and the engine has to
reproduce it. That certifies the states *those* games visit — not the states a trained policy
visits. RL adversarially searches for reward, so a divergence anywhere on the policy's own
distribution is an exploit waiting to be farmed. `RESEARCH_PLAN.md` §P1.1 calls engine-led
on-policy cosim non-negotiable before a scale run; the sidecar is that check, running continuously
against the live checkpoint.

It is **deterministic** — one outcome per decision, byte-compared, no enumeration and no path cap.
Each sweep, per seed:

1. `harness/cosim.mjs --seed S --policy "<cmd>"` plays a full `gen9randombattle` inside pinned
   Showdown with every choice supplied by the run's latest checkpoint. `--policy` is a new flag:
   it spawns `agents/scripts/policy_server.py` as a long-lived child and exchanges one
   line-delimited JSON message per decision. The policy was trained on `encode()` of an engine
   `State`, so each PS request is converted through the certified `convert_state` and the training
   encoder (`showdown_engine.encode_ps_state`) — it sees exactly its training inputs. PS's own
   request is the authority on legality; the engine mask is intersected with it, so a disagreement
   costs an action instead of aborting the recording. Output is a standard v2 trace.
2. `SEED_GATE=1 cosim <trace>` replays it: the engine's `Replicate` executor is driven off a
   `PsPrng` seeded from the same battle seed, and the converted state is byte-compared after
   **every** decision. Any drift desynchronises the PRNG stream and shows immediately.

First measured result on this box: **3/3 full games exact, 122 decisions**, against
`ckpt_000002621440`.

```sh
cat runs/scale1/cosim/verdicts.log   # one line per sweep: CLEAN / DIVERGENCE
```

Knobs: `SIDECAR_EVERY` (default 1800s), `SIDECAR_GAMES` (4), `SIDECAR_SEED_BASE` (40000, kept
clear of the committed `rb1000+` fixture seeds), `SIDECAR_MAX_DECISIONS` (400). Battle seeds must
stay under ~60000 — PS seeds are four `u16` limbs and the recorder derives them as
`[S, S+7, S+13, S+29]`, so a larger S fails to parse as a trace seed.

By hand, against any checkpoint:

```sh
cd showdown-rs
node harness/cosim.mjs --seed 50001 --format gen9randombattle --max-decisions 300 \
  --out /tmp/op.json.gz \
  --policy "../agents/.venv/bin/python ../agents/scripts/policy_server.py \
            --ckpt ../agents/runs/scale1/ckpt_000002621440.pt --device cpu"
SEED_GATE=1 ./target/release/cosim /tmp/op.json.gz
```

### Cost

A sweep (3–4 games recorded in PS + the gate) takes **16–20 s wall**, measured with the trainer
running. At the default 1800 s cadence that is a ~1% duty cycle, and both halves run `nice -n 19`.

Measured A/B on the live run (instantaneous sps between consecutive updates):

| phase | n | mean sps | range |
|---|---|---|---|
| sidecar off | 5 | 2,654 | 2,237–3,141 |
| `SIDECAR_EVERY=120` (~17% duty) | 3 | 2,673 | 2,236–2,972 |
| `SIDECAR_EVERY=1800` (default) | 4 | 2,856 | 2,651–2,990 |

**No measurable cost, even at a 15× cadence.** `nice -n 19` does its job: the sidecar is
essentially single-threaded while the trainer's rayon pool saturates all 8 cores, so the scheduler
hands the trainer the cores whenever it wants them.

Note the range column: update-to-update sps varies ~40% on its own, which is far larger than any
sidecar effect — so don't read a single update's sps as a signal about anything.

**The sidecar only reports.** Divergence burn-down is a separate campaign; the gate's ranked
first-divergence lines land in `runs/<run>/cosim/gate-*.txt` and in `sidecar.log`.

### The other gate, and why it is not the one running

`harness/onpolicy-gate.mjs` is a second, *transplant*-based on-policy check: it takes engine
states sampled straight out of the training env (`agents/scripts/onpolicy_sample.py`), loads them
into PS via the certified exporter, and checks legality plus whether the engine's realized
outcome is reachable in PS at all. It exists because the training env samples outcomes with its
own splitmix RNG, so a training trajectory is *not* PS-reproducible and membership is the
strongest question available — which means enumerating PS's outcome tree under a `--max-paths`
cap, with `inconclusive` results when the cap bites.

The deterministic path above is strictly stronger and much cheaper, so it is what the sidecar
runs. The transplant gate is still useful for two things the deterministic path does not cover:
it exercises the **exporter** on on-policy states, and it checks **legality** directly. Known
limitation: single-sided requests (faint replacement, pivot landing) are skipped, because PS's
`requestState` is not part of `serializeBattle`, so a deserialized battle always resumes believing
it is at a normal turn.

## 5. Gates to run before believing a result

```sh
cd showdown-rs
cargo test --release -p engine
SEED_GATE=1 ./target/release/cosim harness/seed-fixtures/*.fx.json.gz     # 386/401 on this box
cargo run --release -p cosim -- harness/cosim-traces/*.json.gz            # corpus sweep
```
