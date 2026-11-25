#!/usr/bin/env bash
# Wrangler custom build. Keep env assignments here: wrangler splits
# `build.command` on spaces, so inline `RUSTFLAGS=…` in jsonc is not an env var.
set -euo pipefail
cd "$(dirname "$0")"
# worker-build must match the worker crate's minor version, so read it from
# Cargo.lock instead of pinning it here (a crate bump alone broke the build).
WORKER_MINOR=$(awk '/^name = "worker"$/ { getline; gsub(/version = |"/, ""); split($0, v, "."); print v[1] "." v[2]; exit }' ../../Cargo.lock)
[ -n "$WORKER_MINOR" ] || { echo "build.sh: worker crate not found in Cargo.lock" >&2; exit 1; }
if ! command -v worker-build >/dev/null 2>&1 || ! worker-build --version 2>/dev/null | grep -q " ${WORKER_MINOR}\."; then
  cargo install -q worker-build --version "~${WORKER_MINOR}" --force
fi
export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=+reference-types"
# --force-enable-abort-handler (default) needs wasm EH + externref and fails
# wasm-bindgen 0.2.127 on current rustc. Panic recovery is optional for ingest.
exec worker-build --release --no-panic-recovery
