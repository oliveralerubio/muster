#!/usr/bin/env bash
# deploy/tests/backup-check-units.sh — tests of the two checks of
# `muster-cloud-backup-check` that look at systemd: the self-heal of the path
# unit that starts the authority copy, and the watch on the monthly restore
# drill. A fake systemctl, a fake alert tool and a fake rclone; no root, no
# systemd and no network needed.
#
#   deploy/tests/backup-check-units.sh
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_BACKUP_CHECK_SCRIPT:-$REPO/deploy/kamal/muster-cloud-backup-check}
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/backup-check-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT
NOW=$(date -u -d 2026-11-18T12:00:00Z +%s)

for tool in python3 flock; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'backup-check-units.sh: %s is required\n' "$tool" >&2
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
  [ ! -f "$T/alert.log" ] || sed 's/^/     a| /' "$T/alert.log"
  [ ! -f "$T/systemctl.log" ] || sed 's/^/     s| /' "$T/systemctl.log"
}
check() { # description command...
  local desc=$1
  shift
  if "$@"; then ok "$desc"; else bad "$desc"; fi
}
rc_is() { [ "$rc" -eq "$1" ]; }
fired() { grep -q "^fire $1 " "$T/alert.log"; }
not_fired() { ! fired "$1"; }
ran() { grep -qxF -- "$1" "$T/systemctl.log"; }
not_ran() { ! ran "$1"; }

# The fake systemctl: a unit is "enabled" when $T/enabled/<unit> exists and
# "active" when $T/active/<unit> exists; `start` of the path unit succeeds
# unless $T/start-fails exists, and then makes it active.
write_fake_systemctl() {
  cat >"$1" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_STATE/systemctl.log"
case "$1" in
  is-enabled) [ -e "$FAKE_STATE/enabled/${*: -1}" ]; exit $? ;;
  is-active) [ -e "$FAKE_STATE/active/${*: -1}" ]; exit $? ;;
  reset-failed) exit 0 ;;
  start)
    [ ! -e "$FAKE_STATE/start-fails" ] || exit 1
    case "${*: -1}" in *.path) : >"$FAKE_STATE/active/${*: -1}" ;; esac
    exit 0
    ;;
  list-units) exit 0 ;;
esac
exit 0
FAKE
  chmod +x "$1"
}

write_fake_alert() {
  cat >"$1" <<'FAKE'
#!/usr/bin/env bash
echo "$*" >>"$FAKE_STATE/alert.log"
exit 0
FAKE
  chmod +x "$1"
}

setup() {
  T=$ROOT/$1
  mkdir -p "$T/enabled" "$T/active" "$T/drill" "$T/backups/snapshots" "$T/remote" "$T/control"
  : >"$T/alert.log"
  : >"$T/systemctl.log"
  write_fake_systemctl "$T/systemctl"
  write_fake_alert "$T/alert"
  cp -- "$REPO/deploy/tests/fake-rclone" "$T/rclone"
  chmod +x "$T/rclone"
}

run_check() {
  rc=0
  out=$(MUSTER_BACKUP_CHECK_ALLOW_NONROOT=1 MUSTER_BACKUP_CHECK_NOW="$NOW" MUSTER_BACKUP_ROOT="$T/backups" \
    MUSTER_CONTROL_DIR="$T/control" MUSTER_OFFSITE_REMOTE="$T/remote" MUSTER_RCLONE="$T/rclone" \
    MUSTER_ALERT_BIN="$T/alert" MUSTER_SYSTEMCTL="$T/systemctl" MUSTER_DRILL_STATE="$T/drill" \
    MUSTER_AUTODEPLOY_STATE="$T/none" FAKE_STATE="$T" "$SCRIPT" 2>&1) || rc=$?
}

PATH_UNIT=muster-cloud-authority-offsite.path
DRILL_TIMER=muster-cloud-restore-drill.timer
days_ago() { touch -d "@$((NOW - $2 * 86400))" "$1"; }

# ---- the path unit is running: nothing to do ---------------------------------------
setup path-ok
: >"$T/enabled/$PATH_UNIT"
: >"$T/active/$PATH_UNIT"
run_check
check "path running: the check completes" rc_is 0
check "path running: it is not restarted" not_ran "start $PATH_UNIT"
check "path running: no alert for it" not_fired authority-path-down

# ---- the path unit stopped (trigger limit): healed without an alert ------------------
setup path-down
: >"$T/enabled/$PATH_UNIT"
run_check
check "path stopped: the check completes" rc_is 0
check "path stopped: the failed state is reset" ran "reset-failed $PATH_UNIT"
check "path stopped: it is started again" ran "start $PATH_UNIT"
check "path stopped: an authority copy is started (nothing is lost)" ran "start --no-block muster-cloud-authority-offsite.service"
check "path stopped: healed, so no alert" not_fired authority-path-down

# ---- ... and when it cannot be started, an alert --------------------------------------
setup path-stuck
: >"$T/enabled/$PATH_UNIT"
: >"$T/start-fails"
run_check
check "path cannot start: an alert is sent" fired authority-path-down

# ---- not enabled (never installed): left alone ---------------------------------------------
setup path-off
run_check
check "path not enabled: not touched" not_ran "start $PATH_UNIT"

# ---- the restore drill ------------------------------------------------------------------
setup drill-fresh
: >"$T/enabled/$DRILL_TIMER"
: >"$T/drill/last-ok"
days_ago "$T/drill/last-ok" 20
run_check
check "drill 20 days ago: no alert" not_fired restore-drill-stale

setup drill-stale
: >"$T/enabled/$DRILL_TIMER"
: >"$T/drill/last-ok"
days_ago "$T/drill/last-ok" 45
run_check
check "drill 45 days ago: alert" fired restore-drill-stale

setup drill-new-install
: >"$T/enabled/$DRILL_TIMER"
: >"$T/drill/enabled-since"
days_ago "$T/drill/enabled-since" 10
run_check
check "enabled 10 days ago, no drill yet: no alert" not_fired restore-drill-stale
days_ago "$T/drill/enabled-since" 41
run_check
check "enabled 41 days ago, still no drill: alert" fired restore-drill-stale

setup drill-no-mark
: >"$T/enabled/$DRILL_TIMER"
run_check
check "no mark at all: no alert (the mark is made by host-setup)" not_fired restore-drill-stale

setup drill-off
: >"$T/drill/last-ok"
days_ago "$T/drill/last-ok" 90
run_check
check "timer not enabled: not watched" not_fired restore-drill-stale

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
