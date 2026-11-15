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
        pull.status,
        revisions[0].signer,
        revisions[1].signer
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
