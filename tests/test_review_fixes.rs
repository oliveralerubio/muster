//! Code-review fixes on `feat/muster-v1`: `secret check` without decryption,
//! recovery key off argv, `profile`, agent installer, scheduler, uninstall.
mod support;

use std::io::Write;
use std::path::Path;
use std::process::{Output, Stdio};

use serde_json::Value;
use support::{Envelope, MusterEnv};

fn ok(env: &MusterEnv, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert!(out.status.success(), "{args:?}: {}", envelope.raw);
    assert!(envelope.ok(), "{args:?}: {}", envelope.raw);
    envelope.result().clone()
}

fn fails(env: &MusterEnv, args: &[&str], code: i32) -> Envelope {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let (out, envelope) = env.run_json(&full);
    assert_eq!(out.status.code(), Some(code), "{args:?}: {}", envelope.raw);
    assert!(!envelope.ok());
    envelope
}

/// `muster --format json <args>` with `stdin` piped in and extra env vars.
fn with_stdin(env: &MusterEnv, args: &[&str], stdin: &str, vars: &[(&str, &str)]) -> Output {
    let mut cmd = std::process::Command::new(assert_cmd::cargo::cargo_bin("muster"));
    for (k, v) in env.run(&[]).get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    for (k, v) in vars {
        cmd.env(k, v);
    }
    let mut child = cmd
        .args(["--format", "json"])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn envelope(out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("invalid JSON {e}: {stdout}"))
}

/// `muster init --yes`; returns the recovery key.
fn init(env: &MusterEnv) -> String {
    ok(env, &["init", "--yes", "--machine", "laptop"])["recovery_key"]
        .as_str()
        .unwrap()
        .to_string()
}

fn add_secret(env: &MusterEnv, path: &str, value: &str) {
    let out = with_stdin(env, &["secret", "add", path, "--value-stdin"], value, &[]);
    assert!(out.status.success(), "{}", envelope(&out));
}

fn codes(envelope: &Value) -> Vec<String> {
    envelope["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap().to_string())
        .collect()
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// --- secret check -----------------------------------------------------------

#[test]
fn secret_check_confirms_a_native_secret_from_metadata_without_decrypting_it() {
    let env = MusterEnv::new();
    init(&env);
    add_secret(&env, "app/api", "s3cr3t-value");
    // Unreadable ciphertext: anything that decrypts would fail now.
    std::fs::write(env.config_dir().join("vault/app.age"), "not age").unwrap();
    // `secret check` verifies the signed fleet state first: sign the damaged file.
    ok(&env, &["trust", "resign", "--yes"]);

    let result = ok(&env, &["secret", "check", "native://app/api"]);
    assert_eq!(result["reference"], "native://app/api");
    assert_eq!(result["exists"], true, "{result}");

    // Resolving does decrypt, so it fails on the damaged ciphertext.
    fails(&env, &["secret", "get", "native://app/api"], 5);
    let missing = fails(&env, &["secret", "check", "native://app/nope"], 1);
    assert!(missing.raw.to_string().contains("secret_not_found"));
}

#[test]
fn secret_check_never_asks_a_vendor_cli_for_the_value() {
    let env = MusterEnv::new();
    init(&env);
    let log = env.path().join("op.log");
    env.fake_exe(
        "op",
        &format!("echo \"$@\" >> '{}'\necho 2.30.0", log.display()),
    );

    let (out, envelope) = env.run_json(&[
        "--format",
        "json",
        "secret",
        "check",
        "op://vault/item/field",
    ]);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["available"], true);
    assert!(envelope.result()["exists"].is_null(), "{}", envelope.raw);
    assert!(codes(&envelope.raw).contains(&"existence_unverified".to_string()));
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        !calls.contains("read"),
        "check must not read the value: {calls}"
    );
}

#[test]
fn secret_check_reports_env_variables_by_presence() {
    let env = MusterEnv::new();
    init(&env);
    let mut cmd = env.run(&["--format", "json", "secret", "check", "env://MY_TOKEN"]);
    cmd.env("MY_TOKEN", "x");
    let (out, envelope) = support::json(cmd);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["exists"], true);
    fails(&env, &["secret", "check", "env://MY_TOKEN"], 1);
}

// --- recovery key -----------------------------------------------------------

#[test]
fn recovery_key_via_stdin_is_accepted_without_a_diagnostic() {
    let machine = MusterEnv::new();
    let recovery_key = init(&machine);
    ok(&machine, &["enroll", "--name", "bob"]);

    let out = with_stdin(
        &machine,
        &["approve", "bob", "--recovery-key-stdin", "--yes"],
        &format!("{recovery_key}\n"),
        &[],
    );
    assert!(out.status.success(), "{}", envelope(&out));
    let env_json = envelope(&out);
    assert_eq!(env_json["ok"], true);
    assert!(!codes(&env_json).contains(&"insecure_recovery_key_argument".to_string()));
}

#[test]
fn recovery_key_via_env_var_is_accepted_without_a_diagnostic() {
    let machine = MusterEnv::new();
    let recovery_key = init(&machine);
    ok(&machine, &["enroll", "--name", "bob"]);

    let out = with_stdin(
        &machine,
        &["approve", "bob", "--yes"],
        "",
        &[("MUSTER_RECOVERY_KEY", &recovery_key)],
    );
    assert!(out.status.success(), "{}", envelope(&out));
    let env_json = envelope(&out);
    assert_eq!(env_json["ok"], true);
    assert!(!codes(&env_json).contains(&"insecure_recovery_key_argument".to_string()));
}

#[test]
fn recovery_key_via_argv_is_rejected_with_usage_error() {
    let machine = MusterEnv::new();
    let recovery_key = init(&machine);
    ok(&machine, &["enroll", "--name", "bob"]);

    let (out, envelope) = machine.run_json(&[
        "--format",
        "json",
        "approve",
        "bob",
        "--recovery-key",
        &recovery_key,
        "--yes",
    ]);
    assert_eq!(out.status.code(), Some(2));
    assert!(!envelope.ok());
    let raw_text = envelope.raw.to_string();
    assert!(
        !raw_text.contains(&recovery_key),
        "error must not echo the recovery key: {raw_text}"
    );
    assert!(
        raw_text.contains("--recovery-key-stdin") || raw_text.contains("MUSTER_RECOVERY_KEY"),
        "error must point to safe alternatives: {raw_text}"
    );
}

// --- profile ----------------------------------------------------------------

#[test]
fn profile_set_show_list_and_rm_manage_profiles_admin_only_in_team_mode() {
    let owner = MusterEnv::new();
    init(&owner);
    let config = owner.config_dir();
    let cfg = config.to_str().unwrap();
    assert_eq!(
        ok(&owner, &["profile", "list"])["profiles"],
        serde_json::json!([])
    );

    let file = owner.path().join("dev.toml");
    write(&file, "[agents.models]\nclaude = \"sonnet\"\n");
    let set = ok(
        &owner,
        &[
            "profile",
            "set",
            "dev",
            "--from-file",
            file.to_str().unwrap(),
            "--yes",
        ],
    );
    assert_eq!(set["name"], "dev");
    let manifest = std::fs::read_to_string(config.join("muster.toml")).unwrap();
    assert!(manifest.contains("[profiles.dev"), "{manifest}");

    let shown = ok(&owner, &["profile", "show", "dev"]);
    assert_eq!(
        shown["profile"]["agents"]["models"]["claude"], "sonnet",
        "{shown}"
    );
    let names: Vec<_> = ok(&owner, &["profile", "list"])["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].clone())
        .collect();
    assert_eq!(names, vec![serde_json::json!("dev")]);

    // Invalid profile contents are a config error and change nothing.
    let bad = owner.path().join("bad.toml");
    write(&bad, "bogus = 1\n");
    fails(
        &owner,
        &[
            "profile",
            "set",
            "dev",
            "--from-file",
            bad.to_str().unwrap(),
        ],
        3,
    );
    fails(&owner, &["profile", "show", "nope"], 2);

    // Team mode: a member who is not an admin cannot change profiles.
    let alice = MusterEnv::new();
    ok(&owner, &["member", "add", "alice", "--role", "member"]);
    ok(&owner, &["member", "approve", "alice", "--yes"]);
    ok(&alice, &["--config", cfg, "enroll", "--name", "alice-box"]);
    ok(
        &owner,
        &["approve", "alice-box", "--member", "alice", "--yes"],
    );
    let denied = fails(
        &alice,
        &[
            "--config",
            cfg,
            "profile",
            "set",
            "x",
            "--from-file",
            file.to_str().unwrap(),
        ],
        5,
    );
    assert!(denied.raw.to_string().contains("admin"), "{}", denied.raw);
    fails(
        &alice,
        &["--config", cfg, "profile", "rm", "dev", "--yes"],
        5,
    );

    // A profile in use cannot be removed; it shows where it is used.
    ok(&owner, &["member", "profile", "alice", "--profile", "dev"]);
    let shown = ok(&owner, &["profile", "show", "dev"]);
    assert_eq!(
        shown["used_by"]["members"],
        serde_json::json!(["alice"]),
        "{shown}"
    );
    fails(&owner, &["profile", "rm", "dev", "--yes"], 2);
    ok(&owner, &["member", "profile", "alice", "--profile", ""]);

    fails(&owner, &["profile", "rm", "dev"], 2); // no TTY, no --yes
    ok(&owner, &["--dry-run", "profile", "rm", "dev", "--yes"]);
    assert!(std::fs::read_to_string(config.join("muster.toml"))
        .unwrap()
        .contains("[profiles.dev"));
    assert_eq!(
        ok(&owner, &["profile", "rm", "dev", "--yes"])["removed"],
        "dev"
    );
    fails(&owner, &["profile", "show", "dev"], 2);
}

// --- agents install ---------------------------------------------------------

#[test]
fn agents_install_downloads_and_verifies_sha256_refusing_mismatch() {
    let env = MusterEnv::new();
    init(&env);
    let art_dir = env.path().join("artifacts");
    let claude_bin = art_dir.join("claude");
    write(&claude_bin, "#!/bin/sh\necho \"claude 9.9.9\"\n");
    let sha = "9008cbdc57f2b4420ec6fbad28b2d3262c0b4bf9c851929f91307f9199715a77";
    let url = format!("file://{}", claude_bin.display());

    let lock =
        format!("[agents.claude]\nversion = \"9.9.9\"\nurl = \"{url}\"\nsha256 = \"{sha}\"\n");
    write(&env.config_dir().join("muster.lock"), &lock);

    // Dry-run downloads & verifies checksum without writing into ~/.local/bin
    let dry = ok(
        &env,
        &["--dry-run", "agents", "install", "--agent", "claude"],
    );
    assert_eq!(dry["dry_run"], true);
    assert!(!env.path().join(".local/bin/claude").exists());

    // Clean install writes binary with 0755 permissions
    let res = ok(&env, &["agents", "install", "--agent", "claude"]);
    assert_eq!(res["installed"][0]["agent"], "claude");
    assert_eq!(res["installed"][0]["version"], "9.9.9");
    assert_eq!(res["installed"][0]["action"], "install");
    let installed_bin = env.path().join(".local/bin/claude");
    assert!(installed_bin.is_file());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&installed_bin)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }

    // Re-running reports unchanged and does not re-download
    let res2 = ok(&env, &["agents", "install", "--agent", "claude"]);
    assert_eq!(res2["installed"][0]["action"], "unchanged");

    // Mismatched checksum refuses with exit code 5 (Security) and installs nothing
    let bad_bin = art_dir.join("tampered");
    write(&bad_bin, "tampered");
    let bad_url = format!("file://{}", bad_bin.display());
    let bad_lock = format!(
        "[agents.codex]\nversion = \"1.0.0\"\nurl = \"{bad_url}\"\nsha256 = \"deadbeef00000000000000000000000000000000000000000000000000000000\"\n"
    );
    write(&env.config_dir().join("muster.lock"), &bad_lock);
    let bad = fails(&env, &["agents", "install", "--agent", "codex"], 5);
    assert!(bad.raw.to_string().contains("checksum mismatch"));
    assert!(!env.path().join(".local/bin/codex").exists());
}

#[test]
fn agents_install_antigravity_installs_the_agy_binary() {
    let env = MusterEnv::new();
    init(&env);
    env.fake_exe("agy", "exit 1");
    let artifact = env.path().join("artifacts").join("agy");
    write(&artifact, "#!/bin/sh\necho \"agy 1.3.0\"\n");
    let sha = "f4b6c1e9b86760e8688506eb7301f385986370c143a0d9ed8123ce646b81a516";
    let url = format!("file://{}", artifact.display());
    let lock =
        format!("[agents.antigravity]\nversion = \"1.3.0\"\nurl = \"{url}\"\nsha256 = \"{sha}\"\n");
    write(&env.config_dir().join("muster.lock"), &lock);

    let res = ok(&env, &["agents", "install", "--agent", "antigravity"]);
    assert_eq!(res["installed"][0]["agent"], "antigravity");
    assert_eq!(res["installed"][0]["action"], "install");
    assert!(env.path().join(".local/bin/agy").is_file());
    assert!(!env.path().join(".local/bin/antigravity").exists());
}

// --- schedule ---------------------------------------------------------------

#[test]
fn schedule_enable_writes_a_systemd_user_timer_and_disable_removes_it() {
    let env = MusterEnv::new();
    init(&env);
    let log = env.path().join("systemctl.log");
    env.fake_exe("systemctl", &format!("echo \"$@\" >> '{}'", log.display()));
    let units = env.path().join(".config/systemd/user");

    let dry = ok(
        &env,
        &["--dry-run", "schedule", "enable", "--backend", "systemd"],
    );
    assert_eq!(dry["backend"], "systemd");
    assert!(!units.join("muster-sync.timer").exists());

    // Applying needs --yes; without it nothing is installed.
    fails(
        &env,
        &[
            "schedule",
            "enable",
            "--backend",
            "systemd",
            "--interval",
            "30",
        ],
        2,
    );
    assert!(!units.join("muster-sync.timer").exists());

    let enabled = ok(
        &env,
        &[
            "schedule",
            "enable",
            "--backend",
            "systemd",
            "--interval",
            "30",
            "--yes",
        ],
    );
    assert_eq!(enabled["enabled"], true, "{enabled}");
    let service = std::fs::read_to_string(units.join("muster-sync.service")).unwrap();
    let timer = std::fs::read_to_string(units.join("muster-sync.timer")).unwrap();
    assert!(service.contains("Type=oneshot"), "{service}");
    assert!(service.contains("\"sync\""), "{service}");
    assert!(
        service.contains(env.config_dir().to_str().unwrap()),
        "the unit syncs this config repo: {service}"
    );
    assert!(timer.contains("OnUnitActiveSec=30min"), "{timer}");
    assert!(timer.contains("WantedBy=timers.target"), "{timer}");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("--user daemon-reload"), "{calls}");
    assert!(
        calls.contains("--user enable --now muster-sync.timer"),
        "{calls}"
    );

    let status = ok(&env, &["schedule", "status"]);
    assert_eq!(status["enabled"], true, "{status}");
    assert_eq!(status["backend"], "systemd", "{status}");
    assert_eq!(status["interval_minutes"], 30, "{status}");

    let disabled = ok(&env, &["schedule", "disable", "--yes"]);
    assert_eq!(disabled["enabled"], false, "{disabled}");
    assert!(!units.join("muster-sync.timer").exists());
    assert!(!units.join("muster-sync.service").exists());
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        calls.contains("--user disable --now muster-sync.timer"),
        "{calls}"
    );
    assert_eq!(ok(&env, &["schedule", "status"])["enabled"], false);
}

#[test]
fn schedule_enable_writes_a_launchd_agent_plist() {
    let env = MusterEnv::new();
    init(&env);
    // No launchctl on PATH: the plist is written and loading is left to the user.
    let result = ok(
        &env,
        &[
            "schedule",
            "enable",
            "--backend",
            "launchd",
            "--interval",
            "15",
            "--yes",
        ],
    );
    assert_eq!(result["backend"], "launchd");
    let plist = std::fs::read_to_string(
        env.path()
            .join("Library/LaunchAgents/dev.muster.sync.plist"),
    )
    .unwrap();
    assert!(plist.contains("<key>StartInterval</key>"), "{plist}");
    assert!(plist.contains("<integer>900</integer>"), "{plist}");
    assert!(plist.contains("<string>sync</string>"), "{plist}");
    let status = ok(&env, &["schedule", "status"]);
    assert_eq!(status["interval_minutes"], 15, "{status}");
}

#[test]
#[cfg(target_os = "linux")] // auto picks launchd on macOS
fn schedule_without_systemd_or_launchd_falls_back_to_manual_instructions() {
    let env = MusterEnv::new();
    init(&env);
    env.link_system_exe("git");
    let mut cmd = env.run_isolated(&["--format", "json", "schedule", "enable", "--yes"]);
    cmd.env("PATH", &env.bin);
    let (out, envelope) = support::json(cmd);
    assert!(out.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["backend"], "manual", "{}", envelope.raw);
    assert_eq!(envelope.result()["enabled"], false);
    assert!(codes(&envelope.raw).contains(&"scheduler_manual".to_string()));
    assert!(
        envelope.raw.to_string().contains("muster sync"),
        "{}",
        envelope.raw
    );
    assert!(!env.path().join(".config/systemd").exists());
}

// --- uninstall --------------------------------------------------------------

#[test]
fn uninstall_removes_state_and_units_leaving_agent_files_in_place() {
    let env = MusterEnv::new();
    init(&env);

    let cfg = env.config_dir();
    write(&cfg.join("AGENTS.md"), "# Canonical Instructions\n");
    let mut manifest = std::fs::read_to_string(cfg.join("muster.toml")).unwrap();
    manifest.push_str(
        "\n[agents]\nenabled = [\"claude\"]\n\n[agents.instructions]\nsource = \"AGENTS.md\"\n",
    );
    write(&cfg.join("muster.toml"), &manifest);
    // `agents apply` verifies the signed fleet state first.
    ok(&env, &["trust", "resign", "--yes"]);

    let apply_res = ok(&env, &["agents", "apply", "--yes"]);
    assert_eq!(apply_res["applied"], true);
    let claude_md = env.path().join(".claude/CLAUDE.md");
    let agents_md = env.path().join(".claude/AGENTS.md");
    assert!(claude_md.is_file());
    assert!(agents_md.is_file());

    ok(
        &env,
        &["schedule", "enable", "--backend", "systemd", "--yes"],
    );
    let timer = env.path().join(".config/systemd/user/muster-sync.timer");
    assert!(timer.is_file());

    let state_dir = env.path().join(".local/state/muster");
    assert!(state_dir.is_dir());

    let refused = fails(&env, &["uninstall"], 2);
    assert!(refused.raw.to_string().contains("--yes"), "{}", refused.raw);

    let dry = ok(&env, &["--dry-run", "uninstall", "--yes"]);
    assert_eq!(dry["dry_run"], true);
    assert!(state_dir.is_dir());
    assert!(timer.is_file());
    assert!(claude_md.is_file());

    let res = ok(&env, &["uninstall", "--yes"]);
    assert_eq!(res["uninstalled"], true);
    assert_eq!(res["agent_files_preserved"], true);
    let kept = res["kept"]["agent_files"].to_string();
    assert!(kept.contains(claude_md.to_str().unwrap()), "{kept}");

    assert!(!state_dir.exists(), "state dir must be removed");
    assert!(!timer.exists(), "scheduler unit must be removed");

    assert!(claude_md.is_file(), "agent files must be preserved");
    assert!(agents_md.is_file(), "agent files must be preserved");
    let content = std::fs::read_to_string(&agents_md).unwrap();
    assert!(content.contains("Canonical Instructions"));
}
