# AGENTS.md

## Agent skills

### Issue tracker

Issues and specs live in GitHub Issues (`oliveralerubio/muster`), managed with the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary: `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: one `GLOSSARY.md` and `docs/adr/` at the repo root. See `docs/agents/domain.md`.

## Muster Cloud

- The Muster Cloud service lives in the private repo `oliveralerubio/muster-cloud`, not here. This repo keeps the CLI side only.
- Protocol changes go into `src/cloud_protocol.rs` here first and land on main; then the private repo bumps its pinned `rev`.
- Do not narrow the pub API covered by `tests/test_cloud_api_surface.rs` (`signing`, `fleet_state`, `manifest`, `fleet`, `cloud_protocol`) without coordinating with the private repo.
