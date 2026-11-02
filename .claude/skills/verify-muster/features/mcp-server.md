# MCP server (read-only harness tools over stdio)

`muster mcp` runs a JSON-RPC 2.0 server over stdio (one message per line) that exposes read-only
tools to coding agents: `status`, `doctor` (`--fix` hidden), `plan`, `effective` and `why` and
`usage` (`--project` hidden), `provider_list`, `secret_list_meta`, `propose_change` (records a
proposal and returns its plan, never applies it) and `secret_reveal` (the only writing-ish tool:
`reveal` forced true, audited as the calling agent). Responses are the same JSON envelopes as
`muster --format json <command>`.

## Sub-features

- `initialize`: negotiates protocol version (supports `2025-06-18`, `2025-03-26`, `2024-11-05`),
  advertises `capabilities.tools`, and reads `clientInfo.name` to identify the calling agent —
  reveals are audited under that agent id.
- `tools/list`: the tool table with input schemas derived from the command registry.
- `tools/call`: runs the registry command; boolean hidden params are never offered, forced params
  are always set.
- Changes: only via `propose_change` → a human runs `muster proposal accept <id>` then
  `muster sync`.
- `ping` and notifications (no `id`) get an empty reply / no reply; unknown methods return
  `method not found`.

## How to get to it (user POV)

An agent host (Claude Code, Codex, ...) adds `muster mcp` as a stdio MCP server. The agent calls
`tools/list` and `tools/call` like any MCP server; muster answers with its usual envelopes. Humans
never interact with it directly except to accept proposals the agent made.

## Driving it with muster-harness

```bash
$H launch $RUN
$H mcp $RUN initialize                        # protocolVersion, serverInfo muster, instructions
$H mcp $RUN tools-list                        # 10 tools incl. effective, why, usage, secret_reveal
$H mcp $RUN tools-call status
$H mcp $RUN tools-call effective '{}'
$H mcp $RUN tools-call why '{"text":"instructions"}'
$H mcp $RUN tools-call nope '{}'              # expect error, exit 1 (unknown tool)
$H mcp $RUN tools-call propose_change '{"path":"docs/note.md","contents":"hi","message":"note"}'
$H cli $RUN -- --format json proposal list    # the proposal exists; nothing applied
```

Observable end state that proves it: `initialize` returns `serverInfo.name == "muster"`;
`tools/list` contains exactly the ten read-only tool names; `tools/call status` returns the same
`result` as `$H cli $RUN -- --format json status`; `propose_change` creates a proposal (visible in
`proposal list`, and the target file is untouched until a human accepts and syncs); each call leaves
an evidence file with the exact request and response.

## Gotchas

- The harness `mcp` subcommand sends `initialize` (identifying as a claude-code client, so reveals
  are audited to agent `claude`) followed by your one request, in a single `muster mcp` process;
  tool calls work without the explicit `initialize` action.
- MCP reveal gate: `secret_reveal` first requires the reference in `[mcp_server] reveal` in
  muster.toml, then an identified client, then the `[secrets...] allow` grant — each gate has its
  own `security` error message.
- No TTY: `secret_reveal` for a reference with `approval = "each"` fails with
  `secret_approval_required` — that is the human-approval gate, see `secret-leases.md`.
- Responses are line-delimited JSON; the harness parses the reply whose `id` matches and reports
  exit 1 on a JSON-RPC error, so a non-zero exit is a failed call, not a crash.
- `--project` is never offered to agents (arbitrary directory access); `doctor --fix` is hidden
  (write).
- The MCP server runs inside the sandbox when driven by the harness, so it sees the sandbox HOME and
  empty transcripts — same scoping as `$H cli`.
