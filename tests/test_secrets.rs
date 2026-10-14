mod support;

use serde_json::Value;
use std::process::Command;

const SEED: &str = "SEED-8f3a-secret-hygiene-marker";

fn success(env: &support::MusterEnv, args: &[&str]) -> Value {
    let (out, envelope) = env.run_json(args);
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.raw["result"].clone()
}

fn initialized() -> support::MusterEnv {
    let env = support::MusterEnv::new();
    success(&env, &["--format", "json", "init", "--yes"]);
    env
}

fn add(env: &support::MusterEnv, group_name: &str, value: &str) -> std::process::Output {
    env.run(&[
        "--format",
        "json",
        "secret",
        "add",
        group_name,
        "--value-stdin",
    ])
    .write_stdin(value.as_bytes())
    .output()
    .expect("run secret add")
}

#[test]
fn secret_add_reads_value_from_stdin_and_secret_can_be_revealed_explicitly() {
    let env = initialized();
    let output = add(&env, "app/api", SEED);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SEED));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SEED));
    let (output, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "native://app/api",
        "--reveal",
        "--yes",
    ]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result().as_str(), Some(SEED));
}

#[test]
fn secret_list_reports_metadata_without_values() {
    let env = initialized();
    let output = add(&env, "app/api", SEED);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (output, envelope) = env.run_json(&["--format", "json", "secret", "list"]);
    assert!(output.status.success(), "{}", envelope.raw);
    let listed = envelope.result().as_array().expect("metadata list");
    assert_eq!(listed.len(), 1);
    let metadata = &listed[0];
    for field in [
        "group",
        "name",
        "reference",
        "created_at",
        "updated_at",
        "readers",
    ] {
        assert!(metadata.get(field).is_some(), "missing {field}: {metadata}");
    }
    assert_eq!(metadata["group"], "app");
    assert_eq!(metadata["name"], "api");
    assert!(!envelope.raw.to_string().contains(SEED));
}

#[test]
fn secret_rotate_replaces_value_without_disclosing_either_value() {
    let env = initialized();
    let first = add(&env, "app/api", SEED);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let rotated = env
        .run(&[
            "--format",
            "json",
            "secret",
            "rotate",
            "app/api",
            "--value-stdin",
        ])
        .write_stdin(b"ROTATED-8f3a")
        .output()
        .expect("rotate");
    assert!(
        rotated.status.success(),
        "{}",
        String::from_utf8_lossy(&rotated.stderr)
    );
    let revealed = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "native://app/api",
        "--reveal",
        "--yes",
    ]);
    assert!(revealed.0.status.success());
    assert_eq!(revealed.1.result().as_str(), Some("ROTATED-8f3a"));
    for output in [first, rotated] {
        assert!(!String::from_utf8_lossy(&output.stdout).contains(SEED));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(SEED));
    }
}

#[test]
fn secret_rm_removes_secret_without_disclosing_it() {
    let env = initialized();
    let added = add(&env, "app/api", SEED);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let removed = env
        .run(&["--format", "json", "secret", "rm", "app/api", "--yes"])
        .output()
        .expect("rm");
    assert!(
        removed.status.success(),
        "{}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!String::from_utf8_lossy(&removed.stdout).contains(SEED));
    assert!(!String::from_utf8_lossy(&removed.stderr).contains(SEED));
    let listed = success(&env, &["--format", "json", "secret", "list"]);
    assert!(listed.as_array().unwrap().is_empty());
    let log = Command::new("git")
        .args(["-C", env.config_dir().to_str().unwrap(), "log", "-p"])
        .output()
        .expect("git log");
    assert!(!String::from_utf8_lossy(&log.stdout).contains(SEED));
    assert!(
        !std::fs::read_to_string(env.config_dir().join("muster.toml"))
            .unwrap()
            .contains(SEED)
    );
}

#[test]
fn env_provider_resolves_environment_references() {
    let env = initialized();
    let output = env
        .run(&[
            "--format",
            "json",
            "secret",
            "get",
            "env://MUSTER_TEST_VALUE",
            "--reveal",
            "--yes",
        ])
        .env("MUSTER_TEST_VALUE", "ENV-SECRET-8f3a")
        .output()
        .expect("get env");
    assert!(output.status.success(), "{}", envelope_text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("json response");
    assert_eq!(value["result"], "ENV-SECRET-8f3a");
}

#[test]
fn native_provider_resolves_vault_references() {
    let env = initialized();
    let output = add(&env, "app/api", SEED);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let (out, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "native://app/api",
        "--reveal",
        "--yes",
    ]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result().as_str(), Some(SEED));
}

#[test]
fn onepassword_provider_resolves_through_fake_cli() {
    let env = initialized();
    env.fake_exe("op", "echo OP-SECRET-8f3a");
    let (output, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "op://vault/item/field",
        "--reveal",
        "--yes",
    ]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result().as_str(), Some("OP-SECRET-8f3a"));
}

#[test]
fn bitwarden_provider_resolves_through_fake_cli() {
    let env = initialized();
    env.fake_exe("bws", "echo BWS-SECRET-8f3a");
    let (output, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "bws://item/field",
        "--reveal",
        "--yes",
    ]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result().as_str(), Some("BWS-SECRET-8f3a"));
}

#[test]
fn unavailable_provider_returns_typed_diagnostic_and_exit_four() {
    let env = initialized();
    let mut cmd = env.run(&["--format", "json", "secret", "get", "op://vault/item/field"]);
    cmd.env("PATH", &env.bin);
    let (output, envelope) = support::json(cmd);
    assert_eq!(output.status.code(), Some(4), "{}", envelope.raw);
    assert!(
        envelope.raw.to_string().contains("provider_unavailable"),
        "{}",
        envelope.raw
    );
}

#[test]
fn unauthorized_provider_returns_typed_diagnostic() {
    let env = initialized();
    env.fake_exe("op", "echo unauthorized >&2; exit 1");
    let (output, envelope) =
        env.run_json(&["--format", "json", "secret", "get", "op://vault/item/field"]);
    assert!(!output.status.success());
    assert!(
        envelope.raw.to_string().contains("unauthorized"),
        "{}",
        envelope.raw
    );
}

#[test]
fn unknown_secret_reference_is_rejected_as_usage_error() {
    let env = initialized();
    let (output, envelope) = env.run_json(&["--format", "json", "secret", "get", "unknown://item"]);
    assert_eq!(output.status.code(), Some(2), "{}", envelope.raw);
}

#[test]
fn adapter_apply_resolves_native_mcp_environment_without_persisting_plaintext_or_reference() {
    let env = initialized();
    let added = add(&env, "app/api", SEED);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\", \"codex\", \"opencode\"]\n\n\
         [mcp.api]\ncommand = \"api-mcp\"\nenv = { API_KEY = \"native://app/api\", PLAIN = \"visible\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();

    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert!(!String::from_utf8_lossy(&output.stdout).contains(SEED));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SEED));
    assert!(
        !envelope
            .raw
            .to_string()
            .contains("secret_reference_unresolved"),
        "{}",
        envelope.raw
    );

    // The plaintext lands only in each agent's native file under $HOME, private to the user.
    let home = env.path();
    for native in [
        ".claude.json",
        ".codex/config.toml",
        ".config/opencode/opencode.json",
    ] {
        let path = home.join(native);
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{native}: {e}"));
        assert!(text.contains(SEED), "{native} must hold the resolved value");
        assert!(text.contains("visible"), "{native} keeps plain env");
        assert!(
            !text.contains("native://app/api"),
            "{native} must not hold the reference"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{native} holds a secret and must be 0600");
    }

    // Never in the config repo: files or git history.
    let mut stack = vec![env.config_dir()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(SEED),
                    "plaintext leaked into {}",
                    path.display()
                );
            }
        }
    }
    let log = Command::new("git")
        .args([
            "-C",
            env.config_dir().to_str().unwrap(),
            "log",
            "-p",
            "--all",
        ])
        .output()
        .expect("git log");
    assert!(!String::from_utf8_lossy(&log.stdout).contains(SEED));
}

#[test]
fn unresolvable_mcp_secret_reports_typed_provider_code_and_is_not_written() {
    let env = initialized();
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\"]\n\n\
         [mcp.api]\ncommand = \"api-mcp\"\nenv = { API_KEY = \"native://app/missing\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();
    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply"]);
    assert!(output.status.success(), "{}", envelope.raw);
    let text = envelope.raw.to_string();
    assert!(text.contains("secret_not_found"), "{text}");
    assert!(text.contains("secret_reference_unresolved"), "{text}");
    let claude = std::fs::read_to_string(env.path().join(".claude.json")).unwrap();
    assert!(!claude.contains("API_KEY") && !claude.contains("native://"));
}

#[test]
fn secret_add_refuses_values_on_the_command_line_and_dry_run_writes_nothing() {
    let env = initialized();
    let (output, envelope) = env.run_json(&["--format", "json", "secret", "add", "app/api"]);
    assert_eq!(output.status.code(), Some(2), "{}", envelope.raw);

    let dry = env
        .run(&[
            "--format",
            "json",
            "--dry-run",
            "secret",
            "add",
            "app/api",
            "--value-stdin",
        ])
        .write_stdin(SEED.as_bytes())
        .output()
        .expect("dry run");
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stdout)
    );
    assert!(!String::from_utf8_lossy(&dry.stdout).contains(SEED));
    let listed = success(&env, &["--format", "json", "secret", "list"]);
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
}

#[test]
fn get_without_reveal_confirms_resolution_but_never_prints_the_value() {
    let env = initialized();
    let added = add(&env, "app/api", SEED);
    assert!(added.status.success());
    let (output, envelope) =
        env.run_json(&["--format", "json", "secret", "get", "native://app/api"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["resolved"], true);
    assert!(!envelope.raw.to_string().contains(SEED));

    // Reveal without a TTY needs --yes.
    let (output, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "get",
        "native://app/api",
        "--reveal",
    ]);
    assert_eq!(output.status.code(), Some(2), "{}", envelope.raw);
    assert!(!envelope.raw.to_string().contains(SEED));
}

#[test]
fn provider_errors_map_to_stable_codes_and_exit_codes() {
    let env = initialized();
    let cases = [
        ("echo 'item not found' >&2; exit 1", "secret_not_found", 1),
        (
            "echo 'unauthorized' >&2; exit 1",
            "provider_unauthorized",
            5,
        ),
        (
            "echo 'rate limit exceeded' >&2; exit 1",
            "provider_rate_limited",
            4,
        ),
    ];
    for (script, code, exit) in cases {
        env.fake_exe("doppler", script);
        let (output, envelope) = env.run_json(&[
            "--format",
            "json",
            "secret",
            "get",
            "doppler://proj/prd/API_KEY",
        ]);
        assert_eq!(output.status.code(), Some(exit), "{}", envelope.raw);
        assert_eq!(envelope.diagnostics()[0]["code"], code, "{}", envelope.raw);
    }
}

#[test]
fn secret_check_lists_every_provider_and_its_availability() {
    let env = initialized();
    env.fake_exe("infisical", "echo 0.1.0");
    let result = success(&env, &["--format", "json", "secret", "check"]);
    let providers = result["providers"].as_array().expect("providers");
    let scheme = |s: &str| {
        providers
            .iter()
            .find(|p| p["provider"]["scheme"] == s)
            .unwrap_or_else(|| panic!("{s} missing: {result}"))
    };
    for s in ["native", "env", "op", "bws", "doppler", "infisical"] {
        scheme(s);
    }
    assert_eq!(scheme("native")["available"], true);
    assert_eq!(scheme("infisical")["available"], true);
}

fn envelope_text(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}
