# Deploying muster Cloud with Kamal

This is the guide for the production deployment on the `personal` VPS
(Ubuntu 24.04, Tailscale address `100.92.84.56`). The generic image and
configuration reference stays in [`README.md`](README.md); this file is about
how the pieces are wired and what to do once, by hand.

## How it fits together

```
Internet --HTTPS--> Tailscale Funnel (port 10000, certificate by Tailscale)
                      --> 127.0.0.1:18110  kamal-proxy  (loopback only)
                            --> muster-cloud container, port 8080
                                  --> muster-cloud-db container (PostgreSQL 16)

host: systemd timers -> muster-cloud-ops (backup every 6 h, retention hourly)
                     -> rclone sync to Google Drive (hourly)
```

- **Image**: built by GitHub Actions on every green push to `main`, tagged with
  the commit SHA (`ghcr.io/oliveralerubio/muster-cloud:<sha>`).
- **Deploy**: a second job runs on the `deploy-personal` runner, which lives on
  the VPS itself as user `gha`. It runs Kamal (`bin/kamal`, a pinned container,
  no Ruby on the host) against `100.92.84.56` over SSH.
- **Zero downtime**: the new instance must pass `/readyz` (authority journal,
  witness and database) before kamal-proxy moves traffic. The old instance then
  keeps answering requests that are already running (long pushes) for up to 120
  seconds. Two instances of the same active incarnation overlapping is safe by
  design.
- **Config**: `config/deploy.yml`. Secrets: `.kamal/secrets` only calls
  `deploy/kamal/secret KEY`, which reads one value from
  `/etc/muster-cloud/kamal.env` on the host. No secret is in the repository or
  in GitHub; the workflow only uses its own `GITHUB_TOKEN`.
- **Paths are identical inside the container and on the host** (repos, control
  journal, witness). The host-side `muster-cloud-ops` and the service share
  them, so never change a path in one place only.

## What runs by itself after the bootstrap

| Piece | What it does |
| --- | --- |
| `Deploy muster Cloud` workflow | Builds the image, deploys it, installs the matching `muster-cloud-ops` on the host, and checks `/readyz` locally and through Funnel. Skips a commit older than what already runs. |
| `muster-cloud-install-ops` (root, via one sudoers rule) | Copies `muster-cloud-ops` out of the deployed image into `/usr/local/lib/muster-cloud/<sha>/`, checks it runs, switches the `/usr/local/bin/muster-cloud-ops` link, keeps the last 3 versions. |
| `muster-cloud-backup.timer` | Every 6 hours (at about 00:41, 06:41, 12:41, 18:41, up to 15 minutes late): one encrypted, snapshot-bound backup. |
| `muster-cloud-retention.timer` | Hourly: removes expired snapshots and enforces the deletion caps. |
| `muster-cloud-offsite.timer` | Hourly: `rclone sync` of the published `snapshots/` directory to Google Drive. Because it is a sync, a snapshot that retention deletes is deleted offsite too, and Drive's trash is bypassed so a deleted tenant's data really expires. |
| Docker | Restarts the containers after a reboot or crash (`unless-stopped`). |

`bin/kamal`, the sudoers rule and the offsite job are the only places that are
not self-updating: the root-owned helper under `/usr/local/sbin` is copied by
hand on purpose (so a change to the repository can never silently gain root).
The workflow prints a warning when the copy is out of date; fix it by running
`sudo deploy/kamal/host-setup.sh` again.

## One-time bootstrap

Do these steps in order, on the VPS unless stated otherwise.

### 0. What must already be true

- Users `muster-cloud` (10001) and `muster-pg` (10002), the directories under
  `/srv/muster-cloud`, and the witness partition at `/var/lib/muster-witness`
  exist (the host-setup script in step 3 checks and creates them).
- Tailscale is up, **HTTPS certificates are enabled** for the tailnet and
  **Funnel is allowed** for this node (tailnet policy: node attribute `funnel`).
  Funnel only accepts ports 443, 8443 and 10000; this deployment uses 10000 and
  leaves the existing tailnet-only entries on 443, 3443 and 8443 alone.
- The `deploy-personal` runner is registered for this repository and runs as
  `gha`, a member of the `docker` group.
- `gha` can SSH to the host's own Tailscale address (step 1).
- Docker is installed; `rclone` has a working remote called `gdrive:` for root;
  `age`, `git` and a PostgreSQL 16 `pg_dump` are installed.

### 1. Let `gha` SSH to itself

Kamal connects to `100.92.84.56` as `gha`, even though that is this machine.

```
sudo -iu gha
ssh-keygen -t ed25519 -N '' -f ~/.ssh/id_ed25519      # only if there is no key yet
cat ~/.ssh/id_ed25519.pub >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
ssh gha@100.92.84.56 true                              # answer yes once: fills known_hosts
```

`sshd` must listen on `100.92.84.56` (or on all addresses). `host-setup.sh`
reports whether all of this is in place.

### 2. Write the three files in `/etc/muster-cloud`

Create them as root. The directory is `0711 root` (`gha` can walk in; each file
carries its own protection). Values are yours; the names
below are exactly what the code reads. Never paste real values into a chat, an
issue or the repository.

**`kamal.env`** (`gha:gha`, mode `0600`): one `KEY=VALUE` per line, no quotes
needed, no multi-line values. Read only by `deploy/kamal/secret`.

| Key | What it is |
| --- | --- |
| `POSTGRES_PASSWORD` | New random password for the database user `muster` (for example `openssl rand -hex 24`). Used once, when the database is first created. |
| `MUSTER_DATABASE_URL` | `postgres://muster:<POSTGRES_PASSWORD>@muster-cloud-db:5432/muster?sslmode=disable` (the database is reachable only on the private Docker network; percent-encode special characters in the password). |
| `MUSTER_OIDC_ISSUER` | Issuer URL of the identity provider (https). |
| `MUSTER_OIDC_CLIENT_ID` | Client id registered there. |
| `MUSTER_OIDC_CLIENT_SECRET` | Its secret. |
| `MUSTER_PADDLE_API_KEY` | Paddle API key (`pdl_sdbx_apikey_...` for sandbox, `pdl_live_apikey_...` for live). |
| `MUSTER_PADDLE_WEBHOOK_SECRET` | Paddle notification signing secret. |

**`ops.env`** (`root:root`, mode `0600`): read by the three timers' units and
by the one-off `init` below. `host-setup.sh` refuses to run without it.

| Key | Value |
| --- | --- |
| `MUSTER_DATABASE_URL` | `postgres://muster:<POSTGRES_PASSWORD>@localhost/muster?host=/srv/muster-cloud/pgsocket` (unix socket of the database container; same password). |
| `MUSTER_REPO_ROOT` | `/srv/muster-cloud/repos` |
| `MUSTER_CONTROL_DIR` | `/srv/muster-cloud/control` |
| `MUSTER_CONTROL_WITNESS` | `/var/lib/muster-witness/w/witness.json` |
| `MUSTER_BACKUP_ROOT` | `/srv/muster-cloud/backups` |
| `MUSTER_BACKUP_AGE_RECIPIENT` | The age **public** key (`age1...`) backups are encrypted to. |

Generate the age key pair on your own computer (`age-keygen -o muster-backup.key`),
put only the public key here, and keep the private key in your password manager.
It never touches the VPS; without it no backup can be restored.

**`deploy.env`** (`root:root`, mode `0644`, no secrets): read by `bin/kamal`.

| Key | Value |
| --- | --- |
| `MUSTER_PADDLE_PRICE_PERSONAL` | Paddle price id (`pri_...`) of the personal plan. |
| `MUSTER_PADDLE_PRICE_TEAM_SEAT` | Paddle price id of the team seat. |
| `MUSTER_PADDLE_ENVIRONMENT` | Optional: `sandbox` (default) or `live`. |
| `MUSTER_CLOUD_PUBLIC_URL` | Optional: defaults to `https://personal.tailbb30e7.ts.net:10000`. |

### 3. Prepare the host

From a checkout of this repository, as root:

```
sudo deploy/kamal/host-setup.sh --check     # changes nothing; lists what is missing
sudo deploy/kamal/host-setup.sh             # does it
```

It creates what is missing (accounts, directories with exact modes, the
`kamal` Docker network with the fixed subnet `172.31.250.0/24`), installs the
root helper and its one-line sudoers rule, installs and enables the backup,
retention and offsite timers, and sets up Funnel
(`tailscale funnel --bg --https=10000 http://127.0.0.1:18110`). It refuses to
touch a `kamal` network with another subnet, and refuses to create anything
under `/var/lib/muster-witness` unless that is its own mounted filesystem. It
never touches other containers, networks, services, UFW, `/etc/docker` or the
other Tailscale serve entries. Anything left for you appears as `TODO` or
`FAIL`; fix it and run it again until it is quiet (it is safe to repeat).

### 4. Start the database

As `gha`, from the checkout. A GitHub token that can read packages is needed
by Kamal's login step (a classic token with `read:packages`):

```
sudo -iu gha
cd <checkout>
export KAMAL_REGISTRY_PASSWORD=<token with read:packages>
bin/kamal accessory boot db
```

PostgreSQL 16 starts as user 10002 with scram-sha-256 everywhere, no published
port, data in `/srv/muster-cloud/pg/data`.

### 5. Get the first image onto the host

Run the workflow once: GitHub, Actions, **Deploy muster Cloud**, Run workflow
(branch `main`). The `image` job builds and pushes the image. The `deploy` job
will then fail at the health check, because the service refuses to start until
the authority exists (step 6). That is expected only this first time.

### 6. Create the lifecycle authority, exactly once

On pristine targets only (empty database, empty repository root). `init`
refuses anything else, so a mistaken repeat is harmless, but never run it
against a restored or unknown store. The commit SHA is the one the workflow
built.

```
sudo /usr/local/sbin/muster-cloud-install-ops ghcr.io/oliveralerubio/muster-cloud:<sha>
sudo systemd-run --pipe --wait --collect -p User=muster-cloud -p Group=muster-cloud \
  -p UMask=0077 -p EnvironmentFile=/etc/muster-cloud/ops.env \
  /usr/local/bin/muster-cloud-ops init
```

(The first command needs the image on the host: the failed run in step 5 pulled
it. If it did not, `docker login ghcr.io` as `gha` and `docker pull` it.)

### 7. First real deploy

Run the workflow again (Run workflow). It deploys, installs the matching
`muster-cloud-ops`, and checks `/readyz`. Afterwards:

```
docker ps                                   # kamal-proxy, muster-cloud-web-<sha>, muster-cloud-db
systemctl list-timers 'muster-cloud-*'     # three timers
curl -fsS https://personal.tailbb30e7.ts.net:10000/readyz
```

Then register, at the identity provider, the redirect URI
`<public url>/auth/callback`, and at Paddle the notification URL
`<public url>/v1/billing/webhook`.

### 8. Prove backups and the offsite copy once

```
sudo systemctl start muster-cloud-backup.service
sudo journalctl -u muster-cloud-backup.service -n 20 --no-pager
sudo systemctl start muster-cloud-offsite.service
sudo rclone lsf gdrive:muster-cloud-backups/snapshots
```

From then on everything is automatic: a merge to `main` with green Cloud CI
deploys itself.

## Day to day

- Logs: `bin/kamal app logs`, `bin/kamal accessory logs db`,
  `journalctl -u muster-cloud-backup -u muster-cloud-retention -u muster-cloud-offsite`.
- Redeploy the tip of `main` (for example after editing a file in
  `/etc/muster-cloud`): Run workflow.
- Changing a secret: edit `kamal.env`, then redeploy. `POSTGRES_PASSWORD` only
  seeds the first database creation; to rotate the live password use
  `ALTER ROLE muster PASSWORD ...` in the database, then update `kamal.env` and
  `ops.env`.
- PostgreSQL minor update: change the tag and digest in `config/deploy.yml`,
  then `bin/kamal accessory reboot db` (a short database outage; the service
  recovers on its own).
- Docker keeps the last 3 app containers and images (`retain_containers`).

## Moving to a real domain later

Today HTTPS comes from Tailscale Funnel. To use your own domain instead:

1. Point an A record at the VPS public address and make sure ports 80 and 443
   are reachable. Note that Docker-published ports bypass UFW.
2. In `config/deploy.yml`: under `proxy`, set `ssl: true` and
   `host: <your domain>`, set `forward_headers: false` (kamal-proxy then sets
   the real client address itself), and under `proxy.run` use `http_port: 80`,
   `https_port: 443` and remove `bind_ips`.
3. Set `MUSTER_CLOUD_PUBLIC_URL=https://<your domain>` in `deploy.env`.
4. The proxy container only reads its run options when it is created: run
   `bin/kamal proxy reboot` once.
5. Remove the funnel (`sudo tailscale funnel --https=10000 off`), update the
   OIDC redirect URI and the Paddle webhook URL, and drop the loopback port
   checks from `host-setup.sh` and the local check from the workflow.

Session cookies are bound to the exact origin, so everyone signs in again
after the URL changes.

## Rolling back

```
bin/kamal app containers                 # versions still on the host
bin/kamal rollback <sha>                 # boots that version, drains the current one
sudo /usr/local/sbin/muster-cloud-install-ops ghcr.io/oliveralerubio/muster-cloud:<sha>
```

The last command makes the timers use the matching `muster-cloud-ops`.

**Migrations run forward at startup and are never undone.** A rollback runs
older code against a newer database. It is only safe for releases that did not
change the schema, or whose schema changes are additive (expand/contract: new
tables or nullable columns first; remove or rename only in a later release,
once no deployed code uses the old shape). In addition, sqlx's default is to
refuse to start when the database has applied a migration the running binary
does not contain (not tried on this stack, but it is what the service's
`migrate!().run()` call does by default). If so, rolling back past a migration
stops the service at startup (exit 78, `migration error`) and `/readyz` never
turns green; the instance that was serving keeps serving, because kamal-proxy
never switches to an instance that is not ready. When a release changed the
schema, the way back is to fix forward, or to restore a backup (below).

## Restoring from Google Drive

You need the age **private** key (from your password manager), `rclone` with
the `gdrive:` remote, and the repository checkout. Snapshots are directories
named by their backup id, each holding `manifest.json`, `db.dump.age` and
`git.tar.age`.

```
sudo rclone lsf gdrive:muster-cloud-backups/snapshots                  # choose a snapshot
sudo install -d -m 0700 -o 10001 -g 10001 /srv/muster-cloud/restore
sudo rclone copy gdrive:muster-cloud-backups/snapshots/<backup_id> /srv/muster-cloud/restore/<backup_id> --checksum
sudo chown -R 10001:10001 /srv/muster-cloud/restore
```

(Keep downloads outside `/srv/muster-cloud/backups`: anything unknown in the
backup root is reported as uncatalogued.) Local snapshots are in
`/srv/muster-cloud/backups/snapshots`, no download needed. Then restore **into
new targets** with `deploy/pg-restore.sh` as root (see `README.md`: it checks
the structure and every hash first, never overwrites the running database or
repository root, needs the existing authority, joins the restore as a new
candidate incarnation and erases tenants deleted after the backup). Put the age
private key in a temporary file outside the repository and delete it
afterwards. Use the database socket for the admin URL
(`postgres://muster:<password>@localhost/postgres?host=/srv/muster-cloud/pgsocket`),
a new database name and a new repository root such as
`/srv/muster-cloud/repos-restored` (then `chown -R 10001:10001` it).

Cutover is deliberate and manual:

1. `muster-cloud-ops activate --incarnation <id printed by the restore> --database-url <new db>`
   (this fences the running service: it starts refusing requests).
2. `bin/kamal app stop`, move `/srv/muster-cloud/repos` aside and put the
   restored repository root in its place (owner `10001:10001`, mode `0700`).
3. Update the database name in `MUSTER_DATABASE_URL` in both `kamal.env` and
   `ops.env`, then redeploy (Run workflow).

On a brand new host after total loss, do the bootstrap first (steps 0 to 6), then
restore into new targets and cut over as above. The control journal and the
witness are deliberately **not** part of the backups. Rehearse this on a copy
before you ever need it (`deploy/backup-drill.sh` shows the full loop on
synthetic data).

What leaves the machine: artifacts are age-encrypted. Each snapshot's
`manifest.json` is **not**: it lists workspace ids, their current Git head
hashes, sizes and timestamps.

## Risks you accept with this setup

- **Shared machine.** The VPS also runs other projects and CI. Kamal, the
  runner and the helper rely on `gha` being in the `docker` group, which is
  root-equivalent. Any job that runs on the `deploy-personal` pool runs as `gha`
  and can read `kamal.env`; keep that pool limited to repositories you trust.
  The app is capped at 1 GB and 2 CPUs and the database at 1 GB, both with a
  protected OOM score, but a neighbour can still starve the disk or network.
- **The witness is on the same physical disk.** It is its own filesystem (so a
  damaged or rolled-back `/srv` is detected), but a disk failure, or a snapshot
  restore of the whole VPS, takes the database, journal and witness together
  and the rollback cannot be detected.
- **Single server.** Deploys have no downtime, but a host failure means a
  restore from Drive: up to 6 hours of data (the backup interval) can be lost,
  and the service is down until you rebuild.
- **Public entry point.** Funnel exposes port 10000 of the node's Tailscale
  name. Tailscale relays the connection; TLS is terminated on this host. If the
  tailnet is renamed, the URL, cookies, OIDC and Paddle settings change.
- **No TLS inside the Docker network.** The service talks to PostgreSQL
  unencrypted over the private `kamal` network (`sslmode=disable`). The
  database publishes no port, but anything else attached to that network could
  reach it.
- **Offsite sync follows local deletion.** If someone deletes local snapshots
  by mistake, the next hourly sync deletes the offsite copies as well (Drive
  trash is not used). Deliberate: it is what makes tenant deletion real.

## Compatibility note: `muster-cloud-ops` on the host

The binary is built inside the image (Debian bookworm, glibc 2.36) and copied
to the Ubuntu 24.04 host (glibc 2.39). That works because glibc is backward
compatible and the program links only `libc`, `libm` and `libgcc_s` (TLS is
rustls, not OpenSSL). `muster-cloud-install-ops` re-checks with `ldd` on every
install and refuses a binary that would not run. It was verified by running the
image's binary in an `ubuntu:24.04` container.
