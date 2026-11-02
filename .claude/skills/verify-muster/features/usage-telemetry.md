# Usage telemetry

`muster usage` reads local agent session transcripts on this machine (read-only, never leaves it)
and reports which managed items — skills, MCP servers, subagents — were actually used, and which are
dead weight. Data sources: `~/.claude/projects` (claude), `~/.codex/sessions` (codex); other
adapters are reported `unsupported`. In the panel the Skills tab also embeds a usage section from
`GET /api/usage`.

## Sub-features

- Per-item counts: `items[]` with `kind`, `name`, `uses`, `sessions`, `last_used` and a per-agent
  breakdown.
- Unused items: `unused[]` with a ready-made suggestion such as `muster disable skill:babysitpr`.
- Coverage and sources: `sessions_scanned` per adapter, `coverage` = `none|partial|full`, and
  `sources[]` with transcript directory, status (`supported|partial|unsupported`), `malformed_lines`
  and `skipped` counts (`older_than_window`, `too_large`, `unreadable`).
- Filters: `--since 14d` (days, default 14d), `--agent ID`, `--project DIR` (canonicalized, filters
  transcripts by their `cwd`), `--max-file-bytes` (default 50 MB, transcripts bigger are skipped).
- MCP: exposed as the read-only `usage` tool (`--project` and `--max-file-bytes` hidden).

## How to get to it (user POV)

Run `muster usage` in a terminal to see what your agents actually used this fortnight and what to
prune. In the panel, the Skills tab shows the same usage section under the skill list. The underlying
transcripts are the agent CLIs' own session logs under your home directory; muster never writes to
them.

## Driving it with muster-harness

```bash
$H launch $RUN
# Seed representative claude transcripts in the sandbox HOME so the scanner has
# real data (one session using a skill, a subagent and an MCP tool, one unused
# managed item to exercise `unused`):
TS=$(date -u +%Y-%m-%dT%H:%M:%SZ)
PROJ=/tmp/muster-verify/$RUN/work/demo
mkdir -p /tmp/muster-verify/$RUN/home/.claude/projects/-tmp-muster-verify-$RUN-work-demo
cat > /tmp/muster-verify/$RUN/home/.claude/projects/-tmp-muster-verify-$RUN-work-demo/s1.jsonl <<EOF
{"type":"assistant","sessionId":"s1","cwd":"$PROJ","timestamp":"$TS","message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"verify-skill"}}]}}
{"type":"assistant","sessionId":"s1","cwd":"$PROJ","timestamp":"$TS","message":{"content":[{"type":"tool_use","name":"Agent","input":{"subagent_type":"verify-agent"}}]}}
{"type":"assistant","sessionId":"s1","cwd":"$PROJ","timestamp":"$TS","message":{"content":[{"type":"tool_use","name":"mcp__docs__lookup","input":{}}]}}
EOF
$H api $RUN GET 'usage?since=14d'
$H cli $RUN -- --format json usage --since 14d
$H cli $RUN -- --format json usage --agent claude --since 1d
$H cli $RUN -- -- usage --since 0d          # expect ok:false exit 2 (0 not a positive day count)
$H api $RUN GET 'usage?since=bad'           # expect 400-ish failure, not a silent default
$H mcp $RUN tools-call usage '{"since":"7d"}'
$H screenshot $RUN skills skills-usage.png
```

Observable end state that proves it: the JSON result contains `items`, `unused`, `coverage`,
`sessions_scanned` and `sources`; the seeded transcript produces nonzero counts (`items[]` has the
`skill:verify-skill`, `subagent:verify-agent` and `mcp:docs` entries with `uses >= 1` and
`sessions_scanned.claude = 1`, `coverage` at least `partial` because codex is `partial`), and
`sources` lists `~/.claude/projects` as `supported` with `malformed_lines` and `skipped` counts;
`--agent claude` narrows `sessions_scanned` to claude only; without seeded transcripts a fresh
sandbox reports `coverage: "none"` and zero sessions; the panel Skills tab shows the usage table.

## Gotchas

- `--since` must be `<N>d`, 1..36500; `0d` and `14` fail with usage errors.
- The harness sandbox HOME starts empty, so seed transcripts as shown above (JSONL under
  `~/.claude/projects/<escaped-cwd>/`, lines with `type: "assistant"`, `sessionId`, `cwd`,
  RFC3339 `timestamp` and `message.content[]` tool_use blocks). Only records whose `timestamp` is
  inside the lookback window count; `Skill`/`Agent`/`mcp__<server>__<tool>` tool names map to
  skills/subagents/MCP servers.
- Transcripts larger than `--max-file-bytes` are counted in `skipped.too_large`, not read.
- The panel route hides `--project` and `--max-file-bytes` (it must not probe arbitrary dirs); the
  CLI keeps both.
- `unused` suggestions reference `muster disable ...` items — see `toggles.md` to act on them.
