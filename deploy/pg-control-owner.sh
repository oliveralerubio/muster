#!/usr/bin/env bash
# deploy/pg-control-owner.sh — sourced by pg-restore.sh (never run directly).
#
# pg-restore.sh runs as root with umask 077, and `muster-cloud-ops` (restore-
# finalize, restore-check, status) rewrites the lifecycle authority as root:
#   - the witness is REPLACED (write a temporary file, rename it over
#     witness.json), so the new file is root:root 0600;
#   - control.lock is created root:root 0600 when it does not exist yet;
#   - a crash can leave a `.witness.json.tmp-*` behind;
#   - the journal is only appended to or truncated (its owner is kept).
# The service runs as uid 10001 and could then no longer read the witness (or
# take the lock): it would refuse to start. fix_control_ownership gives every
# one of those files the owner and group of the directory it lives in (the
# directories are the service's: owner 10001, mode 0700) and mode 0600.
#
# Only files in the two directories are touched, never anything else, and never
# through a symbolic link.

# fix_control_ownership CONTROL_DIR WITNESS_FILE — returns 1 (and says which
# file) when a file could not be fixed.
fix_control_ownership() {
  local control_dir=$1 witness=$2 witness_dir wname f rc=0
  witness_dir=$(dirname -- "$witness")
  wname=$(basename -- "$witness")
  for f in "$control_dir/journal.v1.jsonl" "$control_dir/control.lock" "$control_dir"/.*.tmp-* \
    "$witness" "$witness_dir/.$wname".tmp-*; do
    [ -f "$f" ] && [ ! -L "$f" ] || continue
    if ! chown --reference="$(dirname -- "$f")" -- "$f" || ! chmod 0600 -- "$f"; then
      printf 'pg-restore.sh: cannot give %s the owner of its directory\n' "$f" >&2
      rc=1
    fi
  done
  return "$rc"
}
