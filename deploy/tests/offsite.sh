#!/usr/bin/env bash
# deploy/tests/offsite.sh — tests of `muster-cloud-offsite` (upload, and above
# all the deletions that retention recorded) with a fake rclone whose "remote"
# is a local directory; no root, no network and no real Google Drive needed
# (python3 and flock are).
#
#   deploy/tests/offsite.sh
#
# The cases that matter: with Drive full the replacement upload fails while a
# purge would succeed, and that must never delete the last recoverable offsite
# snapshot (the audit stays unprocessed and the run fails, so it alerts).
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_OFFSITE_SCRIPT:-$REPO/deploy/kamal/muster-cloud-offsite}
OLD=20261101T000000Z-aaaaaaaa
MID=20261117T000000Z-cccccccc
NEW=20261118T000000Z-bbbbbbbb
AUDIT_NAME=retention-20261118T010000Z-11111111-2222-3333-4444-555555555555.json
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/offsite-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT

for tool in python3 flock; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'offsite.sh: %s is required\n' "$tool" >&2
    exit 2
  }
done

pass=0
fail=0
out=""
rc=0

ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() {
  fail=$((fail + 1))
  printf 'FAIL %s\n' "$1"
  [ -z "${out:-}" ] || printf '%s\n' "$out" | sed 's/^/     | /'
}
check() { # description command...   (passes when the command succeeds)
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
has() { grep -qF -- "$1" <<<"$out"; }
on_remote() { [ -f "$T/remote/snapshots/$1/manifest.json" ]; }
not_on_remote() { ! [ -e "$T/remote/snapshots/$1" ]; }
processed() { grep -qxF -- "$AUDIT_NAME" "$T/state/processed-audits.txt"; }
not_processed() { ! processed; }
rc_is() { [ "$rc" -eq "$1" ]; }

# The fake rclone (deploy/tests/fake-rclone): the "remote" is a plain directory.
fake_rclone() { # path
  cp -- "$REPO/deploy/tests/fake-rclone" "$1"
  chmod +x "$1"
}

# mk_snap dir id [artifact-bytes] — a complete snapshot with a real manifest.
mk_snap() {
  local dir=$1 id=$2 n=${3:-64}
  mkdir -p "$dir/$id"
  python3 -I - "$dir/$id" "$id" "$n" <<'PY'
import hashlib, json, os, sys
path, sid, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
arts = []
for kind, name in (("db", "db.dump.age"), ("git", "git.tar.age")):
    data = (kind + sid).encode() * n
    data = data[:n] if len(data) >= n else data.ljust(n, b"x")
    with open(os.path.join(path, name), "wb") as handle:
        handle.write(data)
    arts.append({"name": name, "kind": kind, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
with open(os.path.join(path, "manifest.json"), "w") as handle:
    json.dump({"format": "muster-backup", "version": 2, "backup_id": sid,
               "op_id": "11111111-2222-3333-4444-555555555555",
               "snapshot_at": "2026-11-18T00:00:00Z", "created_at": "2026-11-18T00:00:05Z",
               "encryption": {"scheme": "age-x25519", "recipient_sha256": "0" * 64},
               "normal_expires_at": "2026-12-18T00:00:00Z", "tenants": [], "artifacts": arts}, handle)
PY
}

# patch_manifest dir id 'python statement on m' — edit a published manifest.
patch_manifest() {
  python3 -I - "$1/$2/manifest.json" "$3" <<'PY'
import json, sys
m = json.load(open(sys.argv[1]))
exec(sys.argv[2])
json.dump(m, open(sys.argv[1], "w"))
PY
}

audit_removing() { # id...   (the audit file is $AUDIT_FILE, default $AUDIT_NAME)
  mkdir -p "$T/backups/audit"
  python3 -I - "$T/backups/audit/${AUDIT_FILE:-$AUDIT_NAME}" "$@" <<'PY'
import json, sys
with open(sys.argv[1], "w") as handle:
    json.dump({"format": "muster-retention", "version": 1, "removed": sys.argv[2:]}, handle)
PY
}

setup() { # name
  T=$ROOT/$1
  mkdir -p "$T/bin" "$T/backups/snapshots" "$T/remote/snapshots" "$T/state"
  fake_rclone "$T/bin/rclone"
  : >"$T/rclone.log"
}

run_offsite() {
  rc=0
  out=$(MUSTER_OFFSITE_ALLOW_NONROOT=1 MUSTER_BACKUP_ROOT="$T/backups" MUSTER_OFFSITE_REMOTE="$T/remote" \
    MUSTER_OFFSITE_STATE="$T/state" MUSTER_RCLONE="$T/bin/rclone" FAKE_RCLONE_LOG="$T/rclone.log" \
    "$SCRIPT" 2>&1) || rc=$?
}

# ---- 1. the bug: Drive full, the purge works, the audit must NOT be consumed ---------
setup drive-full
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
audit_removing "$OLD"
FAKE_RCLONE_FULL=1 run_offsite
check "drive full: the run fails (so the failure alert fires)" test "$rc" -ne 0
check "drive full: the only complete offsite snapshot is NOT purged" on_remote "$OLD"
check "drive full: the audit stays unprocessed (retried next run)" not_processed
check "drive full: the log says why" has "must not be deleted"
check "drive full: no purge was even attempted" bash -c "! grep -q '^purge' '$T/rclone.log'"

# ... and the retry, once Drive has room again, uploads first and then purges.
run_offsite
check "retry: succeeds" rc_is 0
check "retry: the replacement is uploaded" on_remote "$NEW"
check "retry: only then the expired snapshot is purged" not_on_remote "$OLD"
check "retry: the audit is processed" processed

# ---- 2. the normal path ---------------------------------------------------------------
setup normal
mk_snap "$T/backups/snapshots" "$MID"
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
audit_removing "$OLD"
run_offsite
check "normal: exits 0" rc_is 0
check "normal: new snapshots are on Drive" on_remote "$NEW"
check "normal: the expired one is purged" not_on_remote "$OLD"
check "normal: the audit is processed" processed

# ---- 3. a truncated artifact on Drive does not count as a complete snapshot --------------
setup truncated
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
mk_snap "$T/remote/snapshots" "$NEW"
: >"$T/remote/snapshots/$NEW/git.tar.age" # a wrong-size file, the manifest is there
audit_removing "$OLD"
run_offsite
check "truncated: the older complete snapshot is kept" on_remote "$OLD"
check "truncated: the audit is not consumed" not_processed
check "truncated: exits non-zero" test "$rc" -ne 0
check "truncated: the log names the size problem" has "bytes on the remote"

# ---- 4. same size but another content (Drive reports SHA-256) ---------------------------
setup wrong-hash
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
mk_snap "$T/remote/snapshots" "$NEW"
size=$(stat -c %s "$T/remote/snapshots/$NEW/db.dump.age")
head -c "$size" /dev/zero | tr '\0' 'z' >"$T/remote/snapshots/$NEW/db.dump.age"
audit_removing "$OLD"
run_offsite
check "wrong hash: the older complete snapshot is kept" on_remote "$OLD"
check "wrong hash: the audit is not consumed" not_processed
check "wrong hash: the log names the hash" has "SHA-256"
# Without hashes from the remote only the size can be compared: it passes.
FAKE_RCLONE_NO_HASH=1 run_offsite
check "no remote hashes: the size is what is compared" not_on_remote "$OLD"

# ---- 5. the last complete copy is about to be purged and nothing newer exists --------------
setup last-copy
mk_snap "$T/remote/snapshots" "$OLD" # nothing local at all
audit_removing "$OLD"
run_offsite
check "last copy: kept when no local snapshot exists" on_remote "$OLD"
check "last copy: the audit is not consumed" not_processed
check "last copy: exits non-zero" test "$rc" -ne 0

# ---- 6. a manifest without its artifacts is not a snapshot ---------------------------------
setup no-artifacts
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
mk_snap "$T/remote/snapshots" "$NEW"
rm -f "$T/remote/snapshots/$NEW/db.dump.age"
audit_removing "$OLD"
FAKE_RCLONE_FULL=1 run_offsite # (a working upload would simply re-send the missing file)
check "missing artifact: the older complete snapshot is kept" on_remote "$OLD"
check "missing artifact: the log names it" has "db.dump.age is missing"

# ---- 7. an unreadable Drive deletes nothing ---------------------------------------------------
setup unlistable
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
audit_removing "$OLD"
FAKE_RCLONE_LS_FAIL=1 run_offsite
check "unlistable: nothing is purged" on_remote "$OLD"
check "unlistable: the audit is kept" not_processed
check "unlistable: exits non-zero" test "$rc" -ne 0

# ---- 8. a snapshot still present locally is never purged remotely --------------------------------
setup still-local
mk_snap "$T/backups/snapshots" "$OLD"
mk_snap "$T/backups/snapshots" "$NEW"
audit_removing "$OLD"
run_offsite
check "still local: the remote copy stays" on_remote "$OLD"
check "still local: the audit is processed (nothing to delete)" processed

# ---- 9. a manifest the restore would refuse is not a complete snapshot ---------------------------
for variant in 'm["version"] = 3' 'm["artifacts"][1]["kind"] = "extra"' 'm["format"] = "other"' \
  'm["encryption"]["scheme"] = "rot13"' 'm["artifacts"].append(dict(m["artifacts"][0]))'; do
  setup bad-manifest
  mk_snap "$T/backups/snapshots" "$NEW"
  mk_snap "$T/remote/snapshots" "$OLD"
  mk_snap "$T/remote/snapshots" "$NEW"
  patch_manifest "$T/remote/snapshots" "$NEW" "$variant"
  audit_removing "$OLD"
  FAKE_RCLONE_FULL=1 run_offsite
  check "invalid manifest ($variant): the older complete snapshot is kept" on_remote "$OLD"
  check "invalid manifest ($variant): the audit is not consumed" not_processed
done

# ---- 10. a failed purge stops every deletion of the run ---------------------------------------------
SECOND_AUDIT=retention-20261118T020000Z-66666666-2222-3333-4444-555555555555.json
setup purge-fails
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
mk_snap "$T/remote/snapshots" "$MID"
audit_removing "$OLD" "$MID"
AUDIT_FILE=$SECOND_AUDIT audit_removing "$MID"
FAKE_RCLONE_PURGE_FAIL=$OLD run_offsite
check "purge fails: exits 3 (the alert fires)" rc_is 3
check "purge fails: the log says all deletions stopped" has "STOPPING all deletions"
check "purge fails: nothing else is purged after the failure" on_remote "$MID"
check "purge fails: neither audit is consumed" not_processed
check "purge fails: the second audit is left unprocessed" bash -c "! grep -qxF '$SECOND_AUDIT' '$T/state/processed-audits.txt'"
# The damaged snapshot (its db artifact is gone, the manifest stays) is never
# counted as the complete survivor of a later purge; the next run, listing
# Drive afresh, finishes the job.
check "purge fails: the damaged snapshot has no db artifact left" bash -c "! [ -e '$T/remote/snapshots/$OLD/db.dump.age' ]"
run_offsite
check "next run: succeeds" rc_is 0
check "next run: both expired snapshots are gone" bash -c "[ ! -e '$T/remote/snapshots/$OLD' ] && [ ! -e '$T/remote/snapshots/$MID' ]"
check "next run: the newest stays" on_remote "$NEW"

# ---- 11. Drive is listed afresh before every purge decision ------------------------------------------
setup relist
mk_snap "$T/backups/snapshots" "$NEW"
mk_snap "$T/remote/snapshots" "$OLD"
mk_snap "$T/remote/snapshots" "$MID"
audit_removing "$OLD"
AUDIT_FILE=$SECOND_AUDIT audit_removing "$MID"
run_offsite
check "relist: both audits are purged" bash -c "[ ! -e '$T/remote/snapshots/$OLD' ] && [ ! -e '$T/remote/snapshots/$MID' ]"
check "relist: two purge decisions, two listings" test "$(grep -c '^lsjson' "$T/rclone.log")" -eq 2

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
