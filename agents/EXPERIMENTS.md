# Deep-Showdown RL — Design Summary & Experiment Log

Living document: current recipe, design choices with their evidence, and a short log of every run.
Update per experiment (append a log row + amend the recipe when a lever is adopted).
Companion docs: `EVAL_RUBRIC.md` (behavior metrics & review protocol), `RESEARCH_PLAN.md` (long-term).

## Current recipe (as of night2)

**Task**: gen9randombattle vs mixed opponents, from-scratch PPO, no BC. North-star: heuristic win-rate
(SimpleHeuristics ≈ 0.98 vs random) with behavioral depth (EVAL_RUBRIC rows).

| component | choice | why (evidence) |
|---|---|---|
| **Obs (v5, 437 dims)** | public-info only; per-move: bp/acc/eff/est-damage/KO-flag/type-onehot/bench-eff + **mechanics** (is-phys, self-boost off/def/spe, inflicts-status, heal, priority); per-team-slot: hp/fainted/active + types + matchup scalars; globals: field/hazards + **timers**, incoming-threat from revealed foe moves, volatiles+counters both actives, unrevealed count | decision features ≫ identity embeddings at our sample scale; mechanics block gave 2× faster learning + first setup *conversion* (E12) |
| **Frames** | ×2 (prev obs appended after current; indices stay valid) | best late slope vs heuristic (D3: 0.76 rising); memory for the switching game |
| **Arch** | `setslot` ~262K params @ frames 2: trunk(240×2) over TRUNK_IDX subset (type one-hots excluded) + shared per-move scorer + shared per-switch scorer + DeepSets pooled slot context to trunk & scorers | slot-shared scorers make positional bias unrepresentable (flat MLP learned 63% slot-3 bias, plateaued 0.83 vs random); set-context required for setup to emerge at all (B: 0, C: 0.33/game) |
| **Reward** | ΔΦ per step + terminal ±20; Φ = 0.5·HPdiff + 1.5·faints (unknown foes = full HP). Boost/status Φ-terms now OPTIONAL/off (E3: wr-redundant post-features; weak unresolved edge on conversion quality) | single-potential elegance; reveal-artifact fixed |
| **Credit redistribution** | RETIRED from default (flag remains). E12 converted without it; persistence claim confounded with frames; highest-complexity component | complexity not earned (E3/E12 deconfounding) |
| **Curriculum** | frontier PFSP: episode weight ∝ base·(wr(1−wr)+0.1)², over random:1, maxbp:1, heuristic:2, self:0.5 (frozen snapshots q50) | from-scratch 0→0.46 in 22 min; hardest-first wastes episodes on unbeatable opponents cold |
| **PPO** | γ 0.99, λ 0.95, clip 0.2, lr 3e-4 const, ent 0.005–0.01 const (NO anneals — horizon-agnostic), epochs 6, minibatch 512, KL-early-stop 0.03, batch = 10 envs × 192 | env-bound 30:1 (t_collect 2.5s vs t_update 0.08s) → extra epochs free; entropy self-anneals 1.7→0.35; schedules overfit the horizon (Rob) |
| **Infra** | trainer-owned server recycled q40 updates; 10 thread-envs, batched forward; flake-rebuild; true resume; `--max-minutes`; wandb `deep-showdown` (all runs) | server rooms degrade sps 650→24 unrecycled; winning ends games faster (A 422 sps vs C 680) |

**Known gaps**: identity (species/ability/item) unmodeled; status usage 73% redundant; hazards/recovery
unlearned; no real recurrence (GRU is the next capacity step if frames plateau); imperfect-info belief.

## Experiment log

| run | Δ vs predecessor | heuristic wr | behavior/notes |
|---|---|---|---|
| *(pre-history)* flat MLP scratch | — | 0.83 **vs random** plateau | 63% slot-3 positional bias; Defog spam; probe-driven diagnosis |
| *(pre-history)* BC+value pretrain | imitation (now out of scope) | 0.985 vs random | proved capacity; established probe/rubric tooling |
| long-med (1.5M warm) | v2 obs, hard PFSP | 0.45 plateau | first mixed-opponent run; no forgetting |
| long-v3 (2.8M warm) | +v3 obs (damage calc, matchup) | 0.61–0.74 | +20pt step from matchup features; setup still 0.03/g |
| **A** (22min scratch) | frontier PFSP | 0.46 tail | from-scratch viable |
| **B** | +Φ boost/status terms | 0.56 | +0.10; setup still 0 (credit without representability) |
| **C** | +setslot set-context | **0.70** | +0.15; setup EMERGES 0.33/g — then extinct by 1M steps |
| final90 (annealed, 90min) | +anneals +self | 0.67 @1.2M | anneal exonerated for extinction; throughput confounded |
| deliverable (33min, killed) | C+self, const hparams | 0.63–0.65 @1.25M | setup extinct under const entropy too → economics, not schedule |
| **D1** | boost 0.3 | 0.60 | incentive size ≠ answer; recovery dabbling appears |
| **D3** | +frames 2 | **0.76 rising** | best late slope; memory pays vs switcher |
| **D4** | +redistribution | 0.64–0.70 | wr-neutral; setup persists 0.1/g @1M (vs 0); flow decays 5.8→1.0 |
| night1 (partial, 1.7M) | C+frames+redist+self | 0.65–0.75 | setup ALIVE 0.6/g @1.5M (6× deeper than ever); conversion still wasted 0.78 |
| **E12** (920k) | +move mechanics +field timers +volatiles (NOTE: ran WITHOUT redistribute) | 0.64 tail (2× faster early: 0.72 @210k) | **first CONVERSION**: 0% wasted, post-setup dmg 0.27 (prior ≤0.15), boostedKOs 0.45/g — achieved w/o redistribution → redistribution's unique value now unproven; hazards/recovery still 0 |
| night2 (4M) | full recombination: v5+frames+redist+self | **plateau ~0.70–0.75** from 1.4M→4M (close 0.75/0.77) | features×frames did NOT compound past the ceiling; BUG FOUND: ModelPlayer didn't frame-stack → self-play opponents played random (PFSP starved them to w0.02; fixed after) |
| **E3** (920k) | E12 minus Φ boost/status terms (Rob's ablation) | 0.68 tail (≈E12) | **Φ-terms wr-redundant post-features**; setup discovery survives (0.35/g); conversion sloppier (wasted 0.43 vs 0.0, n~5 — inconclusive). RULINGS: redistribution RETIRED (complexity high, unique value never survived deconfounding); Φ-terms optional/off by default |
| night2 snapshots | behavior-over-time probes @480k/1.9M/3.9M | — | setup 0.33→0.07→**0.47/g (wasted 0.14)**: emergence → dip → RE-EMERGENCE with conversion at depth; wr plateau ≠ behavioral stagnation; status play deleted entirely |
| night3 (stopped @2.4M) | elegant recipe: v5+frames, no Φ/redist, REAL self-play | 0.60→0.75, tail 0.73/0.62 | **real self-play verified in prod** (self wr 0.50, 32% of episodes); no plateau-break yet at 2.4M; superseded by night4 |
| **night4** (16M done; extended→27.6M, stopped) | + v6 verb block + v7 context/legality — obs 665, 280K params | flat ~0.70 from 0.6M→14M (powered: 0.717@6M, 0.690@14.2M), then **LATE STEP: 0.763±0.029 @16M; extension HELD the level: 0.746±0.038 @27.3M** (500 eps) | punctuated equilibrium real; behavior phase transition consolidated into a stable "boost-and-break sweeper" identity: setup 1.7/g both opponents, avg max boost 2.4, bKO 1.0/g vs heuristic, recovery integrated (0.4-0.85/g), **status redundancy 0.00 everywhere** (pathology resolved). Weaknesses: over-investment (wasted 0.5-0.7 — stacks boosts beyond conversion), hazards never learned (0.03), safe-setup drops to 0.69 vs heuristic. New plateau ~0.75; next levers: GRU, foul-play calibration, identity tier |
| **P0.1 calib** (eval only, 2026-07-11) | night4 vs **foul-play** (expectiminimax + set inference), 100ms search, via challenge/accept pairs on local PS (`scripts/fp_eval*.sh`) | **0.106 ± 0.035** (31W/262L, n=293; path validated: 0.70 vs heuristic n=30 same code) | the frontier calibration RESEARCH_PLAN P0.1 called for: 0.746-vs-heuristic is nowhere near the ceiling. Foul-play's edge = set inference + search ⇒ identity/belief load-bearing for the new policy, not accelerants. 2000ms full arm (n=150) in `runs/fp_eval/` |

## Decision principles (learned the hard way)
1. Plateau → behavior-probe BEFORE tuning (every real gain came from a diagnosed constraint).
2. Skills can emerge then go EXTINCT — probe along runs, not endpoints.
3. Single-variable probes at fixed STEP budgets (~920k) for attribution; combine only winners.
4. Judge levers on wr AND behavior; wr saturates and hides both gains and losses.
5. No horizon-tuned schedules; constant hparams, `--max-minutes` only as a stop.
6. Self-play steps can arrive VERY late (night4: +0.07 in the final 12% of a run after 8M flat
   steps) — don't certify an asymptote while the league is still hardening; check behavior
   metrics for phase-transition precursors before calling it.
7. Trust only powered evals (300+ eps); in-run n=60 readings are a ±0.10 instrument and WILL
   manufacture breakthroughs. Rolling train wr tracks the stochastic policy, not the greedy one.
