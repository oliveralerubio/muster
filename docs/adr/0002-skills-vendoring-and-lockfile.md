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
