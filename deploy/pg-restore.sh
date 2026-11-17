#!/usr/bin/env bash
# deploy/pg-restore.sh — restore a pg-backup.sh backup into NEW targets only.
#
#   deploy/pg-restore.sh --backup <backup-dir> \
#                        --admin-url <libpq-url-to-existing-maintenance-db> \
#                        --new-db <name> --new-repo-root <dir> \
#                        --control-dir <dir> --control-witness <file> \
#                        [--age-identity-file <path>] [--legacy-format1]
#
# (or export MUSTER_RESTORE_ADMIN_URL instead of --admin-url so a password in
# the URL never appears in argv on multi-user hosts; MUSTER_CONTROL_DIR and
# MUSTER_CONTROL_WITNESS may replace the two control flags.)
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
#   - Lifecycle authority (cloud/src/control.rs, via `muster-cloud-ops`, found
#     as $MUSTER_CLOUD_OPS or on PATH): the deployment's EXISTING control
#     store and witness are required. Before anything is created, the store
#     must open cleanly (missing, rolled-back, divergent or foreign journal or
#     witness: exit 73). After pg_restore, `muster-cloud-ops restore-check`
#     (read-only on the restored database) must accept the restored binding:
#     same deployment and store, not ahead of the journal, not diverging from
#     it; otherwise the targets are torn down (exit 73). This script never
#     creates or initialises an authority. A backup taken before the
#     authority existed has no binding and is refused.
#   - The restored database keeps the incarnation of its source. Promoting
#     it to a new incarnation (and fencing the source) is the cutover step of
#     the restore unit; until then, never serve source and restore at once.
#   - Cutover itself is a separate, manual step: point MUSTER_DATABASE_URL and
#     MUSTER_REPO_ROOT of a NEW service instance at these targets. This script
#     never restarts or reconfigures any service.
#   - Encrypted backups (age) are decrypted with the identity in
#     --age-identity-file (a PATH, never a secret value), streaming into tar
#     and pg_restore: no plaintext copy on disk. A format 2 snapshot
#     (muster-cloud-ops backup run) is verified first: strict manifest and
#     every artifact hash, and verification is mandatory: anything it
#     rejects is refused (67) before any target exists. Legacy format 1
#     directories restore only with the explicit --legacy-format1.
#     Synthetic plaintext snapshots restore as-is.
#   - Prints only target names and counts; never connection strings.
#
# Exit codes: 64 usage; 66 target already exists; 67 invalid/missing backup;
# 68 pg_restore failed; 69 git fsck failed; 70 head-presence check failed;
# 71 age decrypt failed / identity missing; 72 age missing for encrypted
# backup; 73 lifecycle authority refused (missing/stale/foreign/ahead).

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
readonly PG_RESTORE_EXIT_AUTHORITY=73

fail() {
  printf 'pg-restore.sh: %s\n' "$1" >&2
  exit "$2"
}

backup_dir=""
admin_url=""
new_db=""
new_repo_root=""
age_identity=""
legacy_format1=""
control_dir="${MUSTER_CONTROL_DIR:-}"
control_witness="${MUSTER_CONTROL_WITNESS:-}"

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
    --control-dir)
      [ "$#" -ge 2 ] || fail "--control-dir needs a value" "$PG_RESTORE_EXIT_USAGE"
      control_dir=$2; shift 2 ;;
    --control-witness)
      [ "$#" -ge 2 ] || fail "--control-witness needs a value" "$PG_RESTORE_EXIT_USAGE"
      control_witness=$2; shift 2 ;;
    --age-identity-file)
      [ "$#" -ge 2 ] || fail "--age-identity-file needs a value" "$PG_RESTORE_EXIT_USAGE"
      age_identity=$2; shift 2 ;;
    --legacy-format1)
      legacy_format1=1; shift ;;
    *)
      fail "unknown argument: $1" "$PG_RESTORE_EXIT_USAGE" ;;
  esac
done

[ -n "$admin_url" ] || admin_url="${MUSTER_RESTORE_ADMIN_URL:-}"
[ -n "$backup_dir" ] || fail "no --backup given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$admin_url" ] || fail "no --admin-url (or MUSTER_RESTORE_ADMIN_URL) given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$new_db" ] || fail "no --new-db given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$new_repo_root" ] || fail "no --new-repo-root given" "$PG_RESTORE_EXIT_USAGE"
[ -n "$control_dir" ] && [ -n "$control_witness" ] \
  || fail "the deployment's lifecycle authority is required: --control-dir and --control-witness (or MUSTER_CONTROL_DIR/MUSTER_CONTROL_WITNESS)" "$PG_RESTORE_EXIT_USAGE"
ops_bin=${MUSTER_CLOUD_OPS:-$(command -v muster-cloud-ops 2>/dev/null || true)}
[ -n "$ops_bin" ] && [ -x "$ops_bin" ] \
  || fail "muster-cloud-ops not found (PATH or MUSTER_CLOUD_OPS)" "$PG_RESTORE_EXIT_USAGE"
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

# The authority must open cleanly before anything is created. `ops` exit
# 65 is a refusal; 78 a configuration error (e.g. overlapping paths).
authority_ops() {
  MUSTER_CONTROL_DIR=$control_dir MUSTER_CONTROL_WITNESS=$control_witness \
    MUSTER_REPO_ROOT=$new_repo_root "$ops_bin" "$@"
}
authority_out=$(authority_ops status 2>&1 >/dev/null)
authority_rc=$?
case $authority_rc in
  0) ;;
  65) fail "lifecycle authority refused: ${authority_out#muster-cloud-ops: }" "$PG_RESTORE_EXIT_AUTHORITY" ;;
  78|64) fail "lifecycle authority configuration: ${authority_out#muster-cloud-ops: }" "$PG_RESTORE_EXIT_USAGE" ;;
  *) fail "lifecycle authority check failed (rc=$authority_rc): ${authority_out#muster-cloud-ops: }" "$PG_RESTORE_EXIT_AUTHORITY" ;;
esac

# Artifacts, after the authority opened cleanly. Every snapshot is verified
# by `muster-cloud-ops backup verify` (strict format 2 manifest, every
# artifact hash); anything it rejects is refused (67). Legacy format 1
# directories (old pg-backup.sh, no hashes) restore ONLY with the explicit
# --legacy-format1. Either way each artifact is decrypted while streaming
# straight into tar / pg_restore (no plaintext on disk).
age_needed=""
if [ -z "$legacy_format1" ]; then
  verify_err_file=$(mktemp "${TMPDIR:-/tmp}/muster-restore-verify.XXXXXX") \
    || fail "mktemp failed" "$PG_RESTORE_EXIT_USAGE"
  verify_json=$(MUSTER_CONTROL_DIR=$control_dir MUSTER_CONTROL_WITNESS=$control_witness \
    MUSTER_REPO_ROOT=$new_repo_root "$ops_bin" backup verify --snapshot "$backup_dir" \
    2>"$verify_err_file")
  verify_rc=$?
  verify_err=$(cat "$verify_err_file" 2>/dev/null); rm -f -- "$verify_err_file"
  [ "$verify_rc" = 0 ] \
    || fail "backup failed verification: ${verify_err#muster-cloud-ops: }" "$PG_RESTORE_EXIT_BACKUP"
  case $verify_json in
    *'"scheme":"age-x25519"'*)
      age_needed=1
      db_dump=$backup_dir/db.dump.age
      git_tar=$backup_dir/git.tar.age ;;
    *'"scheme":"none"'*)
      db_dump=$backup_dir/db.dump
      git_tar=$backup_dir/git.tar ;;
    *) fail "backup has an unknown encryption scheme" "$PG_RESTORE_EXIT_BACKUP" ;;
  esac
elif ! grep -q '"format_version": 1' "$backup_dir/manifest.json"; then
  fail "--legacy-format1 given but the manifest is not format 1" "$PG_RESTORE_EXIT_BACKUP"
elif grep -q '"encrypted": true' "$backup_dir/manifest.json"; then
  age_needed=1
  db_dump=$backup_dir/db.dump.age
  git_tar=$backup_dir/git.tar.age
else
  [ -s "$backup_dir/db.dump" ] \
    || fail "backup has no db.dump" "$PG_RESTORE_EXIT_BACKUP"
  [ -s "$backup_dir/git.tar" ] \
    || fail "backup has no git.tar" "$PG_RESTORE_EXIT_BACKUP"
  db_dump=$backup_dir/db.dump
  git_tar=$backup_dir/git.tar
fi
if [ -n "$age_needed" ]; then
  age_bin=$(command -v age 2>/dev/null || true)
  [ -n "$age_bin" ] \
    || fail "backup is age-encrypted but the age CLI is not installed" \
      "$PG_RESTORE_EXIT_NOAGE"
  [ -n "$age_identity" ] && [ -f "$age_identity" ] \
    || fail "encrypted backup needs --age-identity-file pointing at an identity file" \
      "$PG_RESTORE_EXIT_DECRYPT"
fi

# Streams an artifact (decrypted when encrypted) into a reader command.
# Returns the reader's status, or 171 when decryption itself failed.
stream_into() {
  local artifact=$1
  shift
  if [ -n "$age_needed" ]; then
    age --decrypt -i "$age_identity" "$artifact" | "$@"
    local st=("${PIPESTATUS[@]}")
    [ "${st[0]}" = 0 ] || return 171
    return "${st[1]}"
  fi
  "$@" <"$artifact"
}

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
stream_into "$git_tar" tar -xf - -C "$new_repo_root"
tar_rc=$?
[ "$tar_rc" = 171 ] && { teardown_fail; fail "age decryption of the Git archive failed" "$PG_RESTORE_EXIT_DECRYPT"; }
[ "$tar_rc" = 0 ] || { teardown_fail; fail "git.tar extraction failed" "$PG_RESTORE_EXIT_BACKUP"; }
find "$new_repo_root" -type d -exec chmod go-rwx {} + 2>/dev/null

stream_into "$db_dump" pg_restore --no-owner --dbname="$new_db"
restore_rc=$?
[ "$restore_rc" = 171 ] && { teardown_fail; fail "age decryption of the database dump failed" "$PG_RESTORE_EXIT_DECRYPT"; }
[ "$restore_rc" = 0 ] || { teardown_fail; fail "pg_restore failed" "$PG_RESTORE_EXIT_PGRESTORE"; }

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
    # Capture ref lists instead of piping into `grep -q` / early-exiting readers:
    # under pipefail, an early reader exit SIGPIPEs the producer (141) and a
    # reachable head would be reported unreachable.
    upload_refs=$(git -C "$repo" for-each-ref --format='%(refname)' refs/muster/uploads/) \
      || upload_refs=""
    if [ -z "$upload_refs" ]; then
      printf 'pg-restore.sh: FAIL workspace %s: no upload refs restored\n' "$ws" >&2
      failed=1
      continue
    fi
    reachable=""
    while IFS= read -r ref; do
      [ -n "$ref" ] || continue
      git -C "$repo" merge-base --is-ancestor "$head" "$ref"
      ancestor_rc=$?
      if [ "$ancestor_rc" -eq 0 ]; then
        reachable=1
        break
      elif [ "$ancestor_rc" -ne 1 ]; then
        teardown_fail
        fail "cannot check reachability of head $head from $ref for workspace $ws" "$PG_RESTORE_EXIT_HEAD"
      fi
    done <<REFS
$upload_refs
REFS
    if [ -z "$reachable" ]; then
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

# Restore gate: the restored binding must belong to this authority and not
# be ahead of or diverge from its journal (read-only on the restored db).
new_db_url=$(pg_conn_with_db "$admin_url" "$new_db")
gate_err_file=$(mktemp "${TMPDIR:-/tmp}/muster-restore-gate.XXXXXX") \
  || { teardown_fail; fail "mktemp failed" "$PG_RESTORE_EXIT_USAGE"; }
gate_json=$(MUSTER_DATABASE_URL=$new_db_url authority_ops restore-check 2>"$gate_err_file")
gate_rc=$?
gate_err=$(cat "$gate_err_file" 2>/dev/null); rm -f -- "$gate_err_file"
unset new_db_url
if [ "$gate_rc" != 0 ]; then
  teardown_fail
  fail "lifecycle authority refused the restored database (targets torn down): ${gate_err#muster-cloud-ops: }" \
    "$PG_RESTORE_EXIT_AUTHORITY"
fi

printf 'pg-restore.sh: authority verified: %s\n' "$gate_json"
printf 'pg-restore.sh: restore complete (NEW resources only; cutover is manual)\n'
printf 'pg-restore.sh: database=%s repo_root=%s heads_checked=%s repos_fscked=%s\n' \
  "$new_db" "$new_repo_root" "$heads_checked" "$repos_fscked"
printf 'pg-restore.sh: next step (manual): point a NEW service instance at these targets via\n'
printf 'pg-restore.sh: MUSTER_DATABASE_URL and MUSTER_REPO_ROOT; never repoint the active one directly.\n'
