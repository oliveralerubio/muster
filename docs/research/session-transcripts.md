# Local session transcript investigation

Inspection on 2026-10-30 sampled local files and record keys only; no prompt or response text was retained. Formats are local implementation details and may change.

- **Claude Code** (`supported`, `~/.claude/projects/**/*.jsonl`): Sampled assistant records contain `timestamp`, `sessionId`, `cwd`, and `message.content` tool-use blocks. Verified `Skill` with `input.skill`, `Agent` with `input.subagent_type`, and `mcp__<server>__<tool>` names. Confidence: high for these observed shapes; `Task` is an unverified older alias.
  Example (synthetic): `{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","sessionId":"id","message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"review"}}]}}`
- **Codex** (`partial`, `~/.codex/sessions/**/*.jsonl`):
  - **Skills** (supported, high confidence): Observed `response_item` records with `timestamp` and `payload.type` of `custom_tool_call` (`name: "exec"`) invoking `tools.exec_command` with `cat` on `<dir>/skills/<name>/SKILL.md`.
    Example (synthetic): `{"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"custom_tool_call","name":"exec","input":"const r = await tools.exec_command({cmd: 'cat /home/user/skills/review/SKILL.md'});"}}`
  - **MCP servers** (supported, high confidence): Observed `event_msg` records with `timestamp` and `payload.type` of `mcp_tool_call_end` containing `payload.invocation.server` and `payload.invocation.tool`. Only `payload.invocation.server` is read; `arguments` and `result` are ignored.
    Example (synthetic): `{"type":"event_msg","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"mcp_tool_call_end","call_id":"call_1","invocation":{"server":"docs","tool":"search","arguments":{}}}}`
  - **Subagents** (unsupported): Observed `spawn_agent` function calls and `sub_agent_activity` events use runtime roles/paths rather than canonical Muster subagent names; not counted.
- **OpenCode** (`unsupported`, `~/.local/share/opencode`): files exist under `~/.local/share/opencode/`, but no transcript schema was verified. Confidence: none (unsupported).
- **Antigravity** (`unsupported`, `~/.gemini`): files exist under `~/.gemini/`, but no transcript schema was verified. Confidence: none (unsupported).

