# deploy/pg-conn.sh — libpq URL parsing for the deploy scripts.
#
# Sourced (not executed) by deploy/pg-backup.sh and deploy/pg-restore.sh.
# pg_conn_export <libpq-url> turns a postgres:// URI into the standard PG*
# environment variables (PGHOST, PGPORT, PGUSER, PGPASSWORD, PGDATABASE,
# PGSSLMODE) so that pg_dump/psql/createdb/pg_restore connect without the
# connection string — and any password in it — appearing in argv.
#
# Recognized query parameters: `host` (a Unix socket directory, overriding the
# host) and `sslmode`. Other parameters are ignored. Values are percent-decoded.
# Callers must never print the exported variables or the input URL.
#
# Only postgres:// URIs are supported; a bare keyword/value connection string
# is rejected (pass a URI, or wrap one in a 0600 file read by the caller).

pg_conn_export() {
  pg_conn_url=$1
  case "$pg_conn_url" in
    postgres://* | postgresql://*) ;;
    *)
      printf 'pg-conn: only postgres:// URLs are supported\n' >&2
      return 64
      ;;
  esac

  pg_conn__decode() {
    # Decode %XX escapes only; leaves everything else untouched.
    printf '%b' "${1//%/\\x}"
  }

  pg_conn_rest=${pg_conn_url#*://}
  pg_conn_query=${pg_conn_rest#*\?}
  if [ "$pg_conn_query" = "$pg_conn_rest" ]; then
    pg_conn_query=""
  else
    pg_conn_rest=${pg_conn_rest%\?*}
  fi
  pg_conn_authority=${pg_conn_rest%%/*}
  pg_conn_path=${pg_conn_rest#*/}
  if [ "$pg_conn_path" = "$pg_conn_rest" ]; then
    pg_conn_path=""
  fi

  pg_conn_hostport=$pg_conn_authority
  pg_conn_user=""
  pg_conn_password=""
  case $pg_conn_authority in
    *@*)
      pg_conn_user=${pg_conn_authority%%@*}
      pg_conn_hostport=${pg_conn_authority#*@}
      case $pg_conn_user in
        *:*) pg_conn_password=${pg_conn_user#*:}; pg_conn_user=${pg_conn_user%%:*} ;;
      esac
      ;;
  esac
  pg_conn_host=$pg_conn_hostport
  pg_conn_port=""
  case $pg_conn_hostport in
    *:*) pg_conn_port=${pg_conn_hostport##*:}; pg_conn_host=${pg_conn_hostport%%:*} ;;
  esac

  pg_conn_db=$pg_conn_path
  case $pg_conn_db in
    */*) pg_conn_db=${pg_conn_db%%/*} ;;
  esac
  [ -n "$pg_conn_db" ] || pg_conn_db=postgres

  pg_conn_sslmode=""
  pg_conn_socket=""
  if [ -n "$pg_conn_query" ]; then
    pg_conn_oldifs=$IFS
    IFS='&'
    for pg_conn_param in $pg_conn_query; do
      IFS=$pg_conn_oldifs
      case $pg_conn_param in
        host=*) pg_conn_socket=${pg_conn_param#host=} ;;
        sslmode=*) pg_conn_sslmode=${pg_conn_param#sslmode=} ;;
      esac
    done
    IFS=$pg_conn_oldifs
  fi

  unset PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE PGSSLMODE PGHOSTADDR
  if [ -n "$pg_conn_socket" ]; then
    PGHOST=$(pg_conn__decode "$pg_conn_socket")
    export PGHOST
  elif [ -n "$pg_conn_host" ]; then
    PGHOST=$(pg_conn__decode "$pg_conn_host")
    export PGHOST
  fi
  if [ -n "$pg_conn_port" ]; then
    PGPORT=$(pg_conn__decode "$pg_conn_port")
    export PGPORT
  fi
  if [ -n "$pg_conn_user" ]; then
    PGUSER=$(pg_conn__decode "$pg_conn_user")
    export PGUSER
  fi
  if [ -n "$pg_conn_password" ]; then
    PGPASSWORD=$(pg_conn__decode "$pg_conn_password")
    export PGPASSWORD
  fi
  if [ -n "$pg_conn_sslmode" ]; then
    PGSSLMODE=$(pg_conn__decode "$pg_conn_sslmode")
    export PGSSLMODE
  fi
  PGDATABASE=$(pg_conn__decode "$pg_conn_db")
  export PGDATABASE
  return 0
}

# Database name only (for manifests/logs); never contains credentials.
pg_conn_dbname() {
  pg_conn_url=$1
  pg_conn_rest=${pg_conn_url#*://}
  pg_conn_rest=${pg_conn_rest%%\?*}
  pg_conn_db=${pg_conn_rest#*/}
  pg_conn_db=${pg_conn_db%%/*}
  printf '%s\n' "${pg_conn_db:-postgres}"
}
