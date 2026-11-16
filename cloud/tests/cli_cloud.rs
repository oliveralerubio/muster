mod support;

use muster_cloud::protocol::{DeviceApproveRequest, DeviceDecision};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn login_approves_real_device_flow_and_stores_account_privately() {
    let stack = support::stack().await;
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
    let stack = support::stack().await;
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
    let stack = support::stack().await;
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
    let stack = support::stack().await;
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
    let stack = support::stack().await;
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
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

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
    let stack = support::stack().await;
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
    let stack = support::stack().await;
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
