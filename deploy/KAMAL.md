# Deploying muster Cloud with Kamal

This is the guide for the production deployment on the `personal` VPS
(Ubuntu 24.04, Tailscale address `100.92.84.56`). The generic image and
configuration reference stays in [`README.md`](README.md); this file is about
how the pieces are wired and what to do once, by hand.

## How it fits together

```
GitHub (hosted runners)                        the VPS (pulls; nothing pushes to it)
  Cloud CI green on main                         muster-cloud-autodeploy.timer (every 5 min)
  -> workflow "Publish muster Cloud image"         asks the public GitHub API: HEAD of main,
     pushes ghcr.io/oliveralerubio/                its Cloud CI run, the image
     muster-cloud:<sha> (public package)  ----->   pulls the image anonymously, runs Kamal
                                                   locally (as gha), verifies /readyz
  workflow "Cloud uptime" (every 30 min)
  -> curl https://<node>.ts.net:10000/readyz

Internet --HTTPS--> Tailscale Funnel (port 10000, certificate by Tailscale)
                      --> 127.0.0.1:18110  kamal-proxy  (loopback only)
                            --> muster-cloud container, port 8080
                                  --> muster-cloud-db container (PostgreSQL 16)

host: systemd timers -> muster-cloud-ops (backup every 6 h, retention hourly)
                     -> muster-cloud-offsite (copy to Google Drive, hourly)
                     -> muster-cloud-authority-offsite (encrypted authority copy)
                     -> muster-cloud-backup-check (every 30 min) -> Discord alerts
```

- **Nothing outside the host can start a deploy.** No GitHub runner has access
  to the VPS, no workflow holds a credential for it, and the host holds no
  registry credential. GitHub only builds and publishes an image. The host
  decides, by itself, to deploy a commit when all of these hold: it is the
  HEAD of `main`; the `Cloud CI` run for that exact commit finished with
  `success` on a push; the image can be pulled anonymously and is labelled with
  that commit; and the commit is not older than what already runs.
- **Image**: built by GitHub Actions on every green push to `main`, tagged with
  the commit SHA (`ghcr.io/oliveralerubio/muster-cloud:<sha>`).
- **Kamal** (`bin/kamal`, a pinned container, no Ruby on the host) runs on the
  host, as user `gha`, against `100.92.84.56` over SSH. `config/deploy.yml`
  names the registry `localhost:5000`, which makes Kamal skip every registry
  login and every pull (verified in the Kamal 2.12.0 source: `Registry#login`
  returns early for a `localhost` registry). The autodeploy script pulls the
  image itself, anonymously, and retags it as
  `localhost:5000/oliveralerubio/muster-cloud:<sha>`; Kamal only starts it
  (`kamal app boot`) and, if needed, rolls back. Never run `kamal deploy` by
  hand here: with a local registry it tries to push through an SSH tunnel to
  itself.
- **Zero downtime**: the new instance must pass `/readyz` (authority journal,
  witness and database) before kamal-proxy moves traffic. The old instance then
  keeps answering requests that are already running (long pushes) for up to 120
  seconds. Two instances of the same active incarnation overlapping is safe by
  design.
- **Config**: `config/deploy.yml`. Secrets: `.kamal/secrets` only calls
  `deploy/kamal/secret KEY`, which reads one value from
  `/etc/muster-cloud/kamal.env` on the host. No secret is in the repository or
  in GitHub; the image workflow only uses its own `GITHUB_TOKEN`, to push.
- **Paths are identical inside the container and on the host** (repos, control
  journal, witness). The host-side `muster-cloud-ops` and the service share
  them, so never change a path in one place only.

## What runs by itself after the bootstrap

| Piece | What it does |
| --- | --- |
| `Publish muster Cloud image` workflow | GitHub-hosted. Builds the image and pushes it to GHCR. Nothing else. |
| `muster-cloud-autodeploy.timer` | Every 5 minutes, as root. Applies the decision above, then: updates its own checkout (`/var/lib/muster-cloud/source`), runs that checkout's `host-setup.sh` in apply mode (so scripts, units, drop-ins and permissions follow the release), pulls the image, boots it with Kamal, installs the matching `muster-cloud-ops` (`muster-cloud-install-ops`), checks `/readyz` locally and through Funnel. If a step after the switch fails it rolls back to the previous version and alerts. A commit whose deploy failed is retried 3 times, 30 minutes apart, then it waits for a newer commit. |
| `muster-cloud-guard.timer` | Every 15 seconds, as root: `muster-cloud-autodeploy --guard`, the liveness guard alone (see Day to day). It never contacts GitHub and never deploys. |
| `muster-cloud-backup.timer` | Every 6 hours (about 00:41, 06:41, 12:41, 18:41, up to 15 minutes late): one encrypted, snapshot-bound backup. |
| `muster-cloud-retention.timer` | Hourly: removes expired snapshots and enforces the deletion caps. Writes an audit record of what it removed. |
| `muster-cloud-offsite.timer` | Hourly: copies new snapshots to Google Drive (never `sync`), and deletes from Drive only what retention recorded as removed (see below). |
| `muster-cloud-authority-offsite.timer` | Hourly and right after every successful backup: an age-encrypted copy of the lifecycle authority (journal and witness) to Google Drive. |
| `muster-cloud-backup-check.timer` | Every 30 minutes: alerts when backups quietly stop (see Alerts). |
| Docker | Restarts the containers after a reboot or crash (`unless-stopped`). |

The root scripts under `/usr/local/sbin` are copied by `host-setup.sh` from the
checkout that autodeploy has just verified (a commit of `main` with green CI),
so they follow the release without anyone running anything. There is no sudoers
rule: user `gha` has no privilege here.

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
- The account `gha` exists and is a member of the `docker` group. It is used
  only by Kamal, started by autodeploy. No GitHub runner is needed for it.
- `gha` can SSH to the host's own Tailscale address (step 1).
- Docker is installed; `rclone` has a working remote called `gdrive:` for root;
  `age`, `git`, `curl`, `python3` and the PostgreSQL **16** client (`pg_dump`, `pg_restore`; a
  newer client writes dumps and restore scripts the 16 server rejects) are installed;
  the Hermes CLI (`hermes`) works for root (alerts).

### 1. Make the GHCR package public (once)

The host has no registry credential and pulls anonymously, so the package must
be public. After the first image has been published (step 5): GitHub, your
profile, Packages, `muster-cloud`, Package settings, Danger zone, **Change
visibility**, Public. The image holds only the compiled service, no secrets.
`host-setup.sh` checks this and reports a `TODO` until it holds.

### 2. Let `gha` SSH to itself

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

### 3. Write the three files in `/etc/muster-cloud`

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

There is no registry token anywhere: neither here nor in GitHub.

Billing can start disabled: write `MUSTER_PADDLE_API_KEY=` and
`MUSTER_PADDLE_WEBHOOK_SECRET=` with empty values in `kamal.env` (the keys must
be present) and leave the two price ids empty or absent in `deploy.env`;
`host-setup.sh` then reports "billing is off" as a note. To enable it later,
fill all four values (`host-setup.sh` asks for the rest once the API key is
set) and boot the running commit again:
`sudo touch /var/lib/muster-cloud/autodeploy/redeploy` plus
`sudo systemctl start muster-cloud-autodeploy.service` (or wait for the timer).

**`ops.env`** (`root:root`, mode `0600`): read by the timers' units and
by the one-off `init` below. `host-setup.sh` refuses to run without it.

| Key | Value |
| --- | --- |
| `MUSTER_DATABASE_URL` | `postgres://muster:<POSTGRES_PASSWORD>@localhost/muster?host=/srv/muster-cloud/pgsocket` (unix socket of the database container; same password). |
| `MUSTER_REPO_ROOT` | `/srv/muster-cloud/repos` |
| `MUSTER_CONTROL_DIR` | `/srv/muster-cloud/control` |
| `MUSTER_CONTROL_WITNESS` | `/var/lib/muster-witness/w/witness.json` |
| `MUSTER_BACKUP_ROOT` | `/srv/muster-cloud/backups` |
| `MUSTER_BACKUP_AGE_RECIPIENT` | The age **public** key (`age1...`) backups and authority copies are encrypted to. |

Generate the age key pair on your own computer (`age-keygen -o muster-backup.key`),
put only the public key here, and keep the private key in your password manager.
It never touches the VPS (except for the minutes of a restore drill, below);
without it no backup can be restored.

**`deploy.env`** (`root:root`, mode `0644`, no secrets): read by `bin/kamal` and
`muster-cloud-alert`. Only these keys are accepted.

| Key | Value |
| --- | --- |
| `MUSTER_PADDLE_PRICE_PERSONAL` | Paddle price id (`pri_...`) of the personal plan. |
| `MUSTER_PADDLE_PRICE_TEAM_SEAT` | Paddle price id of the team seat. |
| `MUSTER_OIDC_EXTRA_AUDIENCES` | Optional, not secret: extra audiences the ID token may carry. With Zitadel set it to the project id (Zitadel puts it in `aud` next to the client id); without it login fails with "invalid ID token". Comma-separated; empty keeps only the client id. |
| `MUSTER_PADDLE_ENVIRONMENT` | Optional: `sandbox` (default) or `live`. |
| `MUSTER_CLOUD_PUBLIC_URL` | Optional: defaults to `https://personal.tailbb30e7.ts.net:10000`. |
| `ALERT_TARGET` | Optional: where alerts go, default `discord:#contabo` (`hermes send -t`). |

### 4. Prepare the host

From a checkout of this repository, as root:

```
sudo deploy/kamal/host-setup.sh --check     # changes nothing; lists what is missing
sudo deploy/kamal/host-setup.sh             # does it
sudo touch /var/lib/muster-cloud/autodeploy/guard-only   # until step 7: no deploy before init (see Day to day)
```

It creates what is missing (accounts, directories with exact modes, the
`kamal` Docker network with the fixed subnet `172.31.250.0/24`), installs the
host scripts and all units, deletes the old `gha` sudoers rule if one exists,
enables the timers, and sets up Funnel
(`tailscale funnel --bg --https=10000 http://127.0.0.1:18110`). It refuses to
touch a `kamal` network with another subnet, and refuses to create anything
under `/var/lib/muster-witness` unless that is its own mounted filesystem. It
never touches other containers, networks, services, UFW, `/etc/docker` or the
other Tailscale serve entries. Anything left for you appears as `TODO` or
`FAIL`; fix it and run it again until it is quiet (it is safe to repeat).
After this first run autodeploy runs it by itself on every release.

### 5. Start the database

As `gha`, from the checkout:

```
sudo -iu gha
cd <checkout>
bin/kamal accessory boot db
```

PostgreSQL 16 starts as user 10002 with scram-sha-256 everywhere, no published
port, data in `/srv/muster-cloud/pg/data`. No registry login happens (the
registry is `localhost:5000`).

### 6. Publish the first image and create the lifecycle authority, once

Merge to `main` (or run the workflow **Publish muster Cloud image** from
`main`). Then make the package public (step 1) and run `host-setup.sh --check`
again: the anonymous pull check must be quiet.

Create the authority on pristine targets only (empty database, empty repository
root). `init` refuses anything else, so a mistaken repeat is harmless, but never
run it against a restored or unknown store. `<sha>` is the commit that was
published.

```
docker pull ghcr.io/oliveralerubio/muster-cloud:<sha>
sudo /usr/local/sbin/muster-cloud-install-ops ghcr.io/oliveralerubio/muster-cloud:<sha>
sudo systemd-run --pipe --wait --collect -p User=muster-cloud -p Group=muster-cloud \
  -p UMask=0077 -p EnvironmentFile=/etc/muster-cloud/ops.env \
  /usr/local/bin/muster-cloud-ops init
```

### 7. First real deploy

```
sudo rm -f /var/lib/muster-cloud/autodeploy/guard-only
sudo systemctl start muster-cloud-autodeploy.timer
sudo systemctl start muster-cloud-autodeploy.service     # do not wait 5 minutes
journalctl -u muster-cloud-autodeploy -n 40 --no-pager
```

Afterwards:

```
docker ps                                   # kamal-proxy, muster-cloud-web-<sha>, muster-cloud-db
systemctl list-timers 'muster-cloud-*'     # six timers
curl -fsS https://personal.tailbb30e7.ts.net:10000/readyz
```

Then register, at the identity provider, the redirect URI
`<public url>/auth/callback`, and at Paddle the notification URL
`<public url>/v1/billing/webhook`.

### 8. Prove the offsite pieces once

```
sudo systemctl start muster-cloud-backup.service          # also starts the authority copy
sudo journalctl -u muster-cloud-backup.service -n 20 --no-pager
sudo systemctl start muster-cloud-offsite.service
sudo rclone lsf gdrive:muster-cloud-backups/snapshots
sudo rclone lsf gdrive:muster-cloud-backups/authority
sudo muster-cloud-alert notify "muster Cloud: alert test"   # arrives in Discord
```

From then on everything is automatic: a merge to `main` with green Cloud CI
deploys itself within minutes.

## Alerts

One channel: `hermes send -t <target> "<text>"` as root (Discord, default
`#contabo`; `ALERT_TARGET` in `deploy.env` changes it). One short line each.

| Alert | Fires when |
| --- | --- |
| deploy | A deploy succeeded (one line), failed, was rolled back, or was refused (older commit, host-setup failed, image problem). |
| `unit:<name>` | Any `muster-cloud-*` service failed (`OnFailure=` on every one of them, plus the 30-minute check for units that stay failed). |
| `local-snapshot-stale` | The newest snapshot on the host is older than 7 hours (backups run every 6). |
| `gdrive-snapshot-stale` / `gdrive-unreachable` | The newest complete snapshot on Google Drive is older than 8 hours or missing, or Drive cannot be listed. |
| `authority-stale` | The newest authority copy on Drive is behind this host's journal for more than 90 minutes. |
| `autodeploy-silent` | Autodeploy has not completed a check of GitHub for 6 hours. |

The same problem is reported at most once every 6 hours, and one
"resolved" message follows when it clears. A message that could not be delivered
is retried by the next run. Every alert also goes to the journal.

**Outside view:** `.github/workflows/uptime.yml` (GitHub-hosted, every 30
minutes) asks the public `/readyz` with retries and fails the run if it is not
200; GitHub emails the failure. This is the only alarm that works when the whole
host or its tunnel is down. GitHub disables scheduled workflows after 60 days
without repository activity (it emails a warning first); if that ever happens,
re-enable the workflow in the Actions tab.

## Day to day

- Logs: `journalctl -u muster-cloud-autodeploy`, `docker logs muster-cloud-web-<sha>`,
  `journalctl -u muster-cloud-backup -u muster-cloud-retention -u muster-cloud-offsite`.
- Boot the running commit again (for example after editing a file in
  `/etc/muster-cloud`): `sudo touch /var/lib/muster-cloud/autodeploy/redeploy`
  and `sudo systemctl start muster-cloud-autodeploy.service`. The marker is
  consumed by that one deploy. A new commit on `main` needs nothing: it deploys
  itself.
- Containers stopped by hand: Docker's restart policy brings a crashed container
  back but not one that was stopped with `docker stop` / `docker kill`. The guard
  starts `muster-cloud-db`, `kamal-proxy` and the web container of the version
  autodeploy recorded as deployed (`/var/lib/muster-cloud/autodeploy/deployed`)
  if they exist and are stopped, in that order, waits up to 60 s for `/readyz`,
  and alerts once. It never starts any other container or an older web version.
  It runs every 15 seconds from its own timer, `muster-cloud-guard.timer`
  (`muster-cloud-autodeploy --guard`), so a stopped container is back in well
  under a minute, and also at the start of every 5-minute autodeploy run. It
  writes nothing to the journal while everything is up. It does nothing (one
  journal line) while a deploy, rollback or redeploy holds the autodeploy lock
  or while any Kamal container runs on the host, so it cannot undo a switch.
- Stop the guard on purpose (for example to stop a container by hand for
  maintenance): `sudo systemctl stop muster-cloud-guard.timer`, and
  `sudo systemctl start muster-cloud-guard.timer` afterwards. Stopping it is not
  enough: the 5-minute autodeploy run still starts the container, so stop
  `muster-cloud-autodeploy.timer` too. Host-setup enables the guard timer again
  at the next deploy. The `guard-only` marker below does not stop it: guard-only
  still guards.
- Hold deploys but keep the guard (for example before the first merge to `main`,
  or while investigating): `sudo touch /var/lib/muster-cloud/autodeploy/guard-only`.
  While the file exists every run does only the guard and exits 0 without
  contacting GitHub; `sudo rm -f /var/lib/muster-cloud/autodeploy/guard-only`
  resumes deploys. Unlike stopping the timer, this keeps the containers up. A run
  that does not check GitHub does not refresh the `autodeploy-silent` stamp, so
  after 6 hours of holding that alert fires as a reminder that deploys are held.
- A commit that failed 3 times is skipped until a newer one arrives; to retry it
  now: `sudo rm -f /var/lib/muster-cloud/autodeploy/attempts/*` and start the
  service.
- Changing a secret: edit `kamal.env`, then boot again (previous bullet). `POSTGRES_PASSWORD`
  only seeds the first database creation; to rotate the live password use
  `ALTER ROLE muster PASSWORD ...` in the database, then update `kamal.env` and
  `ops.env`.
- PostgreSQL minor update: change the tag and digest in `config/deploy.yml`,
  then `bin/kamal accessory reboot db` as `gha` (a short database outage; the
  service recovers on its own).
- Docker keeps the last 3 app containers and images (`retain_containers`).

## Moving to a real domain later

Today HTTPS comes from Tailscale Funnel. To use your own domain instead:

1. Point an A record at the VPS public address and make sure ports 80 and 443
   are reachable. Note that Docker-published ports bypass UFW.
2. In `config/deploy.yml`: under `proxy`, set `ssl: true` and
   `host: <your domain>`, set `forward_headers: false` (kamal-proxy then sets
   the real client address itself), and under `proxy.run` use `http_port: 80`,
   `https_port: 443` and remove `bind_ips`.
3. Set `MUSTER_CLOUD_PUBLIC_URL=https://<your domain>` in `deploy.env`, and the
   same address in `.github/workflows/uptime.yml`.
4. The proxy container only reads its run options when it is created: run
   `bin/kamal proxy reboot` once.
5. Remove the funnel (`sudo tailscale funnel --https=10000 off`), update the
   OIDC redirect URI and the Paddle webhook URL, and drop the loopback port
   checks from `host-setup.sh` and the local check from `muster-cloud-autodeploy`.

Session cookies are bound to the exact origin, so everyone signs in again
after the URL changes.

## Rolling back

Autodeploy only ever moves forward: it refuses a commit older than what runs.
To go back by hand and stay there, stop it first:

```
sudo systemctl stop muster-cloud-autodeploy.timer
sudo -iu gha                              # then, from /var/lib/muster-cloud/run or any checkout of that commit:
bin/kamal app containers                  # versions still on the host
bin/kamal rollback <sha> --version <sha>  # boots that version, drains the current one
sudo /usr/local/sbin/muster-cloud-install-ops localhost:5000/oliveralerubio/muster-cloud:<sha>
```

The last command makes the timers use the matching `muster-cloud-ops`. Start the
timer again (`sudo systemctl start muster-cloud-autodeploy.timer`) once a fixed
commit is on `main`; it will deploy it.

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

## What is on Google Drive, and what can delete it

`gdrive:muster-cloud-backups/` holds:

- `snapshots/<backup_id>/`: the published snapshots (`manifest.json`,
  `db.dump.age`, `git.tar.age`). Artifacts are age-encrypted. Each snapshot's
  `manifest.json` is **not**: it lists workspace ids, their current Git head
  hashes, sizes and timestamps.
- `authority/authority-<UTC>-<sequence>.tar.age`: the lifecycle authority
  (control journal, witness, a small `META.json`), encrypted to the same age
  recipient. Written after every backup and hourly when the journal moved on.
  Append-only: nothing deletes these (they are a few kilobytes).

`muster-cloud-offsite` uploads with `rclone copy` only. An upload can never
delete anything, so an emptied or wiped local snapshot directory deletes
nothing offsite. The only deletions are the ones retention recorded: every
`audit/retention-*.json` has a `removed` list of snapshot ids; for each audit
file not yet processed, each id is deleted from Drive only if it is a valid
snapshot id and is absent locally, with Drive's trash bypassed (a deleted
tenant's data really expires). Each audit file is processed once. A single run
deletes at most 100 snapshots; a record listing more than that, or one that is
not a valid retention record, is refused and alerts. Forging audit files needs
root on the host, and with root the local snapshots could be destroyed anyway;
the 100-per-run cap and the alert make such damage slow and loud.

## Disaster recovery (tested procedure)

The backups deliberately do **not** contain the lifecycle authority: a restore
is judged against a journal that outlives the database. After the loss of the
whole host the authority is gone too, and `muster-cloud-ops init` would make a
NEW one that rejects every restored database. So a recovery starts from the
encrypted authority copy on Drive and the age private key.

You need: the age **private** key (password manager), `rclone` with the
`gdrive:` remote, `age`, Docker, a PostgreSQL 16 client and this repository.

### Rehearse it (do this before you need it)

```
install -m 0600 /dev/null /dev/shm/muster-backup.key          # RAM disk
# paste the age private key into it from your password manager, then:
sudo deploy/kamal/muster-cloud-restore-drill --age-identity /dev/shm/muster-backup.key --shred-identity
```

The drill is a real recovery into throwaway targets on this host, never
touching production: it downloads the newest complete snapshot and the newest
authority copy from Drive, decrypts the authority, starts a throwaway PostgreSQL
(the pinned image, no network, scratch directory), runs `deploy/pg-restore.sh`
(structure and every hash, restore, reconciliation with the journal, head and
`git fsck` checks, `restore-check`), repeats the checks, compares the restored
tenants and heads with `manifest.json`, prints one JSON line and removes
everything it created. The JSON has `ok`, the snapshot id and time,
`max_data_loss_seconds` (the age of that snapshot: the data you would lose),
the authority copy used and `elapsed_seconds`. Exit code 0 only when every check
passed. Run it after setting up, and again every few months or after changing
the backup code; it needs about four times the snapshot size in `/var/tmp`
(`--workdir` changes that).

### Recover for real

**Same host, data damaged (authority intact):** go straight to step 4.

**New host after total loss:**

1. Do the bootstrap, steps 0 to 5, on the new host (including `host-setup.sh`;
   the same `ops.env`, `kamal.env` and `deploy.env`; `MUSTER_BACKUP_AGE_RECIPIENT`
   must stay the same key). Keep the autodeploy timer stopped. Do **not** run
   `init`.
2. Put the age private key in `/dev/shm/muster-backup.key` (mode 0600).
3. Restore the authority from the newest copy on Drive:
   ```
   sudo rclone lsf gdrive:muster-cloud-backups/authority          # newest = last line
   sudo install -d -m 0700 /dev/shm/authority
   sudo rclone copyto gdrive:muster-cloud-backups/authority/<name> /dev/shm/authority/a.tar.age
   sudo age --decrypt -i /dev/shm/muster-backup.key -o /dev/shm/authority/a.tar /dev/shm/authority/a.tar.age
   sudo tar -xf /dev/shm/authority/a.tar -C /dev/shm/authority       # journal.v1.jsonl, witness.json, META.json
   sudo install -m 0600 -o 10001 -g 10001 /dev/shm/authority/journal.v1.jsonl /srv/muster-cloud/control/journal.v1.jsonl
   sudo install -m 0600 -o 10001 -g 10001 /dev/shm/authority/witness.json /var/lib/muster-witness/w/witness.json
   sudo rm -rf /dev/shm/authority
   ```
   The witness was copied before the journal, so the journal is never behind it.
   Install `muster-cloud-ops` (`docker pull` the image of the latest release,
   `muster-cloud-install-ops <ref>`) and check it opens:
   `sudo systemd-run --pipe --wait --collect -p User=muster-cloud -p Group=muster-cloud -p UMask=0077 -p EnvironmentFile=/etc/muster-cloud/ops.env /usr/local/bin/muster-cloud-ops status`.
4. Download the newest complete snapshot (a directory with `manifest.json`):
   ```
   sudo rclone lsf gdrive:muster-cloud-backups/snapshots --recursive --include manifest.json   # newest = last
   sudo install -d -m 0700 /srv/muster-cloud/restore
   sudo rclone copy gdrive:muster-cloud-backups/snapshots/<backup_id> /srv/muster-cloud/restore/<backup_id> --checksum
   ```
   (Keep downloads outside `/srv/muster-cloud/backups`: anything unknown in the
   backup root is reported as uncatalogued. Local snapshots are in
   `/srv/muster-cloud/backups/snapshots`, no download needed.)
5. Restore **into new targets** with `deploy/pg-restore.sh` as root (see
   `README.md`), the same call the drill makes: a new database name and a new
   repository root such as `/srv/muster-cloud/repos-restored`, the admin URL
   through `MUSTER_RESTORE_ADMIN_URL`
   (`postgres://muster:<password>@localhost/postgres?host=/srv/muster-cloud/pgsocket`),
   `--age-identity-file /dev/shm/muster-backup.key`, and `--control-dir`,
   `--control-witness` pointing at the restored authority. It refuses anything
   unsafe, joins the restore as a new candidate incarnation and erases tenants
   deleted after the backup. Then `chown -R 10001:10001` the repository root.
6. Cut over, deliberately:
   1. `muster-cloud-ops activate --incarnation <id printed by the restore> --database-url <new db>`
      (this fences the running service, if any: it starts refusing requests).
   2. `docker stop` the service container if there is one, move
      `/srv/muster-cloud/repos` aside and put the restored repository root in its
      place (owner `10001:10001`, mode `0700`).
   3. Update the database name in `MUSTER_DATABASE_URL` in both `kamal.env` and
      `ops.env`, then start autodeploy again
      (`sudo systemctl start muster-cloud-autodeploy.timer`; it deploys the HEAD of
      `main`).
7. Delete the age key from the RAM disk (`shred -u /dev/shm/muster-backup.key`),
   run the backup once, and check `muster-cloud-backup-check` is quiet.

What you lose: everything after the snapshot (up to 6 hours of data), plus
everything the journal says was deleted stays deleted.

## Risks you accept with this setup

- **Rollback of everything with Drive access plus the age key.** The authority
  copy on Drive is what protects a restore from a stale or forged database. Whoever
  has both write access to the Drive folder and the age private key can upload an
  older authority copy together with an older snapshot, and a recovery would
  roll the whole deployment back to that point without anything detecting it.
  The age key is not on the host, and Drive is not writable from anything but
  root's `rclone` remote; but both are single points of trust, and the system
  does not defend against someone who holds both.
- **Shared machine.** The VPS also runs other projects and CI. Kamal and the
  helper rely on `gha` being in the `docker` group, which is root-equivalent.
  Autodeploy starts Kamal as `gha` only from an export of a commit of `main` with
  green CI, but any other job that runs as `gha` on this host can read
  `kamal.env`. The self-hosted runner pools that are already up (`deploy-personal`,
  `ci-docker`) stay reachable by the pull-request workflows of this public
  repository; this change removes the deploy from them but does not touch the
  pools themselves. Remove `gha` from those runners or restrict the repository's
  workflow permissions (Settings, Actions, "Fork pull request workflows" and
  "Runners") separately. The app is capped at 1 GB and 2 CPUs and the database at
  1 GB, both with a protected OOM score, but a neighbour can still starve the disk
  or network.
- **The witness is on the same physical disk.** It is its own filesystem (so a
  damaged or rolled-back `/srv` is detected), but a disk failure, or a snapshot
  restore of the whole VPS, takes the database, journal and witness together
  and the rollback cannot be detected locally (the authority copy on Drive makes
  it detectable on recovery).
- **Single server.** Deploys have no downtime, but a host failure means a
  restore from Drive: up to 6 hours of data (the backup interval) can be lost,
  and the service is down until you rebuild.
- **Public entry point.** Funnel exposes port 10000 of the node's Tailscale
  name. Tailscale relays the connection; TLS is terminated on this host. If the
  tailnet is renamed, the URL, cookies, OIDC, Paddle and uptime settings change.
- **No TLS inside the Docker network.** The service talks to PostgreSQL
  unencrypted over the private `kamal` network (`sslmode=disable`). The
  database publishes no port, but anything else attached to that network could
  reach it.
- **The package is public.** Anyone can pull the service image (no secrets in
  it). That is what removes every credential from the host.
- **Alerts need Hermes and Discord.** If either is down, alerts stay in the
  journal; the GitHub uptime check still emails a public outage.

## Compatibility note: `muster-cloud-ops` on the host

The binary is built inside the image (Debian bookworm, glibc 2.36) and copied
to the Ubuntu 24.04 host (glibc 2.39). That works because glibc is backward
compatible and the program links only `libc`, `libm` and `libgcc_s` (TLS is
rustls, not OpenSSL). `muster-cloud-install-ops` re-checks with `ldd` on every
install and refuses a binary that would not run. It was verified by running the
image's binary in an `ubuntu:24.04` container.
