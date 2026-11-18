#!/usr/bin/env bash
# deploy/install-systemd.sh — validate the operator environment file and
# STAGE the backup and retention units (deploy/systemd/) under a root.
#
#   deploy/install-systemd.sh --root <staging-dir> --env-file <ops.env> \
#                             [--bin /usr/local/bin/muster-cloud-ops]
#
# Never runs systemctl, never writes outside <staging-dir>, never copies the
# environment file (it holds the database URL): the units point at
# /etc/muster-cloud/ops.env, which the operator installs (mode 0600).
# Installing for real is a separate, manual step:
#   install -d -m 0700 -o muster-cloud -g muster-cloud <MUSTER_REPO_ROOT>/locks
#   cp <staging-dir>/etc/systemd/system/muster-cloud-* /etc/systemd/system/
#   systemctl daemon-reload
#   systemctl enable --now muster-cloud-backup.timer muster-cloud-retention.timer
#
# The environment file must:
#   - be a regular file with mode 0600;
#   - hold only KEY=VALUE lines (comments and blank lines allowed);
#   - set MUSTER_DATABASE_URL, MUSTER_REPO_ROOT, MUSTER_CONTROL_DIR,
#     MUSTER_CONTROL_WITNESS, MUSTER_BACKUP_ROOT (absolute, pairwise
#     disjoint) and MUSTER_BACKUP_AGE_RECIPIENT (an age1… public key);
#   - set no MUSTER_DEV_* setting, no MUSTER_ENV other than production, and
#     nothing about plaintext.
#
# Exit codes: 0 staged; 64 usage; 65 invalid environment file; 74 staging or
# unit verification failed.

set -u
set -o pipefail
umask 022

fail() {
  printf 'install-systemd.sh: %s\n' "$1" >&2
  exit "$2"
}

here=$(cd "$(dirname "$0")" && pwd)
root=""
env_file=""
bin=/usr/local/bin/muster-cloud-ops

while [ "$#" -gt 0 ]; do
  case "$1" in
    --root)
      [ "$#" -ge 2 ] || fail "--root needs a value" 64
      root=$2; shift 2 ;;
    --env-file)
      [ "$#" -ge 2 ] || fail "--env-file needs a value" 64
      env_file=$2; shift 2 ;;
    --bin)
      [ "$#" -ge 2 ] || fail "--bin needs a value" 64
      bin=$2; shift 2 ;;
    *)
      fail "unknown argument: $1" 64 ;;
  esac
done

[ -n "$root" ] || fail "no --root given (a staging directory; never /)" 64
[ "$root" != "/" ] || fail "--root / would install for real: stage into a directory" 64
[ -n "$env_file" ] || fail "no --env-file given" 64
case $bin in /*) ;; *) fail "--bin must be an absolute path" 64 ;; esac

[ -f "$env_file" ] && [ ! -L "$env_file" ] \
  || fail "environment file is not a regular file: $env_file" 65
mode=$(stat -c %a -- "$env_file") || fail "cannot stat $env_file" 65
[ "$mode" = "600" ] || fail "environment file must have mode 0600 (has $mode)" 65

declare -A conf=()
line_no=0
while IFS= read -r line || [ -n "$line" ]; do
  line_no=$((line_no + 1))
  case $line in
    '' | '#'*) continue ;;
  esac
  case $line in
    [A-Z_]*=*) ;;
    *) fail "line $line_no is not KEY=VALUE" 65 ;;
  esac
  key=${line%%=*}
  value=${line#*=}
  case $key in
    *[!A-Z0-9_]*) fail "line $line_no: invalid key" 65 ;;
    MUSTER_DEV_*) fail "$key: MUSTER_DEV_* settings are development only" 65 ;;
    *PLAINTEXT*) fail "$key: plaintext backups are never configured for timers" 65 ;;
  esac
  case $value in
    *plaintext*) fail "$key: plaintext backups are never configured for timers" 65 ;;
  esac
  conf[$key]=$value
done <"$env_file"

if [ -n "${conf[MUSTER_ENV]:-}" ] && [ "${conf[MUSTER_ENV]}" != production ]; then
  fail "MUSTER_ENV must be production (or unset) for the timers" 65
fi
[ -n "${conf[MUSTER_DATABASE_URL]:-}" ] || fail "MUSTER_DATABASE_URL is required" 65
[ -n "${conf[MUSTER_BACKUP_AGE_RECIPIENT]:-}" ] \
  || fail "MUSTER_BACKUP_AGE_RECIPIENT is required: the age recipient (public key) backups are encrypted to" 65
case ${conf[MUSTER_BACKUP_AGE_RECIPIENT]} in
  age1*) ;;
  *) fail "MUSTER_BACKUP_AGE_RECIPIENT is not an age recipient (age1…)" 65 ;;
esac

paths=(MUSTER_REPO_ROOT MUSTER_CONTROL_DIR MUSTER_CONTROL_WITNESS MUSTER_BACKUP_ROOT)
for key in "${paths[@]}"; do
  value=${conf[$key]:-}
  [ -n "$value" ] || fail "$key is required" 65
  case $value in
    /*) ;;
    *) fail "$key must be an absolute path" 65 ;;
  esac
  case /$value/ in
    */../* | */./*) fail "$key must not contain . or .. components" 65 ;;
  esac
done
for a in "${paths[@]}"; do
  for b in "${paths[@]}"; do
    [ "$a" != "$b" ] || continue
    pa=${conf[$a]%/}/
    pb=${conf[$b]%/}/
    case $pa in
      "$pb"*) fail "$a must be disjoint from $b (neither inside the other)" 65 ;;
    esac
  done
done

dest=$root/etc/systemd/system
work=$(mktemp -d "${TMPDIR:-/tmp}/muster-units.XXXXXX") || fail "mktemp failed" 74
trap 'rm -rf -- "$work"' EXIT
# Everything each command writes under ProtectSystem=strict (no optional
# `-` paths: a missing one must fail loudly, not silently become read-only):
#   both: the backup root and the lifecycle authority (control dir, and the
#   witness's directory: it may advance);
#   backup: also the repository root (coordination lock in locks/, and the
#   purge of overdue workspaces it runs before capturing).
witness_dir=$(dirname -- "${conf[MUSTER_CONTROL_WITNESS]}")
authority="${conf[MUSTER_CONTROL_DIR]%/} ${witness_dir%/}"
writable_retention="${conf[MUSTER_BACKUP_ROOT]%/} $authority"
writable_backup="$writable_retention ${conf[MUSTER_REPO_ROOT]%/}"
units=(muster-cloud-backup.service muster-cloud-backup.timer
       muster-cloud-retention.service muster-cloud-retention.timer)
for unit in "${units[@]}"; do
  src=$here/systemd/$unit
  [ -f "$src" ] || fail "missing unit template: $src" 74
  body=$(cat -- "$src") || fail "cannot read $src" 74
  body=${body//@BIN@/$bin}
  case $unit in
    muster-cloud-backup.service) body=${body//@WRITABLE@/$writable_backup} ;;
    *) body=${body//@WRITABLE@/$writable_retention} ;;
  esac
  printf '%s\n' "$body" >"$work/$unit" || fail "cannot render $unit" 74
done

# Verify the rendered units when systemd is available (with a stand-in
# binary: the real one need not exist on the staging host).
if command -v systemd-analyze >/dev/null 2>&1; then
  check=$work/check
  mkdir -- "$check" || fail "mktemp failed" 74
  for unit in "${units[@]}"; do
    sed -e "s|^ExecStart=$bin |ExecStart=/bin/true |" "$work/$unit" >"$check/$unit" \
      || fail "cannot prepare $unit for verification" 74
  done
  verify_out=$(cd "$check" && systemd-analyze verify --man=no "${units[@]/#/$check/}" 2>&1)
  verify_rc=$?
  # It exits 0 on parse warnings: any output is a rejection.
  if [ "$verify_rc" -ne 0 ] || [ -n "$verify_out" ]; then
    printf '%s\n' "$verify_out" >&2
    fail "systemd-analyze verify rejected the units" 74
  fi
  printf 'install-systemd.sh: units verified with systemd-analyze\n'
else
  printf 'install-systemd.sh: systemd-analyze not found; units not verified\n'
fi

mkdir -p -- "$dest" || fail "cannot create $dest" 74
# The coordination lock's directory, staged: the units need it to exist.
locks_dir=$root${conf[MUSTER_REPO_ROOT]%/}/locks
mkdir -p -m 0700 -- "$locks_dir" || fail "cannot stage $locks_dir" 74
for unit in "${units[@]}"; do
  install -m 0644 -- "$work/$unit" "$dest/$unit" || fail "cannot stage $unit" 74
done
printf 'install-systemd.sh: staged %s units under %s (nothing installed)\n' "${#units[@]}" "$dest"
printf 'install-systemd.sh: install %s as /etc/muster-cloud/ops.env (0600) before enabling\n' "$env_file"
printf 'install-systemd.sh: create %s owned by the service user (staged at %s)\n' \
  "${conf[MUSTER_REPO_ROOT]%/}/locks" "$locks_dir"
