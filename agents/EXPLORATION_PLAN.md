# Exploration Plan — de-risking alternatives to pure self-play PPO

Companion to `RESEARCH_PLAN.md` / `EXPERIMENTS.md`; follows their decision principles (fixed
budgets, powered evals ≥300 games, kill criteria declared before launch, single-variable
attribution). Scoped to THIS box: 8 EPYC vCPUs + A100-80GB (96% idle), Rust env at ~17k sps.

**The economics that shape everything here:** a 10M-step PPO probe is ~10 minutes of wall clock.
Env steps (CPU) are the scarce resource; model compute (GPU) is effectively free. So experiments
that spend GPU (BC, value nets, average-policy nets, search over learned values) are cheap, and
every probe is budgeted in env-steps with a decision criterion that ends it. No open-ended runs:
each probe stops the moment its go/no-go resolves.

**Run discipline.** `runs/scale2` (league PPO, the control) keeps running BETWEEN probe windows
and is paused (checkpointed, ~10s) DURING them — probes get all 8 cores. Its curve doubles as
the from-scratch control for E1/E3/E4. Watch its entropy (0.56 and declining at 125M steps): if
`eval/heuristic` flatlines two evals in a row while entropy keeps dropping, stop it for good and
bank the checkpoint — that is also the moment the box becomes free for full-time exploration.

---

## P0 — shared instrumentation (build first; everything below consumes it)

### P0a. Exploiter probe — the universal "how far from Nash" metric
True exploitability is uncomputable here; the standard proxy is a best-response train: fix a
target policy, train a fresh PPO exploiter against ONLY it for 10M steps, report the exploiter's
final win-rate (and AUC of its learning curve — a target that is exploited *slowly* is still
better than one exploited fast).
- Implementation: `train_flow --exploit <ckpt.pt>` mode — league replaced by the single frozen
  target. ~Half a day; reuses everything.
- Cost per probe: ~12 min. Calibrate immediately on: (a) the random-init anchor (should be
  exploited to ~1.0 — sanity), (b) the current scale2 checkpoint (first real number; pure
  self-play PPO is expected to be highly exploitable — this number is the baseline every Nash
  method must beat).

### P0b. MCTS reference on the eval ladder
`baselines.MctsBaseline` (pmariglia's poke-engine, perfect-information MCTS) already has an
adapter but the module isn't installed. `uv pip install poke-engine`, wire `mcts@100ms` into
`standard_baselines` behind a flag (sequential + heavy — eval windows only, ~64 envs).
- Gives the ladder a strong absolute reference between `heuristic` and foul-play (which lives on
  the other machine). Also the candidate BC teacher for E5.
- Cost: ~1h including a 300-game calibration eval of the current checkpoint.

---

## E1 — Behavior cloning warm start (heuristic teacher) + kickstarting
**Hypothesis:** scale2 spent ~100M steps reaching 0.24 vs heuristic from scratch. Cloning the
(scripted, Rust, free) heuristic first should start near mirror-parity (~0.5) and fine-tune
strictly faster — and if it does, the same pipeline upgrades to stronger teachers (MCTS, human
replays) with no new machinery.

- **Data:** 2M decision points of heuristic-vs-heuristic via `FlowVec` + `heuristic_actions_all`
  (minutes of CPU). Store (obs, ids, mask, action).
- **BC train:** cross-entropy on the A100, same `ActorCritic` trunk (value head trained on
  discounted outcome). Minutes per epoch; early-stop on held-out action accuracy.
- **Probes (10M steps each, same seed/config as scale2's recipe):**
  1. control — from-scratch (already have scale2's curve over this range),
  2. BC-init PPO,
  3. scratch + kickstart: auxiliary KL(π‖π_heuristic) with coefficient annealed to 0 by 5M —
     the "distill while learning" alternative that skips the separate BC phase.
- **Signal:** `eval/heuristic` at 10M + slope; BC-net standalone eval before any PPO.
- **Go:** BC-init or kickstart ≥ control + 10pts at 10M → adopt for the next long run; open E5.
- **Kill:** neither beats control at 10M (BC policies often collapse under PPO gradient noise —
  if so, try value-head-only init before declaring dead; one extra 10M probe).
- **Cost:** ~1 day implementation, ~45 min total runtime.

## E2 — Value-guided 1-ply expectiminimax at inference (the ReBeL de-risk)
**Hypothesis:** foul-play (expectiminimax + set inference) beat night4 0.9-to-0.1 — search
dominates this domain. The cheapest possible test of whether OUR nets can power search: no
training at all, wrap the existing checkpoint in 1-ply search at eval time.

- At each Turn request: for each legal joint action pair (≤13×13, mask-reduced), advance a
  cloned engine state (sampled outcome, fixed seed per pair), evaluate V(s′) with the current
  value head (batched on the A100 — 169 states is one forward), solve the resulting matrix game
  (maximin LP or ~100 iters of regret matching; trivial at 13×13), play the mixed strategy.
- Needs one bridge addition: clone-env + step-pair + read-successor (the `Battle` API has most
  of it; ~a day).
- **Evals:** search-wrapped vs raw policy head-to-head, and both vs heuristic (300 games each).
- **Go:** search ≥ raw + 10pts vs heuristic → the value net already supports search; this
  unlocks the whole program — search-augmented training (AlphaZero-style targets), deeper
  ply, and eventually belief-state search (ReBeL proper, which needs the set-inference work
  and is NOT in scope until this gate passes).
- **Kill:** Δ ≤ 0 → value net is the bottleneck, not the policy; redirect effort to value
  quality (the `--aux` heads exist and are off) before any search investment.
- **Cost:** ~1 day implementation, ~1h runtime. Zero training.

## E3 — NFSP-lite (average-policy net alongside the league)
**Hypothesis:** fictitious self-play's average policy converges toward Nash where the
best-response chain just cycles. We approximate NFSP with minimal new machinery: keep the league
PPO as the best-response process; add a reservoir buffer of the learner's own (state, action)
pairs and continuously train an average-policy net on the idle GPU.

- **Signal (via P0a):** exploiter probe against the average net vs against the PPO net at the
  same total budget (30M steps). Secondary: `eval/heuristic` of the average net (must not crater
  >10pts below the BR net — an unexploitable but weak policy is not the goal).
- **Go:** average net meaningfully less exploitable at comparable heuristic-wr → NFSP graduates
  to a longer run and the average net becomes the league's `self` opponent (full NFSP).
- **Kill:** no exploitability gap at 30M → park; the reservoir/SL code is reusable for E5.
- **Cost:** ~1 day implementation, ~2h runtime including two exploiter probes.

## E4 — R-NaD-style regularized PPO (the DeepNash trick at probe scale)
**Hypothesis:** DeepNash's core mechanism — reward-transform regularization toward a slowly
moving reference policy — converts the self-play cycle into a convergent dynamic, and it is the
cheapest Nash-flavored change to the existing trainer (a reward term, not a new algorithm).

- r′ = r − η·(log π(a|s) − log π_ref(a|s)) on acting steps, π_ref = frozen copy refreshed every
  N updates (the "regularization then iterate" outer loop). η ∈ {0.05, 0.2}, N fixed.
- **Probes:** 2 × 10M steps + exploiter probe on each endpoint vs the scale2-checkpoint control.
- **Go:** exploiter wr down ≥ 10pts at equal heuristic-wr → adopt; combines freely with E1/E3.
- **Kill:** heuristic-wr regresses > 5pts at both η → the regularizer is fighting learning at
  this scale; retry only after a BC warm start (interaction with E1).
- **Cost:** ~0.5–1 day implementation, ~40 min runtime + probes.

## E5 — Human-replay BC (conditional; the big prize behind E1)
Public gen9randombattle replays are downloadable; our protocol/parity machinery
(`cosim`, fixtures) already parses PS logs into engine states, and our observation is
public-info by construction — so cloning the VISIBLE player's choices sidesteps most
hidden-info reconstruction. Real scope (~2–4 days): scraper, protocol→decision-point converter,
dedup/quality filters. **Gate:** only starts if E1 shows warm-starts transfer to PPO, or E2
shows value quality is the bottleneck (replay outcomes give a grounded value signal).

---

## Sequencing & total budget

| window | contents | new code | runtime |
|---|---|---|---|
| 1 | P0a exploiter + P0b MCTS ladder + calibrations | 0.5–1d | ~1.5h |
| 2 | E1 BC/kickstart (3 probes) | 0.5–1d | ~45min |
| 3 | E2 value-search wrapper + evals | ~1d | ~1h |
| 4 | E4 R-NaD probes, then E3 NFSP-lite | 1.5–2d | ~3h |
| 5 | decision review → E5 and/or graduate winners into one long run | — | — |

Total incremental training compute across every probe: **< 120M env steps ≈ ~2.5h** of box
time. Wall clock is dominated by implementation, not runs — by design. Each window ends with a
written go/kill in `EXPERIMENTS.md` (append rows there; this file only defines the program).

**What graduates:** the end state is ONE next long run combining every surviving lever (e.g.
BC-init + R-NaD regularization + aux value heads, evaluated with the search wrapper) — not a
fleet of parallel long runs. Anything that didn't clear its gate gets a one-line epitaph in
`EXPERIMENTS.md` and its code left behind a flag.
