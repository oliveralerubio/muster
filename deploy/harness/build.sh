#!/usr/bin/env bash
# deploy/harness/build.sh — compile deploy/harness/drill.rs with rustc against
# the previously built `muster` library rlib. Deliberately NO Cargo manifest
# or lock changes: this is a drill harness, not a workspace member.
#
#   deploy/harness/build.sh <output-binary>
#
# Picks the newest libmuster-*.rlib in the shared target dir (override with
# MUSTER_HARNESS_RLIB or CARGO_TARGET_DIR). The exact rlib name is printed so
# the drill records which library version it linked.

set -euo pipefail
umask 077

out=${1:?"usage: deploy/harness/build.sh <output-binary>"}
worktree=$(cd "$(dirname "$0")/../.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$worktree/target}
rlib=${MUSTER_HARNESS_RLIB:-$(ls -t "$target_dir"/debug/deps/libmuster-*.rlib 2>/dev/null | head -1 || true)}
[ -n "$rlib" ] && [ -f "$rlib" ] \
  || { echo "build.sh: no libmuster rlib found under $target_dir/debug/deps" >&2; exit 69; }
rustc_bin=${RUSTC:-$HOME/.cargo/bin/rustc}
[ -x "$rustc_bin" ] || rustc_bin=$(command -v rustc || true)
[ -n "$rustc_bin" ] \
  || { echo "build.sh: rustc not found" >&2; exit 69; }

mkdir -p "$(dirname "$out")"
"$rustc_bin" --edition 2021 \
  "$(dirname "$0")/drill.rs" \
  --extern "muster=$rlib" \
  -L "dependency=$target_dir/debug/deps" \
  -o "$out"
printf 'build.sh: linked %s -> %s\n' "$(basename "$rlib")" "$out"
