#!/usr/bin/env bash
# deploy/backup-drill.sh — end-to-end backup/restore drill for Muster Cloud.
#
#   deploy/backup-drill.sh
#
# Composition (all synthetic data, one ephemeral PostgreSQL from
# deploy/pg-test.sh, the real muster-cloud binary, real Git, real Ed25519
# device signatures via the optional Cargo-built deploy/harness/drill.rs):
#
#   1. Fixture IdP (harness `idp`) + real service on an ephemeral cluster
#      (migrations applied by the service itself; MUSTER_REPO_ROOT mandatory).
#   2. `seed`: OIDC browser-style login, workspace creation, RFC 8628 device
#      flow, machine link with a real link proof, signed genesis push plus one
#      signed incremental push — everything through the public HTTP API.
#   3. `deploy/pg-backup.sh` (consistent cut: pg_dump first, git tar second).
#   4. `deploy/pg-restore.sh` into a NEW database + NEW repo root (pre-cutover
#      head/fsck checks included).
#   5. A SECOND real service instance is started ON THE RESTORED targets and
#      `verify` accredits recovery through the public API only:
#        - /readyz ready;
#        - the pre-backup browser session still authenticates (/v1/me);
#        - the pre-backup account token still lists the workspace;
#        - signed full pull returns the pre-backup head and a bundle whose
#          clone has exactly that head;
#        - a signed incremental push is ACCEPTED by the restored instance
#          (the restored trust anchor + Git objects verify the machine key);
#        - the ORIGINAL instance still serves the untouched original head.
#   6. Restore into already-existing targets is refused (exit 66) and the
#      active service never sees any of it.
#
# Encryption: the drill runs pg-backup.sh with --plaintext because the data is
# synthetic and `age` may be absent (see pg-backup.sh: without age, plaintext
# is refused unless explicitly allowed). The age code path is exercised
# wherever age + MUSTER_BACKUP_AGE_RECIPIENT are available.
#
# Exit status: non-zero on the first failed check (message on stderr). Prints
# a DRILL-OK line and the service binary hash on success.

set -u
set -o pipefail
umask 077

# shellcheck source=harness/build.sh (invoked, not sourced)
here=$(cd "$(dirname "$0")" && pwd)
WT=$(cd "$here/.." && pwd)

readonly DRILL_EXIT_USAGE=64
readonly DRILL_EXIT_MISSING=69

fail() {
  printf 'backup-drill.sh: %s\n' "$1" >&2
  exit "${2:-1}"
}

drill_target=${CARGO_TARGET_DIR:-$WT/target}
cloud_bin=${MUSTER_DRILL_CLOUD_BIN:-$drill_target/debug/muster-cloud}
[ -x "$cloud_bin" ] || fail "muster-cloud binary not found at $cloud_bin (set MUSTER_DRILL_CLOUD_BIN)" "$DRILL_EXIT_MISSING"
command -v git >/dev/null 2>&1 || fail "git not found" "$DRILL_EXIT_MISSING"
command -v curl >/dev/null 2>&1 || fail "curl not found" "$DRILL_EXIT_MISSING"

drill_dir=$(mktemp -d "${TMPDIR:-/tmp}/muster-drill.XXXXXX")
chmod 0700 "$drill_dir"

cleanup() {
  local status=$?
  trap '' EXIT INT TERM
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

log() { printf 'backup-drill.sh: %s\n' "$1" >&2; }

harness="$drill_dir/drill-harness"
"$here/harness/build.sh" "$harness" || fail "harness build failed" "$DRILL_EXIT_MISSING"

# Versions recorded for the evidence trail: exactly the previously built
# artifacts the drill ran against (no rebuild).
cloud_sha=$(sha256sum "$cloud_bin" | cut -d' ' -f1)
log "cloud binary: $cloud_bin sha256=$cloud_sha"

CLIENT_ID=muster-drill
CLIENT_SECRET=drill-client-secret-not-real

"$here/pg-test.sh" -- bash -c '
set -uo pipefail
umask 077
D="$1"; H="$2"; CLOUD_BIN="$3"; HERE="$4"; CLIENT_ID="$5"; CLIENT_SECRET="$6"
export PGHOST="$MUSTER_TEST_PG_SOCKET_DIR" PGUSER="$MUSTER_TEST_PG_USER"

fail_inner() { printf "backup-drill.sh: %s\n" "$1" >&2; exit "${2:-1}"; }

# --- 1) fixture IdP + real service ---
"$H" idp --port 0 --client-id "$CLIENT_ID" --client-secret "$CLIENT_SECRET" \
  --subject alice --email alice@drill.invalid --key-file "$D/idp-key.pem" \
  >"$D/idp.log" 2>&1 &
IDP=$!
ISSUER=""
for _ in $(seq 1 60); do
  ISSUER=$(sed -n "s/^issuer=//p" "$D/idp.log" | head -1)
  [ -n "$ISSUER" ] && break
  sleep 0.5
done
[ -n "$ISSUER" ] || fail_inner "IdP did not start" 70
PORT1=$("$H" freeport) || exit 70
MUSTER_ENV=development \
MUSTER_BIND="127.0.0.1:$PORT1" \
MUSTER_PUBLIC_URL="http://127.0.0.1:$PORT1" \
MUSTER_DATABASE_URL="$MUSTER_TEST_PG" \
MUSTER_OIDC_ISSUER="$ISSUER" \
MUSTER_OIDC_CLIENT_ID="$CLIENT_ID" \
MUSTER_OIDC_CLIENT_SECRET="$CLIENT_SECRET" \
MUSTER_REPO_ROOT="$D/repos" \
  "$CLOUD_BIN" >"$D/svc1.log" 2>&1 &
SVC1=$!
cleanup_local() {
  for pid in "$SVC2" "$SVC1" "$IDP"; do
    [ -n "${pid:-}" ] && kill -0 "$pid" 2>/dev/null && kill -TERM "$pid" 2>/dev/null
  done
  wait 2>/dev/null || true
}
SVC2=""
trap cleanup_local EXIT
for _ in $(seq 1 60); do
  curl -fsS --max-time 2 "http://127.0.0.1:$PORT1/readyz" >/dev/null 2>&1 && break
  kill -0 "$SVC1" 2>/dev/null || { tail -5 "$D/svc1.log" >&2; fail_inner "service 1 exited early" 70; }
  sleep 0.5
done
curl -fsS --max-time 2 "http://127.0.0.1:$PORT1/readyz" >/dev/null 2>&1 \
  || fail_inner "service 1 never ready" 70
printf "backup-drill.sh: service 1 (original) ready on %s (IdP %s)\n" "$PORT1" "$ISSUER" >&2

# --- 2) seed through the public API ---
"$H" seed --endpoint "http://127.0.0.1:$PORT1" --idp "$ISSUER" \
  --work-dir "$D" --out "$D/state" || fail_inner "seed failed" 1
HEAD1=$(sed -n "s/^head1=//p" "$D/state")
HEAD2=$(sed -n "s/^head2=//p" "$D/state")
[ -n "$HEAD1" ] && [ -n "$HEAD2" ] || fail_inner "seed did not report heads" 1
printf "backup-drill.sh: seeded ws=%s head1=%s head2=%s\n" \
  "$(sed -n "s/^ws=//p" "$D/state")" "$HEAD1" "$HEAD2" >&2

# --- 3) backup (consistent cut) ---
"$HERE/pg-backup.sh" --database-url "$MUSTER_TEST_PG" --repo-root "$D/repos" \
  --output-dir "$D/backups" --plaintext || fail_inner "backup failed" 1
BK=$(find "$D/backups" -maxdepth 1 -type d -name "muster-backup-*" | sort | tail -1)
[ -n "$BK" ] || fail_inner "no backup produced" 1

# --- 4) restore into NEW targets ---
"$HERE/pg-restore.sh" --backup "$BK" --admin-url "$MUSTER_TEST_PG" \
  --new-db drill_restored --new-repo-root "$D/restored-repos" \
  || fail_inner "restore failed" 1

# --- 5) second real instance on the restored targets ---
PORT2=$("$H" freeport) || exit 70
MUSTER_ENV=development \
MUSTER_BIND="127.0.0.1:$PORT2" \
MUSTER_PUBLIC_URL="http://127.0.0.1:$PORT2" \
MUSTER_DATABASE_URL="postgres://postgres@localhost/drill_restored?host=$MUSTER_TEST_PG_SOCKET_DIR" \
MUSTER_OIDC_ISSUER="$ISSUER" \
MUSTER_OIDC_CLIENT_ID="$CLIENT_ID" \
MUSTER_OIDC_CLIENT_SECRET="$CLIENT_SECRET" \
MUSTER_REPO_ROOT="$D/restored-repos" \
  "$CLOUD_BIN" >"$D/svc2.log" 2>&1 &
SVC2=$!
for _ in $(seq 1 60); do
  curl -fsS --max-time 2 "http://127.0.0.1:$PORT2/readyz" >/dev/null 2>&1 && break
  kill -0 "$SVC2" 2>/dev/null || { tail -5 "$D/svc2.log" >&2; fail_inner "service 2 exited early" 70; }
  sleep 0.5
done
curl -fsS --max-time 2 "http://127.0.0.1:$PORT2/readyz" >/dev/null 2>&1 \
  || fail_inner "service 2 (restored) never ready" 70
printf "backup-drill.sh: service 2 (restored) ready on %s\n" "$PORT2" >&2

# --- 6) recovery accreditation via the public API ---
"$H" verify --endpoint "http://127.0.0.1:$PORT2" --state "$D/state" \
  --expect-head "$HEAD2" --push yes --label restored \
  || fail_inner "restored-instance verification failed" 1
"$H" verify --endpoint "http://127.0.0.1:$PORT1" --state "$D/state" \
  --expect-head "$HEAD2" --push no --label original \
  || fail_inner "original-instance verification failed" 1

# --- 7) refuse overwrite ---
"$HERE/pg-restore.sh" --backup "$BK" --admin-url "$MUSTER_TEST_PG" \
  --new-db drill_restored --new-repo-root "$D/restored-repos" >/dev/null 2>&1
RRC=$?
[ "$RRC" = "66" ] || fail_inner "overwrite refusal expected 66, got $RRC" 1

printf "DRILL-OK backup=%s restored_db=drill_restored head1=%s head2=%s\n" \
  "$BK" "$HEAD1" "$HEAD2"
' _ "$drill_dir" "$harness" "$cloud_bin" "$here" "$CLIENT_ID" "$CLIENT_SECRET"
inner_rc=$?

if [ "$inner_rc" = "0" ]; then
  log "drill passed; artifacts kept under $drill_dir"
else
  log "drill FAILED (rc=$inner_rc); artifacts kept under $drill_dir for inspection"
fi
exit "$inner_rc"
