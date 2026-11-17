#!/usr/bin/env bash
# deploy/backup-drill.sh — end-to-end backup/restore drill for Muster Cloud.
#
#   deploy/backup-drill.sh
#
# Composition (all synthetic data, one ephemeral PostgreSQL from
# deploy/pg-test.sh, the real muster-cloud binary, real Git, real Ed25519
# device signatures via the optional Cargo-built deploy/harness/drill.rs):
#
#   1. Fixture IdP (harness `idp`), then the deployment's lifecycle authority
#      created EXPLICITLY by the real `muster-cloud-ops init` on the pristine
#      database and repo root (control dir + independent witness under the
#      drill dir), then the real service (it never creates an authority).
#   2. `seed` TWICE (two tenants, T1 and T2, each its own workspace, device
#      and machine): OIDC browser-style login, workspace creation, RFC 8628
#      device flow, machine link with a real link proof, signed genesis push
#      plus one signed incremental push — all through the public HTTP API.
#   3. `deploy/pg-backup.sh` → `muster-cloud-ops backup run`: one exported
#      PostgreSQL snapshot, its tenants' Git only, age-encrypted while
#      streaming to a synthetic recipient, strict manifest, published into a
#      managed backup root (production-mode ops: no development settings).
#   4. AFTER the backup, T1 is deleted through the public API and purged by
#      the original service (journaled tombstone + purge).
#   5. `deploy/pg-restore.sh` refuses a missing witness, a rolled-back journal
#      and a foreign authority (exit 73, targets torn down), then restores
#      the PRE-DELETION snapshot into a NEW database + NEW repo root. The
#      restore joins as a NEW candidate incarnation and replays the journal:
#      T1 is erased from the new targets before any integrity gate.
#   6. A service on the candidate refuses to start (78). The candidate is
#      activated (`muster-cloud-ops activate`), which fences the original.
#   7. A SECOND real service on the restored targets, and through the public
#      API only:
#        - T2 recovers: `verify` (ready, session, token, signed full pull at
#          the pre-backup head, a signed incremental push ACCEPTED);
#        - T1 stays gone: not listed, its repository absent, and
#          `lifecycle-report` shows it purged with no residue;
#        - the ORIGINAL instance is fenced: /readyz 503 and a write (POST
#          /v1/workspaces) refused with 503.
#   8. Restore into already-existing targets is still refused (exit 66).
#
# Encryption: the drill takes the encrypted path a deployment takes: a
# synthetic age key pair (age-keygen, in the drill dir) is the recipient, and
# the restore decrypts with its identity file while streaming. `age` and
# `age-keygen` are required.
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
ops_bin=${MUSTER_DRILL_OPS_BIN:-$drill_target/debug/muster-cloud-ops}
[ -x "$ops_bin" ] || fail "muster-cloud-ops binary not found at $ops_bin (set MUSTER_DRILL_OPS_BIN)" "$DRILL_EXIT_MISSING"
command -v git >/dev/null 2>&1 || fail "git not found" "$DRILL_EXIT_MISSING"
command -v curl >/dev/null 2>&1 || fail "curl not found" "$DRILL_EXIT_MISSING"
command -v age >/dev/null 2>&1 || fail "age not found (encrypted backups)" "$DRILL_EXIT_MISSING"
command -v age-keygen >/dev/null 2>&1 || fail "age-keygen not found" "$DRILL_EXIT_MISSING"
command -v jq >/dev/null 2>&1 || fail "jq not found" "$DRILL_EXIT_MISSING"
command -v timeout >/dev/null 2>&1 || fail "timeout not found" "$DRILL_EXIT_MISSING"

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
ops_sha=$(sha256sum "$ops_bin" | cut -d' ' -f1)
log "ops binary: $ops_bin sha256=$ops_sha"

CLIENT_ID=muster-drill
CLIENT_SECRET=drill-client-secret-not-real

"$here/pg-test.sh" -- bash -c '
set -uo pipefail
umask 077
D="$1"; H="$2"; CLOUD_BIN="$3"; HERE="$4"; CLIENT_ID="$5"; CLIENT_SECRET="$6"; OPS_BIN="$7"
export PGHOST="$MUSTER_TEST_PG_SOCKET_DIR" PGUSER="$MUSTER_TEST_PG_USER"
export MUSTER_CLOUD_OPS="$OPS_BIN"
# The deployment authority: control store and an independent witness path.
CONTROL="$D/control"; WITNESS="$D/witness/witness.json"
mkdir -m 0700 -p "$D/witness"

fail_inner() { printf "backup-drill.sh: %s\n" "$1" >&2; exit "${2:-1}"; }

# --- 1) fixture IdP + real service ---
"$H" idp --port 0 --client-id "$CLIENT_ID" --client-secret "$CLIENT_SECRET" \
  --subject alice --email alice@drill.invalid --key-file "$D/idp-key.pem" \
  >"$D/idp.log" 2>&1 &
IDP=$!
ISSUER=""
for _ in $(seq 1 60); do
  ISSUER=$(sed -n "/^issuer=/{s///;p;q;}" "$D/idp.log")
  [ -n "$ISSUER" ] && break
  sleep 0.5
done
[ -n "$ISSUER" ] || fail_inner "IdP did not start" 70
# Explicit, pristine-only authority creation (never done by the service).
MUSTER_DATABASE_URL="$MUSTER_TEST_PG" MUSTER_REPO_ROOT="$D/repos" \
MUSTER_CONTROL_DIR="$CONTROL" MUSTER_CONTROL_WITNESS="$WITNESS" \
  "$OPS_BIN" init >"$D/init.json" 2>"$D/init.err" \
  || { cat "$D/init.err" >&2; fail_inner "muster-cloud-ops init failed" 1; }
printf "backup-drill.sh: lifecycle authority initialised (%s)\n" "$CONTROL" >&2
cp -R "$CONTROL" "$D/control-at-init"
PORT1=$("$H" freeport) || exit 70
MUSTER_ENV=development MUSTER_DEV_DELETION_GRACE_SECS=1 \
MUSTER_BIND="127.0.0.1:$PORT1" \
MUSTER_PUBLIC_URL="http://127.0.0.1:$PORT1" \
MUSTER_DATABASE_URL="$MUSTER_TEST_PG" \
MUSTER_OIDC_ISSUER="$ISSUER" \
MUSTER_OIDC_CLIENT_ID="$CLIENT_ID" \
MUSTER_OIDC_CLIENT_SECRET="$CLIENT_SECRET" \
MUSTER_REPO_ROOT="$D/repos" \
MUSTER_CONTROL_DIR="$CONTROL" MUSTER_CONTROL_WITNESS="$WITNESS" \
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

# --- 2) seed two tenants through the public API ---
for T in 1 2; do
  mkdir -m 0700 -p "$D/t$T"
  "$H" seed --endpoint "http://127.0.0.1:$PORT1" --idp "$ISSUER" \
    --work-dir "$D/t$T" --out "$D/state$T" || fail_inner "seed T$T failed" 1
done
WS1=$(sed -n "s/^ws=//p" "$D/state1"); WS2=$(sed -n "s/^ws=//p" "$D/state2")
HEAD1=$(sed -n "s/^head1=//p" "$D/state2")
HEAD2=$(sed -n "s/^head2=//p" "$D/state2")
[ -n "$WS1" ] && [ -n "$WS2" ] && [ "$WS1" != "$WS2" ] || fail_inner "seed did not report two workspaces" 1
[ -n "$HEAD1" ] && [ -n "$HEAD2" ] || fail_inner "seed did not report heads" 1
printf "backup-drill.sh: seeded T1 ws=%s, T2 ws=%s head1=%s head2=%s\n" \
  "$WS1" "$WS2" "$HEAD1" "$HEAD2" >&2

# --- 3) backup: snapshot-bound, encrypted to a synthetic age recipient ---
age-keygen -o "$D/backup-identity.txt" 2>/dev/null || fail_inner "age-keygen failed" 1
RECIPIENT=$(sed -n "s/^# public key: //p" "$D/backup-identity.txt")
[ -n "$RECIPIENT" ] || fail_inner "no age recipient" 1
MUSTER_BACKUP_AGE_RECIPIENT="$RECIPIENT" \
MUSTER_CONTROL_DIR="$CONTROL" MUSTER_CONTROL_WITNESS="$WITNESS" \
  "$HERE/pg-backup.sh" --database-url "$MUSTER_TEST_PG" --repo-root "$D/repos" \
  --output-dir "$D/backups" >"$D/backup.json" || fail_inner "backup failed" 1
BK=$(sed -n "s/.*\"path\":\"\([^\"]*\)\".*/\1/p" "$D/backup.json")
[ -n "$BK" ] && [ -f "$BK/manifest.json" ] || fail_inner "no backup produced" 1
[ -f "$BK/db.dump.age" ] && [ -f "$BK/git.tar.age" ] && [ ! -e "$BK/db.dump" ] \
  || fail_inner "backup is not encrypted" 1
printf "backup-drill.sh: encrypted snapshot %s\n" "$BK" >&2
grep -c "$WS1" "$BK/manifest.json" >/dev/null || fail_inner "the snapshot does not hold T1" 1

# --- 3b) T1 is deleted after the backup and purged by the original ---
COOKIE1=$(sed -n "s/^session_cookie=//p" "$D/state1")
CSRF1=$(sed -n "s/^csrf=//p" "$D/state1")
DEL=$(curl -sS --max-time 10 -o "$D/delete.json" -w "%{http_code}" -X DELETE \
  -H "Cookie: $COOKIE1" -H "Origin: http://127.0.0.1:$PORT1" -H "X-CSRF-Token: $CSRF1" \
  -H "Idempotency-Key: $(cat /proc/sys/kernel/random/uuid)" -H "Content-Type: application/json" \
  --data "{\"confirm_workspace_id\":\"$WS1\",\"confirm_workspace_name\":\"Drill WS\"}" \
  "http://127.0.0.1:$PORT1/v1/workspaces/$WS1")
[ "$DEL" = "202" ] || { cat "$D/delete.json" >&2; fail_inner "deleting T1: expected 202, got $DEL" 1; }
PURGED=""
for _ in $(seq 1 120); do
  LC=$(curl -sS --max-time 5 -H "Cookie: $COOKIE1" "http://127.0.0.1:$PORT1/v1/workspaces/$WS1/lifecycle") || LC=""
  case $LC in *"\"primary\":{\"status\":\"purged\""*) PURGED=1; break ;; esac
  sleep 0.5
done
[ -n "$PURGED" ] || fail_inner "T1 was never purged on the original" 1
[ ! -e "$D/repos/$WS1.git" ] || fail_inner "T1 repository left on the original" 1
printf "backup-drill.sh: T1 deleted and purged on the original after the backup\n" >&2

# --- 4) restore: refusals first, then into NEW targets ---
expect_refused() { # <label> <control-dir> <witness>
  "$HERE/pg-restore.sh" --backup "$BK" --admin-url "$MUSTER_TEST_PG" \
    --new-db drill_restored --new-repo-root "$D/restored-repos" \
  --age-identity-file "$D/backup-identity.txt" \
    --control-dir "$2" --control-witness "$3" >"$D/refused-$1.log" 2>&1
  rc=$?
  [ "$rc" = "73" ] || { cat "$D/refused-$1.log" >&2; fail_inner "restore with $1 authority: expected 73, got $rc" 1; }
  [ ! -e "$D/restored-repos" ] || fail_inner "restore with $1 authority left targets behind" 1
  printf "backup-drill.sh: restore refused with %s authority (73)\n" "$1" >&2
}
expect_refused missing "$CONTROL" "$D/witness/absent.json"
# Rolled back: the journal as it was at init under the witness of today,
# after a candidate incarnation advanced the authority.
createdb drill_candidate || fail_inner "createdb drill_candidate failed" 1
MUSTER_REPO_ROOT="$D/repos" MUSTER_CONTROL_DIR="$CONTROL" MUSTER_CONTROL_WITNESS="$WITNESS" \
  "$OPS_BIN" register \
  --database-url "postgres://$MUSTER_TEST_PG_USER@localhost/drill_candidate?host=$MUSTER_TEST_PG_SOCKET_DIR" \
  >/dev/null || fail_inner "register candidate failed" 1
mkdir -m 0700 -p "$D/stale/witness"
cp -R "$D/control-at-init" "$D/stale/control"
cp "$WITNESS" "$D/stale/witness/witness.json"
expect_refused rolled-back "$D/stale/control" "$D/stale/witness/witness.json"
# Foreign: the authority of another deployment (own pristine targets).
createdb drill_foreign || fail_inner "createdb drill_foreign failed" 1
mkdir -m 0700 -p "$D/foreign/witness" "$D/foreign/repos"
MUSTER_DATABASE_URL="postgres://$MUSTER_TEST_PG_USER@localhost/drill_foreign?host=$MUSTER_TEST_PG_SOCKET_DIR" \
MUSTER_REPO_ROOT="$D/foreign/repos" MUSTER_CONTROL_DIR="$D/foreign/control" \
MUSTER_CONTROL_WITNESS="$D/foreign/witness/witness.json" \
  "$OPS_BIN" init >/dev/null || fail_inner "foreign init failed" 1
expect_refused foreign "$D/foreign/control" "$D/foreign/witness/witness.json"

"$HERE/pg-restore.sh" --backup "$BK" --admin-url "$MUSTER_TEST_PG" \
  --new-db drill_restored --new-repo-root "$D/restored-repos" \
  --age-identity-file "$D/backup-identity.txt" \
  --control-dir "$CONTROL" --control-witness "$WITNESS" >"$D/restore.log" 2>&1 \
  || { cat "$D/restore.log" >&2; fail_inner "restore failed" 1; }
RECON=$(sed -n "s/^pg-restore.sh: reconciled: //p" "$D/restore.log")
INC=$(printf "%s" "$RECON" | jq -r ".db.incarnation_id")
ROLE=$(printf "%s" "$RECON" | jq -r ".db.role")
[ "$ROLE" = "candidate" ] && [ -n "$INC" ] && [ "$INC" != "null" ] \
  || fail_inner "the restore did not join as a new candidate (role=$ROLE)" 1
[ ! -e "$D/restored-repos/$WS1.git" ] || fail_inner "T1 repository came back with the restore" 1
printf "backup-drill.sh: restored as candidate incarnation %s (erased=%s)\n" \
  "$INC" "$(printf "%s" "$RECON" | jq -r ".erased")" >&2

# --- 5) the candidate never serves; activation fences the original ---
RESTORED_URL="postgres://$MUSTER_TEST_PG_USER@localhost/drill_restored?host=$MUSTER_TEST_PG_SOCKET_DIR"
svc2_env() {
  env MUSTER_ENV=development \
    MUSTER_BIND="127.0.0.1:$PORT2" \
    MUSTER_PUBLIC_URL="http://127.0.0.1:$PORT2" \
    MUSTER_DATABASE_URL="$RESTORED_URL" \
    MUSTER_OIDC_ISSUER="$ISSUER" \
    MUSTER_OIDC_CLIENT_ID="$CLIENT_ID" \
    MUSTER_OIDC_CLIENT_SECRET="$CLIENT_SECRET" \
    MUSTER_REPO_ROOT="$D/restored-repos" \
    MUSTER_CONTROL_DIR="$CONTROL" MUSTER_CONTROL_WITNESS="$WITNESS" "$@"
}
PORT2=$("$H" freeport) || exit 70
svc2_env timeout 60 "$CLOUD_BIN" >"$D/svc2-candidate.log" 2>&1
CRC=$?
[ "$CRC" = "78" ] || { tail -5 "$D/svc2-candidate.log" >&2; fail_inner "a service on the candidate: expected exit 78, got $CRC" 1; }
printf "backup-drill.sh: a service on the unactivated restore refuses to start (78)\n" >&2
svc2_env "$OPS_BIN" activate --incarnation "$INC" --database-url "$RESTORED_URL" >"$D/activate.json" 2>"$D/activate.err" \
  || { cat "$D/activate.err" >&2; fail_inner "activating the restore failed" 1; }
printf "backup-drill.sh: restore activated; the original incarnation is fenced\n" >&2

# --- 5b) second real instance on the restored targets ---
svc2_env "$CLOUD_BIN" >"$D/svc2.log" 2>&1 &
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
"$H" verify --endpoint "http://127.0.0.1:$PORT2" --state "$D/state2" \
  --expect-head "$HEAD2" --push yes --label restored-T2 \
  || fail_inner "restored-instance verification of T2 failed" 1
LIST=$(curl -sS --max-time 10 -H "Cookie: $COOKIE1" "http://127.0.0.1:$PORT2/v1/workspaces") \
  || fail_inner "listing workspaces on the restored instance failed" 1
case $LIST in *"$WS2"*) ;; *) fail_inner "T2 not listed on the restored instance" 1 ;; esac
case $LIST in *"$WS1"*) fail_inner "deleted T1 is listed again on the restored instance" 1 ;; esac
[ ! -e "$D/restored-repos/$WS1.git" ] || fail_inner "T1 repository present on the restored instance" 1
REPORT=$(svc2_env "$OPS_BIN" lifecycle-report --workspace "$WS1") \
  || fail_inner "lifecycle-report on the restored database failed" 1
[ "$(printf "%s" "$REPORT" | jq -r ".residue")" = "false" ] \
  && [ "$(printf "%s" "$REPORT" | jq -r ".lifecycle.primary")" = "purged" ] \
  || fail_inner "T1 not purged without residue on the restore: $REPORT" 1
printf "backup-drill.sh: T2 serves on the restore; T1 stays gone (purged, no residue)\n" >&2

# The original instance is fenced: not ready, and it refuses writes.
R1=$(curl -sS --max-time 5 -o /dev/null -w "%{http_code}" "http://127.0.0.1:$PORT1/readyz")
[ "$R1" = "503" ] || fail_inner "fenced original /readyz: expected 503, got $R1" 1
CSRF2=$(sed -n "s/^csrf=//p" "$D/state2"); COOKIE2=$(sed -n "s/^session_cookie=//p" "$D/state2")
W1=$(curl -sS --max-time 10 -o "$D/fenced-write.json" -w "%{http_code}" -X POST \
  -H "Cookie: $COOKIE2" -H "Origin: http://127.0.0.1:$PORT1" -H "X-CSRF-Token: $CSRF2" \
  -H "Idempotency-Key: $(cat /proc/sys/kernel/random/uuid)" -H "Content-Type: application/json" \
  --data "{\"name\":\"After fence\"}" "http://127.0.0.1:$PORT1/v1/workspaces")
[ "$W1" = "503" ] || { cat "$D/fenced-write.json" >&2; fail_inner "fenced original accepted a write (status $W1)" 1; }
printf "backup-drill.sh: original instance fenced (readyz 503, write refused 503)\n" >&2

# --- 7) refuse overwrite ---
"$HERE/pg-restore.sh" --backup "$BK" --admin-url "$MUSTER_TEST_PG" \
  --new-db drill_restored --new-repo-root "$D/restored-repos" \
  --age-identity-file "$D/backup-identity.txt" \
  --control-dir "$CONTROL" --control-witness "$WITNESS" >/dev/null 2>&1
RRC=$?
[ "$RRC" = "66" ] || fail_inner "overwrite refusal expected 66, got $RRC" 1

printf "DRILL-OK backup=%s restored_db=drill_restored incarnation=%s kept=%s deleted=%s head2=%s authority=%s\n" \
  "$BK" "$INC" "$WS2" "$WS1" "$HEAD2" "$CONTROL"
' _ "$drill_dir" "$harness" "$cloud_bin" "$here" "$CLIENT_ID" "$CLIENT_SECRET" "$ops_bin"
inner_rc=$?

if [ "$inner_rc" = "0" ]; then
  log "drill passed; artifacts kept under $drill_dir"
else
  log "drill FAILED (rc=$inner_rc); artifacts kept under $drill_dir for inspection"
fi
exit "$inner_rc"
