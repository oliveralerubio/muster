# Dashboard UX audit (round 0, orchestrator)

Evidence gathered on 2026-11-04 against Oliver's production panel (`muster ui` on his PC,
real config at `~/.config/muster`). All 19 GET routes answer 200 while the server is alive.

## P0 — "Muster is not reachable. Is `muster ui` still running?"

Root cause (src/web/mod.rs `serve`, `Ui::reply`; src/web/assets/app.js `api`):
- `muster ui` is a foreground process on a random port (`--port` default 0). When its
  terminal closes, the tab Oliver keeps open points at a dead port → "not reachable".
- The login token is one-time and sessions live only in memory. Restarting `muster ui`
  gives a new port + new token, so the old tab can never recover; opening a second tab or
  reloading after a restart yields `401 unauthorized: open the URL printed by muster ui`.
- The page never retries: one failed fetch leaves a dead view until a manual reload.

Required outcome: Oliver never has to babysit the panel.
- Stable default port (keep `--port` to override; `0` still means random).
- Single instance: running `muster ui` while one is already serving that config must not
  fail with "address in use"; it must hand the browser a fresh login for the running
  instance (e.g. the new invocation writes a one-time token to a 0600 file under the
  state dir that the running server also accepts, then opens/prints the URL).
- Sessions survive a server restart on the same port (persist the session secret in the
  state dir, 0600), so an open tab recovers on its own once the server is back.
- Front end: on network failure show a non-blocking "reconnecting…" state, retry with
  backoff, and reload the current view automatically when the server answers again.
  Keep the 401 path distinct ("Session expired — run `muster ui`").
- Do not weaken the security model: loopback only, Host/Origin checks, HttpOnly
  SameSite=Strict cookie, constant-time compares, token files 0600.

## P1 — real bugs seen in the browser

1. Doctor is red on every Codex install: `src/adapters/codex.rs` sets
   `min_version: "1.0.0"` but current Codex is `0.166.0`. Use a realistic minimum (Codex
   still versions 0.x). Check the other adapters' minimums against what is installed
   (`claude --version`, `codex --version`, `opencode --version`, `pi --version`).
2. "CLI equivalent" footer shows the wrong command: on Dashboard it shows
   `muster skill list`, on Members & groups `muster group list`. `showCli` is called by every
   `api()` response, so the last parallel/background request wins (the skills dot refresh,
   the second of two parallel loads). The footer must show the command of the view (or of
   the last user action).
3. Skills table shows both "Disable" and "Enable" for every skill and never says whether
   the skill is enabled. Show state; offer only the applicable toggle.
4. Machines view and Dashboard fleet table say "Last sync: never" for the current machine
   while the Last sync card says "10:47 PM ok". Find which field is wrong and make them
   agree.
5. Raw machine values leak to the UI: nanosecond ISO timestamps
   (`2026-11-02T17:10:17.900229397Z`) in Skills/Secrets/Usage, and raw drift reason codes
   (`machines_drifted`, `profile_outdated`). Format timestamps consistently (the dashboard
   card already does) and label reasons.

## P2 — CLI capabilities with no way to do them from the dashboard

Routes already exist in `ROUTES` but no UI calls them (dead backend):
- Members: `member/add`, `member/role`, `member/profile`, `member/rm` (table has an empty
  Actions column).
- Groups: `group/create`, `group/grant`, `group/revoke`.
- Machines: `machine/assign` (assign a profile; Role/Profile columns are empty).
- Providers: `provider/use` (make default).
- Secrets: `secret/rm` (delete a secret).
- Proposals: `proposals`, `proposal/create`, `proposal/accept` (no view at all).
- `why` (find text in agent context) — no UI.

Missing routes + UI:
- **Edit skills** (Oliver asked for this explicitly): view and edit a skill's `SKILL.md`
  (and other files in the skill folder) from the panel, save, then the skill shows as
  modified / can be pinned. Needs read/write routes confined to the skills directory
  (reuse the traversal guard in `src/skills/lock.rs`), no symlink escape, size limit.
- `skill.pin` (approve an own skill's current contents).
- `secret.check`.

Acceptable as terminal-only (state it in the UI where relevant, don't build): `init`,
`enroll`, `provider login` (interactive OAuth), `agents install`, `import.*`, `uninstall`,
`mcp`, `ui`, `schema`, `group rekey` (decide; if trivial add it).

## Out of scope
Visual redesign (done yesterday in #66). Keep the existing visual language; new controls
reuse existing helpers (`button`, `act`, `table`, `field`, `toast`, `busy`).
