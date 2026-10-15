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
curl -fsSL https://raw.githubusercontent.com/oliveralerubio/muster/main/install.sh | sh
```

The script detects your OS and architecture (Linux and macOS, x86_64 and
aarch64), downloads the release tarball, verifies the tarball's SHA-256
checksum, and installs the binary to `~/.local/bin`, telling you how to extend
your `PATH` if the directory is not on it. Override the defaults with
environment variables: `MUSTER_VERSION` (release tag, e.g. `v0.1.0`),
`MUSTER_INSTALL_DIR` and `MUSTER_REPO` (GitHub repository to download from).
The installer is being added in the release-CI pull request and arrives with
the first tagged release; until then, build from source.

**Option 2 — from source with Cargo:**

```sh
git clone https://github.com/oliveralerubio/muster
cd muster
cargo install --path .
```

Requires a Rust toolchain and Git. You get a single executable — no interpreter
or runtime to install — dynamically linked against your system libraries (glibc
on GNU/Linux), like any standard Rust build.

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
- **No telemetry.** Muster collects no usage data and sends no telemetry. Its only network traffic is what you trigger: syncing with your Git remote, asking a subscribed skill upstream for updates, and `muster agents install` downloading the pinned agent CLIs.

## Documentation

- [`docs/SPEC.md`](docs/SPEC.md) — the full product specification.
- [`GLOSSARY.md`](GLOSSARY.md) — the project's vocabulary.
- [`docs/adr/`](docs/adr) — architecture decision records.
- [`docs/agents/muster-for-agents.md`](docs/agents/muster-for-agents.md) — operating Muster from the CLI or MCP.

## Status

Pre-release (0.1.x): the intended v1 behavior is specified in [`docs/SPEC.md`](docs/SPEC.md).

## License

[Apache-2.0](LICENSE)
