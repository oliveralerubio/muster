# Network use and API key checks

Muster sends no telemetry: no analytics, crash uploads, background update
checks, usage reports or machine identifiers to a Muster service. Story 71
does not prohibit the network operations requested by the user below.

## Opt-in key health (story 39)

`muster doctor --check-keys` checks `OPENROUTER_API_KEY` from the process
environment with `GET https://openrouter.ai/api/v1/key`. This is a read-only
metadata request, with no model inference or test purchase. It is disabled
by default. `--offline` skips this optional check even with `--check-keys`.
An absent/empty key, missing `curl`, connection failure or timeout also
reports the `api_keys` check as `skipped`, without failing local diagnostics.

Only OpenRouter is supported. Keys stored under arbitrary vault references
are not guessed or sent anywhere. Existing secret-provider checks still
check availability/resolution; they do not validate an AI provider key.
`--offline` controls the optional API key request, not the network behavior
of external secret-provider CLIs configured by the user.

The documented endpoint supplies `limit`, `limit_remaining` and `expires_at`:

- [Current API key metadata](https://openrouter.ai/docs/api/api-reference/api-keys/get-current-key)
- [Authentication and key limits](https://openrouter.ai/docs/api/reference/authentication)

Rejected (401/403), expired and exhausted keys produce errors. A key with at
most 10% of its spending limit remaining produces a warning. Unlimited keys
have no spending-limit warning. Rate limiting (429), other HTTP errors and
unusable metadata produce warnings. This reports provider metadata at check
time; it does not guarantee that a later inference request will succeed.

The request uses `curl` with a 2-second connection timeout, a 5-second total
timeout and a 16 KiB body limit, also enforced while reading the child pipe
so chunked responses are bounded on older curl versions. It disables curl's
config file and URL globbing, requires HTTPS for the official endpoint, and does not follow
redirects. Proxy/TLS configuration from the environment still applies.
The authorization header is passed through stdin, never argv or a file;
the key is removed from the child environment. Neither response bodies nor
curl stderr enter diagnostics. Secret buffers are cleared on drop, and
the raw response buffer is cleared after inspection.

For local integration tests, `MUSTER_OPENROUTER_KEY_CHECK_URL` accepts an
HTTP URL whose authority parses as a numeric loopback socket address
(for example `http://127.0.0.1:12345/api/v1/key`). Other overrides are rejected
before invoking curl; loopback requests bypass proxies. Set this override
only for tests: it receives the authorization header. Tests use synthetic
credentials and a local HTTP server; they never contact OpenRouter.

## Source audit (stories 35, 39 and 71)

Audit scope: Rust sources, embedded web assets, `Cargo.toml`, `Cargo.lock`,
installation and maintenance scripts, and CI workflows. Runtime outbound
paths and related local operations:

| Source | Purpose and trigger | Destination/data |
| --- | --- | --- |
| `src/git.rs` (`push`, `pull`, `shallow_fetch`) | Config transport on `sync`; enrollment/approval/rejection/revocation and team changes can push to a configured remote. Skill add by URL, check and update shallow-clone/fetch explicitly configured sources through `src/skills/upstream.rs`. An enabled schedule runs sync. | User-selected Git remotes, via Git's configured transport (including HTTPS/SSH). Config repository and encrypted vault; skills use their selected upstreams. |
| `src/installer.rs` (`fetch`) | `agents install` downloads missing/changed artifacts pinned in `muster.lock` and checks SHA-256 before installing. Dry-run may download for verification. | User-pinned HTTPS URLs via curl, including redirects. `file://` is read locally. |
| `src/providers/cli.rs` (`Cli::run`, `bws_list`) | Optional `op`, `bws`, `doppler` and `infisical` commands resolve explicitly configured secret references; `import bws` imports secrets. Resolution can occur during plan, apply, sync, doctor or explicit secret reads. | Vendor CLIs control their own endpoints, authentication, caching and network behavior. Native and environment providers are local. |
| `src/doctor/api_keys.rs` (`check`) | Explicit `doctor --check-keys`, unless `--offline`. | OpenRouter key metadata, as described above; no telemetry. |
| `src/web/mod.rs`; `src/web/assets/app.js` | `ui` binds to `127.0.0.1`; browser fetches relative `/api/` routes. `--open` launches the browser on the local URL. Routes call the same commands and may trigger the operations listed above. | Local HTTP only for the panel itself; embedded HTML, CSS and JS have no remote asset or analytics imports. |
| `src/commands/secret.rs` (`prompt_secret_gui`, `run_with_secrets`) | `secret prompt` (and `--prompt` on `secret add`, `secret rotate`, `provider add`) opens a local graphical password dialog or binds an ephemeral `127.0.0.1:0` prompt page for a single write-only submission; `secret run` executes a user-specified command with authorized secrets injected. | Local GUI / `127.0.0.1` loopback only for secret capture; `secret run` executes the caller-supplied local command. |
| `src/machine.rs`; `src/commands/init.rs`; `src/commands/schedule.rs` | Local hostname, agent `--version`, Git initialization, systemctl/launchctl activation. | No intentional outbound request; third-party executables govern their own behavior. |

`src/mcp.rs` uses stdio, not a network listener. MCP callers can explicitly
opt in to the registered doctor key check. Agent MCP URLs/hooks and the
OpenCode schema URL are written into agent configuration; Muster does not
fetch them or execute hooks. The launchd DTD URL is a format declaration.
There is no separate DNS lookup, socket client, Rust HTTP client or
telemetry SDK in the audited source/dependency set; name resolution for
outbound requests belongs to Git, curl or the vendor CLI.

Outside the installed binary, `install.sh` uses curl or wget to resolve the
latest release and download the platform tarball and `SHA256SUMS` from
GitHub (`MUSTER_REPO`/`MUSTER_VERSION` select the repository/version). It runs
only when invoked and installs after checking the checksum. It does not
add a background updater. `.github/workflows/` downloads build tools/dependencies/actions, exchanges
GitHub caches/artifacts, runs the secret scanner and publishes releases and
provenance. These are build/release operations, not runtime reporting from
users' installations.

## Regression checks

`tests/test_doctor_api_keys.rs` guards the default/offline paths against
network requests and curl execution, tests local provider responses,
rejected endpoint overrides, timeouts and credential redaction. These
tests are a focused guard, not an operating-system network sandbox. When
adding an outbound path, update this inventory and retain explicit user
intent; do not add automatic reporting on command startup.

To repeat the source audit:

```sh
rg -n 'curl|wget|reqwest|ureq|TcpStream|UdpSocket|ToSocketAddrs|https?://|Command::new|telemetry|analytics|sentry|opentelemetry' src Cargo.toml Cargo.lock
rg -n 'fetch\(|XMLHttpRequest|WebSocket|EventSource|sendBeacon|src=|href=' src/web/assets
rg --hidden -n 'curl|wget|https?://|gh (api|run|workflow|variable)|uses:' install.sh scripts .github
```

Story 35 is covered by
`rotated_secret_reaches_authorized_machine_on_next_sync_only` in
`tests/test_enroll_revoke.rs`: two authorized machines and an unauthorized
machine share a bare Git remote. The second machine resolves the old value
until sync, then resolves the new value; the unauthorized machine receives
the same ciphertext and fails to decrypt it.
