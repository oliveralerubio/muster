#!/usr/bin/env bash
# deploy/tests/autodeploy-guard.sh — tests of `muster-cloud-autodeploy --guard`
# (the liveness guard) with a fake docker, curl and alert command; no root, no
# Docker and no network needed.
#
#   deploy/tests/autodeploy-guard.sh
#
# Exit 0 when every case passes, 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
SCRIPT=${MUSTER_AUTODEPLOY_SCRIPT:-$REPO/deploy/kamal/muster-cloud-autodeploy}
SHA=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
SHORT=${SHA:0:12}
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/autodeploy-guard-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT

pass=0
fail=0
bg_pids=()

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
one_line_with() { [ "$(wc -l <<<"$out")" -eq 1 ] && has "$1"; }
only_readyz() { ! grep -v '/readyz' "$T/curl.log" | grep -q .; }
logged() { [ -s "$T/$1" ] && grep -qF -- "$2" "$T/$1"; }
not_logged() { ! { [ -s "$T/$1" ] && grep -qF -- "$2" "$T/$1"; }; }

# ---- a fresh sandbox ---------------------------------------------------------------
setup() { # name
  T=$ROOT/$1
  mkdir -p "$T/bin" "$T/sbin" "$T/c" "$T/state" "$T/source/bin"
  : >"$T/docker.log" && : >"$T/curl.log" && : >"$T/alert.log"
  printf 'running\n' >"$T/c/muster-cloud-db"
  printf 'running\n' >"$T/c/kamal-proxy"
  printf 'running\n' >"$T/c/muster-cloud-web-$SHA"
  printf '%s\n' "$SHA" >"$T/state/deployed"
  touch "$T/ready"
  cp "$REPO/bin/kamal" "$T/source/bin/kamal"

  cat >"$T/bin/docker" <<'EOF'
#!/usr/bin/env bash
# Fake docker. State: $T/c/<container> holds running|exited.
echo "docker $*" >>"$T/docker.log"
st() { cat "$T/c/$1" 2>/dev/null || echo missing; }
case "$1 $2" in
  ps*)
    case "$*" in
      *"--filter ancestor="*) [ ! -e "$T/kamal-running" ] || echo abc123def456 ;;
      *"{{.Image}}"*) [ ! -e "$T/kamal-running" ] || echo 'ghcr.io/basecamp/kamal:v2.12.0@sha256:0' ;;
      *" -a "*) # docker ps -a ... --format '{{.Names}} {{.State}}'
        for f in "$T"/c/muster-cloud-web-*; do
          [ -e "$f" ] && echo "${f##*/} $(cat "$f")"
        done
        ;;
      *) # running web containers
        for f in "$T"/c/muster-cloud-web-*; do
          [ -e "$f" ] && [ "$(cat "$f")" = running ] && echo "${f##*/}"
        done
        ;;
    esac
    exit 0
    ;;
  "inspect --type")
    name=${*: -1}
    [ -e "$T/c/$name" ] || exit 1
    st "$name"
    ;;
  "start "*)
    [ -e "$T/c/$2" ] || exit 1
    echo running >"$T/c/$2"
    ;;
  *) exit 1 ;;
esac
EOF
  cat >"$T/bin/curl" <<'EOF'
#!/usr/bin/env bash
echo "curl $*" >>"$T/curl.log"
case " $* " in
  *github*)
    echo "$*" >>"$T/github.log"
    [ ! -e "$T/github-wait" ] || { : >"$T/github-asked"; sleep 4; }
    exit 22
    ;;
  */readyz*) [ -e "$T/ready" ] && exit 0 || exit 22 ;;
esac
exit 22
EOF
  cat >"$T/sbin/muster-cloud-alert" <<'EOF'
#!/usr/bin/env bash
echo "$*" >>"$T/alert.log"
EOF
  chmod +x "$T/bin/docker" "$T/bin/curl" "$T/sbin/muster-cloud-alert"
}

# run_it args... -> out (stdout+stderr) and rc
run_it() {
  rc=0
  out=$(T=$T MUSTER_AUTODEPLOY_PATH=$T/bin MUSTER_AUTODEPLOY_ALLOW_NONROOT=1 \
    MUSTER_AUTODEPLOY_STATE=$T/state MUSTER_AUTODEPLOY_SOURCE=$T/source \
    MUSTER_AUTODEPLOY_RUN=$T/run MUSTER_AUTODEPLOY_LOCAL_URL=http://127.0.0.1:1 \
    MUSTER_AUTODEPLOY_GUARD_WAIT=5 MUSTER_SBIN=$T/sbin \
    MUSTER_GITHUB_API=http://github.invalid MUSTER_CLOUD_DEPLOY_ENV=$T/none.env \
    "$SCRIPT" "$@" 2>&1) || rc=$?
}

hold_lock() { # keeps the autodeploy flock until release_lock
  mkdir -p "$T/state"
  T=$T bash -c 'exec 9>"$T/state/lock"; flock 9; echo held >"$T/lock-held"; sleep 30' &
  bg_pids+=($!)
  for _ in $(seq 1 50); do [ -e "$T/lock-held" ] && return 0; sleep 0.1; done
  return 1
}
release_lock() {
  local p
  for p in "${bg_pids[@]}"; do
    pkill -P "$p" 2>/dev/null || true
    kill "$p" 2>/dev/null || true
    wait "$p" 2>/dev/null || true
  done
  bg_pids=()
}

# ---- cases ---------------------------------------------------------------------------
setup guard-starts-web
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it --guard
check "--guard with a stopped web container: exit 0" [ "$rc" -eq 0 ]
check "--guard starts the stopped web container" logged docker.log "docker start muster-cloud-web-$SHA"
check "--guard: web container is running again" grep -qx running "$T/c/muster-cloud-web-$SHA"
check "--guard logs the start in one decision line" has "decision: guard: started muster-cloud-web-$SHORT"
check "--guard never calls GitHub" test ! -e "$T/github.log"
check "--guard only ever calls curl for /readyz" only_readyz
check "--guard does not touch Docker images (no pull, no manifest)" not_logged docker.log "docker pull"

setup guard-starts-all-in-order
echo exited >"$T/c/muster-cloud-db"
echo exited >"$T/c/kamal-proxy"
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it --guard
order=$(grep '^docker start' "$T/docker.log" | tr '\n' ' ')
check "--guard starts db, proxy, web in that order" [ "$order" = "docker start muster-cloud-db docker start kamal-proxy docker start muster-cloud-web-$SHA " ]

setup guard-never-starts-older-web
OLD=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
echo exited >"$T/c/muster-cloud-web-$OLD"
run_it --guard
check "--guard never starts an older stopped web version" not_logged docker.log "docker start"

setup guard-quiet
run_it --guard
check "--guard with everything up: exit 0" [ "$rc" -eq 0 ]
check "--guard with everything up: no output at all" [ -z "$out" ]
check "--guard with everything up: starts nothing" not_logged docker.log "docker start"
check "--guard with everything up: no alert" [ ! -s "$T/alert.log" ]

setup guard-lock-held
echo exited >"$T/c/muster-cloud-web-$SHA"
if hold_lock; then
  run_it --guard
  release_lock
  check "--guard with the lock held: exit 0" [ "$rc" -eq 0 ]
  check "--guard with the lock held: one skip line" one_line_with "guard: skipped, a deploy or another autodeploy run holds the lock"
  check "--guard with the lock held: starts nothing" not_logged docker.log "docker start"
  check "--guard with the lock held: does not even ask Docker" [ ! -s "$T/docker.log" ]
else
  release_lock
  bad "could not take the lock for the test"
fi

setup guard-kamal-running
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-running"
run_it --guard
pin=$(sed -n "s/^KAMAL_IMAGE='\\([^'@]*\\).*/\\1/p" "$REPO/bin/kamal")
check "bin/kamal pins a ghcr.io/basecamp/kamal tag" [ "${pin#ghcr.io/basecamp/kamal:}" != "$pin" ]
check "--guard with Kamal running: exit 0" [ "$rc" -eq 0 ]
check "--guard with Kamal running: one skip line" one_line_with "guard: skipped, a Kamal container is running"
check "--guard with Kamal running: starts nothing" not_logged docker.log "docker start"
check "--guard asks Docker for containers of the image pinned in bin/kamal" logged docker.log "docker ps -q --filter ancestor=$pin"

setup guard-kamal-running-no-checkout
rm -rf "$T/source/bin"
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-running"
run_it --guard
check "--guard with Kamal running and no checkout: skips" has "guard: skipped, a Kamal container is running"
check "--guard with Kamal running and no checkout: starts nothing" not_logged docker.log "docker start"

setup guard-no-checkout-no-kamal
rm -rf "$T/source/bin"
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it --guard
check "--guard with no checkout and no Kamal: still starts the container" logged docker.log "docker start muster-cloud-web-$SHA"

setup full-run-kamal-running
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-running"
run_it
check "full run with Kamal running: exit 0" [ "$rc" -eq 0 ]
check "full run with Kamal running: its guard step skips" has "guard: skipped, a Kamal container is running"
check "full run with Kamal running: starts nothing" not_logged docker.log "docker start"

setup full-run-guards-then-github
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it
check "full run (no arguments) still runs the guard first" logged docker.log "docker start muster-cloud-web-$SHA"
check "full run (no arguments) still asks GitHub" test -s "$T/github.log"

setup full-run-holds-lock
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/github-wait"
(
  run_it
) &
bg_pids+=($!)
for _ in $(seq 1 50); do [ -e "$T/github-asked" ] && break; sleep 0.1; done
echo exited >"$T/c/muster-cloud-web-$SHA"
: >"$T/docker.log"
run_it --guard
check "--guard while a full run is busy: skips on the lock" has "holds the lock"
check "--guard while a full run is busy: starts nothing" not_logged docker.log "docker start"
release_lock

setup args
run_it --bogus
check "unknown argument: exit 64" [ "$rc" -eq 64 ]
check "unknown argument: nothing was asked of Docker" [ ! -s "$T/docker.log" ]
run_it --guard extra
check "--guard with an extra argument: exit 64" [ "$rc" -eq 64 ]
run_it -h
check "-h is an unknown argument: exit 64" [ "$rc" -eq 64 ]

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
