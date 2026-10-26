# Muster

> Sync your machines, AI coding agents and secrets — local-first, team-ready.

Muster keeps the configuration of your AI coding agents identical across every
machine you own (and every member of your team): instructions, MCP servers,
models, permissions, hooks, Agent Skills, subagents and the secrets they need, all in one
Git repository you control. Each machine decrypts secrets with its own key; no
accounts, no third-party servers. Git is the only required dependency (`curl`
is needed only when you let Muster download an agent CLI for you).

## Supported agents

One canonical configuration is rendered into each agent's native files.
Capabilities are as implemented in v1; unsupported features are skipped with a
warning, never silently lost.

| Agent | Binary | Capabilities | Skills — user scope | Skills — project scope |
|---|---|---|---|---|
| Claude Code | `claude` | instructions, MCP (stdio + remote), models, permissions, hooks, skills, subagents | `~/.claude/skills/<skill>/` | `.claude/skills/<skill>/` |
| Codex | `codex` | instructions, MCP (stdio + remote), models, skills, subagents | `~/.agents/skills/<skill>/` | `.agents/skills/<skill>/` |
| OpenCode | `opencode` | instructions, MCP (stdio + remote), models, skills, subagents | `$XDG_CONFIG_HOME/opencode/skills/<skill>/` | `.opencode/skills/<skill>/` |
| Antigravity CLI | `agy` | instructions, MCP (stdio + remote), skills, subagents | `~/.gemini/antigravity-cli/skills/<skill>/` | `.agents/skills/<skill>/` |

- Instructions are written once in `AGENTS.md`; each agent gets its native copy (Claude Code's `CLAUDE.md` imports it through a managed block).
- MCP servers, models, permissions and hooks are merged into the agent's own config files, rewriting only the keys Muster manages.
- Project scope comes from `<project>/.muster/muster.toml`. Codex and Antigravity share `.agents/skills` in a project; the file is written once.
- Skills removed from the source are deleted on the next apply; skills you add by hand are left alone.

## Agent Skills

Declare a skills folder in `muster.toml` (`[agents.skills] source = "skills"`), one subfolder per skill with a `SKILL.md` (YAML frontmatter with `name` and `description`) plus any extra files. Muster verifies each skill against `skills.lock`, then installs its reviewed files byte for byte into the skill folders listed in the table above. Missing or mismatching pins produce a warning and keep the previously installed copy; other configuration and verified skills still sync. Empty directories are ignored. A skill without `SKILL.md` or without `name`/`description` is skipped with a `skill_invalid` warning.

### Skill lifecycle (own and subscribed skills)

Write your own skills in the config repo, or subscribe to an external Git repository of skills. Subscribed skills are copied (vendored) into the skills folder under `skills/<name>/`, so every machine gets them through the normal sync, even offline.

```toml
[agents.skills]
source = "skills"

[[agents.skills.remote]]
name = "anthropic"                         # source id
url = "https://github.com/org/skills"      # any git URL
ref = "main"                               # optional branch or tag
path = "skills"                            # optional folder inside the repo
include = ["pdf"]                          # optional: only these skills
```

`skills.lock` (next to `muster.toml`, committed to Git) records for each skill its `origin` (`local` or the source id), upstream `commit`, `content_hash` (`sha256:` over its files), `version` (from `SKILL.md` frontmatter) and `updated_at`.

| Command | What it does |
|---|---|
| `muster skill list` | List every skill with origin, version, last update and status. Offline. |
| `muster skill add <name> [--description <text>]` | Scaffold an own skill at `skills/<name>/SKILL.md`. |
| `muster skill add --url <git-url> [--ref <ref>] [--path <dir>] [--include <skill>]` | Subscribe to a Git repository of skills and vendor them. |
| `muster skill pin <name>` | Record the reviewed files of an own skill in `skills.lock` after editing. |
| `muster skill check [name]` | Query upstream repositories for updates, deprecations and removals. |
| `muster skill update [name] [--all] [--force]` | Apply upstream changes, showing files added, changed or removed (`--force` overwrites local edits). |
| `muster skill rm <name> [--detach]` | Delete a skill, or with `--detach` keep a subscribed skill as your own. |

After editing an own skill, review it, run `muster skill pin <name>`, and commit both its files and `skills.lock`. `skill add` records the initial pin; subscribed skills continue to use `skill update`. Without the pin step, edits do not spread on sync.

Every command accepts `--format json`, `--dry-run` and `--yes`.

Statuses: `local`, `up_to_date`, `update_available`, `deprecated` (upstream `SKILL.md` has `deprecated: true` or `deprecated: "<reason>"`), `upstream_removed`, `unknown` (not checked yet, or the source could not be reached). Nothing is applied or deleted on its own: updates need `muster skill update`, and a skill removed upstream stays until you run `muster skill rm` (with or without `--detach`). A skill edited by hand shows `modified` and `update` refuses to overwrite it without `--force`. In a team, only admins add, update or remove skills; members can list and check.

The web admin panel (`muster ui`) includes a **Skills** tab showing installed skills, upstream status, and an attention indicator when updates are available, with buttons to check sources and update a skill.

## Subagents

Declare a subagents folder in `muster.toml` (`[agents.subagents] source = "subagents"`), one Markdown file per subagent (`<name>.md`) with YAML frontmatter (`name` and `description` required, optional `tools` and `model`) and the body as the system prompt. The frontmatter is parsed as YAML (block scalars, quoted strings and list-form `tools` all work). `name` must match `[a-z0-9][a-z0-9._-]*` (no `/`, `\`, `..` or leading dot) and equal the file stem. Muster installs the file byte for byte where that is the agent's native format, and translates it where it differs:

| Agent | User scope | Project scope |
|---|---|---|
| Claude Code | `~/.claude/agents/<name>.md` | `.claude/agents/<name>.md` |
| Codex | `~/.codex/agents/<name>.toml` | `.codex/agents/<name>.toml` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/agents/<name>.md` | `.opencode/agents/<name>.md` |
| Antigravity CLI | `~/.gemini/antigravity-cli/agents/<name>.md` | `.agents/agents/<name>.md` |

Codex gets a TOML custom-agent file (`developer_instructions`); OpenCode gets a Markdown agent file with `mode: subagent` (the file name is the agent name); generated frontmatter is serialized as proper YAML/TOML, so descriptions with colons, quotes or line breaks stay valid. Fields a target cannot express (`tools`, and any other non-canonical frontmatter key, for Codex and OpenCode) are skipped with a `subagent_field_untranslated` warning. Subagents removed from the source are deleted on the next apply; files you add by hand are left alone. A file without valid frontmatter (missing fields or malformed YAML) is skipped with a `subagent_invalid` warning.

## Platforms

Linux, WSL2 and macOS. Native Windows is not supported.

## Install

**Option 1 — install script (recommended):**

```sh
curl -fsSL https://raw.githubusercontent.com/oliveralerubio/muster/main/install.sh | bash
```

The script is tested in CI. It detects
your OS and architecture (Linux and macOS, x86_64 and aarch64), downloads the
release tarball, verifies the tarball's SHA-256 checksum against `SHA256SUMS`,
and installs the binary to `~/.local/bin`, telling you how to extend your `PATH`
if the directory is not on it.

Override defaults with optional environment variables:
- `MUSTER_VERSION`: release tag to install, e.g. `v0.1.0` or `0.1.0` (default: latest published release).

No release has been published yet, so until the first tagged release the script
has nothing to download; build from source in the meantime.
- `MUSTER_INSTALL_DIR`: directory where the executable will be placed (default: `~/.local/bin`).
- `MUSTER_REPO`: GitHub repository to download from (default: `oliveralerubio/muster`).

**Option 2 — from source with Cargo:**

```sh
git clone https://github.com/oliveralerubio/muster
cd muster
cargo install --path .
```

Requires a Rust toolchain and Git. You get a single executable — no interpreter
or runtime to install — dynamically linked against your system libraries (glibc
on GNU/Linux), like any standard Rust build.

### Pinned agent CLIs

After enrollment is approved, `muster sync --yes` installs agent CLIs pinned in
`muster.lock` into `~/.local/bin`, alongside configuration, secrets and verified
skills. Make sure that directory is on `PATH`. Preview with `muster plan` or
`muster sync --dry-run`; neither downloads artifacts. Pins are written by a
person, not resolved automatically from release feeds:

```toml
[agents.claude]
version = "1.2.3"

[agents.claude.artifacts.linux-x86_64]
url = "https://example.com/claude-1.2.3-linux-x86_64"
sha256 = "<64 hexadecimal characters from the trusted release>"
```

Artifact keys follow the machine platform reported by Muster (for example,
`linux-x86_64` or `macos-aarch64`). Legacy pins with `url` and `sha256` directly
under `[agents.claude]` still work. No artifact for this platform means a warning,
not an attempted install. You can also use `muster agents install [--agent claude]`
without applying configuration.

Interactive sync asks before installing a new pinned version and shows the old
and new versions, URL and checksum. Without a terminal and without `--yes`, sync
never changes agent binaries, even previously approved ones: it applies the rest and
records approval needed. New hooks or MCP servers still refuse unattended sync
as before. A failed download or checksum keeps the previous binary, lets the
rest of sync continue and returns a partial-failure exit code (1). Check
`muster status` for versions, skipped skills and pending approvals. Rollback
restores managed files only; it never downgrades agent binaries.

## Quick start

```sh
muster init          # config repo, this machine's identity, recovery key, empty vault
muster agents apply  # write the canonical config into each enabled agent's files
muster ui            # web admin on 127.0.0.1 with a one-time login token
```

1. **`muster init`** creates your config repository (`$XDG_CONFIG_HOME/muster` by default), generates this machine's identity and an empty age vault, and shows your recovery key once — store it somewhere safe.
2. **`muster agents apply`** renders your configuration into the native files of every agent in `[agents].enabled` — by default every stable adapter (the four in the table above), whether or not its binary is installed on this machine. List the agents you want in `muster.toml` to restrict the set; preview first with `muster plan` if you want the exact diff.
3. **`muster ui`** opens the web admin interface in your browser: machines, secrets, agents, skills and history in one panel.

Everything the UI does has a CLI equivalent, and every ordinary command answers `--format json` with a stable envelope for scripts and AI agents. Two commands are their own protocol: `muster mcp` speaks JSON-RPC over stdio, and `muster ui` prints one startup JSON object (`url`, `port`, `token`) before serving.

To keep an agent's user-scope instructions personal, set `skip_instructions = ["hermes"]` under `[agents]`. For only one machine, set it under `[profiles.personal.agents]` and assign that profile to the machine. This leaves Hermes `~/.hermes/SOUL.md` untouched except for removing any previously managed block; MCP, models, and skills still sync. Project-scope instructions are unaffected. Higher-precedence lists replace lower ones; use `skip_instructions = []` to clear the setting.

## AI providers

Declare endpoints once and sync their settings to your machines:

```toml
[providers.work]
kind = "openai-compatible"
base_url = "https://gateway.example/v1"
default_model = "team-model"
key = "native://app/gateway"

[agents.providers]
codex = "work"
opencode = "work"

[secrets."native://app/gateway"]
allow = ["codex", "opencode"]
```

Use `muster provider add`, `provider list`, `provider rm`, and
`provider use work --agent codex` (or add `--clear` to unset the selection).
`provider add --key native://app/gateway --value-stdin --allow codex ...`
stores a piped key in the vault; avoid `--value`, which exposes it in shell
history and process arguments. The panel's **AI providers** view lists key
references, receiving agents and endpoint settings, with add/remove actions.
Profile overrides merge individual provider fields; `muster plan --explain`
shows their sources. Explicit `[agents.models]` choices win over provider defaults.

Claude Code, Codex and OpenCode receive native settings; unsupported adapters
warn only when selected for a provider. Codex accepts OpenAI-compatible kinds;
Anthropic and Google endpoints are skipped with a warning. The assumed Codex
wire format is `chat` for `openai`, `openai-compatible` and `ollama`; verify
compatibility with your installed Codex release. The installed CLI help does
not document this mapping. Keys are protected in the repository, but resolved
keys can live in the agent's configuration on each machine, like MCP secrets.
Codex and OpenCode `env://` keys keep environment references in settings;
export the variable in the agent's environment.

Claude providers can use `api_key_helper = true` to resolve keys at runtime.
Set `approval = "each"` and `approval_ttl = "15m"` on the secret to require a
human lease for the helper or MCP. Review `muster secret requests`, approve
from a terminal or the panel, and revoke with `muster secret revoke <id>`.
`--yes` alone cannot approve without a terminal. Delivered keys may remain
cached after revocation. See [runtime secret approvals](docs/agents/muster-for-agents.md#runtime-secret-approvals)
for the commands and trust limits.

**Subscription logins are one command per agent per machine**, not synced
credentials: run `muster provider login claude` or `muster provider login codex`
on each machine. Muster delegates to the native login; it never copies OAuth
refresh tokens, `auth.json`, `.credentials.json` or Keychain entries. See the
[agent guide](docs/agents/muster-for-agents.md#ai-providers) for commands and
surface restrictions. `muster doctor --offline` checks local keys and required
logins; network key checks require explicit `--check-keys`.

## Team mode

Team mode starts when `[members]` in `muster.toml` is nonempty. Until then you
are the implicit owner — an implicit admin member holding your machines — and
the first `muster member add` materializes it into an explicit member. Point
every machine at a shared Git repository (`muster sync --remote <url>`) and add
your team. Members join by invitation, hold
`admin` or `member` roles, and own their machines. Secrets live in groups with
their own recipient lists, so each person decrypts only what they need.
Machine enrollments and member invitations can require several admins: set
`[team] required_approvals` (default 1) to the quorum you want — it is separate
configuration, independent of when team mode starts. Approvals are recorded in
the repository, and revoking a member lists exactly which secrets to rotate.
Only admins add, update or remove skills.

## Security

- **age vault.** Secrets are stored as age-encrypted files in your config repo, one ciphertext per secret group, with plaintext metadata sidecars that never hold values. Secret values are encrypted with age before they reach Git: the only data that leaves a machine is what you sync to your own Git remote.
- **Per-machine keys.** Each machine has its own keypair; the private key never leaves it. Revoking a machine re-encrypts the groups it could read.
- **Recovery key.** Generated once at `muster init`, a recipient of every group, never stored on any machine.
- **Deny-by-default agent access.** A secret reaches an agent only if its `allow` list grants that agent; otherwise the reference is omitted from generated configs with a warning. The MCP server never reveals values unless a secret is explicitly allowed for the calling client.
- **Localhost-only UI.** `muster ui` listens on `127.0.0.1` on a random free port and requires a one-time login token from your terminal.
- **No telemetry.** Muster collects no usage data and sends no telemetry. Its only network traffic is what you trigger: syncing with your Git remote, asking a subscribed skill upstream for updates, and approved `muster sync` or manual `muster agents install` downloading pinned agent CLIs.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — the full product specification.
- [`GLOSSARY.md`](GLOSSARY.md) — the project's vocabulary.
- [`docs/adr/`](docs/adr) — architecture decision records.
- [`docs/agents/muster-for-agents.md`](docs/agents/muster-for-agents.md) — operating Muster from the CLI or MCP.

## Status

Pre-release (0.1.x): the intended v1 behavior is specified in [`docs/SPEC.md`](docs/SPEC.md).

## License

[Apache-2.0](LICENSE)
