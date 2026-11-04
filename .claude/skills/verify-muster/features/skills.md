# Skills lifecycle

Manage agent skills as part of the config repo: scaffold your own, or subscribe to a git repository of
skills, see whether upstream moved, pull updates, detach or remove. Provenance is pinned in `skills.lock`.

## Sub-features

- List with origin, commit and last-known upstream status (`skill list`, `GET /api/skills`, offline).
- Add own skill (`skill add NAME --description ...`) or subscribe (`skill add --url URL --path DIR`).
- Check upstream (`skill check`, `POST /api/skills/check`) writes `$XDG_CACHE_HOME/muster/skills-status.json`.
- Update (`skill update --all|NAME [--force]`), never removes.
- Remove or detach (`skill rm NAME [--detach]`).
- Import skills installed directly in an agent folder (`skill import NAME [--adapter ID]` or `--all`, `POST /api/skills/import`); `skill list` / `GET /api/skills` report candidates under `unmanaged`. When `~/.agents/.skill-lock.json` names the upstream repository, import subscribes (adds or extends a `[[agents.skills.remote]]`, vendors the upstream files, reports `replaced_local_copy`); otherwise it imports an own skill.
- Auto-update (`skill auto-update on|off`, `POST /api/skills/auto-update`, `auto_update` in `skill list`): `muster sync` (no `--yes`, no TTY) updates subscribed skills between pull and push, reports `skills_updated`, skips locally modified skills, warns on unreachable sources.
- Editing an own skill (`skill file set`, `POST /api/skills/file`) re-pins it in `skills.lock`; subscribed skills are not re-pinned.

## How to get to it (user POV)

Panel tab "Skills" (red dot = needs attention), "Check repositories", "Update all" and "Auto-update" buttons, a Source column and "Subscribe"/"Import as own"/"Import all" in the unmanaged section, per-skill update/remove actions;
CLI `muster skill ...`.

## Driving it with muster-harness

```bash
W=$(mktemp -d /tmp/muster-verify-up.XXXX)          # local bare git repo as upstream
mkdir -p $W/work/skills/hello && cd $W/work && git init -q -b main
printf -- '---\nname: hello\ndescription: Say hello\n---\nv1\n' > skills/hello/SKILL.md
git add . && git -c user.name=t -c user.email=t@t commit -qm v1 && git clone -q --bare . $W/up.git && cd -
$H launch $RUN
$H cli $RUN -- --yes skill add --url $W/up.git --path skills
$H cli $RUN -- --format json skill list 2>/dev/null | jq -c .result
# upstream moves
cd $W/work && printf -- '---\nname: hello\ndescription: Say hello\n---\nv2\n' > skills/hello/SKILL.md \
  && git -c user.name=t -c user.email=t@t commit -qam v2 && git push -q $W/up.git main; cd -
$H api $RUN POST skills/check '{}'                  # status "update_available"
$H cli $RUN -- --yes skill update --all
cat /tmp/muster-verify/$RUN/home/.config/muster/skills/hello/SKILL.md      # shows v2
$H cli $RUN -- --yes skill rm hello
rm -rf $W
```

Observable end state: after add, `skills/hello/` exists and `skills.lock` has `[[skill]] name="hello"
origin="up" commit=<sha>`, and `muster.toml` has `[[agents.skills.remote]]`; after upstream push + check,
`/api/skills` shows `status:"update_available"` with `upstream_commit` differing from `commit` and
`attention:1`; after update, SKILL.md contains v2 and lock commit equals upstream; after rm the directory is
gone.

## Gotchas

- `skill list` is offline: status is only fresh after `skill check` (cache file in `.cache/muster/`).
- `skill rm` of a subscribed skill reports `source_removed:true` but the source stays in muster.toml; a later
  `skill add --url` of the same repo re-vendors it.
- Local-path URLs work as upstream; no network needed. Git needs an identity only for building your fixture.
- Names must be lowercase letters, digits, `-`.
- `skill update` never deletes upstream-removed skills; that is `skill rm`.
