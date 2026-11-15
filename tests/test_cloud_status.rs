mod support;
use support::MusterEnv;

#[test]
fn status_without_link_is_unlinked() {
    let env = MusterEnv::new();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "status"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert!(envelope.ok());
    assert_eq!(envelope.result()["status"], "unlinked");
}
#[test]
fn status_reports_link_without_exposing_token() {
    let env = MusterEnv::new();
    let state = env.path().join(".local/state/muster/cloud");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("link.json"), r#"{"endpoint":"https://cloud.example.com","workspace":"workspace-1","device":"device-1","token":"sensitive-test-token"}"#).unwrap();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "status"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["status"], "linked");
    assert_eq!(envelope.result()["workspace"], "workspace-1");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("sensitive-test-token"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("sensitive-test-token"));
}
#[test]
fn unlink_is_idempotent_and_preserves_local_data_with_invalid_fleet() {
    let env = MusterEnv::new();
    let state = env.path().join(".local/state/muster");
    let cloud = state.join("cloud");
    std::fs::create_dir_all(&cloud).unwrap();
    std::fs::write(cloud.join("link.json"), r#"{"endpoint":"https://cloud.example.com","workspace":"ws","device":"dev","token":"unlink-secret"}"#).unwrap();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "invalid = [").unwrap();
    std::fs::write(env.config_dir().join("fleet.state.json"), "invalid fleet").unwrap();
    std::fs::create_dir_all(state.join("generations")).unwrap();
    std::fs::write(state.join("generations/keep"), "generation").unwrap();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "status"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["status"], "linked");
    for _ in 0..2 {
        let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "unlink"]);
        assert!(output.status.success(), "{}", envelope.raw);
        assert!(!String::from_utf8_lossy(&output.stdout).contains("unlink-secret"));
    }
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "status"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(envelope.result()["status"], "unlinked");
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap(),
        "invalid = ["
    );
    assert_eq!(
        std::fs::read_to_string(env.config_dir().join("fleet.state.json")).unwrap(),
        "invalid fleet"
    );
    assert_eq!(
        std::fs::read_to_string(state.join("generations/keep")).unwrap(),
        "generation"
    );
    let (output, _) = env.run_json_strict(&["--format", "json", "sync"]);
    assert!(
        !output.status.success(),
        "sync must still reject invalid fleet"
    );
}
#[test]
fn status_rejects_insecure_remote_endpoint_without_exposing_credential() {
    let env = MusterEnv::new();
    let dir = env.path().join(".local/state/muster/cloud");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("link.json"), r#"{"endpoint":"http://remote.example.com","workspace":"ws","device":"dev","token":"private-token"}"#).unwrap();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "status"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert!(!String::from_utf8_lossy(&output.stdout).contains("private-token"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("private-token"));
}
#[test]
fn logout_removes_account_even_if_fleet_is_corrupt() {
    let env = MusterEnv::new();
    let state = env.path().join(".local/state/muster/cloud");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        state.join("account.json"),
        r#"{"endpoint":"http://127.0.0.1:1","account_id":"alice","token":"mca_test_secret"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "invalid = [").unwrap();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "logout"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert!(!state.join("account.json").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("mca_test_secret"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("mca_test_secret"));
}
