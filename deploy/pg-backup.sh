#!/usr/bin/env bash
# deploy/pg-backup.sh — one snapshot-bound, encrypted, catalogued backup.
#
#   deploy/pg-backup.sh --database-url <libpq-url> --repo-root <dir> \
#                       --output-dir <managed-backup-root> [--plaintext]
#
# (or export MUSTER_BACKUP_DATABASE_URL / MUSTER_DATABASE_URL instead of
# --database-url, so the connection string never appears in argv.)
#
# A thin wrapper: the work is `muster-cloud-ops backup run`
# (cloud/src/backup.rs), found as $MUSTER_CLOUD_OPS or on PATH. It needs the
# deployment's MUSTER_CONTROL_DIR and MUSTER_CONTROL_WITNESS (disjointness)
# and MUSTER_BACKUP_AGE_RECIPIENT (age recipient public key; required).
#
#   - First the lifecycle journal is reapplied (authority and incarnation
#     checked) and every workspace past its backup cap is purged; the
#     capture refuses (65) only if one still cannot be purged.
#   - One exported PostgreSQL snapshot: pg_dump and the tenant list come
#     from exactly that state; the Git archive holds only those tenants'
#     <uuid>.git (no root scratch, orphans or later tenants). Per repository,
#     refs/HEAD/config are copied first and objects/ is read afterwards, so
#     a concurrent push never leaves a ref without its objects.
#   - The backup/purge coordination lock is held from the snapshot through
#     the Git archive, so no workspace is captured half-erased.
#   - Encrypted with age while streaming (no plaintext on disk) and hashed;
#     a strict versioned manifest is published last, atomically, under
#     <output-dir>/snapshots/<backup_id>/.
#   - --plaintext maps to --plaintext-synthetic: development
#     (MUSTER_ENV=development) synthetic data only; refused in production.
#   - Retention is `muster-cloud-ops retention run` (deploy/systemd/).
#
# Exit codes (from muster-cloud-ops): 0 ok; 64 usage; 65 refused (no
# recipient, plaintext in production, a tenant past its backup cap); 74 I/O;
# 75 busy (a purge is erasing; retry); 78 configuration.

set -u
set -o pipefail
umask 077

fail() {
  printf 'pg-backup.sh: %s\n' "$1" >&2
  exit "$2"
}

db_url="${MUSTER_BACKUP_DATABASE_URL:-${MUSTER_DATABASE_URL:-}}"
repo_root="${MUSTER_REPO_ROOT:-}"
output_dir="${MUSTER_BACKUP_ROOT:-}"
extra=()

while [ "$#" -gt 0 ]; do
  case "$1" in
    --database-url)
      [ "$#" -ge 2 ] || fail "--database-url needs a value" 64
      db_url=$2; shift 2 ;;
    --repo-root)
      [ "$#" -ge 2 ] || fail "--repo-root needs a value" 64
      repo_root=$2; shift 2 ;;
    --output-dir)
      [ "$#" -ge 2 ] || fail "--output-dir needs a value" 64
      output_dir=$2; shift 2 ;;
    --plaintext)
      extra+=(--plaintext-synthetic); shift ;;
    *)
      fail "unknown argument: $1" 64 ;;
  esac
done

[ -n "$db_url" ] || fail "no --database-url (or MUSTER_BACKUP_DATABASE_URL) given" 64
[ -n "$repo_root" ] || fail "no --repo-root given" 64
[ -n "$output_dir" ] || fail "no --output-dir given" 64
ops_bin=${MUSTER_CLOUD_OPS:-$(command -v muster-cloud-ops 2>/dev/null || true)}
[ -n "$ops_bin" ] && [ -x "$ops_bin" ] \
  || fail "muster-cloud-ops not found (PATH or MUSTER_CLOUD_OPS)" 64

MUSTER_DATABASE_URL=$db_url MUSTER_REPO_ROOT=$repo_root MUSTER_BACKUP_ROOT=$output_dir \
  exec "$ops_bin" backup run "${extra[@]}"
