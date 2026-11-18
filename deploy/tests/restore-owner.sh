#!/usr/bin/env bash
# deploy/tests/restore-owner.sh — proves that deploy/pg-restore.sh, running as
# root, runs `muster-cloud-ops` AS THE OWNER of the control directory (so
# nothing needs a chown afterwards) and refuses a root-owned control directory
# or a witness in a directory with another owner. No database is needed: psql,
# createdb, pg_restore and git are stubs and the fake ops only reports who it
# runs as and then refuses (65), which stops the script right after the
# authority check.
#
#   deploy/tests/restore-owner.sh
#
# Root is faked with a user namespace (unshare, needs /etc/subuid for the
# caller); where that is not possible the test says so and exits 0 (the cargo
# tests of cloud/tests/deploy_restore.rs cover the refusals as an ordinary
# user).
#
# Exit 0 when every case passes (or is skipped), 1 otherwise.

set -uo pipefail
export LC_ALL=C

REPO=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)
T=$(mktemp -d "${TMPDIR:-/tmp}/restore-owner-test.XXXXXX")
trap 'rm -rf -- "$T"' EXIT
chmod 0755 "$T"

mkdir -p "$T/bin" "$T/deploy"
cp -- "$REPO/deploy/pg-restore.sh" "$REPO/deploy/pg-conn.sh" "$T/deploy/"
for stub in psql createdb pg_restore git; do
  printf '#!/bin/sh\nexit 0\n' >"$T/bin/$stub"
done
cat >"$T/bin/ops" <<'EOF'
#!/bin/sh
echo "ops ran as uid=$(id -u) gid=$(id -g) groups=$(id -G) cmd=$1" >&2
exit 65
EOF
chmod 0755 "$T"/bin/*

# The part that runs as (fake) root in the namespace.
cat >"$T/inner.sh" <<'EOF'
set -u
T=$1
mount -t tmpfs none /mnt || exit 90
chmod 0755 /mnt
mkdir -p /mnt/own/control /mnt/own/witness /mnt/rootdir /mnt/bk
chown 10001:10001 /mnt/own/control /mnt/own/witness
printf '{}\n' >/mnt/bk/manifest.json
printf 'precious\n' >/mnt/rootdir/hostname
restore() { # control-dir witness
  PATH=$T/bin:$PATH MUSTER_CLOUD_OPS=$T/bin/ops MUSTER_RESTORE_ADMIN_URL=postgres://u:p@localhost/postgres \
    "$T/deploy/pg-restore.sh" --backup /mnt/bk --new-db x --new-repo-root /mnt/repos \
    --control-dir "$1" --control-witness "$2" 2>&1
  echo "rc=$?"
}
echo "== owner"
restore /mnt/own/control /mnt/own/witness/witness.json
echo "== root-owned control directory"
restore /mnt/rootdir /mnt/rootdir/witness.json
echo "== witness in another owner's directory"
mkdir -p /mnt/other && chown 10002:10002 /mnt/other
restore /mnt/own/control /mnt/other/witness.json
echo "== witness path pointing into a root directory"
restore /mnt/own/control /mnt/rootdir/hostname
echo "== end"
cat /mnt/rootdir/hostname
EOF

out=$(unshare --user --mount --map-users=100000,0,65536 --map-groups=100000,0,65536 --setuid=0 --setgid=0 \
  bash "$T/inner.sh" "$T" 2>&1) || {
  printf 'restore-owner.sh: SKIPPED (no user namespace with subuid here): %s\n' "$(printf '%s' "$out" | head -n 2 | tr '\n' ' ')"
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

check "ops runs as the owner of the control directory (uid and gid)" "owner" "ops ran as uid=10001 gid=10001"
check "ops runs with no supplementary groups" "owner" "groups=10001 cmd=status"
check "a root-owned control directory is refused (73)" "root-owned control directory" "rc=73"
check "...and says why" "root-owned control directory" "owned by root"
check "a witness directory with another owner is refused (73)" "witness in another owner's directory" "rc=73"
check "...and says why" "witness in another owner's directory" "different owners"
check "a witness path into a root directory is refused (73)" "witness path pointing into a root directory" "rc=73"
check "the file that path named is untouched" "end" "precious"

printf '\n%s passed, %s failed\n' "$pass" "$fail"
[ "$fail" -eq 0 ]
