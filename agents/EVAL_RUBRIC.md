# Model Quality Rubric — Long-Horizon Behaviors

Win-rate alone cannot distinguish "spams the best damage move" from "invests a turn now to win
later." This rubric tracks the **delayed-payoff behaviors** that indicate real long-term credit
assignment. Use it when manually judging a checkpoint — no fixed cadence.

**Tools**
- Quantitative: `python -m ppo.behavior_probe <ckpt> --opponent maxbp --games 30 --out probes/<name>.json`
  (each metric below names its probe field; needs a running Showdown server)
- Qualitative: `python -m ppo.play --mode vs --opponent <opp> --games 3 --verbose --log-dir games/`
  then read the prettified `games/*.txt` per the review protocol at the bottom.
- Probe vs **maxbp** by default: it punishes passivity but doesn't switch, so setup opportunities
  exist and payoffs are attributable. Probe vs **heuristic** for switch-quality questions.

## Quantitative rows (probe fields)

| # | Behavior | Probe field | Healthy signs | Red flags |
|---|----------|-------------|---------------|-----------|
| 1 | **Uses setup at all** | `setup.uses_per_game`, `setup.games_with_setup_frac` | >0 and rising across checkpoints; used in games where its mons carry setup moves (~2-4/game available in randbats) | 0.0 everywhere (pure greedy damage — no long-horizon learning); or >4/game (setup spam) |
| 2 | **Sets up safely** | `setup.safe_use_frac` (own HP ≥50% at use) | ≥0.7 | ≤0.4 — boosting while dying (the old Defog-spam failure class) |
| 3 | **Converts boosts** | `setup.boosted_kos_per_game`, `setup.wasted_frac`, `setup.post_setup_dmg_per_decision` vs `setup.baseline_dmg_per_attack` | wasted ≤0.3; post-setup damage clearly > baseline (boosts translate to damage/KOs) | wasted ≥0.5 (boosts then switches/faints); post-setup ≈ baseline (boosting changes nothing) |
| 4 | **Boost magnitude** | `setup.avg_max_boost` | ≥2 in a meaningful share of games (+2 then sweep) | always 0-1 (never commits to a boost line) |
| 5 | **Hazard investment** | `hazards.games_with_hazards_frac`, `hazards.avg_first_hazard_turn` | set early (turn ≤5) when carried; frac tracks availability | never sets; or sets late-game (wasted tempo); or re-stacks pointlessly |
| 6 | **Status with purpose** | `status.uses_per_game`, `status.redundant_frac` | redundant ≤0.1 (never re-statuses a statused foe) | redundant ≥0.25 — statusing into immunity/duplication = not reading foe state |
| 7 | **Recovery timing** | `recovery.uses_per_game`, `recovery.well_timed_frac` (used at 20–70% HP) | well-timed ≥0.6 | recovery at >80% HP (wasted turns) or never recovering when carried |
| 8 | **Decision mix** | `decision_mix` | attack-dominant with minority setup/status/switch; context-dependent | `other_status` spam (the Defog pathology); voluntary switch >30% (dithering) |
| 9 | **Game closure** | `kos_per_game`, `avg_turns`, `win_rate` | turns fall / KOs concentrate as quality rises | long games vs weak opponents (can't close) |

Notes on interpretation:
- Random battles: not every team carries setup/hazards/recovery, so per-game rates are capped by
  availability (~60-80% of teams have some setup move). Compare **across checkpoints**, not to 1.0.
- A rising `setup.uses_per_game` with LOW `wasted_frac` is the single clearest signal of
  long-horizon credit assignment. Usage alone is not — reward-hacked setup spam looks identical
  in row 1 and opposite in row 3.

## Reference readings

| checkpoint | opponent | win_rate | setup/game | safe | wasted | boosted KOs | hazards | ref |
|---|---|---|---|---|---|---|---|---|
| slot2/model_139264 (cold-start vs random) | maxbp (n=10) | 0.90 | **0.00** | — | — | 0.1 | **0.00** | 2026-07-03 probe |
| long-med final (1.5M mixed-PFSP, v2 obs) | maxbp (n=30) | 0.87 | 0.03 | 1.0 | 0.0 | 0.17 | 0.03 | probes/longmed_vs_maxbp.json |

- long-med: first NONZERO setup/hazard behavior (0.03 setup/game, all safe, none wasted; hazards
  3% of games at turn 3) — a skill at the noise floor after mixed training, vs the hard 0.00
  cold-start baseline. Status/recovery still 0. Watch whether v3-obs training steps this up.

| long-v3 @2.87M (v3 obs, mixed-PFSP) | maxbp (n=30) | 0.80 | 0.03 | 1.0* | 1.0* | 0.2 | 0.03 (turn 50) | probes/longv3_mid_vs_maxbp.json |
| it-B (22min scratch, reward v2, slot) | maxbp (n=30) | 0.97 | **0.00** | — | — | 0.03 | 0.07 (turn 12) | probes/itB_vs_maxbp.json |
| it-C (22min scratch, reward v2 + setslot) | maxbp (n=30) | 0.90 | **0.33** | 0.90 | 1.0† | **0.27** | 0.00 | probes/itC_vs_maxbp.json |

- **Interaction effect (the key finding so far):** reward credit for boosts alone (it-B) produced ZERO
  setup; reward + cross-move set-context (it-C) produced 0.33/game, 90% safely timed, 0.27 boosted
  KOs/game — the first deliberate setup play of the project, from scratch in 22 min. Neither lever
  worked alone. †wasted_frac 1.0 is n=10 events under a strict 3-decision window — conversion is
  immature (and the metric may under-credit late follow-through); watch this in longer runs.

- long-v3 mid-run: the +20pt heuristic-eval step came from MATCHUP play (switch% 22->27, better
  targets) — NOT from long-horizon skills, which remain at the noise floor (*single setup event,
  wasted; hazards set at turn 50 = filler; status 50% redundant; recovery 0). `other_status`
  crept 0 -> 3.4% of decisions: exploration toward non-damaging moves without payoff yet — watch
  it (this is the Defog-class bucket). Conclusion: obs features enable reactive skill; investment
  skills likely need an exploration/curriculum lever, not more of the same steps.

- slot2 reading confirms the expected anchor: a pure greedy-damage specialist — zero setup,
  hazards, status, or recovery use; 77% attack / 23% switch. Wins anyway vs maxbp. Any nonzero,
  well-converted setup emerging in mixed-PFSP checkpoints is genuine long-horizon progress.
- Nuance: `avg_max_boost` can be >0 with zero setup uses — passive boosts from move secondaries /
  abilities (slot2: 1.1). Judge deliberate setup by `uses_per_game` + conversion, not raw boosts.

Known history for anchoring:
- **Flat-MLP from-scratch (obs2-random, 0.83 vs random):** 41-44% status-move decisions incl.
  6-turn Defog spam while dying; 60-69% suboptimal move choice; 148-177 missed KOs/15 games.
  The anti-pattern baseline.
- **Slot-arch cold-start (slot1/slot2, ~0.99 vs random):** 81% damage moves, 0 missed KOs,
  5% suboptimal — but setup behavior NOT yet measured; presumed near-zero (greedy-damage
  specialist trained vs random, where setup is unnecessary).

## Qualitative review protocol (agent or human)

Sample ≥3 prettified logs (`--log-dir` + the auto-generated `.txt`), ideally 1 win + 1 loss.
Score each dimension 1-5 (anchors: 1 = the red flag; 3 = mixed; 5 = the healthy sign):

1. **Setup judgment** — boosts when safe AND the boost enables something (a KO threshold it
   didn't previously make)? Or boosts into an obvious counter / while getting chipped out?
2. **Tempo** — any wasted turns (redundant hazards, recovery at full HP, status on statused)?
3. **Switch purpose** — do voluntary switches respond to type disadvantage or preserve a win
   condition, or are they dithering? (Watch for switch loops.)
4. **Endgame closure** — with a winning position, does it take the fastest line? Does it
   preserve its win condition mon when ahead?
5. **Loss autopsy** — in losses: was it outplayed (fine), variance (fine), or did it throw a won
   position (record the pattern verbatim in the notes below)?

Record: date, checkpoint, scores, 1-3 verbatim game moments (turn numbers), and any new
pattern worth adding as a rubric row.

### Review notes log
- _(append entries here)_
