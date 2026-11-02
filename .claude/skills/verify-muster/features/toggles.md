# Toggles (enable / disable)

Turn managed items on or off without deleting them: skills, MCP servers and subagents, globally, per
adapter, per profile, or in a project layer (`<dir>/.muster/muster.toml`). The command is
`muster enable <item>` / `muster disable <item>`; `item` is `kind:name[@adapter]` with kind one of
`skill`, `mcp`, `subagent` (there is no `muster toggle` command; the panel buttons call the same
`enable`/`disable` registry commands).

## Sub-features

- Disable an item globally: appends it to `[agents.toggles] disable` in muster.toml and removes it
  from the `enable` list (`muster disable skill:verification-before-completion`).
- Enable it back: moves it to `enable` (`muster enable skill:verification-before-completion`).
- Per-adapter: `mcp:docs@codex` only affects the codex adapter.
- Per-profile: `--profile NAME` edits `[profiles.NAME.agents.toggles]` instead.
- Project layer: `--project DIR` edits `DIR/.muster/muster.toml` (mutually exclusive with `--profile`).
- Panel toggles: Skills tab and Profiles tab rows have Enable/Disable buttons posting `{"item":...}` to
  `/api/enable` and `/api/disable` (with `confirm: true`).

## How to get to it (user POV)

Open the panel, tab "Skills": every skill row has a Disable button (Enable once disabled); the
"Profiles & diff" tab has the same for MCP servers. CLI users run `muster disable skill:name`
(confirming with `--yes` when piped). The dry-run shows the item/action/path/changed result and
writes nothing.

## Driving it with muster-harness

```bash
$H launch $RUN
$H cli $RUN -- --yes skill add verify-skill --description 'Verify toggle behaviour'   # seed a real skill
$H tree $RUN before
$H cli $RUN -- --format json disable nonsense:no-such-item --yes   # expect ok:false, exit 2: parse_toggle rejects the unknown kind (only skill|mcp|subagent)
$H api $RUN POST disable '{"item":"skill:verify-skill"}'           # expect 400 (no confirm)
$H api $RUN POST disable '{"item":"skill:verify-skill","confirm":true}'
$H tree $RUN after-disable          # muster.toml sha256 changed, skills/ untouched
$H cli $RUN -- --format json effective --agent claude \
  | python3 -c 'import json,sys; e=json.load(sys.stdin)["result"]; \
    print([s["adapters"]["claude"]["state"] for s in e["skills"] if s["name"]=="verify-skill"])'
# expect ['disabled'] — the skill stays listed but its claude state is "disabled"
$H api $RUN POST enable '{"item":"skill:verify-skill","confirm":true}'
$H tree $RUN after-enable           # muster.toml back to a state with it enabled again
mkdir -p /tmp/muster-verify/$RUN/proj/.muster
cp /tmp/muster-verify/$RUN/home/.config/muster/muster.toml /tmp/muster-verify/$RUN/proj/.muster/muster.toml
$H cli $RUN -- --format json disable skill:verify-skill --project /tmp/muster-verify/$RUN/proj --yes
cat /tmp/muster-verify/$RUN/proj/.muster/muster.toml    # shows [agents.toggles] disable = ["skill:verify-skill"]
$H screenshot $RUN skills skills-toggles.png --eval "document.querySelectorAll('#view button').forEach(b=>{if(b.textContent==='Disable')b.click()})"
```

Observable end state that proves it: `tree` before/after shows only `muster.toml` changing and the
diff contains `[agents.toggles]` with the item moved between `disable` and `enable`; `muster
effective` reports the disabled skill's per-adapter `state` as `"disabled"` in the affected adapter
(it is NOT removed from the listing); the project toggle lands in the project's
`.muster/muster.toml`, not the global one. Two separate records — toggles do NOT write
`history.jsonl` events (the audit log only records secret operations); the change is proven by the
config repo's git commit `feat: disable skill:verify-skill`
(`git -C /tmp/muster-verify/$RUN/home/.config/muster log --oneline`), and the project-layer toggle
writes no commit at all (only the project `.muster/muster.toml` file changes).

## Gotchas

- The command is `muster enable`/`muster disable`; there is no `muster toggle` noun, and kind is
  `mcp` (not `mcp-server`). An unknown kind (`nonsense:foo`) fails with a usage error (exit 2), but
  a valid kind with an undeclared name still succeeds and writes the toggle line.
- `--project` and `--profile` together fail with a usage error.
- The target `muster.toml` must already exist and parse; toggling an item that is never declared
  still writes the toggle line (the item just stays absent from `effective`).
- Without `--yes` and without a TTY the command exits 2 on the confirmation; the web API needs
  `confirm: true` or answers 400.
- A no-op toggle (item already in the requested state) reports `changed: false` and writes nothing.
