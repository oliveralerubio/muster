---
name: muster
description: Use when working with Muster, the local-first tool that syncs AI-agent configuration (instructions, skills, MCP servers) and secrets across machines. Covers the CLI for secrets, sync, status, diagnostics and MCP servers.
---

# Muster

Muster is a local-first configuration, secret and agent synchronizer. A config repo (`muster.toml` plus instructions, skills and MCP definitions) is the source of truth; `muster sync` renders it into each agent's native files (Claude, Codex, OpenCode, Antigravity, Pi).

## Flags for agents

- `--format json`: machine-readable output. Always use it when parsing results.
- `--yes`: non-interactive; never wait for a prompt.
- `--stdin` / `--value-stdin`: pass sensitive input through stdin, never as an argument.

## Secrets

Never print, log or paste secret values in chat or command output.

- `muster secret list`: names and paths only, safe to show.
- `muster secret get <ref> --reveal --yes` (e.g. `muster secret get native://app/api --reveal --yes`): prints the value. Pipe it straight into the command that needs it; never echo it. With `--format json`, extract `.result` from the envelope.
- `muster secret add <path> --value-stdin`: read the value from stdin, e.g. `printf %s "$VALUE" | muster secret add <path> --value-stdin`. Do not put the value in argv or shell history.

## Apply changes

After editing the config repo, run `muster sync` to apply it to every enabled agent. Run it again after adding secrets, skills or MCP servers.

## Inspect and debug

- `muster status`: overview of the config repo and sync state.
- `muster doctor`: health checks with fix hints.
- `muster why <needle>`: where a text string appears in agent context ("why does the agent know X").
- `muster effective`: the resolved configuration and where each setting came from.

## MCP servers

- `muster mcp-server list`: servers defined in the config.
- `muster mcp-server add`: add a server; follow with `muster sync`.

Run `muster --help` or `muster <command> --help` for the full option list.
