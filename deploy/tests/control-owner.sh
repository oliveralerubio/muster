#!/usr/bin/env bash
# deploy/tests/control-owner.sh — tests of deploy/pg-control-owner.sh, the step
# that gives the authority files back to the service after pg-restore.sh (which
# runs as root) rewrote them.
#
#   deploy/tests/control-owner.sh
#
# Without root the real ownership cannot be changed, so the first part records
# what is passed to chown (a shim in front of it) and checks the modes. When
# run as root (CI containers, or `docker run`) the second part proves the real
# owner and group of the files.
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/control-owner-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT
# shellcheck source=../pg-control-owner.sh
. "$REPO/deploy/pg-control-owner.sh"

pass=0
fail=0
ok() { pass=$((pass + 1)); printf 'ok   %s\n' "$1"; }
bad() { fail=$((fail + 1)); printf 'FAIL %s\n' "$1"; }
check() { # description command...
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}

REAL_CHOWN=$(command -v chown)

# layout name — control/ (journal, lock, a stray temp file, an unrelated file, a
# symlink) and witness/ (witness.json, a leftover temp file), all mode 0644 so
# the 0600 is visible.
layout() {
  T=$ROOT/$1
  mkdir -p "$T/control" "$T/witness" "$T/shim"
  chmod 0700 "$T/control" "$T/witness"
  : >"$T/control/journal.v1.jsonl"
  : >"$T/control/control.lock"
  : >"$T/control/.witness.json.tmp-1234"
  : >"$T/control/unrelated.txt"
  : >"$T/witness/witness.json"
  : >"$T/witness/.witness.json.tmp-abcd"
  : >"$T/witness/other-file"
  ln -s "$T/control/unrelated.txt" "$T/control/link.tmp-1"
  chmod 0644 "$T"/control/* "$T"/witness/* 2>/dev/null
  : >"$T/chown.log"
}

mode_is() { [ "$(stat -c %a -- "$1")" = "$2" ]; }
called_for() { grep -qF -- "--reference=$1 -- $2" "$T/chown.log"; }
not_called_for() { ! grep -qF -- " $1" "$T/chown.log"; }

# ---- as any user: what is passed to chown, and the modes -----------------------------
layout shim
cat >"$T/shim/chown" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$T/chown.log"
exec "$REAL_CHOWN" "\$@"
EOF
chmod +x "$T/shim/chown"
(PATH="$T/shim:$PATH" fix_control_ownership "$T/control" "$T/witness/witness.json")
check "returns success" test $? -eq 0
check "journal gets the owner of the control dir" called_for "$T/control" "$T/control/journal.v1.jsonl"
check "the lock gets the owner of the control dir" called_for "$T/control" "$T/control/control.lock"
check "the witness gets the owner of the witness dir" called_for "$T/witness" "$T/witness/witness.json"
check "a leftover temp file of the witness is fixed too" called_for "$T/witness" "$T/witness/.witness.json.tmp-abcd"
check "journal mode is 0600" mode_is "$T/control/journal.v1.jsonl" 600
check "lock mode is 0600" mode_is "$T/control/control.lock" 600
check "witness mode is 0600" mode_is "$T/witness/witness.json" 600
check "an unrelated file is not touched (owner)" not_called_for "$T/control/unrelated.txt"
check "an unrelated file is not touched (mode)" mode_is "$T/control/unrelated.txt" 644
check "another file next to the witness is not touched" mode_is "$T/witness/other-file" 644
check "a symlink is never followed" not_called_for "$T/control/link.tmp-1"

# ---- a failing chown is reported -------------------------------------------------------
layout failing
printf '#!/bin/sh\nexit 1\n' >"$T/shim/chown"
chmod +x "$T/shim/chown"
(PATH="$T/shim:$PATH" fix_control_ownership "$T/control" "$T/witness/witness.json" 2>/dev/null)
check "a chown that fails makes the function fail" test $? -ne 0

# ---- missing files are fine (the lock may not exist) ------------------------------------------
layout missing
rm -f "$T/control/control.lock"
fix_control_ownership "$T/control" "$T/witness/witness.json"
check "a missing file is skipped" test $? -eq 0

# ---- as root: the real owner ------------------------------------------------------------------
if [ "$(id -u)" -eq 0 ]; then
  layout root
  "$REAL_CHOWN" 10001:10001 "$T/control" "$T/witness"
  fix_control_ownership "$T/control" "$T/witness/witness.json"
  owner_is() { [ "$(stat -c %u:%g -- "$1")" = "$2" ]; }
  check "root: journal is 10001:10001" owner_is "$T/control/journal.v1.jsonl" 10001:10001
  check "root: lock is 10001:10001" owner_is "$T/control/control.lock" 10001:10001
  check "root: witness is 10001:10001" owner_is "$T/witness/witness.json" 10001:10001
  check "root: the witness temp file is 10001:10001" owner_is "$T/witness/.witness.json.tmp-abcd" 10001:10001
  check "root: an unrelated file keeps its owner" owner_is "$T/control/unrelated.txt" 0:0
  check "root: witness mode is 0600" mode_is "$T/witness/witness.json" 600
else
  printf 'note the real owner is only checked as root (run as root or in a container)\n'
fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
