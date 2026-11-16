#!/usr/bin/env bash
# deploy/pg-test.sh — ephemeral, isolated PostgreSQL cluster for muster-cloud tests.
#
# Usage:
#   deploy/pg-test.sh -- <command> [args...]
#
# What it does:
#   1. Creates a throwaway cluster with initdb in a private mktemp dir (0700).
#   2. Configures it to listen ONLY on a Unix socket inside that dir (0700),
#      with listen_addresses='' (no TCP listener at all). pg_hba: local trust,
#      TCP reject. Trust is acceptable because the cluster holds only
#      synthetic test data and the socket dir is private to the invoking user.
#   3. Exports for <command>:
#        MUSTER_TEST_PG             libpq-style URL, trust auth, no password:
#                                   postgres://postgres@localhost/postgres?host=<socket dir>
#        MUSTER_TEST_PG_SOCKET_DIR  Unix socket directory (sqlx: .socket(...))
#        MUSTER_TEST_PG_DATABASE    postgres
#        MUSTER_TEST_PG_USER        postgres
#        MUSTER_TEST_PG_LOG         postgres log file
#   4. If MUSTER_TEST_PG_MIGRATE_CMD is set, runs it first (via bash -c) with
#      the same environment, preserving a failing status. The script never
#      invents SQL or migrations: callers decide whether and what to migrate.
#      Tests may equally run sqlx migrations themselves against MUSTER_TEST_PG.
#   5. Runs <command>, preserves its exit status, and on ANY exit (including
#      SIGINT/SIGTERM) stops ONLY the cluster it started (pg_ctl against its
#      own data dir, never pgrep/name matching) and removes ONLY its own
#      temp dir.
#
# Guarantees:
#   - Never connects to or reuses an existing PostgreSQL installation.
#   - Never touches Docker or a Docker daemon (CI lacks one).
#   - Never installs packages and never invokes sudo/apt.
#   - Unique socket dir and data dir per invocation; two simultaneous
#     launches don't collide and clean up independently.
#   - Prints no credentials: the trust URL carries no secret.
#
# Exit codes: 64 usage; 69 PostgreSQL binaries missing; 70 cluster failed to
# start; 77 refused as root (no delegate account available); otherwise the
# caller command's status.
#
# Root: initdb/pg_ctl refuse to run as root, so when this script is invoked
# as root it delegates ONLY initdb/pg_ctl to a dedicated non-root account
# (default: the `postgres` user installed with the server packages;
# override with PG_TEST_RUNUSER) via runuser(1) — no login shell, so HOME
# and the rest of the invoking environment are left untouched. The cluster
# runs as that account (0700 data dir, socket file 0700 owned by it); the
# test shell keeps its own user and connects through the socket (root
# bypasses file permissions). On an ordinary user account the script runs
# everything directly, as before. CI must apt-install PostgreSQL
# beforehand; this script never installs packages. See deploy/README.md.
#
# Cleanup removes the temp dir ONLY after the cluster stop is confirmed
# (pg_ctl success, postmaster.pid gone, or its postmaster no longer alive).
# If a stop cannot be confirmed, the data dir is preserved and the run
# status still wins; stop it manually with
#   pg_ctl -D <printed temp dir>/data -m immediate stop
# and then delete the dir by hand; it never affects other clusters.
#
# Known limitation: SIGKILL to this script cannot be trapped, so the cluster
# and temp dir would leak (same as any test harness).

set -u
set -o pipefail
umask 077

readonly PG_TEST_EXIT_USAGE=64
readonly PG_TEST_EXIT_MISSING=69
readonly PG_TEST_EXIT_START=70
readonly PG_TEST_EXIT_ROOT=77

pg_test_tmpdir=""
pg_test_data=""
pg_test_socket_dir=""
pg_test_ctl=""
pg_test_cmd_pid=""
pg_test_cleanup_done=""
pg_test_delegate=""

pg_test_fail() {
  printf 'pg-test.sh: %s\n' "$1" >&2
  exit "${2:-1}"
}

# Run a cluster step as the delegate account (root invocations) or directly.
pg_test_run_as() {
  if [ -n "$pg_test_delegate" ]; then
    runuser -u "$pg_test_delegate" -- "$@"
  else
    "$@"
  fi
}

# Classify only complete, known server messages. Never print log contents:
# PostgreSQL can put SQL or other caller data in arbitrary log records.
pg_test_diagnostics() {
  local pg_test_log="$pg_test_tmpdir/postgres.log"
  [ -n "$pg_test_tmpdir" ] && [ -r "$pg_test_log" ] || return 0
  tail -c 65536 -- "$pg_test_log" 2>/dev/null | LC_ALL=C awk '
    length($0) > 512 { next }
    !/^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9] [0-9][0-9]:[0-9][0-9]:[0-9][0-9](\.[0-9]+)? ([A-Z]+|[-+][0-9][0-9](:[0-9][0-9])?) \[[0-9]+\] (LOG|ERROR|FATAL|PANIC):  / { next }
    {
      severity = $0
      sub(/^[^]]*\] /, "", severity); sub(/:.*/, "", severity)
      sub(/^[^]]*\] (LOG|ERROR|FATAL|PANIC):  /, "")
      label = ""
      role = $0; sub(/ \(PID .*/, "", role)
      if (role ~ /^(server process|client backend|background writer|checkpointer|WAL writer|walwriter|startup process|archiver process|autovacuum launcher|autovacuum worker|logical replication launcher|logical replication worker|background worker "(logical replication launcher|parallel worker)")$/ &&
          $0 ~ / \(PID [0-9]+\) (was terminated by signal [0-9]+(: [A-Za-z ]+)?|exited with exit code [0-9]+)$/) {
        pid = $0; sub(/^.* \(PID /, "", pid); sub(/\).*$/, "", pid)
        if ($0 ~ / was terminated by signal /) {
          number = $0; sub(/^.*signal /, "", number); sub(/:.*/, "", number)
          label = role " terminated by signal " number " (PID " pid ")"
        } else {
          number = $0; sub(/^.*exit code /, "", number)
          label = role " exited with code " number " (PID " pid ")"
        }
      }
      else if ($0 == "terminating any other active server processes") label = "terminating active server processes"
      else if ($0 == "all server processes terminated; reinitializing") label = "reinitializing"
      else if ($0 ~ /^database system was interrupted; last known up at [0-9-]+ [0-9:.]+ ([A-Z]+|[-+][0-9][0-9](:[0-9][0-9])?)$/) label = "database system interrupted"
      else if ($0 == "database system was not properly shut down; automatic recovery in progress") label = "automatic recovery in progress"
      else if ($0 ~ /^redo starts at [0-9A-F]+\/[0-9A-F]+$/) label = "recovery redo started"
      else if ($0 == "redo is not required") label = "recovery redo not required"
      else if ($0 ~ /^redo done at [0-9A-F]+\/[0-9A-F]+( system usage: CPU: user: [0-9.]+ s, system: [0-9.]+ s, elapsed: [0-9.]+ s)?$/) label = "recovery redo completed"
      else if ($0 == "database system is ready to accept connections") label = "ready to accept connections"
      else if ($0 == "database system is starting up") label = "database system starting up"
      else if ($0 == "database system is shutting down" || $0 == "database system is shut down") label = "database system shutting down"
      else if ($0 == "received fast shutdown request" || $0 == "received immediate shutdown request" || $0 == "received smart shutdown request") label = "shutdown requested"
      else if ($0 == "out of memory" || $0 ~ /: (Out of memory|Cannot allocate memory)$/) label = "out of memory"
      else if ($0 ~ /: No space left on device$/) label = "no space left on device"
      else if ($0 ~ /: Disk quota exceeded$/) label = "disk quota exceeded"
      else if ($0 ~ /: Too many open files$/) label = "process file limit reached"
      else if ($0 ~ /: Too many open files in system$/) label = "system file limit reached"
      else if ($0 ~ /: Permission denied$/) label = "permission denied"
      else if (severity == "FATAL" || severity == "PANIC") label = severity " (unclassified)"
      if (label != "") records[++count % 20] = label
    }
    END {
      start = count > 20 ? count - 19 : 1
      for (i = start; i <= count; i++) print "pg-test.sh: PostgreSQL: " records[i % 20] > "/dev/stderr"
    }
  ' || true
}

# Stop only this run's cluster and delete only this run's temp dir. The
# temp dir is removed ONLY once the cluster stop is confirmed (pg_ctl
# success, postmaster.pid gone, or its postmaster no longer alive); if the
# stop cannot be confirmed, the data dir is preserved for manual recovery
# and the run status still wins.
pg_test_cleanup() {
  local pg_test_status=$?
  [ -n "$pg_test_cleanup_done" ] && return 0
  pg_test_cleanup_done=1
  trap '' HUP INT TERM EXIT
  if [ "$pg_test_status" -ne 0 ]; then
    pg_test_diagnostics
  fi
  local pg_test_stop_ok=""
  if [ -n "$pg_test_data" ] && [ -f "$pg_test_data/postmaster.pid" ]; then
    if pg_test_run_as "$pg_test_ctl" -D "$pg_test_data" -m fast -t 20 stop >/dev/null 2>&1 \
      || pg_test_run_as "$pg_test_ctl" -D "$pg_test_data" -m immediate -t 10 stop >/dev/null 2>&1; then
      pg_test_stop_ok=1
    fi
  else
    # Nothing running (start failed or never reached start): safe to remove.
    pg_test_stop_ok=1
  fi
  if [ -z "$pg_test_stop_ok" ] && [ -f "$pg_test_data/postmaster.pid" ]; then
    # Belt and braces: a crash-style exit may leave a stale pid file behind.
    local pg_test_pid=""
    pg_test_pid=$(head -n 1 "$pg_test_data/postmaster.pid" 2>/dev/null || true)
    if [ -n "$pg_test_pid" ] && ! kill -0 "$pg_test_pid" 2>/dev/null; then
      pg_test_stop_ok=1
    fi
  elif [ -z "$pg_test_stop_ok" ]; then
    pg_test_stop_ok=1
  fi
  if [ "$pg_test_stop_ok" != 1 ]; then
    printf 'pg-test.sh: warning: could not stop cluster in %s; NOT deleting its data\n' "$pg_test_data" >&2
    printf 'pg-test.sh: stop it manually with: pg_ctl -D %s -m immediate stop\n' "$pg_test_data" >&2
  elif [ -n "$pg_test_tmpdir" ] && [ -d "$pg_test_tmpdir" ]; then
    rm -rf -- "$pg_test_tmpdir" \
      || printf 'pg-test.sh: warning: could not remove %s\n' "$pg_test_tmpdir" >&2
  fi
  exit "$pg_test_status"
}

pg_test_on_signal() {
  local pg_test_sig=$1
  if [ -n "$pg_test_cmd_pid" ] && kill -0 "$pg_test_cmd_pid" 2>/dev/null; then
    kill -TERM "$pg_test_cmd_pid" 2>/dev/null || true
    wait "$pg_test_cmd_pid" 2>/dev/null || true
  fi
  exit "$pg_test_sig"
}

# Locate a PostgreSQL binary via PATH, then pg_config --bindir.
pg_test_find_bin() {
  local pg_test_name=$1 pg_test_path=""
  if pg_test_path=$(command -v -- "$pg_test_name" 2>/dev/null) && [ -n "$pg_test_path" ]; then
    printf '%s\n' "$pg_test_path"
    return 0
  fi
  if pg_test_path=$(command -v -- pg_config 2>/dev/null) && [ -n "$pg_test_path" ]; then
    local pg_test_bindir=""
    pg_test_bindir=$("$pg_test_path" --bindir 2>/dev/null) || pg_test_bindir=""
    if [ -n "$pg_test_bindir" ] && [ -x "$pg_test_bindir/$pg_test_name" ]; then
      printf '%s\n' "$pg_test_bindir/$pg_test_name"
      return 0
    fi
  fi
  return 1
}

main() {
  if [ "$(id -u)" -eq 0 ]; then
    # initdb/pg_ctl cannot run as root. Delegate ONLY those cluster steps to
    # a dedicated non-root account (the `postgres` user that ships with the
    # server packages, or PG_TEST_RUNUSER) via runuser -u: no login shell,
    # so HOME and the rest of the invoking environment stay untouched, and
    # <command> keeps running as the invoking (root) user.
    pg_test_delegate="${PG_TEST_RUNUSER:-postgres}"
    if ! command -v runuser >/dev/null 2>&1; then
      pg_test_fail "running as root and runuser(1) was not found: cannot delegate initdb/pg_ctl to a non-root account. Install util-linux, or run the launcher as an ordinary user." "$PG_TEST_EXIT_ROOT"
    fi
    if ! id -- "$pg_test_delegate" >/dev/null 2>&1; then
      pg_test_fail "running as root and delegate account '$pg_test_delegate' does not exist: set PG_TEST_RUNUSER, or install the PostgreSQL server packages (they create the 'postgres' user). This script never installs anything." "$PG_TEST_EXIT_ROOT"
    fi
  fi

  if [ "${1:-}" = "--" ]; then
    shift
  fi
  if [ "$#" -lt 1 ]; then
    pg_test_fail "usage: deploy/pg-test.sh -- <command> [args...]" "$PG_TEST_EXIT_USAGE"
  fi

  local pg_test_initdb=""
  pg_test_initdb=$(pg_test_find_bin initdb) \
    || pg_test_fail "initdb not found (PATH or pg_config --bindir). Install PostgreSQL server packages (e.g. apt-get install postgresql); this script never installs anything." "$PG_TEST_EXIT_MISSING"
  pg_test_ctl=$(pg_test_find_bin pg_ctl) \
    || pg_test_fail "pg_ctl not found (PATH or pg_config --bindir)." "$PG_TEST_EXIT_MISSING"

  pg_test_tmpdir=$(mktemp -d "${TMPDIR:-/tmp}/muster-pg-test.XXXXXX") \
    || pg_test_fail "mktemp -d failed" 1
  chmod 0700 "$pg_test_tmpdir"
  pg_test_data="$pg_test_tmpdir/data"
  pg_test_socket_dir="$pg_test_tmpdir/socket"
  if [ -n "$pg_test_delegate" ]; then
    # Delegate mode (invoked as root): the cluster account must be able to
    # create its data dir contents, socket and log, while the root test
    # shell keeps reaching the socket. Root bypasses file permissions, so a
    # 0700 data dir and 0700 socket file owned by the delegate stay private
    # to the cluster; 0755 on tmpdir/socket dir only lets the delegate
    # create its own files there.
    chmod 0755 "$pg_test_tmpdir" || pg_test_fail "chmod tmpdir failed" 1
    mkdir -m 0755 "$pg_test_socket_dir" || pg_test_fail "mkdir socket dir failed" 1
    chown "$pg_test_delegate" "$pg_test_socket_dir" || pg_test_fail "chown socket dir failed" 1
    mkdir -m 0700 "$pg_test_data" || pg_test_fail "mkdir data dir failed" 1
    chown "$pg_test_delegate" "$pg_test_data" || pg_test_fail "chown data dir failed" 1
    : >"$pg_test_tmpdir/postgres.log" || pg_test_fail "cannot create postgres log" 1
    chown "$pg_test_delegate" "$pg_test_tmpdir/postgres.log" || pg_test_fail "chown postgres log failed" 1
  else
    mkdir -m 0700 "$pg_test_socket_dir" || pg_test_fail "mkdir socket dir failed" 1
  fi

  trap pg_test_cleanup EXIT
  trap 'pg_test_on_signal 129' HUP
  trap 'pg_test_on_signal 130' INT
  trap 'pg_test_on_signal 143' TERM

  pg_test_run_as "$pg_test_initdb" --no-locale --encoding=UTF8 -A trust -U postgres \
    --pgdata="$pg_test_data" >/dev/null \
    || pg_test_fail "initdb failed (see stderr above)" "$PG_TEST_EXIT_START"

  # Socket-only listener; TCP rejected even if listen_addresses were ever
  # re-enabled. Trust is scoped to local connections through the private
  # 0700 socket dir (synthetic data only).
  cat >"$pg_test_data/pg_hba.conf" <<'PG_TEST_HBA'
# Managed by deploy/pg-test.sh. Trust for local synthetic-data tests via a
# private 0700 Unix socket only; every TCP method is rejected.
local  all  all  trust
host   all  all  127.0.0.1/32  reject
host   all  all  ::1/128       reject
PG_TEST_HBA

  cat >>"$pg_test_data/postgresql.conf" <<PG_TEST_CONF
# --- managed by deploy/pg-test.sh (appended; later lines win) ---
listen_addresses = ''
unix_socket_directories = '$pg_test_socket_dir'
unix_socket_permissions = 0700
# Ephemeral cluster, synthetic data only: skip durability for test speed.
fsync = off
synchronous_commit = off
full_page_writes = off
PG_TEST_CONF

  if [ -n "$pg_test_delegate" ]; then
    # Root wrote both config files with umask 077; the postmaster runs as
    # the delegate account and must be able to read them.
    chmod 0644 "$pg_test_data/pg_hba.conf" "$pg_test_data/postgresql.conf" \
      || pg_test_fail "cannot make cluster config readable by delegate account" 1
  fi

  if ! pg_test_run_as "$pg_test_ctl" -D "$pg_test_data" -l "$pg_test_tmpdir/postgres.log" \
        -w -t 60 start >/dev/null; then
    pg_test_fail "postgres failed to start" "$PG_TEST_EXIT_START"
  fi

  local pg_test_isready=""
  pg_test_isready=$(pg_test_find_bin pg_isready) || pg_test_isready=""
  if [ -n "$pg_test_isready" ]; then
    local pg_test_i
    for pg_test_i in 1 2 3 4 5; do
      "$pg_test_isready" -d "postgres://postgres@localhost/postgres?host=$pg_test_socket_dir" >/dev/null 2>&1 && break
      if [ "$pg_test_i" = 5 ]; then
        pg_test_fail "cluster did not accept connections" "$PG_TEST_EXIT_START"
      fi
      sleep 1
    done
  fi

  local pg_test_url="postgres://postgres@localhost/postgres?host=$pg_test_socket_dir"
  export MUSTER_TEST_PG="$pg_test_url"
  export MUSTER_TEST_PG_SOCKET_DIR="$pg_test_socket_dir"
  export MUSTER_TEST_PG_DATABASE="postgres"
  export MUSTER_TEST_PG_USER="postgres"
  export MUSTER_TEST_PG_LOG="$pg_test_tmpdir/postgres.log"

  printf 'pg-test.sh: ephemeral PostgreSQL ready (socket-only, trust, synthetic data)\n' >&2
  printf 'pg-test.sh: MUSTER_TEST_PG=%s\n' "$pg_test_url" >&2

  # Caller-supplied migration hook: the caller says what to run; this script
  # never invents SQL or migrations.
  if [ -n "${MUSTER_TEST_PG_MIGRATE_CMD:-}" ]; then
    printf 'pg-test.sh: running MUSTER_TEST_PG_MIGRATE_CMD\n' >&2
    bash -c "$MUSTER_TEST_PG_MIGRATE_CMD"
    local pg_test_migrate_status=$?
    if [ "$pg_test_migrate_status" -ne 0 ]; then
      exit "$pg_test_migrate_status"
    fi
  fi

  "$@" &
  pg_test_cmd_pid=$!
  wait "$pg_test_cmd_pid"
  local pg_test_cmd_status=$?
  exit "$pg_test_cmd_status"
}

main "$@"
