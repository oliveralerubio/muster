# deploy/

Test and operations launchers for Muster Cloud. Owned by GLM5.3Flash per
`IMPLEMENTATION-CONTRACT.md` (`deploy/**`). No migrations, Cargo manifests or
locks live here.

## pg-test.sh — ephemeral PostgreSQL for tests

```
deploy/pg-test.sh -- <command> [args...]
```

Creates a throwaway PostgreSQL cluster (initdb + pg_ctl), starts it listening
ONLY on a Unix socket inside a private 0700 temp dir, exports
`MUSTER_TEST_PG`, runs `<command>`, and exits with its status. On any exit —
including SIGINT/SIGTERM — it stops only the cluster it started (pg_ctl via
its own data dir, never pgrep/name matching) and removes only its own temp
dir.

### Environment exported to `<command>`

| Variable | Meaning |
| --- | --- |
| `MUSTER_TEST_PG` | libpq-style URL, trust auth, no password: `postgres://postgres@localhost/postgres?host=<socket dir>` |
| `MUSTER_TEST_PG_SOCKET_DIR` | Unix socket directory (sqlx: `PgConnectOptions::socket(...)`) |
| `MUSTER_TEST_PG_DATABASE` | `postgres` (default database; callers may create their own) |
| `MUSTER_TEST_PG_USER` | `postgres` |
| `MUSTER_TEST_PG_LOG` | postgres log file for this run |

### Migrations

The launcher never invents SQL or migrations. Either:

- run migrations inside your command (e.g. sqlx `migrate!` against
  `MUSTER_TEST_PG`), or
- set `MUSTER_TEST_PG_MIGRATE_CMD` to a caller-supplied command the launcher
  runs first (via `bash -c`) with the same environment; a failing status
  aborts before `<command>` runs.

Example (CI-style):

```
MUSTER_TEST_PG_MIGRATE_CMD='sqlx migrate run --source cloud/migrations' \
  deploy/pg-test.sh -- cargo test -p cloud
```

### Connecting from Rust (sqlx)

The URL works with libpq tools (`psql`, `pg_isready`) and sqlx
(`PgConnectOptions` honours the `?host=` socket parameter). The
guaranteed-explicit path is:

```rust
let opts = sqlx::postgres::PgConnectOptions::new()
    .socket(std::env::var("MUSTER_TEST_PG_SOCKET_DIR")?)
    .username(std::env::var("MUSTER_TEST_PG_USER")?)
    .database(std::env::var("MUSTER_TEST_PG_DATABASE")?);
```

### Isolation and safety

- Never connects to or reuses an existing PostgreSQL; never touches Docker
  (CI has no daemon); never installs packages or invokes sudo/apt.
- `listen_addresses=''`: no TCP listener at all; pg_hba rejects every TCP
  method even if the config were edited. Trust auth is scoped to local
  connections through the 0700 socket dir and is acceptable only because the
  cluster holds synthetic test data.
- Unique socket dir and data dir per invocation: two simultaneous launches
  don't collide.
- Prints no credentials (the trust URL carries no secret).

### Exit codes

`64` usage; `69` PostgreSQL binaries missing; `70` cluster failed to start;
`77` refused as root; otherwise the caller command's status.

### Root and CI

`initdb`/`pg_ctl` refuse to run as root, so when the script itself is invoked
as root it delegates ONLY those two cluster steps (plus their `pg_ctl stop`)
to a dedicated non-root account via `runuser -u` — no login shell, so `HOME`
and the rest of the invoking environment are left untouched, and `<command>`
keeps running as the invoking user. The delegate account is `postgres` (the
one created by the PostgreSQL server packages) or `PG_TEST_RUNUSER` if set.
`runuser` missing or no delegate account → exit `77`, as before. The cluster
runs as the delegate (0700 data dir, 0700 socket file); a root test shell
connects through the private socket because root bypasses file permissions.
CI should install PostgreSQL (`apt-get install postgresql` provides `initdb`,
`pg_ctl`, `pg_isready` and the `postgres` user) before running the launcher.
Binaries are looked up via `PATH` and `pg_config --bindir`; the launcher
never installs anything itself.

### Cleanup

The temp dir is removed only after the cluster stop is confirmed (`pg_ctl`
success, `postmaster.pid` gone, or the recorded postmaster no longer alive).
If a stop cannot be confirmed, the cluster's data dir is PRESERVED and the
run still exits with the caller command's status; stop it manually with
`pg_ctl -D <printed temp dir>/data -m immediate stop` and then delete the
dir by hand. The launcher never affects other clusters (including any
pre-existing host instance).

Production deployment on the `personal` VPS (Kamal, CI-built image, timers, offsite backups): see [`KAMAL.md`](KAMAL.md).

## Cloud CI workflow (`.github/workflows/cloud.yml`)

CI for the `muster-cloud` backend, separate from `.github/workflows/ci.yml`
(which keeps the OSS CLI gates and is not modified by this workflow).

- Runs on the same `ci-docker` runner pool as `ci.yml`: **no Docker daemon**,
  no preinstalled Rust/Node/PostgreSQL. Missing tools are installed with apt
  (`build-essential`, `git`, `postgresql` + `libpq-dev` +
  `postgresql-client`), each guarded so existing tooling is reused.
- Rust (`dtolnay/rust-toolchain@stable` with clippy/rustfmt) and Node.js 22
  (`actions/setup-node`) are set up like the current CI.
- Checks: `cargo fmt -p muster-cloud -- --check`, `cargo clippy -p
  muster-cloud --all-targets --all-features -- -D warnings`, and tests via
  `deploy/pg-test.sh -- cargo test -p muster-cloud --tests`.
- The cargo `target/` directory is cached across runs with
  `Swatinem/rust-cache@v2`.
- PostgreSQL is never a global service: every test step gets an ephemeral,
  socket-only cluster from `deploy/pg-test.sh`. **Tests are never skipped
  for lack of PostgreSQL** — if the cluster cannot be created, the launcher
  exits non-zero (69/70/77) and the job fails. On a root runner, the
  launcher delegates only `initdb`/`pg_ctl` to the package-installed
  `postgres` account (see above); no PG service is started and
  `HOME`/`CODEX_HOME` are never changed.

## Docker image (`deploy/Dockerfile`)

Build from the repository root (BuildKit builder; modern Docker defaults to
it; the per-Dockerfile ignore file is honored by BuildKit, and a legacy
builder only produces a larger context — the build stays correct):

```
docker build -f deploy/Dockerfile -t muster-cloud:local .
```

Properties:

- Multi-stage: builder compiles `muster-cloud` from the workspace with
  `cargo build --release --locked -p muster-cloud` (a `Cargo.lock` mismatch
  fails the build instead of silently upgrading dependencies); the runtime
  stage contains only the binary plus `ca-certificates` and `git`.
- Runs as non-root: fixed system account `muster` (uid/gid 10001), no login
  shell. Data/Git object store root `/var/lib/muster` is created 0700 and
  owned by the service account — mount your volume there.
- Listens on plain HTTP inside the container: `EXPOSE 8080`. TLS termination
  belongs to a reverse proxy in front (see below).
- No secrets and no development authentication are baked in. All runtime
  configuration comes from the environment; `MUSTER_ENV` defaults to
  production and the service rejects insecure production settings
  (non-https public URL, loopback OIDC issuer, missing database URL).
- Context hygiene: `deploy/Dockerfile.dockerignore` keeps `.git`,
  `target/`, `.hillclimb/`, `.claude/`, `node_modules` and local
  `.env*`/secret files out of the build context while hiding nothing the
  build needs (workspace manifests, `src/`, `cloud/` including
  `cloud/assets`).

Before serving real traffic, pin the two base images by digest
(`rust:1-slim-bookworm@sha256:...`, `debian:bookworm-slim@sha256:...` via
the `RUST_IMAGE`/`RUNTIME_IMAGE` build args) so builds are byte-stable
across time.

Run (environment injected, never baked):

```
docker run -d --name muster-cloud \
  --restart unless-stopped \
  -p 127.0.0.1:8080:8080 \
  -v muster-cloud-data:/var/lib/muster \
  --env-file /etc/muster-cloud/env \
  muster-cloud:local
```

## Runtime configuration

Config is read from the environment by the service (`cloud/src/config.rs`).
Values below are names only — provide real values through your orchestrator's
secret store; never bake them into the image or commit them.

| Variable | Required | Meaning |
| --- | --- | --- |
| `MUSTER_ENV` | no | `production` (default when unset) or `development`. Production enforces https and rejects loopback OIDC. |
| `MUSTER_BIND` | no | Socket address to listen on, default `127.0.0.1:8080`. In a container use `0.0.0.0:8080` so the port mapping can reach it. |
| `MUSTER_TRUSTED_PROXIES` | no | Comma-separated addresses or CIDR networks of the reverse proxies in front (e.g. `127.0.0.1,10.0.0.0/8`). Only requests from these peers have `X-Forwarded-For` read; its rightmost hop that is not a listed proxy is the client the per-address rate limits apply to. Unset: the TCP peer is the client. Not secret. |
| `MUSTER_DATABASE_URL` | **yes** | libpq URL of the managed PostgreSQL. Secret — inject via secret store. |
| `MUSTER_PUBLIC_URL` | **yes** | Public base URL of the service (https in production). Used for OIDC redirects and cookies. |
| `MUSTER_OIDC_ISSUER` | **yes** | OIDC issuer URL of the identity provider (https, non-loopback in production). |
| `MUSTER_OIDC_CLIENT_ID` | **yes** | OIDC client id. |
| `MUSTER_OIDC_CLIENT_SECRET` | **yes** | OIDC client secret. Secret — inject via secret store. |
| `MUSTER_OIDC_EXTRA_AUDIENCES` | no | Comma-separated extra `aud` values the ID token may carry next to the client id (default none = strict). Zitadel needs its project id here. Not secret. |
| `MUSTER_REPO_ROOT` | **yes** | Absolute path of the per-workspace bare Git object store (one `<workspace-id>.git` repo per workspace). There is no default: the service refuses to start without it. Back up this directory together with the database — see `muster-cloud-ops backup run` below. |
| `MUSTER_CONTROL_DIR` | **yes** | Absolute directory of the lifecycle authority's control store (hash-chained journal + lock). Must be disjoint from `MUSTER_REPO_ROOT` (neither inside the other). Not secret. |
| `MUSTER_CONTROL_WITNESS` | **yes** | Absolute path of the authority's independent witness file; not inside the control dir or the repo root. Put it on a DIFFERENT volume/device than the control dir. Not secret. |

### Lifecycle authority (`muster-cloud-ops`)

The service only serves a database that the deployment's lifecycle authority
(`cloud/src/control.rs`) recognises as its **active incarnation**. The service
never creates an authority: create it once, explicitly, on pristine targets
(empty database without accounts or workspaces, repo root without
repositories), with the operator tool shipped in the same image:

```
muster-cloud-ops init        # env: MUSTER_DATABASE_URL, MUSTER_REPO_ROOT,
                             #      MUSTER_CONTROL_DIR, MUSTER_CONTROL_WITNESS
muster-cloud-ops status [--database-url URL]
muster-cloud-ops restore-check [--database-url URL]   # read-only gate
muster-cloud-ops restore-finalize [--database-url URL] # restored db: new candidate + reconcile
muster-cloud-ops register --database-url URL          # new candidate incarnation
muster-cloud-ops activate --incarnation ID --database-url URL
```

Path policy (service, ops and `pg-restore.sh` alike): the three paths must be
absolute and are compared by their real filesystem identity, not their
spelling. `..` components are refused; symbolic links are resolved (the
longest existing ancestor, so not-yet-created leaves are fine for a first
`init`), and a dangling symbolic link is refused. The control dir must not be
inside the repo root nor contain it, and the witness must be in neither, after
resolution (refusal: exit 78, naming the variable). The service and ops then
use the resolved paths. `init` creates a missing control dir and witness
directory.

Database access per command: `init` and `register` apply schema migrations;
`status` and `activate` work on an already bound database without migrating;
`restore-check` is strictly read-only (every session transaction is
read-only, no migrations, no row locks), so it gives its verdict on a
restored copy or a read-only replica without changing it. A database without
the binding table (taken before the authority existed) is refused (65).

`init` is idempotent while the targets stay pristine and refuses (exit 66)
anything with data or an authority in use. Output is versioned JSON (v1) on
stdout; exit codes 64 usage, 65 authority refused, 66 not pristine, 74 I/O,
78 configuration.

Suggested volumes (no secrets involved): `/var/lib/muster/control` for
`MUSTER_CONTROL_DIR` and `/var/lib/muster-witness/witness.json` for
`MUSTER_CONTROL_WITNESS`, on two persistent volumes on different devices,
both outside `MUSTER_REPO_ROOT` and **outside backups and restores**. Losing
or rolling back journal and witness together, along with an equally old
database, cannot be detected; keeping them apart is what makes rollback,
divergence or a foreign store detectable.

Fencing: only the active incarnation serves. A service whose database is a
candidate or fenced refuses to start (78); one that keeps running after
another incarnation is activated refuses every request except `/healthz`
and `/readyz` with 503 `not_ready`, and checks again right before
answering, so a request that entered before the activation (a long push)
is never acknowledged. Its background workers (purge, billing
reconciliation, provider cancellations) check before every job and claim
nothing and call no provider once fenced. The check re-reads journal and
witness only when either file changed, honours only what the witness
covers, and retries a read that races a normal append. An activation that
died between journal and witness was never acknowledged: the next open
discards it (`status` reports `discarded_activation`), the source keeps
serving, and `activate` is simply rerun. A fenced incarnation can never be
activated again (65), and `purge-due`, `backup run` and `retention run`
refuse (65) on any database that is not the active one.

Billing/provider configuration (price IDs, webhook signing secret, fixture
IdP) is still being finalized in `cloud/src/config.rs` — placeholders only
here; **coordinate the exact variable names with the backend owner before
deploying** and do not hardcode credentials anywhere in this directory.

Paddle API key check: when billing is configured (`MUSTER_PADDLE_API_KEY`), a
production service checks its key in the background. The first check runs
right after startup, without delaying it, and then every 10 minutes. It runs
only while the instance is the active one, so a candidate or fenced instance
never uses the key. Each check makes one authenticated read of
subscriptions and one of transactions (one-item list pages; nothing is
created).

- A `401` (key invalid, revoked, or from another account or environment) or a
  `403` (the key lacks `subscription.read` or `transaction.read`) counts only
  when Paddle itself sends it: the service reads Paddle's error shape
  (`error.code` and `meta.request_id`) and nothing else, and logs neither
  the fields nor the body. A refusal is asked again, three attempts in
  about 15 seconds, before it counts.
- A refusal that holds on every attempt never stops the service. Sync,
  sign-in, health checks and Paddle webhooks keep working, and
  reconciliation keeps retrying. Only billing actions (checkout, plan
  change, cancellation, customer portal) answer `503 billing_unconfigured`
  without calling Paddle, and `GET /billing` reports `billing_configured:
  false`. The service logs one error that names every missing permission
  (never the key). Grant them to the key in Paddle; the next check makes
  billing available again by itself, with no restart.
- A network error, a timeout, a rate limit, a `5xx`, or a `401`/`403` that is
  not Paddle's (for example an API gateway or WAF in front of it) logs a
  warning and changes nothing.

Development services run the check only against a local provider fixture
(`MUSTER_PADDLE_BASE_URL`), never Paddle's own API.
`MUSTER_DEV_PADDLE_KEY_CHECK_SECS` shortens its interval in development and
is refused in production (exit 78).

Restart policy: run the service under a supervisor that restarts it after a
failure, so a failure that clears by itself also heals by itself. With
Docker, use `--restart unless-stopped`, as in the `docker run` example above.
With systemd, set `Restart=on-failure` and `RestartSec=30s` in the service
unit. A refused configuration exits 78 and the last log line before the exit
names the setting. A brief incident, such as the database restarting, is
cured by the next restart without anyone stepping in.

Rotating `MUSTER_PADDLE_PRICE_*` needs no cleanup. A subscription created
from a workspace's own checkout keeps that checkout's plan. Once granted,
it keeps following Paddle (cancellation, seat changes) without being traced
to its checkout again. Any other subscription with an unknown price is
refused and left untouched at Paddle. If Paddle cannot return the checkout
transaction, the payment stays pending and is retried. No second checkout
is offered meanwhile.

## Health endpoints

| Route | Kind | Checks |
| --- | --- | --- |
| `GET /healthz` | liveness | Nothing external: returns `{"status":"ok"}` if the process is up. |
| `GET /readyz` | readiness | Re-verifies the lifecycle authority on every probe (journal, witness, database binding; replays a projection that is behind) and requires the database to be the active incarnation; returns `not_ready` (503) with the reason otherwise, or if the database is unreachable, or if the control store stays locked by another process for over 10 s. |

Wire the load balancer's readiness probe to `/readyz` and liveness to
`/healthz`.

## Reverse proxy (HTTPS)

The service serves plain HTTP and must be deployed behind a reverse proxy
that terminates TLS. Requirements:

- Terminate HTTPS at the proxy (certificate management — e.g. ACME — is the
  proxy's job; nothing in this repository provisions certificates, and no
  deployment or certificate is claimed here).
- Forward to the container's `MUSTER_BIND` (e.g. `0.0.0.0:8080`).
- Set `MUSTER_PUBLIC_URL` to the public `https://` origin users actually
  visit; session cookies are `__Host-` prefixed, so scheme, host and port
  must match exactly.
- OIDC redirect URIs registered at the identity provider must be derived
  from the same public URL (`MUSTER_PUBLIC_URL/auth/callback`).
- Do not cache or log request bodies at the proxy (workspace history and
  secrets travel inside them).
- Set `MUSTER_TRUSTED_PROXIES` to the proxy's address so the per-address
  rate limits (link codes, login polls) apply to the real client; if it is
  not set, every client behind the proxy shares the proxy's single budget,
  and one busy client can get the others refused with `429 rate_limited`.

## Managed PostgreSQL prerequisites

- PostgreSQL **16 or newer** (contract requirement for the sqlx setup).
- A dedicated database, user and password for the service; the URL goes in
  `MUSTER_DATABASE_URL` (secret store).
- Require TLS between the service and the database (`sslmode=require` or
  stricter, per your provider).
- Migrations are the service/caller's responsibility (`sqlx` migrations in
  `cloud/migrations/`, applied by the service itself at startup); the deploy
  assets in this directory never invent or run SQL themselves.

## Backups and restore (`pg-backup.sh`, `pg-restore.sh`, `backup-drill.sh`)

The Cloud database (PostgreSQL) and the Git object store (`MUSTER_REPO_ROOT`)
are backed up together, as ONE snapshot, by `muster-cloud-ops backup run`
(`cloud/src/backup.rs`); `pg-backup.sh` is a thin wrapper around it.
Retention is `muster-cloud-ops retention run`. Both run from systemd timers
(`deploy/systemd/`, staged by `deploy/install-systemd.sh`).

### Snapshot-bound capture

- The backup/purge coordination lock (`<repo_root>/locks/capture.lock`) is
  held exclusive from the database snapshot through the Git archive. A purge
  holds it shared from its move to trash until its trash is erased, so no
  workspace is ever captured half-erased (the purge reports `waiting for a
  running backup capture to finish` and retries on its own).
- A read-only repeatable-read transaction exports a PostgreSQL snapshot;
  `pg_dump --snapshot` dumps exactly that state, and the tenant list (and
  their heads and deletion caps) is read in the same snapshot.
- The Git archive holds exactly those tenants' `<uuid>.git` trees: no root
  scratch, no orphan repository, no tenant created after the snapshot.
  Symlinks and special files are refused; transient Git files (`*.lock`,
  `*.tmp`, `tmp_*`, `incoming-*`) are skipped. Git objects are written before
  their PostgreSQL commit (`cloud/src/repo_store.rs`), so every captured head
  has its objects on disk.
- Before the snapshot, `backup run` reapplies the lifecycle journal (a crash
  between journal and projection, or a restored database; the authority and
  incarnation are checked) and purges every workspace already past its
  backup cap, ignoring the purge backoff. It refuses (exit 65) only if such a
  workspace still cannot be purged (e.g. live Git access never drains); the
  next timer run retries on its own.
- Per repository, everything but `objects/` (refs, `HEAD`, `config`) is
  copied into staging first and `objects/` is read live afterwards: a push
  writes objects before refs, so a push during the capture never leaves an
  archived ref without its objects (the restore's `git fsck` passes).
- Artifacts stream through `age -r` (no plaintext on disk) and are hashed
  while written. A strict, versioned manifest (`format: muster-backup`,
  `version: 2`: snapshot time, tenants and heads, encryption scheme and a
  hash of the recipient, normal expiry, artifact sizes and sha256, archive
  entry count and list hash) is written atomically; the snapshot is then
  published by renaming its staging directory.

### Managed backup root (`MUSTER_BACKUP_ROOT`)

Absolute, and disjoint from `MUSTER_REPO_ROOT`, `MUSTER_CONTROL_DIR` and
`MUSTER_CONTROL_WITNESS`. One host per root (leases are `flock`s).

```
snapshots/<backup_id>/   manifest.json, db.dump.age, git.tar.age
staging/<op_id>/         a capture in progress
ops/<op_id>.json|.lock   owned operations; a held lock is a live lease
trash/                   removals in progress
audit/                   one JSON result per retention run
```

Every run first cleans dead operations (nobody holds their lease: a crash),
removing only their staging and trash entries. Anything this tool does not
recognise — in `snapshots/` or at the root, such as legacy
`muster-backup-*` directories — is **uncatalogued**: never removed, always
reported, and it makes the backup status of a deletion `unknown`.

### pg-backup.sh / `muster-cloud-ops backup run`

```
deploy/pg-backup.sh --database-url <libpq-url> --repo-root <dir> \
                    --output-dir <managed-backup-root> [--plaintext]
```

(Or export `MUSTER_BACKUP_DATABASE_URL`/`MUSTER_DATABASE_URL`; the URL is
passed to `pg_dump` through `PG*` environment variables, never argv.) Needs
`MUSTER_CONTROL_DIR`, `MUSTER_CONTROL_WITNESS` and
`MUSTER_BACKUP_AGE_RECIPIENT` (a recipient PUBLIC key). Prints one JSON line
(`backup_id`, `path`, `snapshot_at`, `tenants`, `encryption`).

- Optional `MUSTER_BACKUP_AGE_RECIPIENTS`: more recipient public keys,
  comma-separated (a second identity that may open the backup, such as the
  restore drill's). A backup to several recipients records all of them in the
  manifest (`recipients_sha256`; an older `muster-cloud-ops` cannot read such a
  manifest). A malformed entry refuses the backup (78) before anything is read.
- **Encryption is the default and required**: no recipient → refused (65).
  `--plaintext` (`--plaintext-synthetic`) is for synthetic data in
  development (`MUSTER_ENV=development`) only, and refused in production (65).
- Normal retention: `MUSTER_BACKUP_RETENTION_DAYS` (default 30).
- Development only (refused in production, 78):
  `MUSTER_DEV_BACKUP_RETENTION_SECS`, `MUSTER_DEV_BACKUP_CRASH`
  (`after_snapshot|after_db|after_git|before_publish`),
  `MUSTER_DEV_BACKUP_PAUSE_FILE` (pauses after the snapshot until the file
  exists; announces `<file>.waiting`).
- Exit codes: 0 ok; 64 usage; 65 refused; 74 I/O; 75 busy (a purge is
  erasing; the timer retries); 78 configuration.

### Retention (`muster-cloud-ops retention run`)

- Reapplies the lifecycle journal first, then inventories the root.
- A snapshot's effective expiry is `min(normal expiry, the backup cap of
  every current deletion request of one of its tenants)` (cap = request +
  35 days). It is never extended; a request made after (or during) a capture
  shortens that snapshot too. Expiring a whole snapshot also shortens the
  history kept for its other tenants.
- Removes only catalogued, published, expired snapshots (rename into
  `trash/`, then delete), at most 100 per run; uncatalogued entries and live
  operations are never touched.
- Writes `audit/retention-<time>-<op>.json` and prints the same JSON
  (`removed`, `kept`, `uncatalogued`, `errors`); exit 74 when any removal
  failed.
- Projects the inventory into the database (`backup_snapshots`,
  `backup_inventory`), which `GET /v1/workspaces/{id}/lifecycle` reports as
  `deletion.backups`: `status` (`unknown`, `none`, `scheduled`, `due`,
  `overdue` — past the deadline by more than 24 h —, `error`, `purged`),
  `deadline` (the latest effective expiry of the snapshots still holding the
  workspace), `purged_at`, `cap_at`, `reason`, `last_error`.

### Timers (`deploy/systemd/`, `deploy/install-systemd.sh`)

`muster-cloud-backup.{service,timer}` (daily) and
`muster-cloud-retention.{service,timer}` (hourly): oneshot, `Persistent=true`,
hardened (`ProtectSystem=strict`), reading `/etc/muster-cloud/ops.env`.
Writable paths, none optional: both units get the backup root, the control
dir and the witness's directory (the authority may advance its witness); the
backup unit also gets the repository root (its coordination lock and the
purge of overdue workspaces). The installer stages `<repo>/locks`; create it
on the host, owned by the service user, before enabling.

```
deploy/install-systemd.sh --root <staging-dir> --env-file <ops.env> \
                          [--bin /usr/local/bin/muster-cloud-ops]
```

Validates the environment file (mode 0600; `KEY=VALUE` only; absolute,
pairwise disjoint `MUSTER_REPO_ROOT`, `MUSTER_CONTROL_DIR`,
`MUSTER_CONTROL_WITNESS`, `MUSTER_BACKUP_ROOT`; an `age1…` recipient; no
`MUSTER_DEV_*`, no plaintext), renders the units, checks them with
`systemd-analyze verify` when available, and stages them under
`<staging-dir>/etc/systemd/system/`. It never runs `systemctl` and never
copies the environment file; installing is the manual step it prints.

### pg-restore.sh

```
deploy/pg-restore.sh --backup <backup-dir> \
                     --admin-url <libpq-url-to-maintenance-db> \
                     --new-db <name> --new-repo-root <dir> \
                     --control-dir <dir> --control-witness <file> \
                     [--age-identity-file <path>]
```

(Or export `MUSTER_RESTORE_ADMIN_URL` instead of `--admin-url`, and
`MUSTER_CONTROL_DIR`/`MUSTER_CONTROL_WITNESS` instead of the control flags.
`muster-cloud-ops` is taken from `MUSTER_CLOUD_OPS` or `PATH`.)

- Requires the deployment's EXISTING lifecycle authority and never creates
  one. Before anything is created, the control store must open cleanly
  (missing, rolled-back, divergent or foreign journal or witness: exit 73).
  After `pg_restore`, `muster-cloud-ops restore-check` (read-only on the
  restored database) must accept the restored binding (same deployment and
  store, not ahead of the journal, not diverging from it); otherwise the
  targets are torn down (exit 73). Backups taken before the authority
  existed carry no binding and are refused.
- Reconciliation, right after `pg_restore` and before the integrity gate
  (`muster-cloud-ops restore-finalize`): the restored database joins the
  authority as a NEW **candidate** incarnation (journaled), the journal is
  replayed into it, and every workspace the journal tombstoned or purged
  after the backup was taken is erased from the new database and repo root,
  so a deleted tenant never comes back. If any of them cannot be erased
  (tombstoned or purged alike) the targets are torn down (73). A candidate never serves and never
  journals.

- Creates ONLY new resources: refuses (exit 66) if the database or repo root
  already exist. The active database and repo root are never modified.
- Restores the database with `pg_restore --no-owner`, untars the Git store,
  then runs the **pre-cutover check**: every head recorded in the restored
  `workspace_heads` must exist in the restored Git store AND be reachable
  from `refs/muster/uploads/*`; every restored repo must pass `git fsck`.
  Any failure tears down the targets created in that run.
- The authority (journal, lock, witness) is written as the owner of the control
  directory (`setpriv`, the service's uid), never chowned; a control directory
  owned by root, or a witness in a directory with another owner, is refused (73).
- Cutover is a separate MANUAL step: `MUSTER_DATABASE_URL=<new db>
  muster-cloud-ops activate --incarnation <id printed by the restore>` (the URL
  in the environment, not `--database-url`: a flag puts the password in the
  process list), which fences the source incarnation (its running service starts refusing every
  request), then start a NEW service instance pointed at the restored
  database and repo root (`MUSTER_DATABASE_URL`, `MUSTER_REPO_ROOT`). The
  script never restarts or reconfigures a service, and never overwrites the
  running one.
- Every snapshot is verified by `muster-cloud-ops backup verify` (strict
  format 2 manifest, every sha256), after the authority check and before any
  target exists; whatever it rejects is refused (67). Each artifact is then
  decrypted with `--age-identity-file` while streaming into `tar` and
  `pg_restore`: no plaintext copy on disk. Legacy format 1 directories
  (old `pg-backup.sh`) restore only with the explicit `--legacy-format1`.
- Exit codes: 64 usage; 66 target exists; 67 invalid backup; 68 pg_restore
  failed; 69 git fsck failed; 70 head-presence check failed; 71 age decrypt
  failed; 72 age missing for an encrypted backup; 73 lifecycle authority
  refused.

### backup-drill.sh — rehearse recovery end to end

```
deploy/backup-drill.sh
```

Runs the whole loop against synthetic data in one ephemeral PostgreSQL
cluster (`deploy/pg-test.sh`) with the real `muster-cloud` binary: a reusable
fixture OIDC provider plus a signed-request client
(`deploy/harness/drill.rs`, built by `deploy/harness/build.sh` through the
optional Cargo feature `backup-drill`) seed a
TWO tenants (T1, T2) through the PUBLIC HTTP API (OIDC login, workspace, RFC 8628 device
flow, machine link with a real link proof, signed genesis + incremental
pushes), after creating the deployment's lifecycle authority with an explicit
`muster-cloud-ops init`. Then: encrypted backup → T1 deleted and purged
through the API AFTER the backup → restore refused with a missing,
rolled-back and foreign authority (73, nothing left behind) → the
pre-deletion snapshot restored into new targets as a new candidate (T1
erased by the reconciliation) → a service on the unactivated candidate
refuses to start (78) → `activate` → a SECOND real service instance starts
on the restored data, and recovery is accredited through the public API
only:

- `/readyz` ready; the pre-backup browser session still authenticates
  (`GET /v1/me`); the pre-backup account token still lists the workspace;
- a signed full pull returns the pre-backup head, and the downloaded bundle
  clones to exactly that head (revision + bundle roundtrip);
- a signed incremental push is ACCEPTED by the restored instance — the
  restored trust anchor and Git objects verify the machine key server-side
  (the `GET /export` route is contract-only so far; this drill uses the
  implemented sync pull/push API);
- T1 stays gone on the restore: not listed, no repository, and
  `lifecycle-report` shows it purged with no residue;
- the ORIGINAL instance is fenced: `/readyz` 503 and a write
  (`POST /v1/workspaces`) refused with 503;
- restoring into the now-existing targets is refused (66).

The drill backs up through `pg-backup.sh` on the encrypted path a
deployment takes (production-mode ops, a synthetic `age-keygen` key pair) and
restores with its identity file; `age` is required. Override the service
binary location with `MUSTER_DRILL_CLOUD_BIN`. This workflow runs in CI as the
`backup-drill` job of `.github/workflows/cloud.yml`, which installs `age`
and `jq`.
