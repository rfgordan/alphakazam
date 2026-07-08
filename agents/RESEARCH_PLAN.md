# Research Plan: Expert-Level Pokémon Showdown via RL

Priorities, in order: (1) playing strength, (2) training/GPU efficiency, (3) elegance.

## 1. Objective and scope

**Target format: `gen9randombattle`.** It isolates battling skill from team-building (its own
meta-game), matches our engine's verified corpus (the `r*` cosim traces), and is directly
comparable to prior work (Metamon on gens 1–4 random battles, PokeLLMon/PokéChamp on gen9).
Hidden information is present, but the team generator's distribution is *known* — a lever we can
exploit (see §5). Gen9 OU is a stretch phase, not the main line.

"Solving" Showdown in the Nash sense is not certifiable at this state-space size. Operational
definition of done:
- **M2:** ≥55–60% vs foul-play (expectiminimax search) — beyond strong classical baselines.
- **M3:** top-decile ladder (~1500+ Elo / GXE 75+ on gen9randombattle).
- **M4:** robustness — a dedicated RL exploiter trained against the frozen final agent wins <60%.

## 2. Why we're unusually well-positioned

- **Verified fast simulator.** The Rust engine is cosim-verified against pinned PS (b9dc987d):
  OU gate 100%, randombattle corpus down to 4 divergences. Flat `Copy` state, reversible
  instructions, PyO3 bridge, 633-dim obs + ID embeddings, exact legal-action masks. Published
  work trains either against the real PS server (throughput-bound: ~10²–10³ steps/s) or on
  offline replays (Metamon). A near-exact simulator at 10⁴–10⁵+ steps/s is a 100–1000×
  sample-budget advantage, and sample budget is the main determinant of both strength and GPU
  efficiency here.
- **Working PPO stack** (~5M-param actor-critic, ID-embedding tables, aux prediction heads,
  KL-early-stop fix for the instability we diagnosed empirically).
- **Sim2real loop already built:** the poke-env + live-Showdown env lets us run the *same policy*
  on the real engine periodically and diff win rates.
- **Measured baseline ladder:** maxbp 0.90 and SimpleHeuristics 0.98 vs random; foul-play
  (search) available via adapter as the top scripted rung.
- **Cosim methodology** to keep the simulator honest as mechanics coverage grows.

## 3. Phased plan

### P0 — Infrastructure hardening (~1–2 weeks; partly in progress)
1. Drive cosim divergences 4 → 0 (in progress).
2. **Engine-led on-policy cosim** (backlog #15): sample states/trajectories from *our policy's*
   distribution, replay them through PS, diff exactly. Non-negotiable: RL is an adversarial
   search for reward, and any engine divergence is exploitable reward hacking. PS-led traces
   don't cover the states a trained policy visits.
3. Throughput: benchmark the vectorized Rust env end-to-end with batched GPU inference; make the
   pybridge return batched numpy (obs encoding stays in Rust). Target ≥20–50k env-steps/s on one box.
4. Fixed eval harness: 1000-game evals with 95% CIs vs {random, maxbp, heuristic, foul-play-fast,
   foul-play-full}, seeded, W&B dashboards, checkpoint Elo pool.
5. Hardware: the 16GB MacBook Air is for smoke tests only (it has already OOM'd under parallel
   load). Rent one 4090/A100-class box with 32–64 cores for real runs.

### P1 — Saturate scripted opponents (~2–4 weeks)
- **Obs v2 (public-info only):** revealed opponent moves/items/abilities as they're exposed,
  hazards/screens/field detail, tera state, per-move STAB × type-effectiveness × base-power
  "best damage" hints (SimpleHeuristics wins 0.98 vs random with exactly these features — they're
  a proven sufficient basis).
- **Reward rebalance:** terminal ±1 dominant; small potential-based shaping in the exact PBRS
  form (γΦ(s′) − Φ(s), Φ(terminal)=0; Ng et al. 1999), annealed to zero mid-training.
- Curriculum: random → maxbp → heuristic mixture → frozen self-checkpoints.
- Keep the aux heads (opponent-action + turn-outcome prediction) — cheap representation shaping
  aligned with the POMDP literature.
- **Gate M1:** ≥85% vs maxbp, ≥60% vs SimpleHeuristics, critic explained-variance plateaued >0.3.

### P2 — Self-play league + memory (the core; ~4–8 weeks)
- **Recurrence before transformers:** add a GRU over turn history (Ni et al. 2022: recurrent
  model-free is a strong POMDP baseline). Consider a 15–30M-param transformer only if the GRU
  plateaus below M2.
- **League, not naive self-play.** Pokémon turns are simultaneous-move with hidden information —
  equilibrium play is *mixed*, and naive self-play best-response dynamics cycle
  (rock-paper-scissors in strategy space; Czarnecki et al.'s "spinning top"). Opponent pool =
  current agent + reservoir of past checkpoints (approximate fictitious self-play), plus periodic
  dedicated exploiters (AlphaStar league pattern).
- **Equilibrium regularization, cheapest first:** entropy floor → MMD (Sokota et al. 2023) →
  full R-NaD (DeepNash) only if cycling persists. MMD is likely the sweet spot on elegance.
- **Belief head with exact supervision — our unique lever.** In randombattle the opponent's team
  is drawn from a *known generator*; in our sim we hold the ground truth. Train an auxiliary head
  to predict the opponent's hidden set (moves/item/ability/spread) from public history, with exact
  labels, and feed its posterior into the policy. This is foul-play's Bayesian set inference,
  learned. **Privileged-info hygiene:** ground truth appears only in aux *targets*, never in the
  policy's input features.
- **Sim2real certification cadence:** every N updates, run the current policy on real PS
  (poke-env path) vs the same baselines; a win-rate delta beyond noise ⇒ engine gap ⇒ cosim hunt.
- **Gate M2:** ≥55% vs foul-play-full; league Elo stable/monotone vs frozen pool (no cycling).

### P3 — Human data + ladder (optional accelerant; parallelizable)
- **Warm starts as compute discounts:** (a) imitation from foul-play trajectories *generated on
  our fast engine* (AlphaStar SL-init pattern; nearly free), and/or (b) behavior cloning /
  offline RL on PS replay corpora (Metamon released parsed replay datasets and showed offline RL
  reaches human level on gens 1–4; PokéChamp released a large gen9 dataset). Replays contain only
  public info for both sides — reconstruction required.
- **Ladder protocol:** registered bot account, throttled; 200–500 games; report Elo/GXE with CIs.
- **Gate M3:** top-decile gen9randombattle.

### P4 — Stretch: OU, teambuilding, exploitability audit
- OU with a fixed curated team set + team-preview head; set inference shifts from known-generator
  to Smogon-usage priors (harder).
- Teambuilding as a meta-game: PSRO / double oracle over team space (Lanctot et al. 2017).
- Publish an exploitability curve: train exploiters vs the frozen agent at each milestone.
- Eval-time search multiplier: determinized / information-set MCTS on the Rust engine with the
  learned value function (caveat: naive MCTS is unsound under imperfect info — see ReBeL for why;
  treat as an empirical boost, not a principled solution).

## 4. The hardest parts (ranked, explicit)

1. **Hidden information.** Inferring the opponent's set/moves *is* the skill ceiling of
   randombattle. Mitigations: known-generator belief supervision (§P2), recurrence, replays.
2. **Self-play nonstationarity/cycling** under simultaneous moves. Progress measurement itself is
   subtle (nontransitivity — beating checkpoint N says little about N−5). League + regularized
   dynamics + fixed external eval suite.
3. **Stochasticity → return variance.** Damage rolls, crits, misses, secondaries put a hard floor
   on value-function error (watch explained variance, not raw value loss) and a high sample cost
   on policy evaluation. Answer: huge cheap batches (the Rust engine), KL-guarded updates, PBRS.
4. **Simulator fidelity under coverage growth.** Every newly-exercised ability/item/move is a
   divergence risk, and RL *actively searches* for them. The cosim harness must stay in CI, and
   engine-led on-policy certification (P0.2) is mandatory before big runs.
5. **Evaluation cost and noise.** Elo CI is ~±50 at 200 games; ladder is worse (opponent pool
   drift). Fixed seeded suites, CIs everywhere, cheap CPU eval farms.
6. **Long-horizon credit assignment.** Hazards/status set up wins 30+ turns later: γ≈0.995–0.999
   territory, careful GAE λ; the dynamics aux head helps.
7. **(P4 only) Teambuilding meta-game** — a research project of its own; explicitly out of scope
   until the battling policy is strong.

## 5. Key design decisions (with recommendations)

| Decision | Recommendation | Why |
|---|---|---|
| Training sim | Rust engine only; PS server for certification/eval | 100–1000× throughput; verified |
| Format | gen9randombattle first | No teambuilding; matches corpus; comparable to SOTA |
| Net | MLP+embeddings → GRU → (maybe) small transformer | Capacity added only on demonstrated plateau |
| Self-play | Checkpoint-reservoir league + exploiters | Anti-cycling with minimal machinery |
| Equilibrium method | Entropy floor → MMD; R-NaD only if needed | Elegance/complexity ladder |
| Hidden info | Aux belief head, exact generator labels, aux-only | Unique asset; strict info firewall |
| Reward | Terminal ±1 dominant + annealed exact PBRS | Win is the objective; shaping is scaffolding |
| Warm start | foul-play imitation (cheap), replays (bigger lift) | Compute discounts, not dependencies |
| Search at train time | No (PPO only); IS-MCTS at eval as stretch | GPU efficiency; imperfect-info soundness |

## 6. Efficiency plan (priority 2)

- The Rust engine is the whole ballgame: at 30k steps/s, 100M steps ≈ 1 hour; 1B ≈ half a day.
  Budget estimate: P1 ~50–100M steps (≈half a GPU-day), P2 ~0.5–2B steps (≈1–2 GPU-weeks
  including league overhead), P3 BC ~1–2 GPU-days. Total well under one A100-month.
- **Simplest architecture that saturates one GPU** (also the most elegant): synchronous
  vectorized PPO, num_envs 1k–8k, batched forward, bf16 + `torch.compile`. No distributed
  actor-learner machinery unless profiling proves the GPU starved (EnvPool/Podracer patterns).
- Nets ≤10M params until M2 forces otherwise. OpenAI-Five/AlphaStar scale is explicitly not
  required for this game.
- Eval on CPU in parallel with training; imitation warm starts to cut PPO steps.

## 7. Evaluation protocol (fixed, versioned)

1. Seeded 1000-game suite vs {random, maxbp, heuristic, foul-play-fast, foul-play-full} with CIs.
2. All-checkpoint Elo pool (league health / cycling detector).
3. Sim2real delta: |win-rate on Rust engine − on real PS| within noise for identical opponents.
4. Behavior probes (built): damage-move %, voluntary-switch rate, status usage, tera timing,
   avg game length — catches degenerate styles before they cost a week.
5. Exploiter audit at each milestone.

## 8. Risks

- **RL exploits an engine divergence** → P0.2 certification loop + cosim in CI (top risk, owned).
- **League collapse/cycling** → reservoir + exploiters + external fixed suite as ground truth.
- **Privileged-info leakage** (belief labels reaching the policy input) → strict public-info
  schema for obs; aux-only for ground truth; test by ablating.
- **Ladder ToS/etiquette** → registered bot account, rate limits.
- **Compute creep** → hard gates between phases; capacity increases require a plateaued gate.

## 9. Reading list (annotated)

**Pokémon-specific**
- Grigsby et al. 2025, *Human-Level Competitive Pokémon via Scalable Offline RL with
  Transformers* (Metamon, arXiv:2504.04395) — closest prior; offline RL on ~475k+ human replays,
  transformers, top-10% ladder on gens 1–4 random battles; released parsed replay datasets. Read first.
- Karten et al. 2025, *PokéChamp: an Expert-level Minimax Language Agent* — LLM+minimax on gen9;
  released a large gen9 battle dataset useful for P3.
- Hu et al. 2024, *PokeLLMon* (arXiv:2402.01118) — LLM agent baseline, gen9 randombattle.
- Huang & Lee 2019, *A Self-Play Policy Optimization Approach to Battling Pokémon* (IEEE CoG) —
  early PPO self-play on Showdown; useful for what plateaued and why.
- pmariglia's **foul-play / poke-engine** (GitHub) — expectiminimax + explicit Bayesian set
  inference; mine the inference design for our belief head. Also David Stone's *Technical Machine*.
- Reis et al., *VGC AI Competition* (IEEE CoG) — doubles + teambuilding framework, if ever relevant.

**Imperfect information & self-play**
- Perolat et al. 2022, *Mastering Stratego with Model-Free Multiagent RL* (DeepNash / R-NaD,
  Science) — the template for large imperfect-info zero-sum without search.
- Sokota et al. 2023, *A Unified Approach to RL, QRE, and Two-Player Zero-Sum Games* (MMD,
  arXiv:2206.05825) — simpler regularized-equilibrium method; likely our first reach.
- Heinrich & Silver 2016, *NFSP* (arXiv:1603.01121); Lanctot et al. 2017, *PSRO*
  (arXiv:1711.00832) — fictitious play and the meta-game view (teambuilding).
- Vinyals et al. 2019, *AlphaStar* (Nature) — league/exploiter design; SL warm start.
- Czarnecki et al. 2020, *Real World Games Look Like Spinning Tops* (arXiv:2004.09468) — why
  leagues, nontransitivity.
- Brown et al. 2020, *ReBeL* (arXiv:2007.13544) — belief-state search; context for why naive
  MCTS is unsound here.

**On-policy RL practice**
- Schulman et al. 2017, *PPO* (arXiv:1707.06347).
- Andrychowicz et al. 2021, *What Matters in On-Policy RL* (arXiv:2006.05990) — large-scale
  hyperparameter study; directly speaks to our KL-instability findings.
- Berner et al. 2019, *OpenAI Five* (arXiv:1912.06680) — long-horizon PPO, reward shaping, surgery.
- Ng, Harada & Russell 1999, *Policy Invariance Under Reward Transformations* (PBRS).
- Ni et al. 2022, *Recurrent Model-Free RL Can Be a Strong Baseline for Many POMDPs*
  (arXiv:2110.05038).

**Throughput engineering**
- Weng et al. 2022, *EnvPool* (arXiv:2206.10558); Petrenko et al. 2020, *Sample Factory*
  (arXiv:2006.11751); Hessel et al. 2021, *Podracer* — batched-env + GPU-inference loop patterns
  we replicate in Rust.

**Offline warm start (P3)**
- Kostrikov et al. 2021, *IQL* (arXiv:2110.06169), or filtered BC; Metamon's ablations cover
  what works on Pokémon replays specifically.

## 10. Immediate next actions

1. Finish divergences 4 → 0; then build engine-led on-policy cosim (#15).
2. Benchmark vectorized Rust env + batched inference; batch the pybridge.
3. Reward rebalance (terminal-dominant + exact PBRS), obs v2, GRU option.
4. Stand up the fixed eval suite with CIs; wire foul-play as an eval opponent (adapter exists).
5. Rent a GPU box; first 100M-step P1 run.
