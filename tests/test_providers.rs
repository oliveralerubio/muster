mod support;

use support::MusterEnv;

fn seed(env: &MusterEnv, text: &str) {
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), text).unwrap();
}

#[test]
fn provider_validation_precedes_writes() {
    for provider in [
        "kind = 'invalid'",
        "kind = 'anthropic'\nkey = 'plaintext'",
        "kind = 'anthropic'\nkey = 'env://'",
        "default_model = 'example'",
    ] {
        let env = MusterEnv::new();
        seed(
            &env,
            &format!(
                "version = 1\n[agents]\nenabled = ['claude']\n[providers.gateway]\n{provider}\n"
            ),
        );
        env.run(&["agents", "apply", "--yes", "--format", "json"])
            .assert()
            .code(3);
        assert!(!env.path().join(".claude/settings.json").exists());
    }
}

#[test]
fn provider_configuration_is_accepted_in_manifest_and_profiles() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[providers.gateway]\nkind = 'anthropic'\n[profiles.work.providers.local]\nkind = 'ollama'\n");
    env.run(&["plan", "--format", "json"]).assert().success();
}

#[test]
fn selected_provider_renders_native_settings_and_agent_model_wins() {
    let env = MusterEnv::new();
    seed(
        &env,
        r#"version = 1
[agents]
enabled = ['claude', 'codex', 'opencode', 'antigravity']
[agents.providers]
claude = 'gateway'
codex = 'gateway'
opencode = 'gateway'
antigravity = 'gateway'
[agents.models]
claude = 'explicit-model'
[providers.gateway]
kind = 'openai-compatible'
base_url = 'https://example.invalid/v1'
default_model = 'default-model'
"#,
    );
    let out = env
        .run(&["agents", "apply", "--yes", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("unsupported_feature"));
    let claude: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude["model"], "explicit-model");
    assert_eq!(
        claude["env"]["ANTHROPIC_BASE_URL"],
        "https://example.invalid/v1"
    );
    let codex = std::fs::read_to_string(env.path().join(".codex/config.toml")).unwrap();
    assert!(codex.contains("model_provider = \"gateway\""));
    assert!(codex.contains("model = \"default-model\""));
    assert!(codex.contains("[model_providers.gateway]"));
    assert!(codex.contains("base_url = \"https://example.invalid/v1\""));
    let oc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".config/opencode/opencode.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(oc["model"], "gateway/default-model");
    assert_eq!(
        oc["provider"]["gateway"]["options"]["baseURL"],
        "https://example.invalid/v1"
    );
}

#[test]
fn provider_secrets_obey_allow_and_never_appear_in_output() {
    let env = MusterEnv::new();
    seed(
        &env,
        r#"version = 1
[agents]
enabled = ['claude', 'codex', 'opencode']
[agents.providers]
claude = 'gateway'
codex = 'gateway'
opencode = 'gateway'
[providers.gateway]
kind = 'anthropic'
key = 'env://MUSTER_PROVIDER_TEST_KEY'
[secrets.'env://MUSTER_PROVIDER_TEST_KEY']
allow = ['claude', 'codex']
"#,
    );
    for command in [
        vec!["plan", "--format", "json"],
        vec!["agents", "apply", "--yes", "--format", "json"],
    ] {
        let output = env
            .run(&command)
            .env("MUSTER_PROVIDER_TEST_KEY", "provider-test-only-value")
            .assert()
            .success()
            .get_output()
            .clone();
        for bytes in [&output.stdout, &output.stderr] {
            let text = String::from_utf8_lossy(bytes);
            assert!(!text.contains("provider-test-only-value"));
        }
        assert!(String::from_utf8_lossy(&output.stdout).contains("secret_not_allowed_for_agent"));
    }
    let claude: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        claude["env"]["ANTHROPIC_AUTH_TOKEN"],
        "provider-test-only-value"
    );
    let codex = std::fs::read_to_string(env.path().join(".codex/config.toml")).unwrap();
    assert!(codex.contains("env_key = \"MUSTER_PROVIDER_TEST_KEY\""));
    assert!(!codex.contains("provider-test-only-value"));
    let oc = std::fs::read_to_string(env.path().join(".config/opencode/opencode.json")).unwrap();
    assert!(!oc.contains("gateway"));
}

#[test]
fn provider_removal_preserves_hand_added_provider_keys() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude', 'codex', 'opencode']\n[agents.providers]\nclaude = 'gateway'\ncodex = 'gateway'\nopencode = 'gateway'\n[providers.gateway]\nkind = 'openai-compatible'\nbase_url = 'https://example.invalid/v1'\ndefault_model = 'default-model'\n");
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let codex_path = env.path().join(".codex/config.toml");
    let mut codex = std::fs::read_to_string(&codex_path).unwrap();
    codex.push_str("custom = 'keep'\n\n[model_providers.manual]\nname = 'Manual'\n");
    std::fs::write(&codex_path, codex).unwrap();
    let oc_path = env.path().join(".config/opencode/opencode.json");
    let mut oc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&oc_path).unwrap()).unwrap();
    oc["provider"]["gateway"]["options"]["custom"] = "keep".into();
    oc["provider"]["manual"] = serde_json::json!({"npm": "manual"});
    std::fs::write(&oc_path, oc.to_string()).unwrap();
    seed(
        &env,
        "version = 1\n[agents]\nenabled = ['claude', 'codex', 'opencode']\n",
    );
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let codex = std::fs::read_to_string(&codex_path).unwrap();
    assert!(codex.contains("custom = 'keep'"));
    assert!(codex.contains("[model_providers.manual]"));
    assert!(!codex.contains("base_url"));
    let oc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&oc_path).unwrap()).unwrap();
    assert_eq!(oc["provider"]["gateway"]["options"]["custom"], "keep");
    assert_eq!(oc["provider"]["manual"]["npm"], "manual");
    assert!(oc["provider"]["gateway"]["options"]["baseURL"].is_null());
}

#[test]
fn provider_commands_add_list_and_remove_without_values() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n");
    env.run(&[
        "provider",
        "add",
        "gateway",
        "--kind",
        "anthropic",
        "--base-url",
        "https://example.invalid",
        "--default-model",
        "example",
        "--key",
        "env://MUSTER_PROVIDER_TEST_KEY",
        "--yes",
    ])
    .assert()
    .success();
    let text = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(text.contains("[providers.gateway]"));
    let list = env
        .run_json(&["provider", "list", "--format", "json"])
        .1
        .raw;
    assert_eq!(list["result"]["providers"][0]["id"], "gateway");
    assert_eq!(
        list["result"]["providers"][0]["key"],
        "env://MUSTER_PROVIDER_TEST_KEY"
    );
    env.run(&["provider", "rm", "gateway", "--yes"])
        .assert()
        .success();
    let list = env
        .run_json(&["provider", "list", "--format", "json"])
        .1
        .raw;
    assert_eq!(list["result"]["providers"], serde_json::json!([]));
}

#[test]
fn provider_login_delegates_to_native_cli_and_preserves_credentials() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents.providers]\nclaude = 'subscription'\n[providers.subscription]\nkind = 'anthropic'\n");
    let credentials = env.path().join(".claude/.credentials.json");
    std::fs::create_dir_all(credentials.parent().unwrap()).unwrap();
    std::fs::write(
        &credentials,
        "opaque credentials must not be read or changed",
    )
    .unwrap();
    env.fake_exe("claude", "if [ \"$*\" = 'auth status' ]; then exit 1; fi\nif [ \"$*\" = 'auth login' ]; then printf invoked > \"$HOME/login-called\"; exit 0; fi\nexit 9");
    env.run(&[
        "provider",
        "login",
        "subscription",
        "--yes",
        "--format",
        "json",
    ])
    .assert()
    .success();
    assert_eq!(
        std::fs::read_to_string(env.path().join("login-called")).unwrap(),
        "invoked"
    );
    assert_eq!(
        std::fs::read_to_string(&credentials).unwrap(),
        "opaque credentials must not be read or changed"
    );
    env.fake_exe(
        "claude",
        "if [ \"$*\" = 'auth status' ]; then exit 0; fi\nexit 9",
    );
    let out = env
        .run(&["provider", "login", "claude", "--yes", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("already_logged_in"));
}

#[test]
fn provider_list_matches_mcp_registry_surface() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\n");
    let cli = env
        .run_json(&["provider", "list", "--format", "json"])
        .1
        .raw;
    let messages = concat!(
        "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"provider_list\",\"arguments\":{}}}\n"
    );
    let out = env
        .run(&["mcp"])
        .write_stdin(messages)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: serde_json::Value =
        serde_json::from_str(String::from_utf8_lossy(&out).lines().last().unwrap()).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(
        response["result"]["content"][0]["text"]
            .as_str()
            .expect("MCP text envelope"),
    )
    .unwrap();
    assert_eq!(envelope, cli);
    assert_eq!(
        cli["result"]["providers"][0]["agents"],
        serde_json::json!(["claude"])
    );
}

#[test]
fn doctor_warns_for_unresolved_provider_keys_and_missing_subscription_login() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude', 'codex']\n[agents.providers]\nclaude = 'missing'\ncodex = 'subscription'\n[providers.missing]\nkind = 'anthropic'\nkey = 'env://MUSTER_PROVIDER_ABSENT'\n[providers.subscription]\nkind = 'openai'\n");
    env.fake_exe("codex", "if [ \"$*\" = 'login status' ]; then exit 1; fi\nif [ \"$1\" = '--version' ]; then echo 'codex 1.0.0'; exit 0; fi\nexit 9");
    let (output, envelope) = env.run_json(&["doctor", "--format", "json"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("secret_reference_unresolved"));
    assert!(text.contains("provider_login_required"));
    let diagnostics = envelope.diagnostics().as_array().unwrap();
    assert!(diagnostics
        .iter()
        .any(|d| d["code"] == "secret_reference_unresolved"
            && d["severity"] == "warning"
            && d["path"] == "providers.missing"));
}

#[test]
fn dangling_provider_selection_is_invalid_even_for_read_commands() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents.providers]\nclaude = 'typo'\n");
    env.run(&["provider", "list", "--format", "json"])
        .assert()
        .code(3);
}

#[test]
fn unresolved_provider_is_skipped_without_blocking_other_configuration() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[agents.models]\nclaude = 'explicit'\n[providers.gateway]\nkind = 'anthropic'\nbase_url = 'https://example.invalid'\nkey = 'env://MUSTER_PROVIDER_ABSENT'\n[secrets.'env://MUSTER_PROVIDER_ABSENT']\nallow = ['claude']\n");
    let out = env
        .run(&["agents", "apply", "--yes", "--format", "json"])
        .env_remove("MUSTER_PROVIDER_ABSENT")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("secret_reference_unresolved"));
    let file = std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap();
    assert!(!file.contains("example.invalid"));
    assert!(!file.contains("env://"));
    let json: serde_json::Value = serde_json::from_str(&file).unwrap();
    assert_eq!(json["model"], "explicit");
}

#[test]
fn machine_profile_overrides_provider_by_id_and_keeps_other_providers() {
    let env = MusterEnv::new();
    env.run(&["init", "--machine", "laptop", "--yes"])
        .assert()
        .success();
    let path = env.config_dir().join("muster.toml");
    let mut config = std::fs::read_to_string(&path)
        .unwrap()
        .replace("[machines.laptop]", "[machines.laptop]\nprofile = 'work'");
    config.push_str("\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\ndefault_model = 'team-model'\n[providers.local]\nkind = 'ollama'\n[profiles.work.providers.gateway]\nkind = 'anthropic'\ndefault_model = 'machine-model'\n");
    std::fs::write(path, config).unwrap();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    let file: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(file["model"], "machine-model");
    let (_, list) = env.run_json(&["provider", "list", "--format", "json"]);
    assert_eq!(
        list.result()["providers"][0]["default_model"],
        "machine-model"
    );
    assert_eq!(list.result()["providers"][1]["id"], "local");
}

#[test]
fn project_provider_renders_only_inside_project_and_obeys_project_allow() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[secrets.'env://MUSTER_PROVIDER_TEST_KEY']\nallow = ['claude@demo']\n");
    let project = env.path().join("demo");
    std::fs::create_dir_all(project.join(".muster")).unwrap();
    std::fs::write(project.join(".muster/muster.toml"), "version = 1\n[agents.providers]\nclaude = 'local'\n[providers.local]\nkind = 'anthropic'\nbase_url = 'https://project.invalid'\nkey = 'env://MUSTER_PROVIDER_TEST_KEY'\n").unwrap();
    env.run(&["sync", "--project", project.to_str().unwrap(), "--yes"])
        .env("MUSTER_PROVIDER_TEST_KEY", "project-provider-test-value")
        .assert()
        .success();
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(project.join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"],
        "https://project.invalid"
    );
    assert_eq!(
        settings["env"]["ANTHROPIC_AUTH_TOKEN"],
        "project-provider-test-value"
    );
    assert!(!env.path().join(".claude/settings.json").exists());
}

#[test]
fn rollback_restores_previous_provider_state() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\nbase_url = 'https://first.invalid'\ndefault_model = 'first'\n");
    env.run(&["agents", "apply", "--yes"]).assert().success();
    env.run(&[
        "provider",
        "add",
        "gateway",
        "--kind",
        "anthropic",
        "--base-url",
        "https://second.invalid",
        "--default-model",
        "second",
        "--yes",
    ])
    .assert()
    .success();
    env.run(&["agents", "apply", "--yes"]).assert().success();
    env.run(&["rollback", "1", "--yes"]).assert().success();
    let settings: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".claude/settings.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(settings["model"], "first");
    assert_eq!(
        settings["env"]["ANTHROPIC_BASE_URL"],
        "https://first.invalid"
    );
}

#[test]
fn native_provider_value_is_vaulted_and_codex_uses_bearer_token() {
    let env = MusterEnv::new();
    env.run(&["init", "--machine", "laptop", "--yes"])
        .assert()
        .success();
    let out = env
        .run(&[
            "provider",
            "add",
            "gateway",
            "--kind",
            "openai-compatible",
            "--key",
            "native://app/provider",
            "--value",
            "native-provider-test-value",
            "--allow",
            "codex",
            "--allow",
            "opencode",
            "--yes",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("native-provider-test-value"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("native-provider-test-value"));
    let path = env.config_dir().join("muster.toml");
    let mut config = std::fs::read_to_string(&path).unwrap();
    assert!(!config.contains("native-provider-test-value"));
    config.push_str("\n[agents]\nenabled = ['codex', 'opencode']\n[agents.providers]\ncodex = 'gateway'\nopencode = 'gateway'\n");
    std::fs::write(path, config).unwrap();
    let out = env
        .run(&["agents", "apply", "--yes", "--format", "json"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(!String::from_utf8_lossy(&out.stdout).contains("native-provider-test-value"));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("native-provider-test-value"));
    let codex = std::fs::read_to_string(env.path().join(".codex/config.toml")).unwrap();
    assert!(codex.contains("experimental_bearer_token = \"native-provider-test-value\""));
    let opencode: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".config/opencode/opencode.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        opencode["provider"]["gateway"]["options"]["apiKey"],
        "native-provider-test-value"
    );
}

#[test]
fn doctor_declared_provider_network_check_is_explicit_and_offline_wins() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = []\n[providers.direct]\nkind = 'openai'\nkey = 'env://MUSTER_PROVIDER_TEST_KEY'\n");
    env.fake_exe("curl", "printf '%s\\n' \"$@\" > \"$HOME/curl-args\"\ncat > \"$HOME/curl-stdin\"\nprintf '{}\\n401'");
    for args in [
        vec!["doctor", "--format", "json"],
        vec!["doctor", "--check-keys", "--offline", "--format", "json"],
    ] {
        let output = env
            .run(&args)
            .env("MUSTER_PROVIDER_TEST_KEY", "provider-network-test-value")
            .output()
            .unwrap();
        assert!(!String::from_utf8_lossy(&output.stdout).contains("provider-network-test-value"));
        assert!(!env.path().join("curl-args").exists());
    }
    let output = env
        .run(&["doctor", "--check-keys", "--format", "json"])
        .env("MUSTER_PROVIDER_TEST_KEY", "provider-network-test-value")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("api_key_invalid"));
    assert!(!stdout.contains("provider-network-test-value"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("provider-network-test-value"));
    let args = std::fs::read_to_string(env.path().join("curl-args")).unwrap();
    assert!(args.contains("https://api.openai.com/v1/models"));
    assert!(!args.contains("provider-network-test-value"));
    let stdin = std::fs::read_to_string(env.path().join("curl-stdin")).unwrap();
    assert!(stdin.contains("Authorization: Bearer provider-network-test-value"));
}

#[test]
fn unresolved_opencode_provider_does_not_drop_explicit_agent_model() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['opencode']\n[agents.models]\nopencode = 'manual/model'\n[agents.providers]\nopencode = 'gateway'\n[providers.gateway]\nkind = 'openai-compatible'\nkey = 'env://MUSTER_PROVIDER_ABSENT'\n[secrets.'env://MUSTER_PROVIDER_ABSENT']\nallow = ['opencode']\n");
    env.run(&["agents", "apply", "--yes"])
        .env_remove("MUSTER_PROVIDER_ABSENT")
        .assert()
        .success();
    let oc: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(env.path().join(".config/opencode/opencode.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(oc["model"], "manual/model");
}

#[test]
fn provider_rm_clears_profile_selections_without_leaving_invalid_config() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[providers.gateway]\nkind = 'anthropic'\n[profiles.work.agents.providers]\nclaude = 'gateway'\n");
    env.run(&["provider", "rm", "gateway", "--yes"])
        .assert()
        .success();
    env.run(&["provider", "list", "--format", "json"])
        .assert()
        .success();
    let text = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(!text.contains("gateway"));
}

#[test]
fn plan_explains_provider_field_provenance_without_key_values() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude']\n[agents.providers]\nclaude = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\ndefault_model = 'example'\nkey = 'env://MUSTER_PROVIDER_TEST_KEY'\n[secrets.'env://MUSTER_PROVIDER_TEST_KEY']\nallow = ['claude']\n");
    let output = env
        .run(&["plan", "--explain", "--format", "json"])
        .env("MUSTER_PROVIDER_TEST_KEY", "provider-explain-test-value")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);
    assert!(!text.contains("provider-explain-test-value"));
    let envelope: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(
        envelope["result"]["explain"]["providers"]["gateway"]["default_model"]["layer"],
        "team"
    );
    assert_eq!(
        envelope["result"]["explain"]["providers"]["gateway"]["key"]["layer"],
        "team"
    );
}

#[test]
fn provider_list_distinguishes_selected_agents_from_allowed_capable_receivers() {
    let env = MusterEnv::new();
    seed(&env, "version = 1\n[agents]\nenabled = ['claude', 'codex', 'antigravity']\n[agents.providers]\nclaude = 'gateway'\ncodex = 'gateway'\nantigravity = 'gateway'\n[providers.gateway]\nkind = 'anthropic'\nkey = 'env://MUSTER_PROVIDER_TEST_KEY'\n[secrets.'env://MUSTER_PROVIDER_TEST_KEY']\nallow = ['claude', 'antigravity']\n");
    let (_, envelope) = env.run_json(&["provider", "list", "--format", "json"]);
    assert_eq!(
        envelope.result()["providers"][0]["agents"],
        serde_json::json!(["claude"])
    );
    assert_eq!(
        envelope.result()["providers"][0]["selected_agents"],
        serde_json::json!(["antigravity", "claude", "codex"])
    );
}
