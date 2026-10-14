# Muster for AI Agents

Guide for autonomous and human-supervised AI agents interacting with Muster via the CLI or over the Model Context Protocol (MCP).

## Operating Modes

Agents interact with Muster through two surfaces:

1. **MCP stdio server (`muster mcp`):** Native JSON-RPC 2.0 tool interface with safety boundaries. Mutating actions are gated behind a proposal workflow.
2. **CLI (`muster --format json <command>`):** Scriptable CLI emitting a single JSON envelope on stdout and logs on stderr. Non-interactive invocations require `--yes` for commands that prompt.

## Output Contract & JSON Envelope

Every CLI command with `--format json`, and every MCP tool call result, produces exactly one JSON envelope:

```json
{
  "schema_version": 1,
  "command": "status",
  "ok": true,
  "result": { ... },
  "diagnostics": []
}
```

- `schema_version`: Always `1`.
- `command`: Canonical dotted command name (e.g., `status`, `proposal.create`, `secret.list`).
- `ok`: `true` when the operation succeeded, `false` otherwise.
- `result`: Command-specific payload on success (`null` on failure).
- `diagnostics`: Array of structured diagnostic items:
  - `code`: Stable machine-readable error/warning code (e.g., `usage`, `security`, `invalid_config`, `outdated_binary`, `drift`).
  - `message`: Human-readable summary.
  - `severity`: `"error"`, `"warning"`, or `"info"`.
  - `path`: (Optional) Associated filesystem path.
  - `suggestion`: (Optional) Command or hint to resolve the diagnostic.

## Exit Codes

Muster CLI exits with predictable process codes:

| Code | Name | Description |
|---|---|---|
| `0` | Success | Command succeeded (including valid `--dry-run`). |
| `1` | Operational | State or environment failure (uninitialized repo, missing secret, doctor failure). |
| `2` | Usage | Invalid flags, syntax, missing arguments, or interactive prompt without `--yes` in non-TTY. |
| `3` | Invalid Config | Parse failure in `muster.toml` or invalid manifest schema. |
| `4` | Unavailable | External dependency (git) or secret provider CLI unavailable. |
| `5` | Security | Unauthorized action, recipient key mismatch, or self-approval attempt. |
| `130` | Interrupted | SIGINT received (clean termination). |

## MCP Server (`muster mcp`)

The MCP server runs over stdio using newline-delimited JSON-RPC 2.0.

### Supported Tools

| Tool | Registry Equivalent | Description | Mutating? |
|---|---|---|---|
| `status` | `status` | Fleet machines, sync state, local machine identity | No |
| `doctor` | `doctor` | Machine health, binary versions, permissions (`fix` is hidden) | No |
| `plan` | `plan` | Desired vs. current state diff (secrets redacted) | No |
| `secret_list_meta` | `secret.list` | Secret metadata, groups, readers, allowed agents (never secret values) | No |
| `propose_change` | `proposal.create` | Stashes a proposed config change and returns its plan | Proposal only |
| `secret_reveal` | `secret.get --reveal` | Reveals an explicitly allowlisted secret reference | Read-only |

### Safety Boundaries

- **No Unsupervised Mutating Actions:** Commands like `sync`, `init`, `agents.apply`, `secret.add`, or `rollback` are not exposed directly as MCP tools.
- **Secret Hygiene:** Plaintext secret values are never exposed across tools, listings, plans, or diffs. The only exception is `secret_reveal`, which requires BOTH that the reference is listed in `[mcp_server] reveal` AND that the secret's `allow` list grants the calling agent:
  ```toml
  [mcp_server]
  reveal = ["native://ci/deploy_token"]

  [secrets."native://ci/deploy_token"]
  allow = ["claude"]          # or "codex@my-repo" for one project only
  ```
  The calling agent is identified from the `initialize` request's `clientInfo.name` (contains `claude` -> claude, `codex` -> codex, `opencode` -> opencode, `antigravity`/`agy` -> antigravity). Unknown clients are never granted. A denied reveal returns a `security` diagnostic and `isError: true`.
- **Secret isolation in generated configs:** deny by default. When Muster renders an agent's MCP `env`/`headers`, a secret reference resolves only if its `[secrets."<ref>"] allow` grants that agent (`agent` everywhere, `agent@project` only in that project's scope). Otherwise the key is omitted and a `secret_not_allowed_for_agent` warning is emitted.

## Agent Skills

`[agents.skills] source = "skills"` in `muster.toml` points to a folder with one subfolder per skill (`SKILL.md` with `name` and `description` frontmatter, plus any files). `agents apply` / `sync` install them for every supported agent:

| Agent | User scope | Project scope |
|---|---|---|
| Claude Code | `~/.claude/skills/<skill>/` | `.claude/skills/<skill>/` |
| Codex | `~/.agents/skills/<skill>/` | `.agents/skills/<skill>/` |
| OpenCode | `$XDG_CONFIG_HOME/opencode/skills/<skill>/` | `.opencode/skills/<skill>/` |
| Antigravity CLI | `~/.gemini/antigravity-cli/skills/<skill>/` | `.agents/skills/<skill>/` |

- Project scope uses `<project>/.muster/muster.toml`; Codex and Antigravity share `.agents/skills`.
- Removed skills are deleted on the next apply; hand-added skills are never touched.
- Invalid skills (no `SKILL.md`, or missing `name`/`description`) are skipped with a `skill_invalid` warning; exit code stays `0`.

### Skill lifecycle commands

- Own skills: `muster skill add <name> --description "..."` scaffolds `<source>/<name>/SKILL.md`.
- Subscriptions: `muster skill add --url <git> [--ref <ref>] [--path <dir>] [--include a,b] [--source-name <id>]` adds `[[agents.skills.remote]]` to `muster.toml` and vendors the skills into the skills folder.
- `skill.list` is offline: it reads `skills.lock`, the per-machine status cache (`<state>/skills-status.json`) and on-disk hashes. Each skill has `origin`, `version`, `updated_at`, `status`, `modified`, `needs_attention`.
- `skill.check [name]` fetches sources (shallow git into `$XDG_CACHE_HOME/muster/skill-sources/`). Diagnostics: `skill_update_available`, `skill_deprecated`, `skill_upstream_removed`, `skill_source_unreachable`.
- `skill.update <name>|--all [--force] [--dry-run] --yes`: result `updated[].files` has `added`, `changed`, `removed`. Refuses `upstream_removed` and locally `modified` skills (unless `--force`).
- `skill.rm <name> --yes` deletes; `--detach` keeps a subscribed skill as `origin = "local"`.
- Statuses: `local | up_to_date | update_available | deprecated | upstream_removed | unknown`. Deprecated wins over update_available. Nothing is updated or deleted without an explicit command.
- Team mode: `add`, `update`, `rm` need an admin (exit 5 otherwise); `list` and `check` are open to members.
- Web API: `GET /api/skills`, `POST /api/skills/check|add|update|rm` (`{"confirm": true}` for update/rm).

## Proposal Flow (Agents Proposing Changes)

When an agent needs to adjust Muster configuration (e.g. adding an MCP server, tuning agent instructions, modifying dotfiles):

1. **Agent proposes a change (`propose_change` tool):**
   ```json
   {
     "path": "muster.toml",
     "contents": "...",
     "message": "add postgres MCP server"
   }
   ```
2. **Muster validates and stashes:**
   - Validates paths: restricted to configuration files (cannot escape, cannot touch `vault/`, `.git/`, or `proposals/`).
   - If targeting `muster.toml`, checks manifest syntax and schema.
   - Computes an isolated plan showing the exact diff and required approvals.
   - Writes proposal files to `<config>/proposals/<id>/`.
   - Returns the proposal ID and engine plan.
3. **Human reviews and accepts:**
   - Human inspects: `muster proposal list` or checks the plan.
   - Human accepts: `muster proposal accept <id>` (copies files into config repo and creates git commit).
   - Human syncs: `muster sync` (runs standard plan and interactive approval for new hooks/MCP servers).
