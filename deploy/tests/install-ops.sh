#!/usr/bin/env bash
# deploy/tests/install-ops.sh — tests that `muster-cloud-install-ops` never
# repoints /usr/local/bin/muster-cloud-ops to an image built BEFORE the one that
# is installed (an older ops can miss manifests a newer one wrote; retention
# then misses deletion deadlines), unless MUSTER_OPS_ALLOW_OLDER=1.
#
#   deploy/tests/install-ops.sh
#
# The script is root-only and writes /usr/local, so it runs in a user namespace
# (unshare, needs /etc/subuid for the caller) with tmpfs over /usr/local/lib and
# /usr/local/bin and a fake docker bind-mounted over /usr/bin/docker; the binary
# that gets "installed" is the real muster-cloud-ops of this checkout
# (target/debug or $MUSTER_OPS_TEST_BIN), because the script runs it. Where any of
# that is missing the test says so and exits 0.
#
# Exit 0 when every case passes (or is skipped), 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_INSTALL_OPS_SCRIPT:-$REPO/deploy/kamal/muster-cloud-install-ops}
OPS=${MUSTER_OPS_TEST_BIN:-$REPO/target/debug/muster-cloud-ops}
skip() {
  printf 'install-ops.sh: SKIPPED (%s)\n' "$1"
  exit 0
}
[ -x "$OPS" ] || skip "no muster-cloud-ops binary at $OPS (cargo build -p muster-cloud --bin muster-cloud-ops)"
[ -x /usr/bin/docker ] || skip "no /usr/bin/docker to mount the fake over"
[ -d /usr/local/lib ] && [ -d /usr/local/bin ] || skip "no /usr/local/lib or /usr/local/bin"

T=$(mktemp -d "${TMPDIR:-/tmp}/install-ops-test.XXXXXX")
trap 'rm -rf -- "$T"' EXIT
chmod 0755 "$T"
cp -- "$OPS" "$T/ops"
cp -- "$SCRIPT" "$T/install-ops"
chmod 0755 "$T/ops" "$T/install-ops"

# A fake docker: the image creation time of <sha> is in $T/created-<sha>.
cat >"$T/docker" <<'EOF'
#!/bin/sh
T=$FAKE_DOCKER_DIR
case "$1 $2" in
  "image inspect")
    if [ "$3" = --format ]; then
      ref=$5; sha=${ref##*:}
      [ -f "$T/created-$sha" ] || exit 1
      cat "$T/created-$sha"
    else
      sha=${3##*:}
      [ -f "$T/created-$sha" ] || exit 1
    fi ;;
  "create --pull=never") echo fake-cid ;;
  "cp fake-cid:/usr/local/bin/muster-cloud-ops") cp "$T/ops" "$3" ;;
  "rm -f") exit 0 ;;
  *) echo "fake docker: $*" >&2; exit 1 ;;
esac
EOF
chmod 0755 "$T/docker"
A=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
B=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
printf '2026-11-10T10:00:00.000000000Z\n' >"$T/created-$A"
printf '2026-11-18T10:00:00.000000000Z\n' >"$T/created-$B"

cat >"$T/inner.sh" <<'EOF'
T=$1
export FAKE_DOCKER_DIR=$T
mount -t tmpfs none /usr/local/lib || exit 90
mount -t tmpfs none /usr/local/bin || exit 90
mount --bind "$T/docker" /usr/bin/docker || exit 90
inst() { # sha [env...]
  local sha=$1
  shift
  env "$@" "$T/install-ops" "localhost:5000/oliveralerubio/muster-cloud:$sha" 2>&1
  echo "rc=$?"
}
echo "== first install of the old one"
inst $A_SHA
echo "== the newer one"
inst $B_SHA
echo "link: $(readlink /usr/local/bin/muster-cloud-ops)"
echo "== the older one is refused"
inst $A_SHA
echo "link: $(readlink /usr/local/bin/muster-cloud-ops)"
echo "== the same one again"
inst $B_SHA
echo "== the older one on purpose"
inst $A_SHA MUSTER_OPS_ALLOW_OLDER=1
echo "link: $(readlink /usr/local/bin/muster-cloud-ops)"
echo "== the newer one after that"
inst $B_SHA
echo "== end"
EOF

out=$(A_SHA=$A B_SHA=$B unshare --user --mount --map-users=100000,0,65536 --map-groups=100000,0,65536 --setuid=0 --setgid=0 \
  bash "$T/inner.sh" "$T" 2>&1) || skip "no usable user namespace: $(printf '%s' "$out" | head -n 2 | tr '\n' ' ')"

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

check "the first install works" "first install of the old one" "rc=0"
check "a newer image is installed" "the newer one" "rc=0"
check "...and the link follows it" "the newer one" "link: /usr/local/lib/muster-cloud/$B/muster-cloud-ops"
check "an older image is refused (65)" "the older one is refused" "rc=65"
check "...and says why" "the older one is refused" "older than the installed"
check "...and the link stays on the newest" "the older one is refused" "link: /usr/local/lib/muster-cloud/$B/muster-cloud-ops"
check "installing the same version again is fine" "the same one again" "rc=0"
check "MUSTER_OPS_ALLOW_OLDER=1 goes back on purpose" "the older one on purpose" "link: /usr/local/lib/muster-cloud/$A/muster-cloud-ops"
check "going forward again after that works" "the newer one after that" "rc=0"

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
