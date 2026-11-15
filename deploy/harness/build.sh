#!/usr/bin/env bash
# Build the optional backup drill helper through Cargo so dependency paths
# and crate versions come from the workspace's locked dependency graph.
#
#   deploy/harness/build.sh <output-binary>
#
# Normal service builds do not enable this helper's backup-drill feature.

set -euo pipefail
umask 077

out=${1:?"usage: deploy/harness/build.sh <output-binary>"}
worktree=$(cd "$(dirname "$0")/../.." && pwd)
target_dir=${CARGO_TARGET_DIR:-$worktree/target}
export CARGO_TARGET_DIR="$target_dir"
cargo build --locked --manifest-path "$worktree/Cargo.toml" \
  -p muster-cloud --features backup-drill --bin muster-cloud-drill

mkdir -p "$(dirname "$out")"
install -m 0700 "$target_dir/debug/muster-cloud-drill" "$out"
printf 'build.sh: built muster-cloud-drill -> %s\n' "$out"
