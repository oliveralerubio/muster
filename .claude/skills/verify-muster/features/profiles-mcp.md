# Profiles and MCP servers with diff preview

Declare agent configurations and MCP servers in `muster.toml`. The web panel allows creating and
editing profiles and MCP servers with a mandatory diff preview before confirmation.

## Sub-features

- List profiles (`GET /api/profiles`, `muster profile list`).
- Preview profile diff (`POST /api/profile/set` with `dry_run: true`, `muster profile set --dry-run`).
- Save profile (`POST /api/profile/set` with `confirm: true`, `muster profile set`).
- List MCP servers (`GET /api/mcp-servers`, `muster mcp-server list`).
- Preview MCP server diff (`POST /api/mcp-server/add` with `dry_run: true`, `muster mcp-server add --dry-run`).
- Save MCP server (`POST /api/mcp-server/add` with `confirm: true`, `muster mcp-server add`).
- Remove profile / MCP server (`POST /api/profile/rm`, `POST /api/mcp-server/rm`).
- Plan preview (`GET /api/plan`, `muster plan`).

## How to get to it (user POV)

Open the panel, tab **Profiles & diff preview**: "New profile" opens the inline TOML editor dialog.
The "Add or replace an MCP server" form allows entering command, args, env, and headers. Both forms
require clicking "Preview diff" to verify changes before the "Save" button is enabled.

## Driving it with muster-harness

```bash
$H launch $RUN
# 1. Profile edit with diff preview
$H api $RUN POST profile/set '{"name":"dev","toml":"[agents]\nenabled = [\"claude\"]\n","dry_run":true}'
# response contains "diff"
$H api $RUN POST profile/set '{"name":"dev","toml":"[agents]\nenabled = [\"claude\"]\n","confirm":true}'
$H api $RUN GET profiles

# 2. MCP server with diff preview
$H api $RUN POST mcp-server/add '{"name":"gh","command":"npx","args":["-y","@modelcontextprotocol/server-github"],"dry_run":true}'
$H api $RUN POST mcp-server/add '{"name":"gh","command":"npx","args":["-y","@modelcontextprotocol/server-github"],"confirm":true}'
$H api $RUN GET mcp-servers

# 3. Plan preview
$H api $RUN GET plan
$H screenshot $RUN profiles profiles.png
```

Observable end state: `GET /api/profiles` includes `dev`; `GET /api/mcp-servers` includes `gh`;
`GET /api/plan` shows planned changes for target agents.

## Gotchas

- `POST /api/profile/set` requires `name` and `toml` (inline TOML text); the `--from-file` parameter is intentionally blocked on the web API for security.
- In the web panel UI, any edit to the name or TOML fields after clicking "Preview diff" immediately disables the "Save" button until re-previewed.
- MCP environment variables and headers that reference secrets should use `native://group/name` references.
