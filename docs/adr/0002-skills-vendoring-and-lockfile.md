# Skills are vendored into the config repo and pinned in `skills.lock`; nothing is deleted automatically

Skills come from two places: own skills authored in the config repository, and
skills subscribed from external Git repositories (`[[agents.skills.remote]]`).
Machines must receive skills through a plain `git pull` + apply — even
offline, without network access or credentials on every machine — and the
apply engine and agent adapters should not need to know that Git sources,
upstream refs or deprecations exist.

## Decision

Subscribed skills are vendored: their files are copied into the config
repository's skills folder at subscribe and update time, and
`skills.lock` (versioned TOML next to `muster.toml`) records each skill's
`origin`, upstream `commit`, `content_hash`, `version` and `updated_at`.
Upstream changes are applied only by an explicit `muster skill update`; a
skill removed upstream stays installed until someone runs `muster skill rm`
(delete) or `skill rm --detach` (keep it as an own skill); a locally modified
skill refuses update without `--force`. In team mode, vendoring commits carry
the `Muster-Actor` trailer, so skills change only through admin commits.

## Consequences

- The config repo grows by the size of subscribed skills (plus their history
  once committed), but machines never fetch from upstreams themselves.
- The engine and adapters have a single code path: own and vendored skills are
  indistinguishable at apply time.
- Content hashes surface hand edits as `modified` instead of silently
  overwriting them, and make updates auditable before they happen.
- Deletion is always a human decision: an upstream removal appears as
  `upstream_removed` (needs attention), never as a skill vanishing from a
  machine after a routine sync or scheduled update.
- Skills are reviewed like every other config change: through the config
  repository's Git history and, in teams, admin-gated commits.

## Amendment — 2026-10-22 (#37)

Apply now verifies own and vendored skill files locally against their
`content_hash` in `skills.lock` before rendering them. A missing pin or a hash
mismatch skips that skill with a named warning and keeps its previously managed
copy (or leaves it uninstalled if there was none). Other configuration, secrets
and verified skills still apply. Empty skill directories are ignored.

This amends the original statement that the engine need not know about the
lockfile: adapters still receive the same verified files, and sync still never
contacts skill upstreams. Editing an own skill now requires an explicit
`muster skill pin <name>` before it can spread; commit the reviewed files and
`skills.lock` together. `skill add` records the initial pin and `skill update`
continues to record subscribed skill changes. Deletion remains explicit.

## Amendment — source hygiene

Vendoring, pinning and apply use the same UTF-8 file set: dot-names, names
ending in `~`, `.bak`, `.orig`, `.rej`, `.swp`, `.swo`, or `.tmp`, names
containing `.bak-`, and directories `node_modules` and `__pycache__` are
ignored at every depth. Binary files are unsupported. An unreadable skill is
skipped with a warning and its previously installed copy is preserved.
