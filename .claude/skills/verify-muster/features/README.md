# Feature journeys

Each file: what the feature is, `## Sub-features`, `## How to get to it (user POV)`,
`## Driving it with muster-harness`, `## Gotchas`. Set `H` and `RUN` as in `../SKILL.md` first.
A journey that cannot reach its proof state is a FAILURE; the reason goes into the file's Gotchas, the
journey is never weakened.

| File | Feature | Surfaces |
|---|---|---|
| `secrets.md` | add / rotate / reveal / list secrets, encrypted vault | web panel, API, CLI |
| `skills.md` | skill lifecycle: add own or subscribe to git source, check, update, rm, skills.lock | CLI, API, panel Skills tab |
| `sync-schedule.md` | sync, status, periodic schedule | CLI, API, dashboard |
| `agents-apply.md` | write skills and config into claude / codex / opencode / antigravity | CLI, API plan |
| `dashboard-doctor.md` | dashboard, doctor, history views | panel, API, CLI |
