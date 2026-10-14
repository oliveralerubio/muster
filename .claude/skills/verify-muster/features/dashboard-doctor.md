# Dashboard and doctor

The panel's landing view and health pages: fleet status, this machine's membership and drift, a doctor
report, and the history of actions.

## Sub-features

- Dashboard (`#nav` data-view `dashboard`, `GET /api/status`).
- Doctor (`doctor` view, `GET|POST /api/doctor`, `muster doctor [--fix]`).
- History (`history` view, generations and audit events from `history.jsonl`).
- Footer `#cli` always shows the equivalent CLI command.

## How to get to it (user POV)

`muster ui` opens the browser; tabs Dashboard, Machines, Members & groups, Secrets, Skills, Profiles & diff,
History, Doctor.

## Driving it with muster-harness

```bash
$H launch $RUN && $H doctor $RUN
$H api $RUN GET status                          # ok:true, machines[0].is_current == true
$H screenshot $RUN dashboard dash.png
$H screenshot $RUN doctor doctor.png
$H api $RUN GET doctor                          # result.checks[] with name/status, summary counts
$H cli $RUN -- --format json status 2>/dev/null | jq -c '.result.machines[0].name'
$H cli $RUN --stdin x -- --yes secret add verify/h --value-stdin   # then:
$H screenshot $RUN history history.png          # event list shows secret.add
```

Observable end state: dashboard `.txt` sidecar lists the machine name and status; `GET /api/status` equals
`muster status --format json` for the same sandbox (compare `result`); doctor `.txt` shows the checks and the
`healthy` flag matches `GET /api/doctor`; history view text contains the action just taken.

## Gotchas

- Doctor can be `ok:false` on a pristine sandbox (e.g. `agents` check error from the host's old Codex, a
  warning `repo_dirty` for the uncommitted init). Report what doctor says; do not claim healthy.
- `GET /api/doctor` returns HTTP 200 with `ok:false` for unhealthy; check `ok`, not only status code.
- The Skills tab red dot is from the cached skills status, not live.
- Screenshots need Chrome; without it the journey fails at the screenshot step, use `GET` state as the proof
  for the data and say the visual proof was skipped.
