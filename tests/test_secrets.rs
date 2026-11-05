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
         [secrets.\"native://app/api\"]\nallow = [\"claude\", \"codex\", \"opencode\"]\n\n\
         [mcp.api]\ncommand = \"api-mcp\"\nenv = { API_KEY = \"native://app/api\", PLAIN = \"visible\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();

    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply", "--yes"]);
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
         [secrets.\"native://app/missing\"]\nallow = [\"claude\"]\n\n\
         [mcp.api]\ncommand = \"api-mcp\"\nenv = { API_KEY = \"native://app/missing\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();
    let (output, envelope) = env.run_json(&["--format", "json", "agents", "apply", "--yes"]);
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

// ---------------------------------------------------------------------------
// Secret isolation: per-secret `allow` lists in muster.toml
// (`[secrets."<reference>"] allow = ["<agent>", "<agent>@<project>"]`)
// ---------------------------------------------------------------------------

const DENY_BY_DEFAULT_VALUE: &str = "muster-DENY-by-default-9a1c";
const CLAUDE_ONLY_VALUE: &str = "muster-ALLOW-claude-only-9a2d";

fn apply_for(
    env: &support::MusterEnv,
    agent: &str,
    env_vars: &[(&str, &str)],
) -> (std::process::Output, support::Envelope) {
    let mut cmd = env.run(&[
        "--format", "json", "agents", "apply", "--agent", agent, "--yes",
    ]);
    for (key, value) in env_vars {
        cmd.env(key, value);
    }
    support::json(cmd)
}

#[test]
fn secret_isolation_denies_by_default_to_agents() {
    let env = initialized();
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\"]\n\n\
         [mcp.leaky]\ncommand = \"leaky-mcp\"\nenv = { TOKEN = \"env://GLOBAL_SECRET\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();

    let (output, envelope) = apply_for(&env, "claude", &[("GLOBAL_SECRET", DENY_BY_DEFAULT_VALUE)]);
    assert!(output.status.success(), "{}", envelope.raw);

    // A secret with no `allow` list reaches NO agent config: neither the
    // resolved plaintext nor the raw reference may appear.
    let claude = std::fs::read_to_string(env.path().join(".claude.json")).unwrap();
    assert!(
        !claude.contains(DENY_BY_DEFAULT_VALUE),
        "no-allow secret leaked into claude config: {claude}"
    );
    assert!(
        !claude.contains("env://GLOBAL_SECRET"),
        "unresolved reference must not be written either: {claude}"
    );
}

#[test]
fn secret_isolation_injects_only_allowed_secrets_per_agent() {
    let env = initialized();
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\", \"codex\"]\n\n\
         [secrets.\"env://ANTHROPIC_API_KEY\"]\nallow = [\"claude\"]\n\n\
         [mcp.github]\ncommand = \"api-mcp\"\nenv = { KEY = \"env://ANTHROPIC_API_KEY\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();

    // Claude is in the allow list: the resolved secret lands in its config.
    let (claude_out, claude_env) =
        apply_for(&env, "claude", &[("ANTHROPIC_API_KEY", CLAUDE_ONLY_VALUE)]);
    assert!(claude_out.status.success(), "{}", claude_env.raw);
    let claude = std::fs::read_to_string(env.path().join(".claude.json")).unwrap();
    assert!(
        claude.contains(CLAUDE_ONLY_VALUE),
        "allowed secret must be injected into claude config: {claude}"
    );

    // Codex is NOT in the allow list: no plaintext, no raw reference, and a
    // typed diagnostic naming the denial.
    let (codex_out, codex_env) =
        apply_for(&env, "codex", &[("ANTHROPIC_API_KEY", CLAUDE_ONLY_VALUE)]);
    assert!(codex_out.status.success(), "{}", codex_env.raw);
    assert!(
        codex_env
            .raw
            .to_string()
            .contains("secret_not_allowed_for_agent"),
        "denied injection must emit secret_not_allowed_for_agent: {}",
        codex_env.raw
    );
    let codex = std::fs::read_to_string(env.path().join(".codex/config.toml")).unwrap();
    assert!(
        !codex.contains(CLAUDE_ONLY_VALUE),
        "secret leaked into codex config: {codex}"
    );
    assert!(
        !codex.contains("env://ANTHROPIC_API_KEY"),
        "reference must be omitted, not written raw: {codex}"
    );
}

#[test]
fn secret_isolation_allows_human_cli_access() {
    let env = initialized();
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    // A policy entry with no `allow`: no agent may receive it.
    manifest.push_str("\n[secrets.\"native://app/api\"]\n");
    std::fs::write(&manifest_path, manifest).unwrap();

    let added = add(&env, "app/api", SEED);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );

    // The human, via the CLI, can always reveal regardless of any allow list.
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
fn secret_isolation_agent_at_project_grants_only_in_that_project() {
    let env = initialized();
    let manifest_path = env.config_dir().join("muster.toml");
    let mut manifest = std::fs::read_to_string(&manifest_path).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\"]\n\n\
         [secrets.\"env://REPO_TOKEN\"]\nallow = [\"claude@my-repo\"]\n\n\
         [mcp.github]\ncommand = \"api-mcp\"\nenv = { TOKEN = \"env://REPO_TOKEN\" }\n",
    );
    std::fs::write(&manifest_path, manifest).unwrap();

    // In user/global scope, `claude@my-repo` does NOT grant to claude.
    let (global_out, global_env) =
        apply_for(&env, "claude", &[("REPO_TOKEN", "scoped-token-1234")]);
    assert!(global_out.status.success(), "{}", global_env.raw);
    assert!(
        global_env
            .raw
            .to_string()
            .contains("secret_not_allowed_for_agent"),
        "denied scoped injection must emit secret_not_allowed_for_agent: {}",
        global_env.raw
    );
    let claude = std::fs::read_to_string(env.path().join(".claude.json")).unwrap();
    assert!(!claude.contains("scoped-token-1234"));

    // In project scope `my-repo`, `claude@my-repo` DOES grant to claude.
    let project_dir = env.path().join("my-repo");
    std::fs::create_dir_all(&project_dir).unwrap();
    let project_muster_dir = project_dir.join(".muster");
    std::fs::create_dir_all(&project_muster_dir).unwrap();
    std::fs::write(
        project_muster_dir.join("muster.toml"),
        "version = 1\n[agents]\nenabled = [\"claude\"]\n\n\
         [mcp.github]\ncommand = \"api-mcp\"\nenv = { TOKEN = \"env://REPO_TOKEN\" }\n",
    )
    .unwrap();

    let mut cmd = env.run(&[
        "--format",
        "json",
        "sync",
        "--project",
        project_dir.to_str().unwrap(),
        "--yes",
    ]);
    cmd.env("REPO_TOKEN", "scoped-token-1234");
    let (proj_out, proj_env) = support::json(cmd);
    assert!(proj_out.status.success(), "{}", proj_env.raw);
    let project_claude = std::fs::read_to_string(project_dir.join(".mcp.json")).unwrap();
    assert!(
        project_claude.contains("scoped-token-1234"),
        "allowed scoped secret must be injected into project claude config: {project_claude}"
    );
}

#[test]
fn secret_add_allow_records_the_grant_and_list_reports_it() {
    let env = initialized();
    let added = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            "app/api",
            "--value-stdin",
            "--allow",
            "claude",
            "--allow",
            "codex@my-repo",
        ])
        .write_stdin(SEED.as_bytes())
        .output()
        .expect("secret add --allow");
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(manifest.contains("native://app/api"), "{manifest}");
    assert!(!manifest.contains(SEED));

    let (output, envelope) = env.run_json(&["--format", "json", "secret", "list"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(
        envelope.result()[0]["allow"],
        serde_json::json!(["claude", "codex@my-repo"])
    );

    // Unknown agents are rejected before anything is stored.
    let bad = env
        .run(&[
            "--format",
            "json",
            "secret",
            "add",
            "app/other",
            "--value-stdin",
            "--allow",
            "nobody",
        ])
        .write_stdin(SEED.as_bytes())
        .output()
        .expect("secret add --allow nobody");
    assert_eq!(bad.status.code(), Some(2));
}

#[test]
fn secret_approval_policy_validates_durations_at_load_time() {
    use muster::manifest::Manifest;
    for ttl in ["0s", "25h", "-1m", "forever", "999999999999999999999999h"] {
        let text = format!("version = 1\n[secrets.'env://KEY']\nallow = ['claude']\napproval = 'each'\napproval_ttl = '{ttl}'\n");
        assert!(Manifest::parse(&text, "muster.toml").is_err(), "{ttl}");
    }
    for policy in [
        "approval = 'each'\napproval_ttl = '24h'",
        "approval = 'none'",
        "",
    ] {
        let text = format!("version = 1\n[secrets.'env://KEY']\nallow = ['claude']\n{policy}\n");
        assert!(Manifest::parse(&text, "muster.toml").is_ok(), "{policy}");
    }
}

#[test]
fn concurrent_secret_requests_are_deduplicated_without_losing_state() {
    let env = initialized();
    let path = env.config_dir().join("muster.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str(
        "\n[secrets.'env://MUSTER_CONCURRENT_KEY']\nallow = ['claude']\napproval = 'each'\n",
    );
    std::fs::write(path, text).unwrap();
    std::thread::scope(|scope| {
        let jobs: Vec<_> =
            (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let out = env
                            .run(&[
                                "secret",
                                "get",
                                "env://MUSTER_CONCURRENT_KEY",
                                "--agent",
                                "claude",
                                "--reveal",
                                "--yes",
                            ])
                            .env("MUSTER_CONCURRENT_KEY", "concurrent-private-value")
                            .output()
                            .unwrap();
                        assert_eq!(out.status.code(), Some(5));
                        assert!(!String::from_utf8_lossy(&out.stdout)
                            .contains("concurrent-private-value"));
                        assert!(!String::from_utf8_lossy(&out.stderr)
                            .contains("concurrent-private-value"));
                    })
                })
                .collect();
        for job in jobs {
            job.join().unwrap();
        }
    });
    let requests = success(&env, &["secret", "requests", "--format", "json"]);
    assert_eq!(requests.as_array().unwrap().len(), 1);
    let events = muster::audit::events(&env.path().join(".local/state/muster")).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|e| e["action"] == "secret.request")
            .count(),
        1
    );
}

#[test]
fn secret_get_reveal_without_agent_enforces_policy_and_refuses_yes_alone() {
    let env = initialized();
    let path = env.config_dir().join("muster.toml");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("\n[secrets.'env://POLICY_KEY']\nallow = ['claude']\napproval = 'each'\n");
    std::fs::write(&path, text).unwrap();

    // Human runs `secret get --reveal --yes` without --agent: must fail
    let out = env
        .run(&[
            "--format",
            "json",
            "secret",
            "get",
            "env://POLICY_KEY",
            "--reveal",
            "--yes",
        ])
        .env("POLICY_KEY", "super-secret-value-must-not-leak")
        .output()
        .unwrap();

    assert_eq!(
        out.status.code(),
        Some(5),
        "reveal without --agent must fail code 5 when policy set: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("super-secret-value-must-not-leak"));

    // Also test with allow list only (approval = none)
    let mut manifest: muster::manifest::Manifest =
        muster::manifest::Manifest::load(&env.config_dir()).unwrap();
    manifest
        .secrets
        .get_mut("env://POLICY_KEY")
        .unwrap()
        .approval = muster::manifest::SecretApproval::None;
    manifest.save(&env.config_dir()).unwrap();

    let out2 = env
        .run(&[
            "--format",
            "json",
            "secret",
            "get",
            "env://POLICY_KEY",
            "--reveal",
            "--yes",
        ])
        .env("POLICY_KEY", "super-secret-value-must-not-leak")
        .output()
        .unwrap();
    assert_eq!(
        out2.status.code(),
        Some(5),
        "reveal without --agent must fail code 5 when allow list set: {}",
        String::from_utf8_lossy(&out2.stdout)
    );
    assert!(!String::from_utf8_lossy(&out2.stdout).contains("super-secret-value-must-not-leak"));
}

#[test]
fn bitwarden_json_output_yields_only_the_named_field() {
    let env = initialized();
    env.fake_exe(
        "bws",
        r#"echo '{"key":"K-NAME","value":"V-SECRET-1","note":"N-NOTE-2","other":"O-3"}'"#,
    );
    let get = |reference: &str| {
        env.run_json(&[
            "--format", "json", "secret", "get", reference, "--reveal", "--yes",
        ])
    };
    let (out, envelope) = get("bws://item/value");
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result().as_str(), Some("V-SECRET-1"));
    let (_, envelope) = get("bws://item/note");
    assert_eq!(envelope.result().as_str(), Some("N-NOTE-2"));
    let (out, envelope) = get("bws://item/missing");
    assert!(!out.status.success());
    let text = envelope.raw.to_string();
    assert!(
        !text.contains("V-SECRET-1") && !text.contains("N-NOTE-2"),
        "{text}"
    );
}
