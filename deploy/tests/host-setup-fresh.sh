#!/usr/bin/env bash
# deploy/tests/host-setup-fresh.sh — tests that `host-setup.sh` makes a fresh
# host safe BEFORE the timers are enabled: with no `deployed` record it creates
# the `guard-only` marker (so nothing can deploy before `init`), it never adds
# one to a host that has deployed or when autodeploy itself calls it
# (MUSTER_HOST_SETUP_CALLER=autodeploy: its first deploy runs host-setup before
# it writes `deployed`), and never touches an existing guard-only or hold. Also
# that the drill's boot-time cleanup rule is installed.
#
#   deploy/tests/host-setup-fresh.sh
#
# host-setup.sh --check changes nothing, but it insists on being root and uses
# fixed paths, so it runs in a user namespace (unshare, needs /etc/subuid for the
# caller) with a tmpfs over /var/lib; where that is not possible the test says so
# and exits 0.
#
# Exit 0 when every case passes (or is skipped), 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
T=$(mktemp -d "${TMPDIR:-/tmp}/host-setup-fresh-test.XXXXXX")
trap 'rm -rf -- "$T"' EXIT
chmod 0755 "$T"
mkdir -p "$T/repo"
cp -r -- "$REPO/deploy" "$REPO/config" "$T/repo/"
chmod -R a+rX "$T"

cat >"$T/inner.sh" <<'EOF'
R=$1
mount -t tmpfs none /var/lib || exit 90
mkdir -p /var/lib/muster-cloud
S=/var/lib/muster-cloud/autodeploy
run() { "$R/repo/deploy/kamal/host-setup.sh" --check 2>&1 | grep -E 'guard-only|tmpfiles' ; }
echo "== fresh";    run
echo "== autodeploy"; MUSTER_HOST_SETUP_CALLER=autodeploy run
echo "== other-caller"; MUSTER_HOST_SETUP_CALLER=someone run
mkdir -p $S; : >$S/deployed
echo "== deployed"; run
rm -f $S/deployed; : >$S/guard-only
echo "== marker";   run
rm -f $S/guard-only; : >$S/hold
echo "== hold";     run
echo "== end"
EOF

out=$(unshare --user --mount --map-users=100000,0,65536 --map-groups=100000,0,65536 --setuid=0 --setgid=0 \
  bash "$T/inner.sh" "$T" 2>&1) || {
  printf 'host-setup-fresh.sh: SKIPPED (no usable user namespace): %s\n' "$(printf '%s' "$out" | head -n 2 | tr '\n' ' ')"
  exit 0
}

pass=0
fail=0
section() { printf '%s\n' "$out" | awk -v want="== $1" '$0 == want {on=1; next} /^== /{on=0} on'; }
check() { # description section pattern
  if section "$2" | grep -qF -- "$3"; then
    pass=$((pass + 1))
    printf 'ok   %s\n' "$1"
  else
    fail=$((fail + 1))
    printf 'FAIL %s\n' "$1"
    section "$2" | sed 's/^/     | /'
  fi
}
check_not() { # description section pattern
  if section "$2" | grep -qF -- "$3"; then
    fail=$((fail + 1))
    printf 'FAIL %s\n' "$1"
    section "$2" | sed 's/^/     | /'
  else
    pass=$((pass + 1))
    printf 'ok   %s\n' "$1"
  fi
}

check "a fresh host gets the guard-only marker" fresh "fresh host: create /var/lib/muster-cloud/autodeploy/guard-only"
check "the drill's boot-time cleanup rule is installed" fresh "muster-cloud-drill.conf"
# autodeploy's first deploy runs host-setup before it writes `deployed`: no marker then.
check_not "called by autodeploy with no deployed record: no guard-only is created" autodeploy "create /var/lib/muster-cloud/autodeploy/guard-only"
check "...and says why" autodeploy "called by autodeploy"
check "any other caller value still gets the marker on a fresh host" other-caller "fresh host: create /var/lib/muster-cloud/autodeploy/guard-only"
check_not "a host that has deployed gets none" deployed "create /var/lib/muster-cloud/autodeploy/guard-only"
check "...and says why" deployed "has deployed before"
check_not "an existing guard-only is left alone" marker "create /var/lib/muster-cloud/autodeploy/guard-only"
check_not "an existing hold is left alone" hold "create /var/lib/muster-cloud/autodeploy/guard-only"

# The marker is decided before the timers are enabled.
order=$(grep -n 'fresh host: create\|enable and start \$timer\|change "enable and start' "$REPO/deploy/kamal/host-setup.sh" | cut -d: -f1 | paste -sd' ')
first=${order%% *}
last=${order##* }
if [ -n "$first" ] && [ -n "$last" ] && [ "$first" -lt "$last" ]; then
  pass=$((pass + 1))
  printf 'ok   guard-only is created before the timers are enabled (lines %s)\n' "$order"
else
  fail=$((fail + 1))
  printf 'FAIL guard-only must be created before the timers are enabled (lines %s)\n' "$order"
fi

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
