mod support;

use std::path::Path;

fn init(env: &support::MusterEnv) {
    env.run(&["--yes", "init"]).assert().success();
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn git_log(env: &support::MusterEnv) -> String {
    let out = std::process::Command::new("git")
        .args(["log", "-p", "--all"])
        .current_dir(env.config_dir())
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).to_string()
}

/// Fake `bws`: records its arguments, prints one project's secrets.
fn fake_bws(env: &support::MusterEnv) {
    let log = env.path().join("bws-args");
    env.fake_exe(
        "bws",
        &format!(
            "echo \"$@\" >> '{}'\nprintf '%s\\n' '[{{\"id\":\"1\",\"key\":\"API_KEY\",\"value\":\"sk-123\",\"note\":\"\",\"projectId\":\"project-1\"}},{{\"id\":\"2\",\"key\":\"DB_PASSWORD\",\"value\":\"hunter2\",\"note\":\"\",\"projectId\":\"project-1\"}}]'",
            log.display()
        ),
    );
}

#[test]
fn bws_import_migrates_secrets_without_exposing_values() {
    let env = support::MusterEnv::new();
    init(&env);
    fake_bws(&env);

    let output = env
        .run(&[
            "--format",
            "json",
            "import",
            "bws",
            "--project",
            "project-1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "bws import failed: {stdout} {stderr}"
    );
    for value in ["sk-123", "hunter2"] {
        assert!(!stdout.contains(value), "secret leaked to stdout: {stdout}");
        assert!(!stderr.contains(value), "secret leaked to stderr: {stderr}");
        assert!(!git_log(&env).contains(value), "secret leaked into git");
    }
    let args = std::fs::read_to_string(env.path().join("bws-args")).unwrap();
    assert!(
        args.contains("secret list project-1"),
        "unexpected bws call: {args}"
    );

    // The values are now readable through the native vault.
    let get = env
        .run(&["--yes", "secret", "get", "native://bws/API_KEY", "--reveal"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&get.stdout).contains("sk-123"));
    let (_, list) = env.run_json(&["--format", "json", "secret", "list"]);
    let names: Vec<&str> = list
        .result()
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["API_KEY", "DB_PASSWORD"]);
}

#[test]
fn bws_reimport_keeps_existing_secrets_and_dry_run_writes_nothing() {
    let env = support::MusterEnv::new();
    init(&env);
    fake_bws(&env);

    let (out, dry) = env.run_json(&[
        "--format",
        "json",
        "--dry-run",
        "import",
        "bws",
        "--group",
        "dry",
    ]);
    assert!(out.status.success());
    assert_eq!(dry.result()["imported"].as_array().unwrap().len(), 2);
    assert!(
        !env.config_dir().join("vault/dry.age").exists(),
        "dry-run wrote the vault"
    );

    env.run(&["import", "bws"]).assert().success();
    let (out, again) = env.run_json(&["--format", "json", "import", "bws"]);
    assert!(out.status.success());
    assert!(again.result()["imported"].as_array().unwrap().is_empty());
    assert_eq!(again.result()["skipped"].as_array().unwrap().len(), 2);
}

#[test]
fn bws_import_without_cli_is_a_dependency_error() {
    let env = support::MusterEnv::new();
    init(&env);
    let output = env
        .run(&["import", "bws"])
        .env("PATH", env.bin.display().to_string())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
}

#[test]
fn chezmoi_import_is_visible_in_plan() {
    let env = support::MusterEnv::new();
    init(&env);
    let source = env.path().join("chezmoi");
    write(&source.join("dot_gitconfig"), "[user]\n  name = Ada\n");
    write(
        &source.join("dot_config/tool/AGENTS.md"),
        "Imported instructions\n",
    );

    let imported = env
        .run(&["import", "chezmoi", source.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        imported.status.success(),
        "chezmoi import failed: {}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let plan = env.run(&["plan"]).output().unwrap();
    assert!(
        plan.status.success(),
        "plan failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(&plan.stdout),
        String::from_utf8_lossy(&plan.stderr)
    );
    assert!(
        output.contains("AGENTS.md") && output.contains("Imported instructions"),
        "imported chezmoi content absent from plan: {output}"
    );
    assert!(output.contains(".gitconfig"), "{output}");
    assert!(
        git_log(&env).contains("Imported instructions"),
        "import was not committed"
    );
}

#[test]
fn chezmoi_attributes_apply_on_sync_and_removed_files_are_pruned() {
    use std::os::unix::fs::PermissionsExt;
    let env = support::MusterEnv::new();
    init(&env);
    let source = env.path().join("chezmoi");
    write(&source.join("private_dot_ssh/config"), "Host *\n");
    write(
        &source.join("dot_local/bin/executable_hello"),
        "#!/bin/sh\necho hi\n",
    );
    write(&source.join("dot_profile.tmpl"), "export EDITOR=vi\n");
    env.run(&["import", "chezmoi", source.to_str().unwrap()])
        .assert()
        .success();

    env.run(&["--yes", "sync"]).assert().success();
    let mode = |p: &str| {
        std::fs::metadata(env.path().join(p))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(".ssh/config"), 0o600);
    assert_eq!(mode(".local/bin/hello") & 0o111, 0o111);
    assert_eq!(
        std::fs::read_to_string(env.path().join(".profile")).unwrap(),
        "export EDITOR=vi\n"
    );

    // Dropping a file from the manifest removes what Muster created.
    let manifest_path = env.config_dir().join("muster.toml");
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    let start = manifest.find("[files.\".profile\"]").unwrap();
    let end = manifest[start + 1..]
        .find("\n[")
        .map_or(manifest.len(), |i| start + 1 + i + 1);
    std::fs::write(
        &manifest_path,
        format!("{}{}", &manifest[..start], &manifest[end..]),
    )
    .unwrap();
    env.run(&["--yes", "sync"]).assert().success();
    assert!(!env.path().join(".profile").exists());
    assert!(env.path().join(".ssh/config").exists());
}

#[test]
fn chezmoi_templates_and_unsupported_entries_are_reported_not_applied() {
    let env = support::MusterEnv::new();
    init(&env);
    let source = env.path().join("chezmoi");
    write(
        &source.join("dot_bashrc.tmpl"),
        "export HOST={{ .chezmoi.hostname }}\n",
    );
    write(&source.join("run_once_setup.sh"), "#!/bin/sh\n");
    write(
        &source.join("dot_config/muster/muster.toml"),
        "version = 1\n",
    );
    write(&source.join("README.md"), "docs\n");
    write(&source.join(".chezmoiignore"), "README.md\n");

    let (out, envelope) = env.run_json(&[
        "--format",
        "json",
        "import",
        "chezmoi",
        source.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let codes: Vec<&str> = envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert!(
        codes.contains(&"chezmoi_template_untranslated"),
        "{codes:?}"
    );
    let result = envelope.result();
    assert!(result["files"].as_array().unwrap().is_empty(), "{result}");
    let skipped = result["skipped"].to_string();
    assert!(
        skipped.contains("run_once_setup.sh") && skipped.contains("muster.toml"),
        "{skipped}"
    );
    assert!(
        !result.to_string().contains("README"),
        "ignored file was imported: {result}"
    );
    assert!(env
        .config_dir()
        .join("imports/chezmoi/dot_bashrc.tmpl")
        .exists());
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        !manifest.contains("bashrc") && manifest.contains("recovery"),
        "{manifest}"
    );
}

#[test]
fn cuartel_import_reads_mock_configuration() {
    let env = support::MusterEnv::new();
    init(&env);
    let source = env.path().join("cuartel");
    write(
        &source.join("muster.toml"),
        "version = 1\nhub_ip = \"100.64.0.7\"\n[agents]\nenabled = [\"claude\"]\n",
    );
    write(&source.join("AGENTS.md"), "Cuartel instructions\n");
    write(
        &source.join("mcp.json"),
        r#"{"mcpServers": {"github": {"command": "gh-mcp", "env": {"GITHUB_TOKEN": "ghp_literal", "API_KEY": "bws://abc-123"}}}}"#,
    );

    let (out, envelope) = env.run_json(&[
        "--format",
        "json",
        "import",
        "cuartel",
        source.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "cuartel import failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("AGENTS.md")).unwrap(),
        "Cuartel instructions\n"
    );
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(
        manifest.contains("claude") && manifest.contains("bws://abc-123"),
        "{manifest}"
    );
    assert!(
        manifest.contains("recovery"),
        "existing manifest was replaced: {manifest}"
    );
    assert!(
        !manifest.contains("ghp_literal") && !manifest.contains("100.64.0.7"),
        "{manifest}"
    );
    assert!(!git_log(&env).contains("ghp_literal"));
    assert!(envelope
        .diagnostics()
        .to_string()
        .contains("literal_secret_dropped"));

    let plan = env.run(&["plan"]).output().unwrap();
    assert!(String::from_utf8_lossy(&plan.stdout).contains("Cuartel instructions"));
}

#[test]
fn import_dry_run_reports_changes_without_writing() {
    let env = support::MusterEnv::new();
    init(&env);
    let source = env.path().join("chezmoi-dry");
    write(
        &source.join("dot_config/tool/AGENTS.md"),
        "Dry run instructions\n",
    );
    let cuartel = env.path().join("cuartel-dry");
    write(&cuartel.join("AGENTS.md"), "Dry Cuartel\n");
    let manifest_before = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();

    let output = env
        .run(&["import", "chezmoi", source.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        text.contains("AGENTS.md"),
        "dry-run did not describe import: {text}"
    );
    let cuartel_out = env
        .run(&["import", "cuartel", cuartel.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(cuartel_out.status.success());
    assert!(String::from_utf8_lossy(&cuartel_out.stdout).contains("AGENTS.md"));

    assert!(
        !env.config_dir().join("files").exists(),
        "dry-run wrote imported file"
    );
    assert!(
        !env.config_dir().join("AGENTS.md").exists(),
        "dry-run wrote instructions"
    );
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap(),
        manifest_before
    );
}
