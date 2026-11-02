# Effective configuration and `why`

Read-only provenance views. `muster effective` shows the merged configuration that would be written
to each adapter, with per-item sources (layer ids). `muster why <text>` searches the instruction
files and per-agent overlays (`AGENTS.<id>.md`, CLAUDE.md, ...) plus native context files and
reports which file, which layer and which agents contain the text; `--regex` treats the text as a
regular expression (case-insensitive).

## Sub-features

- `muster effective [--agent ID]`: totals per adapter, subagents, MCP servers, skills, extra files
  on disk, and the merged instructions text with its source layer.
- Project layer: `--project DIR` applies `DIR/.muster/muster.toml`; project MCP servers with the
  same name replace global ones, reported as `overridden` instead of duplicated.
- `muster why <text> [--agent ID] [--regex] [--project DIR]`: matches in instructions, overlays and
  native files, with the file path, source layer and agent ids per hit.
- Panel: the "Effective" tab renders `GET /api/effective`; `GET /api/why` and `GET /api/effective`
  are read-only routes with no `confirm`.
- MCP: the same commands are exposed as read-only tools `effective` and `why` (with `--project`
  hidden from agents).

## How to get to it (user POV)

Open the panel, click the "Effective" tab to see what each adapter would receive and from where. CLI
users run `muster effective` (or `--agent codex`) and ask `muster why "verification"` to find which
instruction file pulls a rule in, or why an item appears in the output.

## Driving it with muster-harness

```bash
$H launch $RUN
# Seed realistic sandbox content: a canonical instructions file and an MCP
# server entry in the sandbox config repo, so effective/why report real items.
printf '# Team instructions\n\nAlways run the muster verify pass.\n' \
  > /tmp/muster-verify/$RUN/home/.config/muster/AGENTS.md
# Seed a GLOBAL `docs` MCP server first (real CLI), then a project layer with a
# same-named entry, so `overridden_layers` is non-empty:
$H cli $RUN -- --yes mcp-server add docs --command docs-server
mkdir -p /tmp/muster-verify/$RUN/proj/.muster
cat > /tmp/muster-verify/$RUN/proj/.muster/muster.toml <<'EOF'
version = 1

[agents]
enabled = ["claude"]

[mcp.docs]
command = "docs-server"
args = ["--project"]
EOF
$H api $RUN GET effective
$H api $RUN GET 'why?text=verify'         # query params, no confirm needed
$H cli $RUN -- --format json effective --agent claude
$H cli $RUN -- --format json why 'verify pass' --regex
$H cli $RUN -- --format json effective --project /tmp/muster-verify/$RUN/proj   # project layer view
$H mcp $RUN tools-call effective
$H screenshot $RUN effective effective-view.png
```

Observable end state that proves it: `GET /api/effective` returns `ok: true` with `totals` per
enabled adapter and `result.mcp`/`result.subagents` matching `muster.toml`; `why 'verify'` finds the
seeded line in the sandbox config repo's `AGENTS.md` (kind `instructions`, the layer that provides
it) once the overlay files exist on disk — note `why` only scans files that exist, so with no
overlays rendered yet the instructions hit needs the seeded `AGENTS.md`; the `--agent` flag narrows
output to exactly that adapter; with a project layer, same-named project MCP entries appear once,
with `overridden_layers` listing the replaced layer kinds (observed with the seed above:
`"layer": "project"`, `"overridden_layers": ["team"]`).

## Gotchas

- `effective` with `--project` needs `DIR/.muster/muster.toml` to exist and parse; project layers
  inherit global toggles.
- `why` output is empty (not an error) when the text matches nothing; exit is still 0.
- `--regex` is always case-insensitive and capped at 1024 bytes of pattern; an invalid regex fails
  with a usage error (exit 2).
- Web routes: `GET /api/why?text=...&agent=...&regex=true` and `GET /api/effective?agent=...`
  (query parameters, not POST bodies); `project` IS accepted as a query parameter on both routes
  (`effective` takes `--project` via the shared PROJECT_PARAM, and only MCP agents and the panel
  hide it — the CLI keeps it).
- MCP tools `effective` and `why` hide `--project` from agents; the `usage` panel route hides
  `--project` and `--max-file-bytes` too.
