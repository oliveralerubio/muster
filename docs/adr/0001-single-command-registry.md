# One command registry feeds CLI, web panel, MCP and `muster schema`

Muster is operated through three surfaces — the CLI, the local web panel
(`muster ui`) and the MCP server (`muster mcp`) — plus machine-readable
introspection (`muster schema`). Implementing each surface by hand would let
them drift: actions reachable from the panel but not the CLI, parameters that
behave differently per surface, help text that disagrees with the JSON command
list agents rely on.

## Decision

Every command is declared exactly once in a static registry,
`src/registry.rs`: a dotted name (`secret.add`), help text, typed parameters
(string, bool, int, list; optionally positional), a `mutating` flag, and one
handler. By convention a handler does not print, exit or read stdin: it returns
the envelope `result` and reports diagnostics and confirmations through the
shared context (`ctx.diag`, `ctx.confirm`), with explicit exceptions — `init`
prints the recovery key, `ui` serves and writes its startup output, and the
secret commands read values from stdin through the context. The CLI, the web
API and the MCP server are thin adapters over this registry — the CLI derives
its nested clap subcommand tree from the dotted names, and `muster schema`
serves the JSON command list (`commands_json`).

## Consequences

- Every operation the web panel or the MCP server exposes is a registry
  command: the same spec (help text, typed parameters, `mutating` flag) and the
  same handler as the CLI. The registry guarantees no drift in what an
  operation means; it does not make the interfaces automatically at parity —
  each one explicitly selects which operations it exposes. The MCP server
  offers six chosen tools (`status`, `doctor`, `plan`, `secret_list_meta`,
  `propose_change`, `secret_reveal`), and the web panel serves its own explicit
  route table over the registry; the CLI remains the complete surface.
- Adding a command is one registration: CLI parsing, help text and the JSON
  command list all follow; an interface picks it up only when its selection
  includes it.
- Cross-cutting guarantees (one JSON envelope, stable exit codes, no prompts
  without a TTY unless `--yes`) are implemented once, in the context layer.
- The no-print/no-stdin/no-exit handler contract is a convention with the
  explicit exceptions noted above, not an enforced constraint; it shapes how
  command code is written but keeps every surface consistent.
