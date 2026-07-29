"""Long-horizon PPO self-play on the **Rust decision-point engine** (`FlowVec`).

The trainer for scale runs: real Showdown rules (faint replacements, pivot landings, Tera),
13-action space, teams drawn from the pinned PS random-battle generator pool, and true
checkpoint/resume so a multi-day run survives restarts.

    python -m ppo.train_flow --num-envs 512 --total-steps 2000000000 --run-dir runs/scale1

Resume after any interruption (weights, optimizer, opponent snapshot, counters all restored):

    python -m ppo.train_flow --resume runs/scale1

Differences from `selfplay.py` (which drives the legacy whole-turn `Battle` bridge MDP):
  * `flow_env.FlowEnvVec` instead of `engine_env.EngineVecEnv` — see that module's docstring
    for why the bridge MDP is disqualifying, and for the `active` (acting-side) mask.
  * a single `--run-dir` that is resumed in place, rather than a fresh timestamped dir per launch.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import signal
import time
from pathlib import Path

import numpy as np
import torch

from .buffer import RolloutBuffer
from .config import PPOConfig
from .flow_env import FlowEnvVec
from .flow_eval import HEURISTIC_STATS, evaluate_flow, make_scripted_heuristic, standard_baselines
from .league import OpponentSlots, SnapshotLeague
from .model import ActorCritic
from .run_logger import RunLogger
from .train import ppo_update, resolve_device, set_seed

DEFAULT_POOL = "../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz"

# Set by the SIGTERM/SIGINT handler so the loop can finish the update it is in and checkpoint
# before exiting — a kill mid-update otherwise loses up to one rollout.
_STOP = False


def _install_signal_handlers():
    def handler(signum, _frame):
        global _STOP
        _STOP = True
        print(f"\n[train_flow] signal {signum} — finishing the current update, then checkpointing")

    signal.signal(signal.SIGTERM, handler)
    signal.signal(signal.SIGINT, handler)


@torch.no_grad()
def greedy_actions(net, obs, ids, mask, device):
    """Argmax over legal actions (the opponent/eval policy)."""
    logits, _ = net.forward(
        torch.as_tensor(obs, device=device),
        torch.as_tensor(mask, device=device),
        obs_ids=torch.as_tensor(ids, device=device),
    )
    return logits.argmax(dim=-1).cpu().numpy()


def build_model(cfg, env, device, aux: bool):
    embed = {"n_mons": env.n_mons, "cols": env.id_columns, "vocab": env.vocab, "dim": cfg.embed_dim}
    return ActorCritic(env.obs_dim, env.n_actions, cfg.hidden_dim, cfg.n_hidden_layers,
                       embed=embed, aux=aux,
                       outcome=getattr(cfg, "outcome_head", False),
                       belief=getattr(cfg, "belief_head", False),
                       setslot=getattr(cfg, "setslot", False)).to(device)


def train(args):
    cfg = PPOConfig(
        num_envs=args.num_envs, rollout_steps=args.rollout_steps, total_steps=args.total_steps,
        lr=args.lr, gamma=args.gamma, gae_lambda=args.gae_lambda, clip_eps=args.clip_eps,
        entropy_coef=args.entropy_coef, value_coef=args.value_coef,
        update_epochs=args.update_epochs, minibatch_size=args.minibatch_size,
        hidden_dim=args.hidden_dim, n_hidden_layers=args.n_hidden_layers,
        embed_dim=args.embed_dim, seed=args.seed, device=args.device or "auto",
        shaping_coef=args.shaping_coef, aux=args.aux, target_kl=args.target_kl,
        outcome_head=args.outcome_head, belief_head=args.belief_head,
        setslot=args.setslot,
    )
    set_seed(cfg.seed)
    device = resolve_device(cfg.device)
    # TF32 matmuls: measured 2× on ppo_update at this run's exact shapes (2.61s -> 1.31s per
    # 131k-sample update on the A100) with fp32 accumulation — the standard Ampere trade.
    torch.set_float32_matmul_precision("high")
    run_dir = Path(args.run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    state_path = run_dir / "training_state.pt"

    pool = args.team_pool
    if pool and not os.path.isabs(pool):
        pool = str((Path(__file__).resolve().parents[1] / pool).resolve())
    if pool and not os.path.exists(pool):
        raise SystemExit(f"team pool not found: {pool} (run harness/gen-team-pool.mjs, or pass --team-pool '')")

    env = FlowEnvVec(cfg.num_envs, seed=cfg.seed, team_pool=pool or None,
                     max_requests=args.max_requests, shaping_coef=cfg.shaping_coef, gamma=cfg.gamma,
                     fog_species=args.fog_species, obs_version=args.obs_version,
                     frames=args.frames)
    if env.pool_size == 0:
        print("[train_flow] WARNING: no team pool loaded — every env replays the same fixed "
              "debug matchup. This is not a real training distribution.")

    # Peek at the saved state BEFORE building the model: architecture belongs to the run, not to
    # whatever flags this relaunch happened to pass — resuming a 1024-wide run with the default
    # --hidden-dim 256 must not die on a state_dict shape mismatch.
    ck = None
    if args.resume and state_path.exists():
        ck = torch.load(state_path, map_location=device, weights_only=False)
        for k in ("hidden_dim", "n_hidden_layers", "embed_dim"):
            if k in ck and getattr(cfg, k) != ck[k]:
                print(f"[train_flow] resume: {k} {getattr(cfg, k)} -> {ck[k]} (from checkpoint)")
                setattr(cfg, k, ck[k])

    model = build_model(cfg, env, device, aux=cfg.aux)
    # Evaluators and the search core read these off the net (load_ckpt sets them for loaded
    # checkpoints; the live training model needs them too, e.g. for search distillation).
    model.frames = args.frames
    model.obs_version = args.obs_version
    model.fog_species = args.fog_species
    # fused=True: the per-minibatch Adam step was 23% of trainer wall time (py-spy, post-TF32) —
    # 32 unfused step() calls per update over 10M params is kernel-launch soup. The fused CUDA
    # kernel shares the same state-dict format, so resume is unaffected.
    fused = device.type == "cuda"
    opt = torch.optim.Adam(model.parameters(), lr=cfg.lr, eps=1e-5, fused=fused)

    # League, not a single frozen self. Training only against the most recent snapshot lets the
    # pair co-adapt and forget everything it stopped seeing; the reservoir keeps past checkpoints
    # in the mix, PFSP-weighted (see ppo/league.py).
    # Scripted league members beyond `random`. The heuristic is the important one: the eval
    # opponent must also be a TRAINING opponent, or win-rate against it is pure transfer from
    # self-play (scale1: 0.18 at 26M steps; the poke-env recipe, which trained on it at base
    # weight 2, hit 0.46 in its first 22 minutes).
    scripted_opps: dict = {}
    scripted_weights: dict[str, float] = {}
    heur_stats: dict = {}
    if args.league_heuristic_weight > 0:
        scripted_opps["heuristic"] = make_scripted_heuristic(heur_stats)
        scripted_weights["heuristic"] = args.league_heuristic_weight
    league = SnapshotLeague(run_dir / "pool", keep=args.pool_size, pfsp_power=args.pfsp_power,
                            mode=args.pfsp_mode, random_weight=args.random_weight,
                            scripted_weights=scripted_weights)
    slots = OpponentSlots(args.opponent_slots, lambda: build_model(cfg, env, device, aux=cfg.aux),
                          device, scripted=scripted_opps)

    # --exploit: best-response probe (EXPLORATION_PLAN P0a). The league is replaced by ONE frozen
    # target policy; the learner's win-rate curve against it IS the exploitability proxy — a
    # target that gets exploited fast/high is far from equilibrium.
    exploit_net = None
    if args.exploit:
        eck = torch.load(args.exploit, map_location=device, weights_only=False)
        if not (isinstance(eck, dict) and "model" in eck):
            eck = {"model": eck}  # bare state_dict (e.g. anchor.pt); dims come from the CLI cfg
        embed = {"n_mons": env.n_mons, "cols": env.id_columns, "vocab": env.vocab,
                 "dim": eck.get("embed_dim", cfg.embed_dim)}
        exploit_net = ActorCritic(env.obs_dim, env.n_actions,
                                  eck.get("hidden_dim", cfg.hidden_dim),
                                  eck.get("n_hidden_layers", cfg.n_hidden_layers),
                                  embed=embed, aux=False).to(device)
        exploit_net.load_state_dict(eck["model"])
        exploit_net.eval()
        for prm in exploit_net.parameters():
            prm.requires_grad_(False)
        print(f"[exploit] target: {args.exploit} (step {eck.get('global_step', '?')})")

    # E4 (R-NaD-lite): regularize the reward toward a slowly-refreshed reference policy —
    # r' = r − η·(log π(a|s) − log π_ref(a|s)) on the learner's acting steps. DeepNash's core
    # convergence trick, applied to the learner side only (the league plays the other seat).
    rnad_ref = None
    if args.rnad_eta > 0:
        rnad_ref = build_model(cfg, env, device, aux=False)
        rnad_ref.eval()
        for prm in rnad_ref.parameters():
            prm.requires_grad_(False)

    # The anchor is the run's random-init policy, frozen before any training — the fixed reference
    # that says whether the agent has learned anything at all.
    anchor = build_model(cfg, env, device, aux=False)
    anchor_path = run_dir / "anchor.pt"

    # --init: warm-start weights (e.g. a BC checkpoint) into an otherwise-fresh run. Distinct
    # from --resume: no optimizer/counters/league state, and a resume takes precedence.
    if args.init and ck is None:
        ick = torch.load(args.init, map_location=device, weights_only=False)
        model.load_state_dict(ick["model"] if "model" in ick else ick)
        print(f"[train_flow] initialized weights from {args.init}")

    global_step, update, total_games = 0, 0, 0
    if ck is not None:
        model.load_state_dict(ck["model"])
        opt.load_state_dict(ck["opt"])
        global_step, update, total_games = ck["global_step"], ck["update"], ck["total_games"]
        league.load_state(ck.get("league", {}))
        print(f"[train_flow] resumed {state_path} @ update {update}, step {global_step:,}")
    elif args.resume:
        print(f"[train_flow] --resume: no {state_path} yet, starting fresh")

    # The anchor must be the SAME weights for the life of the run, or the "absolute" curve is
    # measured against a moving reference. Written once, reloaded on every resume.
    if anchor_path.exists():
        anchor.load_state_dict(torch.load(anchor_path, map_location=device, weights_only=True))
    else:
        torch.save(anchor.state_dict(), anchor_path)
    anchor.eval()
    for prm in anchor.parameters():
        prm.requires_grad_(False)
    # Seed the reservoir so the first updates have something to play that is not just `random`.
    if exploit_net is None and not league.snapshots():
        league.add(model, global_step)

    buffer = RolloutBuffer(cfg.rollout_steps, cfg.num_envs, env.obs_dim, env.n_actions, device,
                           id_dim=env.id_dim, aux=cfg.aux,
                           outcome=getattr(cfg, "outcome_head", False),
                           belief=getattr(cfg, "belief_head", False))
    logger = RunLogger(str(run_dir.parent), run_dir.name,
                       wandb_project=args.wandb_project if args.wandb else None)
    logger.config({"cfg": vars(cfg), "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                   "pool_size": env.pool_size, "params": model.num_params(),
                   "device": str(device), "env": "FlowEnvVec", "run_dir": str(run_dir)})

    batch = cfg.num_envs * cfg.rollout_steps
    indefinite = cfg.total_steps <= 0
    print(f"device={device}  params={model.num_params():,}  obs_dim={env.obs_dim}  "
          f"actions={env.n_actions}  batch={batch}  pool={env.pool_size}  "
          f"target={'INDEFINITE' if indefinite else f'{cfg.total_steps:,} steps'}")

    def save(tag: str | int):
        torch.save({"model": model.state_dict(), "opt": opt.state_dict(),
                    "league": league.save_state(), "global_step": global_step,
                    "update": update, "total_games": total_games,
                    "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                    "hidden_dim": cfg.hidden_dim, "n_hidden_layers": cfg.n_hidden_layers,
                    "embed_dim": cfg.embed_dim, "fog_species": args.fog_species,
                    "belief_head": args.belief_head, "obs_version": args.obs_version,
                    "frames": args.frames},
                   state_path)
        # A separate, weights-only artifact per checkpoint — this is what the on-policy cosim
        # sidecar and offline evals load, and it must never be a half-written training_state.
        ckpt = run_dir / f"ckpt_{int(global_step):012d}.pt"
        torch.save({"model": model.state_dict(), "global_step": global_step, "update": update,
                    "obs_dim": env.obs_dim, "n_actions": env.n_actions,
                    "hidden_dim": cfg.hidden_dim, "n_hidden_layers": cfg.n_hidden_layers,
                    "embed_dim": cfg.embed_dim, "fog_species": args.fog_species,
                    "belief_head": args.belief_head, "obs_version": args.obs_version,
                    "frames": args.frames}, ckpt)
        if args.keep_checkpoints > 0:
            for old in sorted(glob.glob(str(run_dir / "ckpt_*.pt")))[:-args.keep_checkpoints]:
                os.remove(old)
        (run_dir / "LATEST").write_text(ckpt.name + "\n")

    baselines = standard_baselines(anchor, device, pool or None) if args.eval_every else []

    # E3 (NFSP-lite): reservoir-sample the learner's own acting decisions across the whole run —
    # a uniform draw over the policy's history, which is exactly what the fictitious-play
    # "average policy" is fit on afterward (probes/bc_train.py). Approximate algorithm-R done in
    # batches: accept ~cap/seen of each step's rows into random slots.
    res = None
    if args.reservoir_out:
        res = {"obs": None, "ids": None, "mask": None, "action": None,
               "n": 0, "seen": 0, "cap": args.reservoir_cap,
               "rng": np.random.default_rng(cfg.seed ^ 0x9E5)}

    window: list[int] = []
    start, start_step = time.time(), global_step

    if rnad_ref is not None:
        rnad_ref.load_state_dict(model.state_dict())  # ref starts AT the (possibly resumed) policy

    opp_rng = np.random.default_rng(cfg.seed ^ 0xBEEF)
    while not _STOP and (indefinite or global_step < cfg.total_steps):
        update += 1
        if args.lr_anneal_horizon > 0:
            # Wang 2024's schedule, the single biggest lever in that work (+25pts vs constant):
            # lr(x) = lr0 / (8x + 1)^1.5, x = progress in [0, 1] over the anneal horizon.
            x = min(1.0, global_step / args.lr_anneal_horizon)
            lr_now = cfg.lr / (8.0 * x + 1.0) ** 1.5
            for pg in opt.param_groups:
                pg["lr"] = lr_now
        if rnad_ref is not None and update % args.rnad_ref_every == 0:
            rnad_ref.load_state_dict(model.state_dict())
        # Fresh draw from the reservoir each rollout, so an update sees several past selves.
        if exploit_net is None:
            slots.assign(league, model.state_dict())
        distill_probs_t = distill_mask_t = None
        for t in range(cfg.rollout_steps):
            obs_l, ids_l, mask_l, _ = env.learner_view()
            obs_o, ids_o, mask_o, _ = env.opponent_view()

            # Search distillation (goal lever): on step 0 only, run the budget subgame search on
            # a small subset of Turn-state envs and keep its mixed strategy as a soft policy
            # target. AlphaZero's improvement operator at ~a few percent of wall clock.
            if t == 0 and args.search_distill_envs > 0 and update > 10:
                from probes.value_search import root_strategies
                act0 = np.asarray(env.vec.acting_all(0), dtype=bool)
                act1 = np.asarray(env.vec.acting_all(1), dtype=bool)
                turn_envs = np.flatnonzero(act0 & act1)
                pick = opp_rng.choice(turn_envs, size=min(args.search_distill_envs,
                                                          turn_envs.size), replace=False)
                rows_es = [(int(e), int(env.learner_side[e])) for e in pick]
                model.eval()
                try:
                    strats = root_strategies(model, device, env.vec, rows_es,
                                             topk=4, n_samples=1, det=True,
                                             counter=[int(global_step) + update])
                finally:
                    model.train()
                dp = np.zeros((cfg.num_envs, env.n_actions), dtype=np.float32)
                dmsk = np.zeros(cfg.num_envs, dtype=np.float32)
                for i, (acts, strat) in strats.items():
                    e = rows_es[i][0]
                    dp[e][acts] = strat
                    dmsk[e] = 1.0
                distill_probs_t = torch.as_tensor(dp, device=device)
                distill_mask_t = torch.as_tensor(dmsk, device=device)

            obs_t = torch.as_tensor(obs_l, device=device)
            ids_t = torch.as_tensor(ids_l, device=device)
            mask_t = torch.as_tensor(mask_l, device=device)
            with torch.no_grad():
                action, log_prob, _, value = model.act(obs_t, mask_t, obs_ids=ids_t)
            if exploit_net is None:
                opp_action = slots.actions(obs_o, ids_o, mask_o, opp_rng,
                                           vec=env.vec, sides=1 - env.learner_side)
            else:
                opp_action = greedy_actions(exploit_net, obs_o, ids_o, mask_o, device)

            # Kickstart: the Rust heuristic's opinion on the LEARNER's request (cheap — one
            # rayon call), stored as a distillation target while the anneal is live.
            teacher_t = None
            if args.kickstart_coef > 0 and global_step < args.kickstart_anneal_steps:
                teacher_np = np.asarray(env.vec.heuristic_actions_all(env.learner_side),
                                        dtype=np.int64)
                teacher_t = torch.as_tensor(teacher_np, device=device)

            belief_t = belief_m = None
            if buffer.belief:
                # Labels for the CURRENT (pre-step) obs, from the true state — both sides
                # fetched, learner's perspective selected per env.
                t0_, m0_ = env.vec.belief_targets_all(0)
                t1_, m1_ = env.vec.belief_targets_all(1)
                sel = (env.learner_side == 1)[:, None]
                belief_t = torch.as_tensor(np.where(sel, np.asarray(t1_), np.asarray(t0_)),
                                           device=device)
                belief_m = torch.as_tensor(
                    np.where(sel, np.asarray(m1_), np.asarray(m0_)).astype(np.float32),
                    device=device)

            action_np = action.cpu().numpy()
            reward, done, active, dyn, outcome = env.step(action_np, opp_action)

            if res is not None:
                rows = np.flatnonzero(active.astype(bool))
                if rows.size:
                    if res["obs"] is None:
                        res["obs"] = np.zeros((res["cap"], env.obs_dim), dtype=np.float32)
                        res["ids"] = np.zeros((res["cap"], env.id_dim), dtype=np.int64)
                        res["mask"] = np.zeros((res["cap"], env.n_actions), dtype=bool)
                        res["action"] = np.zeros(res["cap"], dtype=np.int64)
                    free = res["cap"] - res["n"]
                    take_new = rows[:free]
                    sl = slice(res["n"], res["n"] + take_new.size)
                    res["obs"][sl] = obs_l[take_new]; res["ids"][sl] = ids_l[take_new]
                    res["mask"][sl] = mask_l[take_new]; res["action"][sl] = action_np[take_new]
                    res["n"] += take_new.size
                    rest = rows[free:] if free else rows
                    if res["n"] >= res["cap"] and rest.size:
                        # batched algorithm-R: each survivor replaces a uniform slot w.p. cap/seen
                        p = res["cap"] / max(res["cap"], res["seen"])
                        pick = rest[res["rng"].random(rest.size) < p]
                        if pick.size:
                            slots_ = res["rng"].integers(0, res["cap"], size=pick.size)
                            res["obs"][slots_] = obs_l[pick]; res["ids"][slots_] = ids_l[pick]
                            res["mask"][slots_] = mask_l[pick]; res["action"][slots_] = action_np[pick]
                    res["seen"] += rows.size

            reward_t = torch.as_tensor(reward, device=device)
            if rnad_ref is not None:
                with torch.no_grad():
                    _, ref_lp, _, _ = rnad_ref.act(obs_t, mask_t, action=action, obs_ids=ids_t)
                act_f = torch.as_tensor(active.astype(np.float32), device=device)
                reward_t = reward_t - args.rnad_eta * (log_prob - ref_lp) * act_f

            buffer.add(t, obs_t, mask_t, action, log_prob, value,
                       reward_t,
                       torch.as_tensor(done, device=device),
                       obs_ids=ids_t,
                       opp_action=torch.as_tensor(opp_action, device=device) if cfg.aux else None,
                       dyn_target=torch.as_tensor(dyn, device=device) if cfg.aux else None,
                       active=torch.as_tensor(active.astype(np.float32), device=device),
                       teacher_action=teacher_t,
                       outcome_reward=torch.as_tensor(outcome, device=device)
                       if buffer.outcome else None,
                       outcome_value=model.outcome_pred if buffer.outcome else None,
                       belief_target=belief_t, belief_mask=belief_m)
            global_step += cfg.num_envs

            for e in np.flatnonzero(done):
                r = outcome[e]
                total_games += 1
                window.append(1 if r > 0 else (-1 if r < 0 else 0))
                # Credit the result to the opponent that actually played it — that is what makes
                # the PFSP weights mean anything.
                if exploit_net is None:
                    league.record(slots.env_key(int(e)), 1.0 if r > 0 else (0.0 if r < 0 else 0.5))

        with torch.no_grad():
            obs_l, ids_l, mask_l, _ = env.learner_view()
            _, last_value = model.forward(
                torch.as_tensor(obs_l, device=device),
                torch.as_tensor(mask_l, device=device),
                obs_ids=torch.as_tensor(ids_l, device=device))
        if distill_probs_t is not None:
            buffer.set_distill(distill_probs_t, distill_mask_t)
        else:
            buffer.set_distill(None, None)
        buffer.compute_gae(last_value, cfg.gamma, cfg.gae_lambda)
        if buffer.outcome:
            buffer.compute_gae_outcome(model.outcome_pred, cfg.gamma, cfg.gae_lambda)
        data = buffer.flat_view()
        if "distill_probs" in data:
            data["distill_coef"] = args.search_distill_coef
        if args.kickstart_coef > 0:
            frac = max(0.0, 1.0 - global_step / max(1, args.kickstart_anneal_steps))
            data["kick_coef"] = args.kickstart_coef * frac
        stats = ppo_update(model, opt, data, cfg, batch)

        # In exploit mode nothing clears the window; keep it a sliding recent-form estimate.
        if exploit_net is not None and len(window) > 50_000:
            del window[:-50_000]
        win_rate = float(np.mean([r == 1 for r in window])) if window else float("nan")
        sps = int((global_step - start_step) / max(1e-9, time.time() - start))
        print(f"update {update:>8}  step {global_step:>12,}  games {total_games:>7}  "
              f"win_rate(vs snapshot) {win_rate:5.2f}  pi {stats['policy_loss']:+.3f}  "
              f"v {stats['value_loss']:.3f}  ent {stats['entropy']:.3f}  "
              f"kl {stats['approx_kl']:.4f}  {sps} sps", flush=True)
        logger.metrics({"update": update, "step": global_step, "games": total_games,
                        "win_rate_vs_snapshot": win_rate, "sps": sps, **stats})

        if exploit_net is None and args.snapshot_every and update % args.snapshot_every == 0:
            league.add(model, global_step)
            window.clear()
            print(f"    [league +snapshot @ step {global_step:,}; "
                  f"pool={len(league.snapshots())}] {json.dumps(league.stats())[:220]}", flush=True)
            if heur_stats.get("fallbacks"):
                # The scripted league heuristic degrading to random would silently gut the
                # curriculum — same failure mode the eval path warns about, so warn here too.
                print(f"    [league] !! heuristic fallbacks {heur_stats['fallbacks']}"
                      f"/{heur_stats.get('calls', 0)} ({heur_stats.get('last_error', '?')})",
                      flush=True)
                heur_stats.clear()
            # `metrics()` keeps only scalars, so flatten the league summary into the
            # `pool/<opp>/{wr,weight}` keys train_long.py used for the nightX runs — snapshots
            # aggregate under `self` (mean wr over played snapshots / summed weight), which is
            # the nightX name for the frozen-checkpoint opponent.
            st = league.stats()
            scripted = {league.RANDOM, *league.scripted_weights}
            flat = {"update": update, "step": global_step, "league": st,
                    "pool_size": len(league.snapshots())}
            for k in scripted:
                if k in st:
                    flat[f"pool/{k}/wr"] = st[k]["wr"]
                    flat[f"pool/{k}/weight"] = st[k]["w"]
            snaps = {k: v for k, v in st.items() if k not in scripted}
            played = [v["wr"] for v in snaps.values() if v["n"] > 0]
            flat["pool/self/wr"] = (sum(played) / len(played)) if played else 0.5
            flat["pool/self/weight"] = sum(v["w"] for v in snaps.values())
            logger.metrics(flat)

        # --- absolute progress: fixed opponents, the only curve that means anything on its own ---
        if args.eval_every and update % args.eval_every == 0:
            for name, opp in baselines:
                r = evaluate_flow(model, opp, device, n_games=args.eval_games,
                                  num_envs=min(cfg.num_envs, 128), team_pool=pool or None,
                                  seed=cfg.seed + update, fog_species=args.fog_species,
                                  obs_version=args.obs_version, frames=args.frames)
                note = ""
                if name == "heuristic":
                    st = HEURISTIC_STATS.get("ref", {})
                    fb = st.get("fallbacks", 0)
                    if fb:
                        # A scripted baseline that cannot produce a move degrades to random and
                        # flatters the agent. Say so loudly rather than logging a pretty number.
                        note = (f"  !! {fb}/{st.get('calls', 0)} fallbacks "
                                f"({st.get('last_error', '?')}) — baseline NOT trustworthy")
                    st.clear()
                print(f"    [eval @ {update}] vs {name:>12}: win_rate {r['win_rate']:.3f} "
                      f"[{r['ci_low']:.3f},{r['ci_high']:.3f}] "
                      f"W/L/D {r['wins']}/{r['losses']}/{r['draws']}{note}", flush=True)
                logger.eval({"update": update, "step": global_step, "baseline": name, **r})
        if args.ckpt_every and update % args.ckpt_every == 0:
            save(update)

    save(update)
    if res is not None and res["n"]:
        np.savez_compressed(args.reservoir_out,
                            obs=res["obs"][:res["n"]], ids=res["ids"][:res["n"]],
                            mask=res["mask"][:res["n"]], action=res["action"][:res["n"]],
                            ret=np.zeros(res["n"], dtype=np.float32),  # bc_train value target: unused
                            gamma=cfg.gamma, obs_dim=env.obs_dim, id_dim=env.id_dim)
        print(f"[reservoir] wrote {res['n']:,} of {res['seen']:,} seen decisions "
              f"to {args.reservoir_out}")
    if exploit_net is not None:
        recent = window[-20_000:]
        summary = {"target": args.exploit, "steps": global_step, "games": total_games,
                   "exploiter_wr_recent": float(np.mean([r == 1 for r in recent])) if recent else None,
                   "recent_n": len(recent)}
        (run_dir / "exploit.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"[exploit] RESULT {json.dumps(summary)}")
    if _STOP:
        print(f"[train_flow] stopped cleanly at update {update}, step {global_step:,}")
    else:
        print(f"[train_flow] done. update {update}, step {global_step:,}")


def main():
    p = argparse.ArgumentParser(description="Long-horizon PPO self-play on the Rust decision-point engine.")
    p.add_argument("--run-dir", type=str, default=None, help="run directory (created if absent)")
    p.add_argument("--resume", type=str, default=None, metavar="RUN_DIR",
                   help="resume this run directory in place (implies --run-dir)")
    p.add_argument("--num-envs", type=int, default=512)
    p.add_argument("--rollout-steps", type=int, default=64, help="PER ENV; batch = num_envs * this")
    p.add_argument("--total-steps", type=int, default=0, help="<= 0 trains indefinitely")
    p.add_argument("--max-requests", type=int, default=1000, help="decision points before truncation")
    p.add_argument("--team-pool", type=str, default=DEFAULT_POOL,
                   help="gzipped JSONL team pool; '' for the fixed debug matchup (not for real runs)")
    p.add_argument("--lr", type=float, default=3e-4)
    p.add_argument("--gamma", type=float, default=0.995)
    p.add_argument("--gae-lambda", type=float, default=0.95)
    p.add_argument("--clip-eps", type=float, default=0.2)
    p.add_argument("--entropy-coef", type=float, default=0.005)
    p.add_argument("--value-coef", type=float, default=0.5)
    p.add_argument("--update-epochs", type=int, default=4)
    p.add_argument("--minibatch-size", type=int, default=4096)
    p.add_argument("--hidden-dim", type=int, default=256)
    p.add_argument("--n-hidden-layers", type=int, default=2)
    p.add_argument("--embed-dim", type=int, default=32)
    p.add_argument("--shaping-coef", type=float, default=0.0)
    p.add_argument("--aux", action="store_true", help="auxiliary opponent-action / world-model heads")
    p.add_argument("--frames", type=int, default=1, choices=[1, 2],
                   help="2 = append the previous request's obs (night-era D3 memory lever)")
    p.add_argument("--obs-version", type=int, default=1, choices=[1, 2],
                   help="2 = +damage-calc feature block (encode_v2, honest by scramble test)")
    p.add_argument("--fog-species", action="store_true",
                   help="honest fog of war: unseen foe species are masked in the obs (W8). "
                        "Breaking obs-semantics change — do NOT flip on a run trained without it")
    p.add_argument("--belief-head", action="store_true",
                   help="predict hidden foe identity (6 species + active item/moves) from the "
                        "public obs — free labels from the true state (W9)")
    p.add_argument("--outcome-head", action="store_true",
                   help="second value head on UNSHAPED terminal-outcome lambda-returns — the "
                        "search evaluator (E2 branch-B fix; EXPLORATION_PLAN W7)")
    p.add_argument("--snapshot-every", type=int, default=20,
                   help="add the current policy to the league reservoir every N updates")
    p.add_argument("--pool-size", type=int, default=20, help="league reservoir size (0 = unbounded)")
    p.add_argument("--opponent-slots", type=int, default=4,
                   help="distinct league opponents played per rollout (one forward pass each)")
    p.add_argument("--pfsp-power", type=float, default=2.0)
    p.add_argument("--pfsp-mode", type=str, default="frontier", choices=["frontier", "hard"],
                   help="frontier = contested opponents first; hard = whoever currently beats us")
    p.add_argument("--random-weight", type=float, default=0.25,
                   help="fixed share of league sampling given to the uniform-random opponent")
    p.add_argument("--league-heuristic-weight", type=float, default=0.0,
                   help="PFSP base weight for the scripted heuristic as a TRAINING opponent "
                        "(0 = off). The proven curriculum used 2 vs 1-per-snapshot.")
    p.add_argument("--target-kl", type=float, default=0.0,
                   help=">0: cut the epoch loop when approx_kl exceeds this (recipe: 0.03)")
    p.add_argument("--reservoir-out", type=str, default=None, metavar="NPZ",
                   help="reservoir-sample the learner's acting decisions across the run and "
                        "write them here at exit — the NFSP average-policy dataset (E3)")
    p.add_argument("--reservoir-cap", type=int, default=1_500_000)
    p.add_argument("--rnad-eta", type=float, default=0.0,
                   help=">0: R-NaD-style reward regularization toward a reference policy, "
                        "r' = r − η(log π − log π_ref) on acting steps (E4)")
    p.add_argument("--rnad-ref-every", type=int, default=50,
                   help="refresh the R-NaD reference to the current policy every N updates")
    p.add_argument("--setslot", action="store_true",
                   help="slot-shared move/switch scorers (night-era arch; needs obs v2)")
    p.add_argument("--lr-anneal-horizon", type=int, default=0,
                   help=">0: anneal lr by Wang-2024's power law over this many env steps "
                        "(lr0/(8x+1)^1.5; reaches lr0/27 at the horizon)")
    p.add_argument("--search-distill-envs", type=int, default=0,
                   help=">0: run the budget subgame search on this many Turn-state envs at "
                        "rollout step 0 and distill its mixed strategy into the policy")
    p.add_argument("--search-distill-coef", type=float, default=1.0)
    p.add_argument("--kickstart-coef", type=float, default=0.0,
                   help=">0: distill toward the Rust heuristic's action with this coefficient, "
                        "annealed linearly to 0 over --kickstart-anneal-steps (E1 arm c)")
    p.add_argument("--kickstart-anneal-steps", type=int, default=5_000_000)
    p.add_argument("--init", type=str, default=None, metavar="CKPT",
                   help="warm-start model weights from this checkpoint (fresh optimizer/league; "
                        "ignored when --resume finds a training_state)")
    p.add_argument("--exploit", type=str, default=None, metavar="CKPT",
                   help="best-response probe: train ONLY against this frozen checkpoint "
                        "(league disabled); the learner's win-rate curve is the "
                        "exploitability proxy (EXPLORATION_PLAN P0a)")
    p.add_argument("--eval-every", type=int, default=25,
                   help="powered eval vs fixed baselines every N updates (0 = never)")
    p.add_argument("--eval-games", type=int, default=300, help="games per baseline eval")
    p.add_argument("--ckpt-every", type=int, default=25)
    p.add_argument("--keep-checkpoints", type=int, default=5, help="0 keeps all")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--device", type=str, default=None, help="auto|cpu|cuda")
    p.add_argument("--wandb", action="store_true")
    p.add_argument("--wandb-project", type=str, default="deep-showdown")
    args = p.parse_args()

    if args.resume:
        args.run_dir = args.resume
        args.resume = True
    else:
        if not args.run_dir:
            args.run_dir = time.strftime("runs/flow-%Y%m%d-%H%M%S")
        args.resume = True  # resuming a fresh dir is a no-op; makes relaunch always safe
    _install_signal_handlers()
    train(args)


if __name__ == "__main__":
    main()
