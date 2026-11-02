# Feature journeys

Each file: what the feature is, `## Sub-features`, `## How to get to it (user POV)`,
`## Driving it with muster-harness`, `## Gotchas`. Set `H` and `RUN` as in `../SKILL.md` first.
A journey that cannot reach its proof state is a FAILURE; the reason goes into the file's Gotchas, the
journey is never weakened.

| File | Feature | Surfaces |
|---|---|---|
| `dashboard-doctor.md` | dashboard and doctor views, health checks | panel, API, CLI |
| `machines-team.md` | fleet machines, pending enrollments, team members & groups, approve / reject | panel, API, CLI |
| `secrets.md` | add / rotate / reveal / list secrets, encrypted vault, zero-leak verification | web panel, API, CLI |
| `skills.md` | skill lifecycle: add own or subscribe to git source, check, update, rm, skills.lock | CLI, API, panel Skills tab |
| `profiles-mcp.md` | agent profiles & MCP servers declared in muster.toml, mandatory diff preview | panel, API, CLI |
| `sync-schedule.md` | sync, status, periodic schedule | CLI, API, dashboard |
| `history-rollback.md` | generations history, audit log, rollback to generation | panel, API, CLI |
| `agents-apply.md` | write skills and config into claude / codex / opencode / antigravity | CLI, API plan |
| `toggles.md` | enable / disable skills, MCP servers, subagents (global, per-adapter, profile, project) | panel, API, CLI |
| `effective-why.md` | effective merged config per adapter; `why` provenance search of instructions & overlays | panel, API, CLI, MCP |
| `usage-telemetry.md` | local transcript usage telemetry: item counts, unused items, coverage | CLI, API, panel Skills tab, MCP |
| `mcp-server.md` | `muster mcp` stdio JSON-RPC server: read-only tools + propose_change | MCP (stdio), CLI |
| `secret-leases.md` | runtime secret requests, human approvals, time-boxed leases, revoke | panel, API, CLI, MCP |
| `provider-sync.md` | AI providers (kind, key as secret reference), per-agent use, native login, fleet sync | panel, API, CLI |

> [!NOTE]
> **Surfaces not in the web panel**:
> - `import`: The `muster import` command suite (`bws`, `chezmoi`, `cuartel`) is currently CLI-only. No web panel route or tab exists for import.
> - `usage`/`why`/`effective` (CLI-first, panel `Effective` tab for effective/why, Skills tab usage section): see `effective-why.md` and `usage-telemetry.md`.
