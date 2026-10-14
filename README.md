# Muster

> Sync your machines, AI coding agents and secrets. Local-first, team-ready.

Muster keeps the configuration of your AI coding agents (Claude Code, Codex, OpenCode, Gemini CLI) identical across every machine you own, and across every member of your team, including rules, MCP servers, profiles and the secrets they need.

- **Machines:** a fleet inventory. Enroll a new laptop in one command.
- **Agents:** one source of truth (`AGENTS.md` and a universal MCP manifest) rendered into each agent's native format, without clobbering local edits.
- **Secrets:** a built-in encrypted vault ([age](https://age-encryption.org)) that lives in your git repo. Each machine keeps its own private key. No accounts, no third-party servers. Bitwarden, 1Password, Doppler and Infisical are optional adapters.

Three equivalent interfaces:

| Interface | Command | For |
|---|---|---|
| CLI | `muster sync`, `muster status`, ... | Everything, 100% scriptable |
| Web UI | `muster ui` | Admins managing machines, members, groups and secrets |
| Agents | `muster mcp`, `--format json` | AI agents operating Muster with scoped permissions |

## Status

Pre-alpha. The full product spec lives in [`docs/SPEC.md`](docs/SPEC.md).

## License

[Apache-2.0](LICENSE)
