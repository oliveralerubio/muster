# Secrets

Store API keys and similar values encrypted in the config repo's vault, see who may read them, change them,
and reveal one on purpose. The value never appears in lists, history or responses.

## Sub-features

- Add a secret `group/name` (web "New secret", `POST /api/secret/add`, CLI `secret add --value-stdin`).
- Rotate (replace) its value (`secret/rotate`, "Change value").
- List metadata only (`GET /api/secrets`, `secret list`).
- Reveal, explicit confirm, recorded in history (`secret/reveal`, "Reveal...").
- Remove (`secret/rm`).

## How to get to it (user POV)

Open the panel, tab "Secrets": "New secret" button opens a dialog (path, value as password input, a confirm
checkbox, Save). Each row has "Reveal..." and "Change value". CLI users run `muster secret add app/api
--value-stdin` and pipe the value.

## Driving it with muster-harness

```bash
$H launch $RUN
$H tree $RUN before
$H api $RUN POST secret/add '{"path":"verify/api","value":"verify-dummy-123"}'        # expect 400 (no confirm)
$H api $RUN POST secret/add '{"path":"verify/api","value":"verify-dummy-123","confirm":true}'
$H tree $RUN after-add
$H api $RUN POST secret/rotate '{"path":"verify/api","value":"verify-dummy-456","confirm":true}'
$H tree $RUN after-rotate
$H api $RUN GET secrets
$H leakcheck $RUN verify-dummy-123 ; $H leakcheck $RUN verify-dummy-456
$H cli $RUN --stdin 'cli-dummy-789' -- --yes secret add verify/cli --value-stdin
echo '{"name":"verify/api","confirm":true}' | $H api $RUN POST secret/reveal -
$H screenshot $RUN secrets secrets-view.png
tail -n 5 /tmp/muster-verify/$RUN/home/.local/state/muster/history.jsonl
```

Observable end state that proves it: `vault/verify.age` is created by add and its sha256 differs after rotate
(`tree` diff); `vault/verify.meta.json` updates; `history.jsonl` has `secret.add`, `secret.rotate` (and
`secret.reveal` after a reveal) with subject `verify/api` and no value; `GET /api/secrets` lists
`native://verify/api`; both leakchecks print CLEAN; the screenshot shows the row with Reveal/Change value.

## Gotchas

- Missing `confirm:true` returns 400 `secret writes require confirm: true`; the CLI needs `--yes` (no TTY).
- `secret add --value-stdin` with empty stdin fails exit 2 ("value on stdin is empty"); pass `--stdin TEXT`.
- Reveal responses contain plaintext in `result`; the harness withholds it from evidence files, but it is
  printed on your terminal, so only use dummy values.
- Adding does not need a git identity; the group's `.age` file is shared by all secrets of that group, so
  the file is `<group>.age` not per secret.
- Readers default to `personal, recovery` (the sandbox's machine and the recovery key).
- The panel's `Updated` timestamp is the real clock; do not assert on it.
