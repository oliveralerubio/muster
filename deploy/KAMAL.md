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
                     -> muster-cloud-authority-offsite (encrypted authority copy: seconds
                        after every journal change, via a .path unit, plus hourly)
                     -> muster-cloud-restore-drill (monthly, unattended, throwaway targets)
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
| `muster-cloud-authority-offsite.path` + `.timer` | An age-encrypted copy of the lifecycle authority (journal and witness) to Google Drive: started a few seconds after every change of the control journal (the path unit), right after every successful backup, and hourly (the timer, the backstop). See "How soon a change reaches Drive" below. |
| `muster-cloud-restore-drill.timer` | Monthly (the 3rd, about 03:30, up to 3 hours late): a real recovery of the newest Drive snapshot into throwaway targets, opened with the drill-only key. See "The monthly restore drill". |
| `muster-cloud-backup-check.timer` | Every 30 minutes: alerts when backups quietly stop (see Alerts). Also restarts the authority-copy path unit if systemd stopped it, and watches that the restore drill keeps succeeding. |
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
| `MUSTER_BACKUP_AGE_RECIPIENTS` | Optional, comma-separated: further age public keys every backup and authority copy is also encrypted to. `host-setup.sh` adds the public half of the drill-only key here by itself (step 4); you do not write it. |

Generate the age key pair on your own computer (`age-keygen -o muster-backup.key`),
put only the public key here, and keep the private key in your password manager.
It never touches the VPS (except for the minutes of a manual restore drill or a
recovery, below); without it no backup can be restored. The only private age key
that lives on the host is the separate drill-only key of step 4. It is a full
recipient: it can decrypt every backup and every authority copy made after it was
added, including the ones the drill never restores, so whoever has root on the
host can read all of them (what that costs you in security is under "The monthly
restore drill" and "Risks you accept"). It is not what restores a new host: that
is still the main key.

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
```

On a fresh host (no `/var/lib/muster-cloud/autodeploy/deployed` record yet)
`host-setup.sh` itself creates the `guard-only` marker
(`/var/lib/muster-cloud/autodeploy/guard-only`) **before it enables the timers**,
so nothing can deploy before `init` (step 6) and you have nothing to remember. The
marker stays until you remove it in step 7. On a host that has deployed before it
adds nothing, and never touches an existing `guard-only` or `hold`.

It creates what is missing (accounts, directories with exact modes, the
`kamal` Docker network with the fixed subnet `172.31.250.0/24`), installs the
host scripts and all units, deletes the old `gha` sudoers rule if one exists,
enables the timers and the authority-copy path unit, generates the drill-only
age key `/etc/muster-cloud/drill-age.key` (root `0400`) when it does not exist
and adds its public half to `MUSTER_BACKUP_AGE_RECIPIENTS` in `ops.env`
(see "The monthly restore drill"), and sets up Funnel
(`tailscale funnel --bg --https=10000 http://127.0.0.1:18110`). It refuses to
touch a `kamal` network with another subnet, and refuses to create anything
under `/var/lib/muster-witness` unless that is its own mounted filesystem. It
never touches other containers, networks, services, UFW, `/etc/docker` or the
other Tailscale serve entries. Anything left for you appears as `TODO` or
`FAIL`; fix it and run it again until it is quiet (it is safe to repeat).
After this first run autodeploy runs it by itself on every release.

### 5. Start the database (required, before `init`)

On a fresh host this step is not optional, and the order matters. `init`
(step 6) needs the database to exist, and nothing else would create it in time:
autodeploy is what normally boots the database, but it refuses to deploy before
`init` (the `guard-only` marker `host-setup.sh` created in step 4), and the guard
cannot boot anything yet because there is no source checkout on a new host (it
leaves that to the first deploy). So the database is started by hand, once, as `gha`, from a
checkout of this repository that `gha` can read:

```
sudo -iu gha
git clone https://github.com/oliveralerubio/muster.git ~/muster   # skip when you already have a checkout gha can read
cd ~/muster
./bin/kamal accessory boot db
exit
```

Check it: `docker ps` shows `muster-cloud-db` running and
`sudo docker exec muster-cloud-db pg_isready -U muster` answers "accepting
connections". The command is safe to repeat: it skips a container that already
exists, and PostgreSQL's entrypoint runs `initdb` only when
`/srv/muster-cloud/pg/data` has no `PG_VERSION`, so an existing data directory
(a restore, or a host that lost only its containers) is used as it is.
PostgreSQL 16 starts as user 10002 with scram-sha-256 everywhere, no published
port, data in `/srv/muster-cloud/pg/data`. It reads `POSTGRES_PASSWORD` from
`kamal.env` (step 3), and no registry login happens (the registry is
`localhost:5000`). `bin/kamal` labels its container as a manual run, so the
guard (which runs every 15 seconds) waits until it finishes.

You do not need `./bin/kamal proxy boot` here: `init` does not use the proxy,
and the first deploy boots `kamal-proxy` itself when no such container exists
(it logs one decision line). If you want the proxy up earlier, `./bin/kamal
proxy boot` is safe to repeat; it is start-or-run with `proxy.run`
(kamal-proxy v0.10.3, published only on `127.0.0.1:18110` and
`127.0.0.1:18111`; checked in the Kamal 2.12.0 source and by rendering the
command).

After the first deploy, autodeploy starts what is missing by itself: before
booting the app it runs `kamal proxy boot` when no `kamal-proxy` container
exists at all, and `kamal accessory boot db` when no `muster-cloud-db` container
exists. A stopped container is not "missing": the liveness guard starts it.

### 6. Publish the first image and create the lifecycle authority, once

Merge to `main` (or run the workflow **Publish muster Cloud image** from
`main`). Then make the package public (step 1) and run `host-setup.sh --check`
again: the anonymous pull check must be quiet.

Create the authority on pristine targets only (empty database from step 5, empty
repository root). `init` refuses anything else, so a mistaken repeat is harmless, but never
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
systemctl list-timers 'muster-cloud-*'     # eight timers
systemctl status muster-cloud-authority-offsite.path   # active (waiting)
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
sudo systemctl start muster-cloud-restore-drill.service    # a few minutes; see "The monthly restore drill"
sudo journalctl -u muster-cloud-restore-drill -n 5 --no-pager   # one JSON line with "ok":true
```

The drill needs a backup made after the drill key was added (step 4 did that
before the first backup, so the one above is enough), uploaded by the offsite
service, and an authority copy. If you run it earlier it stops with a clear
message and nothing is changed.

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
| `authority-path-down` | `muster-cloud-authority-offsite.path` is enabled but not running and could not be restarted. (When the restart works, nothing is sent: the check restarts it by itself, and starts an authority copy at once so no change is missed.) |
| `restore-drill-stale` | The monthly restore drill has not succeeded for 40 days, counted from its last success or from the day it was enabled. A drill that fails also raises `unit:muster-cloud-restore-drill.service` at once. |
| `autodeploy-silent` | Autodeploy has not completed a check of GitHub for 6 hours. |

The same problem is reported at most once every 6 hours, and one
"resolved" message follows when it clears. A message that could not be delivered
is retried by the next run. Every alert also goes to the journal.

**Outside view:** `.github/workflows/uptime.yml` (GitHub-hosted, every 30
minutes) asks the public `/readyz` with retries and fails the run if it is not
200; GitHub emails the failure. This is the only alarm that works when the whole
host or its tunnel is down. GitHub disables scheduled workflows after 60 days
without repository activity, so the workflow re-enables itself once a week (a job
that calls the "enable workflow" API with its own `GITHUB_TOKEN`, no commits).
That relies on GitHub's enable API restarting the inactivity timer, which is how
the `keepalive-workflow` action works too; it is not a documented guarantee, so
if GitHub ever changed it, the failure mode is the old one: re-enable the
workflow in the Actions tab (GitHub emails a warning first).

## Day to day

- Logs: `journalctl -u muster-cloud-autodeploy`, `docker logs muster-cloud-web-<sha>`,
  `journalctl -u muster-cloud-backup -u muster-cloud-retention -u muster-cloud-offsite`.
- Boot the running version again with fresh settings (for example after editing
  a file in `/etc/muster-cloud`, or after a disaster-recovery cutover):
  `sudo touch /var/lib/muster-cloud/autodeploy/redeploy` and
  `sudo systemctl start muster-cloud-autodeploy.service` (or wait up to 5
  minutes). `redeploy` reboots **the version that is routed now** (or the
  recorded one when the routes cannot be read). It never moves to a newer commit
  of `main`, asks neither GitHub nor CI, and needs the image to be only on this
  host (or that exact sha to be pullable). Kamal renames the old container, boots
  a new one that reads its settings afresh, and stops the old one once the new
  one is routed and ready (no downtime, and no need to stop anything by hand).
  The marker is removed only after a deploy succeeded: a failed or interrupted
  attempt keeps it, under the same limit of 3 attempts 30 minutes apart as any
  commit, and a marker touched again while that deploy ran stays for the next
  run. While the `hold` below exists nothing runs, so the redeploy waits until it
  is released. A new commit on `main` needs nothing: it deploys itself.
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
  or while a Kamal container runs on the host (one that carries the label
  `muster.kamal=1`, which `bin/kamal` sets on its `docker run`, or any
  `ghcr.io/basecamp/kamal` image, so also another Kamal version or a Kamal
  started by hand), so it cannot undo a switch, for example during
  `kamal proxy reboot` or `kamal accessory reboot db`.
- What "the deployed version" is: the version kamal-proxy routes the
  `muster-cloud-web` service to (`docker exec kamal-proxy kamal-proxy list`),
  not a container that merely runs. The guard records it in
  `/var/lib/muster-cloud/autodeploy/deployed` only when at most one web
  container runs and no deploy was cut short; it never records a version the proxy
  does not route to. A full run treats a deploy that was cut short as incomplete
  and boots the commit again through the normal path (readiness checks,
  rollback, alert), never as a no-op: when the file
  `/var/lib/muster-cloud/autodeploy/deploying` was left behind (the previous
  run died after it started changing things), or when two web containers run and
  the proxy routes to one that is not the recorded version or does not answer
  `/readyz`. The journal says `decision: incomplete deploy: ...`.
- Stop the guard on purpose (for example to stop a container by hand for
  maintenance): `sudo muster-cloud-autodeploy --hold` (below). It stops the guard
  and the deploys together. Stopping the timers by hand is not needed and is not
  enough: host-setup enables them again at the next deploy.
- Two markers pause autodeploy, and they are not the same:
  - `guard-only` holds **deploys** but keeps the **guard** (use it before the
    first merge to `main`, or while investigating):
    `sudo touch /var/lib/muster-cloud/autodeploy/guard-only`. While the file
    exists every run does only the guard and exits 0 without contacting GitHub;
    `sudo rm -f /var/lib/muster-cloud/autodeploy/guard-only` resumes deploys.
    Unlike stopping the timer, this keeps the containers up. A run that does not
    check GitHub does not refresh the `autodeploy-silent` stamp, so after 6 hours
    of holding that alert fires as a reminder that deploys are held.
  - `hold` is **maintenance mode**. `sudo muster-cloud-autodeploy --hold` creates
    the marker (`/var/lib/muster-cloud/autodeploy/hold`) **and waits until any run
    that is already in flight, the guard included, has finished**; it prints
    `held` when nothing of autodeploy is running any more (a run that is cut
    short at its next step keeps its `deploying` marker and is finished after the
    release). From then on both the guard and the full run do nothing at all (no
    container is started or stopped, no deploy, no change to the deployed record,
    no contact with GitHub) until `sudo muster-cloud-autodeploy --release`
    removes it and prints `released`. Both are safe to repeat. Use it for **every
    manual Kamal command, every rollback and every disaster-recovery cutover**:
    start with `--hold`, end with `--release`; touching the file by hand is not
    the same, because a deploy that is already running would carry on under your
    feet. It logs one journal line when it appears and then every 10 minutes, and
    alerts once when it is older than 24 hours (then at most once per 24 hours),
    so a forgotten hold does not stay quiet. While it exists the containers are
    **not** kept up: if one stops, no one starts it. Release it as soon as you
    are done; the next run (within 5 minutes) resumes from the state you left,
    and runs the `redeploy` marker if you touched one.
- Manual Kamal runs: `bin/kamal` labels the container it starts with
  `muster.kamal.owner=<owner>`, where the owner is `MUSTER_KAMAL_OWNER` or
  `manual` when that is not set; autodeploy's own calls use `autodeploy`. A full
  autodeploy run does nothing (it exits 0 and writes one decision line) while
  any Kamal container that is not its own runs, checked at the start and before
  every Kamal step, and the guard also waits. So a `bin/kamal` you started by
  hand is never undone halfway by a deploy. That protects one command, not a
  whole procedure with pauses between commands: for a procedure (a rollback, a
  cutover) run `--hold` first and `--release` at the end. Run Kamal **only on
  this host, through `./bin/kamal`** (as `gha`), never from a laptop: only a
  Kamal that runs here is visible to the guard and to the lock recovery below,
  and a Kamal on another machine holds the same deploy lock without anyone here
  knowing. The owner may only contain letters, digits, `.`, `_` and `-`, and is
  not a secret.
- A stale Kamal lock: Kamal keeps a deploy lock (a directory on the host) while a
  command runs, and a power loss or a kill leaves it behind, which makes every
  later Kamal command refuse with "Deploy lock found". Before its Kamal commands
  autodeploy looks at the lock directory (`~gha/.kamal/lock-muster-cloud`): when
  there is a lock, no Kamal container runs here, and the lock was taken before
  this host's last boot or is **older than 60 minutes**, it releases it by
  itself (one decision line and one alert). A younger lock is waited for: that
  run is skipped and the next one looks again. The age counts from when the lock
  was taken, and a lock left by a command you started by hand is treated the
  same way, so do not leave a half-finished manual Kamal command around; if you
  must clear one at once (nothing running), run `./bin/kamal lock release` as
  `gha` (a lock whose `details` file was never written is not removed by it:
  autodeploy removes the directory itself).
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
To go back by hand and stay there, put it in maintenance mode first. `--hold`
stops the guard and the deploys together and waits until a run that is already
in flight has finished (stopping the timers is not needed, and not enough: the
guard timer would start the container you drained):

```
sudo muster-cloud-autodeploy --hold        # prints "held" once nothing of autodeploy is running
sudo -iu gha                               # then, from /var/lib/muster-cloud/run or any checkout of that commit:
bin/kamal app containers                   # versions still on the host
bin/kamal rollback <sha> --version <sha>   # boots that version, drains the current one (owner "manual")
exit
curl -fsS http://127.0.0.1:18110/readyz    # and check docker ps: nothing keeps the containers up while the hold lasts
```

Run it on the host only, through `./bin/kamal`, never from a laptop. Do **not**
run `muster-cloud-install-ops` with the older image (older guides said to): see
the next paragraph. Leave the hold in place until a fixed commit is on `main`:
releasing it while the bad commit is still HEAD makes the next run deploy that
commit again, undoing the rollback. When the fix is on `main`, run
`sudo muster-cloud-autodeploy --release`; it deploys it within 5 minutes. (The
hold alerts after 24 hours as a reminder.)

**The host timers keep the newest `muster-cloud-ops`.** The `muster-cloud-ops`
that the backup, retention and authority timers run is a separate binary on the
host (`/usr/local/bin/muster-cloud-ops`, installed by
`muster-cloud-install-ops`), not the service container. A rollback moves only the
service; never point the host back to an older ops. A backup made for more than
one recipient (every backup since the drill key was added) has a manifest field,
`recipients_sha256`, that an older ops refuses to read. Retention would then
treat those snapshots as unknown and skip them, so their deletion deadlines (30
days, or the earlier cap a tenant's deletion sets) would pass unnoticed and data
that must expire would stay. For the same reason an older ops cannot verify or
restore those snapshots: use the newest `deploy/pg-restore.sh` and
`muster-cloud-ops` for recoveries. `muster-cloud-install-ops` therefore refuses
(exit 65) an image that was built before the one installed now (it compares the
image creation times it recorded); installing the same version again is fine.
Autodeploy's own rollback after a failed deploy goes through the same command,
so it also leaves the newest ops in place and logs "could not reinstall the
matching muster-cloud-ops": that is expected. `MUSTER_OPS_ALLOW_OLDER=1` in front
of one `muster-cloud-install-ops` call overrides the refusal; use it only if the
newest ops is itself broken and you have checked that the older one reads every
manifest made since.

Why the newest ops is safe next to an older service, exactly: `muster-cloud-ops`
applies database migrations only in `init` and `register` (never run those during
a rollback); `backup run`, `retention run`, `status`, `activate` and
`restore-check` work on the schema as it is. Migrations only go forward and the
newest service had already applied every migration of its release when it
started, which happened before its ops was installed. So the database is at
least as new as the newest ops expects, whether the older service then runs on
it (a rollback that crosses no schema change, or only additive ones) or refuses
to start (below), and the newest ops never meets an older schema than its own.

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
  recipients as the backups. Written within seconds of a journal change, after
  every backup and hourly when the journal moved on. Append-only: nothing deletes
  these (they are a few kilobytes).

### How soon a change reaches Drive

The authority copy is what lets a recovery know which tenants were deleted, so
it matters that it follows the journal closely. It is **not** instantaneous, and
this guide does not claim it is:

- `muster-cloud-authority-offsite.path` watches the journal file
  (`PathChanged`). A change starts the copy service, which waits 5 seconds (so a
  burst of records becomes one or two uploads), archives, encrypts and uploads,
  and, if the journal grew while it ran, copies again by itself (up to 5 rounds
  per start; a start while a copy runs is a no-op for systemd, which is why the
  script does this itself). The path unit is rate-limited (60 triggers per 30
  seconds). When the journal is still moving after the fifth round, the copy
  does not report success: it exits 75 and the service (`Restart=on-failure`,
  every 30 seconds) runs it again, so a busy period catches up without waiting
  for the hourly timer. Those restarts do not alert one by one: the alert fires
  only when the service finally fails (40 starts within 30 minutes, about 20
  minutes of failing in a row, or a configuration problem, which is not retried),
  and `muster-cloud-alert` sends one message per 6 hours for it. The hourly timer
  is the backstop for anything this missed, and the 30-minute check restarts the
  path unit if systemd ever stopped it (or refused its triggers while the service
  was in that failed state).
- So the window between a journal change and its copy on Drive is **seconds to
  about a minute** when Drive answers (the settle time plus the upload), and up
  to an hour when an upload fails and the timer has to retry; the
  `authority-stale` alert fires when a copy has been behind for more than 90
  minutes, and a copy that keeps failing raises
  `unit:muster-cloud-authority-offsite.service` (after about 20 minutes of
  retries every 30 seconds; at once for a configuration problem). A host lost inside that window recovers with an authority copy that
  misses the last change: a deletion made in those seconds is not in the copy,
  and a restore from it would not erase that tenant again until the deletion is
  repeated. The window is small, not zero.

`muster-cloud-offsite` uploads with `rclone copy` only. An upload can never
delete anything, so an emptied or wiped local snapshot directory deletes
nothing offsite. The only deletions are the ones retention recorded: every
`audit/retention-*.json` has a `removed` list of snapshot ids; for each audit
file not yet processed, each id is deleted from Drive only if it is a valid
snapshot id and is absent locally, with Drive's trash bypassed (a deleted
tenant's data really expires). Each audit file is processed once. A single run
deletes at most 100 snapshots; a record listing more than that, or one that is
not a valid retention record, is refused and alerts.

Before it deletes anything the script lists Drive **afresh** (never a listing
from before an earlier purge of the same run) and checks that a complete
snapshot at least as new as the newest local one would remain: its
`manifest.json` is read back and must pass the rules the restore applies
(format `muster-backup` version 2, the encryption scheme's two artifact names for
the kinds `db` and `git` and nothing else, valid timestamps), and both artifacts
must be on Drive with exactly the listed size (and SHA-256, when Drive reports
one). The first deletion that fails stops all deletions for that run: the
remaining audit records stay unprocessed, the run exits 3 and alerts, and the
next run lists Drive again, so a half-deleted snapshot is never counted as the
complete one that must remain. Forging audit files needs
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
passed. Run it by hand with the main key when you want to prove that the key in your
password manager still opens the backups (the monthly drill below uses the
drill-only key, so it proves everything except that one key); it needs about
four times the snapshot size in `/var/tmp` (`--workdir` changes that). The
drill refuses cleanly, before it downloads anything, when there is not enough
space, and with `--select-only` it only says which snapshot it would use.

### The monthly restore drill

A recovery nobody practised is a guess, and a person will not run the drill every
month, so it runs by itself: `muster-cloud-restore-drill.timer` (the 3rd of each
month at about 03:30, up to 3 hours late, `Persistent=true` so a month missed
because the host was off runs at the next boot). It is the same command as
above, run as root from the source checkout autodeploy keeps current:

```
muster-cloud-restore-drill --age-identity /etc/muster-cloud/drill-age.key --workdir /var/lib/muster-cloud/drill
```

- **A second, drill-only age key.** The main key stays in your password manager.
  `host-setup.sh` generates `/etc/muster-cloud/drill-age.key` (root, `0400`; it
  never overwrites an existing one) and adds its public half to
  `MUSTER_BACKUP_AGE_RECIPIENTS` in `ops.env`, so every backup and every
  authority copy made from then on can be opened by either key. The drill uses
  only the drill key and never shreds it (`--shred-identity` is for the main key).
- **Which snapshot.** The newest complete snapshot whose manifest lists the drill
  key's public key as a recipient, so it can really be opened. If the newest
  snapshots were made before the drill key existed, it skips them and says so;
  if no such snapshot is younger than 8 days it fails with a message saying so
  (a long gap here means backups stopped, or the offsite copy did). The same
  goes for the authority copy: it must be encrypted to the drill key, so the
  first drill after the key was added needs one backup and one authority copy
  made after it (a drill started earlier fails loudly and changes nothing).
- **Space.** The scratch directory is `/var/lib/muster-cloud/drill` (never under
  `/srv/muster-cloud`), and the drill checks about four times the snapshot size is
  free before it downloads anything; if not, it fails with the needed and the free
  space. Free space or move the directory (`--workdir` in the unit) and run
  `sudo systemctl start muster-cloud-restore-drill.service`.
- **Leftovers.** While it runs the drill holds restored data in plaintext in a
  scratch directory under `/var/lib/muster-cloud/drill` (mode `0700`, the
  snapshot and the repositories inside are root's or the service's only). A
  drill that is killed, or cut short by a power loss or reboot, would leave it
  there: so every drill first removes any `muster-restore-drill.*` directory in
  its workdir and any `muster-drill-pg-*` container (and logs a line saying so),
  `host-setup.sh` installs `/etc/tmpfiles.d/muster-cloud-drill.conf`, which
  removes those directories at boot, and
  `muster-cloud-restore-drill --clean-only --workdir /var/lib/muster-cloud/drill`
  does the same by hand. The restore itself runs as the service's uid (the same
  way the real recovery does), so `host-setup.sh` keeps the drill directory
  `0711` (others may pass through, not list).
- **What counts as success.** Exit 0 and one JSON line with `"ok":true` in the
  journal (`journalctl -u muster-cloud-restore-drill`), printed only after the
  scratch directory and the container were removed. A drill that cannot remove
  them fails (non-zero, `"ok":false`, "cleanup failed"): the unit fails, so
  `last-ok` is not touched and the failure alerts. Only a clean success touches
  `/var/lib/muster-cloud/drill/last-ok`. A failed drill raises
  `unit:muster-cloud-restore-drill.service` at once, and the 30-minute check
  raises `restore-drill-stale` when there has been no success for 40 days.
- **The cost, honestly.** Until now the age private key never lived on this host.
  The drill key does, readable by root. Whoever has root on this host (or its disk)
  can now decrypt the Drive snapshots and authority copies made after the key
  was added: that includes data of tenants deleted since, until their snapshot
  expires (30 days by default, `MUSTER_BACKUP_RETENTION_DAYS`). That is the price of
  an unattended drill. It does not give more than root already had on live data
  (root can read the database and the repositories), and it does not open anything
  made before the key was added. This is deliberately not a one-line switch:
  `host-setup.sh` puts the key, its recipient entry and the timer back at every
  deploy, so removing the drill means removing it from `host-setup.sh` (and
  `deploy/systemd/`) in a commit, then deleting `drill-age.key` and its public
  key from `MUSTER_BACKUP_AGE_RECIPIENTS` on the host.
- **Changing the key.** Move `drill-age.key` aside and run
  `sudo deploy/kamal/host-setup.sh`: it generates a new one and adds the new public
  key (remove the old public key from `MUSTER_BACKUP_AGE_RECIPIENTS` by hand if you
  do not want backups to keep including it). A new key is used for backups made
  after it; the drill fails until one exists, as described above. On a new host
  after a total loss the same thing happens: the old public key stays listed (it
  is harmless) and a new key joins it.

### Recover for real

**Same host, data damaged (authority intact):** start the maintenance mode
yourself, because nothing else does it on this path:
`sudo muster-cloud-autodeploy --hold` (it waits for any run in flight and prints
`held`; the guard and the deploys stay stopped until `--release` at the end of
step 6), then go straight to step 4.

**New host after total loss:**

1. Do the bootstrap, steps 0 to 5, on the new host (including `host-setup.sh`;
   the same `ops.env`, `kamal.env` and `deploy.env`; `MUSTER_BACKUP_AGE_RECIPIENT`
   must stay the same key; step 5 starts the empty database). `host-setup.sh`
   creates `guard-only` on this fresh host by itself (step 4). Add the **hold**
   on top: `sudo muster-cloud-autodeploy --hold` (prints `held`) and keep it until
   the cutover below is done: `hold` also stops the guard, which would otherwise
   start containers you are about to replace. Do **not** run `init`.
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
   deleted after the backup. The script runs as root but writes the authority
   (the journal, the lock and the replaced witness) **as the owner of the control
   directory**, the service's uid 10001 (`setpriv`), so those files are created
   with the right owner and nothing is chowned afterwards. It refuses (exit 73),
   before it creates anything, a control directory owned by root and a witness
   file whose directory has another owner than the control directory, so a
   mistyped `--control-witness` can never rewrite an unrelated file. It also
   hands the new repository root (empty, `0700`) to that uid and the restored
   repositories keep the owner they had in the backup (10001). Nothing is left to
   fix by hand; just confirm:
   ```
   sudo ls -ln /srv/muster-cloud/repos-restored | head -3             # owner 10001 10001
   sudo ls -ln /srv/muster-cloud/control /var/lib/muster-witness/w    # everything 10001 10001, -rw-------
   ```
6. Cut over, deliberately, with the hold still in place (`--hold`: nothing
   starts, stops or deploys while you work):
   1. Activate the restored database. `activate` reads the database URL from the
      environment, like `restore-check`, so the password never appears in the
      process list; put the URL of the **new** database in a root-only file
      without echoing it, and add the file after `ops.env` (a later
      `EnvironmentFile` wins):
      ```
      sudo bash -c 'umask 077; read -rsp "new database URL: " u; echo; printf "MUSTER_DATABASE_URL=%s\n" "$u" >/dev/shm/new-db.env'
      sudo systemd-run --pipe --wait --collect -p User=muster-cloud -p Group=muster-cloud -p UMask=0077 \
        -p EnvironmentFile=/etc/muster-cloud/ops.env -p EnvironmentFile=/dev/shm/new-db.env \
        /usr/local/bin/muster-cloud-ops activate --incarnation <id printed by the restore>
      sudo rm -f /dev/shm/new-db.env
      ```
      (The URL has the form `postgres://muster:<password>@localhost/<new db>?host=/srv/muster-cloud/pgsocket`.
      This fences the running service, if any: it starts refusing requests. The
      old `--database-url` flag still works but prints a warning, because it puts
      the password in the process list.)
   2. Move `/srv/muster-cloud/repos` aside and put the restored repository root in
      its place (owner `10001:10001`, mode `0700`). You do not need to stop the
      service container: it is fenced, and the redeploy below replaces it.
   3. Update the database name in `MUSTER_DATABASE_URL` in both `kamal.env` and
      `ops.env`.
   4. Redeploy, release the hold and verify, in this order:
      ```
      sudo rm -f /var/lib/muster-cloud/autodeploy/guard-only    # new host only: host-setup put it there; it would also block the redeploy
      sudo touch /var/lib/muster-cloud/autodeploy/redeploy
      sudo muster-cloud-autodeploy --release                    # prints "released"
      sudo systemctl start muster-cloud-autodeploy.service      # do not wait 5 minutes
      journalctl -u muster-cloud-autodeploy -n 40 --no-pager
      curl -fsS https://personal.tailbb30e7.ts.net:10000/readyz
      ```
      `redeploy` reboots the version that is routed now with the new settings: on
      the same host it needs neither GitHub nor CI and never moves to a newer
      commit; it stops the fenced container once the new one is routed and ready.
      (On a new host nothing is routed yet, so the run deploys the HEAD of `main`
      as usual, which does need GitHub, its green CI and the public image.) The
      cutover is done when `/readyz` answers 200.
7. Delete the age key from the RAM disk (`shred -u /dev/shm/muster-backup.key`),
   run the backup once, and check `muster-cloud-backup-check` is quiet. The
   next monthly drill (or `sudo systemctl start muster-cloud-restore-drill.service`
   once a backup made after the restore exists) proves the new host can recover too.

What you lose: everything after the snapshot (up to 6 hours of data), plus
everything the journal says was deleted stays deleted.

## Risks you accept with this setup

- **Rollback of everything with Drive access plus the age key.** The authority
  copy on Drive is what protects a restore from a stale or forged database. Whoever
  has both write access to the Drive folder and the age private key can upload an
  older authority copy together with an older snapshot, and a recovery would
  roll the whole deployment back to that point without anything detecting it.
  The main age key is not on the host, and Drive is not writable from anything
  but root's `rclone` remote; but both are single points of trust, and the system
  does not defend against someone who holds both. Since the monthly drill, the
  drill-only age key also lives on the host (root `0400`), and a backup made
  after it was added is encrypted to it too, so **root on this host now holds both
  halves** for everything made since then: Drive access and a key that opens it.
  That means a compromised root can read recent Drive backups (including
  tenants deleted within the retention window) and can rewrite what Drive holds,
  not only damage the live data. Root could already do all of that to the live
  data; the drill key extends it to the retained history. The main key, the one
  that restores a new host, is still off the host.
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
