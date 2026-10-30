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
| `effective` | `effective` | Effective configuration and its sources | No |
| `why` | `why` | Find text in agent context | No |
| `usage` | `usage` | Local agent usage | No |
| `provider_list` | `provider.list` | AI endpoint settings, key references and receiving agents | No |
| `secret_list_meta` | `secret.list` | Secret metadata, groups, readers, allowed agents (never secret values) | No |
| `propose_change` | `proposal.create` | Stashes a proposed config change and returns its plan | Proposal only |
| `secret_reveal` | `secret.get --reveal` | Reveals an explicitly allowlisted secret reference | Read-only |

The new read-only tools use global scope: `project` is hidden, as is `usage`'s `max-file-bytes`.

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

## Effective configuration

`muster effective --format json` reports active layers, origins, instructions, skills, MCP servers, subagents, per-adapter disk states, unmanaged extras, approximate token totals, and warnings. Use `--project DIR` for a project and `--agent ID` to select an adapter. The read-only web route `GET /api/effective` returns the same result as the CLI.

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
   To propose enabling or disabling an item, supply the complete `muster.toml`
   with the item moved between the `enable` and `disable` arrays under
   `[agents.toggles]` (or `[profiles.<name>.agents.toggles]` for a profile).
   For example, `disable = ["skill:demo@claude"]` disables that skill for
   Claude. Preserve the rest of the manifest. Project configuration outside
   the config repository cannot be proposed through this flow.
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


## AI providers

`muster provider list --format json` reports named AI endpoints and key
references, never resolved values. Configure `[providers.<id>]` with `kind`,
optional `base_url`, `default_model`, and a secret reference `key`; select it
with `muster provider use <id> --agent <agent> --yes`. Add `--clear` to unset
the base selection. Profile fields override individually and `plan --explain`
shows their sources. A per-agent model wins over the provider default.

Use `provider add ... --value-stdin` with a `native://` key to vault a piped
value. Do not use `--value` in scripts; it can leak through shell history and
process arguments. Keys written to native agent configs are plaintext there,
even though the repository vault remains encrypted. For Codex `env://` keys,
export the referenced variable in Codex's own environment too.

**Subscription logins are one command per agent per machine.** Run
`muster provider login claude --yes` or `muster provider login codex --yes`
in that machine's terminal. Muster runs native status/login commands, never
copies OAuth tokens or reads `auth.json`, `.credentials.json` or Keychain.
This is not a zero-setup login sync. The panel does not offer an interactive
login route; it directs users to their terminal.

MCP exposes `provider_list`, but not `provider_add`, `provider_rm`,
`provider_use` or `provider_login`. Propose endpoint/selection edits to
`muster.toml` with `propose_change`; a person reviews and accepts them before
normal sync. This preserves the existing mutation policy, not accidental
surface parity. The web panel offers list/add/remove/use routes with its
normal session and confirmation requirements.

`muster import cuartel <directory> --dry-run --format json` detects native
`.claude/settings.json`, `.codex/config.toml` and
`.config/opencode/opencode.json` endpoints. `import chezmoi` detects their
normalized source-tree equivalents. Both propose `[providers]` entries with
placeholder `env://MUSTER_IMPORTED_<ID>_KEY` references instead of copying key
values; fill these references and configure `allow`, then select receiving
agents. Native login stores are not imported. `doctor --offline` checks local
availability and subscription login; `doctor --check-keys` opts into online key
checks. Codex `wire_api = "chat"` for OpenAI-compatible kinds is an assumption
not documented by installed CLI help; Anthropic and Google kinds are skipped.


## Runtime secret approvals

For a secret that needs human approval, configure:

```toml
[secrets."native://app/key"]
allow = ["claude"]
approval = "each"
approval_ttl = "15m"
```

The default is `approval = "none"`. Durations accept `s`, `m`, or `h`, up to
24h. Use `muster secret get native://app/key --agent claude --reveal --yes`
for agent calls. The current `allow` is checked on every reveal, even with a
lease. Denial returns exit 5 and `secret_not_allowed_for_agent`. Without a
lease, exit 5 and `secret_approval_required` include a suggested approval
command. Repeated requests reuse the pending request. MCP `secret_reveal`
uses its initialized agent and follows the same approval check.

A person reviews `muster secret requests` and runs `muster secret approve <id>`
in a real terminal, or approves in the panel with a confirmed session.
`--yes` without a terminal cannot approve. `--ttl 5m` overrides the configured
duration, up to 24h. `secret deny <id>` removes a request. `secret leases` lists active
approvals, and `secret revoke <id>` or `secret revoke --all` removes them.
Approve, deny, and revoke are never MCP tools. Human CLI reveals without an
agent retain ordinary confirmation. Local state and audit contain no values.

CLI agent names and MCP client names are self-declared, not authenticated.
These controls provide traceability and prevent accidental delivery, not
isolation from malicious processes running as the same OS user. Leases allow
repeated reveals until expiry. Revocation cannot erase delivered credentials.

Claude providers with `api_key_helper = true` include `--agent claude` in the
helper. With approval `each`, the generated cache interval is at most 60s and
never exceeds `approval_ttl`. Claude's
[documented default](https://code.claude.com/docs/en/settings-reference#apikeyhelper)
is five minutes. Cached keys can outlive revocation or lease expiry until the
next refresh. OpenCode `env://VAR` providers use
[documented environment substitution](https://opencode.ai/docs/config/#env-vars)
and require that variable in OpenCode's environment. This avoids writing the
value into settings but does not enforce leases. Use the runtime Claude helper
or MCP for lease checks; static configuration injection does not check them.

Agent installs preserve versions newer than `muster.lock` and emit the warning
`agent_newer_than_pin`, suggesting a pin bump. Status includes `kept_newer` and a
note for these agents; they count as converged when a platform artifact exists.
