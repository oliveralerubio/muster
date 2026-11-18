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
  mkdir -p "$T/work"
  cp -- "$REPO/deploy/tests/fake-rclone" "$T/rclone"
  chmod +x "$T/rclone"
}

# drill identity-file args...
drill() {
  local identity=$1
  shift
  rc=0
  MUSTER_DRILL_ALLOW_NONROOT=1 MUSTER_OFFSITE_REMOTE="$T/remote" MUSTER_RCLONE="$T/rclone" \
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

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
