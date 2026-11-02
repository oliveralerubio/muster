# AI providers and sync across machines

`muster provider` manages named AI endpoints (`[providers.<id>]` in muster.toml): kind
(anthropic, openai, openai-compatible, ollama, google), optional base URL and default model, and a
key that is always a secret reference — the plaintext lives in the native vault, never in the
manifest. `provider use` binds one provider to one agent (`--clear` unbinds). Because providers are
manifest entries, they sync to every machine like the rest of the configuration; `provider login`
runs an agent's native subscription login locally (claude: `claude auth login`, codex:
`codex login`), so per-machine credentials stay out of the synced repo. The panel has an "AI
providers" tab listing providers and which agents use them.

## Sub-features

- `provider list`: providers with kind, base URL, key reference and the agents using them (never
  key values); API `GET /api/providers`.
- `provider add <id> --kind K [--base-url U] [--default-model M] [--key REF | --value-stdin
  [--allow AGENT]]`: replace-or-add with diff preview (`--dry-run` prints the manifest diff).
- `provider rm <id>`: removes the provider and its agent selections.
- `provider use <id> --agent claude` / `--clear`: per-agent selection.
- `provider login <id-or-agent>`: runs the agent's native login command in this terminal.
- Panel: "AI providers" tab (read-only list); add/remove go through `POST /api/provider/add` and
  `POST /api/provider/rm`, selection via `POST /api/provider/use` (all need `confirm: true`;
  `--value-stdin`/`--allow` are hidden from the panel — keys enter via the CLI).

## How to get to it (user POV)

On the admin machine: `muster provider add work-anthropic --kind anthropic --base-url
https://example/v1 --key native://work/api --value-stdin --allow claude` (paste the key on stdin),
then `muster provider use work-anthropic --agent claude`, then `muster sync`. On another enrolled
machine, `muster sync` brings
the provider definition; the agent logs in locally with `muster provider login claude`. The panel's
"AI providers" tab shows the resulting table.

## Driving it with muster-harness

```bash
$H launch $RUN
$H tree $RUN before
$H api $RUN GET providers
$H cli $RUN -- --format json provider add verify-anthropic --kind anthropic --dry-run   # diff, nothing written
$H tree $RUN after-dry-run                  # unchanged
$H cli $RUN --stdin 'verify-dummy-key' -- --yes provider add verify-anthropic --kind anthropic --key native://verify/key --value-stdin --allow claude
$H tree $RUN after-add                      # muster.toml has [providers.verify-anthropic] with key = "native://verify/key"; vault/verify.age created
$H cli $RUN -- --format json provider use verify-anthropic --agent claude --yes
$H api $RUN GET providers                   # key shown as reference, selected_agents include claude
$H cli $RUN -- --format json provider rm verify-anthropic --yes
$H leakcheck $RUN verify-dummy-key
$H screenshot $RUN providers providers-view.png
```

Observable end state that proves it: `tree` before/after shows `muster.toml` and a vault `.age` file
change on add; `provider rm` removes the manifest entry and its agent selections but does NOT touch
the vault — the `.age` file (and its `.meta.json`) created by `secret add` is retained, and `secret
list` still shows the reference afterwards; `GET /api/providers` and `provider list` show the key as
a `native://...` reference, never the plaintext; `provider use` flips the `selected_agents` column;
`--dry-run` changed nothing (tree identical); leakcheck CLEAN. Providers write NO audit events: the
only `history.jsonl` lines are the `secret.add` from storing the value (check with `tail
/tmp/muster-verify/$RUN/home/.local/state/muster/history.jsonl`). The manifest change is proven by
the config repo's git commit instead (`feat(provider): set <id>` / `feat(provider): remove <id>`,
written when the config repo has changes to commit): `git -C
/tmp/muster-verify/$RUN/home/.config/muster log --oneline`.

## Gotchas

- Provider ids follow machine-name rules: lowercase letters, digits, `-` (no `/`); `verify/anthropic`
  fails with a usage error.
- `--key` takes a secret reference, `--value-stdin` takes the plaintext from stdin — never pass a
  key as an argument (`--value` exists but warns `secret_in_argv`; prefer `--value-stdin`). Storing
  a value requires BOTH `--key native://<group>/<name>` (exactly two name segments) and
  `--value-stdin`; without `--key` the provider is declared but has no credential and `--value-stdin`
  fails with `a native:// key reference is required to store a value`.
- The panel's `provider/add` route hides `--value-stdin` (no plaintext through the web API) — add
  keys from the CLI.
- `provider login` executes the real agent binary (`claude`, `codex`) on the machine; in the sandbox
  those may be absent, so it fails there — test it on a machine that has the agent.
- `provider use` for an unknown provider or agent id fails with a usage error (exit 2); it needs
  `--yes` when stdin is not a TTY.
