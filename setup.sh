#!/usr/bin/env bash
# One-shot, idempotent bootstrap for a fresh Linux GPU box.
#
#   ./setup.sh              # everything
#   ./setup.sh --no-gates   # skip the (slow) verification sweeps at the end
#
# Installs/【verifies】: Rust toolchain, Node, the pinned Pokémon Showdown clone + build, the
# Python venv with a torch built for the *installed driver's* CUDA, and the pyo3 bridge. Safe to
# re-run: every step short-circuits when it is already satisfied.
set -euo pipefail

cd "$(dirname "$0")"
ROOT=$(pwd)
NODE_VER=${NODE_VER:-v22.14.0}
NODE_DIR="$HOME/.local/node-$NODE_VER-linux-x64"
PS_PIN=$(sed -n 's/.*"commit"[[:space:]]*:[[:space:]]*"\([0-9a-f]\{40\}\)".*/\1/p' showdown-rs/ps.lock | head -1)
[ -n "$PS_PIN" ] || { echo "could not read the PS pin out of showdown-rs/ps.lock" >&2; exit 1; }

say() { printf '\n=== %s ===\n' "$*"; }

# ---- Rust ---------------------------------------------------------------------------------
say "Rust toolchain"
if ! command -v cargo >/dev/null && [ ! -x "$HOME/.cargo/bin/cargo" ]; then
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain stable --profile minimal
fi
export PATH="$HOME/.cargo/bin:$PATH"
cargo --version

# ---- Node ---------------------------------------------------------------------------------
say "Node $NODE_VER"
if [ ! -x "$NODE_DIR/bin/node" ]; then
	mkdir -p "$HOME/.local"
	curl -sSL "https://nodejs.org/dist/$NODE_VER/node-$NODE_VER-linux-x64.tar.xz" -o /tmp/node.tar.xz
	tar -xJf /tmp/node.tar.xz -C "$HOME/.local"
	rm -f /tmp/node.tar.xz
fi
export PATH="$NODE_DIR/bin:$PATH"
node --version

# ---- Pokémon Showdown, pinned -------------------------------------------------------------
say "Pokémon Showdown @ $PS_PIN"
# NOTE: `engines/` is gitignored and, in some checkouts, is a stale symlink to another machine's
# path. Replace anything that isn't a real directory.
[ -e engines ] && [ ! -d engines ] && rm -f engines
mkdir -p engines
if [ ! -d engines/pokemon-showdown/.git ]; then
	git clone -q https://github.com/smogon/pokemon-showdown engines/pokemon-showdown
fi
(
	cd engines/pokemon-showdown
	if [ "$(git rev-parse HEAD)" != "$PS_PIN" ]; then
		git fetch -q origin "$PS_PIN" 2>/dev/null || git fetch -q origin
		git checkout -q "$PS_PIN"
		rm -rf dist
	fi
	if [ ! -f dist/sim/index.js ]; then
		npm install --no-audit --no-fund --silent
		npm run build
	fi
)
node showdown-rs/harness/check-ps-pin.mjs >/dev/null && echo "PS pin verified"

# ---- Python venv --------------------------------------------------------------------------
say "Python venv + torch"
command -v uv >/dev/null || { echo "uv not found — install it: https://docs.astral.sh/uv/" >&2; exit 1; }
# GitHub CLI: used for PRs/issues against this repo. Cheap, and absent from a bare box.
command -v gh >/dev/null || sudo apt-get install -y gh >/dev/null 2>&1 || \
  echo "note: gh install failed (not fatal)" >&2
cd agents
[ -d .venv ] || uv venv --python 3.12 .venv
export VIRTUAL_ENV="$PWD/.venv"

# torch must match the *driver's* CUDA, not the newest wheel: a cu130 wheel on a 12.8 driver
# imports fine and then reports cuda.is_available() == False. Pick the index from nvidia-smi.
if ! .venv/bin/python -c "import torch, sys; sys.exit(0 if torch.cuda.is_available() else 1)" 2>/dev/null; then
	CUDA_VER=$(nvidia-smi --query-gpu=driver_version --format=csv,noheader 2>/dev/null >/dev/null && \
		nvidia-smi 2>/dev/null | sed -n 's/.*CUDA Version: \([0-9]*\)\.\([0-9]*\).*/\1\2/p' | head -1)
	IDX="https://download.pytorch.org/whl/cu${CUDA_VER:-128}"
	echo "installing torch from $IDX"
	uv pip install --quiet numpy wandb "maturin>=1.5,<2.0"
	uv pip install --quiet --index-url "$IDX" torch || {
		echo "cu$CUDA_VER wheels unavailable; falling back to cu128"
		uv pip install --quiet --index-url https://download.pytorch.org/whl/cu128 torch
	}
else
	uv pip install --quiet numpy wandb "maturin>=1.5,<2.0"
fi
.venv/bin/python -c "import torch; print('torch', torch.__version__, 'cuda', torch.cuda.is_available(),
      torch.cuda.get_device_name(0) if torch.cuda.is_available() else '(CPU ONLY)')"

# ---- the Rust<->Python bridge --------------------------------------------------------------
say "pyo3 bridge (showdown_engine)"
.venv/bin/maturin develop --release -m ../showdown-rs/crates/pybridge/Cargo.toml 2>&1 | tail -2
.venv/bin/python -c "
import showdown_engine as se
v = se.FlowVec(4, seed=1, team_pool='../showdown-rs/harness/team-pool/gen9randombattle-2k.jsonl.gz')
print(f'FlowVec ok: obs_dim={v.obs_dim} actions={v.n_actions} pool={v.pool_size}')"
cd "$ROOT"

# ---- gates ---------------------------------------------------------------------------------
if [ "${1:-}" = "--no-gates" ]; then
	say "skipping gates (--no-gates)"
else
	say "engine unit tests"
	(cd showdown-rs && cargo test --release -p engine 2>&1 | tail -5)
	say "seed gate (committed fixtures)"
	(cd showdown-rs && cargo build --release -p cosim 2>&1 | tail -1
	 SEED_GATE=1 ./target/release/cosim harness/seed-fixtures/*.fx.json.gz 2>/dev/null | head -6)
fi

cat <<EOF

=== ready ===
Add to your shell (or re-run this script's exports):
  export PATH="\$HOME/.cargo/bin:$NODE_DIR/bin:\$PATH"

Start a long run (detached, auto-resuming, with the on-policy cosim sidecar):
  agents/scripts/launch_train.sh runs/scale1 --num-envs 4096 --rollout-steps 8

Follow it:
  tail -f runs/scale1/train.log
EOF
