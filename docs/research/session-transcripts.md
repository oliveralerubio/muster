# Local session transcript investigation

Inspection on 2026-10-30 sampled local files and record keys only; no prompt or response text was retained. Formats are local implementation details and may change.

- **Claude Code**: `~/.claude/projects/**/*.jsonl`. Sampled assistant records contain `timestamp`, `sessionId`, `cwd`, and `message.content` tool-use blocks. Verified `Skill` with `input.skill`, `Agent` with `input.subagent_type`, and `mcp__<server>__<tool>` names. Confidence: high for these observed shapes; `Task` is an unverified older alias.
  Example (synthetic): `{"type":"assistant","timestamp":"2026-01-01T00:00:00Z","sessionId":"id","message":{"content":[{"type":"tool_use","name":"Skill","input":{"skill":"review"}}]}}`
- **Codex**: `~/.codex/sessions/**/*.jsonl`. Observed `response_item` records with `timestamp` and `payload.type` of `function_call` or `custom_tool_call`; payloads contain a name and arguments or input. Verified custom `exec` calls invoking `tools.exec_command` with `cat` on a skills path. Confidence: high for that observed form; MCP naming was not verified and is not counted.
  Example (synthetic): `{"type":"response_item","timestamp":"2026-01-01T00:00:00Z","payload":{"type":"function_call","name":"example","arguments":"{}"}}`
- **OpenCode**: files exist under `~/.local/share/opencode/`, but no transcript schema was verified. Unsupported.
- **Antigravity**: files exist under `~/.gemini/`, but no transcript schema was verified. Unsupported.
