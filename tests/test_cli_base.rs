mod support;

use predicates::prelude::*;
use serde_json::json;

const GLOBAL_OPTIONS: [&str; 6] = [
    "--config",
    "--format",
    "--yes",
    "--dry-run",
    "--quiet",
    "--verbose",
];

#[test]
fn test_help() {
    let assert = support::MusterEnv::new()
        .run(&["--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Usage: muster"))
        .stdout(predicate::str::contains("schema"));
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    for opt in GLOBAL_OPTIONS {
        assert!(stdout.contains(opt), "help must list {opt}:\n{stdout}");
    }
}

#[test]
fn test_global_options_parse_before_and_after_the_command() {
    let env = support::MusterEnv::new();
    let cfg = env.path().join("cfg");
    let cfg = cfg.to_str().unwrap();
    env.run(&[
        "--config",
        cfg,
        "--format",
        "json",
        "--yes",
        "--dry-run",
        "--quiet",
        "schema",
    ])
    .assert()
    .code(0);
    env.run(&["schema", "--verbose", "--yes", "--dry-run", "--config", cfg])
        .assert()
        .code(0);
    // --quiet and --verbose contradict each other.
    env.run(&["--quiet", "--verbose", "schema"])
        .assert()
        .code(2);
    // --format only accepts text|json.
    env.run(&["--format", "yaml", "schema"]).assert().code(2);
}

#[test]
fn test_json_envelope_success() {
    let env = support::MusterEnv::new();
    let (output, envelope) = env.run_json(&["--format", "json", "schema"]);
    assert!(output.status.success());
    assert_eq!(envelope.schema_version(), 1);
    assert_eq!(envelope.command(), "schema");
    assert!(envelope.ok());
    assert_eq!(envelope.diagnostics(), &json!([]));
}

#[test]
fn test_json_envelope_error() {
    let env = support::MusterEnv::new();
    for args in [
        &["--format", "json", "not-a-command"][..],
        &["--format=json", "--not-a-flag", "schema"][..],
        &["schema", "--format", "json", "--quiet", "--verbose"][..],
    ] {
        let (output, envelope) = env.run_json(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert_eq!(envelope.schema_version(), 1);
        assert!(!envelope.ok());
        assert_eq!(envelope.result(), &json!(null));
        let diags = envelope.diagnostics().as_array().expect("diagnostics list");
        assert!(!diags.is_empty());
        assert_eq!(diags[0]["code"], "usage");
        assert_eq!(diags[0]["severity"], "error");
        assert!(!diags[0]["message"].as_str().unwrap().is_empty());
    }
}

#[test]
fn test_text_mode_errors_go_to_stderr() {
    support::MusterEnv::new()
        .run(&["not-a-command"])
        .assert()
        .code(2)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("not-a-command"));
}

#[test]
fn test_exit_codes() {
    let env = support::MusterEnv::new();
    // 0 on success
    env.run(&["schema"]).assert().code(0);
    // 2 on unknown flag
    env.run(&["--not-a-flag"]).assert().code(2);
    // 2 on unknown command
    env.run(&["not-a-command"]).assert().code(2);
    // 2 when no command is given
    env.run(&[]).assert().code(2);
}

fn config_path(cmd: assert_cmd::Command) -> String {
    let (output, envelope) = support::json(cmd);
    assert_eq!(output.status.code(), Some(0));
    envelope.result()["config_path"]
        .as_str()
        .expect("config_path")
        .to_string()
}

#[test]
fn test_value_precedence() {
    let env = support::MusterEnv::new();
    let flag_dir = env.path().join("flag-cfg");
    let env_dir = env.path().join("env-cfg");
    let flag = flag_dir.to_str().unwrap();
    let from_env = env_dir.to_str().unwrap();

    // CLI flag beats MUSTER_CONFIG.
    let mut cmd = env.run(&["--format", "json", "--config", flag, "schema"]);
    cmd.env("MUSTER_CONFIG", from_env);
    assert_eq!(config_path(cmd), flag);

    // MUSTER_CONFIG beats the default.
    let mut cmd = env.run(&["--format", "json", "schema"]);
    cmd.env("MUSTER_CONFIG", from_env);
    assert_eq!(config_path(cmd), from_env);

    // Default: $XDG_CONFIG_HOME/muster.
    let cmd = env.run(&["--format", "json", "schema"]);
    assert_eq!(config_path(cmd), env.config_dir().to_str().unwrap());

    // Without XDG_CONFIG_HOME: ~/.config/muster.
    let mut cmd = env.run(&["--format", "json", "schema"]);
    cmd.env_remove("XDG_CONFIG_HOME");
    assert_eq!(
        config_path(cmd),
        env.path().join(".config/muster").to_str().unwrap()
    );

    // MUSTER_FORMAT selects JSON; the --format flag overrides it.
    let mut cmd = env.run(&["schema"]);
    cmd.env("MUSTER_FORMAT", "json");
    let (_, envelope) = support::json(cmd);
    assert_eq!(envelope.command(), "schema");
    env.run(&["--format", "text", "schema"])
        .env("MUSTER_FORMAT", "json")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("schema_version").not());

    // Resolving paths never creates or touches them.
    assert!(!flag_dir.exists() && !env_dir.exists() && !env.config_dir().exists());
}

#[test]
fn test_schema_command() {
    let env = support::MusterEnv::new();
    // Text mode succeeds and mentions the schema command itself.
    env.run(&["schema"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("schema"));

    // JSON mode: manifest JSON Schema plus the command list.
    let (output, envelope) = env.run_json(&["--format", "json", "schema"]);
    assert_eq!(output.status.code(), Some(0));
    let result = envelope.result();
    let manifest_schema = &result["manifest_schema"];
    assert_eq!(manifest_schema["type"], "object");
    assert_eq!(manifest_schema["additionalProperties"], json!(false));
    let pattern_props = manifest_schema["patternProperties"]
        .as_object()
        .expect("patternProperties");
    assert!(
        pattern_props.keys().any(|k| k == "^x-"),
        "patternProperties must cover ^x- extension keys"
    );
    assert!(manifest_schema["properties"]["version"].is_object());

    let commands = result["commands"].as_array().expect("commands list");
    let schema = commands
        .iter()
        .find(|c| c["name"] == "schema")
        .expect("command list must contain 'schema'");
    assert_eq!(schema["mutating"], json!(false));
    assert!(schema["params"].is_array());
    assert!(!schema["about"].as_str().unwrap().is_empty());
}
