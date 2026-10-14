# Agents apply (adapters)

Muster writes the canonical config and skills into each agent's native files: claude, codex, opencode,
antigravity. Users run `muster agents apply` (or sync) and every installed agent sees the same skills.

## Sub-features

- List adapters, enabled/detected, managed files (`muster agents`).
- Plan (`GET /api/plan`, `muster plan`, `agents apply --dry-run`) and apply (`agents apply [--agent X]`).
- Skill install per agent, MCP/instruction files per adapter; generations recorded for rollback.

## How to get to it (user POV)

CLI `muster agents apply`; panel "Profiles & diff" shows the plan.

## Driving it with muster-harness

```bash
$H launch $RUN        # then add a skill so there is something to install (see skills.md), e.g. an own skill:
$H cli $RUN -- --yes skill add demo --description "demo skill"
$H cli $RUN -- --format json agents 2>/dev/null | jq -c '.result.agents[]|{id,enabled,detected}'
$H tree $RUN before
$H cli $RUN -- --dry-run agents apply              # shows planned paths
$H tree $RUN after-dry                             # unchanged
$H cli $RUN -- --yes agents apply
H0=/tmp/muster-verify/$RUN/home
ls $H0/.claude/skills/demo/SKILL.md $H0/.agents/skills/demo/SKILL.md \
   $H0/.config/opencode/skills/demo/SKILL.md $H0/.gemini/antigravity-cli/skills/demo/SKILL.md
$H api $RUN GET plan                               # changes == 0 after apply
```

Observable end state: the four `SKILL.md` files exist with the skill's content (claude `.claude/skills/`,
codex `.agents/skills/`, opencode `.config/opencode/skills/`, antigravity `.gemini/antigravity-cli/skills/`),
plus `.claude/settings.json`, `.claude.json`, `.codex/config.toml`, `opencode.json`, `.gemini/config/mcp_config.json`;
`generations/` has generation 1; a repeat plan reports `changes: 0`.

## Gotchas

- Paths are relative to the sandbox `HOME`, so nothing lands in the real home; this was observed, not assumed.
- Agent detection uses PATH: the sandbox keeps your real PATH after `<scratch>/bin`, so host agent CLIs are
  "detected" and `doctor` may report the host's outdated Codex (`outdated_binary`, severity error). That is a
  stand-in environment fact, not a muster bug.
- `agents install` downloads pinned CLIs into `HOME/.local/bin`; needs network and a pinned `muster.lock`; not
  covered here.
- `--agent` accepts one adapter id; `team::record_applied` bookkeeping only runs for a full apply.
