mod support;

#[test]
fn sync_records_last_result_and_status_exposes_it() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    env.run(&["sync"]).assert().success();
    let path = env.path().join(".local/state/muster/sync-status.json");
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(status["result"], "ok");
    assert!(status["ts"].is_string());
    assert!(status["applied"].is_boolean());
    let (_, output) = env.run_json(&["--format", "json", "status"]);
    assert_eq!(output.result()["sync"], status);
}

#[test]
fn hourly_systemd_install_and_uninstall_use_fake_control_tool() {
    let env = support::MusterEnv::new();
    env.fake_exe("systemctl", "exit 0");
    env.run(&["schedule", "enable", "--backend", "systemd", "--yes"])
        .assert()
        .success();
    let dir = env.path().join(".config/systemd/user");
    let timer = std::fs::read_to_string(dir.join("muster-sync.timer")).unwrap();
    assert!(timer.contains("OnCalendar=hourly"));
    assert!(timer.contains("Persistent=true"));
    assert!(timer.contains("RandomizedDelaySec=300"));
    env.run(&["schedule", "disable"]).assert().success();
    assert!(!dir.join("muster-sync.timer").exists());
    assert!(!dir.join("muster-sync.service").exists());
}

#[test]
fn held_sync_lock_skips_without_writing_status() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let dir = env.path().join(".local/state/muster");
    std::fs::create_dir_all(&dir).unwrap();
    let lock = std::fs::File::create(dir.join("sync.lock")).unwrap();
    lock.lock().unwrap();
    let (output, envelope) = env.run_json(&["--format", "json", "sync"]);
    assert!(output.status.success());
    assert!(envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == "sync_skipped_locked"));
    assert!(!dir.join("sync-status.json").exists());
}

#[test]
fn failed_sync_records_error_and_releases_lock() {
    let env = support::MusterEnv::new();
    env.run(&["sync"]).assert().failure();
    let path = env.path().join(".local/state/muster/sync-status.json");
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(status["result"], "error");
    assert_eq!(status["applied"], false);
    env.run(&["--yes", "init"]).assert().success();
    env.run(&["sync"]).assert().success();
    let status: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(status["result"], "ok");
}

#[test]
fn dry_run_sync_does_not_write_state() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    env.run(&["--dry-run", "sync"]).assert().success();
    let dir = env.path().join(".local/state/muster");
    assert!(!dir.join("sync.lock").exists());
    assert!(!dir.join("sync-status.json").exists());
}
