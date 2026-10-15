# Muster

> Sync your machines, AI coding agents and secrets. Local-first, team-ready.

Muster keeps the configuration of your AI coding agents (Claude Code, Codex, OpenCode, Antigravity CLI) identical across every machine you own, and across every member of your team, including rules, MCP servers, profiles and the secrets they need.

- **Machines:** a fleet inventory. Enroll a new laptop in one command.
- **Agents:** one source of truth (`AGENTS.md`, Agent Skills and a universal MCP manifest) rendered into each agent's native format, without clobbering local edits.
- **Secrets:** a built-in encrypted vault ([age](https://age-encryption.org)) that lives in your git repo. Each machine keeps its own private key. No accounts, no third-party servers. Bitwarden, 1Password, Doppler and Infisical are optional adapters.

Three equivalent interfaces:

| Interface | Command | For |
|---|---|---|
| CLI | `muster sync`, `muster status`, ... | Everything, 100% scriptable |
| Web UI | `muster ui` | Admins managing machines, members, groups and secrets |
| Agents | `muster mcp`, `--format json` | AI agents operating Muster with scoped permissions |

## Agent Skills

Declare a skills folder in `muster.toml` (`[agents.skills] source = "skills"`), one subfolder per skill with a `SKILL.md` (YAML frontmatter with `name` and `description`) plus any extra files. Muster installs every file, byte for byte, for all four agents:

| Agent | User scope | Project scope |
|---|---|---|
| Claude Code | `~/.claude/skills/<skill>/` | `.claude/skills/<skill>/` |
| Codex | `~/.agents/skills/<skill>/` | `.agents/skills/<skill>/` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/skills/<skill>/` | `.opencode/skills/<skill>/` |
| Antigravity CLI | `~/.gemini/antigravity-cli/skills/<skill>/` | `.agents/skills/<skill>/` |

Project scope comes from `<project>/.muster/muster.toml`. Codex and Antigravity share `.agents/skills` in a project. Skills removed from the source are deleted on the next apply; skills you add by hand are left alone. A skill without `SKILL.md` or without `name`/`description` is skipped with a `skill_invalid` warning.

## Subagents

Declare a subagents folder in `muster.toml` (`[agents.subagents] source = "subagents"`), one Markdown file per subagent (`<name>.md`) with YAML frontmatter (`name` and `description` required, optional `tools` and `model`) and the body as the system prompt. The frontmatter is parsed as YAML (block scalars, quoted strings and list-form `tools` all work). `name` must match `[a-z0-9][a-z0-9._-]*` (no `/`, `\`, `..` or leading dot) and equal the file stem. Muster installs the file byte for byte where that is the agent's native format, and translates it where it differs:

| Agent | User scope | Project scope |
|---|---|---|
| Claude Code | `~/.claude/agents/<name>.md` | `.claude/agents/<name>.md` |
| Codex | `~/.codex/agents/<name>.toml` | `.codex/agents/<name>.toml` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/agents/<name>.md` | `.opencode/agents/<name>.md` |
| Antigravity CLI | `~/.gemini/antigravity-cli/agents/<name>.md` | `.agents/agents/<name>.md` |

Codex gets a TOML custom-agent file (`developer_instructions`); OpenCode gets a Markdown agent file with `mode: subagent` (the file name is the agent name); generated frontmatter is serialized as proper YAML/TOML, so descriptions with colons, quotes or line breaks stay valid. Fields a target cannot express (`tools`, and any other non-canonical frontmatter key, for Codex and OpenCode) are skipped with a `subagent_field_untranslated` warning. Subagents removed from the source are deleted on the next apply; files you add by hand are left alone. A file without valid frontmatter (missing fields or malformed YAML) is skipped with a `subagent_invalid` warning.

### Skill lifecycle (own and subscribed skills)

Write your own skills in the config repo, or subscribe to a git repository of skills. Subscribed skills are copied into the skills folder, so every machine gets them through the normal sync, even offline.

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

| Command | What it does |
|---|---|
| `muster skill list` | Every skill with origin, version, last update and status. No network. |
| `muster skill add <name> [--description ...]` | Creates `skills/<name>/SKILL.md` for an own skill. |
| `muster skill add --url <git> [--ref] [--path] [--include a,b] [--source-name]` | Subscribes to a repository and copies its skills. |
| `muster skill check [name]` | Asks the upstream repositories for news. |
| `muster skill update <name>\|--all [--force] [--dry-run] --yes` | Applies upstream changes; shows which files are added, changed or removed. |
| `muster skill rm <name> [--detach] --yes` | Deletes a skill, or with `--detach` keeps a subscribed skill as your own. |

`skills.lock` (next to `muster.toml`, committed) records for each skill its `origin` (`local` or the source id), upstream `commit`, `content_hash` (`sha256:` over its files), `version` (from `SKILL.md` frontmatter) and `updated_at`.

Statuses: `local`, `up_to_date`, `update_available`, `deprecated` (upstream `SKILL.md` has `deprecated: true` or `deprecated: "<reason>"`), `upstream_removed`, `unknown` (not checked yet, or the source could not be reached). Nothing is applied or deleted on its own: updates need `skill update`, and a skill removed upstream stays until you run `skill rm` (with or without `--detach`). A skill edited by hand shows `modified` and `update` refuses to overwrite it without `--force`. In a team, only admins add, update or remove skills; members can list and check. The web panel has a Skills tab with the same actions.

## Status

Pre-alpha. The full product spec lives in [`docs/SPEC.md`](docs/SPEC.md).

## License

[Apache-2.0](LICENSE)
