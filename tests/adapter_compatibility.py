#!/usr/bin/env python3
"""Compare real adapter behavior against a baseline binary using verify-muster.

Usage: python3 tests/adapter_compatibility.py BASE_BINARY CANDIDATE_BINARY EVIDENCE_DIR
The binaries run sequentially in fresh sandboxes at the same path, so native
files (including runtime helper commands) can be compared byte for byte.
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

REPO = Path(__file__).resolve().parents[1]
HARNESS = REPO / ".claude/skills/verify-muster/harness/muster-harness"
AGENTS = ["claude", "codex", "opencode", "antigravity", "pi", "hermes"]
DUMMIES = {name: f"verify-dummy-adapter-{name}-123" for name in ("mcp", "api", "old")}


def normalize(value):
    # Fresh init creates different age recipients and git commits. Neither is
    # an adapter result. Keep all other fields, messages and array ordering.
    if isinstance(value, dict):
        return {normalize(key): normalize(item) for key, item in value.items()}
    if isinstance(value, list):
        return [normalize(item) for item in value]
    if isinstance(value, str):
        value = re.sub(r"\.backup\.\d+", ".backup.<timestamp>", value)
        value = re.sub(r"age1[0-9a-z]{58}", "<age-recipient>", value)
        value = re.sub(r"\b[0-9a-f]{40}\b", "<git-commit>", value)
        value = re.sub(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d+)?Z", "<timestamp>", value)
    return value


def scenario(binary, destination, run_id):
    destination.mkdir(parents=True)
    harness_path = binary.parents[2] / HARNESS.relative_to(REPO)
    assert harness_path.is_file(), "binary must be in its checkout target/debug or target/release directory"
    env = dict(os.environ, MUSTER_BIN=str(binary), MUSTER_VERIFY_ROOT="/tmp/muster-verify",
               MUSTER_VERIFY_EVIDENCE_ROOT=str(destination / "harness"))
    scratch = Path("/tmp/muster-verify") / run_id
    home = scratch / "home"
    config = home / ".config/muster"
    project = scratch / "project"
    outputs = destination / "outputs"
    outputs.mkdir()
    index = 0

    def harness(*args, stdin=None, expected=(0,)):
        result = subprocess.run([str(harness_path), args[0], run_id, *args[1:]],
                                input=stdin, capture_output=True, text=True, env=env)
        if result.returncode not in expected:
            # Error outputs can themselves hold credentials: keep them in the
            # evidence, never include plaintext in a raised exception.
            (destination / "failure.txt").write_text(result.stdout + result.stderr)
            raise AssertionError(f"harness {args[0]} exited {result.returncode}; see {destination}")
        return result

    def cli(*args, stdin=None, expected=(0,)):
        nonlocal index
        options = ("--stdin", "-") if stdin is not None else ()
        result = harness("cli", *options, "--", "--format", "json", "--yes", *args,
                         stdin=stdin, expected=expected)
        assert not any(secret in result.stdout for secret in DUMMIES.values()), "CLI secret leak"
        assert not any(secret in result.stderr for secret in DUMMIES.values()), "stderr secret leak"
        envelope = json.loads(result.stdout)
        index += 1
        (outputs / f"{index:02d}-{'-'.join(args[:2]).replace('/', '_')}.json").write_text(
            json.dumps({"exit": result.returncode, "envelope": normalize(envelope)}, indent=2) + "\n")
        return envelope

    try:
        harness("launch")
        harness("doctor")
        for name in ("mcp", "api"):
            cli("secret", "add", f"verify/{name}", "--value-stdin", stdin=DUMMIES[name])
        original = (config / "muster.toml").read_text()
        manifest = '''
[agents]
enabled = ["claude", "codex", "opencode", "antigravity", "pi", "hermes"]
[agents.instructions]
source = "AGENTS.md"
[agents.skills]
source = "skills"
[agents.subagents]
source = "subagents"
[agents.models]
hermes = "hermes-model"
[agents.providers]
claude = "anthropic"
codex = "openai"
opencode = "anthropic"
[providers.anthropic]
kind = "anthropic"
base_url = "https://example.invalid/anthropic"
default_model = "anthropic-model"
key = "native://verify/api"
api_key_helper = true
[providers.openai]
kind = "openai-compatible"
base_url = "https://example.invalid/v1"
default_model = "openai-model"
key = "native://verify/api"
[mcp.stdio]
command = "example-mcp"
args = ["--stdio"]
[mcp.stdio.env]
TOKEN = "native://verify/mcp"
[mcp.remote]
url = "https://example.invalid/mcp"
[mcp.remote.headers]
Authorization = "native://verify/mcp"
'''
        # Secret add can declare an access table. Replace only the original
        # init prefix, then explicitly declare the policy for both scopes.
        original = original.split('[secrets.')[0]
        policy = ""
        for name in ("mcp", "api"):
            grants = AGENTS + [agent + "@project" for agent in AGENTS]
            policy += f'\n[secrets."native://verify/{name}"]\nallow = {json.dumps(grants)}\n'
        (config / "muster.toml").write_text(original + manifest + policy)
        (config / "AGENTS.md").write_text("# Canonical\nFollow the compatibility rule.\n")
        for agent in ("claude", "codex"):
            (config / f"AGENTS.{agent}.md").write_text(f"{agent} overlay compatibility rule.\n")
        skill = config / "skills/compatibility"
        skill.mkdir(parents=True)
        (skill / "SKILL.md").write_text("---\nname: compatibility\ndescription: Check compatibility\n---\nSkill compatibility rule.\n")
        (skill / "refs").mkdir()
        (skill / "refs/details.md").write_text("Nested compatibility reference.\n")
        (config / "subagents").mkdir()
        (config / "subagents/reviewer.md").write_text("---\nname: reviewer\ndescription: Review compatibility\ntools: Read\nmodel: sonnet\n---\nSubagent compatibility rule.\n")
        cli("skill", "pin", "compatibility")
        project_config = project / ".muster"
        project_config.mkdir(parents=True)
        (project_config / "muster.toml").write_text("version = 1\n" + manifest)
        for source in ("AGENTS.md", "AGENTS.claude.md", "AGENTS.codex.md", "skills", "subagents", "skills.lock"):
            path = config / source
            if path.is_dir():
                shutil.copytree(path, project_config / source)
            elif path.exists():
                shutil.copy2(path, project_config / source)
        for root in (home, project):
            (root / ".claude").mkdir(exist_ok=True)
            (root / ".claude/settings.json").write_text(json.dumps({
                "env": {"ANTHROPIC_API_KEY": DUMMIES["old"], "KEEP": "public"},
                "custom": "preserve this setting"}))
        cli("agents")
        cli("plan")
        cli("agents", "apply")
        cli("plan")
        cli("plan", "--project", str(project))
        # agents apply has no project parameter; sync is the public project
        # apply command and calls the same engine without a remote here.
        cli("sync", "--project", str(project))
        cli("plan", "--project", str(project))
        for query in ("compatibility", "overlay", "reviewer"):
            cli("why", query)
            cli("why", query, "--project", str(project))
        cli("why", "compatibility", "--agent", "opencode")
        cli("doctor", "--offline", expected=(0, 1))
        api = harness("api", "GET", "plan")
        assert not any(secret in api.stdout for secret in DUMMIES.values()), "API secret leak"
        (outputs / "api-plan.json").write_text(json.dumps(normalize(json.loads(api.stdout[api.stdout.index("{"):])), indent=2) + "\n")

        # Check the plaintext-credential warnings, unsupported provider kinds,
        # and subscription login messages in addition to the helper scenario.
        path = config / "muster.toml"
        helper_manifest = path.read_text()
        path.write_text(helper_manifest.replace("api_key_helper = true", "api_key_helper = false"))
        cli("plan")
        cli("doctor", "--offline", expected=(0, 1))
        login_manifest = helper_manifest.replace('key = "native://verify/api"\n', '')
        login_manifest = login_manifest.replace('kind = "openai-compatible"', 'kind = "openai"')
        path.write_text(login_manifest)
        cli("doctor", "--offline", expected=(0, 1))
        path.write_text(helper_manifest.replace('codex = "openai"', 'codex = "anthropic"'))
        cli("plan")
        path.write_text(helper_manifest)

        tree = destination / "agent-files"
        tree.mkdir()
        for relative in (".claude", ".claude.json", ".codex", ".agents", ".gemini", ".pi", ".hermes", ".config/opencode"):
            source = home / relative
            target = tree / "home" / relative
            target.parent.mkdir(parents=True, exist_ok=True)
            if source.is_dir():
                shutil.copytree(source, target)
            elif source.exists():
                shutil.copy2(source, target)
        shutil.copytree(project, tree / "project", ignore=shutil.ignore_patterns(".muster"))
        assert all((tree / "home" / relative).exists() for relative in (
            ".claude/skills/compatibility/SKILL.md", ".agents/skills/compatibility/SKILL.md",
            ".config/opencode/skills/compatibility/SKILL.md", ".gemini/antigravity-cli/skills/compatibility/SKILL.md",
            ".pi/agent/skills/compatibility/SKILL.md", ".hermes/skills/compatibility/SKILL.md"))
        assert all((tree / "project" / relative).exists() for relative in (
            ".claude/skills/compatibility/SKILL.md", ".agents/skills/compatibility/SKILL.md",
            ".opencode/skills/compatibility/SKILL.md", ".pi/skills/compatibility/SKILL.md",
            ".hermes/skills/compatibility/SKILL.md"))
        # Claude's real `auth status` initializes its own machine identity and
        # dated backup. Normalize only those native-CLI fields, preserving all
        # whitespace and every other byte of the generated files.
        for file in list(tree.rglob("*")):
            if not file.is_file():
                continue
            if file.name == ".claude.json" or file.name.startswith(".claude.json.backup."):
                contents = file.read_text()
                contents = re.sub(r'("firstStartTime"\s*:\s*")[^"]+(")', r'\1<timestamp>\2', contents)
                contents = re.sub(r'("machineID"\s*:\s*")[^"]+(")', r'\1<machine-id>\2', contents)
                file.write_text(contents)
                if file.name.startswith(".claude.json.backup."):
                    file.rename(file.with_name(re.sub(r"\.backup\.\d+$", ".backup.<timestamp>", file.name)))
        modes = {str(file.relative_to(tree)): oct(file.stat().st_mode & 0o777)
                 for file in sorted(tree.rglob("*")) if file.is_file()}
        (outputs / "native-modes.json").write_text(json.dumps(modes, indent=2) + "\n")
        assert "## AGENTS.claude.md" in (tree / "project/CLAUDE.md").read_text()
        assert "codex overlay" not in (tree / "project/AGENTS.md").read_text()

        # Native settings intentionally contain granted MCP/provider secrets.
        # Record only paths, then require identical occurrences across A/B.
        occurrences = {}
        for name, value in DUMMIES.items():
            matches = []
            for file in tree.rglob("*"):
                if file.is_file() and value.encode() in file.read_bytes():
                    matches.append(str(file.relative_to(tree)))
            occurrences[name] = sorted(matches)
        assert occurrences["mcp"] and occurrences["api"]
        assert not occurrences["old"], "old credentials survived the runtime helper migration"
        (outputs / "native-secret-paths.json").write_text(json.dumps(occurrences, indent=2) + "\n")
        for file in outputs.iterdir():
            assert not any(value in file.read_text() for value in DUMMIES.values()), "output secret leak"
        (destination / "redaction.txt").write_text("No dummy plaintext in CLI/API outputs. Native plaintext paths recorded for byte comparison. Old credential removed.\n")
        harness("tree", "after")
    finally:
        harness("cleanup")


def main():
    if len(sys.argv) != 4:
        raise SystemExit(__doc__)
    base, candidate = (Path(arg).resolve() for arg in sys.argv[1:3])
    evidence = Path(sys.argv[3]).resolve()
    evidence.mkdir(parents=True, exist_ok=True)
    run_id = f"adapter-ab-{os.getpid()}"
    for label, binary in (("base", base), ("candidate", candidate)):
        scenario(binary, evidence / label, run_id)
    for directory in ("outputs", "agent-files"):
        result = subprocess.run(["diff", "-r", str(evidence / "base" / directory),
                                 str(evidence / "candidate" / directory)], capture_output=True, text=True)
        (evidence / f"{directory}.diff").write_text(result.stdout + result.stderr)
        assert result.returncode == 0, f"{directory} differ; see {evidence / (directory + '.diff')}"
        print(f"diff -r base/{directory} candidate/{directory}: identical (exit 0)")
    print(f"CLI/API redaction and generated native secret paths: identical; evidence {evidence}")


if __name__ == "__main__":
    main()
