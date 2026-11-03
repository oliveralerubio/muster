//! Pi adapter: subagents, packages and default enablement.
mod support;

use serde_json::Value;
use support::MusterEnv;

const SUBAGENT: &str =
    "---\nname: reviewer\ndescription: Review code\ntools: read, grep\nfuture: kept\n---\nReview carefully.\n";

fn seed(env: &MusterEnv, manifest: &str) {
    let cfg = env.config_dir();
    std::fs::create_dir_all(cfg.join("subagents")).unwrap();
    std::fs::write(cfg.join("subagents/reviewer.md"), SUBAGENT).unwrap();
    std::fs::write(cfg.join("muster.toml"), manifest).unwrap();
}

fn apply(env: &MusterEnv) -> support::Envelope {
    let (output, envelope) = env.run_json(&["agents", "apply", "--yes", "--format", "json"]);
    assert!(output.status.success(), "{:?}", envelope.raw);
    envelope
}

fn settings(env: &MusterEnv) -> Value {
    let text = std::fs::read_to_string(env.path().join(".pi/agent/settings.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

const PI_PACKAGES: &str = r#"
version = 1
[agents]
enabled = ["pi"]
[agents.subagents]
source = "subagents"
[agents.packages]
pi = ["npm:pi-subagents@1.0.0", "git:github.com/acme/ext@v2", "./pi/my-ext", "../shared/ext", "/opt/abs-ext"]
"#;

#[test]
fn subagents_install_byte_exact_at_user_and_project_scope() {
    let env = MusterEnv::new();
    seed(
        &env,
        "version = 1\n[agents]\nenabled = [\"pi\"]\n[agents.subagents]\nsource = \"subagents\"\n",
    );
    apply(&env);
    let user = std::fs::read_to_string(env.path().join(".pi/agent/agents/reviewer.md")).unwrap();
    assert_eq!(user, SUBAGENT);

    let project = env.path().join("proj");
    std::fs::create_dir_all(project.join(".muster/subagents")).unwrap();
    std::fs::write(
        project.join(".muster/muster.toml"),
        "version = 1\n[agents]\nenabled = [\"pi\"]\n[agents.subagents]\nsource = \"subagents\"\n",
    )
    .unwrap();
    std::fs::write(project.join(".muster/subagents/reviewer.md"), SUBAGENT).unwrap();
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .assert()
        .success();
    assert_eq!(
        std::fs::read_to_string(project.join(".pi/agents/reviewer.md")).unwrap(),
        SUBAGENT
    );
}

#[test]
fn packages_are_written_with_local_paths_made_absolute_and_other_keys_kept() {
    let env = MusterEnv::new();
    seed(&env, PI_PACKAGES);
    let path = env.path().join(".pi/agent/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"theme":"dark","packages":["npm:manual"]}"#).unwrap();
    apply(&env);
    let s = settings(&env);
    assert_eq!(s["theme"], "dark");
    let cfg = env.config_dir();
    assert_eq!(
        s["packages"],
        serde_json::json!([
            "npm:pi-subagents@1.0.0",
            "git:github.com/acme/ext@v2",
            cfg.join("./pi/my-ext").to_string_lossy(),
            cfg.join("../shared/ext").to_string_lossy(),
            "/opt/abs-ext"
        ])
    );
    assert!(cfg
        .join("./pi/my-ext")
        .to_string_lossy()
        .starts_with(&*cfg.to_string_lossy()));
    // Idempotent.
    let second = apply(&env);
    for agent in second.result()["agents"].as_array().unwrap() {
        for file in agent["files"].as_array().unwrap() {
            assert_eq!(file["changed"], false);
        }
    }
}

#[test]
fn removing_packages_prunes_the_list_and_keeps_other_keys() {
    let env = MusterEnv::new();
    seed(&env, PI_PACKAGES);
    let path = env.path().join(".pi/agent/settings.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
    apply(&env);
    assert!(settings(&env).get("packages").is_some());
    std::fs::write(
        env.config_dir().join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"pi\"]\n",
    )
    .unwrap();
    apply(&env);
    let s = settings(&env);
    assert!(s.get("packages").is_none(), "{s}");
    assert_eq!(s["theme"], "dark");
}

#[test]
fn packages_warn_on_agents_without_support_only_when_declared_for_them() {
    let env = MusterEnv::new();
    seed(
        &env,
        r#"
version = 1
[agents]
enabled = ["pi", "claude", "codex"]
[agents.packages]
pi = ["npm:x@1"]
claude = ["npm:y@1"]
"#,
    );
    let result = apply(&env);
    let diags = result.diagnostics().as_array().unwrap();
    let warned = |id: &str| {
        diags.iter().any(|d| {
            d["code"] == "unsupported_feature"
                && d["path"] == id
                && d["message"].as_str().unwrap().contains("packages")
        })
    };
    assert!(warned("claude"));
    assert!(!warned("codex"));
    assert!(!warned("pi"));
}

#[test]
fn empty_package_entry_is_rejected() {
    let env = MusterEnv::new();
    seed(
        &env,
        "version = 1\n[agents]\nenabled = [\"pi\"]\n[agents.packages]\npi = [\"npm:x\", \" \"]\n",
    );
    let output = env
        .run(&["agents", "apply", "--yes", "--format", "json"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let text = String::from_utf8_lossy(&output.stdout).to_string()
        + &String::from_utf8_lossy(&output.stderr);
    assert!(text.contains("agents.packages.pi"), "{text}");
}

#[test]
fn pi_is_enabled_by_default_when_unconfigured() {
    assert!(muster::adapters::default_enabled().contains(&"pi"));
    assert!(!muster::adapters::default_enabled().contains(&"hermes"));
}
