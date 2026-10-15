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

> [!NOTE]
> **Surfaces not in the web panel**:
> - `import`: The `muster import` command suite (`bws`, `chezmoi`, `cuartel`) is currently CLI-only. No web panel route or tab exists for import.
