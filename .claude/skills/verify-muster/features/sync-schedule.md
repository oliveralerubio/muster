# Sync, status and schedule

Keep this machine aligned with the config repo: `sync` pulls/pushes (when a remote exists) and applies desired
state as a numbered generation; `status` shows fleet and last sync; `schedule` runs sync periodically.

## Sub-features

- Sync (`muster sync`, `POST /api/sync`), dry-run, with `sync-status.json` and `sync.lock` in state dir.
- Status (`muster status`, `GET /api/status`, dashboard) including `sync` last result and drift.
- Schedule `enable|disable|status` (systemd user timer, launchd agent, or manual instructions).

## How to get to it (user POV)

Dashboard shows last sync and drift; CLI `muster sync`, `muster schedule enable --interval 30`.

## Driving it with muster-harness

```bash
$H launch $RUN
$H tree $RUN before
$H cli $RUN -- --format json sync --dry-run 2>/dev/null | jq -c '.result|{dry_run,applied,changes}'
$H tree $RUN after-dry                      # identical to before => dry-run really wrote nothing
$H cli $RUN -- --yes sync
cat /tmp/muster-verify/$RUN/home/.local/state/muster/sync-status.json
$H api $RUN GET status                      # result.sync.result == "ok"
$H cli $RUN -- schedule status
$H cli $RUN -- --dry-run schedule enable --interval 30 --backend systemd   # prints the unit paths it WOULD write
$H cli $RUN -- --yes schedule enable --interval 30 --backend manual        # writes nothing, prints cron hint
$H tree $RUN after-schedule
```

Observable end state: `sync-status.json` = `{"ts":...,"result":"ok","applied":...}`, `sync.lock` exists,
`GET /api/status` `result.sync` matches it, `generations/` gained an entry on the first applying sync;
`--dry-run` leaves the file tree byte-identical; schedule paths printed are under the sandbox
`home/.config/systemd/user/`.

## Gotchas

- `schedule enable --backend systemd|launchd` WRITES real unit files (systemd also runs `systemctl --user`).
  The sandbox HOME/XDG redirects the unit files, but `systemctl --user` talks to the real user manager and
  `launchctl` to the real launchd. Verified here only with `--dry-run` (and `--backend manual`, which writes
  nothing). Do not run a real enable unless you will disable it and confirm the real unit is gone.
- `schedule enable --dry-run` still reports `"enabled": false`; judge by the printed `files`.
- `schedule enable` embeds the absolute path of the binary under test and `--config` of the sandbox: a
  timer that outlives cleanup would point at a deleted directory.
- With no git remote, sync only applies locally (`applied` false when nothing changed).
- `status` right after init shows drift `profile_not_applied` until the first apply.
