mod support;

use axum::response::IntoResponse;
use muster_cloud::protocol::{DeviceApproveRequest, DeviceDecision};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

/// [`support::stack`] with the `free_port()` -> `spawn_cloud()` window
/// serialized across concurrent tests in this binary so two stacks starting at
/// the same time cannot pick the same loopback port.
async fn stack() -> support::Stack {
    static START_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = START_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    support::stack().await
}

#[tokio::test]
async fn login_approves_real_device_flow_and_stores_account_privately() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let created = support::http()
        .post(stack.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name":"Test workspace"}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    let workspace: muster_cloud::protocol::Workspace = created.json().await.unwrap();
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let binary =
        std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI");
    let mut child = tokio::process::Command::new(binary)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args([
            "cloud",
            "login",
            "--endpoint",
            &stack.cloud.base,
            "--no-browser",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let line = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("login must display approval instructions");
    assert!(!line.contains("mca_") && !line.contains("device_code"));
    let code = line.split_whitespace().last().expect("user code");
    assert!(line.contains(&stack.url("/device")), "{line}");
    let response = support::http()
        .post(stack.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&DeviceApproveRequest {
            user_code: code.into(),
            decision: DeviceDecision::Approve,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let account = home.join("state/muster/cloud/account.json");
    let stored = std::fs::read_to_string(&account).unwrap();
    assert!(stored.contains("mca_"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("mca_"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("mca_"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(account).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(home.join("state/muster/cloud"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    let listing = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["--format", "json", "cloud", "workspaces"])
        .output()
        .await
        .unwrap();
    assert!(
        listing.status.success(),
        "{}",
        String::from_utf8_lossy(&listing.stdout)
    );
    assert!(String::from_utf8_lossy(&listing.stdout).contains("Test workspace"));
    assert!(!String::from_utf8_lossy(&listing.stdout).contains("mca_"));
    let init = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["--yes", "init", "--machine", "laptop"])
        .output()
        .await
        .unwrap();
    assert!(init.status.success(), "init failed");
    let other = support::http()
        .post(stack.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name":"Other workspace"}))
        .send()
        .await
        .unwrap();
    assert_eq!(other.status(), 201);
    let ambiguous = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["--format", "json", "cloud", "link"])
        .output()
        .await
        .unwrap();
    assert!(!ambiguous.status.success());
    assert!(String::from_utf8_lossy(&ambiguous.stdout).contains("--workspace"));
    assert!(!home.join("state/muster/cloud/link.json").exists());
    let link = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args([
            "--format",
            "json",
            "cloud",
            "link",
            "--workspace",
            &workspace.id,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        link.status.success(),
        "{}",
        String::from_utf8_lossy(&link.stdout)
    );
    assert!(!String::from_utf8_lossy(&link.stdout).contains("mcd_"));
    let first_result: serde_json::Value = serde_json::from_slice(&link.stdout).unwrap();
    assert_eq!(first_result["result"]["bootstrap"], true);
    let repeat = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args([
            "--format",
            "json",
            "cloud",
            "link",
            "--workspace",
            &workspace.id,
        ])
        .output()
        .await
        .unwrap();
    assert!(repeat.status.success());
    let repeated: serde_json::Value = serde_json::from_slice(&repeat.stdout).unwrap();
    assert_eq!(
        repeated["result"]["device"],
        first_result["result"]["device"]
    );
    let link_state = std::fs::read_to_string(home.join("state/muster/cloud/link.json")).unwrap();
    assert!(link_state.contains("mcd_"));
    let sync = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["--format", "json", "sync", "--push"])
        .output()
        .await
        .unwrap();
    assert!(
        sync.status.success(),
        "{}",
        String::from_utf8_lossy(&sync.stdout)
    );
    let revisions = support::http()
        .get(stack.url(&format!("/v1/workspaces/{}/revisions", workspace.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(revisions.status(), 200);
    let revisions: Vec<muster_cloud::protocol::RevisionItem> = revisions.json().await.unwrap();
    // One `sync --push` publishes two accepted heads, newest first: the
    // signed genesis `init` committed, then the applied-profile record the
    // same sync committed after applying. Both are epoch 1: the applied
    // record changes operational manifest metadata only, never signed
    // governance. A third revision or a repeated genesis would mean an
    // accidental republication.
    assert_eq!(
        revisions.len(),
        2,
        "exactly genesis + applied record, no accidental republication"
    );
    assert_eq!(
        revisions[0].parent_revision.as_deref(),
        Some(revisions[1].revision.as_str()),
        "the applied record builds on the accepted genesis"
    );
    assert_eq!(revisions[1].parent_revision, None, "genesis has no parent");
    assert_eq!(
        revisions[0].message, "chore(fleet): laptop applied the approved profile",
        "newest accepted revision is the applied record"
    );
    assert_eq!(revisions[1].message, "chore(fleet): initialize fleet");
    assert_eq!(revisions[0].epoch, 1);
    assert_eq!(revisions[1].epoch, 1);
    assert_eq!(
        revisions[0].signer, revisions[1].signer,
        "both heads are signed by the same fleet key"
    );
    assert!(revisions[0].signer.starts_with("musterpk1"));
    assert!(!String::from_utf8_lossy(&sync.stdout).contains("mcd_"));
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    let cli = || {
        let mut c = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap());
        c.env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &second)
            .env("XDG_CONFIG_HOME", second.join("config"))
            .env("XDG_STATE_HOME", second.join("state"))
            .env("XDG_DATA_HOME", second.join("data"))
            .env("XDG_CACHE_HOME", second.join("cache"));
        c
    };
    let mut login = cli()
        .args([
            "cloud",
            "login",
            "--endpoint",
            &stack.cloud.base,
            "--no-browser",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut login_lines = BufReader::new(login.stderr.take().unwrap()).lines();
    let instruction =
        tokio::time::timeout(std::time::Duration::from_secs(15), login_lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    let response = support::http()
        .post(stack.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&DeviceApproveRequest {
            user_code: instruction.split_whitespace().last().unwrap().into(),
            decision: DeviceDecision::Approve,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(login.wait_with_output().await.unwrap().status.success());
    assert!(cli()
        .args(["--yes", "init", "--machine", "second"])
        .output()
        .await
        .unwrap()
        .status
        .success());
    let second_link = cli()
        .args([
            "--format",
            "json",
            "cloud",
            "link",
            "--workspace",
            &workspace.id,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        second_link.status.success(),
        "{}",
        String::from_utf8_lossy(&second_link.stdout)
    );
    let result: serde_json::Value = serde_json::from_slice(&second_link.stdout).unwrap();
    assert_eq!(result["result"]["bootstrap"], false);
    assert_eq!(result["result"]["status"], "linked");
    assert!(!String::from_utf8_lossy(&second_link.stdout).contains("mcd_"));

    // The second machine downloads the fleet history. A first pull of a fleet
    // this machine has never pinned must not trust it without naming the
    // signer (non-interactive: no typed confirmation possible).
    let unconfirmed = cli()
        .args(["--format", "json", "sync", "--pull"])
        .output()
        .await
        .unwrap();
    assert!(
        !unconfirmed.status.success(),
        "first pull must fail closed without --expect-signer: {}",
        String::from_utf8_lossy(&unconfirmed.stdout)
    );
    let pull = cli()
        .args([
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &revisions[0].signer,
        ])
        .output()
        .await
        .unwrap();
    eprintln!(
        "DIAG pull: status={} revisions0.signer={} revisions1.signer={}",
        pull.status, revisions[0].signer, revisions[1].signer
    );
    assert!(
        pull.status.success(),
        "{}",
        String::from_utf8_lossy(&pull.stdout)
    );
    let joined: serde_json::Value = serde_json::from_slice(&pull.stdout).unwrap();
    assert_eq!(joined["result"]["joined"], true, "{joined}");
    assert_eq!(joined["result"]["applied"], false);
    let joined_manifest =
        std::fs::read_to_string(second.join("config/muster/muster.toml")).unwrap();
    assert!(
        joined_manifest.contains("[machines.laptop]"),
        "pull must replace the scaffold with the fleet manifest: {joined_manifest}"
    );

    std::fs::remove_dir_all(second).unwrap();
    let account_data: serde_json::Value = serde_json::from_str(&stored).unwrap();
    let token = account_data["token"].as_str().unwrap();
    let logout = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args(["--format", "json", "cloud", "logout"])
        .output()
        .await
        .unwrap();
    assert!(logout.status.success());
    assert!(!home.join("state/muster/cloud/account.json").exists());
    assert!(home.join("state/muster/cloud/link.json").exists());
    let revoked = support::http()
        .get(stack.url("/v1/workspaces"))
        .bearer_auth(token)
        .send()
        .await
        .unwrap();
    assert_eq!(
        revoked.status(),
        401,
        "logout must revoke the account token"
    );
    std::fs::remove_dir_all(home).unwrap();
}
#[tokio::test]
async fn denied_login_leaves_no_account() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    let mut child = tokio::process::Command::new(std::env::var("MUSTER_CLI_BIN").unwrap())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .args([
            "cloud",
            "login",
            "--endpoint",
            &stack.cloud.base,
            "--no-browser",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let instruction = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response = support::http()
        .post(stack.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&DeviceApproveRequest {
            user_code: instruction.split_whitespace().last().unwrap().into(),
            decision: DeviceDecision::Deny,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(!output.status.success());
    assert!(!home.join("state/muster/cloud/account.json").exists());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("mca_"));
    std::fs::remove_dir_all(home).unwrap();
}

/// Runs the CLI binary for one machine HOME with a clean environment, the same
/// isolation every CLI test here uses.
async fn run_cli(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new(
        std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI"),
    )
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("HOME", home)
    .env("XDG_CONFIG_HOME", home.join("config"))
    .env("XDG_STATE_HOME", home.join("state"))
    .env("XDG_DATA_HOME", home.join("data"))
    .env("XDG_CACHE_HOME", home.join("cache"))
    .args(args)
    .output()
    .await
    .unwrap()
}

/// Completes a real device-flow login for one machine HOME, approved through
/// `alice`'s browser session (setup: the only approval channel the service
/// offers; the behaviors under test run below it).
async fn device_login(
    stack: &support::Stack,
    alice: &support::UserSession,
    home: &std::path::Path,
) {
    device_login_at(stack, alice, home, &stack.cloud.base).await;
}

/// Same, through an explicit endpoint (e.g. the receipts proxy).
async fn device_login_at(
    stack: &support::Stack,
    alice: &support::UserSession,
    home: &std::path::Path,
    endpoint: &str,
) {
    let mut child = tokio::process::Command::new(
        std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI"),
    )
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("HOME", home)
    .env("XDG_CONFIG_HOME", home.join("config"))
    .env("XDG_STATE_HOME", home.join("state"))
    .env("XDG_DATA_HOME", home.join("data"))
    .env("XDG_CACHE_HOME", home.join("cache"))
    .args(["cloud", "login", "--endpoint", endpoint, "--no-browser"])
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true)
    .spawn()
    .unwrap();
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    let instruction = tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("login must display approval instructions");
    let response = support::http()
        .post(stack.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&DeviceApproveRequest {
            user_code: instruction.split_whitespace().last().unwrap().into(),
            decision: DeviceDecision::Approve,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let output = tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Creates a workspace through the service API (setup, not the behavior under test).
async fn create_workspace(
    stack: &support::Stack,
    alice: &support::UserSession,
    name: &str,
) -> muster_cloud::protocol::Workspace {
    let created = support::http()
        .post(stack.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    created.json().await.unwrap()
}

/// A machine with its own scaffold joins a published Cloud fleet by naming the
/// fleet's exact state hash (`--expect-state`, read over the public CLI from a
/// trusted machine). A foreign hash must be refused with the scaffold and the
/// Cloud link intact — the join never replaces local files unconfirmed.
#[tokio::test]
async fn expected_state_joins_remote_fleet_and_foreign_hash_is_refused() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let workspace = create_workspace(&stack, &alice, "Join by state").await;

    // Machine A bootstraps the workspace fleet and publishes it.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    let link = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "link",
            "--workspace",
            &workspace.id,
        ],
    )
    .await;
    assert!(
        link.status.success(),
        "{}",
        String::from_utf8_lossy(&link.stdout)
    );
    let push = run_cli(&home, &["--format", "json", "sync", "--push"]).await;
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stdout)
    );
    let show = run_cli(&home, &["--format", "json", "trust", "show"]).await;
    assert!(
        show.status.success(),
        "{}",
        String::from_utf8_lossy(&show.stdout)
    );
    let shown: serde_json::Value = serde_json::from_slice(&show.stdout).unwrap();
    eprintln!("DIAG trust show: {}", String::from_utf8_lossy(&show.stdout));
    let state_hash = shown["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .expect("a trusted machine exposes the hash of the state it accepted")
        .to_string();

    // Machine B has its own scaffold and links to the same workspace.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login(&stack, &alice, &second).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    let link_b = run_cli(
        &second,
        &[
            "--format",
            "json",
            "cloud",
            "link",
            "--workspace",
            &workspace.id,
        ],
    )
    .await;
    assert!(
        link_b.status.success(),
        "{}",
        String::from_utf8_lossy(&link_b.stdout)
    );

    // A foreign hash is refused: the scaffold and the Cloud link survive.
    let foreign = format!("SHA256:{}", "0".repeat(64));
    let refused = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-state",
            &foreign,
        ],
    )
    .await;
    assert!(
        !refused.status.success(),
        "a foreign --expect-state must be refused: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let scaffold = std::fs::read_to_string(second.join("config/muster/muster.toml")).unwrap();
    assert!(
        scaffold.contains("[machines.second]"),
        "the refusal must keep the local scaffold: {scaffold}"
    );
    assert!(second.join("state/muster/cloud/link.json").exists());

    // The realistic first-use journey: a pull without any confirmation fails
    // closed and leaves no pin behind — the next pull must still be able to
    // name the remote state it expects.
    let unconfirmed = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        !unconfirmed.status.success(),
        "first pull must fail closed without --expect-state: {}",
        String::from_utf8_lossy(&unconfirmed.stdout)
    );
    let scaffold = std::fs::read_to_string(second.join("config/muster/muster.toml")).unwrap();
    assert!(
        scaffold.contains("[machines.second]"),
        "the failed pull must keep the local scaffold: {scaffold}"
    );

    // The correct hash (out-of-band by contract; here over the public CLI)
    // joins: the scaffold is replaced by the remote fleet manifest.
    let pull = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-state",
            &state_hash,
        ],
    )
    .await;
    assert!(
        pull.status.success(),
        "{}",
        String::from_utf8_lossy(&pull.stdout)
    );
    let joined: serde_json::Value = serde_json::from_slice(&pull.stdout).unwrap();
    assert_eq!(joined["result"]["joined"], true, "{joined}");
    let joined_manifest =
        std::fs::read_to_string(second.join("config/muster/muster.toml")).unwrap();
    assert!(
        joined_manifest.contains("[machines.laptop]"),
        "pull must replace the scaffold with the fleet manifest: {joined_manifest}"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// A machine that already accepted a Cloud fleet keeps its antirollback pin
/// when the remote serves an unrelated history: a matching `--expect-signer`
/// alone cannot erase the pin, because an old-epoch signer must never
/// authorize a rewritten history. Rejoining another fleet on purpose still
/// works by naming that fleet's exact state hash.
#[tokio::test]
async fn unrelated_remote_history_keeps_pin_without_expected_state() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Fleet one").await;
    let ws2 = create_workspace(&stack, &alice, "Fleet two").await;

    // Machine A bootstraps fleet one and publishes it.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    let link_a = run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id],
    )
    .await;
    assert!(
        link_a.status.success(),
        "{}",
        String::from_utf8_lossy(&link_a.stdout)
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();
    let h1 = show_a["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine B joins fleet one: it now holds a real antirollback pin.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login(&stack, &alice, &second).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    let link_b = run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id],
    )
    .await;
    assert!(
        link_b.status.success(),
        "{}",
        String::from_utf8_lossy(&link_b.stdout)
    );
    let join = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        join.status.success(),
        "{}",
        String::from_utf8_lossy(&join.stdout)
    );
    let b_show: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(b_show["result"]["trust_pin"]["payload_hash"], h1);

    // Fleet two is a DIFFERENT fleet with its own unrelated history (machine
    // C bootstraps it with its own key). From B's point of view the remote
    // now serves an unrelated history whose signer it does not pin — the
    // same code path a rewritten same-signer history would take, and the
    // hardening below is signer-agnostic by construction.
    let third = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&third).unwrap();
    device_login(&stack, &alice, &third).await;
    assert!(run_cli(&third, &["--yes", "init", "--machine", "laptop2"])
        .await
        .status
        .success());
    let link_c = run_cli(
        &third,
        &["--format", "json", "cloud", "link", "--workspace", &ws2.id],
    )
    .await;
    assert!(
        link_c.status.success(),
        "{}",
        String::from_utf8_lossy(&link_c.stdout)
    );
    assert!(run_cli(&third, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_c: serde_json::Value = serde_json::from_slice(
        &run_cli(&third, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let h2 = show_c["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();
    let signer2 = show_c["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(h1, h2, "both fleets must be distinct states");

    // B moves to fleet two and pulls its unrelated history with the signer
    // expectation only: this must NOT erase the fleet-one pin.
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    let link_b2 = run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws2.id],
    )
    .await;
    assert!(
        link_b2.status.success(),
        "{}",
        String::from_utf8_lossy(&link_b2.stdout)
    );
    let pulled = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer2,
        ],
    )
    .await;
    assert!(
        !pulled.status.success(),
        "a matching --expect-signer alone must not replace the antirollback pin: {}",
        String::from_utf8_lossy(&pulled.stdout)
    );
    let b_show2: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(
        b_show2["result"]["trust_pin"]["payload_hash"], h1,
        "the fleet-one pin must survive the refused pull"
    );
    assert!(second.join("state/muster/cloud/link.json").exists());

    // Naming fleet two's exact state hash joins it on purpose.
    let rejoined = run_cli(
        &second,
        &["--format", "json", "sync", "--pull", "--expect-state", &h2],
    )
    .await;
    assert!(
        rejoined.status.success(),
        "{}",
        String::from_utf8_lossy(&rejoined.stdout)
    );
    let joined: serde_json::Value = serde_json::from_slice(&rejoined.stdout).unwrap();
    assert_eq!(joined["result"]["joined"], true, "{joined}");
    let b_show3: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(
        b_show3["result"]["trust_pin"]["payload_hash"], h2,
        "the deliberate rejoin must pin fleet two"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
    std::fs::remove_dir_all(third).unwrap();
}

/// A machine that legitimately SIGNED its own fleet after genesis (here: the
/// author re-signs after a local edit, epoch 2 with a `prev` chain) holds an
/// antirollback pin, not a scaffold: an unrelated history must not replace it
/// on a matching signer name alone — only its exact state hash confirms the
/// move deliberately.
#[tokio::test]
async fn own_signed_fleet_refuses_unrelated_history_without_expected_state() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Author fleet").await;
    let ws2 = create_workspace(&stack, &alice, "Other fleet").await;

    // Machine A bootstraps and publishes fleet one.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let push2 = run_cli(&home, &["--format", "json", "sync", "--push"]).await;
    assert!(
        push2.status.success(),
        "{} {}",
        String::from_utf8_lossy(&push2.stdout),
        String::from_utf8_lossy(&push2.stderr)
    );

    // A legitimate local edit, re-signed by the author: the fleet state is
    // now epoch ≥ 2 with a `prev` chain, still signed by A's own key.
    let manifest_path = home.join("config/muster/muster.toml");
    let edited = std::fs::read_to_string(&manifest_path).unwrap()
        + "[secrets.\"env://MUSTER_TEST_DRIFT\"]\nallow = []\n";
    std::fs::write(&manifest_path, edited).unwrap();
    let resign = run_cli(&home, &["--yes", "trust", "resign"]).await;
    assert!(
        resign.status.success(),
        "{} {}",
        String::from_utf8_lossy(&resign.stdout),
        String::from_utf8_lossy(&resign.stderr)
    );
    let show: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert!(
        show["result"]["fleet_state"]["prev"].is_string(),
        "the resigned fleet state must carry the prev chain: {show}"
    );
    let h1 = show["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine C: an unrelated fleet with its own git history.
    let third = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&third).unwrap();
    device_login(&stack, &alice, &third).await;
    assert!(run_cli(&third, &["--yes", "init", "--machine", "laptop3"])
        .await
        .status
        .success());
    assert!(run_cli(
        &third,
        &["--format", "json", "cloud", "link", "--workspace", &ws2.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&third, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_c: serde_json::Value = serde_json::from_slice(
        &run_cli(&third, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer_c = show_c["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();
    let h3 = show_c["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ne!(h1, h3);

    // Pulling that unrelated history with the signer name alone is refused:
    // A's pin is over a fleet it signed and accepted — the antirollback
    // boundary — not over a scaffold.
    assert!(run_cli(&home, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    let pulled = run_cli(
        &home,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", third.join("config/muster").display()),
            "--expect-signer",
            &signer_c,
        ],
    )
    .await;
    assert!(
        !pulled.status.success(),
        "signer-only must not replace the pin over a fleet this machine signed: {}",
        String::from_utf8_lossy(&pulled.stdout)
    );
    let show2: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(
        show2["result"]["trust_pin"]["payload_hash"], h1,
        "the author's pin must survive the refused pull"
    );
    let kept = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(
        kept.contains("[secrets.\"env://MUSTER_TEST_DRIFT\"]"),
        "HEAD must be restored: {kept}"
    );

    // Naming the other fleet's exact state hash joins it deliberately.
    let joined = run_cli(
        &home,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", third.join("config/muster").display()),
            "--expect-state",
            &h3,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );
    let result: serde_json::Value = serde_json::from_slice(&joined.stdout).unwrap();
    assert_eq!(result["result"]["joined"], true, "{result}");

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(third).unwrap();
}

/// A refused `--remote` join with a wrong `--expect-state` restores the full
/// pre-pull state: HEAD and the antirollback pin. The pin is dropped for the
/// verification, but a failed verification must never leave the machine
/// without its boundary.
#[tokio::test]
async fn remote_join_with_wrong_expected_state_preserves_pin() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Pinned fleet").await;
    let ws2 = create_workspace(&stack, &alice, "Remote fleet").await;

    // Machine A bootstraps fleet one.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // Machine B joins fleet one over Cloud: it holds a real pin.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login(&stack, &alice, &second).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let signer = {
        let show_a: serde_json::Value = serde_json::from_slice(
            &run_cli(&home, &["--format", "json", "trust", "show"])
                .await
                .stdout,
        )
        .unwrap();
        show_a["result"]["fleet_state"]["signer"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let joined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );
    let show_b: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let h1 = show_b["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine C: an unrelated fleet served to B over a plain git remote.
    let third = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&third).unwrap();
    device_login(&stack, &alice, &third).await;
    assert!(run_cli(&third, &["--yes", "init", "--machine", "laptop3"])
        .await
        .status
        .success());
    assert!(run_cli(
        &third,
        &["--format", "json", "cloud", "link", "--workspace", &ws2.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&third, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_c: serde_json::Value = serde_json::from_slice(
        &run_cli(&third, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let h3 = show_c["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_string();

    // A wrong --expect-state must be refused AND keep B's pin: the drop
    // happened for the verification, so the failure must restore it.
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    let foreign = format!("SHA256:{}", "f".repeat(64));
    let refused = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", third.join("config/muster").display()),
            "--expect-state",
            &foreign,
        ],
    )
    .await;
    assert!(
        !refused.status.success(),
        "a wrong --expect-state must be refused: {}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let show_b2: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(
        show_b2["result"]["trust_pin"]["payload_hash"], h1,
        "the failed verification must restore the pin: {show_b2}"
    );
    let kept = std::fs::read_to_string(second.join("config/muster/muster.toml")).unwrap();
    assert!(
        kept.contains("[machines.laptop]"),
        "HEAD must be restored to fleet one: {kept}"
    );

    // The exact hash joins deliberately.
    let rejoined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", third.join("config/muster").display()),
            "--expect-state",
            &h3,
        ],
    )
    .await;
    assert!(
        rejoined.status.success(),
        "{}",
        String::from_utf8_lossy(&rejoined.stdout)
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
    std::fs::remove_dir_all(third).unwrap();
}

/// Runs the CLI binary with a value on stdin (for `secret add --value-stdin`).
async fn run_cli_stdin(
    home: &std::path::Path,
    args: &[&str],
    value: &[u8],
) -> std::process::Output {
    let mut child = tokio::process::Command::new(
        std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI"),
    )
    .env_clear()
    .env("PATH", std::env::var_os("PATH").unwrap_or_default())
    .env("HOME", home)
    .env("XDG_CONFIG_HOME", home.join("config"))
    .env("XDG_STATE_HOME", home.join("state"))
    .env("XDG_DATA_HOME", home.join("data"))
    .env("XDG_CACHE_HOME", home.join("cache"))
    .args(args)
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(value).await.unwrap();
        stdin.shutdown().await.unwrap();
    }
    child.wait_with_output().await.unwrap()
}

/// Enrollment over the Cloud transport: the second machine joins the
/// transport (device approved by the owner's session — authorization for the
/// pipe, never for governance), pins the fleet, and CANNOT publish fleet
/// content before enrollment. Its signed enrollment request is published to
/// the fleet, approved LOCALLY by the author machine with its own signing
/// key (the Cloud session never signs governance), and only then the
/// enrolled machine publishes content that the author receives.
#[tokio::test]
async fn enrolled_second_machine_publishes_after_local_signed_approval() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Enrollment fleet").await;

    // Machine A bootstraps fleet one and publishes it.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine B links the transport and pins fleet one.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login(&stack, &alice, &second).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let joined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );

    // BEFORE enrollment: B's signing key is not a fleet writer, so a content
    // change is refused — the Cloud link grants no signing authority.
    let unauthorized = run_cli_stdin(
        &second,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"too-early",
    )
    .await;
    assert!(
        !unauthorized.status.success(),
        "an unenrolled machine must not sign fleet state: {}",
        String::from_utf8_lossy(&unauthorized.stdout)
    );

    // B proposes its enrollment (signed request): the CLI submits it as an
    // OPEN enrollment proposal on the linked workspace — the only channel a
    // non-member can publish through. A full bundle push stays forbidden.
    let enroll = run_cli(&second, &["--format", "json", "enroll", "--name", "second"]).await;
    assert!(
        enroll.status.success(),
        "{} {}",
        String::from_utf8_lossy(&enroll.stdout),
        String::from_utf8_lossy(&enroll.stderr)
    );
    let enrolled: serde_json::Value = serde_json::from_slice(&enroll.stdout).unwrap();
    assert!(
        enrolled["result"]["proposal"].is_string(),
        "enroll must hand the request to the transport as a proposal: {enrolled}"
    );
    let still_forbidden = run_cli(&second, &["--format", "json", "sync", "--push"]).await;
    assert!(
        !still_forbidden.status.success(),
        "an unenrolled machine must not publish bundles: {}",
        String::from_utf8_lossy(&still_forbidden.stdout)
    );

    // A receives the request and approves LOCALLY with its own signing key;
    // the Cloud session never signs governance.
    assert!(run_cli(&home, &["--format", "json", "sync", "--pull"])
        .await
        .status
        .success());
    let approve = run_cli(&home, &["--yes", "approve", "second"]).await;
    assert!(
        approve.status.success(),
        "{} {}",
        String::from_utf8_lossy(&approve.stdout),
        String::from_utf8_lossy(&approve.stderr)
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // B receives the approval: the fleet state advanced, still signed by the
    // author's key (never by anything derived from a Cloud session), and B
    // is now a writer.
    let pull_v2 = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        pull_v2.status.success(),
        "{} {}",
        String::from_utf8_lossy(&pull_v2.stdout),
        String::from_utf8_lossy(&pull_v2.stderr)
    );
    let show_b: serde_json::Value = serde_json::from_slice(
        &run_cli(&second, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    assert_eq!(
        show_b["result"]["fleet_state"]["signer"],
        signer.as_str(),
        "governance stays signed by the author's local key"
    );
    assert!(
        show_b["result"]["fleet_state"]["epoch"]
            .as_u64()
            .unwrap_or(0)
            >= 2,
        "the approval must advance the fleet state: {show_b}"
    );
    // B's local unpublished sibling commit (the enrollment request) survived
    // the pull as a normal pull (no expect bypass needed beyond the join),
    // and the accepted remote approval is now part of B's history: siblings,
    // not one replacing the other; user data preserved.
    let git_log = tokio::process::Command::new("git")
        .current_dir(second.join("config/muster"))
        .args(["log", "--oneline"])
        .output()
        .await
        .unwrap();
    let history = String::from_utf8_lossy(&git_log.stdout);
    assert!(
        history.contains("enrollment request for second"),
        "the local unpublished enrollment request must survive the pull: {history}"
    );
    assert!(
        history.contains("approve machine second"),
        "the accepted remote approval must be in B's history: {history}"
    );

    // The enrolled machine publishes content, and A receives it.
    let add = run_cli_stdin(
        &second,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"post-enrollment-value",
    )
    .await;
    assert!(
        add.status.success(),
        "{} {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let content_push = run_cli(&second, &["--format", "json", "sync", "--push"]).await;
    assert!(
        content_push.status.success(),
        "{} {}",
        String::from_utf8_lossy(&content_push.stdout),
        String::from_utf8_lossy(&content_push.stderr)
    );
    let pull_back = run_cli(&home, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        pull_back.status.success(),
        "{} {}",
        String::from_utf8_lossy(&pull_back.stdout),
        String::from_utf8_lossy(&pull_back.stderr)
    );
    let show_a2: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let groups = show_a2["result"]["fleet_state"]["vault_groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert!(
        groups.iter().any(|g| g == "app"),
        "A must have received B's published vault group (secret path app/token): {show_a2}"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Reads a machine's Cloud link identity from its HOME (CLI-owned state).
fn cloud_device_id(home: &std::path::Path) -> String {
    let link: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/link.json")).unwrap(),
    )
    .unwrap();
    link["device"].as_str().unwrap().to_owned()
}

/// The newest receipt the service has accepted from a device, read through
/// the owner's session (`GET devices`), or `None`.
async fn device_last_receipt(
    stack: &support::Stack,
    alice: &support::UserSession,
    ws_id: &str,
    device_id: &str,
) -> Option<muster_cloud::protocol::Receipt> {
    let response = support::http()
        .get(stack.url(&format!("/v1/workspaces/{ws_id}/devices")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let devices: Vec<muster_cloud::protocol::Device> = response.json().await.unwrap();
    devices
        .iter()
        .find(|d| d.id == device_id)
        .and_then(|d| d.last_receipt.clone())
}

/// A qualifying global apply queues and delivers a signed receipt attesting
/// the exact accepted revision and epoch. Download-only joins, no-op pulls
/// and dry runs produce none; a later no-op never advances the sequence.
#[tokio::test]
async fn apply_receipt_attests_accepted_revision_and_skips_non_qualifying() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Receipt fleet").await;

    // Machine A bootstraps and publishes fleet one.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let bootstrap_push = run_cli(&home, &["--format", "json", "sync", "--push"]).await;
    assert!(
        bootstrap_push.status.success(),
        "{} {}",
        String::from_utf8_lossy(&bootstrap_push.stdout),
        String::from_utf8_lossy(&bootstrap_push.stderr)
    );

    // Machine B joins: a download-only join applies nothing and produces no
    // receipt.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login(&stack, &alice, &second).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();
    let joined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );
    let b_device = cloud_device_id(&second);
    assert!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .is_none(),
        "a download-only join must not produce a receipt"
    );

    // A publishes a real content change; B pulls and APPLIES it: the
    // receipt attests the exact accepted revision and the fleet epoch.
    let add = run_cli_stdin(
        &home,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"receipted-value",
    )
    .await;
    assert!(
        add.status.success(),
        "{} {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let push2 = run_cli(&home, &["--format", "json", "sync", "--push"]).await;
    assert!(
        push2.status.success(),
        "{} {}",
        String::from_utf8_lossy(&push2.stdout),
        String::from_utf8_lossy(&push2.stderr)
    );
    let show_a2: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let expected_epoch = show_a2["result"]["fleet_state"]["epoch"].as_u64().unwrap();
    let expected_revision = show_a2["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap();
    let _ = expected_revision; // revision is asserted from B's accepted.json below

    let apply_pull = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        apply_pull.status.success(),
        "{}",
        String::from_utf8_lossy(&apply_pull.stdout)
    );
    let applied: serde_json::Value = serde_json::from_slice(&apply_pull.stdout).unwrap();
    assert_eq!(applied["result"]["applied"], true, "{applied}");

    let accepted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(second.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    let accepted_revision = accepted["revision"].as_str().unwrap().to_owned();
    // The receipt was queued during the applying pull; the NEXT invocation
    // drains it early in its transport (before that pull can fail).
    let drained = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        drained.status.success(),
        "{}",
        String::from_utf8_lossy(&drained.stdout)
    );
    let receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device).await;
    let receipt = receipt.expect("the next invocation must deliver the queued receipt");
    assert_eq!(receipt.revision, accepted_revision);
    assert_eq!(receipt.epoch, expected_epoch);
    assert_eq!(receipt.sequence, 1);
    assert_eq!(
        receipt.outcome,
        muster_cloud::protocol::ReceiptOutcome::Applied
    );

    // A no-op pull applies nothing and must not advance the sequence.
    assert!(run_cli(&second, &["--format", "json", "sync", "--pull"])
        .await
        .status
        .success());
    let again = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(again.sequence, 1, "a no-op pull must not produce a receipt");

    // A dry run performs no transport and no apply: still no new receipt.
    assert!(run_cli(
        &second,
        &["--format", "json", "--dry-run", "sync", "--pull"]
    )
    .await
    .status
    .success());
    let after_dry = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(
        after_dry.sequence, 1,
        "a dry run must not produce a receipt"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Proxy in front of the service with injectable wire faults for receipts.
#[derive(Clone)]
struct ReceiptProxy {
    upstream: String,
    client: reqwest::Client,
    /// Forward the POST, let the service commit it, then drop the response.
    drop_receipts_response_once: Arc<std::sync::atomic::AtomicBool>,
    /// Refuse receipts uploads at the proxy (transport-level failure).
    fail_receipts: Arc<std::sync::atomic::AtomicBool>,
    /// Refuse `GET .../sync` (a later transport failure after delivery).
    drop_sync: Arc<std::sync::atomic::AtomicBool>,
    /// Refuse `GET .../devices` (remote sequence unavailable on link).
    fail_devices_get: Arc<std::sync::atomic::AtomicBool>,
    /// When `Some(rev)`, replace `x-muster-revision` on `GET .../sync` (empty omits the header).
    override_sync_revision: Arc<std::sync::Mutex<Option<String>>>,
    /// Render the `applied_at` of successful receipt acks as the same instant
    /// in a different textual form (trailing zeros trimmed, numeric offset).
    reformat_receipt_ack: Arc<std::sync::atomic::AtomicBool>,
    /// How many acks were reformatted: proves the fault was injected.
    reformatted_acks: Arc<std::sync::atomic::AtomicUsize>,
}

/// A running [`ReceiptProxy`]: its base URL and fault controls.
struct ReceiptProxyHandle {
    url: String,
    drop_receipts_response_once: Arc<std::sync::atomic::AtomicBool>,
    fail_receipts: Arc<std::sync::atomic::AtomicBool>,
    drop_sync: Arc<std::sync::atomic::AtomicBool>,
    fail_devices_get: Arc<std::sync::atomic::AtomicBool>,
    override_sync_revision: Arc<std::sync::Mutex<Option<String>>>,
    reformat_receipt_ack: Arc<std::sync::atomic::AtomicBool>,
    reformatted_acks: Arc<std::sync::atomic::AtomicUsize>,
    _task: tokio::task::JoinHandle<()>,
}

/// The same instant as an RFC 3339 `…Z` string, rendered at a different
/// fractional precision and with a numeric offset.
fn equivalent_instant_rendering(applied_at: &str) -> String {
    let utc = applied_at
        .strip_suffix('Z')
        .or_else(|| applied_at.strip_suffix("+00:00"))
        .expect("UTC RFC 3339 applied_at");
    let (whole, fraction) = utc.split_once('.').unwrap_or((utc, ""));
    let trimmed = fraction.trim_end_matches('0');
    if trimmed.is_empty() {
        format!("{whole}+00:00")
    } else if trimmed.len() < fraction.len() {
        format!("{whole}.{trimmed}+00:00")
    } else {
        format!("{whole}.{trimmed}000+00:00")
    }
}

async fn receipt_proxy_handler(
    axum::extract::State(proxy): axum::extract::State<ReceiptProxy>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use std::sync::atomic::Ordering;
    let path = uri.path();
    let is_receipt_post = method == axum::http::Method::POST && path.ends_with("/receipts");
    if is_receipt_post && proxy.fail_receipts.load(Ordering::Relaxed) {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "receipt transport down",
        )
            .into_response();
    }
    if method == axum::http::Method::GET
        && path.ends_with("/devices")
        && proxy.fail_devices_get.load(Ordering::Relaxed)
    {
        return (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "devices transport down",
        )
            .into_response();
    }
    if method == axum::http::Method::GET
        && path.ends_with("/sync")
        && proxy.drop_sync.load(Ordering::Relaxed)
    {
        return (axum::http::StatusCode::BAD_GATEWAY, "sync transport down").into_response();
    }
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("{}{}{query}", proxy.upstream, path);
    let mut request = proxy.client.request(method.clone(), &url);
    for (name, value) in &headers {
        if !matches!(
            name.as_str(),
            "host" | "connection" | "content-length" | "transfer-encoding"
        ) {
            request = request.header(name, value);
        }
    }
    let response = match request.body(body).send().await {
        Ok(response) => response,
        Err(e) => {
            return (
                axum::http::StatusCode::BAD_GATEWAY,
                axum::body::Body::from(format!("upstream: {e}")),
            )
                .into_response();
        }
    };
    let status = response.status();
    let response_headers = response.headers().clone();
    let mut bytes = response.bytes().await.expect("upstream body");
    // The response is dropped only AFTER the service processed the request,
    // modelling a lost response to an already-committed publication.
    if is_receipt_post
        && proxy
            .drop_receipts_response_once
            .swap(false, Ordering::Relaxed)
    {
        return (axum::http::StatusCode::BAD_GATEWAY, "response dropped").into_response();
    }
    if is_receipt_post && status.is_success() && proxy.reformat_receipt_ack.load(Ordering::Relaxed)
    {
        let mut ack: serde_json::Value =
            serde_json::from_slice(&bytes).expect("the receipt ack is JSON");
        let sent = ack["applied_at"]
            .as_str()
            .expect("the receipt ack carries applied_at")
            .to_owned();
        let rendered = equivalent_instant_rendering(&sent);
        eprintln!("receipt ack applied_at as served: {sent}; as delivered: {rendered}");
        ack["applied_at"] = serde_json::Value::String(rendered);
        bytes = serde_json::to_vec(&ack).expect("encode the ack").into();
        proxy.reformatted_acks.fetch_add(1, Ordering::Relaxed);
    }
    let override_rev = if method == axum::http::Method::GET && path.ends_with("/sync") {
        proxy.override_sync_revision.lock().unwrap().clone()
    } else {
        None
    };
    let mut out = axum::response::Response::builder().status(status);
    for (name, value) in &response_headers {
        // The body may have been rewritten: let hyper frame it.
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        if override_rev.is_some() && name.as_str().eq_ignore_ascii_case("x-muster-revision") {
            continue;
        }
        out = out.header(name, value);
    }
    if let Some(rev) = override_rev.filter(|r| !r.is_empty()) {
        out = out.header("x-muster-revision", rev);
    }
    out.body(axum::body::Body::from(bytes)).unwrap()
}

/// Starts the receipts proxy in front of `upstream`.
async fn start_receipt_proxy(upstream: String) -> ReceiptProxyHandle {
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    let state = ReceiptProxy {
        upstream,
        client: support::http(),
        drop_receipts_response_once: Arc::new(AtomicBool::new(false)),
        fail_receipts: Arc::new(AtomicBool::new(false)),
        drop_sync: Arc::new(AtomicBool::new(false)),
        fail_devices_get: Arc::new(AtomicBool::new(false)),
        override_sync_revision: Arc::new(std::sync::Mutex::new(None)),
        reformat_receipt_ack: Arc::new(AtomicBool::new(false)),
        reformatted_acks: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind receipts proxy");
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .fallback(receipt_proxy_handler)
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service()).await.ok();
    });
    ReceiptProxyHandle {
        url: format!("http://127.0.0.1:{port}"),
        drop_receipts_response_once: state.drop_receipts_response_once,
        fail_receipts: state.fail_receipts,
        drop_sync: state.drop_sync,
        fail_devices_get: state.fail_devices_get,
        override_sync_revision: state.override_sync_revision,
        reformat_receipt_ack: state.reformat_receipt_ack,
        reformatted_acks: state.reformatted_acks,
        _task: task,
    }
}

/// The signed receipt survives a dropped response: the service committed it,
/// the machine keeps it queued, and the next invocation replays the exact
/// same payload (same sequence, signature and applied_at) which the service
/// answers idempotently — no second row, no new sequence.
#[tokio::test]
async fn receipt_survives_dropped_response_and_replays_exactly() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Replay fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let proxy_url = proxy.url.clone();
    let drop_receipts_once = proxy.drop_receipts_response_once.clone();

    // Machine A bootstraps and publishes fleet one.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // Machine B links THROUGH the proxy (its whole transport goes through
    // it) and joins.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    {
        let mut child = tokio::process::Command::new(
            std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI"),
        )
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &second)
        .env("XDG_CONFIG_HOME", second.join("config"))
        .env("XDG_STATE_HOME", second.join("state"))
        .env("XDG_DATA_HOME", second.join("data"))
        .env("XDG_CACHE_HOME", second.join("cache"))
        .args(["cloud", "login", "--endpoint", &proxy_url, "--no-browser"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
        let instruction =
            tokio::time::timeout(std::time::Duration::from_secs(15), lines.next_line())
                .await
                .unwrap()
                .unwrap()
                .expect("login must display approval instructions");
        let response = support::http()
            .post(stack.url("/v1/device/approve"))
            .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
            .header("Origin", stack.origin())
            .header("X-CSRF-Token", &alice.csrf_token)
            .json(&DeviceApproveRequest {
                user_code: instruction.split_whitespace().last().unwrap().into(),
                decision: DeviceDecision::Approve,
            })
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let output =
            tokio::time::timeout(std::time::Duration::from_secs(20), child.wait_with_output())
                .await
                .unwrap()
                .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();
    let joined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );

    // A publishes a content change; B pulls and applies it: the receipt is
    // queued, and the drain's POST is forwarded (the service commits it) but
    // its response is dropped: the delivery "failed" while the receipt is
    // actually accepted.
    let add = run_cli_stdin(
        &home,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"replay-value",
    )
    .await;
    assert!(add.status.success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let apply_pull = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        apply_pull.status.success(),
        "{}",
        String::from_utf8_lossy(&apply_pull.stdout)
    );
    let applied: serde_json::Value = serde_json::from_slice(&apply_pull.stdout).unwrap();
    assert_eq!(applied["result"]["applied"], true, "{applied}");
    let accepted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(second.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    let accepted_revision = accepted["revision"].as_str().unwrap().to_owned();
    let b_device = cloud_device_id(&second);
    assert!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .is_none(),
        "the receipt is queued locally; the service has not seen it yet"
    );

    // The next invocation drains the queued receipt: the POST is forwarded
    // (the service commits it) and its response is dropped — the machine
    // keeps the receipt queued and visible.
    drop_receipts_once.store(true, std::sync::atomic::Ordering::Relaxed);
    let dropped_pull = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        dropped_pull.status.success(),
        "{} {}",
        String::from_utf8_lossy(&dropped_pull.stdout),
        String::from_utf8_lossy(&dropped_pull.stderr)
    );
    let first = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .expect("the service must have committed the receipt despite the dropped response");
    assert_eq!(first.revision, accepted_revision);
    assert_eq!(first.sequence, 1);

    // The queue still holds the receipt; the next invocation replays it
    // exactly and the service answers idempotently.
    let replay = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        replay.status.success(),
        "{} {}",
        String::from_utf8_lossy(&replay.stdout),
        String::from_utf8_lossy(&replay.stderr)
    );
    let second_receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .expect("receipt must remain after replay");
    assert_eq!(second_receipt.sequence, 1, "no new sequence on replay");
    assert_eq!(second_receipt.applied_at, first.applied_at);
    assert_eq!(
        second_receipt.received_at, first.received_at,
        "the replay must hit the same stored receipt (identical received_at)"
    );
    assert_eq!(second_receipt.signature, first.signature);

    // The queue drained.
    let queue: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(second.join("state/muster/cloud/receipts.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        queue["links"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["pending"].as_array().unwrap().len())
            .sum::<usize>(),
        0,
        "the replayed receipt must have been dequeued: {queue}"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// An apply whose receipt upload fails keeps the receipt queued while the
/// sync itself succeeds; the next invocation drains the queue even when the
/// later sync transport fails; the next real application continues the
/// sequence; and unlink/relink isolates counters per device (a new device
/// never inherits the old queue or counter, and the old device's receipt
/// stays intact).
#[tokio::test]
async fn offline_queue_drains_despite_later_sync_failure_and_isolates_links() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Queued fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let proxy_url = proxy.url.clone();
    let fail_receipts = proxy.fail_receipts.clone();
    let drop_sync = proxy.drop_sync.clone();

    // Machine A bootstraps and publishes fleet one.
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&home, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine B joins through the proxy and pins fleet one: its WHOLE
    // transport goes through the proxy, so the injected faults apply to it.
    let second = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&second).unwrap();
    device_login_at(&stack, &alice, &second, &proxy_url).await;
    assert!(run_cli(&second, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let joined = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{}",
        String::from_utf8_lossy(&joined.stdout)
    );

    // B enrolls and A approves LOCALLY: the approved fleet state re-encrypts
    // the vault for B, so later content changes materialize for it (and its
    // receipts attest real applications). The Cloud session approves only
    // the transport; governance is signed by A's local key.
    let enroll = run_cli(&second, &["--format", "json", "enroll", "--name", "second"]).await;
    assert!(
        enroll.status.success(),
        "{} {}",
        String::from_utf8_lossy(&enroll.stdout),
        String::from_utf8_lossy(&enroll.stderr)
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--pull"])
        .await
        .status
        .success());
    let approve = run_cli(&home, &["--yes", "approve", "second"]).await;
    assert!(
        approve.status.success(),
        "{} {}",
        String::from_utf8_lossy(&approve.stdout),
        String::from_utf8_lossy(&approve.stderr)
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let approved_apply = pull_json(&second).await;
    assert_eq!(
        approved_apply["result"]["applied"], true,
        "{approved_apply}"
    );
    // The receipt this apply freezes must describe THIS accepted revision.
    let first_applied_revision = accepted_revision_of(&second);

    // A publishes content; B applies it while the receipts upload is down:
    // the sync SUCCEEDS and the receipt stays queued (the service has not
    // seen it).
    let add = run_cli_stdin(
        &home,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/queued",
            "--value-stdin",
        ],
        b"queued-value",
    )
    .await;
    assert!(add.status.success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    fail_receipts.store(true, std::sync::atomic::Ordering::Relaxed);
    let offline = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        offline.status.success(),
        "the local apply must succeed while the receipt upload fails: {}",
        String::from_utf8_lossy(&offline.stdout)
    );
    // The pull is a no-op this time: the content was applied by the
    // previous pull (whose receipt is the one queued). The offline upload
    // must not turn a successful local state into a sync failure.
    let applied: serde_json::Value = serde_json::from_slice(&offline.stdout).unwrap();
    assert_eq!(applied["result"]["applied"], false, "{applied}");
    let b_device = cloud_device_id(&second);
    assert!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .is_none(),
        "the failed upload must not have reached the service"
    );
    let queue: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(second.join("state/muster/cloud/receipts.json")).unwrap(),
    )
    .unwrap();
    let pending_count = queue["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["pending"].as_array().unwrap().len())
        .sum::<usize>();
    assert_eq!(pending_count, 1, "the receipt must remain queued: {queue}");

    // The next invocation drains the queue FIRST (the receipt is delivered)
    // even though the later sync transport fails.
    fail_receipts.store(false, std::sync::atomic::Ordering::Relaxed);
    drop_sync.store(true, std::sync::atomic::Ordering::Relaxed);
    let drained_but_failed = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        !drained_but_failed.status.success(),
        "the dropped sync transport must fail this invocation: {} {}",
        String::from_utf8_lossy(&drained_but_failed.stdout),
        String::from_utf8_lossy(&drained_but_failed.stderr)
    );
    let receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device).await;
    let receipt = receipt.expect("the drained receipt must be on the service");
    assert_eq!(receipt.sequence, 1);
    assert_eq!(
        receipt.revision, first_applied_revision,
        "the receipt describes the apply it was frozen for"
    );
    assert_ne!(
        accepted_revision_of(&second),
        first_applied_revision,
        "a later pull accepted a newer revision; the queued receipt must not be relabeled with it"
    );

    // Continuity: the next real application allocates the next sequence.
    drop_sync.store(false, std::sync::atomic::Ordering::Relaxed);
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "# Shared instructions (v2)\n",
    )
    .unwrap();
    let add2 = run_cli_stdin(
        &home,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/queued2",
            "--value-stdin",
        ],
        b"queued-value-2",
    )
    .await;
    assert!(add2.status.success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let second_apply = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        second_apply.status.success(),
        "{} {}",
        String::from_utf8_lossy(&second_apply.stdout),
        String::from_utf8_lossy(&second_apply.stderr)
    );
    let second_applied_revision = accepted_revision_of(&second);
    // The second application's receipt is queued by that pull; the next
    // invocation drains it (same rule as every queue transition here).
    let drain2 = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        drain2.status.success(),
        "{}",
        String::from_utf8_lossy(&drain2.stdout)
    );
    let receipt2 = device_last_receipt(&stack, &alice, &ws1.id, &b_device).await;
    let receipt2 = receipt2.expect("the second application must deliver a receipt");
    assert_eq!(receipt2.sequence, 2, "the sequence must advance");
    assert_eq!(receipt2.revision, second_applied_revision);

    // Unlink/relink: the service re-links the same signing key to the SAME
    // device, so the durable counter survives and keeps advancing.
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    assert_eq!(
        high_waters(&second),
        vec![2],
        "unlink must preserve the durable high-water: {}",
        receipt_state(&second)
    );
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert_eq!(
        cloud_device_id(&second),
        b_device,
        "re-linking the same signing key returns the same device"
    );
    assert_eq!(
        high_waters(&second),
        vec![2],
        "relink must not reset the counter: {}",
        receipt_state(&second)
    );

    // The third application continues at sequence 3, not at 1.
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "# Shared instructions (v3)\n",
    )
    .unwrap();
    publish_secret(&home, "app/queued3", "queued-value-3").await;
    let third = pull_json(&second).await;
    assert_eq!(third["result"]["applied"], true, "{third}");
    let third_applied_revision = accepted_revision_of(&second);
    pull_json(&second).await;
    let receipt3 = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .expect("the third application must deliver a receipt");
    assert_eq!(receipt3.sequence, 3, "the counter survives relink");
    assert_eq!(receipt3.revision, third_applied_revision);

    // Lost local receipt history: the same device is re-linked and the
    // counter is recovered from the service's newest receipt, never guessed.
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    std::fs::remove_file(second.join("state/muster/cloud/receipts.json")).unwrap();
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert_eq!(cloud_device_id(&second), b_device);
    assert_eq!(
        high_waters(&second),
        vec![3],
        "the counter must be recovered from the service: {}",
        receipt_state(&second)
    );

    // Isolation: another workspace is another device; it neither inherits
    // the counter nor the queue, and the old device's receipts stay intact.
    let ws2 = create_workspace(&stack, &alice, "Other queued fleet").await;
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws2.id]
    )
    .await
    .status
    .success());
    let other_device = cloud_device_id(&second);
    assert_ne!(
        other_device, b_device,
        "another workspace is another device"
    );
    let mut waters = high_waters(&second);
    waters.sort_unstable();
    assert_eq!(
        waters,
        vec![0, 3],
        "the new link starts its own counter: {}",
        receipt_state(&second)
    );
    assert!(pending_receipts(&second).is_empty());
    assert!(
        device_last_receipt(&stack, &alice, &ws2.id, &other_device)
            .await
            .is_none(),
        "the other device must not inherit receipts"
    );
    let old_receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .expect("the old device's receipt must stay intact");
    assert_eq!(old_receipt.sequence, 3);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Durable high-water of every link in the machine's receipt state.
fn high_waters(home: &std::path::Path) -> Vec<u64> {
    receipt_state(home)["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|link| link["high_water"].as_u64().unwrap())
        .collect()
}

/// The accepted revision recorded by a machine's last verified pull.
fn accepted_revision_of(home: &std::path::Path) -> String {
    let accepted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    accepted["revision"].as_str().unwrap().to_owned()
}

/// A fresh home directory for one machine.
fn new_home() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    home
}

/// Machine A (owner) bootstraps and publishes workspace `ws_id`; machine B
/// logs in through `b_endpoint`, links, and joins download-only. Returns the
/// two homes `(a, b)`.
async fn bootstrap_and_join(
    stack: &support::Stack,
    alice: &support::UserSession,
    ws_id: &str,
    b_endpoint: &str,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let a = new_home();
    device_login(stack, alice, &a).await;
    assert!(run_cli(&a, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &a,
        &["--format", "json", "cloud", "link", "--workspace", ws_id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&a, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    let b = new_home();
    device_login_at(stack, alice, &b, b_endpoint).await;
    assert!(run_cli(&b, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &b,
        &["--format", "json", "cloud", "link", "--workspace", ws_id]
    )
    .await
    .status
    .success());
    let show: serde_json::Value = serde_json::from_slice(
        &run_cli(&a, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_owned();
    let joined = run_cli(
        &b,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ],
    )
    .await;
    assert!(
        joined.status.success(),
        "{} {}",
        String::from_utf8_lossy(&joined.stdout),
        String::from_utf8_lossy(&joined.stderr)
    );
    (a, b)
}

/// Adds a secret on machine `home` and publishes the resulting fleet change.
async fn publish_secret(home: &std::path::Path, name: &str, value: &str) {
    let add = run_cli_stdin(
        home,
        &["--format", "json", "secret", "add", name, "--value-stdin"],
        value.as_bytes(),
    )
    .await;
    assert!(
        add.status.success(),
        "{} {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let push = run_cli(home, &["--format", "json", "sync", "--push"]).await;
    assert!(
        push.status.success(),
        "{} {}",
        String::from_utf8_lossy(&push.stdout),
        String::from_utf8_lossy(&push.stderr)
    );
}

/// `muster sync --pull` as JSON: asserts success and returns the envelope.
async fn pull_json(home: &std::path::Path) -> serde_json::Value {
    let pull = run_cli(home, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        pull.status.success(),
        "{} {}",
        String::from_utf8_lossy(&pull.stdout),
        String::from_utf8_lossy(&pull.stderr)
    );
    serde_json::from_slice(&pull.stdout).unwrap()
}

fn has_diagnostic(envelope: &serde_json::Value, code: &str) -> bool {
    envelope["diagnostics"]
        .as_array()
        .is_some_and(|all| all.iter().any(|d| d["code"] == code))
}

/// The machine's durable receipt state (CLI-owned `receipts.json`).
fn receipt_state(home: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/receipts.json")).unwrap(),
    )
    .unwrap()
}

/// Receipts still queued for delivery, across every link.
fn pending_receipts(home: &std::path::Path) -> Vec<serde_json::Value> {
    receipt_state(home)["links"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|link| link["pending"].as_array().unwrap().clone())
        .collect()
}

/// The service renders a replayed receipt's `applied_at` at another precision
/// than the machine froze it with (trailing zeros trimmed). That is the same
/// receipt: the delivery must be acknowledged and dequeued. The fault is
/// injected on the wire so the test does not depend on the clock happening
/// to produce a trailing zero.
#[tokio::test]
async fn receipt_ack_rendered_at_another_precision_still_dequeues() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Precision fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;

    publish_secret(&home, "app/token", "precision-value").await;
    let applied = pull_json(&second).await;
    assert_eq!(applied["result"]["applied"], true, "{applied}");
    let applied_revision = accepted_revision_of(&second);
    assert_eq!(pending_receipts(&second).len(), 1);

    proxy
        .reformat_receipt_ack
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let drained = pull_json(&second).await;
    assert!(
        proxy
            .reformatted_acks
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 1,
        "the proxy must have re-rendered at least one ack"
    );
    assert!(
        !has_diagnostic(&drained, "receipt_not_accepted"),
        "an ack for the same instant must be accepted: {drained}"
    );
    assert!(
        has_diagnostic(&drained, "receipt_delivered"),
        "the delivery must be reported: {drained}"
    );
    assert_eq!(
        pending_receipts(&second).len(),
        0,
        "the acknowledged receipt must leave the queue"
    );
    let device = cloud_device_id(&second);
    let receipt = device_last_receipt(&stack, &alice, &ws1.id, &device)
        .await
        .expect("the service must hold the receipt");
    assert_eq!(receipt.revision, applied_revision);
    assert_eq!(receipt.sequence, 1);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Asserts that `second` runs a qualifying `sync --pull` (`applied = true`)
/// whose receipt is skipped because of unpublished local configuration, with
/// nothing queued or delivered.
async fn assert_pull_skips_unpublished_config(
    stack: &support::Stack,
    alice: &support::UserSession,
    ws_id: &str,
    second: &std::path::Path,
) {
    let envelope = pull_json(second).await;
    assert_eq!(envelope["result"]["applied"], true, "{envelope}");
    let skip = envelope["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .unwrap_or_else(|| panic!("the skip must be visible: {envelope}"));
    assert!(
        skip["message"]
            .as_str()
            .unwrap()
            .contains("differs from the accepted Cloud revision"),
        "the skip must name the unpublished configuration: {skip}"
    );

    let device = cloud_device_id(second);
    assert!(
        device_last_receipt(stack, alice, ws_id, &device)
            .await
            .is_none(),
        "no receipt may attest an apply that included unpublished local configuration"
    );
    assert!(
        pending_receipts(second).is_empty(),
        "nothing may be queued for this apply: {}",
        receipt_state(second)
    );
    let drain = pull_json(second).await;
    assert!(!has_diagnostic(&drain, "receipt_delivered"), "{drain}");
    assert!(
        device_last_receipt(stack, alice, ws_id, &device)
            .await
            .is_none(),
        "no receipt may reach the service later either"
    );
}

/// A machine whose configuration carries something the accepted revision does
/// not contain must not attest that revision: `mutate` introduces one kind of
/// unpublished local configuration in machine B's config repository, B then
/// pulls and applies fleet content, and the skip must be honest and visible
/// while no receipt (queued or delivered) attests the accepted revision.
async fn assert_unpublished_local_change_is_not_attested(
    fleet: &str,
    mutate: impl FnOnce(&std::path::Path),
) {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, fleet).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &stack.cloud.base).await;
    publish_secret(&home, "app/token", "unpublished-config-value").await;

    mutate(&second.join("config/muster"));
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

fn git_in(config: &std::path::Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .current_dir(config)
        .args(args)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// A COMMITTED manifest change the fleet has not accepted (including a
/// bookkeeping field written under ANOTHER machine's table).
#[tokio::test]
async fn receipt_skips_committed_unpublished_manifest_change() {
    assert_unpublished_local_change_is_not_attested("Committed manifest fleet", |config| {
        let manifest = config.join("muster.toml");
        let edited = std::fs::read_to_string(&manifest).unwrap()
            + "\n[x-local]\nnote = \"unpublished local change\"\n";
        std::fs::write(&manifest, edited).unwrap();
        git_in(
            config,
            &["commit", "-aqm", "local unpublished manifest change"],
        );
    })
    .await;

    // Bookkeeping fields (`applied_profile` / `last_sync`) are ignored ONLY on
    // THIS machine's table (`[machines.second]`): a local commit that alters
    // another machine's `[machines.laptop]` table is an unpublished manifest
    // difference and must skip the receipt.
    assert_unpublished_local_change_is_not_attested("Other machine bookkeeping fleet", |config| {
        let manifest = config.join("muster.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        let replaced = text.replacen("applied_profile = \"", "applied_profile = \"foreign-", 1);
        assert_ne!(
            replaced, text,
            "manifest must contain laptop applied_profile"
        );
        std::fs::write(&manifest, replaced).unwrap();
        git_in(
            config,
            &["commit", "-aqm", "foreign machine bookkeeping change"],
        );
    })
    .await;
}

/// Uncommitted engine inputs in the working tree (`AGENTS.md`, `skills.lock`,
/// overlays, and subagent sources) — including gitignored untracked engine
/// inputs — must each prevent attesting the accepted revision.
#[tokio::test]
async fn receipt_skips_uncommitted_instruction_edit() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Instruction fleet").await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &stack.cloud.base).await;
    publish_secret(&home, "app/token", "unpublished-config-value").await;
    let config = second.join("config/muster");

    // 1. Uncommitted shared instructions edit (`AGENTS.md`).
    let instructions = config.join("AGENTS.md");
    let original_instructions = std::fs::read_to_string(&instructions).unwrap_or_default();
    std::fs::write(
        &instructions,
        format!("{original_instructions}\nUnpublished local instruction.\n"),
    )
    .unwrap();
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;
    std::fs::write(&instructions, &original_instructions).unwrap();

    // 2. Uncommitted `skills.lock`.
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    std::fs::write(
        config.join("skills.lock"),
        "[[skill]]\nname = \"local-skill\"\norigin = \"local\"\ncontent_hash = \"sha256:0000000000000000000000000000000000000000000000000000000000000000\"\nupdated_at = \"2026-10-14T12:00:00Z\"\n",
    )
    .unwrap();
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;
    std::fs::remove_file(config.join("skills.lock")).unwrap();

    // 3. Uncommitted instruction overlay (`AGENTS.claude.md`).
    std::fs::write(
        config.join("AGENTS.claude.md"),
        "Unpublished local overlay.\n",
    )
    .unwrap();
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;
    std::fs::remove_file(config.join("AGENTS.claude.md")).unwrap();

    // 4. Gitignored untracked engine input (`AGENTS.claude.md` in `.git/info/exclude`):
    // `engine::plan` reads it from disk regardless of git ignore rules, so
    // `config_matches_revision` must detect it even when ignored by git.
    let exclude = config.join(".git/info/exclude");
    let original_exclude = std::fs::read_to_string(&exclude).unwrap_or_default();
    std::fs::write(&exclude, format!("{original_exclude}\nAGENTS.claude.md\n")).unwrap();
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    std::fs::write(
        config.join("AGENTS.claude.md"),
        "Unpublished gitignored local overlay.\n",
    )
    .unwrap();
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;
    std::fs::remove_file(config.join("AGENTS.claude.md")).unwrap();
    std::fs::write(&exclude, &original_exclude).unwrap();

    // 5. Uncommitted subagent source (`subagents/reviewer.md`).
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    std::fs::create_dir_all(config.join("subagents")).unwrap();
    std::fs::write(
        config.join("subagents/reviewer.md"),
        "---\nname: reviewer\ndescription: unpublished\n---\nLocal subagent.\n",
    )
    .unwrap();
    assert_pull_skips_unpublished_config(&stack, &alice, &ws1.id, &second).await;
    std::fs::remove_dir_all(config.join("subagents")).unwrap();

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// A skill added to the index (staged, never published).
#[tokio::test]
async fn receipt_skips_staged_unpublished_skill() {
    assert_unpublished_local_change_is_not_attested("Staged skill fleet", |config| {
        let skill = config.join("skills/local-skill");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: local-skill\ndescription: unpublished\n---\nLocal only.\n",
        )
        .unwrap();
        git_in(config, &["add", "skills/local-skill/SKILL.md"]);
    })
    .await;
}

/// `x-muster-revision` must be a canonical 40/64-hex commit SHA matching the
/// verified fetched tip on 200 OK (and the current accepted revision on 204 No
/// Content). Any mismatch, non-canonical revspec (`"HEAD"`), or missing header
/// clears `accepted.json`, emits an honest diagnostic, and prevents receipt
/// generation.
#[tokio::test]
async fn sync_pull_rejects_mismatched_or_non_canonical_revision_header() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Header mismatch fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;
    let c1_revision = accepted_revision_of(&second);
    let b_device = cloud_device_id(&second);

    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "Shared fleet instruction.\n",
    )
    .unwrap();
    publish_secret(&home, "app/token1", "v1").await;
    let config = second.join("config/muster");

    // 1. 200 OK with non-canonical header `"HEAD"` + unpublished local commit:
    // must NOT launder `"HEAD"` into `accepted.json` or sign a receipt for `"HEAD"`.
    std::fs::write(config.join("AGENTS.claude.md"), "Unpublished overlay.\n").unwrap();
    git_in(&config, &["add", "AGENTS.claude.md"]);
    git_in(&config, &["commit", "-qm", "unpublished local commit"]);
    *proxy.override_sync_revision.lock().unwrap() = Some("HEAD".into());

    let pull1 = pull_json(&second).await;
    assert_eq!(pull1["result"]["applied"], true, "{pull1}");
    assert!(
        has_diagnostic(&pull1, "cloud_revision_mismatch"),
        "non-canonical header must emit cloud_revision_mismatch: {pull1}"
    );
    assert!(
        has_diagnostic(&pull1, "receipt_skipped"),
        "missing provenance must skip receipt: {pull1}"
    );
    assert!(
        !second.join("state/muster/cloud/accepted.json").exists(),
        "mismatched 200 header must clear accepted.json"
    );
    assert!(pending_receipts(&second).is_empty());

    // Drop only the extra unpublished commit on top of the fetched tip (`HEAD~1`
    // is now `c2`, the server head). Do NOT reset back to `c1_revision`:
    // because `accepted.json` was cleared by `pull1`, subsequent pulls must
    // omit `since` and receive 200 OK even though `HEAD` already equals `c2`.
    git_in(&config, &["reset", "--hard", "HEAD~1"]);

    // 2. 200 OK with a stale canonical commit SHA (`c1_revision` when `c2` is fetched).
    *proxy.override_sync_revision.lock().unwrap() = Some(c1_revision.clone());
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();

    let pull2 = pull_json(&second).await;
    assert_eq!(pull2["result"]["applied"], true, "{pull2}");
    assert!(
        has_diagnostic(&pull2, "cloud_revision_mismatch"),
        "stale commit header on 200 must emit cloud_revision_mismatch: {pull2}"
    );
    assert!(
        has_diagnostic(&pull2, "receipt_skipped"),
        "stale commit header must skip receipt: {pull2}"
    );
    assert!(!second.join("state/muster/cloud/accepted.json").exists());
    assert!(pending_receipts(&second).is_empty());

    // 3. 200 OK with omitted `x-muster-revision` header.
    *proxy.override_sync_revision.lock().unwrap() = Some(String::new());
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();

    let pull3 = pull_json(&second).await;
    assert_eq!(pull3["result"]["applied"], true, "{pull3}");
    assert!(
        has_diagnostic(&pull3, "cloud_revision_missing"),
        "missing header on 200 must emit cloud_revision_missing: {pull3}"
    );
    assert!(has_diagnostic(&pull3, "receipt_skipped"), "{pull3}");
    assert!(!second.join("state/muster/cloud/accepted.json").exists());
    assert!(pending_receipts(&second).is_empty());

    // 4. Self-heal after 200 with bad header (test b): without any manual
    // `git reset`, a normal pull sends no `since`, receives 200 OK, restores
    // `accepted.json`, and emits the receipt for the qualifying apply.
    *proxy.override_sync_revision.lock().unwrap() = None;
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let ok_pull = pull_json(&second).await;
    assert_eq!(ok_pull["result"]["applied"], true, "{ok_pull}");
    assert!(
        second.join("state/muster/cloud/accepted.json").exists(),
        "normal pull after a bad 200 header must restore accepted.json without manual reset: {ok_pull}"
    );
    let c2_revision = accepted_revision_of(&second);
    assert_ne!(c2_revision, c1_revision);
    assert!(has_diagnostic(&ok_pull, "receipt_queued"), "{ok_pull}");
    // Drain the legitimate receipt (sequence 1).
    let drained = pull_json(&second).await;
    assert!(has_diagnostic(&drained, "receipt_delivered"), "{drained}");
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1
    );

    // 5. 204 No Content whose `x-muster-revision` header (`c1_revision`) differs
    // from the sent `since` (`c2_revision`) (test c): emits `cloud_revision_mismatch`,
    // clears `accepted.json`, skips the receipt, and self-heals on the next pull.
    *proxy.override_sync_revision.lock().unwrap() = Some(c1_revision);
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let pull4 = pull_json(&second).await;
    assert_eq!(pull4["result"]["applied"], true, "{pull4}");
    assert!(
        has_diagnostic(&pull4, "cloud_revision_mismatch"),
        "mismatched 204 header must emit cloud_revision_mismatch: {pull4}"
    );
    assert!(has_diagnostic(&pull4, "receipt_skipped"), "{pull4}");
    assert!(
        !second.join("state/muster/cloud/accepted.json").exists(),
        "mismatched 204 header must clear accepted.json"
    );
    assert!(pending_receipts(&second).is_empty());
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1,
        "no new receipt may be produced after a 204 header mismatch"
    );

    // Next normal pull self-heals because `accepted.json` was cleared (sends no
    // `since` -> 200 OK -> restores `accepted.json` and emits sequence 2).
    *proxy.override_sync_revision.lock().unwrap() = None;
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let healed = pull_json(&second).await;
    assert_eq!(healed["result"]["applied"], true, "{healed}");
    assert_eq!(accepted_revision_of(&second), c2_revision);
    assert!(has_diagnostic(&healed, "receipt_queued"), "{healed}");
    let drained2 = pull_json(&second).await;
    assert!(has_diagnostic(&drained2, "receipt_delivered"), "{drained2}");
    let r2 = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(r2.sequence, 2);
    assert_eq!(r2.revision, c2_revision);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// On a converged machine (`HEAD == server head`), `muster cloud unlink` +
/// `muster cloud link` + `muster sync --pull` recreates `accepted.json` (by
/// sending no `since` when `accepted.json` is absent), emits a receipt on the
/// next qualifying apply, and allows `muster sync --push` of a new local commit
/// without a spurious 409 / conflict proposal.
#[tokio::test]
async fn converged_unlink_relink_pull_restores_provenance_receipts_and_push() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Unlink relink converged fleet").await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &stack.cloud.base).await;
    let b_device = cloud_device_id(&second);

    // Enroll and approve `second` so it is an authorized fleet writer.
    assert!(
        run_cli(&second, &["--format", "json", "enroll", "--name", "second"])
            .await
            .status
            .success()
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--pull"])
        .await
        .status
        .success());
    assert!(run_cli(&home, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "Shared fleet instruction.\n",
    )
    .unwrap();
    publish_secret(&home, "app/token", "v1").await;

    // Converge `second` and deliver receipt sequence 1.
    let first = pull_json(&second).await;
    assert_eq!(first["result"]["applied"], true, "{first}");
    let r1 = accepted_revision_of(&second);
    let drain1 = pull_json(&second).await;
    assert!(has_diagnostic(&drain1, "receipt_delivered"), "{drain1}");
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1
    );

    // Unlink and re-link `second` while it is already converged (`HEAD == r1`).
    let accepted_path = second.join("state/muster/cloud/accepted.json");
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    assert!(!accepted_path.exists(), "unlink must remove accepted.json");
    assert!(run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(!accepted_path.exists());

    // Pull on the converged machine must recreate `accepted.json` (since no
    // `accepted.json` existed, `pull` sends no `since` and verifies the 200 bundle).
    let relink_pull = pull_json(&second).await;
    assert!(
        accepted_path.exists(),
        "pull after unlink + link on a converged machine must recreate accepted.json: {relink_pull}"
    );
    assert_eq!(accepted_revision_of(&second), r1);

    // Next qualifying apply emits and delivers receipt sequence 2.
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let qualifying = pull_json(&second).await;
    assert_eq!(qualifying["result"]["applied"], true, "{qualifying}");
    assert!(
        has_diagnostic(&qualifying, "receipt_queued"),
        "{qualifying}"
    );
    let drain2 = pull_json(&second).await;
    assert!(has_diagnostic(&drain2, "receipt_delivered"), "{drain2}");
    let second_receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(second_receipt.sequence, 2);
    assert_eq!(second_receipt.revision, r1);

    // A new local commit on `second` pushes cleanly (base = `r1`, not `none`:
    // no 409 and no spurious conflict proposal).
    let add_on_second = run_cli_stdin(
        &second,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/from-second",
            "--value-stdin",
        ],
        b"from-second",
    )
    .await;
    assert!(
        add_on_second.status.success(),
        "{} {}",
        String::from_utf8_lossy(&add_on_second.stdout),
        String::from_utf8_lossy(&add_on_second.stderr)
    );
    let push_on_second = run_cli(&second, &["--format", "json", "sync", "--push"]).await;
    assert!(
        push_on_second.status.success(),
        "sync --push after unlink+link+pull must succeed without 409: {} {}",
        String::from_utf8_lossy(&push_on_second.stdout),
        String::from_utf8_lossy(&push_on_second.stderr)
    );
    let proposals_resp = support::http()
        .get(stack.url(&format!("/v1/workspaces/{}/proposals", ws1.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(proposals_resp.status(), 200);
    let proposals: Vec<muster_cloud::protocol::Proposal> = proposals_resp.json().await.unwrap();
    assert!(
        proposals
            .iter()
            .all(|p| p.kind != muster_cloud::protocol::ProposalKind::Conflict),
        "no spurious conflict proposal may be opened: {proposals:?}"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// After a single `200 OK` pull with a bad `x-muster-revision` header (where the
/// verified checkout already moved `HEAD` to the fetched tip and cleared
/// `accepted.json`), a subsequent normal pull restores `accepted.json` and
/// receipt provenance automatically without any manual `git reset`.
#[tokio::test]
async fn bad_200_revision_header_self_heals_on_next_pull_without_manual_reset() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Bad 200 self-heal fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;
    let c1_revision = accepted_revision_of(&second);
    let b_device = cloud_device_id(&second);

    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "Shared fleet instruction.\n",
    )
    .unwrap();
    publish_secret(&home, "app/token", "v1").await;

    // 1. One 200 OK with a bad header (`c1_revision` when `c2` is served):
    // checkout moves `HEAD` to `c2`, `accepted.json` is cleared, and the receipt
    // is skipped.
    *proxy.override_sync_revision.lock().unwrap() = Some(c1_revision.clone());
    let bad_pull = pull_json(&second).await;
    assert_eq!(bad_pull["result"]["applied"], true, "{bad_pull}");
    assert!(
        has_diagnostic(&bad_pull, "cloud_revision_mismatch"),
        "{bad_pull}"
    );
    assert!(has_diagnostic(&bad_pull, "receipt_skipped"), "{bad_pull}");
    assert!(!second.join("state/muster/cloud/accepted.json").exists());
    assert!(pending_receipts(&second).is_empty());

    // 2. Normal pull immediately afterwards (no manual `git reset`): because
    // `accepted.json` is absent, `pull` sends no `since`, receives the 200
    // bundle, and restores `accepted.json` to `c2`.
    *proxy.override_sync_revision.lock().unwrap() = None;
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let healed_pull = pull_json(&second).await;
    assert_eq!(healed_pull["result"]["applied"], true, "{healed_pull}");
    assert!(
        second.join("state/muster/cloud/accepted.json").exists(),
        "normal pull must restore accepted.json without manual reset: {healed_pull}"
    );
    let c2_revision = accepted_revision_of(&second);
    assert_ne!(c2_revision, c1_revision);
    assert!(
        has_diagnostic(&healed_pull, "receipt_queued"),
        "{healed_pull}"
    );

    let drained = pull_json(&second).await;
    assert!(has_diagnostic(&drained, "receipt_delivered"), "{drained}");
    let receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(receipt.sequence, 1);
    assert_eq!(receipt.revision, c2_revision);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// When a `204 No Content` response carries an `x-muster-revision` header that
/// differs from the `since` revision actually sent, `pull` clears
/// `accepted.json`, emits `cloud_revision_mismatch`, skips the receipt, and
/// self-heals on the next pull (which sends no `since` and re-verifies the 200
/// bundle).
#[tokio::test]
async fn mismatched_204_revision_header_emits_diagnostic_and_self_heals_on_next_pull() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "204 mismatch self-heal fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;
    let c1_revision = accepted_revision_of(&second);
    let b_device = cloud_device_id(&second);

    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "Shared fleet instruction.\n",
    )
    .unwrap();
    publish_secret(&home, "app/token", "v1").await;

    // Converge `second` at `c2` and deliver receipt sequence 1.
    let first = pull_json(&second).await;
    assert_eq!(first["result"]["applied"], true, "{first}");
    let c2_revision = accepted_revision_of(&second);
    assert_ne!(c2_revision, c1_revision);
    let drain1 = pull_json(&second).await;
    assert!(has_diagnostic(&drain1, "receipt_delivered"), "{drain1}");
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1
    );

    // 1. 204 whose `x-muster-revision` header (`c1_revision`) differs from the
    // sent `since` (`c2_revision`): clears `accepted.json` and emits
    // `cloud_revision_mismatch`.
    *proxy.override_sync_revision.lock().unwrap() = Some(c1_revision);
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let mismatch_pull = pull_json(&second).await;
    assert_eq!(mismatch_pull["result"]["applied"], true, "{mismatch_pull}");
    assert!(
        has_diagnostic(&mismatch_pull, "cloud_revision_mismatch"),
        "{mismatch_pull}"
    );
    assert!(
        has_diagnostic(&mismatch_pull, "receipt_skipped"),
        "{mismatch_pull}"
    );
    assert!(!second.join("state/muster/cloud/accepted.json").exists());
    assert!(pending_receipts(&second).is_empty());

    // 2. Next normal pull self-heals: sends no `since`, receives 200 OK,
    // restores `accepted.json` to `c2_revision`, and emits sequence 2.
    *proxy.override_sync_revision.lock().unwrap() = None;
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let healed_pull = pull_json(&second).await;
    assert_eq!(healed_pull["result"]["applied"], true, "{healed_pull}");
    assert_eq!(accepted_revision_of(&second), c2_revision);
    assert!(
        has_diagnostic(&healed_pull, "receipt_queued"),
        "{healed_pull}"
    );
    let drain2 = pull_json(&second).await;
    assert!(has_diagnostic(&drain2, "receipt_delivered"), "{drain2}");
    let r2 = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(r2.sequence, 2);
    assert_eq!(r2.revision, c2_revision);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Negative provenance boundaries (`accepted.json` missing, belonging to another
/// link, not covered by `trust.json` pin, or holding a divergent revision not in
/// `HEAD`) and sequence bounds (`pending` sequence folding and `i64::MAX`
/// overflow guard) each behave honestly through the public CLI without relying
/// on `since=<HEAD>` or unchecked 204 headers.
#[tokio::test]
async fn receipt_skips_broken_provenance_and_sequence_bounds() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Provenance bounds fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;

    // Enroll and approve `second` so it can sign local state transitions.
    assert!(
        run_cli(&second, &["--format", "json", "enroll", "--name", "second"])
            .await
            .status
            .success()
    );
    assert!(run_cli(&home, &["--format", "json", "sync", "--pull"])
        .await
        .status
        .success());
    assert!(run_cli(&home, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "Shared fleet instruction.\n",
    )
    .unwrap();
    publish_secret(&home, "app/token", "provenance-value").await;

    // Initial qualifying pull + drain establishes accepted.json and sequence 1.
    let first = pull_json(&second).await;
    assert_eq!(first["result"]["applied"], true, "{first}");
    let drained = pull_json(&second).await;
    assert!(has_diagnostic(&drained, "receipt_delivered"), "{drained}");
    let accepted_path = second.join("state/muster/cloud/accepted.json");
    let accepted_rev = accepted_revision_of(&second);
    let b_device = cloud_device_id(&second);

    // 1. `accepted.json` missing when `accepted_provenance` runs -> skipped
    // ("no accepted Cloud revision recorded"): when the 204 response omits
    // `x-muster-revision`, `pull` clears `accepted.json`, and `accepted_provenance`
    // skips the receipt because no accepted revision is recorded.
    *proxy.override_sync_revision.lock().unwrap() = Some(String::new());
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let missing_env = pull_json(&second).await;
    assert_eq!(missing_env["result"]["applied"], true, "{missing_env}");
    assert!(
        has_diagnostic(&missing_env, "cloud_revision_missing"),
        "{missing_env}"
    );
    let skip_msg = missing_env["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("no accepted Cloud revision recorded"),
        "expected missing accepted.json diagnostic, got: {missing_env}"
    );
    assert!(!accepted_path.exists());
    assert!(pending_receipts(&second).is_empty());
    // Self-heal `accepted.json` via a normal no-op pull (sends no `since` -> 200 OK).
    *proxy.override_sync_revision.lock().unwrap() = None;
    let healed_after_missing = pull_json(&second).await;
    assert_eq!(healed_after_missing["result"]["applied"], false);
    assert_eq!(accepted_revision_of(&second), accepted_rev);

    // 2. `accepted.json` belonging to another link -> skipped ("belongs to another link"):
    // `clear_accepted_if_same_link` preserves a foreign link's `accepted.json` when
    // a pull response cannot establish provenance for this link, and
    // `accepted_provenance` then skips the receipt because `accepted.json`
    // belongs to another link; a subsequent valid pull overwrites it with this
    // link's verified revision.
    std::fs::write(
        &accepted_path,
        serde_json::to_vec(&serde_json::json!({
            "workspace": "ws-foreign",
            "device": b_device,
            "revision": accepted_rev,
        }))
        .unwrap(),
    )
    .unwrap();
    *proxy.override_sync_revision.lock().unwrap() = Some(String::new());
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let other_link_env = pull_json(&second).await;
    assert_eq!(
        other_link_env["result"]["applied"], true,
        "{other_link_env}"
    );
    let skip_msg = other_link_env["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("belongs to another link"),
        "expected other-link diagnostic, got: {other_link_env}"
    );
    assert!(pending_receipts(&second).is_empty());
    // A normal pull with a valid header sends no `since` (because `accepted.json`
    // belongs to `ws-foreign`) and overwrites `accepted.json` with `ws1`'s revision.
    *proxy.override_sync_revision.lock().unwrap() = None;
    let healed_after_foreign = pull_json(&second).await;
    assert_eq!(healed_after_foreign["result"]["applied"], false);
    assert_eq!(accepted_revision_of(&second), accepted_rev);

    // 3. `accepted.json` revision not covered by `trust.json` pin -> skipped
    // ("not covered by this machine's pin"): `second` signs a new local epoch
    // (`muster trust checkpoint`, advancing `HEAD` and `trust.json` to the next
    // epoch) before pushing to Cloud. `HEAD` still contains `accepted_rev`, so
    // `pull` sends `since=<accepted_rev>`, the service (still at `accepted_rev`)
    // responds 204 with `x-muster-revision: <accepted_rev>`, and
    // `accepted_provenance` sees `accepted_rev` is behind this machine's
    // `trust.json` pin.
    assert!(run_cli(&second, &["--yes", "trust", "checkpoint"])
        .await
        .status
        .success());
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let uncovered_env = pull_json(&second).await;
    assert_eq!(uncovered_env["result"]["applied"], true, "{uncovered_env}");
    let skip_msg = uncovered_env["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("not covered by this machine's pin"),
        "expected uncovered pin diagnostic, got: {uncovered_env}"
    );
    assert!(pending_receipts(&second).is_empty());
    // Publish `second`'s new epoch commit so `second` and the service converge.
    assert!(run_cli(&second, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let next_rev = accepted_revision_of(&second);
    assert_ne!(next_rev, accepted_rev);

    // 4. `accepted.json` holding a divergent commit not contained in `HEAD`
    // (`refs/muster/pre-join`): `pull` refuses to send `since=<divergent_rev>`,
    // so the service serves the full 200 bundle (`next_rev`). Even if a proxy
    // rewrites the response header to `divergent_rev`, `pull` rejects it against
    // `fetched_tip` (`cloud_revision_mismatch`), clears `accepted.json`, and
    // skips the receipt; the next normal pull self-heals `accepted.json` to `next_rev`.
    let pre_join_out = std::process::Command::new("git")
        .current_dir(second.join("config/muster"))
        .args(["rev-parse", "--verify", "refs/muster/pre-join^{commit}"])
        .output()
        .unwrap();
    assert!(pre_join_out.status.success());
    let divergent_rev = String::from_utf8_lossy(&pre_join_out.stdout)
        .trim()
        .to_owned();
    std::fs::write(
        &accepted_path,
        serde_json::to_vec(&serde_json::json!({
            "workspace": ws1.id,
            "device": b_device,
            "revision": divergent_rev,
        }))
        .unwrap(),
    )
    .unwrap();
    *proxy.override_sync_revision.lock().unwrap() = Some(divergent_rev);
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let divergent_env = pull_json(&second).await;
    assert_eq!(divergent_env["result"]["applied"], true, "{divergent_env}");
    assert!(
        has_diagnostic(&divergent_env, "cloud_revision_mismatch"),
        "divergent accepted.json must force a full pull that rejects divergent header: {divergent_env}"
    );
    assert!(has_diagnostic(&divergent_env, "receipt_skipped"));
    assert!(!accepted_path.exists());
    assert!(pending_receipts(&second).is_empty());
    *proxy.override_sync_revision.lock().unwrap() = None;
    let healed_after_divergent = pull_json(&second).await;
    assert_eq!(healed_after_divergent["result"]["applied"], false);
    assert_eq!(accepted_revision_of(&second), next_rev);

    // 5. Sequence bounds (F2): when `high_water` is at `i64::MAX as u64`,
    // `freeze_applied_receipt` refuses to overflow and defers honestly.
    let receipts_path = second.join("state/muster/cloud/receipts.json");
    let valid_receipts = std::fs::read_to_string(&receipts_path).unwrap();
    let mut overflow_state: serde_json::Value = serde_json::from_str(&valid_receipts).unwrap();
    for link_entry in overflow_state["links"].as_array_mut().unwrap() {
        link_entry["high_water"] = serde_json::json!(i64::MAX as u64);
    }
    std::fs::write(
        &receipts_path,
        serde_json::to_vec_pretty(&overflow_state).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let overflow_env = pull_json(&second).await;
    assert_eq!(overflow_env["result"]["applied"], true, "{overflow_env}");
    let skip_msg = overflow_env["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("outside the accepted range; receipt deferred"),
        "expected sequence bound deferral diagnostic, got: {overflow_env}"
    );
    assert!(pending_receipts(&second).is_empty());

    // 6. Sequence folding over `pending` (F2): if `pending` holds sequence 2
    // while `high_water` is 0 (and receipt upload is offline so `drain` keeps
    // sequence 2 queued), the next qualifying apply allocates sequence 3.
    proxy
        .fail_receipts
        .store(true, std::sync::atomic::Ordering::Relaxed);
    std::fs::write(&receipts_path, &valid_receipts).unwrap();
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let queued_seq2 = pull_json(&second).await;
    assert!(
        has_diagnostic(&queued_seq2, "receipt_queued"),
        "{queued_seq2}"
    );
    let mut desynced_hw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipts_path).unwrap()).unwrap();
    let ws1_link = desynced_hw["links"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|l| l["workspace"] == ws1.id)
        .unwrap();
    assert_eq!(ws1_link["pending"][0]["sequence"], 2);
    ws1_link["high_water"] = serde_json::json!(0);
    std::fs::write(
        &receipts_path,
        serde_json::to_vec_pretty(&desynced_hw).unwrap(),
    )
    .unwrap();
    std::fs::remove_file(second.join(".claude/CLAUDE.md")).unwrap();
    let queued_seq3 = pull_json(&second).await;
    assert!(
        has_diagnostic(&queued_seq3, "receipt_queued"),
        "{queued_seq3}"
    );
    let pending = pending_receipts(&second);
    assert_eq!(pending.len(), 2, "{pending:?}");
    assert_eq!(pending[0]["sequence"], 2);
    assert_eq!(
        pending[1]["sequence"], 3,
        "sequence allocation must fold pending sequences: {pending:?}"
    );

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// Non-qualifying apply outcomes — project-scoped apply (`--project <dir>`),
/// partial/failed agent apply, and pending multi-admin governance — must never
/// queue or deliver an apply receipt.
#[tokio::test]
async fn non_qualifying_project_governance_and_failed_agent_applies_skip_receipts() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Non-qualifying fleet").await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &stack.cloud.base).await;
    let b_device = cloud_device_id(&second);

    // 1. Project-scoped apply (`muster sync --pull --project <dir>`): applies
    // project files (`applied == true`) but is not a global fleet apply, so it
    // must produce no receipt.
    let project_dir = new_home();
    std::fs::create_dir_all(project_dir.join(".muster")).unwrap();
    std::fs::write(project_dir.join(".muster/muster.toml"), "version = 1\n").unwrap();
    std::fs::write(
        project_dir.join(".muster/AGENTS.md"),
        "# Project-scoped instructions\n",
    )
    .unwrap();
    let proj_pull = run_cli(
        &second,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--project",
            project_dir.to_str().unwrap(),
        ],
    )
    .await;
    assert!(
        proj_pull.status.success(),
        "{} {}",
        String::from_utf8_lossy(&proj_pull.stdout),
        String::from_utf8_lossy(&proj_pull.stderr)
    );
    let proj_env: serde_json::Value = serde_json::from_slice(&proj_pull.stdout).unwrap();
    assert_eq!(proj_env["result"]["applied"], true, "{proj_env}");
    assert!(
        !has_diagnostic(&proj_env, "receipt_queued"),
        "project apply must not queue a receipt: {proj_env}"
    );
    assert!(
        !second.join("state/muster/cloud/receipts.json").exists()
            || pending_receipts(&second).is_empty()
    );

    // 2. Partial/failed agent apply: replace `.claude/CLAUDE.md` with a
    // non-empty directory so the `claude` adapter fails while other adapters
    // apply the new shared instructions (`applied == true` + failed agent).
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "# Shared instructions for partial failure test\n",
    )
    .unwrap();
    publish_secret(&home, "app/partial", "partial-value").await;
    let claude_md = second.join(".claude/CLAUDE.md");
    let _ = std::fs::remove_file(&claude_md);
    std::fs::create_dir_all(claude_md.join("blocker")).unwrap();

    let partial_out = run_cli(&second, &["--format", "json", "sync", "--pull"]).await;
    let partial_env: serde_json::Value = serde_json::from_slice(&partial_out.stdout).unwrap();
    assert_eq!(partial_env["result"]["applied"], true, "{partial_env}");
    let skip_msg = partial_env["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("at least one agent failed to apply"),
        "partial agent failure must skip receipt: {partial_env}"
    );
    assert!(pending_receipts(&second).is_empty());
    std::fs::remove_dir_all(&claude_md).unwrap();
    // Converge `second` cleanly and drain that legitimate receipt (sequence 1).
    let converged = pull_json(&second).await;
    assert_eq!(converged["result"]["applied"], true, "{converged}");
    pull_json(&second).await;
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1
    );

    // 3. Pending governance (`governance_pending == true`): configure 2-admin
    // team mode (`required_approvals = 2`) accepted by both machines, then
    // publish a 1-of-2 governance change from `home` and pull it on `second`.
    let home_manifest_path = home.join("config/muster/muster.toml");
    let current_manifest = std::fs::read_to_string(&home_manifest_path).unwrap();
    let team_manifest = format!(
        "{current_manifest}\n\
         [team]\nrequired_approvals = 2\n\n\
         [members.alice]\nrole = \"admin\"\nstatus = \"active\"\nmachines = [\"laptop\"]\n"
    );
    std::fs::write(&home_manifest_path, &team_manifest).unwrap();
    assert!(run_cli(&home, &["--yes", "trust", "resign"])
        .await
        .status
        .success());
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    // `second` pulls the 2-admin transition (epoch N, accepted because the prior
    // state required only 1 approval).
    pull_json(&second).await;

    // Now `home` proposes a governance change (`required_approvals = 3`) under
    // the 2-admin rule: with only `laptop`'s signature (1 of 2), it is pending.
    // Running `muster sync --pull` while `governance_pending` is active must
    // return `governance_pending = true`, `applied = false`, and queue no receipt.
    let pending_manifest =
        team_manifest.replacen("required_approvals = 2", "required_approvals = 3", 1);
    std::fs::write(&home_manifest_path, pending_manifest).unwrap();
    assert!(run_cli(&home, &["--yes", "trust", "resign"])
        .await
        .status
        .success());

    let gov_pull = pull_json(&home).await;
    assert_eq!(gov_pull["result"]["governance_pending"], true, "{gov_pull}");
    assert_eq!(gov_pull["result"]["applied"], false, "{gov_pull}");
    assert!(
        !has_diagnostic(&gov_pull, "receipt_queued"),
        "pending governance pull must not queue a receipt: {gov_pull}"
    );
    assert!(pending_receipts(&home).is_empty());
    assert!(pending_receipts(&second).is_empty());
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1,
        "pending governance must not advance the service receipt sequence"
    );

    std::fs::remove_dir_all(project_dir).unwrap();
    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// When the service rejects a queued receipt with `422 Unprocessable Entity`
/// (`idempotency_mismatch`, because an earlier receipt with the same sequence
/// and idempotency key `receipt-1` was already stored with different content),
/// `drain` reports `receipt_not_accepted` and leaves the queued receipt
/// untouched (never re-sequenced or rewritten).
#[tokio::test]
async fn drain_422_idempotency_mismatch_keeps_queued_receipt_untouched_and_unrelabeled() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Idempotency mismatch fleet").await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &stack.cloud.base).await;
    let b_device = cloud_device_id(&second);

    // 1. Apply revision R1 and deliver receipt sequence 1 to the service.
    publish_secret(&home, "app/r1", "v1").await;
    let pull1 = pull_json(&second).await;
    assert_eq!(pull1["result"]["applied"], true, "{pull1}");
    let r1 = accepted_revision_of(&second);
    let drain1 = pull_json(&second).await;
    assert!(has_diagnostic(&drain1, "receipt_delivered"), "{drain1}");
    let stored = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(stored.sequence, 1);
    assert_eq!(stored.revision, r1);

    // 2. Simulate local counter regression (`high_water: 0`) before applying a
    // newer revision R2: `second` freezes a validly signed receipt for R2 at
    // sequence 1 (`Idempotency-Key: receipt-1`), colliding with R1 on the server.
    let receipts_path = second.join("state/muster/cloud/receipts.json");
    let mut state: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&receipts_path).unwrap()).unwrap();
    state["links"][0]["high_water"] = serde_json::json!(0);
    std::fs::write(&receipts_path, serde_json::to_vec_pretty(&state).unwrap()).unwrap();

    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "# Shared instructions for R2\n",
    )
    .unwrap();
    publish_secret(&home, "app/r2", "v2").await;
    let pull2 = pull_json(&second).await;
    assert_eq!(pull2["result"]["applied"], true, "{pull2}");
    let r2 = accepted_revision_of(&second);
    assert_ne!(r1, r2);

    let before_drain = pending_receipts(&second);
    assert_eq!(before_drain.len(), 1);
    assert_eq!(before_drain[0]["sequence"], 1);
    assert_eq!(before_drain[0]["revision"], r2);

    // 3. The next pull attempts to drain sequence 1 (for R2) against the real
    // service, which rejects it with HTTP 422 `idempotency_mismatch`.
    let rejected = pull_json(&second).await;
    let diag_msg = rejected["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_not_accepted")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        diag_msg.contains("HTTP 422") && diag_msg.contains("idempotency_mismatch"),
        "expected HTTP 422 idempotency_mismatch diagnostic, got: {rejected}"
    );

    // The queued entry must remain completely untouched: same sequence (1),
    // same revision (R2), same applied_at, and same signature.
    let after_first_reject = pending_receipts(&second);
    assert_eq!(
        after_first_reject, before_drain,
        "422 idempotency_mismatch must not re-sequence or rewrite the queued receipt"
    );
    let after_second_reject = pull_json(&second).await;
    assert!(has_diagnostic(&after_second_reject, "receipt_not_accepted"));
    assert_eq!(
        pending_receipts(&second),
        before_drain,
        "repeated drain after 422 must still keep the exact queued receipt"
    );
    let server_receipt = device_last_receipt(&stack, &alice, &ws1.id, &b_device)
        .await
        .unwrap();
    assert_eq!(server_receipt.sequence, 1);
    assert_eq!(server_receipt.revision, r1);

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}

/// When local `receipts.json` is lost AND the service's device list (`GET
/// /v1/workspaces/{ws}/devices`) is unreachable during `muster cloud link`,
/// `cloud link` succeeds with `receipt_sequence_deferred` and leaves the link
/// without an initialized receipt queue entry; a subsequent qualifying `sync
/// --pull` skips the receipt (`receipt_skipped`: "no initialized receipt
/// sequence for this link") instead of guessing sequence 1.
#[tokio::test]
async fn lost_receipt_state_with_unavailable_remote_sequence_defers_honestly() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Deferred sequence fleet").await;
    let proxy = start_receipt_proxy(stack.cloud.base.clone()).await;
    let (home, second) = bootstrap_and_join(&stack, &alice, &ws1.id, &proxy.url).await;
    let b_device = cloud_device_id(&second);

    // Deliver sequence 1 so the service already holds a receipt for `second`.
    publish_secret(&home, "app/token", "v1").await;
    assert_eq!(pull_json(&second).await["result"]["applied"], true);
    let drained = pull_json(&second).await;
    assert!(has_diagnostic(&drained, "receipt_delivered"), "{drained}");
    assert_eq!(
        device_last_receipt(&stack, &alice, &ws1.id, &b_device)
            .await
            .unwrap()
            .sequence,
        1
    );

    // Unlink, lose `receipts.json`, and make `GET .../devices` fail on re-link.
    assert!(run_cli(&second, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    std::fs::remove_file(second.join("state/muster/cloud/receipts.json")).unwrap();
    proxy
        .fail_devices_get
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let relink = run_cli(
        &second,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id],
    )
    .await;
    assert!(
        relink.status.success(),
        "{} {}",
        String::from_utf8_lossy(&relink.stdout),
        String::from_utf8_lossy(&relink.stderr)
    );
    let relink_env: serde_json::Value = serde_json::from_slice(&relink.stdout).unwrap();
    assert!(
        has_diagnostic(&relink_env, "receipt_sequence_deferred"),
        "relink with unavailable remote sequence must emit receipt_sequence_deferred: {relink_env}"
    );
    assert!(
        !second.join("state/muster/cloud/receipts.json").exists(),
        "relink must not guess high_water = 0 when remote sequence cannot be read"
    );

    // A subsequent qualifying apply must defer honestly instead of guessing
    // sequence 1 (which would collide with the service's existing sequence 1).
    std::fs::write(
        home.join("config/muster/AGENTS.md"),
        "# Shared instructions after lost receipt state\n",
    )
    .unwrap();
    publish_secret(&home, "app/token2", "v2").await;
    let pull_deferred = pull_json(&second).await;
    assert_eq!(pull_deferred["result"]["applied"], true, "{pull_deferred}");
    let skip_msg = pull_deferred["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == "receipt_skipped")
        .and_then(|d| d["message"].as_str())
        .unwrap_or_default();
    assert!(
        skip_msg.contains("no initialized receipt sequence for this link"),
        "expected uninitialized sequence skip diagnostic, got: {pull_deferred}"
    );
    assert!(!second.join("state/muster/cloud/receipts.json").exists());

    std::fs::remove_dir_all(home).unwrap();
    std::fs::remove_dir_all(second).unwrap();
}
