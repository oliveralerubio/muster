//! Library-level tests of `engine::effective::compute`: the module that
//! turns configuration layers into the effective configuration (typed items
//! with their origin layer, per-adapter on-disk state and a scrubbed JSON
//! view). Real layer files in tempdirs, never mocks.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use muster::engine::effective::{self, Inputs, State, Verification};
use muster::registry::Params;
use serde_json::{json, Value};

/// `compute` resolves adapter roots from `HOME`/`XDG_CONFIG_HOME`, so every
/// test that needs a deterministic home holds this lock for its whole body.
static ENV: Mutex<()> = Mutex::new(());

/// Temporary home with a config repository (`<home>/.config/muster`) and
/// machine state (`<home>/.local/state/muster`), isolated from the real user.
struct Sandbox {
    _guard: MutexGuard<'static, ()>,
    home: tempfile::TempDir,
    saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl Sandbox {
    fn new() -> Self {
        let guard = ENV.lock().unwrap();
        let home = tempfile::TempDir::new().expect("temp home");
        let saved = ["HOME", "XDG_CONFIG_HOME"]
            .iter()
            .map(|k| (*k, std::env::var_os(k)))
            .collect();
        std::env::set_var("HOME", home.path());
        std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        for dir in [".config/muster", ".local/state/muster"] {
            std::fs::create_dir_all(home.path().join(dir)).unwrap();
        }
        Self {
            _guard: guard,
            home,
            saved,
        }
    }

    fn config(&self) -> PathBuf {
        self.home.path().join(".config/muster")
    }

    fn state(&self) -> PathBuf {
        self.home.path().join(".local/state/muster")
    }

    fn write(&self, rel: &str, contents: &str) {
        let path = self.config().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        for (key, value) in &self.saved {
            match value {
                Some(value) => std::env::set_var(key, value),
                None => std::env::remove_var(key),
            }
        }
    }
}

/// The real `skill list` listing for the sandbox, as the `effective` command
/// hands it to `compute`.
fn skill_listing() -> Value {
    let mut ctx = muster::context::Context::new(Default::default()).unwrap();
    (muster::commands::skill::LIST.handler)(&mut ctx, &Params::default()).unwrap()
}

fn compute(
    sandbox: &Sandbox,
    agent: Option<&str>,
    project: Option<&Path>,
) -> effective::EffectiveConfig {
    let listing = skill_listing();
    effective::compute(&Inputs {
        config_dir: &sandbox.config(),
        state_dir: &sandbox.state(),
        agent,
        project,
        skill_listing: &listing,
    })
    .unwrap()
}

const DEMO_SKILL: &str = "---\nname: demo\ndescription: Helpful skill\n---\nHello\n";
const REVIEWER: &str = "---\nname: reviewer\ndescription: Review changes\n---\nReview.\n";

#[test]
fn compute_reports_typed_items_with_team_layer_origin() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [agents.instructions]\n\
         source = \"AGENTS.md\"\n\
         [agents.skills]\n\
         source = \"skills\"\n\
         [agents.subagents]\n\
         source = \"agents\"\n\
         [mcp.tool]\n\
         command = \"tool\"\n",
    );
    sandbox.write("AGENTS.md", "Be kind.\n");
    sandbox.write("skills/demo/SKILL.md", DEMO_SKILL);

    let config = compute(&sandbox, None, None);

    assert_eq!(
        config.layers,
        vec![effective::LayerOrigin {
            kind: "team",
            name: "team".to_owned(),
        }]
    );
    // Instructions: origin layer, token estimate, per-adapter state.
    assert_eq!(config.instructions.len(), 1);
    let instructions = &config.instructions[0];
    assert_eq!(instructions.layer, Some("team"));
    assert_eq!(instructions.approx_tokens, 2);
    assert_eq!(instructions.adapters["claude"].state, State::Missing);
    assert_eq!(
        instructions.adapters["claude"].files[0].path,
        sandbox.home.path().join(".claude/AGENTS.md")
    );
    // MCP server: origin layer, transport, secret references.
    assert_eq!(config.mcp.len(), 1);
    let mcp = &config.mcp[0];
    assert_eq!(mcp.name, "tool");
    assert_eq!(mcp.layer, Some("team"));
    assert_eq!(mcp.overridden_layers, Vec::<&str>::new());
    assert_eq!(mcp.transport, "stdio");
    assert_eq!(mcp.agents, vec!["claude".to_owned()]);
    assert_eq!(mcp.adapters["claude"], State::Missing);
    assert!(mcp.secret_references.is_empty());
    // Skill: verification against skills.lock, token estimates, adapters.
    assert_eq!(config.skills.len(), 1);
    let skill = &config.skills[0];
    assert_eq!(skill.name, "demo");
    assert_eq!(skill.layer, Some("team"));
    assert!(!skill.customized);
    assert_eq!(skill.verification, Verification::Unreviewed);
    assert_eq!(skill.description_tokens, 3);
    assert_eq!(skill.body_tokens, 13);
    assert_eq!(skill.adapters["claude"].state, State::Missing);
    // Subagents need their own source; without one, none are reported.
    assert!(config.subagents.is_empty());
}

#[test]
fn compute_applies_profile_layers_after_the_team_layer() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [agents.subagents]\n\
         source = \"agents\"\n\
         [mcp.team-only]\n\
         command = \"a\"\n\
         [mcp.shared]\n\
         command = \"team\"\n\
         [profiles.extra]\n\
         [profiles.extra.mcp.shared]\n\
         command = \"profile\"\n\
         [machines.box]\n\
         recipient = \"age1fakebox\"\n\
         platform = \"linux-x86_64\"\n\
         muster_version = \"0.1.0\"\n\
         profile = \"extra\"\n",
    );
    sandbox.write("agents/reviewer.md", REVIEWER);
    // The machine layer only applies to the machine named in local state.
    std::fs::write(sandbox.state().join("machine"), "box\n").unwrap();

    let config = compute(&sandbox, None, None);

    assert_eq!(
        config.layers,
        vec![
            effective::LayerOrigin {
                kind: "team",
                name: "team".to_owned()
            },
            effective::LayerOrigin {
                kind: "machine",
                name: "box".to_owned()
            },
        ]
    );
    // `shared` is overridden by the machine's profile layer; the last layer
    // that declares it is its origin.
    let shared = config.mcp.iter().find(|m| m.name == "shared").unwrap();
    assert_eq!(shared.layer, Some("machine"));
    let team_only = config.mcp.iter().find(|m| m.name == "team-only").unwrap();
    assert_eq!(team_only.layer, Some("team"));
    // Subagents are read from the layer that declared them.
    assert_eq!(config.subagents.len(), 1);
    assert_eq!(config.subagents[0].name, "reviewer");
    assert_eq!(config.subagents[0].layer, Some("team"));
}

#[test]
fn compute_merges_the_project_layer_with_highest_precedence() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [mcp.tool]\n\
         command = \"team-tool\"\n",
    );
    let project = sandbox.home.path().join("project");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(
        project.join(".muster/muster.toml"),
        "version = 1\n\
         [mcp.local]\n\
         command = \"local-tool\"\n\
         [mcp.tool]\n\
         command = \"project-tool\"\n",
    )
    .unwrap();

    let config = compute(&sandbox, None, Some(&project));

    assert_eq!(
        config.layers,
        vec![
            effective::LayerOrigin {
                kind: "team",
                name: "team".to_owned()
            },
            effective::LayerOrigin {
                kind: "project",
                name: project.display().to_string()
            },
        ]
    );
    // The project's `tool` replaces the team's entry instead of duplicating
    // it, and reports the lower layer as overridden.
    let names: Vec<_> = config.mcp.iter().map(mcp_name).collect();
    assert_eq!(names, vec!["local", "tool"]);
    let tool = config.mcp.iter().find(|m| mcp_name(m) == "tool").unwrap();
    assert_eq!(tool.layer, Some("project"));
    assert_eq!(tool.overridden_layers, vec!["team"]);
    // The project layer declares no `[agents] enabled`: it inherits the
    // global one, so its items still get per-adapter views.
    let local = config.mcp.iter().find(|m| mcp_name(m) == "local").unwrap();
    assert_eq!(local.agents, vec!["claude".to_owned()]);
    assert_eq!(local.adapters["claude"], State::Missing);
}

fn mcp_name(m: &effective::McpItem) -> &str {
    &m.name
}

#[test]
fn compute_scrubs_secret_values_from_on_disk_state() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [mcp.tool]\n\
         command = \"tool\"\n\
         [mcp.tool.env]\n\
         TOKEN = \"native://app/token\"\n",
    );
    // The on-disk config holds the resolved secret this read-only view must
    // never touch or report.
    std::fs::write(
        sandbox.home.path().join(".claude.json"),
        r#"{"mcpServers":{"tool":{"command":"tool","env":{"TOKEN":"super-secret-value"}}}}"#,
    )
    .unwrap();

    let config = compute(&sandbox, None, None);

    let mcp = &config.mcp[0];
    assert_eq!(mcp.secret_references, vec!["native://app/token".to_owned()]);
    // Unresolvable secret values must not read as drift: the comparison
    // ignores the secret-bearing fields entirely.
    assert_eq!(mcp.adapters["claude"], State::InSync);
    let json = config.to_json().to_string();
    assert!(json.contains("native://app/token"), "{json}");
    assert!(!json.contains("super-secret-value"), "{json}");
}

#[test]
fn compute_tracks_skill_disk_states_and_extras() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [agents.skills]\n\
         source = \"skills\"\n",
    );
    sandbox.write("skills/demo/SKILL.md", DEMO_SKILL);

    let before = compute(&sandbox, None, None);
    assert_eq!(before.skills[0].adapters["claude"].state, State::Missing);
    // The adapter's skills directory exists as a location even when empty.
    assert_eq!(
        before.extra_on_disk,
        vec![("claude".to_owned(), Vec::<String>::new())]
    );

    // Install the skill byte-exact: the state becomes `installed`.
    let skill_dir = sandbox.home.path().join(".claude/skills");
    std::fs::create_dir_all(skill_dir.join("demo")).unwrap();
    std::fs::create_dir_all(skill_dir.join("personal")).unwrap();
    std::fs::write(skill_dir.join("demo/SKILL.md"), DEMO_SKILL).unwrap();

    let after = compute(&sandbox, None, None);
    assert_eq!(after.skills[0].adapters["claude"].state, State::Installed);
    assert_eq!(
        after.extra_on_disk,
        vec![("claude".to_owned(), vec!["personal".to_owned()])]
    );

    // A local edit reads as drift.
    std::fs::write(skill_dir.join("demo/SKILL.md"), "changed").unwrap();
    let drifted = compute(&sandbox, None, None);
    assert_eq!(drifted.skills[0].adapters["claude"].state, State::Drifted);
}

#[test]
fn compute_applies_toggles_and_counts_tokens_per_adapter() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\", \"codex\"]\n\
         skip_instructions = [\"codex\"]\n\
         [agents.instructions]\n\
         source = \"AGENTS.md\"\n\
         [agents.skills]\n\
         source = \"skills\"\n\
         [agents.toggles]\n\
         disable = [\"skill:demo@claude\", \"mcp:tool@claude\"]\n\
         [mcp.tool]\n\
         command = \"tool\"\n",
    );
    sandbox.write("AGENTS.md", "Be kind.\n");
    sandbox.write("skills/demo/SKILL.md", DEMO_SKILL);

    let config = compute(&sandbox, None, None);

    assert_eq!(
        config.instructions[0].adapters["claude"].state,
        State::Missing
    );
    assert_eq!(
        config.instructions[0].adapters["codex"].state,
        State::Skipped
    );
    assert_eq!(config.skills[0].adapters["claude"].state, State::Disabled);
    assert_eq!(config.skills[0].adapters["codex"].state, State::Missing);
    assert_eq!(config.mcp[0].adapters["claude"], State::Disabled);
    // A skill disabled for claude contributes no description tokens to its
    // totals; codex is skipped for instructions but keeps its skill tokens.
    assert_eq!(
        config.totals,
        vec![("claude".to_owned(), 2), ("codex".to_owned(), 3),]
    );
}

#[test]
fn compute_reports_provider_provenance_from_the_winning_layer() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [providers.fast]\n\
         kind = \"openai-compatible\"\n\
         base_url = \"https://team.example\"\n\
         default_model = \"m-team\"\n\
         [profiles.tuned]\n\
         [profiles.tuned.providers.fast]\n\
         default_model = \"m-machine\"\n\
         [machines.box]\n\
         recipient = \"age1fakebox\"\n\
         platform = \"linux-x86_64\"\n\
         muster_version = \"0.1.0\"\n\
         profile = \"tuned\"\n",
    );
    std::fs::write(sandbox.state().join("machine"), "box\n").unwrap();

    let config = compute(&sandbox, None, None);

    assert_eq!(config.providers.len(), 1);
    let provider = &config.providers[0];
    assert_eq!(provider.id, "fast");
    assert_eq!(provider.layer, Some("machine"));
    assert_eq!(
        provider.provider.default_model.as_deref(),
        Some("m-machine")
    );
}

#[test]
fn compute_restricts_to_one_adapter_and_rejects_unknown_agents() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\", \"codex\"]\n\
         [agents.instructions]\n\
         source = \"AGENTS.md\"\n",
    );
    sandbox.write("AGENTS.md", "Hello\n");

    let config = compute(&sandbox, Some("claude"), None);
    let adapters: Vec<&str> = config.instructions[0]
        .adapters
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(adapters, vec!["claude"]);

    let listing = skill_listing();
    let error = effective::compute(&Inputs {
        config_dir: &sandbox.config(),
        state_dir: &sandbox.state(),
        agent: Some("unknown-agent"),
        project: None,
        skill_listing: &listing,
    })
    .unwrap_err();
    assert!(
        error.to_string().contains("unknown agent `unknown-agent`"),
        "{error}"
    );
}

#[test]
fn compute_serializes_the_same_json_shape_as_the_command() {
    let sandbox = Sandbox::new();
    sandbox.write(
        "muster.toml",
        "version = 1\n\
         [agents]\n\
         enabled = [\"claude\"]\n\
         [agents.instructions]\n\
         source = \"AGENTS.md\"\n\
         [agents.skills]\n\
         source = \"skills\"\n\
         [mcp.tool]\n\
         command = \"tool\"\n",
    );
    sandbox.write("AGENTS.md", "Be kind.\n");
    sandbox.write("skills/demo/SKILL.md", DEMO_SKILL);

    let config = compute(&sandbox, None, None);
    let json = config.to_json();

    // Key order is part of the byte-identical contract with the command.
    assert_eq!(
        json.as_object().unwrap().keys().collect::<Vec<_>>(),
        vec![
            "totals",
            "subagents",
            "mcp",
            "extra_on_disk",
            "skills",
            "layers",
            "instructions",
            "providers"
        ]
    );
    assert_eq!(
        json["mcp"][0]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        vec![
            "name",
            "layer",
            "overridden_layers",
            "transport",
            "agents",
            "adapters",
            "secret_references"
        ]
    );
    assert_eq!(json["skills"][0]["name"], json!("demo"));
}
