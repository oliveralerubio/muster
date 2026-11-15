#!/usr/bin/env bash
# deploy/pg-backup.sh — encrypted, consistent-cut backup of Cloud data.
#
#   deploy/pg-backup.sh --database-url <libpq-url> --repo-root <dir> \
#                       --output-dir <dir> [--plaintext]
#
# (or export MUSTER_BACKUP_DATABASE_URL instead of --database-url, so the
# connection string never appears in argv on multi-user hosts.)
#
# What it does:
#   1. pg_dump (custom format) of the service database into db.dump.
#   2. tar of MUSTER_REPO_ROOT (one bare git repo per workspace) into git.tar,
#      excluding transient git files (*.lock, *.tmp, tmp_obj_*).
#   3. manifest.json written LAST: a backup without it is incomplete.
#
# Consistent cut (why the order matters): pg_dump completes FIRST. Every
# accepted head the dump captured was committed to PostgreSQL before the dump
# snapshot was taken, and the service's durability order (cloud/src/repo_store.rs)
# writes Git objects plus the immutable refs/muster/uploads/<id> BEFORE the
# PostgreSQL transaction commits. So when the tar runs afterwards, every object
# any captured head references is already on disk. Upload refs are never deleted
# and gc.auto=0 in service repos, so nothing captured can be GC'd in between.
# Concurrent writers during the tar can only add objects (uploads are
# immutable); mid-write transient files are excluded and restore-side checks
# (pg_restore validity, git fsck, per-head presence) catch real corruption.
#
# Encryption: with `age` installed and MUSTER_BACKUP_AGE_RECIPIENT set to a
# recipient public key, db.dump and git.tar are encrypted (age -r) and the
# plaintext intermediates are removed. Without age, the script refuses to run
# unless --plaintext is passed explicitly: that produces an UNENCRYPTED backup
# (workspace content is not end-to-end encrypted by design; see ADR 0006) and
# is acceptable only for synthetic/dev data. The recipient key is public, but
# it is read from the environment, never logged.
#
# Never overwrites anything: each run creates a NEW muster-backup-<UTC>
# directory (0700) and fails if that exact path already exists. The source
# database and repo root are only ever read. Retention/purge of old backups is
# a policy decision NOT implemented here — old backup directories accumulate
# until the operator or a documented scheduler removes them.
#
# Exit codes: 64 usage; 65 plaintext refused; 66 target path exists;
# 67 pg_dump failed; 68 tar failed; 69 age recipient configured but age
# missing; 70 encryption failed.

set -u
set -o pipefail
umask 077

# shellcheck source=pg-conn.sh
. "$(dirname "$0")/pg-conn.sh"

readonly PG_BACKUP_EXIT_USAGE=64
readonly PG_BACKUP_EXIT_PLAINTEXT=65
readonly PG_BACKUP_EXIT_EXISTS=66
readonly PG_BACKUP_EXIT_DUMP=67
readonly PG_BACKUP_EXIT_TAR=68
readonly PG_BACKUP_EXIT_NOAGE=69
readonly PG_BACKUP_EXIT_ENCRYPT=70

fail() {
  printf 'pg-backup.sh: %s\n' "$1" >&2
  exit "$2"
}

db_url=""
repo_root=""
output_dir=""
plaintext=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --database-url)
      [ "$#" -ge 2 ] || fail "--database-url needs a value" "$PG_BACKUP_EXIT_USAGE"
      db_url=$2; shift 2 ;;
    --repo-root)
      [ "$#" -ge 2 ] || fail "--repo-root needs a value" "$PG_BACKUP_EXIT_USAGE"
      repo_root=$2; shift 2 ;;
    --output-dir)
      [ "$#" -ge 2 ] || fail "--output-dir needs a value" "$PG_BACKUP_EXIT_USAGE"
      output_dir=$2; shift 2 ;;
    --plaintext)
      plaintext=1; shift ;;
    *)
      fail "unknown argument: $1" "$PG_BACKUP_EXIT_USAGE" ;;
  esac
done

[ -n "$db_url" ] || db_url="${MUSTER_BACKUP_DATABASE_URL:-}"
[ -n "$db_url" ] || fail "no --database-url (or MUSTER_BACKUP_DATABASE_URL) given" "$PG_BACKUP_EXIT_USAGE"
[ -n "$repo_root" ] || fail "no --repo-root given" "$PG_BACKUP_EXIT_USAGE"
[ -n "$output_dir" ] || fail "no --output-dir given" "$PG_BACKUP_EXIT_USAGE"
[ -d "$repo_root" ] || fail "repo root is not a directory: $repo_root" "$PG_BACKUP_EXIT_USAGE"

command -v pg_dump >/dev/null 2>&1 \
  || fail "pg_dump not found in PATH" "$PG_BACKUP_EXIT_USAGE"
command -v tar >/dev/null 2>&1 \
  || fail "tar not found in PATH" "$PG_BACKUP_EXIT_USAGE"

# Encryption decision. Refuse plaintext unless explicitly allowed: without it
# a forgotten flag would silently write workspace history in the clear.
age_bin=$(command -v age 2>/dev/null || true)
age_recipient=${MUSTER_BACKUP_AGE_RECIPIENT:-}
encrypted=""
if [ -n "$age_recipient" ] && [ -z "$age_bin" ]; then
  fail "MUSTER_BACKUP_AGE_RECIPIENT is set but the age CLI is not installed" \
    "$PG_BACKUP_EXIT_NOAGE"
fi
if [ -n "$age_bin" ] && [ -n "$age_recipient" ] && [ -z "$plaintext" ]; then
  encrypted=1
fi
if [ -z "$encrypted" ] && [ -z "$plaintext" ]; then
  printf 'pg-backup.sh: refusing to write an unencrypted backup.\n' >&2
  printf 'pg-backup.sh: install age and set MUSTER_BACKUP_AGE_RECIPIENT (recipient public key),\n' >&2
  printf 'pg-backup.sh: or pass --plaintext explicitly for synthetic/dev data only.\n' >&2
  exit "$PG_BACKUP_EXIT_PLAINTEXT"
fi
if [ -n "$plaintext" ] && [ -n "$age_bin" ] && [ -n "$age_recipient" ]; then
  printf 'pg-backup.sh: WARNING: --plaintext overrides configured age encryption\n' >&2
fi

# Database name only (path component of the URL); the full connection string
# is never printed or written to the manifest.
db_name=$(pg_conn_dbname "$db_url")

timestamp=$(date -u +%Y%m%dT%H%M%SZ)
backup_dir=$output_dir/muster-backup-$timestamp
if [ -e "$backup_dir" ]; then
  fail "backup path already exists (rerun within the same second?): $backup_dir" \
    "$PG_BACKUP_EXIT_EXISTS"
fi
mkdir -m 0700 -p -- "$output_dir" \
  || fail "cannot create output dir: $output_dir" "$PG_BACKUP_EXIT_USAGE"
mkdir -m 0700 -- "$backup_dir" \
  || fail "cannot create backup dir: $backup_dir" "$PG_BACKUP_EXIT_EXISTS"

cleanup_fail() {
  # We created this dir moments ago and verified it did not exist before;
  # remove the partial artifacts so nothing incomplete is ever consumed.
  rm -rf -- "$backup_dir"
}

# 1) Database first: freezes the accepted-head set for the consistent cut.
# pg_conn_export moves the connection string (and any password) from argv
# into the PG* environment variables before invoking pg_dump.
pg_conn_export "$db_url" \
  || { cleanup_fail; fail "invalid connection URL" "$PG_BACKUP_EXIT_USAGE"; }
pg_dump --format=custom --file="$backup_dir/db.dump" \
  || { cleanup_fail; fail "pg_dump failed" "$PG_BACKUP_EXIT_DUMP"; }
unset PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE PGSSLMODE
[ -s "$backup_dir/db.dump" ] \
  || { cleanup_fail; fail "pg_dump produced an empty file" "$PG_BACKUP_EXIT_DUMP"; }

# 2) Git objects second (see the consistent-cut note above). Transient files
# excluded; immutable objects and upload refs are always included.
tar --exclude='*.lock' --exclude='*.tmp' --exclude='tmp_obj_*' \
  -C "$repo_root" -cf "$backup_dir/git.tar" . \
  || { cleanup_fail; fail "tar of repo root failed" "$PG_BACKUP_EXIT_TAR"; }
[ -s "$backup_dir/git.tar" ] \
  || { cleanup_fail; fail "tar produced an empty file" "$PG_BACKUP_EXIT_TAR"; }

# 3) Encrypt when configured: age the artifacts, then remove plaintext.
suffix=""
if [ -n "$encrypted" ]; then
  for f in db.dump git.tar; do
    age -r "$age_recipient" <"$backup_dir/$f" >"$backup_dir/$f.age" \
      || { cleanup_fail; fail "age encryption of $f failed" "$PG_BACKUP_EXIT_ENCRYPT"; }
    rm -f -- "$backup_dir/$f"
  done
  suffix=".age"
fi

# 4) Manifest last: its presence marks the backup as complete.
git_version=$(git --version 2>/dev/null || echo unknown)
pg_version=$(pg_dump --version 2>/dev/null || echo unknown)
db_dump_name="db.dump$suffix"
git_tar_name="git.tar$suffix"
{
  printf '{\n'
  printf '  "tool": "deploy/pg-backup.sh",\n'
  printf '  "format_version": 1,\n'
  printf '  "created_at": "%s",\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf '  "encrypted": %s,\n' "$([ -n "$encrypted" ] && echo true || echo false)"
  printf '  "database": "%s",\n' "$db_name"
  printf '  "repo_root": "%s",\n' "$repo_root"
  printf '  "pg_dump_version": "%s",\n' "$pg_version"
  printf '  "git_version": "%s",\n' "$git_version"
  printf '  "artifacts": { "db_dump": "%s", "git_tar": "%s" },\n' "$db_dump_name" "$git_tar_name"
  printf '  "ordering": "pg_dump first, then git tar (consistent cut; see script header)",\n'
  printf '  "retention": "not implemented: old backup directories are never removed by this tool"\n'
  printf '}\n'
} >"$backup_dir/manifest.json" || {
  cleanup_fail
  fail "cannot write manifest" "$PG_BACKUP_EXIT_USAGE"
}

printf 'pg-backup.sh: backup complete: %s (%s)\n' \
  "$backup_dir" "$([ -n "$encrypted" ] && echo encrypted || echo 'PLAINTEXT — dev/synthetic data only')"
printf 'pg-backup.sh: database=%s repo_root=%s\n' "$db_name" "$repo_root"
