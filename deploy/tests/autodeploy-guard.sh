#!/usr/bin/env bash
# deploy/tests/autodeploy-guard.sh — tests of `muster-cloud-autodeploy` (the
# liveness guard, what counts as "running", the proxy/database bootstrap, the
# incomplete-deploy detection, hold (also mid-run, --hold, --release), Kamal
# ownership and deploy-lock recovery, routes that cannot be read, the retry limit
# and the redeploy request) with a fake docker, curl, Kamal and alert command; no
# root, no Docker and no network needed (git and python3 are). The fakes model
# what the script relies on: Kamal 2.12's deploy lock directory (`mkdir`, then
# `details`; `lock release` is `rm details && rm -r dir`, so it fails on a lock
# without `details`), `app boot` renaming a container of the same version and
# stopping only the container the `latest` tag selects, `app stale_containers
# --stop` stopping everything but that one, and docker's per-container network
# address, which the candidate probe of an unreadable-routes deploy reaches.
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
count_of() { grep -cF -- "$2" "$T/$1" 2>/dev/null || true; }
not_logged() { ! { [ -s "$T/$1" ] && grep -qF -- "$2" "$T/$1"; }; }
recorded_is() { [ "$(cat "$T/state/deployed" 2>/dev/null)" = "$1" ]; }
routed_is() { [ "$(cat "$T/route" 2>/dev/null)" = "muster-cloud-web-$1" ]; }

# ---- a fresh sandbox ---------------------------------------------------------------
setup() { # name
  T=$ROOT/$1
  mkdir -p "$T/bin" "$T/sbin" "$T/c" "$T/state" "$T/source/bin"
  : >"$T/docker.log" && : >"$T/curl.log" && : >"$T/alert.log" && : >"$T/kamal.log" && : >"$T/stop.log"
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
      *"{{.Image}}|"*) # running Kamal containers: lines "image|label muster.kamal|label muster.kamal.owner"
        [ ! -e "$T/docker-ps-fails" ] || exit 1
        [ ! -s "$T/kamal-containers" ] || cat "$T/kamal-containers"
        ;;
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
      *"State.Health"*)
        [ -e "$T/c/$name" ] || exit 1
        cat "$T/health-$name" 2>/dev/null || echo none
        ;;
      *"NetworkSettings"*) # the container's address on the kamal network (the probe curls it)
        [ -e "$T/c/$name" ] || exit 1
        [ ! -e "$T/no-address" ] || exit 0 # on no network: an empty address
        ip="10.77.$(($(printf %s "$name" | cksum | cut -d' ' -f1) % 200 + 1)).7"
        echo "$name" >"$T/ipmap-$ip"
        echo "$ip"
        ;;
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
    [ ! -e "$T/routes-unreadable" ] || exit 1
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
  "stop "*) # docker stop [--time N] NAME; stop.log says whether it was the routed container
    name=${*: -1}
    [ -e "$T/c/$name" ] || exit 1
    routed=0
    [ "$(cat "$T/route" 2>/dev/null)" != "$name" ] || routed=1
    echo "stop $name routed=$routed by=docker" >>"$T/stop.log"
    echo exited >"$T/c/$name"
    # the proxy's route moves, or its routes become unreadable, while the cleanup runs
    [ ! -s "$T/route-after-stop" ] || cp "$T/route-after-stop" "$T/route"
    [ ! -e "$T/unreadable-after-stop" ] || touch "$T/routes-unreadable"
    ;;
  "pull "*)
    [ ! -e "$T/fail-pull" ] || exit 1
    rm -f "$T/no-local-image" # after a pull the image is here
    [ ! -e "$T/hold-on-pull" ] || touch "$T/state/hold" # the operator sets the hold while the run pulls
    # another Kamal run starts while this one is between its checks
    [ ! -e "$T/kamal-appears-on-pull" ] || echo 'ghcr.io/basecamp/kamal:v2.12.0|1|manual' >"$T/kamal-containers"
    exit 0
    ;;
  "manifest inspect" | "tag "* | "image rm") exit 0 ;;
  "image inspect")
    [ ! -e "$T/no-local-image" ] || exit 1
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
    [ ! -e "$T/github-wait" ] || { : >"$T/github-asked"; sleep 2; }
    if [ -s "$T/github-head" ]; then
      sha=$(cat "$T/github-head")
      case "$*" in
        */commits/main*) printf '%s' "$sha"; exit 0 ;;
        */actions/runs*)
          if [ -e "$T/github-ci-failed" ]; then
            printf '{"workflow_runs":[{"name":"Cloud CI","event":"push","head_branch":"main","head_sha":"%s","head_repository":{"full_name":"oliveralerubio/muster"},"status":"completed","conclusion":"failure","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]}' "$sha"
            exit 0
          fi
          printf '{"workflow_runs":[{"name":"Cloud CI","event":"push","head_branch":"main","head_sha":"%s","head_repository":{"full_name":"oliveralerubio/muster"},"status":"completed","conclusion":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}]}' "$sha"
          exit 0
          ;;
      esac
    fi
    exit 22
    ;;
  *://10.77.*) # a container probed directly at its address: 200 when it runs and is not marked down
    ip=$(sed 's|.*://\([0-9.]*\):.*|\1|' <<<"$*")
    name=$(cat "$T/ipmap-$ip" 2>/dev/null) || exit 7
    [ "$(cat "$T/c/$name" 2>/dev/null)" = running ] || exit 7
    [ ! -e "$T/cand-down-$name" ] || { case "$*" in *http_code*) printf 503 ;; esac; exit 0; }
    case "$*" in *http_code*) printf 200 ;; esac
    exit 0
    ;;
  *ts.net*) # the public URL
    [ ! -e "$T/public-down" ] || exit 22
    exit 0
    ;;
  */readyz*) # ready, unless the container kamal-proxy routes to is stopped
    [ -e "$T/ready" ] || exit 22
    r=$(cat "$T/route" 2>/dev/null)
    [ -z "$r" ] || [ "$(cat "$T/c/$r" 2>/dev/null)" != exited ] || exit 22
    exit 0
    ;;
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
[ ! -s "$T/route-after-ops" ] || cp "$T/route-after-ops" "$T/route" # the route moves after the checks
[ ! -e "$T/fail-install-ops" ] || exit 1
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
# is the sandbox: run_kamal's getent says so), records the owner label it was
# given and moves the fake containers like Kamal 2.12:
#   - the commands that take the deploy lock `mkdir` it (fail when it exists)
#     and release it when they end, on success or error; `lock status` prints
#     "Locked by: <who> at <iso8601>" or "There is no deploy lock";
#   - `app boot --version X` needs kamal-proxy, renames a container of version X
#     out of the way, starts X, routes the proxy to X and then stops ONLY the
#     "current" container, which Kamal picks by the `latest` image tag ($T/latest,
#     else the routed one), then moves the tag to X;
#   - `app stale_containers --stop` stops every running web container except the
#     one the `latest` tag selects (it can be the routed one's victim);
#   - `rollback V` boots the existing container V the same way.
setup_deploy() { # name
  setup "$1"
  touch "$T/no-local-image" # a new commit's image is not on the host yet
  rm -rf "$T/source"
  mkdir -p "$T/work/bin" "$T/work/deploy/kamal"
  cat >"$T/work/bin/kamal" <<'EOF'
#!/usr/bin/env bash
S=$HOME
echo "kamal $*" >>"$S/kamal.log"
echo "owner=${MUSTER_KAMAL_OWNER:-}" >>"$S/kamal-owner.log"
ver=${*: -1}
LOCK=$S/kamal-lock
stop_c() { # name: what Kamal's `docker stop` does; stop.log says whether it was routed
  local routed=0
  [ "$(cat "$S/route" 2>/dev/null)" != "$1" ] || routed=1
  echo "stop $1 routed=$routed by=kamal" >>"$S/stop.log"
  echo exited >"$S/c/$1"
}
locked() {
  if ! mkdir "$LOCK" 2>/dev/null; then
    echo "Deploy lock found. Run 'kamal lock help' for more information" >&2
    exit 1
  fi
  printf 'Locked by: test at %s\nVersion: %s\nMessage: Automatic deploy lock\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$ver" | base64 >"$LOCK/details"
  trap 'rm -rf "$LOCK"' EXIT
}
latest_version() { # the version Kamal calls current: the `latest` tag, else what is routed
  local v
  v=$(cat "$S/latest" 2>/dev/null || true)
  [ -n "$v" ] || v=$(sed 's/^muster-cloud-web-//' "$S/route" 2>/dev/null || true)
  printf '%s' "$v"
}
boot_version() { # version
  local v=$1 old
  [ "$(cat "$S/c/kamal-proxy" 2>/dev/null)" = running ] || exit 1
  [ ! -e "$S/retouch-redeploy" ] || touch "$S/state/redeploy" # a new redeploy request arrives during the boot
  [ ! -e "$S/fail-app-boot" ] || exit 1
  old=$(latest_version)
  if [ -e "$S/c/muster-cloud-web-$v" ]; then # a container of this version exists: renamed out of the way
    mv "$S/c/muster-cloud-web-$v" "$S/c/muster-cloud-web-${v}_replaced_1"
    [ "$old" != "$v" ] || old=${v}_replaced_1
  fi
  echo running >"$S/c/muster-cloud-web-$v"
  echo "muster-cloud-web-$v" >"$S/route"
  touch "$S/ready"
  [ ! -e "$S/hold-after-boot" ] || touch "$S/state/hold" # the operator sets the hold while Kamal boots
  if [ -n "$old" ] && [ "$old" != "$v" ] && [ -e "$S/c/muster-cloud-web-$old" ]; then
    stop_c "muster-cloud-web-$old"
  fi
  echo "$v" >"$S/latest"
}
case "$1 $2" in
  "lock status") # Kamal 2.12: stat dir && cat details | base64 -d; a missing file reads as "no lock"
    if [ -f "$LOCK/details" ]; then base64 -d "$LOCK/details"; else echo "There is no deploy lock"; fi
    ;;
  "lock release") # Kamal 2.12: rm details && rm -r dir (the script never calls it: see lock_recover)
    if rm "$LOCK/details" 2>/dev/null && rm -r "$LOCK"; then echo "Released the deploy lock"; else echo "There is no deploy lock"; fi
    ;;
  "proxy boot")
    locked
    [ ! -e "$S/fail-proxy-boot" ] || exit 1
    echo running >"$S/c/kamal-proxy"
    ;;
  "accessory boot")
    locked
    [ ! -e "$S/fail-accessory-boot" ] || exit 1
    echo running >"$S/c/muster-cloud-db"
    ;;
  "app boot")
    locked
    boot_version "$ver"
    ;;
  "app stale_containers")
    locked
    keep=$(latest_version)
    for f in "$S"/c/muster-cloud-web-*; do
      [ -e "$f" ] && [ "$(cat "$f")" = running ] && [ "${f##*/}" != "muster-cloud-web-$keep" ] && stop_c "${f##*/}"
    done
    ;;
  "rollback "*)
    locked
    [ -e "$S/c/muster-cloud-web-$2" ] || exit 1
    old=$(latest_version)
    echo running >"$S/c/muster-cloud-web-$2"
    echo "muster-cloud-web-$2" >"$S/route"
    if [ -n "$old" ] && [ "$old" != "$2" ] && [ -e "$S/c/muster-cloud-web-$old" ]; then stop_c "muster-cloud-web-$old"; fi
    echo "$2" >"$S/latest"
    ;;
  "prune all") locked ;;
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
checkout_source() { # the root-owned checkout the guard runs Kamal from, at $REL
  git clone -q "$T/origin.git" "$T/source" && git -C "$T/source" -c advice.detachedHead=false checkout -q --detach "$REL"
}
kamal_ps() { printf '%s\n' "$@" >"$T/kamal-containers"; } # the running Kamal containers: image|label|owner
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
    MUSTER_AUTODEPLOY_RUN=$T/run MUSTER_AUTODEPLOY_LOCAL_URL=http://127.0.0.1:1 MUSTER_AUTODEPLOY_LOCK_DIR=$T/kamal-lock \
    MUSTER_AUTODEPLOY_GUARD_WAIT=5 MUSTER_AUTODEPLOY_DB_WAIT=5 MUSTER_SBIN=$T/sbin \
    MUSTER_AUTODEPLOY_BOOT_TIME=${TEST_BOOT_TIME:-1} MUSTER_AUTODEPLOY_INFRA_GAP=${TEST_INFRA_GAP:-300} \
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
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0@sha256:7b5be276aa17bbe122887f6a1ff12865f4848111989699b9786083be95d98415|1|manual'
run_it --guard
pin=$(sed -n "s/^KAMAL_IMAGE='\\([^'@]*\\).*/\\1/p" "$REPO/bin/kamal")
check "bin/kamal pins a ghcr.io/basecamp/kamal tag" [ "${pin#ghcr.io/basecamp/kamal:}" != "$pin" ]
check "--guard with a labelled Kamal container running: exit 0" [ "$rc" -eq 0 ]
check "--guard with a labelled Kamal container running: one skip line" one_line_with "guard: skipped, a Kamal container is running"
check "--guard with a labelled Kamal container running: starts nothing" not_logged docker.log "docker start"
check "--guard asks Docker for the bin/kamal label and owner" logged docker.log 'muster.kamal.owner'
check "--guard no longer looks Kamal up with ancestor=" not_logged docker.log "ancestor="

setup guard-kamal-running-digest-image
echo exited >"$T/c/muster-cloud-web-$SHA"
kamal_ps 'ghcr.io/basecamp/kamal@sha256:7b5be276aa17bbe122887f6a1ff12865f4848111989699b9786083be95d98415||'
run_it --guard
check "--guard with a Kamal container started by digest (no tag, no label): skips" one_line_with "guard: skipped, a Kamal container is running"
check "--guard with a Kamal container started by digest: starts nothing" not_logged docker.log "docker start"

setup guard-kamal-running-older-version
echo exited >"$T/c/muster-cloud-web-$SHA"
kamal_ps 'ghcr.io/basecamp/kamal:v2.11.0||'
run_it --guard
check "--guard with another Kamal version running: skips" one_line_with "guard: skipped, a Kamal container is running"

setup guard-kamal-lookalike-image
echo exited >"$T/c/muster-cloud-web-$SHA"
kamal_ps 'basecamp/kamal-proxy:v0.10.3||' 'ghcr.io/basecamp/kamal-other:1||'
run_it --guard
check "--guard with only kamal-proxy and lookalike images running: still guards" logged docker.log "docker start muster-cloud-web-$SHA"

setup guard-kamal-running-no-checkout
rm -rf "$T/source/bin"
echo exited >"$T/c/muster-cloud-web-$SHA"
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0|1|manual'
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
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0|1|manual'
run_it
check "full run with Kamal running: exit 0" [ "$rc" -eq 0 ]
check "full run with Kamal running: skips entirely, naming the owner" one_line_with "skip: a Kamal container is running (owner: manual); not deploying, not touching anything"
check "full run with Kamal running: starts nothing" not_logged docker.log "docker start"
check "full run with Kamal running: never asks GitHub" test ! -e "$T/github.log"

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
check "fresh host: Kamal runs proxy boot, accessory boot db, then the app" [ "$(kamal_order)" = "proxy boot,accessory boot,app boot,prune all," ]
check "fresh host: deployed and recorded" recorded_is "$REL"

setup_deploy bootstrap-existing-host
run_it
check "existing proxy and database: exit 0" [ "$rc" -eq 0 ]
check "existing proxy and database: no proxy boot" not_logged kamal.log "kamal proxy boot"
check "existing proxy and database: no accessory boot" not_logged kamal.log "kamal accessory boot"
check "existing proxy and database: no bootstrap decision" lacks "booting it with Kamal"
check "existing proxy and database: only the app is booted" [ "$(kamal_order)" = "app boot,prune all," ]

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

# ---- helpers for the cases below ---------------------------------------------------------
iso_ago() { date -u -d "$1 ago" +%Y-%m-%dT%H:%M:%SZ; }
plant_lock() { # age: a Kamal deploy lock left behind on the host (dir mtime = when it was taken)
  mkdir -p "$T/kamal-lock"
  printf 'Locked by: test at %s\nVersion: x\nMessage: Automatic deploy lock\n' "$(iso_ago "$1")" | base64 >"$T/kamal-lock/details"
  touch -d "$1 ago" "$T/kamal-lock"
}
no_routed_stop() { ! grep -q 'routed=1' "$T/stop.log"; }
# REL is running and routed and recorded: HEAD of main is what runs.
setup_head_running() { # name
  setup_deploy "$1"
  rm -f "$T/c/muster-cloud-web-$SHA"
  printf 'running\n' >"$T/c/muster-cloud-web-$REL"
  printf '%s\n' "$REL" >"$T/state/deployed"
  printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
  printf '%s\n' "$REL" >"$T/latest"
  rm -f "$T/no-local-image" # the running version's image is on the host
}

# ---- F2: missing infrastructure is created by the guard ----------------------------------------
setup_head_running guard-boots-missing-db
checkout_source
rm -f "$T/c/muster-cloud-db"
run_it --guard
check "--guard with the database container missing: exit 0" [ "$rc" -eq 0 ]
check "--guard with the database container missing: accessory boot db" logged kamal.log "kamal accessory boot db"
check "--guard with the database container missing: waits for pg_isready" logged docker.log "docker exec muster-cloud-db pg_isready"
check "--guard with the database container missing: no proxy boot" not_logged kamal.log "kamal proxy boot"
check "--guard with the database container missing: Kamal ran as the autodeploy owner" grep -qx 'owner=autodeploy' "$T/kamal-owner.log"
check "--guard with the database container missing: one decision line says /readyz answers" has "decision: guard: booted muster-cloud-db (it was missing); /readyz answers again"
check "--guard with the database container missing: one alert" [ "$(count_of alert.log 'notify muster-cloud: muster-cloud-db was missing; booted it with Kamal')" -eq 1 ]
check "--guard with the database container missing: the Kamal run order" [ "$(kamal_order)" = "accessory boot," ]
check "--guard never calls GitHub while booting infrastructure" test ! -e "$T/github.log"

setup_head_running guard-boots-missing-proxy
checkout_source
rm -f "$T/c/kamal-proxy"
run_it --guard
check "--guard with kamal-proxy missing: proxy boot" logged kamal.log "kamal proxy boot"
check "--guard with kamal-proxy missing: no accessory boot" not_logged kamal.log "kamal accessory boot"
check "--guard with kamal-proxy missing: running again" grep -qx running "$T/c/kamal-proxy"

# F2: DB container missing, web routed, no source checkout: the guard creates the
# checkout at the routed sha (the deploy path's update_source) and boots the DB.
setup_head_running guard-infra-no-checkout
rm -f "$T/c/muster-cloud-db"
run_it --guard
check "--guard with the database missing and no checkout: creates the checkout at the routed sha" eval 'has "no source checkout; creating it at ${REL:0:12}" && [ "$(git -C "$T/source" rev-parse HEAD)" = "$REL" ]'
check "--guard with no checkout: then boots the database with Kamal" eval 'logged kamal.log "kamal accessory boot db" && grep -qx running "$T/c/muster-cloud-db"'
check "--guard with no checkout: never calls GitHub" test ! -e "$T/github.log"
run_it
check "full run after that: no deferral, exit 0" eval '[ "$rc" -eq 0 ] && lacks "leaving it to the deploy"'

setup_head_running guard-infra-no-checkout-recorded
rm -f "$T/c/muster-cloud-db"
: >"$T/route" # nothing routed: the recorded version is used
run_it --guard
check "--guard with no checkout and nothing routed: uses the recorded version" eval 'has "creating it at ${REL:0:12}" && logged kamal.log "kamal accessory boot db"'

setup_head_running guard-infra-no-checkout-no-version
rm -f "$T/c/muster-cloud-db" "$T/state/deployed"
: >"$T/route"
run_it --guard
check "--guard with no checkout and no routed or recorded version: leaves it to the deploy" eval 'has "leaving it to the deploy" && [ ! -s "$T/kamal.log" ]'

setup_head_running guard-infra-checkout-fails
rm -f "$T/c/muster-cloud-db"
rm -rf "$T/origin.git"
run_it --guard
check "--guard whose checkout cannot be made: exit 0, Kamal not run, deferred" eval '[ "$rc" -eq 0 ] && [ ! -s "$T/kamal.log" ] && has "leaving it to the deploy"'

setup_head_running guard-infra-boot-fails
checkout_source
rm -f "$T/c/kamal-proxy"
touch "$T/fail-proxy-boot"
run_it --guard
check "--guard with a proxy boot that fails: exit 0, alerted" logged alert.log "fire autodeploy-guard-infra"
run_it --guard
check "--guard retries a failed infrastructure boot only after the gap" [ "$(count_of kamal.log 'kamal proxy boot')" -eq 1 ]
TEST_INFRA_GAP=0 run_it --guard
check "--guard retries a failed infrastructure boot once the gap has passed" [ "$(count_of kamal.log 'kamal proxy boot')" -eq 2 ]

setup_head_running full-run-boots-missing-db-no-silent-noop
checkout_source
rm -f "$T/c/muster-cloud-db"
run_it
check "full run, HEAD routed but the database container missing: exit 0" [ "$rc" -eq 0 ]
check "full run, HEAD routed but the database container missing: the guard step boots it" logged kamal.log "kamal accessory boot db"
check "full run, HEAD routed but the database container missing: no deploy" not_logged kamal.log "kamal app boot"
check "full run, HEAD routed but the database container missing: reports the no-op only after /readyz answered" has "decision: no-op: main HEAD ${REL:0:12} is already running"

setup_head_running full-run-noop-not-ready
rm -f "$T/ready"
run_it
check "full run, HEAD routed but /readyz down: exit 75 (alerted), not a silent success" [ "$rc" -eq 75 ]
check "full run, HEAD routed but /readyz down: alert fired" logged alert.log "fire autodeploy-notready"
check "full run, HEAD routed but /readyz down: never says it is already running" lacks "already running"
touch "$T/ready"
run_it
check "full run, /readyz back: the no-op is reported" has "no-op: main HEAD ${REL:0:12} is already running"
check "full run, /readyz back: the alert is resolved" logged alert.log "resolve autodeploy-notready"

# ---- F3: Kamal's stale_containers is never used --------------------------------------------------
# OLD serves and is routed; a previous deploy of REL was cut short and left its
# container running, and the `latest` tag points at REL: stale_containers --stop
# would stop OLD, the routed one.
setup_deploy recovery-never-stops-routed
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route"
printf '%s\n' "$REL" >"$T/latest"
printf '%s\n' "$REL" >"$T/state/deploying"
run_it
check "recovery with latest pointing at the unrouted container: deployed" has "decision: deployed ${REL:0:12}"
check "recovery: Kamal's stale_containers is never run" not_logged kamal.log "stale_containers"
check "recovery: the routed container is never stopped while it is routed" no_routed_stop
check "recovery: the old container is stopped by name, after the new one is routed" logged stop.log "stop muster-cloud-web-$OLD routed=0 by=docker"
check "recovery: ends routed to the target" routed_is "$REL"

setup_deploy normal-path-never-stops-routed
printf 'running\n' >"$T/c/muster-cloud-web-$OLD" # a leftover, and the latest tag points at it
printf '%s\n' "$OLD" >"$T/latest"
run_it
check "normal deploy with a misleading latest tag: deployed" has "decision: deployed ${REL:0:12}"
check "normal deploy: Kamal's stale_containers is never run" not_logged kamal.log "stale_containers"
check "normal deploy: the routed container is never stopped while it is routed" no_routed_stop
check "normal deploy: every other web container ends up stopped by name" grep -qx exited "$T/c/muster-cloud-web-$SHA"
check "normal deploy: the leftover is stopped too" grep -qx exited "$T/c/muster-cloud-web-$OLD"
check "normal deploy: the target runs and is routed" grep -qx running "$T/c/muster-cloud-web-$REL"

# ---- F4: a stale Kamal deploy lock ---------------------------------------------------------------
# The lock is a directory on the host; the script reads its mtime and removes it
# itself (Kamal's `lock release` cannot remove a lock that has no `details`).
setup_deploy lock-stale-old
plant_lock "90 minutes"
run_it
check "an old lock with no Kamal running: released and deployed" has "decision: deployed ${REL:0:12}"
check "an old lock: a decision line says why" has "decision: releasing the Kamal deploy lock: no Kamal container is running and it is 90 minutes old (more than 60)"
check "an old lock: Kamal is never asked about the lock, only the deploy runs" [ "$(kamal_order)" = "app boot,prune all," ]
check "an old lock: exactly one alert about it" [ "$(count_of alert.log 'notify muster-cloud: released a stale Kamal deploy lock')" -eq 1 ]
check "an old lock: the lock is gone" [ ! -d "$T/kamal-lock" ]

setup_deploy lock-without-details
mkdir "$T/kamal-lock" # a crash between mkdir and writing `details`
touch -d "2 hours ago" "$T/kamal-lock"
check "the fake Kamal, like 2.12, cannot release a lock without details" bash -c 'T=$1; cd "$T/work" && HOME=$T bash bin/kamal lock release | grep -q "no deploy lock" && [ -d "$T/kamal-lock" ]' _ "$T"
run_it
check "a lock with no details, 2 hours old: removed by the script and deployed" eval 'has "decision: deployed ${REL:0:12}" && [ ! -d "$T/kamal-lock" ]'

setup_deploy lock-without-details-young
mkdir "$T/kamal-lock"
run_it
check "a lock with no details, just made: waited for" eval 'has "a Kamal deploy lock is held" && [ -d "$T/kamal-lock" ] && [ ! -s "$T/kamal.log" ]'

setup_deploy lock-recent-waits
plant_lock "1 minute"
run_it
check "a recent lock: the run is skipped, exit 0" [ "$rc" -eq 0 ]
check "a recent lock: one decision line says it waits" eval 'has "decision: skip: a Kamal deploy lock is held (" && has "s old) and may be in use; waiting"'
check "a recent lock: not released" [ -d "$T/kamal-lock" ]
check "a recent lock: Kamal was not run at all" [ ! -s "$T/kamal.log" ]
check "a recent lock: no attempt was counted" [ -z "$(ls "$T/state/attempts")" ]
check "a recent lock: no deploying marker, nothing recorded" eval '[ ! -e "$T/state/deploying" ] && recorded_is "$SHA"'
check "a recent lock: no alert" [ ! -s "$T/alert.log" ]

setup_deploy lock-20-minutes-still-waits
plant_lock "20 minutes"
run_it
check "a 20-minute lock (a Kamal client elsewhere may hold it): still waits" eval 'has "a Kamal deploy lock is held" && [ -d "$T/kamal-lock" ]'

setup_deploy lock-predates-boot
plant_lock "1 minute"
TEST_BOOT_TIME=$(($(date +%s) - 10)) run_it
check "a recent lock taken before the last boot: released" has "decision: releasing the Kamal deploy lock: no Kamal container is running and it was taken before the last boot of this host"
check "a recent lock taken before the last boot: deployed" has "decision: deployed ${REL:0:12}"

setup_deploy lock-newer-than-boot
plant_lock "1 minute"
TEST_BOOT_TIME=$(($(date +%s) - 3600)) run_it
check "a recent lock taken after the last boot: waits" has "a Kamal deploy lock is held"

setup_deploy lock-old-but-kamal-running
plant_lock "90 minutes"
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0|1|manual'
run_it
check "an old lock while a Kamal container runs: waits, Kamal not even asked" eval '[ ! -s "$T/kamal.log" ] && has "a Kamal container is running"'
check "an old lock while a Kamal container runs: the lock stays" [ -d "$T/kamal-lock" ]

setup_deploy lock-old-but-docker-ps-fails
plant_lock "90 minutes"
touch "$T/docker-ps-fails"
run_it
check "a failed docker ps counts as busy, never idle: the full run skips" eval '[ "$rc" -eq 0 ] && has "skip: Docker did not answer" && [ -d "$T/kamal-lock" ] && [ ! -s "$T/kamal.log" ]'
run_it --guard
check "a failed docker ps: the guard skips too" eval 'has "guard: skipped, Docker did not answer" && not_logged docker.log "docker start"'

setup_deploy lock-release-fails
plant_lock "90 minutes"
touch "$T/kamal-lock/unexpected-file" # rmdir cannot remove it
touch -d "90 minutes ago" "$T/kamal-lock"
run_it
check "a stale lock that cannot be released: exit 75, alerted" eval '[ "$rc" -eq 75 ] && logged alert.log "fire autodeploy-lock"'
check "a stale lock that cannot be released: nothing is deployed" not_logged kamal.log "kamal app boot"

setup_head_running lock-guard-run
checkout_source
rm -f "$T/c/muster-cloud-db"
plant_lock "90 minutes"
run_it --guard
check "--guard with a stale lock and a missing database: releases the lock, then boots it" eval '[ "$(kamal_order)" = "accessory boot," ] && [ ! -d "$T/kamal-lock" ]'

# ---- F5: Kamal ownership and hold ---------------------------------------------------------------
setup kamal-wrapper-owner
PATH="$T/bin:$PATH" MUSTER_CLOUD_DEPLOY_ENV="$T/none.env" MUSTER_CLOUD_SECRETS_FILE="$T/none.secrets" T="$T" \
  "$REPO/bin/kamal" config >/dev/null 2>&1 || true
check "bin/kamal labels its container with the owner, manual by default" logged docker.log "--label muster.kamal.owner=manual"
check "bin/kamal keeps the muster.kamal=1 label too" logged docker.log "--label muster.kamal=1"
: >"$T/docker.log"
PATH="$T/bin:$PATH" MUSTER_KAMAL_OWNER=autodeploy MUSTER_CLOUD_DEPLOY_ENV="$T/none.env" MUSTER_CLOUD_SECRETS_FILE="$T/none.secrets" T="$T" \
  "$REPO/bin/kamal" config >/dev/null 2>&1 || true
check "bin/kamal takes the owner from MUSTER_KAMAL_OWNER" logged docker.log "--label muster.kamal.owner=autodeploy"
: >"$T/docker.log"
PATH="$T/bin:$PATH" MUSTER_KAMAL_OWNER='a b;c' MUSTER_CLOUD_DEPLOY_ENV="$T/none.env" MUSTER_CLOUD_SECRETS_FILE="$T/none.secrets" T="$T" \
  "$REPO/bin/kamal" config >/dev/null 2>&1
krc=$?
check "bin/kamal refuses an owner with odd characters and runs nothing" eval '[ "$krc" -ne 0 ] && [ ! -s "$T/docker.log" ]'

setup_deploy owner-autodeploy-calls
run_it
check "every Kamal call of the deploy carries MUSTER_KAMAL_OWNER=autodeploy" eval '[ -s "$T/kamal-owner.log" ] && ! grep -qv '\''^owner=autodeploy$'\'' "$T/kamal-owner.log"'

setup_deploy manual-kamal-skips-full-run
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0|1|manual'
run_it
check "a manual Kamal run: the full run skips, exit 0" eval '[ "$rc" -eq 0 ] && one_line_with "skip: a Kamal container is running (owner: manual)"'
check "a manual Kamal run: no image pulled, no Kamal, no GitHub" eval 'not_logged docker.log "docker pull" && [ ! -s "$T/kamal.log" ] && [ ! -e "$T/github.log" ]'
check "a manual Kamal run: the recorded version is untouched" recorded_is "$SHA"

setup_deploy kamal-owner-unknown
kamal_ps 'ghcr.io/basecamp/kamal:v2.12.0|1|'
run_it
check "a labelled Kamal container with no owner is somebody else's" has "(owner: unknown)"

setup_deploy kamal-appears-between-checks
touch "$T/kamal-appears-on-pull"
run_it
check "a Kamal run that starts after the first check: stops before any Kamal step, exit 0" eval '[ "$rc" -eq 0 ] && has "skip: a Kamal container is running (owner: manual); not running Kamal"'
check "a Kamal run that starts after the first check: Kamal never ran" [ ! -s "$T/kamal.log" ]
check "a Kamal run that starts after the first check: no attempt counted, no marker" eval '[ -z "$(ls "$T/state/attempts")" ] && [ ! -e "$T/state/deploying" ]'

setup guard-only-marker
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/state/guard-only"
run_it
check "guard-only: the guard still starts the stopped container" logged docker.log "docker start muster-cloud-web-$SHA"
check "guard-only: no deploy, GitHub is not asked" eval 'has "guard-only:" && [ ! -e "$T/github.log" ]'

setup hold-guard
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/state/hold"
run_it --guard
check "hold: --guard exits 0" [ "$rc" -eq 0 ]
check "hold: --guard logs one hold line" one_line_with "decision: hold:"
check "hold: --guard does not even ask Docker" [ ! -s "$T/docker.log" ]
check "hold: --guard starts nothing" not_logged docker.log "docker start"
run_it --guard
check "hold: the second run within 10 minutes logs nothing" [ -z "$out" ]
printf '%s\t%s\n' "$(($(date +%s) - 700))" "$(stat -c %y "$T/state/hold")" >"$T/state/hold.seen"
run_it --guard
check "hold: after 10 minutes the line comes again" has "decision: hold:"
touch "$T/state/hold"
run_it --guard
check "hold: a marker touched again counts as new and is logged" has "decision: hold:"
check "hold: a young hold sends no alert" [ ! -s "$T/alert.log" ]
rm -f "$T/state/hold"
run_it --guard
check "hold removed: the guard works again" logged docker.log "docker start muster-cloud-web-$SHA"
check "hold removed: its bookkeeping is gone" [ ! -e "$T/state/hold.seen" ]

setup_deploy hold-full-run
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'exited\n' >"$T/c/muster-cloud-web-$OLD"
printf '%s\n' "$OLD" >"$T/state/deployed"
: >"$T/route"
touch "$T/state/hold" "$T/state/redeploy"
run_it
check "hold: the full run exits 0 with one hold line" eval '[ "$rc" -eq 0 ] && one_line_with "decision: hold:"'
check "hold: the full run starts, stops and deploys nothing" eval '[ ! -s "$T/docker.log" ] && [ ! -s "$T/kamal.log" ] && [ ! -e "$T/github.log" ]'
check "hold: the record does not change" recorded_is "$OLD"
check "hold: a redeploy request is kept" [ -e "$T/state/redeploy" ]
rm -f "$T/state/hold"

setup hold-alert
touch -d '25 hours ago' "$T/state/hold"
run_it --guard
check "hold older than 24 h: one alert" [ "$(count_of alert.log 'notify muster-cloud: autodeploy has been on hold for 25 h')" -eq 1 ]
run_it --guard
check "hold older than 24 h: not again at the next run" [ "$(count_of alert.log 'on hold for')" -eq 1 ]
printf '%s\n' "$(($(date +%s) - 90000))" >"$T/state/hold.alerted"
run_it
check "hold older than 24 h: again after 24 h, also from a full run" [ "$(count_of alert.log 'on hold for')" -eq 2 ]

# ---- F7: the DR cutover: redeploy with the same HEAD, hold in between -----------------------------
setup_head_running dr-cutover
touch "$T/state/hold" "$T/state/redeploy"
echo exited >"$T/c/muster-cloud-web-$REL" # the operator stopped the old container for the cutover
run_it
run_it --guard
check "cutover: with hold nothing restarts the old container" eval 'grep -qx exited "$T/c/muster-cloud-web-$REL" && not_logged docker.log "docker start"'
rm -f "$T/state/hold" "$T/state/guard-only"
run_it
check "cutover: without hold, redeploy boots the same HEAD afresh" logged kamal.log "kamal app boot --version $REL"
check "cutover: the new container serves" eval 'routed_is "$REL" && grep -qx running "$T/c/muster-cloud-web-$REL"'
check "cutover: the renamed old container is not running" grep -qx exited "$T/c/muster-cloud-web-${REL}_replaced_1"
check "cutover: the request is consumed after the deploy succeeded" [ ! -e "$T/state/redeploy" ]
check "cutover: said so" eval 'has "decision: redeploy requested: booting the running version ${REL:0:12} again" && has "decision: deployed ${REL:0:12}"'

# ---- F9: the proxy routes cannot be read ---------------------------------------------------------
setup_head_running routes-unreadable-noop
touch "$T/routes-unreadable"
run_it
check "routes unreadable, /readyz answers, HEAD recorded: alerts" eval '[ "$rc" -eq 75 ] && logged alert.log "fire autodeploy-routes muster-cloud: cannot read proxy routes"'
check "routes unreadable, /readyz answers, HEAD recorded: changes nothing" eval '[ ! -s "$T/kamal.log" ] && not_logged docker.log "docker pull" && recorded_is "$REL"'
check "routes unreadable: never boots and rolls back to the same commit" lacks "rolling back"

setup_deploy routes-unreadable-new-head
touch "$T/routes-unreadable"
run_it
check "routes unreadable, a new HEAD: deployed, accepted on the container's own health and /readyz" has "decision: deployed ${REL:0:12}"
check "routes unreadable, a new HEAD: a warning alert says so" logged alert.log "WARNING: muster-cloud: cannot read proxy routes; accepted ${REL:0:12}"
check "routes unreadable, a new HEAD: no rollback" not_logged kamal.log "kamal rollback"
check "routes unreadable, a new HEAD: the superseded container was stopped by Kamal, nothing by name" not_logged stop.log "by=docker"

setup_deploy routes-unreadable-unhealthy-target
touch "$T/routes-unreadable"
printf 'unhealthy\n' >"$T/health-muster-cloud-web-$REL"
run_it
check "routes unreadable and the target unhealthy: the failure path" has "FAILED while switching with Kamal" || has "deploy of ${REL:0:12} FAILED"
check "routes unreadable and the target unhealthy: rolls back to the previous commit, never to the new one" eval 'logged kamal.log "kamal rollback $SHA" && not_logged kamal.log "kamal rollback $REL"'

setup_head_running routes-unreadable-redeploy-boot-fails
touch "$T/routes-unreadable" "$T/state/redeploy" "$T/fail-app-boot"
run_it
check "a boot that fails with unreadable routes and the same commit: no rollback to it" not_logged kamal.log "kamal rollback"
check "a boot that fails with unreadable routes and the same commit: alerted" eval '[ "$rc" -eq 75 ] && has "there is no other version to roll back to"'
check "F11: the failed attempt keeps the redeploy request" [ -e "$T/state/redeploy" ]
check "F11: the failed attempt is counted" [ "$(cut -d' ' -f1 "$T/state/attempts/$REL")" = 1 ]

# ---- F10: the retry limit ----------------------------------------------------------------------
setup_deploy retry-limit-unclean
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route"
printf '%s\n' "$REL" >"$T/state/deploying"
mkdir -p "$T/state/attempts"
printf '3 %s\n' "$(date +%s)" >"$T/state/attempts/$REL"
run_it
check "retry limit reached with an unclean state: alerts, exit 75" eval '[ "$rc" -eq 75 ] && logged alert.log "fire autodeploy-stuck"'
check "retry limit reached with an unclean state: the marker stays" [ -e "$T/state/deploying" ]
check "retry limit reached with an unclean state: nothing deployed" [ ! -s "$T/kamal.log" ]
run_it
check "retry limit: alerts again at the next run (the alert command dedups)" [ "$(count_of alert.log 'fire autodeploy-stuck')" -eq 2 ]

setup_deploy retry-limit-clean
rm -f "$T/c/muster-cloud-web-$SHA"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
printf '%s\n' "$REL" >"$T/state/deploying"
mkdir -p "$T/state/attempts"
printf '3 %s\n' "$(date +%s)" >"$T/state/attempts/$REL"
run_it
check "retry limit with one routed, ready web container: exit 0, marker cleared" eval '[ "$rc" -eq 0 ] && [ ! -e "$T/state/deploying" ]'
check "retry limit with one routed, ready web container: the routed version is recorded" recorded_is "$REL"
check "retry limit with one routed, ready web container: the alert is resolved" logged alert.log "resolve autodeploy-stuck"
check "retry limit with one routed, ready web container: Kamal untouched" [ ! -s "$T/kamal.log" ]
run_it
check "after the marker is cleared the next run is a no-op" has "no-op: main HEAD ${REL:0:12} is already running"

setup_deploy retry-limit-not-ready
rm -f "$T/c/muster-cloud-web-$SHA" "$T/ready"
printf 'running\n' >"$T/c/muster-cloud-web-$REL"
printf '%s\n' "$OLD" >"$T/state/deployed"
printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
printf '%s\n' "$REL" >"$T/state/deploying"
mkdir -p "$T/state/attempts"
printf '3 %s\n' "$(date +%s)" >"$T/state/attempts/$REL"
run_it
check "retry limit with a web container that is not ready: alert, marker kept" eval '[ "$rc" -eq 75 ] && [ -e "$T/state/deploying" ] && logged alert.log "fire autodeploy-stuck"'

# ---- F11: the redeploy request survives a failure ------------------------------------------------
setup_head_running redeploy-failed-pull
touch "$T/state/redeploy" "$T/fail-pull" "$T/no-local-image" # the image is gone: that exact sha is pulled
run_it
check "redeploy with a pull that fails: alerted" eval '[ "$rc" -eq 75 ] && has "FAILED while pulling the image"'
check "redeploy with a pull that fails: the request is kept" [ -e "$T/state/redeploy" ]
rm -f "$T/fail-pull"
run_it
check "redeploy retried after the pull works: deployed" has "decision: deployed ${REL:0:12}"
check "redeploy retried: consumed only now" [ ! -e "$T/state/redeploy" ]

setup_head_running redeploy-failed-boot
touch "$T/state/redeploy" "$T/fail-app-boot"
run_it
check "redeploy with a boot that fails: the request is kept" [ -e "$T/state/redeploy" ]
check "redeploy with a boot that fails: counted as an attempt" [ "$(cut -d' ' -f1 "$T/state/attempts/$REL")" = 1 ]
mkdir -p "$T/state/attempts"
printf '%s %s\n' 3 "$(date +%s)" >"$T/state/attempts/$REL"
rm -f "$T/fail-app-boot"
run_it
check "redeploy past the retry limit: not retried, request kept" eval '[ -e "$T/state/redeploy" ] && [ "$(count_of kamal.log '\''kamal app boot'\'')" -eq 1 ]'

setup_head_running redeploy-touched-during-deploy
touch "$T/state/redeploy" "$T/retouch-redeploy"
run_it
check "a redeploy requested again during the deploy is not lost" eval 'has "decision: deployed ${REL:0:12}" && [ -e "$T/state/redeploy" ]'

# ---- B2: hold stops a run already in flight; --hold and --release -------------------------------
bg_run() { # args: a run in the background; its output and exit code land in $T/bg.out and $T/bg.rc
  (
    run_it "$@"
    printf '%s' "$out" >"$T/bg.out"
    printf '%s' "$rc" >"$T/bg.rc"
  ) &
  bg_pid=$!
  bg_pids+=($!)
}
wait_for() { for _ in $(seq 1 100); do [ -e "$1" ] && return 0; sleep 0.1; done; return 1; } # path

setup_deploy hold-in-flight
touch "$T/github-wait"
bg_run
wait_for "$T/github-asked"
touch "$T/state/hold" # set while the run waits on GitHub
wait "$bg_pid"
out=$(cat "$T/bg.out")
check "hold set while a run waits on GitHub: the run stops cleanly (exit 0)" eval '[ "$(cat "$T/bg.rc")" -eq 0 ] && has "decision: hold: $T/state/hold is set; stopping before the next change"'
check "hold set mid-run: no Kamal call, no pull, no start or stop" eval '[ ! -s "$T/kamal.log" ] && not_logged docker.log "docker pull" && not_logged docker.log "docker start" && not_logged docker.log "docker stop"'
check "hold set mid-run: nothing recorded, no marker, no attempt" eval 'recorded_is "$SHA" && [ ! -e "$T/state/deploying" ] && [ -z "$(ls "$T/state/attempts")" ]'
rm -f "$T/state/hold" "$T/github-wait"

setup_deploy hold-on-pull
touch "$T/hold-on-pull"
run_it
check "hold set while the image is pulled: stops before any Kamal call" eval '[ "$rc" -eq 0 ] && has "decision: hold:" && [ ! -s "$T/kamal.log" ] && [ ! -e "$T/state/deploying" ] && recorded_is "$SHA"'
rm -f "$T/state/hold" "$T/hold-on-pull"

setup_deploy hold-during-kamal-boot
touch "$T/hold-after-boot"
run_it
check "hold set while Kamal boots: stops after that call, exit 0" eval '[ "$rc" -eq 0 ] && has "decision: hold:" && logged kamal.log "kamal app boot --version $REL"'
check "hold set while Kamal boots: no muster-cloud-ops install, no record" eval 'not_logged alert.log "install-ops" && recorded_is "$SHA"'
check "hold set while Kamal boots: no stop by name, no prune" eval 'not_logged stop.log "by=docker" && not_logged kamal.log "prune"'
check "hold set while Kamal boots: the cut-short marker stays" [ -e "$T/state/deploying" ]
rm -f "$T/state/hold" "$T/hold-after-boot"
run_it
check "hold set while Kamal boots: the stopped deploy is not counted as a failed attempt" eval '[ -z "$(ls "$T/state/attempts")" ]'
check "after the release the next run finishes the deploy" eval 'has "decision: incomplete deploy: a previous deploy was cut short" && has "decision: deployed ${REL:0:12}" && recorded_is "$REL" && [ ! -e "$T/state/deploying" ]'

setup_deploy hold-guard-stops-before-start
echo exited >"$T/c/muster-cloud-web-$SHA"
touch "$T/state/hold"
run_it --guard
check "hold: a guard run stops before docker start" eval '[ "$rc" -eq 0 ] && not_logged docker.log "docker start"'
rm -f "$T/state/hold"

setup_deploy hold-command
touch "$T/github-wait"
bg_run
wait_for "$T/github-asked"
run_it --hold # blocks on the flock until the run in flight has finished
check "--hold: exit 0 and prints held" eval '[ "$rc" -eq 0 ] && [ "$out" = held ]'
check "--hold: returns only after the run in flight has finished" eval '[ -e "$T/bg.rc" ] && ! kill -0 "$bg_pid" 2>/dev/null'
out=$(cat "$T/bg.out")
check "--hold: the run in flight stopped at its next gate, changing nothing" eval '[ "$(cat "$T/bg.rc")" -eq 0 ] && has "decision: hold:" && [ ! -s "$T/kamal.log" ] && recorded_is "$SHA"'
check "--hold: the marker exists" [ -e "$T/state/hold" ]
touch -d '3 hours ago' "$T/state/hold"
before=$(stat -c %Y "$T/state/hold")
run_it --hold
check "--hold again: idempotent (exit 0, held, marker not touched)" eval '[ "$rc" -eq 0 ] && [ "$out" = held ] && [ "$(stat -c %Y "$T/state/hold")" = "$before" ]'
rm -f "$T/github-wait"
echo exited >"$T/c/muster-cloud-web-$SHA"
run_it
check "--hold: while held a full run does nothing" eval 'has "decision: hold:" && not_logged docker.log "docker start"'
printf '%s\t%s\n' "$(date +%s)" "x" >"$T/state/hold.seen"
run_it --release
check "--release: exit 0, prints released, removes the marker and its bookkeeping" eval '[ "$rc" -eq 0 ] && [ "$out" = released ] && [ ! -e "$T/state/hold" ] && [ ! -e "$T/state/hold.seen" ]'
run_it --release
check "--release again: idempotent" eval '[ "$rc" -eq 0 ] && [ "$out" = released ]'
run_it --guard
check "--release: the guard works again" logged docker.log "docker start muster-cloud-web-$SHA"
setup hold-command-idle
run_it --hold
check "--hold with nothing in flight: returns at once" eval '[ "$rc" -eq 0 ] && [ "$out" = held ] && [ -e "$T/state/hold" ]'
run_it --hold extra
check "--hold with an extra argument: exit 64" [ "$rc" -eq 64 ]
run_it --release extra
check "--release with an extra argument: exit 64" [ "$rc" -eq 64 ]

# ---- B3: redeploy boots what runs now, never a newer commit ---------------------------------------
NEWER=cccccccccccccccccccccccccccccccccccccccc
setup_head_running redeploy-ignores-newer-failing-head
printf '%s\n' "$NEWER" >"$T/github-head"
touch "$T/github-ci-failed" "$T/state/redeploy"
run_it
check "redeploy with a newer failing commit on main: the routed sha is rebooted" eval 'has "decision: deployed ${REL:0:12}" && logged kamal.log "kamal app boot --version $REL" && routed_is "$REL" && recorded_is "$REL"'
check "redeploy: GitHub is not consulted at all (no HEAD, no CI)" [ ! -e "$T/github.log" ]
check "redeploy: nothing of the newer commit is pulled or booted" eval 'not_logged docker.log "$NEWER" && not_logged kamal.log "$NEWER"'
check "redeploy: the image is local, so nothing is pulled" not_logged docker.log "docker pull"
check "redeploy: the request is consumed" eval '[ ! -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'
run_it
check "after the redeploy the newer failing commit is still not deployed (CI fails)" eval 'has "did not succeed" && lacks "decision: deploying"'

setup_head_running redeploy-newer-green-head
printf '%s\n' "$NEWER" >"$T/github-head"
touch "$T/state/redeploy"
run_it
check "redeploy with a newer GREEN commit on main: still only the routed sha is booted" eval 'routed_is "$REL" && not_logged kamal.log "--version $NEWER" && [ ! -e "$T/github.log" ]'

setup_head_running redeploy-routes-unreadable-uses-record
touch "$T/routes-unreadable" "$T/state/redeploy"
printf '%s\n' "$NEWER" >"$T/github-head"
run_it
check "redeploy with unreadable routes: the recorded version is booted again" eval 'logged kamal.log "kamal app boot --version $REL" && has "booting the running version ${REL:0:12} again" && [ ! -e "$T/github.log" ]'

setup_head_running redeploy-image-missing-pulls-exact-sha
touch "$T/state/redeploy" "$T/no-local-image"
printf '%s\n' "$NEWER" >"$T/github-head"
run_it
check "redeploy with the image gone: pulls that exact sha, nothing else" eval 'logged docker.log "docker pull --quiet ghcr.io/oliveralerubio/muster-cloud:$REL" && not_logged docker.log "$NEWER" && has "decision: deployed ${REL:0:12}"'

setup_deploy redeploy-nothing-running
touch "$T/state/redeploy"
reset_containers
run_it
check "redeploy with no running or recorded version: a normal deploy of HEAD, the request is dropped" eval 'has "no version runs or is recorded" && has "decision: deployed ${REL:0:12}" && [ ! -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'

# ---- F11: the redeploy request is consumed atomically -----------------------------------------------
setup_head_running redeploy-inflight-success
touch "$T/state/redeploy"
run_it
check "redeploy success: neither the request nor the inflight file is left" eval '[ ! -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'

setup_head_running redeploy-inflight-failure-gives-back
touch "$T/state/redeploy" "$T/fail-app-boot"
run_it
check "redeploy failure with no new request: the inflight file is given back" eval '[ "$rc" -eq 75 ] && [ -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'

setup_head_running redeploy-inflight-failure-new-request
touch "$T/state/redeploy" "$T/fail-app-boot" "$T/retouch-redeploy"
run_it
check "redeploy failure with a new request meanwhile: the inflight one is dropped, the new one stays" eval '[ "$rc" -eq 75 ] && [ -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'

setup_head_running redeploy-inflight-killed-run
touch "$T/state/redeploy.inflight" # a run killed after it consumed the request
run_it
check "a killed run's inflight request is given back and served by the next run" eval 'has "decision: redeploy requested" && has "decision: deployed ${REL:0:12}" && [ ! -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'

setup_head_running redeploy-inflight-hold
touch "$T/state/redeploy" "$T/hold-after-boot"
run_it
check "hold in the middle of a redeploy: the request is given back, not lost" eval 'has "decision: hold:" && [ -e "$T/state/redeploy" ] && [ ! -e "$T/state/redeploy.inflight" ]'
rm -f "$T/state/hold"

# ---- F3/F9: the candidate is probed itself; cleanup re-reads the route ------------------------------
setup_deploy routes-unreadable-candidate-down
touch "$T/routes-unreadable"
touch "$T/cand-down-muster-cloud-web-$REL" # the container answers 503 at its own address; the OLD route would still answer
run_it
check "routes unreadable, candidate not ready itself (the old route answers): rejected" eval '[ "$rc" -eq 75 ] && lacks "cannot be read; ${REL:0:12} runs" && has "FAILED while switching with Kamal"'
check "routes unreadable, candidate not ready itself: rolled back to the previous commit" eval 'logged kamal.log "kamal rollback $SHA" && not_logged kamal.log "kamal rollback $REL"'
check "routes unreadable: the candidate was probed at its address on the kamal network, not through the proxy" eval 'logged docker.log '\''NetworkSettings.Networks "kamal"'\'' && logged curl.log "http://10.77."'

setup_deploy routes-unreadable-candidate-no-address
touch "$T/routes-unreadable" "$T/no-address"
run_it
check "routes unreadable and the candidate has no address on the kamal network: rejected" eval '[ "$rc" -eq 75 ] && lacks "cannot be read; ${REL:0:12} runs"'

setup_deploy cleanup-route-moves-to-leftover
printf 'running\n' >"$T/c/muster-cloud-web-$OLD" # SHA (the old route) and OLD both run on after the boot:
printf '%s\n' "$REL" >"$T/latest"                # Kamal's `latest` tag is the new one, it stops neither
printf 'muster-cloud-web-%s\n' "$OLD" >"$T/route-after-stop" # after the first stop the proxy routes to the other leftover
run_it
check "cleanup: the routed container is read again before EACH stop, a leftover that became routed is skipped" eval 'has "decision: deployed ${REL:0:12}" && no_routed_stop && grep -qx running "$T/c/muster-cloud-web-$OLD"'

setup_deploy cleanup-route-unreadable-after-first-stop
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'running\n' >"$T/c/muster-cloud-web-cccccccccccccccccccccccccccccccccccccccc"
printf '%s\n' "$SHA" >"$T/latest"
touch "$T/unreadable-after-stop"
run_it
check "cleanup: when the route becomes unreadable between stops, nothing more is stopped" eval '[ "$(count_of stop.log '\''by=docker'\'')" -eq 1 ] && has "cannot be read; leaving the other web containers running"'

setup_deploy cleanup-empty-route-stops-nothing
printf 'running\n' >"$T/c/muster-cloud-web-$OLD"
printf 'muster-cloud-web-ghost\n' >"$T/route-after-ops" # after the checks the route's target cannot be inspected
run_it
check "cleanup: a route to a target that cannot be inspected (empty name): nothing is stopped by name" eval 'has "decision: deployed ${REL:0:12}" && not_logged stop.log "by=docker"'
check "cleanup with an unusable route: the leftover keeps running, said so" eval 'grep -qx running "$T/c/muster-cloud-web-$OLD" && has "leaving the other web containers running"'

# ---- F10: the retry-limit recovery runs the same steps as a deploy -----------------------------------
retry_limit_clean() { # name: REL runs alone, routed and ready, but a cut-short deploy left its marker and 3 attempts
  setup_deploy "$1"
  rm -f "$T/c/muster-cloud-web-$SHA"
  printf 'running\n' >"$T/c/muster-cloud-web-$REL"
  printf '%s\n' "$OLD" >"$T/state/deployed"
  printf 'muster-cloud-web-%s\n' "$REL" >"$T/route"
  printf '%s\n' "$REL" >"$T/state/deploying"
  mkdir -p "$T/state/attempts"
  printf '3 %s\n' "$(date +%s)" >"$T/state/attempts/$REL"
}
retry_limit_clean retry-limit-clean-finishes
run_it
check "retry limit recovery: installs muster-cloud-ops for the routed sha" logged alert.log "install-ops localhost:5000/oliveralerubio/muster-cloud:$REL"
check "retry limit recovery: checks the public URL too" logged curl.log "ts.net"
check "retry limit recovery: then records, clears the marker and the exhausted attempts" eval 'recorded_is "$REL" && [ ! -e "$T/state/deploying" ] && [ ! -e "$T/state/attempts/$REL" ]'

retry_limit_clean retry-limit-install-ops-fails
touch "$T/fail-install-ops"
run_it
check "retry limit recovery with install-ops failing: not recorded, marker kept, alerted" eval '[ "$rc" -eq 75 ] && recorded_is "$OLD" && [ -e "$T/state/deploying" ] && logged alert.log "fire autodeploy-stuck" && logged alert.log "failed while installing the matching muster-cloud-ops"'

retry_limit_clean retry-limit-public-down
touch "$T/public-down"
run_it
check "retry limit recovery with the public URL down: not recorded, marker kept, alerted" eval '[ "$rc" -eq 75 ] && recorded_is "$OLD" && [ -e "$T/state/deploying" ] && logged alert.log "fire autodeploy-stuck" && logged alert.log "failed while checking /readyz"'

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
