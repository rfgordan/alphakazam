# Research Plan v2: Superhuman Pokémon Showdown via RL Self-Play

Supersedes v1 ("Expert-Level", June 2026). Priorities unchanged: (1) playing strength,
(2) training/GPU efficiency, (3) elegance. What changed: the ambition (expert → **superhuman
SOTA**), and the starting point — v1's P1 is now *done and measured* (night4: 0.746±0.038 vs
SimpleHeuristics, from scratch, no BC, on the poke-env path at ~650 sps). This plan starts from
that frontier. Companion docs: `EXPERIMENTS.md` (recipe + run log + decision principles),
`EVAL_RUBRIC.md` (behavior probes), `showdown-rs/HANDOFF.md` (engine parity state).

## 0. Problem statement and definition of done

**Format: `gen9randombattle`.** Isolates battling from teambuilding, matches the verified cosim
corpus, comparable to all published baselines. OU/teambuilding stays a stretch phase.

"Superhuman" must be operational, not vibes. Three claims, each independently certified:

- **S1 — SOTA vs machines:** beats every strong scripted/published baseline we can run locally:
  ≥65% vs foul-play (expectiminimax + Bayesian set inference — the strongest classical agent),
  and dominates LLM agents' reported ladder levels (PokeLLMon/PokéChamp territory, ~1300–1500).
- **S2 — Superhuman vs the ladder:** sustained ladder performance above the top human accounts
  on gen9randombattle — GXE in the high-80s+ over ≥500 games on a registered bot account
  (top humans sit roughly mid-80s GXE; we certify against the live leaderboard at run time,
  not a stale number). Metamon-class results (top-10% on gens 1–4) are the published RL
  high-water mark; top-of-ladder gen9 would be clear SOTA.
- **S3 — Robustness (the honest superhuman test):** a dedicated RL exploiter given serious
  compute against the frozen final agent stays <60% — i.e., strength is not a brittle style.

Anything short of S2 is "expert-level" and we say so.

## 1. Where we are (measured, July 2026)

**Agent (poke-env path, from scratch, no BC):**
- night4 certified **0.746±0.038 vs SimpleHeuristics** (500 eps; heuristic ≈0.98 vs random).
  Stable "boost-and-break sweeper" identity: turn-1 setup+tera, boosted KOs 1.0/game, recovery
  integrated, zero redundant status. Best checkpoint `runs/night4/model_27264000.pt`
  (obs v7 = 665 dims, setslot + frames 2 + set context, 280K params).
- Recipe levers with measured attribution (see EXPERIMENTS.md): frontier PFSP, set-context arch
  (required for setup to *emerge*), move-mechanics features (required for setup to *convert*),
  frames=2. Retired after deconfounding: credit redistribution, Φ boost/status terms.
- Two phenomena that shape everything downstream:
  **(a) setup extinction** — skills emerge, get pruned when unconverted, re-emerge with
  conversion at depth. Endpoint probes lie; probe along the run.
  **(b) punctuated equilibrium** — night4 was flat 0.6M→14M, then a +0.07 step in the final 12%.
  Don't certify asymptotes while the league hardens; only powered evals (300+ eps) count.
- Known weaknesses = next diagnoses: over-investment in boosts (wasted 0.5–0.7/game), risky
  setup vs strong opponents, hazards never learned (0.03/game), no identity knowledge
  (species/ability/item), no recurrence, no belief modeling.

**Engine (`showdown-rs`):** cosim-verified vs pinned PS (b9dc987d). P1.1 (Codex, 2026-07-11,
reviewed and approved): sampled corpus **1,532/1,532 exact, zero unsupported**, AND the exact
**distribution oracle** landed (PS-side exhaustive PRNG-tree enumeration, factorized per-action
move kernels vs `generate_move_action`, forced-switch + pivot boundary certification) — outcome
*probabilities* are now certified, not just outcome support. Seed campaigns through 240 clean;
capped seeds reported honestly as unverified. See `showdown-rs/P1_1_PARITY_PROGRESS.md`.
Open review asks: gates should fail on `unsupported > 0`; mutation suite re-run + probability
mutants. Engine-led **on-policy** cosim deliberately deferred until the new policy exists (Rob,
2026-07-11) — it certifies the states a specific policy visits, so it runs as the pre-scale-run
gate in P2. Known bridge-MDP deviations from real rules (faint replacement burns a turn, no
pivot, no tera in the action space) — must die before Rust-side training (§P1).

**Throughput:** poke-env + local PS ≈ 650–680 steps/s. The Rust engine's design target is
10⁴–10⁵+ — a 100–1000× sample-budget multiplier that nobody in the published literature has
(they train against the real PS server or offline replays). This is our structural advantage.

## 2. Theory of the gap: what separates 0.75-vs-heuristic from superhuman

Ranked by expected effect. Each is a research question with a falsifiable bet.

1. **Sample scale (RQ1).** night4's late step suggests the recipe is sample-starved, not
   capacity-starved: more phase transitions live beyond 27M steps. Bet: the same recipe at
   1–5B steps on the Rust engine crosses 0.9 vs heuristic without architectural change.
   The punctuated-equilibrium finding makes this the single highest-EV lever.
2. **Identity knowledge (RQ2).** The agent doesn't know what a Kingambit *is* — no
   species/ability/item representation. At 650 sps, embedding tables were unlearnable
   (decision features ≫ identity at small sample scale, measured); at 100× samples that
   reverses. Bet: ID embeddings (species/move/ability/item) become net-positive at ≥100M steps
   and are *necessary* for S1 (foul-play knows every set).
3. **Memory and belief (RQ3).** frames=2 was the best single win-rate lever — memory pays.
   GRU is the next capacity step. Beyond memory: in randombattle the opponent's team is drawn
   from a **known generator**, and in our sim we hold ground truth — train a belief head to
   predict the opponent's hidden set from public history with *exact labels* (foul-play's
   Bayesian inference, learned). Strict info firewall: ground truth in aux targets only, never
   in policy inputs.
4. **Opponent population quality (RQ4).** Simultaneous-move + hidden info ⇒ equilibrium play is
   mixed and naive self-play cycles. Frontier PFSP over a checkpoint reservoir is already in the
   recipe; superhuman needs the full league: main agent + reservoir + periodic dedicated
   exploiters (AlphaStar pattern), with equilibrium regularization escalated only on evidence:
   entropy floor → MMD → R-NaD.
5. **Eval-time search (RQ5, multiplier).** Determinized IS-MCTS over the Rust engine with the
   learned policy/value/belief. Unsound in principle under imperfect info (ReBeL caveat), often
   a real Elo boost in practice. Cheap to try once the value function is strong; may be the
   difference between S1 and S2.

## 3. Phased plan

### P0 — Calibrate the frontier (days; do first, it re-scopes everything)
The single most important unknown: **is 0.746 vs SimpleHeuristics near the ceiling or nowhere
close?** Never measured.
1. ✅ **MEASURED (2026-07-11): night4 = 0.106 ± 0.035 vs foul-play at 100ms search**
   (31W/262L, n=293; eval path validated by reproducing 0.70 vs SimpleHeuristics n=30 through
   the same code). Full-arm (2000ms) in flight. **The decision branch fired: 0.746-vs-heuristic
   is nowhere near the frontier.** Foul-play's margin = set inference + search — so identity
   (RQ2) and belief (RQ3) are load-bearing for S1, not optional accelerants, and move from
   "probe at P2" to "core P2 components". The pure-scale bet (RQ1) alone is unlikely to close
   a 0.11 → 0.65 gap.
2. Ladder probe: put night4 on a registered bot account for ~200 throttled games → a real
   Elo/GXE anchor tying our internal ladder (random/maxbp/heuristic/foul-play) to human Elo.
   (Blocked on account registration — needs Rob.)

### P1 — Engine to training-grade (~2–3 weeks; the unlock for everything after)
1. Parity 4 → 0, then **engine-led on-policy cosim**: sample trajectories from *our policy's*
   distribution, replay through PS, diff exactly. Non-negotiable — RL adversarially searches
   for reward, and any divergence is exploitable reward hacking. PS-led traces don't cover the
   states a trained policy visits. Stays in CI forever.
2. **Decision-point refactor** (the request-model state machine): kills the bridge-MDP
   deviations — real faint-replacement phase, pivot moves, tera in the action space (13-action
   space). Training on wrong rules is disqualifying for S2.
3. **Randombattle team generator in the training loop** (sample from PS's actual generator data,
   pinned) — the training distribution must match the ladder distribution, and the known
   generator is what makes exact belief labels possible.
4. Throughput engineering: batched obs encoding in Rust, pybridge returns batched numpy,
   vectorized env stepping. Gate: **≥20k env-steps/s end-to-end** with batched GPU inference
   on rented hardware. Profile before adding any distributed machinery (EnvPool/Podracer
   patterns only if the GPU is provably starved).
   **✅ MEASURED + GATE CLEARED (2026-07-11).** Enumerated executor: 2.0k turns/s single
   thread (encode ~2.6%). **Sampled executor (`Exec::Sample`, commit 2f2a144): 21.4k turns/s
   single thread — 10.7×.** Distribution-pinned to Enumerate (transcript membership + 5σ,
   100k draws, `tests/sampled_distribution.rs`); all parity gates re-verified green (sweep
   1532/1532, smoke 18/18, mutations 8/8). End-to-end `BattleVec` @1024 envs on the 10-core
   laptop: **104.7k turns/s raw, 95.4k turns/s with a both-sides torch forward in the loop**
   (≈190k agent-steps/s) — ~5× the 20k gate, ~150× the poke-env path, CPU-only. Remaining
   P1 engine work: the decision-point request machine + 13-action space (DECISION_POINTS.md
   steps 2-4) — correctness (real replacements/pivots/tera), not throughput.
5. Port the v7 obs/setslot encoder to the Rust env with a checkpoint-compatibility test
   (night4 must reproduce its poke-env eval numbers on the Rust env within noise — this *is*
   the sim2real gate in miniature). Keep obs layout changes append-only + write transfers
   (the v3–v6 orphaned-checkpoint lesson).
6. Hardware: rent a 4090/A100-class box (32–64 cores). The 16GB Air is for smoke tests only.
   Ops discipline from night4: detached launches, powered evals only, wandb everything.

### P2 — The scale run (RQ1 + RQ2; ~3–5 weeks)
1. **Reproduce-at-scale:** current recipe, unchanged, 1–5B steps on the Rust engine. This is
   the cleanest test of the punctuated-equilibrium bet. Probe behavior along the run
   (extinction watch), powered evals every ~250M steps.
2. **Identity tier:** species/move/ability/item embedding tables feeding the set-context
   encoder. Single-variable attribution at a fixed step budget (the E-series discipline,
   scaled: ~100M-step probes).
3. **GRU** over turn history replacing/augmenting frames (Ni et al.: recurrent model-free is a
   strong POMDP baseline). Transformer only if GRU demonstrably plateaus.
4. Capacity ladder: 280K → 1–5M → ≤15M params, each step gated on a plateaued predecessor.
- **Gate M1 (expert):** ≥0.90 vs SimpleHeuristics, ≥55% vs foul-play-full, sim2real delta
  within noise on real PS.

### P3 — League + belief (RQ3 + RQ4; the core; ~4–8 weeks)
1. Full league: main agent + frozen reservoir (PFSP frontier weighting, already proven) +
   periodic dedicated exploiters trained vs the frozen main. Exploiter win-rate is a first-class
   health metric, not an afterthought.
2. Equilibrium regularization escalation, cheapest first, each step evidence-gated on observed
   cycling: entropy floor → MMD (Sokota et al.) → R-NaD (DeepNash) only if cycling persists.
3. **Belief head with exact generator labels** (our unique lever): predict opponent
   sets/items/abilities/spreads from public history; posterior feeds the policy; ground truth
   never does. Ablate to prove no leakage.
4. Sim2real certification cadence: every N updates, same policy on real PS vs same baselines;
   delta beyond noise ⇒ engine gap ⇒ cosim hunt before continuing.
- **Gate M2 (SOTA-machines = S1):** ≥65% vs foul-play-full; league Elo monotone vs frozen pool;
  exploiter <65% vs current main.

### P4 — Optional accelerants (parallel, pull in only if P2/P3 stall)
- foul-play imitation trajectories *generated on our fast engine* (nearly free) as an init or
  kickstart-KL anchor — the BC+value-pretrain machinery already exists and works.
- Offline RL / filtered BC on human replay corpora (Metamon's parsed datasets, PokéChamp's gen9
  set). Bigger lift; only if the from-scratch line plateaus below M2.
- The project identity is from-scratch self-play; warm starts are compute discounts, not
  dependencies, and results get reported both ways.

### P5 — Ladder campaign + superhuman certification (S2 + S3; ~3–4 weeks)
1. Eval-time search: determinized IS-MCTS on the Rust engine using learned policy/value/belief
   priors. Measure as a pure Elo multiplier vs the raw policy. Latency budget: ladder timer.
2. Ladder protocol: registered bot account, rate-limited, ≥500 games; report Elo/GXE with CIs;
   snapshot the live leaderboard for the human reference at claim time.
3. Exploiter audit at full compute vs the frozen final agent (S3).
4. Human showmatches (top-ladder volunteers) as the qualitative capstone — not the certification
   basis (n too small), but the thing that makes the claim legible.
- **Gate M3 (superhuman = S2+S3):** sustained high-80s+ GXE above contemporaneous top human
  accounts, exploiter <60%.

### P6 — Stretch: OU + teambuilding
- OU with curated team set + team preview head; set inference shifts to Smogon-usage priors.
- Teambuilding as PSRO/double-oracle over team space. Explicitly out of scope until S2.

## 4. Hardest problems (ranked; updated with what we've learned)

1. **Hidden information** — set inference *is* the skill ceiling of randombattle. Mitigation:
   belief head with exact known-generator labels (unique asset), recurrence, and search that
   consumes the belief.
2. **Skill extinction / punctuated equilibrium** — skills emerge, get pruned, re-emerge; plateaus
   hide phase transitions in both directions. Mitigation: behavior probes along every run,
   powered evals only, never certify during league hardening. This is now a measured phenomenon,
   not a hypothesis, and it invalidates naive early stopping at every phase.
3. **Self-play nonstationarity/cycling** under simultaneous moves. League + escalating
   regularization + fixed external eval suite as ground truth.
4. **Simulator fidelity under adversarial search** — RL will find any divergence and farm it.
   On-policy cosim in CI is mandatory before every big run (top engineering risk, owned).
5. **Return variance** from damage rolls/crits/secondaries: hard floor on value error, high eval
   cost. Huge cheap batches, KL-guarded updates, watch explained variance.
6. **Long-horizon credit** (hazards pay off 30+ turns later; still unlearned at 27M steps —
   the cleanest open behavioral failure). γ 0.995–0.999 territory at scale; candidate probe
   for the identity/belief tiers.
7. **Evaluation cost at the top** — ladder Elo CI is brutal (~±50 at 200 games) and the S2 claim
   needs sustained numbers. Budget eval compute like training compute.

## 5. Design decisions (updated)

| Decision | Choice | Status/why |
|---|---|---|
| Training sim | Rust engine; real PS for certification | 100–1000× samples; gated on P1 parity+MDP fixes |
| Format | gen9randombattle | Unchanged |
| Base recipe | v7 obs + setslot + frames 2 + frontier PFSP, const hparams | Measured; see EXPERIMENTS.md; retired levers stay retired |
| Net ladder | setslot 280K → +ID embeddings → +GRU → maybe transformer | Capacity only on demonstrated plateau |
| League | PFSP reservoir + exploiters | Frontier PFSP proven; exploiters new at P3 |
| Equilibrium | entropy floor → MMD → R-NaD | Escalate on observed cycling only |
| Hidden info | Belief head, exact generator labels, aux-only firewall | Unique asset |
| Reward | ΔΦ (0.5·HPdiff + 1.5·faints) + terminal | Φ-extras measured redundant post-features |
| Warm start | None by default; foul-play imitation as fallback | From-scratch is the line |
| Search | Eval-time IS-MCTS only; none at train time | Elo multiplier for S2; GPU efficiency |

## 6. Compute plan

- At 20–50k steps/s: 1B steps ≈ 6–14 h/box. P2 ≈ 2–6 GPU-days per scale run + ~4×100M-step
  attribution probes. P3 league ≈ 2–4 GPU-weeks (population overhead ~3×). P5 eval/search ≈
  GPU-days. **Total: well under one A100-month** — the entire premise of owning the fast engine.
- Synchronous vectorized PPO, 1k–8k envs, bf16 + torch.compile, single box until profiling
  proves otherwise. Eval farms on CPU in parallel.
- Hard gates between phases; capacity/complexity increases require a plateaued gate (v1 rule,
  kept — it's what kept the recipe honest).

## 7. Evaluation protocol (fixed, versioned)

1. Seeded powered suite (≥300 eps, 95% CIs) vs {random, maxbp, SimpleHeuristics, foul-play-fast,
   foul-play-full} at every certification point. In-run small-n readings are a ±0.10 instrument
   and are never quoted (night4 lesson).
2. Behavior rubric (EVAL_RUBRIC.md) probed *along* runs: setup rate/waste/conversion, hazards,
   recovery, status redundancy, tera timing — the extinction detector.
3. All-checkpoint Elo pool (cycling detector) + exploiter win-rate trend.
4. Sim2real delta on real PS at fixed cadence.
5. Ladder: registered account, throttled, ≥500 games, GXE with CIs, leaderboard snapshot.

## 8. Risks

- **Engine divergence farmed by RL** → on-policy cosim in CI; no big run without a green gate.
- **The bet on scale fails** (P2 flat at 1B steps) → the gap is representational; pull identity/
  belief/GRU forward; foul-play imitation as kickstart. P0's foul-play calibration tells us early.
- **League collapse/cycling** → reservoir + exploiters + external suite; escalate regularization.
- **Belief-label leakage** → aux-only schema, leakage ablation before any S-claim.
- **Sim2real drift** (Rust-trained policy underperforms on real PS) → P1.5 checkpoint-parity
  gate + certification cadence; any unexplained delta halts scaling.
- **Ladder ToS/etiquette** → registered bot, rate limits, no smurfing; coordinate if needed.
- **Ops attrition** (the silent killer so far: reaped background runs, lid-close throttling,
  OOMs) → rented box, detached launches, watchdog scripts, wandb alerts.

## 9. Reading list

(v1 list stands — Metamon, PokéChamp, PokeLLMon, foul-play, DeepNash/R-NaD, MMD, NFSP/PSRO,
AlphaStar, Spinning Tops, ReBeL, PPO practice, EnvPool/Sample Factory/Podracer, IQL. Read
Metamon first; mine foul-play's set-inference design for the belief head.)

## 10. Immediate next actions (updated 2026-07-11)

1. ~~P0.1 foul-play calibration~~ — measured ≈0.15 at 100ms (full arm finishing); branch fired.
2. ~~P1.1 parity + exact distributions~~ — done (Codex); on-policy cosim deferred to P2 gate.
3. **P1.4: throughput** — batched obs encoding in Rust, batched pybridge, ≥20k steps/s bench.
4. **P1.2: decision-point refactor** (tera/pivot/replacement in the action space) — now unblocked
   since Codex's campaign is closed.
5. Identity tier design doc: generator-prior features (P(move)/P(item)/P(ability) per species
   from the pinned randombattle data) as obs features — the no-learning-cost half of RQ2 —
   plus revealed-attribute obs gaps (ability/item/weight).
6. Engine gate hardening (small): fail on unsupported>0; mutation re-run + probability mutants.
7. P0.2 ladder probe when Rob registers the bot account.
