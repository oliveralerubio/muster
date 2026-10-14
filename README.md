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

## Status

Pre-alpha. The full product spec lives in [`docs/SPEC.md`](docs/SPEC.md).

## License

[Apache-2.0](LICENSE)
