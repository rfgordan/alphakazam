"""Long-run training infrastructure.

Three pieces, each fixing an observed long-run failure:
  ServerManager — the trainer owns the Showdown server process and recycles it periodically
                  (battle-room accumulation degraded sps 650 -> 24 on a multi-hour run).
  OpponentPool  — PFSP-weighted opponent sampling over scripted baselines + frozen self
                  checkpoints (single-opponent training produced a random-specialist:
                  0.99 random / 0.05 heuristic).
  EnvFleet      — N parallel envs stepped by worker threads with one batched policy forward
                  (a single synchronous env is network-bound at ~650 steps/s).
"""

from __future__ import annotations

import logging
import random as pyrandom
import socket
import subprocess
import time
from collections import deque
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from threading import Lock

import numpy as np
import torch

from poke_env.player import RandomPlayer, MaxBasePowerPlayer, SimpleHeuristicsPlayer

from .pokeenv_env import DeepShowdownSinglesEnv, MaskedSingleAgentEnv

_LOGGER = logging.getLogger("fleet")
SCRIPTED = {"random": RandomPlayer, "maxbp": MaxBasePowerPlayer, "heuristic": SimpleHeuristicsPlayer}
PS_DIR = Path(__file__).resolve().parents[2] / "engines" / "pokemon-showdown"


class ServerManager:
    """Owns the local Showdown server process; recycle() gives a fresh one mid-training."""

    def __init__(self, port: int = 8000, log_path: str = "/tmp/ps-server-train.log"):
        self.port, self.log_path, self.proc = port, log_path, None

    def _port_open(self) -> bool:
        with socket.socket() as s:
            s.settimeout(0.3)
            return s.connect_ex(("127.0.0.1", self.port)) == 0

    def start(self, timeout: float = 60):
        subprocess.run(["pkill", "-f", "pokemon-showdown start"], capture_output=True)
        t0 = time.time()
        while self._port_open() and time.time() - t0 < 10:
            time.sleep(0.3)
        self.proc = subprocess.Popen(
            ["node", "pokemon-showdown", "start", "--no-security"], cwd=PS_DIR,
            stdout=open(self.log_path, "w"), stderr=subprocess.STDOUT)
        t0 = time.time()
        while time.time() - t0 < timeout:
            if self._port_open():
                time.sleep(1.5)   # let workers finish booting
                return
            if self.proc.poll() is not None:
                raise RuntimeError(f"server exited at boot; see {self.log_path}")
            time.sleep(0.5)
        raise TimeoutError("Showdown server did not open its port")

    def stop(self):
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(10)
            except subprocess.TimeoutExpired:
                self.proc.kill()
        subprocess.run(["pkill", "-f", "pokemon-showdown start"], capture_output=True)
        self.proc = None

    def recycle(self):
        _LOGGER.info("recycling Showdown server")
        self.stop()
        self.start()


class OpponentPool:
    """PFSP opponent sampling: weight ∝ base_w * (1 - winrate + 0.1)^power, so training time
    flows to opponents that currently beat us. 'self' draws a frozen checkpoint from pool_dir."""

    def __init__(self, spec: str, battle_format: str, pool_dir, pfsp_power: float = 2.0,
                 window: int = 256, self_recent: int = 10, mode: str = "hard"):
        # mode: "hard" = focus the currently-hardest opponents, w ∝ (1-wr)^p (exploit-the-gap);
        #       "frontier" = focus LEARNABLE opponents, w ∝ (wr(1-wr))^p — from a cold start,
        #       unbeatable opponents get few episodes until the policy can contest them.
        self.mode = mode
        self.base: dict[str, float] = {}
        for part in spec.split(","):
            name, _, w = part.strip().partition(":")
            assert name in SCRIPTED or name == "self", f"unknown opponent '{name}'"
            self.base[name] = float(w) if w else 1.0
        self.battle_format = battle_format
        self.pool_dir = Path(pool_dir)
        self.pool_dir.mkdir(parents=True, exist_ok=True)
        self.pfsp_power = pfsp_power
        self.self_recent = self_recent
        self.results = {k: deque(maxlen=window) for k in self.base}
        self._lock = Lock()

    def snapshots(self):
        return sorted(self.pool_dir.glob("snap_*.pt"))

    def winrate(self, k: str) -> float:
        d = self.results[k]
        return sum(d) / len(d) if d else 0.5

    def weights(self) -> dict[str, float]:
        w = {}
        for k, b in self.base.items():
            if k == "self" and not self.snapshots():
                w[k] = 0.0
                continue
            wr = self.winrate(k)
            if self.mode == "frontier":
                w[k] = b * (wr * (1.0 - wr) + 0.1) ** self.pfsp_power
            else:
                w[k] = b * (1.0 - wr + 0.1) ** self.pfsp_power
        tot = sum(w.values())
        if tot <= 0:   # degenerate (e.g. only 'self' with empty pool): uniform over scripted
            w = {k: 1.0 for k in self.base if k in SCRIPTED} or {"random": 1.0}
            tot = sum(w.values())
        return {k: v / tot for k, v in w.items()}

    def sample(self):
        """-> (key, fresh Player instance). Thread-safe."""
        with self._lock:
            w = self.weights()
        keys = list(w)
        k = keys[int(np.random.choice(len(keys), p=[w[x] for x in keys]))]
        if k == "self":
            try:
                from .model_player import ModelPlayer   # lazy: avoids import cycle
                ck = pyrandom.choice(self.snapshots()[-self.self_recent:])
                return k, ModelPlayer(str(ck), battle_format=self.battle_format,
                                      start_listening=False, greedy=False)
            except Exception as e:   # a bad snapshot must never kill a long run
                _LOGGER.warning("self-snapshot load failed (%s: %s); falling back to random",
                                type(e).__name__, e)
                return "random", SCRIPTED["random"](battle_format=self.battle_format,
                                                    start_listening=False)
        return k, SCRIPTED[k](battle_format=self.battle_format, start_listening=False)

    def record(self, k: str, won: bool):
        with self._lock:
            self.results[k].append(1.0 if won else 0.0)

    def snapshot(self, model, update: int):
        torch.save(model.state_dict(), self.pool_dir / f"snap_{update:06d}.pt")

    def stats(self) -> dict:
        w = self.weights()
        return {k: {"wr": round(self.winrate(k), 3), "n": len(self.results[k]),
                    "w": round(w.get(k, 0.0), 3)} for k in self.base}

    def load_results(self, saved: dict):
        for k, vals in saved.items():
            if k in self.results:
                self.results[k].extend(vals)

    def dump_results(self) -> dict:
        return {k: list(d) for k, d in self.results.items()}


class EnvFleet:
    """N parallel single-agent envs with per-episode PFSP opponent sampling and flake recovery."""

    def __init__(self, n_envs: int, battle_format: str, pool: OpponentPool,
                 hp_value: float, fainted_value: float, victory_value: float,
                 boost_value: float = 0.0, status_value: float = 0.0,
                 eval_opponents: tuple = (), frames: int = 1, redistribute: bool = False):
        self.n = n_envs
        self.battle_format = battle_format
        self.pool = pool
        self.frames = frames
        self.redistribute = redistribute
        self.reward_kw = dict(hp_value=hp_value, fainted_value=fainted_value,
                              victory_value=victory_value, boost_value=boost_value,
                              status_value=status_value)
        self.eval_opponents = eval_opponents
        self.executor = ThreadPoolExecutor(max_workers=n_envs)
        self.envs: list = []
        self.opp_keys: list[str] = []
        self.eval_envs: dict = {}
        self.flakes = 0
        self.episodes = 0

    def _new_env(self):
        key, opp = self.pool.sample()
        env = DeepShowdownSinglesEnv(battle_format=self.battle_format, **self.reward_kw)
        return MaskedSingleAgentEnv(env, opp, frames=self.frames,
                                    redistribute=self.redistribute), key

    def build(self):
        """(Re)create all train + eval envs; returns initial (obs[N], mask[N])."""
        self.envs, self.opp_keys = [], []
        for _ in range(self.n):                       # serial create (websocket setup)...
            w, k = self._new_env()
            self.envs.append(w)
            self.opp_keys.append(k)
        firsts = list(self.executor.map(lambda w: w.reset(), self.envs))   # ...parallel reset
        self.eval_envs = {}
        for name in self.eval_opponents:
            env = DeepShowdownSinglesEnv(battle_format=self.battle_format, **self.reward_kw)
            self.eval_envs[name] = MaskedSingleAgentEnv(
                env, SCRIPTED[name](battle_format=self.battle_format, start_listening=False),
                frames=self.frames)
        obs = np.stack([f[0] for f in firsts])
        mask = np.stack([f[1] for f in firsts])
        return obs, mask

    def close(self):
        for w in self.envs + list(self.eval_envs.values()):
            try:
                w.env.close()
            except Exception:
                pass
        self.envs, self.eval_envs = [], {}

    def rebuild(self, server: ServerManager):
        """Full refresh: close envs, recycle the server, rebuild. The scheduled cure for
        battle-room accumulation and poke-env per-battle dict growth."""
        self.close()
        server.recycle()
        return self.build()

    # -- stepping -----------------------------------------------------------------------------------
    def _step_one(self, i: int, action: int):
        env = self.envs[i]
        try:
            obs, mask, r, done = env.step(int(action))
            shaping, victory = env.last_shaping, env.last_victory
            is_setup, redist = env.last_is_setup, env.last_redist
            if done:
                self.episodes += 1
                self.pool.record(self.opp_keys[i], bool(env.env.battle1.won))
                key, opp = self.pool.sample()
                env.opponent = opp
                self.opp_keys[i] = key
                obs, mask = env.reset()
            return i, obs, mask, r, done, shaping, victory, is_setup, redist
        except Exception as e:
            self.flakes += 1
            _LOGGER.warning("env %d flake (%s: %s) — rebuilding env", i, type(e).__name__, e)
            try:
                env.env.close()
            except Exception:
                pass
            w, k = self._new_env()          # raises if the server itself is down -> outer recycle
            self.envs[i], self.opp_keys[i] = w, k
            obs, mask = w.reset()
            return i, obs, mask, 0.0, True, 0.0, 0.0, False, 0.0   # truncated episode

    def step(self, actions: np.ndarray):
        """Step all envs concurrently. -> obs[N], mask[N], reward[N], done[N], shaping[N], victory[N]"""
        outs = list(self.executor.map(lambda i: self._step_one(i, actions[i]), range(self.n)))
        outs.sort(key=lambda o: o[0])
        obs = np.stack([o[1] for o in outs])
        mask = np.stack([o[2] for o in outs])
        rew = np.array([o[3] for o in outs], dtype=np.float32)
        done = np.array([o[4] for o in outs], dtype=np.float32)
        shp = np.array([o[5] for o in outs], dtype=np.float32)
        vic = np.array([o[6] for o in outs], dtype=np.float32)
        setup = np.array([o[7] for o in outs], dtype=bool)
        redist = np.array([o[8] for o in outs], dtype=np.float32)
        return obs, mask, rew, done, shp, vic, setup, redist
