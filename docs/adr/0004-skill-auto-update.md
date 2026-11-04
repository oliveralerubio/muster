# Skill auto-update is opt-in, unattended, and never overwrites local edits

[ADR 0002](0002-skills-vendoring-and-lockfile.md) says upstream changes are
applied only by an explicit `muster skill update`. In practice subscribed skills
then go stale on every machine unless someone remembers to run it, and people who
already run `muster schedule` expect the fleet to keep itself current.

## Decision

`[agents.skills] auto_update = true` (default `false`; `muster skill auto-update
on|off`, `POST /api/skills/auto-update`) makes `muster sync` update subscribed
skills in the same run: after the config repo is pulled and before it is pushed,
Muster checks every source and applies updates, so the resulting commit is
pushed and the new files are applied to the agent folders immediately.

Safeguards:

- **Opt-in and explicit.** Turning it on asks for confirmation; the manifest
  records the choice in the repo like any other setting.
- **No global `--yes`.** Scheduled syncs run without `--yes` on purpose, because
  `--yes` would also approve new hooks and MCP servers. Auto-update instead uses
  an internal unattended path that skips only the skill-update confirmation.
- **Local edits are never overwritten.** A skill whose files differ from its
  `skills.lock` pin is skipped with a `skill_modified` warning; auto-update never
  forces.
- **Team mode.** Only admins change skills (ADR 0002), so on a non-admin machine
  auto-update warns and does nothing.
- **Failures are warnings.** An unreachable source or a git error produces a
  diagnostic and sync carries on and exits 0.
- **Nothing is deleted.** Skills removed or deprecated upstream are reported, as
  before, never removed automatically.

## Consequences

- Upstream content reaches every machine without a person in the loop, which
  raises the trust placed in subscribed repositories. Users who want a review
  step leave `auto_update` off (the default) and keep running `skill update`.
- Updates remain visible: each is a commit in the config repo, reported as
  `skills_updated` in the sync output.
- Own skills are unaffected: editing one re-pins it (`skill.file.set`), so edits
  made in the panel propagate on the next sync without a manual `skill pin`.
