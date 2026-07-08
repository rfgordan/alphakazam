"""Long-horizon PPO on poke-env with parallel envs, a managed/recycled Showdown server,
a PFSP opponent pool (scripted + frozen self snapshots), and true checkpoint/resume.

    python -m ppo.train_long --num-envs 8 --total-steps 3000000 \
        --opponents "random:1,maxbp:1,heuristic:1,self:2" --run-dir runs/long1

Resume after any interruption (weights, optimizer, schedules, PFSP stats all restored):
    python -m ppo.train_long --resume runs/long1

Semantics: --rollout-steps is PER ENV (batch = num_envs * rollout_steps).
Defaults differ from the short-run trainer where robustness demands it: gamma 0.995 (longer
games vs strong opponents), constant entropy 0.005 (mixed strategies stay valuable — do not
anneal to determinism against adaptive opponents).
"""

from __future__ import annotations

import argparse
import json
import resource
import time
from pathlib import Path

import numpy as np
import torch

from .buffer import RolloutBuffer
from .model import ActorCritic
from .pokeenv_env import OBS_DIM, N_ACTIONS
from .pokeenv_train import ppo_update, evaluate
from .fleet import ServerManager, OpponentPool, EnvFleet


def build_model(args, device):
    obs_dim = OBS_DIM * args.frames
    if args.arch in ("slot", "setslot"):
        from .slot_model import SlotActorCritic
        return SlotActorCritic(obs_dim, N_ACTIONS, hidden_dim=args.hidden_dim,
                               n_hidden_layers=args.n_hidden_layers,
                               set_context=(args.arch == "setslot"),
                               frames=args.frames).to(device)
    return ActorCritic(obs_dim, N_ACTIONS, args.hidden_dim, args.n_hidden_layers,
                       embed=None, aux=False).to(device)


def train(args):
    device = torch.device(args.device)
    torch.manual_seed(args.seed)
    np.random.seed(args.seed)
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    resource.setrlimit(resource.RLIMIT_NOFILE, (max(soft, min(hard, 8192)), hard))

    run_dir = Path(args.run_dir)
    run_dir.mkdir(parents=True, exist_ok=True)
    state_path = run_dir / "training_state.pt"

    model = build_model(args, device)
    if args.init_from:
        sd = torch.load(args.init_from, map_location=device)
        if args.arch == "slot":
            from .slot_model import transfer_v2_state_dict, _V2_TRUNK_IN
            if sd["trunk.0.weight"].shape[1] == _V2_TRUNK_IN != len(model.trunk_idx):
                sd = transfer_v2_state_dict(sd, model)
                print(f"transferred v2-era checkpoint {args.init_from} to v3 obs layout")
        model.load_state_dict(sd)
        print(f"initialized weights from {args.init_from}")
    opt = torch.optim.Adam(model.parameters(), lr=args.lr, eps=1e-5)

    pool = OpponentPool(args.opponents, args.format, run_dir / "pool",
                        pfsp_power=args.pfsp_power, mode=args.pfsp_mode)

    start_update, global_step = 1, 0
    if args.resuming and state_path.exists():
        st = torch.load(state_path, map_location=device)
        model.load_state_dict(st["model"])
        opt.load_state_dict(st["optimizer"])
        start_update, global_step = st["update"] + 1, st["global_step"]
        pool.load_results(st.get("pool_results", {}))
        print(f"resumed from update {st['update']} (step {global_step:,})")

    M, T = args.num_envs, args.rollout_steps
    batch = M * T
    num_updates = args.total_steps // batch
    buf = RolloutBuffer(T, M, OBS_DIM * args.frames, N_ACTIONS, device)

    server = ServerManager()
    server.start()
    fleet = EnvFleet(M, args.format, pool, args.hp_value, args.fainted_value,
                     args.victory_value, boost_value=args.boost_value,
                     status_value=args.status_value, frames=args.frames,
                     redistribute=args.redistribute,
                     eval_opponents=tuple(
                         s for s in args.eval_opponents.split(",") if s) if args.eval_every else ())
    obs_np, mask_np = fleet.build()
    obs = torch.as_tensor(obs_np, dtype=torch.float32, device=device)
    mask = torch.as_tensor(mask_np, dtype=torch.bool, device=device)

    metrics_f = (run_dir / "metrics.jsonl").open("a" if args.resuming else "w")
    eval_f = (run_dir / "eval.jsonl").open("a" if args.resuming else "w")

    wb = None
    if args.wandb:
        try:
            import wandb
            run_id = run_dir.name.replace("/", "-")
            try:
                wb = wandb.init(project=args.wandb, name=run_dir.name, id=run_id,
                                resume="allow", dir=str(run_dir),
                                config={k: v for k, v in vars(args).items() if k != "resuming"})
            except Exception:
                print("[wandb] online init failed; falling back to offline mode "
                      "(sync later with `wandb sync`)")
                wb = wandb.init(project=args.wandb, name=run_dir.name, id=run_id,
                                resume="allow", dir=str(run_dir), mode="offline",
                                config={k: v for k, v in vars(args).items() if k != "resuming"})
        except Exception as e:
            print(f"[wandb] disabled ({type(e).__name__}: {e})")
    print(f"arch={args.arch} params={model.num_params():,} envs={M} batch={batch} "
          f"updates {start_update}..{num_updates} opponents={args.opponents}")

    ep_ret = np.zeros(M, dtype=np.float32)
    recent_returns: list[float] = []
    start = time.time()
    steps_at_start = global_step

    try:
        for update in range(start_update, num_updates + 1):
            if args.server_recycle and update > start_update and \
                    (update - 1) % args.server_recycle == 0:
                obs_np, mask_np = fleet.rebuild(server)
                obs = torch.as_tensor(obs_np, dtype=torch.float32, device=device)
                mask = torch.as_tensor(mask_np, dtype=torch.bool, device=device)

            if args.anneal_lr:
                for g in opt.param_groups:
                    g["lr"] = args.lr * (1.0 - (update - 1) / num_updates)
            ent_coef = args.entropy_coef * (1.0 - (update - 1) / num_updates) \
                if args.anneal_entropy else args.entropy_coef

            # --- collect (with one full-recycle retry if the fleet dies mid-rollout) -------------
            t_collect0 = time.time()
            last_setup_t = np.full(M, -1, dtype=np.int64)   # per-env setup timestep (this rollout)
            redist_total = 0.0
            for t in range(T):
                with torch.no_grad():
                    action, log_prob, _, value = model.act(obs, mask)
                try:
                    obs_np, mask_np, rew, done, _shp, _vic, setup_ev, redist = \
                        fleet.step(action.cpu().numpy())
                except Exception as e:
                    print(f"[recover] fleet.step failed ({type(e).__name__}: {e}); full recycle")
                    obs_np, mask_np = fleet.rebuild(server)
                    rew = np.zeros(M, np.float32)
                    done = np.ones(M, np.float32)
                    setup_ev = np.zeros(M, bool)
                    redist = np.zeros(M, np.float32)
                buf.add(t, obs, mask, action, log_prob, value,
                        torch.as_tensor(rew, device=device), torch.as_tensor(done, device=device))
                if args.redistribute:
                    for i in range(M):
                        # move the boost's damage share from this step back to the setup action
                        if redist[i] > 0 and last_setup_t[i] >= 0:
                            buf.rewards[t, i] -= float(redist[i])
                            buf.rewards[last_setup_t[i], i] += float(redist[i])
                            redist_total += float(redist[i])
                        if setup_ev[i]:
                            last_setup_t[i] = t
                        if done[i]:
                            last_setup_t[i] = -1
                ep_ret += rew
                for i in np.flatnonzero(done):
                    recent_returns.append(float(ep_ret[i]))
                    ep_ret[i] = 0.0
                obs = torch.as_tensor(obs_np, dtype=torch.float32, device=device)
                mask = torch.as_tensor(mask_np, dtype=torch.bool, device=device)
                global_step += M

            t_collect = time.time() - t_collect0

            t_update0 = time.time()
            with torch.no_grad():
                _, last_value = model.forward(obs, mask)
            buf.compute_gae(last_value, args.gamma, args.gae_lambda)
            data = buf.flat_view()
            with torch.no_grad():
                var = data["returns"].var()
                ev = float(1 - (data["returns"] - data["values"]).var() / (var + 1e-8)) if var > 0 else 0.0
            stats = ppo_update(model, opt, data, args, batch, ent_coef)
            t_update = time.time() - t_update0
            # env-bound diagnostic: wallclock split between generating and consuming experience
            stats["t_collect"] = round(t_collect, 2)
            stats["t_update"] = round(t_update, 3)
            if args.redistribute:
                stats["redist_total"] = round(redist_total, 2)

            if update % args.snapshot_every == 0:
                pool.snapshot(model, update)

            if update % args.log_every == 0:
                ps = pool.stats()
                sps = int((global_step - steps_at_start) / max(1e-9, time.time() - start))
                ret = float(np.mean(recent_returns[-100:])) if recent_returns else float("nan")
                row = dict(update=update, step=global_step, sps=sps, ep_return=ret,
                           episodes=fleet.episodes, flakes=fleet.flakes, ent_coef=ent_coef,
                           explained_var=ev, pool=ps, **stats)
                opps = "  ".join(f"{k}:{v['wr']:.2f}(w{v['w']:.2f})" for k, v in ps.items())
                print(f"upd {update:4d}/{num_updates} step {global_step:>9,} {sps}sps "
                      f"ret {ret:+.1f} ev {ev:+.2f} ent {stats['entropy']:.2f} "
                      f"kl {stats['approx_kl']:.4f} | {opps} | flakes {fleet.flakes}")
                metrics_f.write(json.dumps(row) + "\n")
                metrics_f.flush()
                if wb is not None:
                    flat = {k: v for k, v in row.items() if not isinstance(v, dict)}
                    for k, s in ps.items():   # pool/<opp>/{wr,weight,n}
                        flat[f"pool/{k}/wr"] = s["wr"]; flat[f"pool/{k}/weight"] = s["w"]
                    wb.log(flat, step=global_step)

            if args.eval_every and update % args.eval_every == 0:
                try:
                    wr = evaluate(model, fleet.eval_envs, device, episodes=args.eval_episodes)
                    print("  [eval] " + "  ".join(f"{k}={v:.2f}" for k, v in wr.items()))
                    eval_f.write(json.dumps(dict(update=update, step=global_step, **wr)) + "\n")
                    eval_f.flush()
                    if wb is not None:
                        wb.log({f"eval/{k}": v for k, v in wr.items()}, step=global_step)
                except Exception as e:
                    print(f"  [eval] skipped: {type(e).__name__}: {e}")

            if args.max_minutes and (time.time() - start) / 60.0 >= args.max_minutes:
                print(f"[wallclock] {args.max_minutes} min reached at update {update} — stopping")
                torch.save(model.state_dict(), run_dir / f"model_{global_step}.pt")
                torch.save(dict(model=model.state_dict(), optimizer=opt.state_dict(),
                                update=update, global_step=global_step,
                                pool_results=pool.dump_results(),
                                args={k: v for k, v in vars(args).items()
                                      if k not in ("resuming", "resume_dir")}), state_path)
                break

            if update % args.save_every == 0 or update == num_updates:
                torch.save(model.state_dict(), run_dir / f"model_{global_step}.pt")
                torch.save(dict(model=model.state_dict(), optimizer=opt.state_dict(),
                                update=update, global_step=global_step,
                                pool_results=pool.dump_results(),
                                args={k: v for k, v in vars(args).items()
                                      if k not in ("resuming", "resume_dir")}), state_path)
    finally:
        metrics_f.close(); eval_f.close()
        fleet.close()
        server.stop()
        if wb is not None:
            wb.finish()

    torch.save(model.state_dict(), run_dir / "model_final.pt")
    print(f"done. {run_dir}/model_final.pt")


def main():
    p = argparse.ArgumentParser(description="Long-run PPO: parallel envs + PFSP opponent pool.")
    p.add_argument("--run-dir", default="runs/long")
    p.add_argument("--resume", dest="resume_dir", default=None,
                   help="resume from this run dir (restores weights/optimizer/schedules/PFSP)")
    p.add_argument("--num-envs", type=int, default=8)
    p.add_argument("--rollout-steps", type=int, default=256, help="PER-ENV steps per update")
    p.add_argument("--total-steps", type=int, default=None,
                   help="env steps to train (default 3M; on --resume, only an EXPLICIT value extends the run)")
    p.add_argument("--opponents", default="random:1,maxbp:1,heuristic:1,self:2")
    p.add_argument("--pfsp-power", type=float, default=2.0)
    p.add_argument("--pfsp-mode", choices=["hard", "frontier"], default="hard",
                   help="hard: w~(1-wr)^p; frontier: w~(wr(1-wr))^p (learnability — for cold starts)")
    p.add_argument("--max-minutes", type=float, default=0,
                   help="hard wallclock stop for the training loop (0 = off)")
    p.add_argument("--boost-value", type=float, default=0.0,
                   help="potential weight per net positive boost stage (reward v2)")
    p.add_argument("--status-value", type=float, default=0.0,
                   help="potential weight per statused mon differential (reward v2)")
    p.add_argument("--redistribute", action="store_true",
                   help="move each boosted attack's boost-attributable damage reward back to the setup action")
    p.add_argument("--snapshot-every", type=int, default=25, help="updates between self-pool snapshots")
    p.add_argument("--server-recycle", type=int, default=30, help="updates between server recycles (0=off)")
    p.add_argument("--arch", choices=["mlp", "slot", "setslot"], default="slot")
    p.add_argument("--frames", type=int, default=1,
                   help="stack the previous k-1 observations (short-term memory; current frame first)")
    p.add_argument("--hidden-dim", type=int, default=240)
    p.add_argument("--n-hidden-layers", type=int, default=2)
    p.add_argument("--init-from", default=None)
    p.add_argument("--format", default="gen9randombattle")
    p.add_argument("--device", default="cpu")
    p.add_argument("--seed", type=int, default=0)
    p.add_argument("--lr", type=float, default=2.5e-4)
    p.add_argument("--anneal-lr", action="store_true")
    p.add_argument("--gamma", type=float, default=0.995)
    p.add_argument("--gae-lambda", type=float, default=0.95)
    p.add_argument("--clip-eps", type=float, default=0.2)
    p.add_argument("--entropy-coef", type=float, default=0.005)
    p.add_argument("--anneal-entropy", action="store_true")
    p.add_argument("--value-coef", type=float, default=0.5)
    p.add_argument("--max-grad-norm", type=float, default=0.5)
    p.add_argument("--update-epochs", type=int, default=4)
    p.add_argument("--minibatch-size", type=int, default=512)
    p.add_argument("--grad-accum", type=int, default=1)
    p.add_argument("--norm-advantages", action="store_true", default=True)
    p.add_argument("--target-kl", type=float, default=0.03)
    p.add_argument("--hp-value", type=float, default=0.5)
    p.add_argument("--fainted-value", type=float, default=1.5)
    p.add_argument("--victory-value", type=float, default=20.0)
    p.add_argument("--eval-every", type=int, default=0, help="updates between evals (0=off)")
    p.add_argument("--eval-episodes", type=int, default=100)
    p.add_argument("--eval-opponents", default="random,maxbp,heuristic")
    p.add_argument("--save-every", type=int, default=25)
    p.add_argument("--log-every", type=int, default=1)
    p.add_argument("--wandb", default="deep-showdown", metavar="PROJECT",
                   help="log metrics/evals to this wandb project (resume-aware; offline fallback; "
                        "pass an empty string to disable)")
    args = p.parse_args()
    if args.target_kl == 0:
        args.target_kl = None
    args.resuming = bool(args.resume_dir)
    if args.resume_dir:
        args.run_dir = args.resume_dir
        state_path = Path(args.resume_dir) / "training_state.pt"
        if state_path.exists():   # restore the run's own hyperparameters/schedules exactly
            cli_total = args.total_steps          # None unless the user explicitly passed it
            saved = torch.load(state_path, map_location="cpu").get("args", {})
            for k, v in saved.items():
                setattr(args, k, v)
            args.run_dir = args.resume_dir
            if cli_total is not None:             # only an explicit value extends a run
                args.total_steps = max(args.total_steps, cli_total)
    if args.total_steps is None:
        args.total_steps = 3_000_000
    train(args)


if __name__ == "__main__":
    main()
