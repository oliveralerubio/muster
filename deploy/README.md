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
| `MUSTER_DATABASE_URL` | **yes** | libpq URL of the managed PostgreSQL. Secret — inject via secret store. |
| `MUSTER_PUBLIC_URL` | **yes** | Public base URL of the service (https in production). Used for OIDC redirects and cookies. |
| `MUSTER_OIDC_ISSUER` | **yes** | OIDC issuer URL of the identity provider (https, non-loopback in production). |
| `MUSTER_OIDC_CLIENT_ID` | **yes** | OIDC client id. |
| `MUSTER_OIDC_CLIENT_SECRET` | **yes** | OIDC client secret. Secret — inject via secret store. |
| `MUSTER_REPO_ROOT` | **yes** | Absolute path of the per-workspace bare Git object store (one `<workspace-id>.git` repo per workspace). There is no default: the service refuses to start without it. Back up this directory together with the database — see `pg-backup.sh` below. |

Billing/provider configuration (price IDs, webhook signing secret, fixture
IdP) is still being finalized in `cloud/src/config.rs` — placeholders only
here; **coordinate the exact variable names with the backend owner before
deploying** and do not hardcode credentials anywhere in this directory.

## Health endpoints

| Route | Kind | Checks |
| --- | --- | --- |
| `GET /healthz` | liveness | Nothing external: returns `{"status":"ok"}` if the process is up. |
| `GET /readyz` | readiness | Runs `SELECT 1` against PostgreSQL; returns `not_ready` (503) if the database is unreachable. |

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
are two stores that must be backed up together. Both tools here are plain
POSIX shell driving real `pg_dump`/`pg_restore`, `tar` and `git` — no SQL is
invented by them.

### Consistent cut

`pg-backup.sh` dumps the database FIRST, then tars the repo root. That order
is what makes the pair consistent while the service keeps running: every
accepted head the dump captured was committed to PostgreSQL before the dump
snapshot, and the service's durability order (`cloud/src/repo_store.rs`)
writes Git objects plus the immutable `refs/muster/uploads/<id>` ref BEFORE
the PostgreSQL transaction commits — so all objects any captured head needs
are already on disk when the tar runs. Upload refs are never deleted and
`gc.auto=0` in service repos, so nothing captured can be GC'd in between.
Concurrent writers during the tar can only add objects (uploads are
immutable); transient git files (`*.lock`, `*.tmp`, `tmp_obj_*`) are excluded.

### pg-backup.sh

```
deploy/pg-backup.sh --database-url <libpq-url> --repo-root <dir> \
                    --output-dir <dir> [--plaintext]
```

(Or export `MUSTER_BACKUP_DATABASE_URL` so a password in the URL never
appears in argv; the script moves the connection string into the standard
`PG*` environment variables before invoking `pg_dump`.)

- Creates `<output-dir>/muster-backup-<UTC>/` (0700) containing `db.dump`
  (pg_dump custom format), `git.tar`, and `manifest.json` written LAST: a
  backup directory without a manifest is incomplete and must not be used.
- Never touches the source database or repo root; never deletes or overwrites
  anything — each run makes a NEW timestamped directory.
- **Encryption**: with the `age` CLI installed and
  `MUSTER_BACKUP_AGE_RECIPIENT` set (a recipient PUBLIC key), both artifacts
  are encrypted and the plaintext intermediates removed. Without `age`, the
  script refuses to run unless `--plaintext` is passed explicitly — workspace
  content is not end-to-end encrypted by design (ADR 0006), so plaintext
  backups are for dev/synthetic data only.
- **Retention/purge is NOT implemented.** The "export 30 days after
  cancellation / backups expire within 35 days" figures from the product
  research are proposed, configurable policy — this tool never deletes old
  backup directories. Do not promise purge behavior that does not exist yet.

### pg-restore.sh

```
deploy/pg-restore.sh --backup <backup-dir> \
                     --admin-url <libpq-url-to-maintenance-db> \
                     --new-db <name> --new-repo-root <dir> \
                     [--age-identity-file <path>]
```

(Or export `MUSTER_RESTORE_ADMIN_URL` instead of `--admin-url`.)

- Creates ONLY new resources: refuses (exit 66) if the database or repo root
  already exist. The active database and repo root are never modified.
- Restores the database with `pg_restore --no-owner`, untars the Git store,
  then runs the **pre-cutover check**: every head recorded in the restored
  `workspace_heads` must exist in the restored Git store AND be reachable
  from `refs/muster/uploads/*`; every restored repo must pass `git fsck`.
  Any failure tears down the targets created in that run.
- Cutover is a separate MANUAL step: start a NEW service instance pointed at
  the restored database and repo root (`MUSTER_DATABASE_URL`,
  `MUSTER_REPO_ROOT`). The script never restarts or reconfigures a service,
  and never overwrites the running one.
- Exit codes: 64 usage; 66 target exists; 67 invalid backup; 68 pg_restore
  failed; 69 git fsck failed; 70 head-presence check failed; 71 age decrypt
  failed; 72 age missing for an encrypted backup.

### backup-drill.sh — rehearse recovery end to end

```
deploy/backup-drill.sh
```

Runs the whole loop against synthetic data in one ephemeral PostgreSQL
cluster (`deploy/pg-test.sh`) with the real `muster-cloud` binary: a reusable
fixture OIDC provider plus a signed-request client
(`deploy/harness/drill.rs`, built by `deploy/harness/build.sh` through the
optional Cargo feature `backup-drill`) seed a
workspace through the PUBLIC HTTP API (OIDC login, workspace, RFC 8628 device
flow, machine link with a real link proof, signed genesis + incremental
pushes). Then: backup → restore into new targets → a SECOND real service
instance starts on the restored data, and recovery is accredited through the
public API only:

- `/readyz` ready; the pre-backup browser session still authenticates
  (`GET /v1/me`); the pre-backup account token still lists the workspace;
- a signed full pull returns the pre-backup head, and the downloaded bundle
  clones to exactly that head (revision + bundle roundtrip);
- a signed incremental push is ACCEPTED by the restored instance — the
  restored trust anchor and Git objects verify the machine key server-side
  (the `GET /export` route is contract-only so far; this drill uses the
  implemented sync pull/push API);
- the ORIGINAL instance still serves the untouched original head;
- restoring into the now-existing targets is refused (66).

The drill runs `pg-backup.sh` with `--plaintext` because the data is
synthetic; wherever `age` + `MUSTER_BACKUP_AGE_RECIPIENT` are available, run
it without the flag through `pg-backup.sh` to exercise encryption. Override
the service binary location with `MUSTER_DRILL_CLOUD_BIN`. This workflow runs
in CI as the `backup-drill` job of `.github/workflows/cloud.yml`.

A scheduler (cron/systemd timer/container) that runs `pg-backup.sh` on a
schedule is a policy decision for the operator: nothing here installs one,
and retention/purge remains unimplemented (see above).
