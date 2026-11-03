#!/usr/bin/env bash
# Smoke-run every fuzz target for a few iterations (needs nightly + cargo-fuzz).
# Usage: scripts/fuzz_smoke.sh [runs]   (default 200)
set -euo pipefail
cd "$(dirname "$0")/.."
runs="${1:-200}"
if ! cargo fuzz --help >/dev/null 2>&1; then
  echo "cargo-fuzz not installed: cargo install cargo-fuzz (requires nightly)" >&2
  exit 2
fi
for target in $(cargo +nightly fuzz list); do
  echo "== $target ($runs runs)"
  cargo +nightly fuzz run "$target" -- -runs="$runs" -max_total_time=60
done
