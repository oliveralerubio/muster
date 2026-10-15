# History, generations and rollback

Every mutating sync or apply creates a recorded generation. Users can inspect the history of
generations and audit events, and roll back configuration to any earlier generation.

## Sub-features

- List generations and events (`GET /api/history`, `muster history`, `muster rollback`).
- Roll back to a previous generation (`POST /api/rollback`, `muster rollback <generation>`).
- Audit log of administrative actions (`history.jsonl`).

## How to get to it (user POV)

Open the panel, tab **History**: The "Generations" table shows past generations (`#`, `Created`, `Kind`,
`Files`) with a "Roll back" button per row. The "Events" table lists timestamped administrative actions.

## Driving it with muster-harness

```bash
$H launch $RUN
# 1. Apply a sync to produce Generation 1
$H api $RUN POST sync '{"confirm":true}'
$H api $RUN GET history                         # shows generation 1
$H screenshot $RUN history history-gen1.png

# 2. Roll back to Generation 1
$H api $RUN POST rollback '{"generation":1,"confirm":true}'
$H api $RUN GET history                         # shows generation 2 (rollback of 1)
$H screenshot $RUN history history-gen2.png
```

Observable end state: `GET /api/history` contains the newly created rollback generation with
`kind: "rollback of 1"`, and the managed agent config files are restored to match that generation.

## Gotchas

- Rollback requires explicit `confirm: true` on the API (or `--yes` on the CLI).
- Rollback itself creates a new forward generation (it does not erase history).
- Secret values are never stored in history records or generation manifests.
