#!/usr/bin/env bash
# deploy/pg-restore.sh — restore a pg-backup.sh backup into NEW targets only.
#
#   deploy/pg-restore.sh --backup <backup-dir> \
#                        --admin-url <libpq-url-to-existing-maintenance-db> \
#                        --new-db <name> --new-repo-root <dir> \
#                        [--age-identity-file <path>]
#
# (or export MUSTER_RESTORE_ADMIN_URL instead of --admin-url so a password in
# the URL never appears in argv on multi-user hosts.)
#
# Safety contract:
#   - Only ever creates NEW resources. Refuses (exit 66) if <new-db> already
#     exists on the target cluster or <new-repo-root> already exists and is
#     not empty. The ACTIVE database and repo root are never connected to for
#     writes and never modified.
#   - Pre-cutover check: after pg_restore, every head recorded in the restored
#     workspace_heads must exist in the restored git store AND be reachable
#     from its refs/muster/uploads/* (the service's durable order means PG is
#     the authority; a head without its Git objects must abort the cutover).
#     Every restored repo passes `git fsck`. Any failure tears down the
#     targets created in this run (drop <new-db>, remove <new-repo-root>).
#   - Cutover itself is a separate, manual step: point MUSTER_DATABASE_URL and
#     MUSTER_REPO_ROOT of a NEW service instance at these targets. This script
#     never restarts or reconfigures any service.
#   - Encrypted backups (age) are decrypted with the identity in
#     --age-identity-file (a PATH, never a secret value); plaintext backups
#     restore as-is.
#   - Prints only target names and counts; never connection strings.
#
# Exit codes: 64 usage; 66 target already exists; 67 invalid/missing backup;
# 68 pg_restore failed; 69 git fsck failed; 70 head-presence check failed;
# 71 age decrypt failed / identity missing; 72 age missing for encrypted
# backup.

set -u
set -o pipefail
umask 077

# shellcheck source=pg-conn.sh
. "$(dirname "$0")/pg-conn.sh"

readonly PG_RESTORE_EXIT_USAGE=64
readonly PG_RESTORE_EXIT_EXISTS=66
readonly PG_RESTORE_EXIT_BACKUP=67
readonly PG_RESTORE_EXIT_PGRESTORE=68
readonly PG_RESTORE_EXIT_FSCK=69
readonly PG_RESTORE_EXIT_HEAD=70
readonly PG_RESTORE_EXIT_DECRYPT=71
readonly PG_RESTORE_EXIT_NOAGE=72

fail() {
  printf 'pg-restore.sh: %s\n' "$1" >&2
  exit "$2"
}

backup_dir=""
admin_url=""
new_db=""
new_repo_root=""
age_identity=""

while [ "$#" -gt 0 ]; do
  case "$1" in
    --backup)
      [ "$#" -ge 2 ] || fail "--backup needs a value" "$PG_RESTORE_EXIT_USAGE"
      backup_dir=$2; shift 2 ;;
    --admin-url)
      [ "$#" -ge 2 ] || fail "--admin-url needs a value" "$PG_RESTORE_EXIT_USAGE"
      admin_url=$2; shift 2 ;;
    --new-db)
      [ "$#" -ge 2 ] || fail "--new-db needs a value" "$PG_RESTORE_EXIT_USAGE"
      new_db=$2; shift 2 ;;
    --new-repo-root)
      [ "$#" -ge 2 ] || fail "--new-repo-root needs a value" "$PG_RESTORE_EXIT_USAGE"
      new_repo_root=$2; shift 2 ;;
    --age-identity-file)
      [ "$#" -ge 2 ] || fail "--age-identity-file needs a value" "$PG_RESTORE_EXIT_USAGE"
      age_identity=$2; shift 2 ;;
    *)
      fail "unknown argument: $1" "$PG_RESTORE_EXIT_USAGE" ;;
  esac
done

[ -n "$admin_url" ] || admin_url="${MUSTER_RESTORE_ADMIN_URL:-}"
[ -n "$backup_dir" ] || fail "no --backup given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$admin_url" ] || fail "no --admin-url (or MUSTER_RESTORE_ADMIN_URL) given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$new_db" ] || fail "no --new-db given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$new_repo_root" ] || fail "no --new-repo-root given" "$PG_RESTORE_EXIT_USAGE"
[ -d "$backup_dir" ] || fail "backup dir does not exist: $backup_dir" "$PG_RESTORE_EXIT_BACKUP"

# The database name becomes part of SQL literals and libpq arguments: accept
# only safe lowercase identifiers (what PostgreSQL folds unquoted names to),
# so quoting stays trivial and quoting/injection bugs are structurally
# impossible rather than merely avoided.
case $new_db in
  [a-z_]*)
    bad_name=""
    case $new_db in
      *[!a-z0-9_$]*) bad_name=1 ;;
    esac
    [ -n "$bad_name" ] && fail \
      "invalid target database name '$new_db': use lowercase letters, digits, '_' or '\$', starting with a letter or '_'" \
      "$PG_RESTORE_EXIT_USAGE"
    ;;
  *)
    fail "invalid target database name '$new_db': use lowercase letters, digits, '_' or '\$', starting with a letter or '_'" \
      "$PG_RESTORE_EXIT_USAGE"
    ;;
esac

command -v psql >/dev/null 2>&1 || fail "psql not found" "$PG_RESTORE_EXIT_USAGE"
command -v createdb >/dev/null 2>&1 || fail "createdb not found" "$PG_RESTORE_EXIT_USAGE"
command -v pg_restore >/dev/null 2>&1 || fail "pg_restore not found" "$PG_RESTORE_EXIT_USAGE"
command -v git >/dev/null 2>&1 || fail "git not found" "$PG_RESTORE_EXIT_USAGE"

[ -f "$backup_dir/manifest.json" ] \
  || fail "backup is incomplete (no manifest.json): $backup_dir" "$PG_RESTORE_EXIT_BACKUP"

# Artifacts, encrypted or not, per the manifest's claim.
if grep -q '"encrypted": true' "$backup_dir/manifest.json"; then
  age_bin=$(command -v age 2>/dev/null || true)
  [ -n "$age_bin" ] \
    || fail "backup is age-encrypted but the age CLI is not installed" \
      "$PG_RESTORE_EXIT_NOAGE"
  [ -n "$age_identity" ] && [ -f "$age_identity" ] \
    || fail "encrypted backup needs --age-identity-file pointing at an identity file" \
      "$PG_RESTORE_EXIT_DECRYPT"
  work=$(mktemp -d "${TMPDIR:-/tmp}/muster-restore.XXXXXX") \
    || fail "mktemp failed" "$PG_RESTORE_EXIT_USAGE"
  chmod 0700 "$work"
  age --decrypt -i "$age_identity" -o "$work/db.dump" "$backup_dir/db.dump.age" \
    && age --decrypt -i "$age_identity" -o "$work/git.tar" "$backup_dir/git.tar.age" \
    || { rm -rf -- "$work"; fail "age decryption failed" "$PG_RESTORE_EXIT_DECRYPT"; }
  db_dump=$work/db.dump
  git_tar=$work/git.tar
else
  [ -s "$backup_dir/db.dump" ] \
    || fail "backup has no db.dump" "$PG_RESTORE_EXIT_BACKUP"
  [ -s "$backup_dir/git.tar" ] \
    || fail "backup has no git.tar" "$PG_RESTORE_EXIT_BACKUP"
  db_dump=$backup_dir/db.dump
  git_tar=$backup_dir/git.tar
  work=""
fi

# Never touch anything that exists: this run creates only new resources.
pg_conn_export "$admin_url" \
  || fail "invalid admin connection URL" "$PG_RESTORE_EXIT_USAGE"
if psql -d "$PGDATABASE" -Atc "SELECT 1 FROM pg_database WHERE datname='$new_db'" | grep -q 1; then
  fail "target database already exists; refusing to overwrite: $new_db" \
    "$PG_RESTORE_EXIT_EXISTS"
fi
if [ -e "$new_repo_root" ] && [ -n "$(ls -A "$new_repo_root" 2>/dev/null)" ]; then
  fail "target repo root already exists and is not empty; refusing to overwrite: $new_repo_root" \
    "$PG_RESTORE_EXIT_EXISTS"
fi

# Targets this run owns; torn down if any step below fails.
created_db=""
created_root=""

teardown_fail() {
  if [ -n "$created_db" ]; then
    dropdb --if-exists "$created_db" >/dev/null 2>&1 \
      || printf 'pg-restore.sh: warning: could not drop created database %s\n' "$created_db" >&2
  fi
  if [ -n "$created_root" ] && [ -d "$created_root" ]; then
    rm -rf -- "$created_root" \
      || printf 'pg-restore.sh: warning: could not remove created repo root %s\n' "$created_root" >&2
  fi
  [ -z "$work" ] || rm -rf -- "$work"
}

if [ -e "$new_repo_root" ]; then
  mkdir -m 0700 -- "$new_repo_root" \
    || { teardown_fail; fail "cannot use repo root: $new_repo_root" "$PG_RESTORE_EXIT_USAGE"; }
else
  mkdir -m 0700 -p -- "$new_repo_root" \
    || { teardown_fail; fail "cannot create repo root: $new_repo_root" "$PG_RESTORE_EXIT_USAGE"; }
fi
created_root=$new_repo_root

createdb "$new_db" || { teardown_fail; fail "cannot create database $new_db" "$PG_RESTORE_EXIT_PGRESTORE"; }
created_db=$new_db

# Untar first (restore order does not affect the consistent cut: both
# artifacts are complete files from one pg-backup.sh run).
tar -xf "$git_tar" -C "$new_repo_root" \
  || { teardown_fail; fail "git.tar extraction failed" "$PG_RESTORE_EXIT_BACKUP"; }
find "$new_repo_root" -type d -exec chmod go-rwx {} + 2>/dev/null

pg_restore --no-owner --dbname="$new_db" "$db_dump" \
  || { teardown_fail; fail "pg_restore failed" "$PG_RESTORE_EXIT_PGRESTORE"; }

# Pre-cutover integrity gate: PG-referenced heads must exist in the restored
# git store, reachable from upload refs; every restored repo must fsck clean.
heads=$(psql -d "$new_db" -Atc \
  "SELECT workspace_id || ' ' || head_revision FROM workspace_heads") \
  || { teardown_fail; fail "cannot read restored workspace_heads" "$PG_RESTORE_EXIT_PGRESTORE"; }

heads_checked=0
repos_fscked=0
failed=""
if [ -n "$heads" ]; then
  while read -r ws head; do
    [ -n "$ws" ] || continue
    heads_checked=$((heads_checked + 1))
    repo=$new_repo_root/$ws.git
    if [ ! -f "$repo/HEAD" ]; then
      printf 'pg-restore.sh: FAIL workspace %s: no git repo in restored store\n' "$ws" >&2
      failed=1
      continue
    fi
    if ! git -C "$repo" cat-file -e "$head^{commit}" 2>/dev/null; then
      printf 'pg-restore.sh: FAIL workspace %s: head %s object missing\n' "$ws" "$head" >&2
      failed=1
      continue
    fi
    if ! git -C "$repo" for-each-ref --format='%(refname)' refs/muster/uploads/ \
      | grep -q .; then
      printf 'pg-restore.sh: FAIL workspace %s: no upload refs restored\n' "$ws" >&2
      failed=1
      continue
    fi
    upload_refs=$(git -C "$repo" for-each-ref --format='%(refname)' refs/muster/uploads/)
    if ! printf '%s\n' "$upload_refs" \
      | xargs git -C "$repo" rev-list \
      | grep -qx "$head"; then
      printf 'pg-restore.sh: FAIL workspace %s: head %s not reachable from upload refs\n' "$ws" "$head" >&2
      failed=1
      continue
    fi
  done <<HEADS
$heads
HEADS
fi
[ -z "$failed" ] || { teardown_fail; fail "head-presence check failed; targets torn down" "$PG_RESTORE_EXIT_HEAD"; }

for repo in "$new_repo_root"/*.git; do
  [ -d "$repo" ] || continue
  repos_fscked=$((repos_fscked + 1))
  git -C "$repo" fsck --no-progress --no-dangling >/dev/null 2>&1 \
    || { teardown_fail; printf 'pg-restore.sh: FAIL git fsck: %s\n' "$repo" >&2; \
         fail "restored repo failed git fsck; targets torn down" "$PG_RESTORE_EXIT_FSCK"; }
done

[ -z "$work" ] || rm -rf -- "$work"

printf 'pg-restore.sh: restore complete (NEW resources only; cutover is manual)\n'
printf 'pg-restore.sh: database=%s repo_root=%s heads_checked=%s repos_fscked=%s\n' \
  "$new_db" "$new_repo_root" "$heads_checked" "$repos_fscked"
printf 'pg-restore.sh: next step (manual): point a NEW service instance at these targets via\n'
printf 'pg-restore.sh: MUSTER_DATABASE_URL and MUSTER_REPO_ROOT; never repoint the active one directly.\n'
