#!/usr/bin/env bash
# deploy/tests/restore-drill.sh — tests of what `muster-cloud-restore-drill`
# decides BEFORE it restores anything: which snapshot it uses (the newest one
# whose manifest lists the identity's public key as a recipient, younger than
# the limit) and that it refuses cleanly when the scratch space is too small.
# A fake rclone (deploy/tests/fake-rclone) and real age keys; no root, no
# Docker, no PostgreSQL and no network needed (the restore itself is covered by
# the backup-drill CI job and the Rust restore tests).
#
#   deploy/tests/restore-drill.sh
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_DRILL_SCRIPT:-$REPO/deploy/kamal/muster-cloud-restore-drill}
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/restore-drill-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT
NOW=$(date -u -d 2026-11-18T12:00:00Z +%s)

for tool in python3 age age-keygen sha256sum; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'restore-drill.sh: %s is required\n' "$tool" >&2
    exit 2
  }
done

pass=0
fail=0
out=""
err=""
rc=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() {
  fail=$((fail + 1))
  printf 'FAIL %s\n' "$1"
  [ -z "${out:-}${err:-}" ] || printf '%s\n%s\n' "$out" "$err" | sed 's/^/     | /'
}
check() { # description command...
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
has_out() { grep -qF -- "$1" <<<"$out"; }
has_err() { grep -qF -- "$1" <<<"$err"; }
rc_is() { [ "$rc" -eq "$1" ]; }
picked() { python3 -I -c 'import json,sys; sys.exit(0 if json.loads(sys.argv[1])["snapshot_id"] == sys.argv[2] else 1)' "$out" "$1"; }

age-keygen -o "$ROOT/main.key" >/dev/null 2>&1
age-keygen -o "$ROOT/drill.key" >/dev/null 2>&1
age-keygen -o "$ROOT/other.key" >/dev/null 2>&1
pub() { sed -n 's/^# public key: //p' "$1"; }
sha() { printf '%s' "$1" | sha256sum | cut -d' ' -f1; }
MAIN=$(sha "$(pub "$ROOT/main.key")")
DRILL=$(sha "$(pub "$ROOT/drill.key")")

# snap id recipients... — a remote snapshot whose manifest lists these recipient
# hashes the way `backup run` writes them: recipient_sha256 is the first,
# recipients_sha256 appears only with more than one.
snap() {
  local id=$1
  shift
  mkdir -p "$T/remote/snapshots/$id"
  python3 -I - "$T/remote/snapshots/$id" "$id" "$@" <<'PY'
import json, os, sys
path, sid, hashes = sys.argv[1], sys.argv[2], sys.argv[3:]
enc = {"scheme": "age-x25519", "recipient_sha256": hashes[0]}
if len(hashes) > 1:
    enc["recipients_sha256"] = hashes
for name in ("db.dump.age", "git.tar.age"):
    with open(os.path.join(path, name), "wb") as handle:
        handle.write(b"x" * 100)
with open(os.path.join(path, "manifest.json"), "w") as handle:
    json.dump({"format": "muster-backup", "version": 2, "backup_id": sid, "encryption": enc,
               "tenants": [], "artifacts": []}, handle)
PY
}
authority() { mkdir -p "$T/remote/authority"; : >"$T/remote/authority/authority-20261118T100000Z-7.tar.age"; }

setup() {
  T=$ROOT/$1
  mkdir -p "$T/work" "$T/known1" "$T/known2"
  cp -- "$REPO/deploy/tests/fake-rclone" "$T/rclone"
  chmod +x "$T/rclone"
  fake_docker
  : >"$T/containers"
}

# A fake docker: `ps` lists the containers named in $T/containers, `rm -f NAME` drops the name
# and logs it.
fake_docker() {
  cat >"$T/docker" <<'FAKE'
#!/bin/sh
dir=$(dirname "$0")
case $1 in
  ps) [ -z "$FAKE_DOCKER_PS_FAIL" ] || { echo 'Cannot connect to the Docker daemon' >&2; exit 1; }; cat "$dir/containers" ;;
  rm) shift; shift; for n in "$@"; do printf '%s\n' "$n" >>"$dir/removed"; grep -vxF "$n" "$dir/containers" >"$dir/c.new"; mv "$dir/c.new" "$dir/containers"; done ;;
  *) exit 1 ;;
esac
FAKE
  chmod +x "$T/docker"
}
# drill identity-file args...
drill() {
  local identity=$1
  shift
  rc=0
  MUSTER_DRILL_ALLOW_NONROOT=1 MUSTER_OFFSITE_REMOTE="$T/remote" MUSTER_RCLONE="$T/rclone" \
    MUSTER_DRILL_LOCK="$T/drill.lock" MUSTER_DRILL_KNOWN_WORKDIRS="$T/known1:$T/known2" MUSTER_DOCKER="$T/docker" \
    MUSTER_AGE="$(command -v age)" MUSTER_AGE_KEYGEN="$(command -v age-keygen)" MUSTER_DRILL_NOW="$NOW" \
    "$SCRIPT" --age-identity "$identity" "$@" >"$T/out" 2>"$T/err" || rc=$?
  out=$(cat "$T/out")
  err=$(cat "$T/err")
}

A=20261118T060000Z-aaaaaaaa # 6 h old
B=20261118T000000Z-bbbbbbbb # 12 h old
C=20261117T180000Z-cccccccc # 18 h old
OLD=20261101T000000Z-dddddddd # 17 days old

# ---- the newest snapshot is encrypted to the drill key --------------------------------------
setup newest
snap "$C" "$MAIN" "$DRILL"
snap "$B" "$MAIN" "$DRILL"
snap "$A" "$MAIN" "$DRILL"
authority
drill "$ROOT/drill.key" --select-only
check "newest: exits 0" rc_is 0
check "newest: the newest complete snapshot is used" picked "$A"

# ---- the newest one predates the drill key: the newest that lists it -----------------------------
setup older
snap "$A" "$MAIN"
snap "$B" "$MAIN"
snap "$C" "$MAIN" "$DRILL"
authority
drill "$ROOT/drill.key" --select-only
check "older: exits 0" rc_is 0
check "older: picks the newest snapshot that lists the drill key" picked "$C"
check "older: says so" has_err "the newest one this identity can open"
# The main key opens all of them: the newest again.
drill "$ROOT/main.key" --select-only
check "older: the main key (recipient_sha256) takes the newest" picked "$A"

# ---- none lists the key ---------------------------------------------------------------------------
setup none
snap "$A" "$MAIN"
snap "$B" "$MAIN"
authority
drill "$ROOT/drill.key" --select-only
check "none: fails" rc_is 1
check "none: says no snapshot is encrypted to this key" has_err "is encrypted to this identity's key"
check "none: the JSON result says ok=false" has_out '"ok": false'
drill "$ROOT/other.key" --select-only
check "a stranger's key: fails" rc_is 1

# ---- the one that lists it is too old ----------------------------------------------------------------
setup stale
snap "$OLD" "$MAIN" "$DRILL"
snap "$A" "$MAIN"
authority
drill "$ROOT/drill.key" --select-only
check "too old: fails" rc_is 1
check "too old: names the age limit" has_err "younger than 8 days"
drill "$ROOT/drill.key" --select-only --max-age-days 30
check "a longer limit accepts it" picked "$OLD"

# ---- a snapshot without a manifest is not complete -----------------------------------------------------
setup incomplete
snap "$B" "$MAIN" "$DRILL"
snap "$A" "$MAIN" "$DRILL"
rm -f "$T/remote/snapshots/$A/manifest.json"
authority
drill "$ROOT/drill.key" --select-only
check "incomplete: the newest COMPLETE snapshot is used" picked "$B"

# ---- no authority copy ------------------------------------------------------------------------------------
setup noauth
snap "$A" "$MAIN" "$DRILL"
mkdir -p "$T/remote/authority"
drill "$ROOT/drill.key" --select-only
check "no authority copy: fails" rc_is 1
check "no authority copy: says so" has_err "no authority copy"

# ---- too little scratch space: a clean refusal before anything is downloaded ------------------------------------
setup space
snap "$A" "$MAIN" "$DRILL"
authority
FAKE_RCLONE_SIZE_BYTES=900000000000000000 drill "$ROOT/drill.key" --workdir "$T/work"
check "no space: fails" rc_is 1
check "no space: says how much is needed" has_err "not enough free space"
check "no space: the JSON result says ok=false" has_out '"ok": false'
check "no space: nothing was created in the workdir" bash -c '[ -z "$(ls -A "$1")" ]' _ "$T/work"
check "no space: nothing was downloaded" bash -c '! grep -q "^copy" "$1" 2>/dev/null' _ "$T/rclone.log"

# A leftover can be what takes the space: it is removed BEFORE the check, so
# the check sees the space it frees (and it is gone even when the check fails).
setup space-leftover
snap "$A" "$MAIN" "$DRILL"
authority
mkdir -p "$T/work/muster-restore-drill.Old123/snapshot"
FAKE_RCLONE_SIZE_BYTES=900000000000000000 drill "$ROOT/drill.key" --workdir "$T/work"
check "leftover before the space check: the check still fails here" has_err "not enough free space"
check "leftover before the space check: the leftover was removed first" test ! -e "$T/work/muster-restore-drill.Old123"

# ---- leftovers of an interrupted drill are removed (plaintext restored data must not stay) ---------
clean() {
  rc=0
  MUSTER_DRILL_ALLOW_NONROOT=1 MUSTER_DOCKER="$T/docker" MUSTER_DRILL_LOCK="$T/drill.lock" \
    MUSTER_DRILL_KNOWN_WORKDIRS="$T/known1:$T/known2" "$SCRIPT" --clean-only --workdir "$T/work" "$@" >"$T/out" 2>"$T/err" || rc=$?
  out=$(cat "$T/out")
  err=$(cat "$T/err")
}
setup leftovers
fake_docker
mkdir -p "$T/work/muster-restore-drill.AbC123/snapshot" "$T/work/muster-restore-drill.Zzz999/repos" "$T/work/keep-me"
printf 'plaintext\n' >"$T/work/muster-restore-drill.AbC123/snapshot/db.dump"
printf 'muster-drill-pg-0123abcd\nmuster-drill-pg-ffff0000\nunrelated\nmuster-drill-pg-NOTHEX\n' >"$T/containers"
clean
check "leftovers: exits 0" rc_is 0
check "leftovers: the scratch directories are gone" bash -c '[ ! -e "$1/muster-restore-drill.AbC123" ] && [ ! -e "$1/muster-restore-drill.Zzz999" ]' _ "$T/work"
check "leftovers: other directories are kept" test -d "$T/work/keep-me"
check "leftovers: the drill containers are removed" bash -c 'grep -qx muster-drill-pg-0123abcd "$1" && grep -qx muster-drill-pg-ffff0000 "$1"' _ "$T/removed"
check "leftovers: other containers are never touched" bash -c '! grep -q "unrelated\|NOTHEX" "$1"' _ "$T/removed"
check "leftovers: says what it removed" has_err "removing leftover scratch directory"
clean
check "no leftovers: exits 0" rc_is 0
# ONE host-wide cleanup: the given workdir and the known ones are all cleaned.
setup known
mkdir -p "$T/known1/muster-restore-drill.K1" "$T/known2/muster-restore-drill.K2" "$T/work/muster-restore-drill.W"
clean
check "known workdirs: all three are cleaned" bash -c '[ ! -e "$1/known1/muster-restore-drill.K1" ] && [ ! -e "$1/known2/muster-restore-drill.K2" ] && [ ! -e "$1/work/muster-restore-drill.W" ]' _ "$T"
# --clean-only fails when Docker cannot be listed (leftover containers are unknown).
setup docker-down
mkdir -p "$T/work/muster-restore-drill.W"
FAKE_DOCKER_PS_FAIL=1 clean
check "docker enumeration fails: --clean-only exits non-zero" test "$rc" -ne 0
check "docker enumeration fails: says why" has_err "cannot list the Docker containers"
# A leftover that cannot be removed fails the run (so the unit fails, alerts and last-ok stays old).
if [ "$(id -u)" -ne 0 ]; then
  setup stuck
  fake_docker
  : >"$T/containers"
  mkdir -p "$T/work/muster-restore-drill.Stuck/sub"
  chmod 0555 "$T/work/muster-restore-drill.Stuck"
  clean
  check "cannot remove: exits non-zero" test "$rc" -ne 0
  check "cannot remove: says which" has_err "cannot remove the leftover"
  chmod 0755 "$T/work/muster-restore-drill.Stuck"
fi
# Two drills never remove each other's directories or containers: ONE lock, whatever the workdir.
setup locked
mkdir -p "$T/work/muster-restore-drill.Live" "$T/other/muster-restore-drill.Live2"
printf 'muster-drill-pg-0123abcd\n' >"$T/containers"
(
  exec 8>"$T/drill.lock"
  flock -n 8 || exit 1
  clean
  [ "$rc" -ne 0 ] && [ -d "$T/work/muster-restore-drill.Live" ] && has_err "another restore drill is running" \
    && [ ! -e "$T/removed" ]
) && ok "a running drill's directory survives a second drill" || bad "a running drill's directory survives a second drill"

setup locked-full
snap "$A" "$MAIN" "$DRILL"
authority
mkdir -p "$T/other"
printf 'muster-drill-pg-0123abcd\n' >"$T/containers"
(
  exec 8>"$T/drill.lock"
  flock -n 8 || exit 1
  drill "$ROOT/drill.key" --workdir "$T/other"
  [ "$rc" -ne 0 ] && has_err "another restore drill is running" && [ ! -e "$T/removed" ] && grep -qx muster-drill-pg-0123abcd "$T/containers"
) && ok "another workdir, same host-wide lock: the live container is not killed" || bad "another workdir, same host-wide lock: the live container is not killed"
drill "$ROOT/drill.key" --workdir "$T/other"
check "the lock is released with the drill that held it" bash -c '! grep -q "another restore drill" "$1"' _ "$T/err"

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
