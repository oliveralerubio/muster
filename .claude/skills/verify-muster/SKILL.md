---
name: verify-muster
description: Drive and verify the REAL muster app (Rust CLI plus the `muster ui` localhost web panel) in an isolated sandbox and capture proof. Use when asked to run muster, confirm a change works end to end, reproduce a CLI or web-panel behavior, or take a screenshot of the panel. Not for unit tests (use cargo test).
---

# verify-muster

Muster syncs machines, AI-agent configuration and secrets. Two surfaces: the CLI (`muster ...`) and the web
panel (`muster ui`, 127.0.0.1 only, JSON API under `/api/`). Everything below runs the real binary in a
throwaway HOME so your real `~/.config/muster` is never touched.

All commands use the helper `H` (python3 stdlib only). Set it once per shell, from anywhere in the repo:

```bash
H="$(git rev-parse --show-toplevel)/.claude/skills/verify-muster/harness/muster-harness"
RUN=v-$(date +%s)        # pick a unique run id; reuse it for every command of one verification
```

Layout: scratch `/tmp/muster-verify/$RUN/` (deleted by cleanup), evidence `/tmp/muster-verify-evidence/$RUN/`
(never deleted by the harness; do not delete it either). Override roots with `MUSTER_VERIFY_ROOT` /
`MUSTER_VERIFY_EVIDENCE_ROOT`; use a prebuilt binary with `MUSTER_BIN=/path/to/muster`. Different run ids
are fully independent (UI uses port 0), so parallel runs are safe.

## Launch

```bash
$H launch $RUN
# READY run=<id> pid=<pid> port=<port> muster 0.1.0 scratch=... evidence=...
```

`launch` does: `cargo build` (needs cargo; `source ~/.cargo/env`) unless `MUSTER_BIN` is set, makes an empty
sandbox (`HOME` and `XDG_{CONFIG,STATE,DATA,CACHE}_HOME` under scratch, `PATH=<scratch>/bin:$PATH`, every
`MUSTER_*` var unset, stdin not a TTY), runs `muster --yes init`, starts `muster ui --no-open --port 0` in its
own session, records `ui.pid`/`ui.port` and a `meta.json`, exchanges the one-time token for the session cookie
(`cookie` file), and only prints READY after `GET /api/status` returns `ok:true`. Port-open alone is not
readiness. Nothing needs a git identity or age key.

Teardown is `cleanup` (see below). If `launch` fails it removes its own half-built scratch where it can; run
`$H cleanup $RUN` anyway.

## Doctor

One read-only health check; run it after launch and whenever something looks odd:

```bash
$H doctor $RUN        # exit 0 = all PASS
```

Checks: our PID alive and really our `muster ... ui` process; the port is listened on by that PID; binary runs
(`muster --version`); git HEAD unchanged since launch; binary not older than `src/`, `Cargo.*` (rebuild and
relaunch if FAIL); session cookie valid via `GET /api/status`. A FAIL on stale-binary or HEAD means the app
you are looking at is not the code you think it is: stop, rebuild, relaunch.

## Drive

### CLI

```bash
$H cli $RUN -- --format json secret list                  # stdout = JSON envelope only
$H cli $RUN --stdin 'verify-dummy-123' -- --yes secret add verify/api --value-stdin
```

Runs the sandboxed binary, prints stdout/stderr unchanged, and writes a numbered evidence file
(stdin never logged; `--reveal` output withheld; JSON keys named value/plaintext/secret/password redacted).
The footer `[exit=N evidence=...]` goes to stderr, so `2>/dev/null | jq` works. Global flags go before or
after the subcommand: `--format text|json` (there is no `--json`), `--yes` (required without a TTY for any
confirmation), `--dry-run`, `--quiet`, `--verbose`, `--config`. JSON envelope:
`schema_version, command, ok, result, diagnostics`. Never pass secrets as arguments; use `--stdin`.

### HTTP (web API)

```bash
$H api $RUN GET secrets
$H api $RUN POST secret/add '{"path":"verify/api","value":"verify-dummy-123","confirm":true}'
$H api $RUN POST secret/rotate '{"path":"verify/api","value":"verify-dummy-456","confirm":true}'
$H api $RUN POST skills/check '{}'
echo '{"name":"verify/api","confirm":true}' | $H api $RUN POST secret/reveal -   # body from stdin or @file
```

Exit 0 on 2xx. The helper handles the session cookie and `Host` header. Rules: POST bodies are JSON, at most
64KB; `confirm:true` stands for `--yes` and mutating routes without it answer 400; `dry_run:true` stands for
`--dry-run`. Every response carries `cli`, the equivalent CLI command, which is a good cross-check.
Routes (prefix `/api/`): GET `status why effective machines members groups profiles providers profile
secrets secret/requests secret/leases plan proposals mcp-servers schedule doctor skills usage`;
POST `doctor sync rollback approve revoke machine/assign machine/rm member/{add,approve,reject,role,profile,rm}
group/{create,grant,revoke} secret/{add,rotate,rm,reveal,approve,deny,revoke} proposal/{create,accept}
skills/{check,add,update,rm} schedule/{enable,disable} provider/{add,rm,use} mcp-server/{add,rm}
profile/{set,rm} enable disable`. Secret add/rotate bodies: `{path|(group,name), value, confirm}`; reveal:
`{name, confirm}`. Mutating routes need `confirm:true`; `enable`/`disable` take `{item:"kind:name[@adapter]"}`.
Authoritative list: `ROUTES` in `src/web/mod.rs`; reference client: `tests/test_ui.rs`.
Raw curl, if you must: `GET /?token=<T>` once, read `Set-Cookie: muster_session=...`, then send
`Cookie:` and `Host: 127.0.0.1:<port>` on every call.

### Browser (headless Chrome, real panel)

```bash
$H screenshot $RUN secrets secrets-view.png        # view = dashboard|machines|team|providers|secrets|skills|effective|profiles|history|doctor
$H screenshot $RUN secrets after.png --eval "document.querySelector('#view button').click()"
```

The token printed by `muster ui` is ONE-TIME and `launch` already spent it for the API cookie. So
`screenshot` starts a second short-lived `muster ui` against the same sandbox (fresh token, own PID file
`ui-shot.pid`), drives Chrome over the DevTools protocol (own minimal websocket client, no pip), clicks the
`#nav [data-view=VIEW]` tab, saves the PNG plus `<name>.txt` holding the visible `#view` text, then stops
Chrome and that UI. Relative output names land in the evidence dir. Finds `google-chrome`, `chromium` or
macOS Chrome; override with `CHROME_BIN`. Adds `--no-sandbox` only when running as root. Read the PNG to
actually look at it. Handles for `--eval`: `#nav` tabs, `#title`, `#actions`, `#view`, `#cli`,
`#secret-path/#secret-input/#secret-confirm/#secret-save`, `#reveal-*`.

### State inspection

```bash
$H tree $RUN before          # sha256/size of every config+state file, saved as evidence (.git excluded)
$H tree $RUN after           # diff the two to prove which files changed
$H leakcheck $RUN verify-dummy-123    # exit 1 if the string appears in any sandbox file
```

Sandbox paths: config `/tmp/muster-verify/$RUN/home/.config/muster` (muster.toml, vault/<group>.age +
.meta.json, skills.lock, skills/, proposals/); state `.../home/.local/state/muster` (identity.txt,
history.jsonl, generations/, sync.lock, sync-status.json); cache `.../home/.cache/muster/skills-status.json`.

## Evidence

A claim counts only with all of these:

1. Real user path: the actual binary or panel, not a unit test or a mock. Say which command or view.
2. Action AND resulting state: the response, plus the file or API state it should have changed
   (`$H tree` before/after, `GET /api/...`, files in agent dirs).
3. Side effects observed, not assumed: e.g. `vault/<group>.age` bytes changed, a `history.jsonl` line with the
   right `action` and `subject`, `sync-status.json` written.
4. Plaintext never leaks: `$H leakcheck` clean for any secret value used (history, responses, vault, evidence).
   Only `secret/reveal` may return a value, and it must leave a `secret.reveal` history line.
5. Dry-run claims verified by observation: run `$H tree` before and after a `--dry-run` and show nothing
   changed; do not trust the flag name (e.g. `schedule enable --dry-run` still writes nothing but prints files).
6. Negative case when cheap (missing `confirm` gives 400; bad input gives exit 2).
7. A screenshot only proves rendering; pair it with the API/CLI state.

Use dummy values only (`verify-dummy-123`), never real secrets. Report exact commands and results; a step you
skipped or that failed is reported as such, never softened.

## Cleanup

```bash
$H cleanup $RUN               # stops ONLY pids recorded in our pidfiles, checks port closed, removes scratch
$H cleanup $RUN --keep-scratch   # when you still need to inspect files
ls /tmp/muster-verify-evidence/$RUN                # evidence must still be there
```

It refuses to signal a pid that is no longer our `muster ui` (pid reuse), sends SIGINT, then SIGKILL, and never
touches evidence or any process it did not start. Run cleanup after every attempt, including failed ones.
Also remove any extra scratch you made yourself (e.g. a local upstream git repo).

## Helpers (summary)

| Invocation | Purpose |
|---|---|
| `$H launch RUN` | build, sandbox, init, start UI, log in, print READY |
| `$H doctor RUN` | read-only health check |
| `$H cli RUN [--stdin TXT\|-] -- ARGS` | run the CLI in the sandbox |
| `$H api RUN GET\|POST PATH [JSON\|@file\|-]` | call the web API |
| `$H screenshot RUN VIEW OUT.png [--eval JS]` | headless Chrome screenshot of a panel view |
| `$H tree RUN [label]` | hash sandbox files into evidence |
| `$H leakcheck RUN NEEDLE` | assert a string is absent from sandbox files |
| `$H mcp RUN initialize\|tools-list\|tools-call NAME [JSON]` | one JSON-RPC 2.0 message to `muster mcp` over stdio (tool envelope; exit 1 on JSON-RPC error). stdout prints the REAL reply — a successful `secret_reveal` shows the value; the evidence file gets the redacted form and never holds plaintext (failed reveals keep their diagnostics in evidence) |
| `$H cleanup RUN [--keep-scratch]` | stop our processes, remove scratch, keep evidence |

## Feature journeys

Index: `features/README.md`. Read the one matching your change; each ends in an observable state that proves it.
