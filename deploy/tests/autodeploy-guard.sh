#!/usr/bin/env bash
# deploy/tests/autodeploy-guard.sh — tests of `muster-cloud-autodeploy` (the
# liveness guard, what counts as "running", the proxy/database bootstrap and the
# incomplete-deploy detection) with a fake docker, curl, Kamal and alert
# command; no root, no Docker and no network needed (git and python3 are).
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
OLD=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
ROOT=$(mktemp -d "${TMPDIR:-/tmp}/autodeploy-guard-test.XXXXXX")
trap 'rm -rf -- "$ROOT"' EXIT

for tool in git python3 flock; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'autodeploy-guard.sh: %s is required\n' "$tool" >&2
    exit 2
  }
done

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
lacks() { ! grep -qF -- "$1" <<<"$out"; }
one_line_with() { [ "$(wc -l <<<"$out")" -eq 1 ] && has "$1"; }
only_readyz() { ! grep -v '/readyz' "$T/curl.log" | grep -q .; }
logged() { [ -s "$T/$1" ] && grep -qF -- "$2" "$T/$1"; }
not_logged() { ! { [ -s "$T/$1" ] && grep -qF -- "$2" "$T/$1"; }; }
recorded_is() { [ "$(cat "$T/state/deployed" 2>/dev/null)" = "$1" ]; }
routed_is() { [ "$(cat "$T/route" 2>/dev/null)" = "muster-cloud-web-$1" ]; }

# ---- a fresh sandbox ---------------------------------------------------------------
setup() { # name
  T=$ROOT/$1
  mkdir -p "$T/bin" "$T/sbin" "$T/c" "$T/state" "$T/source/bin"
  : >"$T/docker.log" && : >"$T/curl.log" && : >"$T/alert.log" && : >"$T/kamal.log"
  printf 'running\n' >"$T/c/muster-cloud-db"
  printf 'running\n' >"$T/c/kamal-proxy"
  printf 'running\n' >"$T/c/muster-cloud-web-$SHA"
  printf '%s\n' "$SHA" >"$T/state/deployed"
  printf 'muster-cloud-web-%s\n' "$SHA" >"$T/route" # what kamal-proxy routes muster-cloud-web to
  touch "$T/ready"
  cp "$REPO/bin/kamal" "$T/source/bin/kamal"

  cat >"$T/bin/docker" <<'EOF'
#!/usr/bin/env bash
# Fake docker. State: $T/c/<container> holds running|exited; $T/route holds the
# container kamal-proxy routes muster-cloud-web to (its id is "id-<name>").
echo "docker $*" >>"$T/docker.log"
st() { cat "$T/c/$1" 2>/dev/null || echo missing; }
case "$1 $2" in
  ps*)
    case "$*" in
      *"label=muster.kamal=1"*) [ ! -e "$T/kamal-labeled" ] || echo abc123def456 ;;
      *"{{.Image}}"*) [ ! -s "$T/kamal-image" ] || cat "$T/kamal-image" ;;
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
    case "$*" in
      *"{{.Name}}"*)
        name=${name#id-}
        [ -e "$T/c/$name" ] || exit 1
        echo "/$name"
        ;;
      *)
        [ -e "$T/c/$name" ] || exit 1
        st "$name"
        ;;
    esac
    ;;
  "exec kamal-proxy")
    [ "$(st kamal-proxy)" = running ] || exit 1
    if [ -s "$T/route" ]; then
      printf '{\n  "muster-cloud-web": {\n    "targets": [\n      "id-%s:8080"\n    ],\n    "read_targets": []\n  }\n}\n' "$(cat "$T/route")"
    else
      echo '{}'
    fi
    ;;
  "exec muster-cloud-db") [ "$(st muster-cloud-db)" = running ] || exit 1 ;;
  "start "*)
    [ -e "$T/c/$2" ] || exit 1
    echo running >"$T/c/$2"
    ;;
  "manifest inspect" | "pull "* | "tag "* | "image rm") exit 0 ;;
  "image inspect")
    case "$*" in
      *revision*) cat "$T/revision" ;;
      *) echo muster-cloud ;;
    esac
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
    if [ -s "$T/github-head" ]; then
      sha=$(cat "$T/github-head")
      case "$*" in
        */commits/main*) printf '%s' "$sha"; exit 0 ;;
        */actions/runs*)
          printf '{"workflow_runs":[{"name":"Cloud CI","event":"push","head_branch":"main","head_sha":"%s","head_repository":{"full_name":"oliveralerubio/muster"},"status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]}' "$sha"
          exit 0
          ;;
      esac
    fi
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
  cat >"$T/sbin/muster-cloud-install-ops" <<'EOF'
#!/usr/bin/env bash
echo "install-ops $*" >>"$T/alert.log"
EOF
  # Only the deploy cases get this far: run_kamal asks for gha's home, runs
  # through runuser and chowns the export.
  printf '#!/usr/bin/env bash\necho "gha:x:1000:1000::$T:/bin/bash"\n' >"$T/bin/getent"
  printf '#!/usr/bin/env bash\nwhile [ "$1" != -- ]; do shift; done\nshift\nexec "$@"\n' >"$T/bin/runuser"
  printf '#!/usr/bin/env bash\nexit 0\n' >"$T/bin/chown"
  chmod +x "$T/bin/"* "$T/sbin/"*
}

# A real git origin whose only commit ($REL) carries a fake bin/kamal and a
# host-setup.sh that does nothing. The fake Kamal logs to $HOME/kamal.log ($HOME
# is the sandbox: run_kamal's getent says so) and moves the fake containers:
# `app boot --version X` needs kamal-proxy, starts X, stops every other web
# container and routes the proxy to X.
setup_deploy() { # name
  setup "$1"
  rm -rf "$T/source"
  mkdir -p "$T/work/bin" "$T/work/deploy/kamal"
  cat >"$T/work/bin/kamal" <<'EOF'
#!/usr/bin/env bash
S=$HOME
echo "kamal $*" >>"$S/kamal.log"
ver=${*: -1}
case "$1 $2" in
  "proxy boot")
    [ ! -e "$S/fail-proxy-boot" ] || exit 1
    echo running >"$S/c/kamal-proxy"
    ;;
  "accessory boot") echo running >"$S/c/muster-cloud-db" ;;
  "app boot")
    [ "$(cat "$S/c/kamal-proxy" 2>/dev/null)" = running ] || exit 1
    for f in "$S"/c/muster-cloud-web-*; do [ -e "$f" ] && echo exited >"$f"; done
    echo running >"$S/c/muster-cloud-web-$ver"
    echo "muster-cloud-web-$ver" >"$S/route"
    touch "$S/ready"
    ;;
esac
EOF
  printf '#!/bin/sh\nexit 0\n' >"$T/work/deploy/kamal/host-setup.sh"
  chmod +x "$T/work/bin/kamal" "$T/work/deploy/kamal/host-setup.sh"
  git -C "$T/work" init -q -b main
  git -C "$T/work" add -A
  git -C "$T/work" -c user.name=test -c user.email=test@example.invalid -c commit.gpgsign=false commit -q -m release
  git clone -q --bare "$T/work" "$T/origin.git"
  REL=$(git -C "$T/work" rev-parse HEAD)
  printf '%s\n' "$REL" >"$T/github-head"
  printf '%s\n' "$REL" >"$T/revision"
}
reset_containers() { # leaves no container and nothing routed or recorded
  rm -f "$T"/c/* "$T/state/deployed"
  : >"$T/route"
}
kamal_order() { sed 's/^kamal \([a-z_]* [a-z_]*\).*/\1/' "$T/kamal.log" | tr '\n' ','; }

# run_it args... -> out (stdout+stderr) and rc
run_it() {
  rc=0
  out=$(T=$T MUSTER_AUTODEPLOY_PATH=$T/bin MUSTER_AUTODEPLOY_ALLOW_NONROOT=1 \
    MUSTER_AUTODEPLOY_STATE=$T/state MUSTER_AUTODEPLOY_SOURCE=$T/source \
    MUSTER_AUTODEPLOY_RUN=$T/run MUSTER_AUTODEPLOY_LOCAL_URL=http://127.0.0.1:1 \
    MUSTER_AUTODEPLOY_GUARD_WAIT=5 MUSTER_AUTODEPLOY_DB_WAIT=5 MUSTER_SBIN=$T/sbin \
    MUSTER_GITHUB_API=http://github.invalid MUSTER_GITHUB_URL=$T/origin.git \
    MUSTER_CLOUD_DEPLOY_ENV=$T/none.env \
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

# ---- the liveness guard ----------------------------------------------------------------
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
check "--guard with the proxy stopped still starts the recorded web container" logged docker.log "docker start muster-cloud-web-$SHA"

setup guard-never-starts-older-web
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

# ---- what the guard records as deployed ---------------------------------------------------
# A deploy cut short after the new container (SHA, which sorts first) was created
# but before the proxy switched from OLD: both run, the proxy routes to OLD.
setup guard-records-routed-not-listed-first
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route"
run_it --guard
check "--guard with an unrouted container created first: the record is untouched" recorded_is "$OLD"
check "--guard with an unrouted container created first: starts nothing" not_logged docker.log "docker start"
check "--guard with an unrouted container created first: silent" [ -z "$out" ]

setup guard-two-running-record-stays
# The proxy already switched to SHA, the old OLD still runs, record says OLD: the
# full run decides, the guard must not bless SHA.
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
run_it --guard
check "--guard with two web containers and a routed one that is not the record: record stays" recorded_is "$OLD"

setup guard-records-single-routed
printf '%s\n' "$OLD" >"$T/state/deployed"
run_it --guard
check "--guard with one running container the proxy routes to: it becomes the record" recorded_is "$SHA"

setup guard-never-records-unrouted
printf '%s\n' "$OLD" >"$T/state/deployed"
: >"$T/route"
run_it --guard
check "--guard never records a running container the proxy does not route to" recorded_is "$OLD"

setup guard-never-records-ghost-route
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route" # routed to a container that no longer exists
run_it --guard
check "--guard never records a route to a container that does not exist" recorded_is "$OLD"

setup guard-never-records-while-deploying
printf '%s\n' "$OLD" >"$T/state/deployed"
printf '%s\n' "$SHA" >"$T/state/deploying"
run_it --guard
check "--guard never records while a cut-short deploy is unverified" recorded_is "$OLD"

setup guard-proxy-down-record-stands
printf '%s\n' "$OLD" >"$T/state/deployed"
echo exited >"$T/c/kamal-proxy"
run_it --guard
check "--guard with the proxy not answering: the record stands" recorded_is "$OLD"

# ---- a Kamal run in progress (fix: detected by label or image, not by `ancestor=`) ------------
setup guard-kamal-running-label
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-labeled"
run_it --guard
pin=$(sed -n "s/^KAMAL_IMAGE='\\([^'@]*\\).*/\\1/p" "$REPO/bin/kamal")
check "bin/kamal pins a ghcr.io/basecamp/kamal tag" [ "${pin#ghcr.io/basecamp/kamal:}" != "$pin" ]
check "--guard with a labelled Kamal container running: exit 0" [ "$rc" -eq 0 ]
check "--guard with a labelled Kamal container running: one skip line" one_line_with "guard: skipped, a Kamal container is running"
check "--guard with a labelled Kamal container running: starts nothing" not_logged docker.log "docker start"
check "--guard asks Docker for containers with the bin/kamal label" logged docker.log "docker ps -q --filter label=muster.kamal=1"
check "--guard no longer looks Kamal up with ancestor=" not_logged docker.log "ancestor="

setup guard-kamal-running-digest-image
echo exited >"$T/c/muster-cloud-web-$SHA"
printf 'ghcr.io/basecamp/kamal@sha256:7b5be276aa17bbe122887f6a1ff12865f4848111989699b9786083be95d98415\n' >"$T/kamal-image"
run_it --guard
check "--guard with a Kamal container started by digest (no tag, no label): skips" one_line_with "guard: skipped, a Kamal container is running"
check "--guard with a Kamal container started by digest: starts nothing" not_logged docker.log "docker start"

setup guard-kamal-running-older-version
echo exited >"$T/c/muster-cloud-web-$SHA"
printf 'ghcr.io/basecamp/kamal:v2.11.0\n' >"$T/kamal-image"
run_it --guard
check "--guard with another Kamal version running: skips" one_line_with "guard: skipped, a Kamal container is running"

setup guard-kamal-lookalike-image
echo exited >"$T/c/muster-cloud-web-$SHA"
printf 'basecamp/kamal-proxy:v0.10.3\nghcr.io/basecamp/kamal-other:1\n' >"$T/kamal-image"
run_it --guard
check "--guard with only kamal-proxy and lookalike images running: still guards" logged docker.log "docker start muster-cloud-web-$SHA"

setup guard-kamal-running-no-checkout
rm -rf "$T/source/bin"
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-labeled"
run_it --guard
check "--guard with Kamal running and no checkout: skips" has "guard: skipped, a Kamal container is running"
check "--guard with Kamal running and no checkout: starts nothing" not_logged docker.log "docker start"

setup guard-no-checkout-no-kamal
rm -rf "$T/source/bin"
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it --guard
check "--guard with no checkout and no Kamal: still starts the container" logged docker.log "docker start muster-cloud-web-$SHA"

setup kamal-wrapper-label
PATH="$T/bin:$PATH" MUSTER_CLOUD_DEPLOY_ENV="$T/none.env" MUSTER_CLOUD_SECRETS_FILE="$T/none.secrets" T="$T" \
  "$REPO/bin/kamal" config >/dev/null 2>&1 || true
check "bin/kamal runs its container with the label the guard looks for" logged docker.log "--label muster.kamal=1"

setup full-run-kamal-running
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/kamal-labeled"
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

# ---- the full run: what counts as running -------------------------------------------------------
setup_deploy full-noop
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$REL" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
rm -f "$T/c/muster-cloud-web-$SHA"
run_it
check "full run, HEAD is routed and recorded: no-op" has "decision: no-op: main HEAD ${REL:0:12} is already running"
check "full run, HEAD is routed and recorded: Kamal not touched" [ ! -s "$T/kamal.log" ]

# The deploy was cut short after the new container (REL) was created but before the
# proxy left OLD. The container REL exists and runs, the proxy routes to OLD.
setup_deploy full-cut-before-switch
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route"
run_it
check "full run, new container exists but the proxy still routes to the old one: not a no-op" lacks "no-op"
check "full run, new container exists but the proxy still routes to the old one: boots HEAD" logged kamal.log "kamal app boot --version $REL"
check "full run, new container exists but the proxy still routes to the old one: now routed to HEAD" routed_is "$REL"
check "full run, new container exists but the proxy still routes to the old one: deployed" has "decision: deployed ${REL:0:12}"
check "full run, new container exists but the proxy still routes to the old one: recorded" recorded_is "$REL"

# The proxy switched to REL, the old container (OLD) still runs and the record
# still says OLD: the checks after the switch never ran.
setup_deploy full-cut-after-switch
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
run_it
check "full run, two web containers and the routed one is not the record: incomplete deploy" has "decision: incomplete deploy: 2 web containers run, the proxy routes to ${REL:0:12} but the deployed version is recorded as ${OLD:0:12}"
check "full run, incomplete deploy: never a silent no-op" lacks "no-op"
check "full run, incomplete deploy: boots HEAD again through the normal path" logged kamal.log "kamal app boot --version $REL"
check "full run, incomplete deploy: verified and recorded" recorded_is "$REL"
check "full run, incomplete deploy: the old container is stopped" grep -qx exited "$T/c/muster-cloud-web-$OLD"

setup_deploy full-cut-routed-not-ready
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$REL" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
rm -f "$T/ready"
run_it
check "full run, two web containers and the routed one is not ready: incomplete deploy" has "does not answer /readyz; will boot ${REL:0:12} again"
check "full run, routed not ready: boots HEAD again" logged kamal.log "kamal app boot --version $REL"

setup_deploy full-two-running-benign
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$REL" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
run_it
check "full run, two web containers but the routed one is the record and ready: no-op" has "decision: no-op: main HEAD ${REL:0:12} is already running"

setup_deploy full-marker
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
printf '%s\n' "$REL" >"$T/state/deploying"
run_it
check "full run, a previous deploy left its marker: incomplete deploy" has "decision: incomplete deploy: a previous deploy was cut short while booting ${REL:0:12}"
check "full run, marker: boots HEAD again" logged kamal.log "kamal app boot --version $REL"
check "full run, marker: removed after the verified deploy" [ ! -e "$T/state/deploying" ]
check "full run, marker: recorded after the verified deploy" recorded_is "$REL"

setup_deploy full-proxy-down
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$REL" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
echo exited >"$T/c/kamal-proxy"
mv "$T/bin/docker" "$T/bin/docker.real"
cat >"$T/bin/docker" <<'EOF'
#!/usr/bin/env bash
# `docker start kamal-proxy` fails: the proxy stays down.
[ "$1 $2" != "start kamal-proxy" ] || { echo "docker $*" >>"$T/docker.log"; exit 1; }
exec "$T/bin/docker.real" "$@"
EOF
chmod +x "$T/bin/docker"
run_it
check "full run, the proxy does not answer: never a no-op" lacks "no-op"

# ---- the full run: proxy and database bootstrap ---------------------------------------------------
setup_deploy bootstrap-fresh-host
reset_containers
run_it
check "fresh host: exit 0" [ "$rc" -eq 0 ]
check "fresh host: logs the proxy decision" has "decision: no kamal-proxy container exists; booting it with Kamal (proxy boot)"
check "fresh host: logs the database decision" has "decision: no muster-cloud-db container exists; booting it with Kamal (accessory boot db)"
check "fresh host: Kamal runs proxy boot, accessory boot db, then the app" [ "$(kamal_order)" = "proxy boot,accessory boot,app stale_containers,app boot,prune all," ]
check "fresh host: deployed and recorded" recorded_is "$REL"

setup_deploy bootstrap-existing-host
run_it
check "existing proxy and database: exit 0" [ "$rc" -eq 0 ]
check "existing proxy and database: no proxy boot" not_logged kamal.log "kamal proxy boot"
check "existing proxy and database: no accessory boot" not_logged kamal.log "kamal accessory boot"
check "existing proxy and database: no bootstrap decision" lacks "booting it with Kamal"
check "existing proxy and database: only the app is booted" [ "$(kamal_order)" = "app stale_containers,app boot,prune all," ]

setup_deploy bootstrap-stopped-proxy-is-not-recreated
echo exited >"$T/c/kamal-proxy"
echo exited >"$T/c/muster-cloud-db"
run_it
check "a stopped proxy or database is the guard's business, not a bootstrap" not_logged kamal.log "kamal proxy boot"

setup_deploy bootstrap-only-proxy-missing
rm -f "$T/c/kamal-proxy"
run_it
check "only the proxy missing: proxy boot runs" logged kamal.log "kamal proxy boot"
check "only the proxy missing: no accessory boot" not_logged kamal.log "kamal accessory boot"

setup_deploy bootstrap-only-db-missing
rm -f "$T/c/muster-cloud-db"
run_it
check "only the database missing: accessory boot db runs" logged kamal.log "kamal accessory boot db"
check "only the database missing: no proxy boot" not_logged kamal.log "kamal proxy boot"
check "only the database missing: waits for it with pg_isready" logged docker.log "docker exec muster-cloud-db pg_isready"

setup_deploy bootstrap-proxy-boot-fails
reset_containers
touch "$T/fail-proxy-boot"
run_it
check "proxy boot failing: the existing failure path (exit 75)" [ "$rc" -eq 75 ]
check "proxy boot failing: alerted with the stage" logged alert.log "FAILED while booting kamal-proxy"
check "proxy boot failing: the app is never booted" not_logged kamal.log "kamal app boot"
check "proxy boot failing: no marker, nothing was switched" [ ! -e "$T/state/deploying" ]
check "proxy boot failing: nothing recorded" [ ! -e "$T/state/deployed" ]

# ---- arguments ----------------------------------------------------------------------------------
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
