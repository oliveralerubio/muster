#!/usr/bin/env bash
# deploy/kamal/host-setup.sh — one-time (and safely repeatable) preparation of
# the `personal` VPS for the Kamal deployment of muster Cloud. Guide:
# deploy/KAMAL.md.
#
#   sudo deploy/kamal/host-setup.sh            apply: create/fix what is missing
#   sudo deploy/kamal/host-setup.sh --check    change NOTHING; report what is
#                                              missing or different
#
# Run as root from a checkout of this repository. Idempotent: a second run
# changes nothing. It manages only what is listed here and never touches any
# other container, network, service, UFW rule, /etc/docker or Tailscale serve
# entry:
#
#   accounts      users/groups muster-cloud (10001) and muster-pg (10002)
#   directories   /srv/muster-cloud/{repos,repos/locks,control,backups,pg/data,
#                 pgsocket}, /var/lib/muster-witness/w, /etc/muster-cloud
#   witness       refuses to create anything under /var/lib/muster-witness
#                 unless that is a mounted filesystem of its own
#   network       Docker network `kamal` with the fixed subnet 172.31.250.0/24
#                 (refuses if a `kamal` network exists with another subnet)
#   host scripts  /usr/local/sbin/muster-cloud-{install-ops,alert,autodeploy,
#                 backup-check,offsite,authority-offsite} (root:root 0755), copied
#                 from deploy/kamal/. No sudoers rule exists or is created: user
#                 `gha` has no privilege here, and the old drop-in
#                 /etc/sudoers.d/muster-cloud-deploy is DELETED when found
#   state dirs    /var/lib/muster-cloud/{alerts,offsite,authority} (root 0700) and
#                 drill (root 0711: the restore drill runs the restore as the
#                 service's uid, which must reach its scratch directory; every
#                 scratch directory is 0700 or 0711 and the snapshot inside root's)
#   tmpfiles      /etc/tmpfiles.d/muster-cloud-drill.conf removes the scratch
#                 directories of a restore drill cut short by a reboot
#   fresh host    when /var/lib/muster-cloud/autodeploy/deployed does not exist
#                 yet, the marker .../autodeploy/guard-only is created BEFORE the
#                 timers are enabled, so nothing can deploy before `init` (the
#                 guide's bootstrap removes it in step 7). Not when called by
#                 autodeploy (MUSTER_HOST_SETUP_CALLER=autodeploy, below): its
#                 first deploy runs this before it writes `deployed`, and the
#                 marker would block every later run
#   drill key     /etc/muster-cloud/drill-age.key (root 0400): a SECOND age
#                 identity only the monthly restore drill uses, generated here
#                 when missing. Its public half is added to ops.env as an extra
#                 backup recipient (MUSTER_BACKUP_AGE_RECIPIENTS); the main key
#                 stays off the host
#   timers        backup (every 6 h at :41), retention (hourly), offsite copy to
#                 Google Drive (hourly), authority copy (hourly and after every
#                 backup), backup check (every 30 min), autodeploy (every 5 min),
#                 liveness guard (every 15 s: starts a container stopped by hand),
#                 restore drill (monthly); enabled with `enable --now`. Also the
#                 path unit muster-cloud-authority-offsite.path, which starts the
#                 authority copy a few seconds after every change of the control
#                 journal. Every muster-cloud oneshot alerts through
#                 muster-cloud-alert@.service when it fails
#   funnel        `tailscale funnel --bg --https=10000 http://127.0.0.1:18110`
#
# muster-cloud-autodeploy runs this script in apply mode from the checkout it is
# about to deploy, so everything listed above follows the release by itself. It
# sets the environment variable MUSTER_HOST_SETUP_CALLER=autodeploy for that call;
# the only thing it changes is that the `guard-only` marker is never created
# (autodeploy running means someone already removed it on purpose).
#
# It refuses to proceed unless /etc/muster-cloud/ops.env is a regular file
# owned by root with mode 0600. It also CHECKS (never creates) the secret
# files /etc/muster-cloud/kamal.env and deploy.env, the gha account, rclone's
# `gdrive:` remote, the loopback ports of kamal-proxy and that the GHCR package
# can be pulled anonymously.
#
# Exit codes: 0 everything in place (apply: and done); 1 something is missing,
# different or failed (see the FAIL/TODO/WOULD lines); 64 usage; 77 not root or
# ops.env refused.

# The `bash -c '...'` bodies passed to change() are single-quoted on purpose:
# their $1..$4 are positional arguments of that inner shell.
# shellcheck disable=SC2016
set -uo pipefail
umask 022
export LC_ALL=C
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

HERE=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd -P)
REPO=$(cd "$HERE/../.." && pwd -P)
readonly HERE REPO

# ---- fixed facts of this deployment ------------------------------------------
readonly SVC_UID=10001 SVC_NAME=muster-cloud
readonly PG_UID=10002 PG_NAME=muster-pg
readonly ROOT_DIR=/srv/muster-cloud
readonly WITNESS_MOUNT=/var/lib/muster-witness
readonly ETC_DIR=/etc/muster-cloud
readonly OPS_ENV=$ETC_DIR/ops.env
readonly KAMAL_ENV=$ETC_DIR/kamal.env
readonly DEPLOY_ENV=$ETC_DIR/deploy.env
readonly DRILL_KEY=$ETC_DIR/drill-age.key
readonly KAMAL_NET=kamal
readonly KAMAL_SUBNET=172.31.250.0/24
readonly DEPLOY_USER=gha
readonly SBIN=/usr/local/sbin
readonly OLD_SUDOERS_FILE=/etc/sudoers.d/muster-cloud-deploy
readonly STATE_BASE=/var/lib/muster-cloud
readonly UNIT_DIR=/etc/systemd/system
readonly GHCR_PACKAGE=oliveralerubio/muster-cloud
# The scripts of deploy/kamal/ that live in /usr/local/sbin.
readonly HOST_SCRIPTS=(muster-cloud-install-ops muster-cloud-alert muster-cloud-autodeploy
  muster-cloud-backup-check muster-cloud-offsite muster-cloud-authority-offsite)
# The units of deploy/systemd/ that this script installs itself (backup and
# retention come from install-systemd.sh).
readonly OWN_UNITS=(muster-cloud-alert@.service
  muster-cloud-autodeploy.service muster-cloud-autodeploy.timer
  muster-cloud-guard.service muster-cloud-guard.timer
  muster-cloud-backup-check.service muster-cloud-backup-check.timer
  muster-cloud-authority-offsite.service muster-cloud-authority-offsite.timer
  muster-cloud-authority-offsite.path
  muster-cloud-restore-drill.service muster-cloud-restore-drill.timer
  muster-cloud-offsite.service muster-cloud-offsite.timer)
readonly FUNNEL_PORT=10000
readonly PROXY_HTTP_PORT=18110
readonly PROXY_HTTPS_PORT=18111
readonly TS_HOST_IP=100.92.84.56

MODE=apply
case "${1:-}" in
  '') ;;
  --check) MODE=check ;;
  -h | --help)
    sed -n '2,/ops.env refused\./p' "$0" | sed 's/^# \{0,1\}//'
    exit 0
    ;;
  *)
    printf 'usage: %s [--check]\n' "$0" >&2
    exit 64
    ;;
esac
[ "$#" -le 1 ] || {
  printf 'usage: %s [--check]\n' "$0" >&2
  exit 64
}

# ---- reporting -----------------------------------------------------------------
n_change=0 # check: would change; apply: changed
n_fail=0   # blocked or failed
n_todo=0   # needs a human (secrets, SSH keys, rclone remote)

ok() { printf 'ok     %s\n' "$1"; }
note() { printf 'note   %s\n' "$1"; }
fail() {
  printf 'FAIL   %s\n' "$1"
  n_fail=$((n_fail + 1))
}
todo() {
  printf 'TODO   %s\n' "$1"
  n_todo=$((n_todo + 1))
}

# change "description" cmd args...  — run it (apply) or only report it (check).
change() {
  local desc=$1
  shift
  if [ "$MODE" = check ]; then
    printf 'WOULD  %s\n' "$desc"
    n_change=$((n_change + 1))
    return 0
  fi
  if "$@"; then
    printf 'done   %s\n' "$desc"
    n_change=$((n_change + 1))
    return 0
  fi
  fail "$desc (command failed)"
  return 1
}

WORK=""
# shellcheck disable=SC2329  # runs through the EXIT trap
cleanup() {
  [ -z "$WORK" ] || rm -rf --one-file-system -- "$WORK"
}
trap cleanup EXIT
WORK=$(mktemp -d "${TMPDIR:-/tmp}/muster-host-setup.XXXXXX") || {
  printf 'host-setup.sh: mktemp failed\n' >&2
  exit 74
}

# ---- preflight ---------------------------------------------------------------
[ "$(id -u)" -eq 0 ] || {
  printf 'host-setup.sh: must run as root (sudo)\n' >&2
  exit 77
}
for tool in docker systemctl install cmp stat getent flock mountpoint findmnt ss; do
  command -v "$tool" >/dev/null 2>&1 || {
    printf 'host-setup.sh: required tool not found: %s\n' "$tool" >&2
    exit 77
  }
done
printf 'host-setup.sh: mode=%s repo=%s\n' "$MODE" "$REPO"

# The unit files run with this file as their environment (it holds the
# database URL): refuse anything but a root-owned 0600 regular file.
ops_env_ok=1
if [ ! -f "$OPS_ENV" ] || [ -L "$OPS_ENV" ]; then
  ops_env_ok=0
  why="is not a regular file"
elif [ "$(stat -c '%u:%g %a' -- "$OPS_ENV")" != "0:0 600" ]; then
  ops_env_ok=0
  why="must be owned by root:root with mode 0600 (has $(stat -c '%U:%G %a' -- "$OPS_ENV"))"
fi
if [ "$ops_env_ok" -eq 0 ]; then
  if [ "$MODE" = apply ]; then
    printf 'host-setup.sh: refusing to proceed: %s %s\n' "$OPS_ENV" "$why" >&2
    printf 'host-setup.sh: create it as root first (see deploy/KAMAL.md, step 2)\n' >&2
    exit 77
  fi
  fail "$OPS_ENV $why (apply mode refuses to proceed)"
fi

# ---- helpers: accounts and directories --------------------------------------
ensure_account() { # name id
  local name=$1 id=$2 gid uid pgid
  if gid=$(getent group "$name" | cut -d: -f3) && [ -n "$gid" ]; then
    if [ "$gid" = "$id" ]; then
      ok "group $name ($id)"
    else
      fail "group $name exists with gid $gid, expected $id"
    fi
  else
    change "create group $name ($id)" groupadd --system --gid "$id" "$name"
  fi
  if uid=$(getent passwd "$name" | cut -d: -f3) && [ -n "$uid" ]; then
    pgid=$(getent passwd "$name" | cut -d: -f4)
    if [ "$uid" = "$id" ] && [ "$pgid" = "$id" ]; then
      ok "user $name ($id:$id)"
    else
      fail "user $name exists as $uid:$pgid, expected $id:$id"
    fi
  else
    change "create user $name ($id)" useradd --system --uid "$id" --gid "$id" \
      --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin "$name"
  fi
}

# ensure_dir path uid gid mode
ensure_dir() {
  local path=$1 uid=$2 gid=$3 mode=$4 have
  if [ -L "$path" ]; then
    fail "$path is a symbolic link; refusing"
    return
  fi
  if [ ! -d "$path" ]; then
    change "create $path ($uid:$gid $mode)" install -d -m "$mode" -o "$uid" -g "$gid" "$path"
    return
  fi
  have=$(stat -c '%u:%g %a' -- "$path")
  if [ "$have" = "$uid:$gid ${mode#0}" ] || [ "$have" = "$uid:$gid $mode" ]; then
    ok "$path ($uid:$gid $mode)"
  else
    change "fix $path: $have -> $uid:$gid $mode" bash -c 'chown "$1:$2" "$4" && chmod "$3" "$4"' _ "$uid" "$gid" "$mode" "$path"
  fi
}

# ensure_traversable path — must exist; "other" needs x so uid 10001/10002 can
# walk down to their directories.
ensure_traversable() {
  local path=$1 mode
  if [ ! -d "$path" ]; then
    change "create $path (root 0755)" install -d -m 0755 -o 0 -g 0 "$path"
    return
  fi
  mode=$(stat -c %a -- "$path")
  if [ $((0$mode & 1)) -ne 0 ]; then
    ok "$path is traversable ($mode)"
  else
    change "make $path traversable ($mode -> o+x)" chmod o+x "$path"
  fi
}

# put_file src dest mode — install root:root, atomically; skip when identical.
put_file() {
  local src=$1 dest=$2 mode=$3
  if [ -f "$dest" ] && [ ! -L "$dest" ] && cmp -s -- "$src" "$dest" \
    && [ "$(stat -c '%u:%g %a' -- "$dest")" = "0:0 ${mode#0}" ]; then
    ok "$dest up to date"
    return 1
  fi
  change "install $dest" bash -c 'install -m "$3" -o 0 -g 0 "$1" "$2.new" && mv -f "$2.new" "$2"' _ "$src" "$dest" "$mode"
  return 0
}

# ---- 1. accounts ---------------------------------------------------------------
printf '\n== accounts\n'
ensure_account "$SVC_NAME" "$SVC_UID"
ensure_account "$PG_NAME" "$PG_UID"

# ---- 2. directories --------------------------------------------------------------
printf '\n== directories\n'
# 0711: gha must be able to walk to kamal.env; the files inside carry the secrecy.
ensure_dir "$ETC_DIR" 0 0 0711
ensure_traversable "$ROOT_DIR"
ensure_dir "$ROOT_DIR/repos" "$SVC_UID" "$SVC_UID" 0700
ensure_dir "$ROOT_DIR/repos/locks" "$SVC_UID" "$SVC_UID" 0700
ensure_dir "$ROOT_DIR/control" "$SVC_UID" "$SVC_UID" 0700
ensure_dir "$ROOT_DIR/backups" "$SVC_UID" "$SVC_UID" 0700
if [ -d "$ROOT_DIR/pg" ] && [ ! -L "$ROOT_DIR/pg" ]; then
  ensure_traversable "$ROOT_DIR/pg"
elif [ ! -e "$ROOT_DIR/pg" ]; then
  change "create $ROOT_DIR/pg (root 0711)" install -d -m 0711 -o 0 -g 0 "$ROOT_DIR/pg"
else
  fail "$ROOT_DIR/pg is not a directory"
fi
ensure_dir "$ROOT_DIR/pg/data" "$PG_UID" "$PG_UID" 0700
# The unix socket directory: postgres (10002) writes, the service group
# (10001) may enter it to connect.
ensure_dir "$ROOT_DIR/pgsocket" "$PG_UID" "$SVC_UID" 0750

# State of the root-run host scripts (alerts, offsite, authority copies). The
# autodeploy state and checkout under the same base are created by the script.
ensure_dir "$STATE_BASE" 0 0 0755
for sub in alerts offsite authority; do
  ensure_dir "$STATE_BASE/$sub" 0 0 0700
done
# 0711: the drill's scratch directories are made reachable for uid 10001 (see
# muster-cloud-restore-drill); the contents stay 0700 or root-only.
ensure_dir "$STATE_BASE/drill" 0 0 0711

printf '\n== witness (own filesystem, never the root disk)\n'
if mountpoint -q "$WITNESS_MOUNT"; then
  src=$(findmnt -n -o SOURCE,FSTYPE "$WITNESS_MOUNT" | head -1)
  ok "$WITNESS_MOUNT is a mount point ($src)"
  if [ "$(stat -c %d -- "$WITNESS_MOUNT")" = "$(stat -c %d -- "$ROOT_DIR")" ]; then
    fail "$WITNESS_MOUNT is on the same filesystem as $ROOT_DIR"
  else
    ensure_dir "$WITNESS_MOUNT/w" "$SVC_UID" "$SVC_UID" 0700
  fi
else
  fail "$WITNESS_MOUNT is not mounted: refusing to create the witness directory on the root disk (mount the witness partition first)"
fi

# ---- 3. secret and settings files (checked, never created) --------------------
printf '\n== files under %s (values are never printed)\n' "$ETC_DIR"

# has_key file KEY — a non-empty KEY=VALUE line exists.
has_key() { grep -Eq "^$2=.+" "$1"; }

# has_line file KEY — a KEY= line exists (its value may be empty).
has_line() { grep -Eq "^$2=" "$1"; }

# key_filled file KEY — the (last) value of KEY is not empty after the same
# one-pair-of-quotes removal deploy/kamal/secret does. Nothing is printed.
key_filled() {
  local v
  v=$(grep -E "^$2=" "$1" | tail -n 1) || true
  v=${v#*=}
  case $v in
    \"*\") v=${v#\"}; v=${v%\"} ;;
    \'*\') v=${v#\'}; v=${v%\'} ;;
  esac
  [ -n "$v" ]
}

# check_keys file label key... — every key has a non-empty value. Keys listed
# after a literal "--" must only be present: the Paddle ones, which may be
# empty (billing off; see check_billing below).
check_keys() {
  local file=$1 label=$2 key missing="" optional=0
  shift 2
  for key in "$@"; do
    if [ "$key" = "--" ]; then
      optional=1
    elif [ "$optional" -eq 1 ]; then
      has_line "$file" "$key" || missing="$missing $key"
    else
      has_key "$file" "$key" || missing="$missing $key"
    fi
  done
  if [ -z "$missing" ]; then
    ok "$label has all required keys"
  else
    todo "$label is missing:$missing"
  fi
}

# Billing is optional: an empty MUSTER_PADDLE_API_KEY means "billing off" and
# the webhook secret and price ids may stay empty. With an API key, all four
# must be filled in. Runs after both files were looked at.
check_billing() {
  local missing="" key
  if [ ! -f "$KAMAL_ENV" ] || [ -L "$KAMAL_ENV" ] || ! has_line "$KAMAL_ENV" MUSTER_PADDLE_API_KEY; then
    return 0 # already reported above
  fi
  if ! key_filled "$KAMAL_ENV" MUSTER_PADDLE_API_KEY; then
    note "billing is off: MUSTER_PADDLE_API_KEY is empty (fill the four Paddle values and redeploy to enable it; deploy/KAMAL.md)"
    return 0
  fi
  key_filled "$KAMAL_ENV" MUSTER_PADDLE_WEBHOOK_SECRET || missing="$missing MUSTER_PADDLE_WEBHOOK_SECRET($KAMAL_ENV)"
  for key in MUSTER_PADDLE_PRICE_PERSONAL MUSTER_PADDLE_PRICE_TEAM_SEAT; do
    if [ -f "$DEPLOY_ENV" ] && [ ! -L "$DEPLOY_ENV" ] && key_filled "$DEPLOY_ENV" "$key"; then
      :
    else
      missing="$missing $key($DEPLOY_ENV)"
    fi
  done
  if [ -z "$missing" ]; then
    ok "billing is on: the Paddle API key, webhook secret and both price ids are set"
  else
    todo "MUSTER_PADDLE_API_KEY is set, so billing needs these non-empty:$missing"
  fi
}

if getent passwd "$DEPLOY_USER" >/dev/null; then
  ok "account $DEPLOY_USER exists"
  if id -nG "$DEPLOY_USER" | tr ' ' '\n' | grep -qx docker; then
    ok "$DEPLOY_USER is in the docker group"
  else
    todo "add $DEPLOY_USER to the docker group (usermod -aG docker $DEPLOY_USER)"
  fi
else
  todo "account $DEPLOY_USER does not exist"
fi

if [ -f "$KAMAL_ENV" ] && [ ! -L "$KAMAL_ENV" ]; then
  if getent passwd "$DEPLOY_USER" >/dev/null; then
    # Owned by gha (it reads it through bin/kamal), nobody else.
    if [ "$(stat -c '%U:%G %a' -- "$KAMAL_ENV")" = "$DEPLOY_USER:$DEPLOY_USER 600" ]; then
      ok "$KAMAL_ENV ($DEPLOY_USER:$DEPLOY_USER 0600)"
    else
      change "fix $KAMAL_ENV -> $DEPLOY_USER:$DEPLOY_USER 0600" bash -c 'chown "$2:$2" "$1" && chmod 0600 "$1"' _ "$KAMAL_ENV" "$DEPLOY_USER"
    fi
  fi
  check_keys "$KAMAL_ENV" "$KAMAL_ENV" MUSTER_DATABASE_URL MUSTER_OIDC_ISSUER \
    MUSTER_OIDC_CLIENT_ID MUSTER_OIDC_CLIENT_SECRET POSTGRES_PASSWORD \
    -- MUSTER_PADDLE_API_KEY MUSTER_PADDLE_WEBHOOK_SECRET
else
  todo "$KAMAL_ENV does not exist ($DEPLOY_USER:$DEPLOY_USER 0600; keys in deploy/KAMAL.md, step 2)"
fi

if [ -f "$DEPLOY_ENV" ] && [ ! -L "$DEPLOY_ENV" ]; then
  if grep -Evq '^(#.*|)$|^(MUSTER_CLOUD_PUBLIC_URL|MUSTER_PADDLE_ENVIRONMENT|MUSTER_PADDLE_PRICE_PERSONAL|MUSTER_PADDLE_PRICE_TEAM_SEAT|MUSTER_OIDC_EXTRA_AUDIENCES|ALERT_TARGET)=' "$DEPLOY_ENV"; then
    fail "$DEPLOY_ENV has lines other than the non-secret settings (bin/kamal refuses them)"
  fi
elif [ -e "$DEPLOY_ENV" ] || [ -L "$DEPLOY_ENV" ]; then
  fail "$DEPLOY_ENV must be a regular file, not a symlink"
else
  note "$DEPLOY_ENV does not exist: defaults apply and billing price ids stay empty (deploy/KAMAL.md, step 3)"
fi

check_billing

if [ "$ops_env_ok" -eq 1 ]; then
  check_keys "$OPS_ENV" "$OPS_ENV" MUSTER_DATABASE_URL MUSTER_REPO_ROOT MUSTER_CONTROL_DIR \
    MUSTER_CONTROL_WITNESS MUSTER_BACKUP_ROOT MUSTER_BACKUP_AGE_RECIPIENT
  # The host-side tool and the service share these paths; a typo here would
  # make them look at different stores.
  for pair in "MUSTER_REPO_ROOT=$ROOT_DIR/repos" "MUSTER_CONTROL_DIR=$ROOT_DIR/control" \
    "MUSTER_CONTROL_WITNESS=$WITNESS_MOUNT/w/witness.json" "MUSTER_BACKUP_ROOT=$ROOT_DIR/backups"; do
    if grep -qxF "$pair" "$OPS_ENV"; then
      ok "$OPS_ENV: $pair"
    else
      fail "$OPS_ENV must contain exactly the line $pair (same path as the container)"
    fi
  done
fi

# ---- 3b. the drill-only age identity ------------------------------------------------
# The monthly restore drill opens Drive snapshots without a person: it needs a
# private key on this host. That key is a SECOND identity, used by nothing else;
# the main backup key stays off the host. Backups and authority copies are
# encrypted to both (MUSTER_BACKUP_AGE_RECIPIENTS in ops.env).
printf '\n== restore drill identity (%s)\n' "$DRILL_KEY"
drill_pub=""
if ! command -v age-keygen >/dev/null 2>&1; then
  fail "age-keygen not found in PATH (it comes with age)"
elif [ -L "$DRILL_KEY" ] || { [ -e "$DRILL_KEY" ] && [ ! -f "$DRILL_KEY" ]; }; then
  fail "$DRILL_KEY must be a regular file"
else
  if [ ! -e "$DRILL_KEY" ]; then
    change "generate the drill-only age identity $DRILL_KEY (root 0400)" \
      bash -c 'umask 077 && rm -f -- "$1.new" && age-keygen -o "$1.new" >/dev/null 2>&1 && chmod 0400 "$1.new" && mv -f -- "$1.new" "$1"' _ "$DRILL_KEY"
  fi
  if [ -f "$DRILL_KEY" ]; then
    if [ "$(stat -c '%u:%g %a' -- "$DRILL_KEY")" = "0:0 400" ]; then
      ok "$DRILL_KEY (root:root 0400)"
    else
      change "fix $DRILL_KEY -> root:root 0400" bash -c 'chown 0:0 "$1" && chmod 0400 "$1"' _ "$DRILL_KEY"
    fi
    drill_pub=$(age-keygen -y "$DRILL_KEY" 2>/dev/null | head -n 1) || drill_pub=""
    if [[ $drill_pub =~ ^age1[a-z0-9]+$ ]]; then
      ok "the drill identity has a valid public key"
    else
      drill_pub=""
      fail "$DRILL_KEY is not an age identity (move it away and run this script again to make a new one)"
    fi
  fi
fi

# add_drill_recipient FILE PUBKEY — append the key to MUSTER_BACKUP_AGE_RECIPIENTS
# (comma separated; the line is created when there is none), atomically, keeping
# ops.env root:root 0600. Never prints the file.
add_drill_recipient() {
  python3 -I - "$1" "$2" <<'PY'
import os, sys
path, pub = sys.argv[1], sys.argv[2]
key = "MUSTER_BACKUP_AGE_RECIPIENTS="
with open(path) as handle:
    lines = handle.read().splitlines()
index = None
for i, line in enumerate(lines):
    if line.startswith(key):
        index = i
if index is None:
    lines.append(key + pub)
else:
    value = lines[index][len(key):].strip().strip("\"'")
    items = [item.strip() for item in value.split(",") if item.strip()]
    if pub not in items:
        items.append(pub)
    lines[index] = key + ",".join(items)
tmp = path + ".new"
try:
    os.unlink(tmp)
except FileNotFoundError:
    pass
fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as handle:
    handle.write("\n".join(lines) + "\n")
    handle.flush()
    os.fsync(handle.fileno())
if os.geteuid() == 0:  # (this script only runs as root; the guard lets tests run it)
    os.chown(tmp, 0, 0)
os.replace(tmp, path)
PY
}

if [ "$ops_env_ok" -eq 1 ]; then
  if [ -n "$drill_pub" ]; then
    have_recipients=$(sed -n 's/^MUSTER_BACKUP_AGE_RECIPIENTS=//p' "$OPS_ENV" | tail -n 1 | tr -d "\"' \r")
    main_recipient=$(sed -n 's/^MUSTER_BACKUP_AGE_RECIPIENT=//p' "$OPS_ENV" | tail -n 1 | tr -d "\"' \r")
    if [ "$drill_pub" = "$main_recipient" ]; then
      fail "the drill key is the same as MUSTER_BACKUP_AGE_RECIPIENT: the drill key must be a separate identity"
    elif [[ ",$have_recipients," == *",$drill_pub,"* ]]; then
      ok "$OPS_ENV lists the drill key as an extra backup recipient"
    else
      change "add the drill public key to MUSTER_BACKUP_AGE_RECIPIENTS in $OPS_ENV" add_drill_recipient "$OPS_ENV" "$drill_pub"
    fi
  else
    note "the drill key is added to $OPS_ENV once the drill identity exists"
  fi
fi

# ---- 4. docker network ------------------------------------------------------------
printf '\n== docker network %s\n' "$KAMAL_NET"
if ! docker info >/dev/null 2>&1; then
  fail "docker daemon is not reachable"
elif docker network inspect "$KAMAL_NET" >/dev/null 2>&1; then
  subnets=$(docker network inspect "$KAMAL_NET" --format '{{range .IPAM.Config}}{{.Subnet}} {{end}}')
  subnets=${subnets% }
  if [ "$subnets" = "$KAMAL_SUBNET" ]; then
    ok "network $KAMAL_NET exists with subnet $KAMAL_SUBNET"
  else
    fail "network $KAMAL_NET exists with subnet '$subnets', not $KAMAL_SUBNET; refusing to touch it (every container on it would have to be recreated)"
  fi
else
  change "create docker network $KAMAL_NET ($KAMAL_SUBNET)" \
    docker network create --driver bridge --subnet "$KAMAL_SUBNET" "$KAMAL_NET"
fi

# ---- 5. host scripts (no sudoers: gha has no privilege here) ---------------------
printf '\n== host scripts in %s\n' "$SBIN"
for script in "${HOST_SCRIPTS[@]}"; do
  if [ ! -f "$HERE/$script" ]; then
    fail "missing $HERE/$script"
    continue
  fi
  put_file "$HERE/$script" "$SBIN/$script" 0755 || true
done

# The previous design let gha run muster-cloud-install-ops as root. Nothing
# needs that now (autodeploy runs as root): the rule is removed, not kept.
if [ -e "$OLD_SUDOERS_FILE" ] || [ -L "$OLD_SUDOERS_FILE" ]; then
  change "remove $OLD_SUDOERS_FILE (user $DEPLOY_USER no longer needs any privilege)" rm -f -- "$OLD_SUDOERS_FILE"
else
  ok "no sudoers rule for $DEPLOY_USER ($OLD_SUDOERS_FILE is absent)"
fi
if grep -rIlE --exclude="${OLD_SUDOERS_FILE##*/}" "^[^#]*\\b$DEPLOY_USER\\b.*muster-cloud" /etc/sudoers /etc/sudoers.d 2>/dev/null | grep -q .; then
  fail "a sudoers rule that lets $DEPLOY_USER run a muster-cloud script still exists under /etc/sudoers*; remove it by hand (grep -rn muster-cloud /etc/sudoers /etc/sudoers.d)"
fi

# ---- 6. systemd units ------------------------------------------------------------
printf '\n== systemd timers\n'
# (named "timers" for history: it also holds the path unit that starts the
# authority copy after a change of the journal)
timers=(muster-cloud-backup.timer muster-cloud-retention.timer muster-cloud-offsite.timer
  muster-cloud-authority-offsite.timer muster-cloud-authority-offsite.path
  muster-cloud-backup-check.timer muster-cloud-autodeploy.timer
  muster-cloud-guard.timer muster-cloud-restore-drill.timer)

if [ "$ops_env_ok" -eq 1 ]; then
  stage=$WORK/units
  mkdir -p -- "$stage"
  if out=$("$REPO/deploy/install-systemd.sh" --root "$stage" --env-file "$OPS_ENV" 2>&1); then
    ok "backup and retention units rendered and verified by install-systemd.sh"
    for unit in muster-cloud-backup.service muster-cloud-backup.timer \
      muster-cloud-retention.service muster-cloud-retention.timer; do
      put_file "$stage/etc/systemd/system/$unit" "$UNIT_DIR/$unit" 0644 || true
    done
  else
    printf '%s\n' "$out" | sed 's/^/       /'
    fail "install-systemd.sh rejected $OPS_ENV or the units"
  fi
else
  note "skipping backup/retention units until $OPS_ENV is valid"
fi

# The other units are installed as they are in deploy/systemd/. They are checked
# first with systemd-analyze, on a copy whose ExecStart points at /bin/true (the
# scripts may not be installed yet in --check mode).
verify_own_units() {
  local vdir=$WORK/verify unit verify
  command -v systemd-analyze >/dev/null 2>&1 || {
    note "systemd-analyze not found; the units were not verified"
    return 0
  }
  mkdir -p -- "$vdir"
  for unit in "${OWN_UNITS[@]}"; do
    [ -f "$REPO/deploy/systemd/$unit" ] || {
      fail "missing $REPO/deploy/systemd/$unit"
      return 1
    }
    # (the drill runs the script of the source checkout, the others the installed one)
    sed -E 's#^(ExecStart=)(/usr/local/sbin|/var/lib/muster-cloud/source/deploy/kamal)/muster-cloud-[a-z-]+#\1/bin/true#' "$REPO/deploy/systemd/$unit" >"$vdir/$unit"
  done
  verify=$(cd "$vdir" && systemd-analyze verify --man=no "${OWN_UNITS[@]/#/$vdir/}" 2>&1) || true
  if [ -n "$verify" ]; then
    printf '%s\n' "$verify" | sed 's/^/       /'
    fail "systemd-analyze rejected the muster-cloud units"
    return 1
  fi
  ok "muster-cloud units verified by systemd-analyze"
}

if verify_own_units; then
  for unit in "${OWN_UNITS[@]}"; do
    put_file "$REPO/deploy/systemd/$unit" "$UNIT_DIR/$unit" 0644 || true
  done
fi

# Drop-ins: a 6-hourly backup at :41 (this host has another backup at 03:17),
# ordering after Docker (the database is a container), an alert when backup or
# retention fail, and an authority copy right after every successful backup.
dropin_tmp=$WORK/dropins
mkdir -p -- "$dropin_tmp"
printf '%s\n' '[Timer]' 'OnCalendar=' 'OnCalendar=*-*-* 00/6:41:00' >"$dropin_tmp/schedule.conf"
printf '%s\n' '[Unit]' 'After=docker.service' >"$dropin_tmp/docker.conf"
printf '%s\n' '[Unit]' 'OnFailure=muster-cloud-alert@%n.service' >"$dropin_tmp/alert.conf"
printf '%s\n' '[Unit]' 'OnSuccess=muster-cloud-authority-offsite.service' >"$dropin_tmp/authority.conf"
ensure_dropin() { # dir name srcfile
  local dir=$UNIT_DIR/$1 file=$2 src=$3
  if [ -f "$dir/$file" ] && cmp -s -- "$src" "$dir/$file"; then
    ok "$dir/$file up to date"
    return
  fi
  change "install $dir/$file" bash -c 'install -d -m 0755 -o 0 -g 0 "$1" && install -m 0644 -o 0 -g 0 "$2" "$1/$3"' _ "$dir" "$src" "$file"
}
ensure_dropin muster-cloud-backup.timer.d 10-schedule.conf "$dropin_tmp/schedule.conf"
ensure_dropin muster-cloud-backup.service.d 10-docker.conf "$dropin_tmp/docker.conf"
ensure_dropin muster-cloud-retention.service.d 10-docker.conf "$dropin_tmp/docker.conf"
ensure_dropin muster-cloud-backup.service.d 20-alert.conf "$dropin_tmp/alert.conf"
ensure_dropin muster-cloud-retention.service.d 20-alert.conf "$dropin_tmp/alert.conf"
ensure_dropin muster-cloud-backup.service.d 30-authority-copy.conf "$dropin_tmp/authority.conf"

# A restore drill cut short by a reboot leaves plaintext restored data in its
# scratch directory: remove it at boot (the next drill also removes leftovers).
printf '%s\n' "R! $STATE_BASE/drill/muster-restore-drill.*" >"$dropin_tmp/drill.tmpfiles"
if [ -f /etc/tmpfiles.d/muster-cloud-drill.conf ] && cmp -s -- "$dropin_tmp/drill.tmpfiles" /etc/tmpfiles.d/muster-cloud-drill.conf; then
  ok "/etc/tmpfiles.d/muster-cloud-drill.conf up to date"
else
  change "install /etc/tmpfiles.d/muster-cloud-drill.conf" \
    bash -c 'install -d -m 0755 /etc/tmpfiles.d && install -m 0644 -o 0 -g 0 "$1" /etc/tmpfiles.d/muster-cloud-drill.conf' _ "$dropin_tmp/drill.tmpfiles"
fi

# A fresh host has no `deployed` record: until `init` has run, nothing may
# deploy (the timer would pull the HEAD of main onto an empty database).
# guard-only keeps the guard but stops deploys; the bootstrap removes it. It is
# created BEFORE the timers are enabled below, never on a host that deployed and
# never when autodeploy itself calls this script (its first deploy runs it before
# it writes `deployed`; the marker would block every later run).
if [ -e "$STATE_BASE/autodeploy/deployed" ]; then
  ok "this host has deployed before ($STATE_BASE/autodeploy/deployed): no guard-only marker is added"
elif [ "${MUSTER_HOST_SETUP_CALLER:-}" = autodeploy ]; then
  ok "called by autodeploy: no guard-only marker is added"
elif [ -e "$STATE_BASE/autodeploy/guard-only" ] || [ -e "$STATE_BASE/autodeploy/hold" ]; then
  ok "fresh host: $STATE_BASE/autodeploy already has a guard-only or hold marker"
else
  change "fresh host: create $STATE_BASE/autodeploy/guard-only (no deploy before init)" \
    bash -c 'install -d -m 0755 "$1" && : >"$1/guard-only"' _ "$STATE_BASE/autodeploy"
fi

if [ "$MODE" = apply ] && [ "$n_change" -gt 0 ]; then
  # Cheap and idempotent: pick up any unit or drop-in written above.
  systemctl daemon-reload || fail "systemctl daemon-reload failed"
fi
for timer in "${timers[@]}"; do
  if [ ! -f "$UNIT_DIR/$timer" ] && [ "$MODE" = apply ]; then
    fail "$timer is not installed; cannot enable it"
    continue
  fi
  if systemctl is-enabled --quiet "$timer" 2>/dev/null && systemctl is-active --quiet "$timer" 2>/dev/null; then
    ok "$timer is enabled and active"
  else
    change "enable and start $timer" systemctl enable --now "$timer"
  fi
done

# The drill's "enabled since" mark: the check that watches the drill counts its
# grace period from here until the first successful drill.
if [ "$MODE" = apply ] && [ -d "$STATE_BASE/drill" ] && [ ! -e "$STATE_BASE/drill/enabled-since" ] \
  && systemctl is-enabled --quiet muster-cloud-restore-drill.timer 2>/dev/null; then
  change "record when the restore drill was enabled" touch "$STATE_BASE/drill/enabled-since"
fi

# ---- 7. host tools the timers need -------------------------------------------------
printf '\n== host tools for backups\n'
for tool in age age-keygen git tar rclone python3 curl runuser flock timeout; do
  if command -v "$tool" >/dev/null 2>&1; then
    ok "$tool found"
  else
    fail "$tool not found in PATH"
  fi
done
# pg_dump (backups) and pg_restore (recoveries and the drill) must be the same
# major version as the server (16): a newer client writes a dump or a restore
# script the 16 server rejects (found while rehearsing the restore drill with a
# 18.x client). Not blocking (a deploy must not stop for it): the restore drill
# fails loudly if it ever matters.
# (the drill also needs psql, createdb and dropdb of the same version; the drill
# itself refuses with a clear message when one is missing)
for pgtool in pg_dump pg_restore psql; do
  if command -v "$pgtool" >/dev/null 2>&1; then
    pgv=$("$pgtool" --version | sed -n 's/^[a-z_]* (PostgreSQL) \([0-9]*\).*/\1/p')
    if [ "${pgv:-0}" = 16 ]; then
      ok "$pgtool major version 16 (same as the server)"
    elif [ "${pgv:-0}" -gt 16 ] 2>/dev/null; then
      note "$pgtool is version $pgv, the server is 16: backups and restores may be refused; install postgresql-client-16 and put it first in PATH"
    else
      fail "$pgtool is version '${pgv:-unknown}', need 16"
    fi
  else
    fail "$pgtool not found in PATH (postgresql-client-16)"
  fi
done
if command -v rclone >/dev/null 2>&1; then
  rconf=$(rclone config file 2>/dev/null | tail -1)
  if [ "$rconf" = /root/.config/rclone/rclone.conf ]; then
    ok "rclone config is $rconf"
  else
    fail "rclone config is '$rconf'; the offsite unit expects /root/.config/rclone/rclone.conf"
  fi
  if rclone listremotes 2>/dev/null | grep -qx 'gdrive:'; then
    ok "rclone remote gdrive: is configured for root"
  else
    todo "rclone remote gdrive: is not configured for root"
  fi
fi

# The alert channel (Hermes). A missing CLI does not block a deploy; it is
# reported so it is not forgotten: alerts would then reach only the journal.
if [ -x /root/.local/bin/hermes ] || command -v hermes >/dev/null 2>&1; then
  ok "hermes found (alerts go to Discord)"
else
  note "hermes not found: alerts are written to the journal only (muster-cloud-alert needs 'hermes send')"
fi

# Anonymous pulls: the GHCR package must be public, because the host holds no
# registry credential at all. Checked the way a Docker client does it: an
# anonymous pull token, then the tag list. A network failure is only a note.
printf '\n== anonymous image pull (ghcr.io/%s)\n' "$GHCR_PACKAGE"
if command -v curl >/dev/null 2>&1 && command -v python3 >/dev/null 2>&1; then
  ghcr_tok=""
  ghcr_rc=0
  ghcr_tok=$(curl -sS --max-time 20 "https://ghcr.io/token?service=ghcr.io&scope=repository:$GHCR_PACKAGE:pull" 2>/dev/null \
    | python3 -I -c 'import json,sys; print(json.load(sys.stdin).get("token",""))' 2>/dev/null) || ghcr_rc=$?
  if [ "$ghcr_rc" -ne 0 ] || [ -z "$ghcr_tok" ]; then
    note "could not get an anonymous token from ghcr.io (network?); the package visibility was not checked"
  else
    code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 20 -H "Authorization: Bearer $ghcr_tok" \
      "https://ghcr.io/v2/$GHCR_PACKAGE/tags/list?n=1" 2>/dev/null) || code=000
    case $code in
      200) ok "ghcr.io/$GHCR_PACKAGE can be listed anonymously (the package is public)" ;;
      000) note "ghcr.io did not answer; the package visibility was not checked" ;;
      *) todo "ghcr.io/$GHCR_PACKAGE answers HTTP $code to an anonymous request: make the package public once (deploy/KAMAL.md, step 1) or publish the first image" ;;
    esac
  fi
  unset ghcr_tok
fi

# ---- 8. Tailscale Funnel ---------------------------------------------------------
printf '\n== tailscale funnel (public HTTPS on port %s)\n' "$FUNNEL_PORT"

# funnel_state: prints "match" when :10000 is a funnel to the proxy, else
# "absent" or "other"; "others" prints a canonical dump of every serve entry
# except :10000, so we can prove we did not touch them.
serve_py() { python3 -I -c '
import json, sys
mode, port, target = sys.argv[1], sys.argv[2], sys.argv[3]
cfg = json.load(sys.stdin) or {}
def on_port(key):
    return str(key).rsplit(":", 1)[-1] == port
if mode == "others":
    out = {}
    for section in ("TCP", "Web", "Services", "AllowFunnel", "Foreground"):
        value = cfg.get(section)
        if isinstance(value, dict):
            kept = {k: v for k, v in value.items() if not on_port(k)}
            if kept:
                out[section] = kept
        elif value:
            out[section] = value
    print(json.dumps(out, sort_keys=True))
else:
    funnel = [k for k, v in (cfg.get("AllowFunnel") or {}).items() if on_port(k) and v]
    if not funnel:
        print("absent")
    else:
        handlers = [h for k in funnel for h in ((cfg.get("Web") or {}).get(k, {}).get("Handlers") or {}).items()]
        print("match" if handlers == [("/", {"Proxy": target})] else "other")
' "$@"; }

if ! command -v tailscale >/dev/null 2>&1; then
  fail "tailscale not found"
elif ! command -v python3 >/dev/null 2>&1; then
  fail "python3 not found (needed to read tailscale serve status --json)"
else
  target="http://127.0.0.1:$PROXY_HTTP_PORT"
  before=$(tailscale serve status --json 2>&1)
  if ! state=$(printf '%s' "$before" | serve_py state "$FUNNEL_PORT" "$target" 2>&1); then
    fail "cannot read tailscale serve status: $(printf '%s' "$before" | head -2)"
  elif [ "$state" = match ]; then
    ok "funnel :$FUNNEL_PORT -> $target is configured"
  else
    [ "$state" = other ] && note "funnel :$FUNNEL_PORT exists with a different target; it will be replaced"
    others_before=$(printf '%s' "$before" | serve_py others "$FUNNEL_PORT" "$target")
    if change "tailscale funnel --bg --https=$FUNNEL_PORT $target" \
      tailscale funnel --bg "--https=$FUNNEL_PORT" "$target"; then
      if [ "$MODE" = apply ]; then
        after=$(tailscale serve status --json 2>&1)
        others_after=$(printf '%s' "$after" | serve_py others "$FUNNEL_PORT" "$target")
        [ "$(printf '%s' "$after" | serve_py state "$FUNNEL_PORT" "$target")" = match ] \
          || fail "funnel :$FUNNEL_PORT is not active after configuring it (is Funnel allowed for this node?)"
        [ "$others_before" = "$others_after" ] \
          || fail "OTHER tailscale serve entries changed; inspect 'tailscale serve status' now"
      fi
    fi
  fi
fi

# ---- 9. kamal-proxy loopback ports ---------------------------------------------
printf '\n== kamal-proxy ports (loopback only)\n'
if docker inspect kamal-proxy >/dev/null 2>&1; then
  pub=$(docker port kamal-proxy 2>/dev/null | tr '\n' ' ')
  want80="80/tcp -> 127.0.0.1:$PROXY_HTTP_PORT"
  want443="443/tcp -> 127.0.0.1:$PROXY_HTTPS_PORT"
  if [[ $pub == *"$want80"* && $pub == *"$want443"* ]] && ! [[ $pub == *"0.0.0.0"* || $pub == *"[::]"* ]]; then
    ok "kamal-proxy publishes only on 127.0.0.1 ($pub)"
  else
    fail "kamal-proxy exists and publishes '$pub' (expected only 127.0.0.1:$PROXY_HTTP_PORT and :$PROXY_HTTPS_PORT); Kamal would reuse it: remove it by hand (docker rm -f kamal-proxy) so the first deploy recreates it"
  fi
else
  for port in "$PROXY_HTTP_PORT" "$PROXY_HTTPS_PORT"; do
    if ss -Hltn "sport = :$port" 2>/dev/null | grep -q .; then
      fail "something already listens on port $port; kamal-proxy could not bind it"
    else
      ok "port $port is free for kamal-proxy"
    fi
  done
fi

# ---- 10. SSH reachability that Kamal needs (never changed here) ----------------
printf '\n== ssh for Kamal (gha connects to %s)\n' "$TS_HOST_IP"
if ss -Hltn 'sport = :22' 2>/dev/null | awk '{print $4}' | grep -Eq "^(0\.0\.0\.0|\*|\[::\]|$TS_HOST_IP):22$"; then
  ok "sshd listens on $TS_HOST_IP:22 (or on all addresses)"
else
  todo "sshd does not listen on $TS_HOST_IP:22 or on all addresses"
fi
gha_home=$(getent passwd "$DEPLOY_USER" | cut -d: -f6 || true)
if [ -n "$gha_home" ] && [ -d "$gha_home/.ssh" ]; then
  if [ -s "$gha_home/.ssh/authorized_keys" ]; then
    ok "$DEPLOY_USER has authorized_keys"
  else
    todo "$gha_home/.ssh/authorized_keys is missing or empty (gha must be able to ssh to itself)"
  fi
  if ssh-keygen -F "$TS_HOST_IP" -f "$gha_home/.ssh/known_hosts" >/dev/null 2>&1; then
    ok "$DEPLOY_USER knows the host key of $TS_HOST_IP"
  else
    todo "$gha_home/.ssh/known_hosts has no entry for $TS_HOST_IP (run once: sudo -u $DEPLOY_USER ssh $DEPLOY_USER@$TS_HOST_IP true)"
  fi
else
  todo "no ~/.ssh for $DEPLOY_USER; it needs a key it can use to ssh to $TS_HOST_IP"
fi

# ---- summary --------------------------------------------------------------------
printf '\n== summary (%s)\n' "$MODE"
if [ "$MODE" = check ]; then
  printf 'would change: %s   failed/blocked: %s   to do by hand: %s\n' "$n_change" "$n_fail" "$n_todo"
  [ "$n_change" -eq 0 ] && [ "$n_fail" -eq 0 ] && [ "$n_todo" -eq 0 ] && exit 0
  exit 1
fi
printf 'changed: %s   failed/blocked: %s   to do by hand: %s\n' "$n_change" "$n_fail" "$n_todo"
[ "$n_fail" -eq 0 ] && [ "$n_todo" -eq 0 ] && exit 0
exit 1
