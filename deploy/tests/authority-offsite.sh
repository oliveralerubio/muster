#!/usr/bin/env bash
# deploy/tests/authority-offsite.sh — tests of `muster-cloud-authority-offsite`
# (the encrypted offsite copy of the lifecycle authority) with a fake rclone
# (deploy/tests/fake-rclone), a fake `muster-cloud-ops status` and the real
# `age`; no root and no network needed.
#
#   deploy/tests/authority-offsite.sh
#
# Covers: the copy is encrypted to every recipient of ops.env (each identity
# decrypts it), a bad extra recipient is refused, an unchanged journal uploads
# nothing, and a journal that grows while the copy runs is copied again at once
# (a start request during a run is a no-op for systemd, so the script itself
# must not leave the change for the hourly run).
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_AUTHORITY_SCRIPT:-$REPO/deploy/kamal/muster-cloud-authority-offsite}
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/authority-offsite-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT

for tool in python3 flock age age-keygen tar; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'authority-offsite.sh: %s is required\n' "$tool" >&2
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
check() { # description command...
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
has() { grep -qF -- "$1" <<<"$out"; }
rc_is() { [ "$rc" -eq "$1" ]; }
uploads() { find "$T/remote/authority" -name 'authority-*.tar.age' 2>/dev/null | wc -l | tr -d ' '; }
uploaded_count_is() { [ "$(uploads)" = "$1" ]; }

# Two identities: the backup key (kept off the host) and the drill key.
age-keygen -o "$ROOT/main.key" >/dev/null 2>&1
age-keygen -o "$ROOT/drill.key" >/dev/null 2>&1
pub() { sed -n 's/^# public key: //p' "$1"; }
MAIN=$(pub "$ROOT/main.key")
DRILL=$(pub "$ROOT/drill.key")

# A fake `muster-cloud-ops status`: the sequence is the number of lines of the
# journal it is pointed at. On its first call it may append a line to the real
# journal, as a record committed while the copy runs.
write_fake_ops() { # path
  cat >"$1" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = status ] || exit 64
seq=$(wc -l <"$MUSTER_CONTROL_DIR/journal.v1.jsonl" | tr -d ' ')
hash=$(printf '%064d' "$seq")
if [ -n "${FAKE_OPS_GROW:-}" ] && { [ -n "${FAKE_OPS_GROW_ALWAYS:-}" ] || [ ! -e "$FAKE_OPS_GROW.done" ]; }; then
  : >"$FAKE_OPS_GROW.done"
  printf '{"n":%s}\n' "$((seq + 1))" >>"$FAKE_OPS_GROW"
fi
printf '{"sequence":%s,"journal_hash":"%s","deployment_id":"d","control_store_id":"s"}\n' "$seq" "$hash"
EOF
  chmod +x "$1"
}

setup() { # name ops-env-extra-lines
  T=$ROOT/$1
  mkdir -p "$T/control" "$T/witness" "$T/state" "$T/remote"
  printf '{"n":1}\n' >"$T/control/journal.v1.jsonl"
  printf '{"w":1}\n' >"$T/witness/witness.json"
  cp -- "$REPO/deploy/tests/fake-rclone" "$T/rclone"
  chmod +x "$T/rclone"
  write_fake_ops "$T/ops"
  printf 'MUSTER_BACKUP_AGE_RECIPIENT=%s\n%s' "$MAIN" "${2:-}" >"$T/ops.env"
}

run_copy() {
  rc=0
  out=$(MUSTER_AUTHORITY_ALLOW_NONROOT=1 MUSTER_CONTROL_DIR="$T/control" MUSTER_CONTROL_WITNESS="$T/witness/witness.json" \
    MUSTER_OPS_ENV="$T/ops.env" MUSTER_OPS_BIN="$T/ops" MUSTER_OFFSITE_REMOTE="$T/remote" \
    MUSTER_AUTHORITY_STATE="$T/state" MUSTER_RCLONE="$T/rclone" MUSTER_AGE="$(command -v age)" FAKE_RCLONE_LOG="$T/rclone.log" \
    FAKE_OPS_GROW="${FAKE_OPS_GROW:-}" FAKE_OPS_GROW_ALWAYS="${FAKE_OPS_GROW_ALWAYS:-}" "$SCRIPT" 2>&1) || rc=$?
}

# opens_with identity archive — the identity decrypts it and it holds the three members.
opens_with() {
  age --decrypt -i "$1" -o "$T/plain.tar" "$2" 2>/dev/null \
    && [ "$(tar -tf "$T/plain.tar" | sort | tr '\n' ' ')" = "META.json journal.v1.jsonl witness.json " ]
}
newest_upload() { find "$T/remote/authority" -name 'authority-*.tar.age' | sort | tail -n 1; }

# ---- 1. two recipients: each identity decrypts ----------------------------------------------
setup two "MUSTER_BACKUP_AGE_RECIPIENTS=$DRILL
"
run_copy
check "two recipients: the copy succeeds" rc_is 0
check "two recipients: one archive is uploaded" uploaded_count_is 1
check "two recipients: the main identity opens it" opens_with "$ROOT/main.key" "$(newest_upload)"
check "two recipients: the drill identity opens it" opens_with "$ROOT/drill.key" "$(newest_upload)"

# ---- 2. unchanged journal: nothing new -----------------------------------------------------------
run_copy
check "unchanged: succeeds" rc_is 0
check "unchanged: says it is already offsite" has "already offsite"
check "unchanged: nothing more is uploaded" uploaded_count_is 1

# ---- 2b. the recipients change (the drill key is added): the same journal is copied again --------------
sleep 1 # the archive name carries the time to the second
printf 'MUSTER_BACKUP_AGE_RECIPIENT=%s\n' "$MAIN" >"$T/ops.env"
run_copy
check "recipients changed: a copy for the new set is made" uploaded_count_is 2
sleep 1
printf 'MUSTER_BACKUP_AGE_RECIPIENT=%s\nMUSTER_BACKUP_AGE_RECIPIENTS=%s\n' "$MAIN" "$DRILL" >"$T/ops.env"
run_copy
check "recipients changed back: copied again, the drill key opens it" opens_with "$ROOT/drill.key" "$(newest_upload)"

# ---- 3. one recipient: only the main identity ------------------------------------------------------
setup one
run_copy
check "one recipient: the main identity opens it" opens_with "$ROOT/main.key" "$(newest_upload)"
check "one recipient: the drill identity does not" bash -c '! age --decrypt -i "$1" -o /dev/null "$2" 2>/dev/null' _ "$ROOT/drill.key" "$(newest_upload)"

# ---- 4. a bad extra recipient is refused before anything is uploaded -------------------------------------
setup bad "MUSTER_BACKUP_AGE_RECIPIENTS=$DRILL,not-a-key
"
run_copy
check "bad extra recipient: exit 69" rc_is 69
check "bad extra recipient: the log names the setting" has "MUSTER_BACKUP_AGE_RECIPIENTS"
check "bad extra recipient: nothing uploaded" uploaded_count_is 0

# ---- 5. a repeat of the primary and blanks are tolerated ---------------------------------------------------
setup dup "MUSTER_BACKUP_AGE_RECIPIENTS= $MAIN , $DRILL ,
"
run_copy
check "duplicates and blanks: succeeds" rc_is 0
check "duplicates and blanks: the drill identity opens it" opens_with "$ROOT/drill.key" "$(newest_upload)"

# ---- 6. a record committed while the copy runs is copied at once -------------------------------------------
setup grow
FAKE_OPS_GROW="$T/control/journal.v1.jsonl" run_copy
check "growing journal: succeeds" rc_is 0
check "growing journal: copied again in the same run" has "copying again"
check "growing journal: both states were uploaded" uploaded_count_is 2
check "growing journal: the second copy has sequence 2" bash -c 'ls "$1/remote/authority" | grep -q -- "-2.tar.age"' _ "$T"

# ---- 7. a journal that never stops growing: not "ok" while the upload is behind ----------------------------
setup busy
FAKE_OPS_GROW="$T/control/journal.v1.jsonl" FAKE_OPS_GROW_ALWAYS=1 run_copy
check "busy journal: exits 75, not 0" rc_is 75
check "busy journal: five rounds were tried" uploaded_count_is 5
check "busy journal: the log says the copy is behind" has "still changing after 5 rounds"
# The unit starts it again by itself, and the alert is not sent per restart.
UNIT=$REPO/deploy/systemd/muster-cloud-authority-offsite.service
check "unit: Restart=on-failure" grep -qx 'Restart=on-failure' "$UNIT"
check "unit: RestartSec=30s" grep -qx 'RestartSec=30s' "$UNIT"
check "unit: a sane start limit (not unlimited)" bash -c 'grep -qx "StartLimitBurst=40" "$1" && grep -qx "StartLimitIntervalSec=30min" "$1"' _ "$UNIT"
check "unit: configuration errors are not retried" grep -qx 'RestartPreventExitStatus=65 69 77' "$UNIT"
check "unit: still alerts through OnFailure" grep -qx 'OnFailure=muster-cloud-alert@%n.service' "$UNIT"

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
