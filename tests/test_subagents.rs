//! Custom subagent synchronization (SPEC US 22) for every adapter + schema.
//!
//! 1. `agents apply --yes` installs every subagent declared in
//!    `[agents.subagents]` at each adapter's documented agents path, user
//!    scope and project scope: byte-exact where the native format matches the
//!    canonical Markdown (Claude Code, Antigravity CLI), exactly translated
//!    where it differs (Codex TOML, OpenCode frontmatter).
//! 2. Removing a subagent from the source deletes the previously installed
//!    Muster-owned file on next apply; untracked user files survive.
//! 3. Invalid subagents (no frontmatter, missing name/description) emit a
//!    `subagent_invalid` warning and are skipped; exit stays 0.
//! 4. All four documented adapters support subagents: the capability is
//!    declared in `muster agents --format json` and no
//!    `unsupported_feature` warning is emitted.
//! 5. A second apply reports no changes; `muster schema` lists
//!    `agents.subagents`.

mod support;

use std::path::{Path, PathBuf};
use support::MusterEnv;

const REVIEW_MD: &str = "---\nname: review\ndescription: Reviews code for quality\ntools: Read, Grep\nmodel: gpt-6-luna\n---\n\nReview carefully.\n";
const EXPLORER_MD: &str =
    "---\nname: explorer\ndescription: Explores the repository\n---\nMap the code.\n";

/// Expected Codex translation of `REVIEW_MD` (toml_edit output).
fn codex_toml(name: &str, description: &str, model: Option<&str>, body: &str) -> String {
    let mut out = format!("name = \"{name}\"\ndescription = \"{description}\"\n");
    if let Some(model) = model {
        out.push_str(&format!("model = \"{model}\"\n"));
    }
    out.push_str(&format!("developer_instructions = \"\"\"\n{body}\"\"\"\n"));
    out
}

/// Expected OpenCode translation of a canonical subagent file.
fn opencode_md(description: &str, model: Option<&str>, body: &str) -> String {
    let mut out = format!("---\ndescription: {description}\nmode: subagent\n");
    if let Some(model) = model {
        out.push_str(&format!("model: {model}\n"));
    }
    out.push_str(&format!("---\n\n{}\n", body.trim()));
    out
}

/// Seed a subagents source with `review` (frontmatter with tools+model) and
/// `explorer` (minimal frontmatter).
fn seed_subagents(dir: &Path) {
    std::fs::create_dir_all(dir).expect("subagents dir");
    std::fs::write(dir.join("review.md"), REVIEW_MD).expect("write review.md");
    std::fs::write(dir.join("explorer.md"), EXPLORER_MD).expect("write explorer.md");
}

/// Write a muster.toml (subagents) with sources in `dir`.
fn seed_manifest(dir: &Path, agents: &[&str]) {
    std::fs::create_dir_all(dir).expect("config dir");
    seed_subagents(&dir.join("subagents"));
    let list: Vec<String> = agents.iter().map(|a| format!("\"{a}\"")).collect();
    std::fs::write(
        dir.join("muster.toml"),
        format!(
            r#"
version = 1

[agents]
enabled = [{}]

[agents.subagents]
source = "subagents"
"#,
            list.join(", ")
        ),
    )
    .expect("write manifest");
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()))
}

/// Installed user-scope agents dir for one adapter.
fn user_agents_dir(env: &MusterEnv, adapter: &str) -> PathBuf {
    match adapter {
        "claude" => env.path().join(".claude/agents"),
        "codex" => env.path().join(".codex/agents"),
        "opencode" => env.path().join(".config/opencode/agents"),
        "antigravity" => env.path().join(".gemini/antigravity-cli/agents"),
        other => panic!("unknown adapter {other}"),
    }
}

/// Byte-exact or exact translated install, user scope, per adapter.
fn assert_installed(base: &Path, adapter: &str) {
    match adapter {
        // Native format == canonical format: byte-exact.
        "claude" | "antigravity" => {
            assert_eq!(read(&base.join("review.md")), REVIEW_MD);
            assert_eq!(read(&base.join("explorer.md")), EXPLORER_MD);
        }
        // TOML custom-agent files.
        "codex" => {
            assert_eq!(
                read(&base.join("review.toml")),
                codex_toml(
                    "review",
                    "Reviews code for quality",
                    Some("gpt-6-luna"),
                    "\nReview carefully.\n"
                )
            );
            assert_eq!(
                read(&base.join("explorer.toml")),
                codex_toml(
                    "explorer",
                    "Explores the repository",
                    None,
                    "Map the code.\n"
                )
            );
        }
        // Markdown agent files with `mode: subagent`; the file name is the
        // agent name, so `name` is not written.
        "opencode" => {
            assert_eq!(
                read(&base.join("review.md")),
                opencode_md(
                    "Reviews code for quality",
                    Some("gpt-6-luna"),
                    "\nReview carefully."
                )
            );
            assert_eq!(
                read(&base.join("explorer.md")),
                opencode_md("Explores the repository", None, "\nMap the code.")
            );
        }
        other => panic!("unknown adapter {other}"),
    }
}

// ---------------------------------------------------------------------------
// 1. User-scope install
// ---------------------------------------------------------------------------

const ALL: &[&str] = &["claude", "codex", "opencode", "antigravity"];

#[test]
fn subagents_user_scope_claude() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["claude"]);
    env.run(&["agents", "apply", "--agent", "claude", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_agents_dir(&env, "claude"), "claude");
}

#[test]
fn subagents_user_scope_codex() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["codex"]);
    env.run(&["agents", "apply", "--agent", "codex", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_agents_dir(&env, "codex"), "codex");
}

#[test]
fn subagents_user_scope_opencode() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["opencode"]);
    env.run(&["agents", "apply", "--agent", "opencode", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_agents_dir(&env, "opencode"), "opencode");
}

#[test]
fn subagents_user_scope_antigravity() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    assert_installed(&user_agents_dir(&env, "antigravity"), "antigravity");
}

// ---------------------------------------------------------------------------
// 2. Project-scope install (layer in <project>/.muster/muster.toml)
// ---------------------------------------------------------------------------

#[test]
fn subagents_project_scope_all_agents() {
    let env = MusterEnv::new();
    env.run(&["--yes", "init"]).assert().code(0);
    let project = env.path().join("my-project");
    seed_manifest(&project.join(".muster"), ALL);
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .code(0);
    for (adapter, rel) in [
        ("claude", ".claude/agents"),
        ("codex", ".codex/agents"),
        ("opencode", ".opencode/agents"),
        ("antigravity", ".agents/agents"),
    ] {
        assert_installed(&project.join(rel), adapter);
    }
}

// ---------------------------------------------------------------------------
// 3. Stale deletion; untracked user files survive
// ---------------------------------------------------------------------------

#[test]
fn subagents_removed_from_source_are_deleted_user_files_survive() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let base = user_agents_dir(&env, "antigravity");
    // Untracked user file next to the files Muster will own.
    std::fs::create_dir_all(&base).expect("pre-create agents dir");
    let user_file = base.join("my-own.md");
    std::fs::write(&user_file, "---\nname: my-own\ndescription: mine\n---\n").expect("user file");

    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);
    assert!(
        base.join("explorer.md").is_file(),
        "explorer installed first"
    );

    // `explorer` disappears from the source.
    std::fs::remove_file(env.config_dir().join("subagents/explorer.md")).expect("remove source");
    env.run(&["agents", "apply", "--agent", "antigravity", "--yes"])
        .assert()
        .code(0);

    assert!(
        !base.join("explorer.md").exists(),
        "removed subagent must be deleted on next apply"
    );
    assert_eq!(read(&base.join("review.md")), REVIEW_MD);
    assert_eq!(
        read(&user_file),
        "---\nname: my-own\ndescription: mine\n---\n",
        "untracked user files must survive"
    );
}

// ---------------------------------------------------------------------------
// 4. Invalid subagents: warn once, skip, exit 0
// ---------------------------------------------------------------------------

#[test]
fn invalid_subagent_warns_and_is_skipped() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let source = env.config_dir().join("subagents");
    std::fs::write(source.join("nofrontmatter.md"), "just a body\n").expect("no frontmatter");
    std::fs::write(source.join("nodesc.md"), "---\nname: nodesc\n---\nbody\n")
        .expect("missing description");
    std::fs::write(
        source.join("noname.md"),
        "---\ndescription: has one\n---\nbody\n",
    )
    .expect("missing name");

    let (output, envelope) = env.run_json(&[
        "agents",
        "apply",
        "--yes",
        "--agent",
        "antigravity",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "apply must exit 0");
    assert!(envelope.ok(), "envelope must be ok: {}", envelope.raw);
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    let hits: Vec<_> = diags
        .iter()
        .filter(|d| d["code"] == "subagent_invalid")
        .collect();
    for name in ["nofrontmatter", "nodesc", "noname"] {
        assert_eq!(
            hits.iter()
                .filter(|d| d["message"]
                    .as_str()
                    .map(|m| m.contains(name))
                    .unwrap_or(false))
                .count(),
            1,
            "warn exactly once per invalid subagent: {hits:?}"
        );
    }
    let base = user_agents_dir(&env, "antigravity");
    for name in ["nofrontmatter.md", "nodesc.md", "noname.md"] {
        assert!(
            !base.join(name).exists(),
            "invalid subagent {name} must be skipped, not installed"
        );
    }
    assert!(
        base.join("review.md").is_file(),
        "valid subagents in the same source must still install"
    );
}

// ---------------------------------------------------------------------------
// 5. Capability declaration: every adapter supports subagents
// ---------------------------------------------------------------------------

#[test]
fn every_adapter_declares_subagents_without_unsupported_warning() {
    let env = MusterEnv::new();
    let (output, envelope) = env.run_json(&["agents", "--format", "json"]);
    assert!(output.status.success(), "agents must succeed");
    let adapters = envelope.result()["agents"].as_array().expect("adapters");
    for adapter in adapters {
        let capabilities: Vec<&str> = adapter["capabilities"]
            .as_array()
            .expect("capabilities list")
            .iter()
            .filter_map(|c| c.as_str())
            .collect();
        assert!(
            capabilities.contains(&"subagents"),
            "{} must declare the subagents capability: {capabilities:?}",
            adapter["id"]
        );
    }

    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), ALL);
    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success(), "apply must succeed");
    assert!(envelope.ok());
    let diags = envelope.diagnostics().as_array().expect("diagnostics");
    assert!(
        !diags.iter().any(|d| d["code"] == "unsupported_feature"),
        "no adapter may warn that subagents are unsupported: {diags:?}"
    );
    for adapter in ALL {
        assert!(
            user_agents_dir(&env, adapter).join("review.md").is_file()
                || user_agents_dir(&env, adapter).join("review.toml").is_file(),
            "{adapter} must install subagents"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. `tools` is translated only where the target format has an equivalent
// ---------------------------------------------------------------------------

#[test]
fn tools_field_warns_only_where_it_cannot_be_translated() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), ALL);
    let (_, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(envelope.ok());
    let diags = envelope.diagnostics().as_array().expect("diagnostics");
    let warnings: Vec<&str> = diags
        .iter()
        .filter(|d| d["code"] == "subagent_field_untranslated")
        .filter_map(|d| d["message"].as_str())
        .collect();
    assert_eq!(
        warnings.iter().filter(|m| m.contains("Codex")).count(),
        1,
        "codex warns once for review's tools: {warnings:?}"
    );
    assert_eq!(
        warnings.iter().filter(|m| m.contains("OpenCode")).count(),
        1,
        "opencode warns once for review's tools: {warnings:?}"
    );
    // Claude Code and Antigravity read `tools` natively: byte-exact, no warning.
    assert!(
        !warnings
            .iter()
            .any(|m| m.contains("Claude Code") || m.contains("Antigravity")),
        "native-format adapters must not warn: {warnings:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Idempotent second apply
// ---------------------------------------------------------------------------

#[test]
fn second_apply_reports_no_changes() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), ALL);
    env.run(&["agents", "apply", "--yes"]).assert().code(0);
    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply", "--yes"]);
    assert!(output.status.success(), "second apply must exit 0");
    assert!(envelope.ok(), "second apply envelope ok=true");
    assert_eq!(
        envelope.result()["changes"],
        0,
        "second apply must report no changes: {}",
        envelope.raw
    );
    for agent in envelope.result()["agents"].as_array().expect("agents") {
        assert_eq!(
            agent["status"], "unchanged",
            "every adapter must be unchanged on second apply: {agent}"
        );
    }
}

// ---------------------------------------------------------------------------
// 8. `muster schema` includes the new config key
// ---------------------------------------------------------------------------

#[test]
fn schema_includes_agents_subagents() {
    let env = MusterEnv::new();
    let (_, envelope) = env.run_json(&["schema", "--format", "json"]);
    assert!(output_ok(&envelope));
    let schema = &envelope.result()["manifest_schema"];
    // schemars references named structs from `$defs`; optional fields wrap the
    // ref in `anyOf` with a null type.
    let defs = &schema["$defs"];
    let properties = &defs["AgentsConfig"]["properties"];
    let subagents = properties
        .get("subagents")
        .expect("agents.subagents in schema");
    let reference = subagents["$ref"]
        .as_str()
        .or_else(|| {
            subagents["anyOf"]
                .as_array()
                .and_then(|options| options.iter().find_map(|o| o["$ref"].as_str()))
        })
        .and_then(|r| r.rsplit('/').next())
        .expect("subagents resolves to a definition");
    let source = &defs[reference]["properties"]["source"];
    assert_eq!(source["type"], "string", "subagents.source is a string");
}

fn output_ok(envelope: &support::Envelope) -> bool {
    envelope.ok()
}

// ---------------------------------------------------------------------------
// 9. QA round 1: frontmatter `name` cannot escape the agents directory
// ---------------------------------------------------------------------------

/// Invalid frontmatter `name`s: path traversal, separators, `..`, leading
/// dot, wrong case — and a valid name that does not match the file stem.
const BAD_NAMES: &[(&str, &str)] = &[
    ("escapee", "../../escaped"),
    ("nested", "sub/dir"),
    ("backslash", "..\\win"),
    ("dotdot", "a..b"),
    ("hidden", ".hidden"),
    ("shouty", "UPPER"),
    ("sneaky", "review"),
];

#[test]
fn invalid_frontmatter_names_warn_and_are_skipped() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let source = env.config_dir().join("subagents");
    for (stem, name) in BAD_NAMES {
        std::fs::write(
            source.join(format!("{stem}.md")),
            format!("---\nname: {name}\ndescription: tries to escape\n---\nbody\n"),
        )
        .expect("write bad subagent");
    }

    let (output, envelope) = env.run_json(&[
        "agents",
        "apply",
        "--yes",
        "--agent",
        "antigravity",
        "--format",
        "json",
    ]);
    assert!(
        output.status.success(),
        "apply must exit 0: {}",
        envelope.raw
    );
    assert!(envelope.ok());
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    let hits: Vec<_> = diags
        .iter()
        .filter(|d| d["code"] == "subagent_invalid")
        .filter_map(|d| d["message"].as_str())
        .collect();
    for (stem, name) in BAD_NAMES {
        assert_eq!(
            hits.iter().filter(|m| m.contains(stem)).count(),
            1,
            "warn exactly once for `{stem}` (name `{name}`): {hits:?}"
        );
    }

    // Nothing may land outside the agents directory, whatever the name.
    let agents = user_agents_dir(&env, "antigravity");
    for (stem, _) in BAD_NAMES {
        assert!(!agents.join(format!("{stem}.md")).exists());
    }
    for escaped in [
        env.path().join("escaped.md"),
        env.path().join(".gemini/escaped.md"),
        env.path().join(".gemini/antigravity-cli/escaped.md"),
    ] {
        assert!(
            !escaped.exists(),
            "{} must not be written from frontmatter name",
            escaped.display()
        );
    }
    // `sneaky.md` claims `name: review`: the legitimate `review` subagent
    // must survive untouched.
    assert_eq!(read(&agents.join("review.md")), REVIEW_MD);
    assert!(
        agents.join("review.md").is_file() && agents.join("explorer.md").is_file(),
        "valid subagents in the same source must still install"
    );
}

// ---------------------------------------------------------------------------
// 10. QA round 1: frontmatter is parsed as real YAML
// ---------------------------------------------------------------------------

#[test]
fn malformed_yaml_warns_and_is_skipped() {
    let env = MusterEnv::new();
    seed_manifest(&env.config_dir(), &["antigravity"]);
    let source = env.config_dir().join("subagents");
    std::fs::write(
        source.join("badflow.md"),
        "---\nname: [unclosed\ndescription: broken flow\n---\nbody\n",
    )
    .expect("write badflow");
    std::fs::write(
        source.join("badtab.md"),
        "---\nname: badtab\ndescription:\n\t- tab indentation is invalid YAML\n---\nbody\n",
    )
    .expect("write badtab");

    let (output, envelope) = env.run_json(&[
        "agents",
        "apply",
        "--yes",
        "--agent",
        "antigravity",
        "--format",
        "json",
    ]);
    assert!(output.status.success(), "apply must exit 0");
    let diags = envelope.diagnostics().as_array().expect("diagnostics list");
    let hits: Vec<_> = diags
        .iter()
        .filter(|d| d["code"] == "subagent_invalid")
        .filter_map(|d| d["message"].as_str())
        .collect();
    for stem in ["badflow", "badtab"] {
        assert_eq!(
            hits.iter().filter(|m| m.contains(stem)).count(),
            1,
            "malformed YAML `{stem}` must warn `subagent_invalid`: {hits:?}"
        );
    }
    let agents = user_agents_dir(&env, "antigravity");
    assert!(!agents.join("badflow.md").exists());
    assert!(!agents.join("badtab.md").exists());
    assert!(
        agents.join("review.md").is_file(),
        "valid files still install"
    );
}

const DETAILED_MD: &str = "---
name: detailed
description: |
  Reviews code with care.
  Second line.
tools:
  - Read
  - Grep
model: gpt-6-luna
---

Review carefully.
";

#[test]
fn block_scalars_and_list_tools_parse_and_translate() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    seed_manifest(&config, &["codex", "opencode"]);
    std::fs::write(config.join("subagents/detailed.md"), DETAILED_MD).expect("write detailed");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(
        output.status.success(),
        "apply must exit 0: {}",
        envelope.raw
    );
    assert!(envelope.ok(), "{}", envelope.raw);

    // List-form `tools` must warn where it cannot be translated (it used to
    // vanish silently).
    let untranslated: Vec<String> = envelope
        .diagnostics()
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == "subagent_field_untranslated")
        .filter_map(|d| d["message"].as_str().map(str::to_owned))
        .filter(|m| m.contains("detailed") && m.contains("tools"))
        .collect();
    assert_eq!(
        untranslated.iter().filter(|m| m.contains("Codex")).count(),
        1,
        "Codex must warn once for list-form tools: {untranslated:?}"
    );
    assert_eq!(
        untranslated
            .iter()
            .filter(|m| m.contains("OpenCode"))
            .count(),
        1,
        "OpenCode must warn once for list-form tools: {untranslated:?}"
    );

    // The block scalar must survive translation as one description.
    let codex: toml::Value =
        toml::from_str(&read(&user_agents_dir(&env, "codex").join("detailed.toml")))
            .expect("generated Codex TOML must parse");
    assert_eq!(
        codex.get("description").and_then(toml::Value::as_str),
        Some("Reviews code with care.\nSecond line."),
        "Codex description must keep the block-scalar text"
    );
    let opencode = parse_opencode_frontmatter(&read(
        &user_agents_dir(&env, "opencode").join("detailed.md"),
    ));
    assert_eq!(
        opencode["description"].as_str(),
        Some("Reviews code with care.\nSecond line."),
        "OpenCode description must keep the block-scalar text"
    );
    assert_eq!(opencode["mode"].as_str(), Some("subagent"));
    assert_eq!(opencode["model"].as_str(), Some("gpt-6-luna"));
}

const ANNOTATED_MD: &str = "---
name: annotated
description: Has extra fields
priority: high
mode: subagent
---

Body.
";

#[test]
fn unknown_frontmatter_fields_warn_untranslated_on_translating_adapters() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    seed_manifest(&config, ALL);
    std::fs::write(config.join("subagents/annotated.md"), ANNOTATED_MD).expect("write annotated");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(
        output.status.success(),
        "apply must exit 0: {}",
        envelope.raw
    );
    assert!(envelope.ok(), "{}", envelope.raw);
    let warnings: Vec<String> = envelope
        .diagnostics()
        .as_array()
        .expect("diagnostics")
        .iter()
        .filter(|d| d["code"] == "subagent_field_untranslated")
        .filter_map(|d| d["message"].as_str().map(str::to_owned))
        .collect();
    for (agent, field) in [
        ("Codex", "priority"),
        ("Codex", "mode"),
        ("OpenCode", "priority"),
        ("OpenCode", "mode"),
    ] {
        assert_eq!(
            warnings
                .iter()
                .filter(|m| m.contains(agent) && m.contains("annotated") && m.contains(field))
                .count(),
            1,
            "{agent} must warn once that `{field}` is not carried over: {warnings:?}"
        );
    }
    assert!(
        !warnings
            .iter()
            .any(|m| m.contains("Claude Code") || m.contains("Antigravity")),
        "byte-exact adapters keep every field and must not warn: {warnings:?}"
    );
    // The rest of the file is still translated, despite the warning.
    let codex: toml::Value = toml::from_str(&read(
        &user_agents_dir(&env, "codex").join("annotated.toml"),
    ))
    .expect("generated Codex TOML must parse");
    assert_eq!(
        codex.get("description").and_then(toml::Value::as_str),
        Some("Has extra fields")
    );
}

// ---------------------------------------------------------------------------
// 11. QA round 1: generated frontmatter is always valid YAML / TOML
// ---------------------------------------------------------------------------

const QUOTED_MD: &str = "---
name: quoted
description: \"Review: code, it's \\\"quoted\\\" & <special>\"
model: \"gpt-6 #main\"
---

Prompt with: colons, #hash and 'quotes'.
";

/// Split and parse the frontmatter of a generated OpenCode agent file.
fn parse_opencode_frontmatter(contents: &str) -> yaml_rust2::Yaml {
    let body = contents.strip_prefix("---\n").expect("opening ---");
    let end = body.find("\n---").expect("closing ---");
    yaml_rust2::YamlLoader::load_from_str(&body[..end])
        .expect("generated frontmatter must be valid YAML")
        .into_iter()
        .next()
        .expect("generated frontmatter must have a document")
}

#[test]
fn generated_frontmatter_parses_back_for_codex_and_opencode() {
    let env = MusterEnv::new();
    let config = env.config_dir();
    seed_manifest(&config, &["codex", "opencode"]);
    std::fs::write(config.join("subagents/quoted.md"), QUOTED_MD).expect("write quoted");

    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(
        output.status.success(),
        "apply must exit 0: {}",
        envelope.raw
    );
    assert!(envelope.ok(), "{}", envelope.raw);

    let description = "Review: code, it's \"quoted\" & <special>";
    let opencode =
        parse_opencode_frontmatter(&read(&user_agents_dir(&env, "opencode").join("quoted.md")));
    assert_eq!(opencode["description"].as_str(), Some(description));
    assert_eq!(opencode["mode"].as_str(), Some("subagent"));
    assert_eq!(opencode["model"].as_str(), Some("gpt-6 #main"));

    let codex: toml::Value =
        toml::from_str(&read(&user_agents_dir(&env, "codex").join("quoted.toml")))
            .expect("generated Codex TOML must parse");
    assert_eq!(
        codex.get("description").and_then(toml::Value::as_str),
        Some(description)
    );
    assert_eq!(
        codex.get("model").and_then(toml::Value::as_str),
        Some("gpt-6 #main")
    );
    assert_eq!(
        codex.get("name").and_then(toml::Value::as_str),
        Some("quoted")
    );
    assert_eq!(
        codex
            .get("developer_instructions")
            .and_then(toml::Value::as_str),
        Some("\nPrompt with: colons, #hash and 'quotes'.\n")
    );
}
