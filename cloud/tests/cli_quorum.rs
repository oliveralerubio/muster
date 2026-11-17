//! Multi-approver (quorum) enrollment over the real Cloud service: a new
//! machine's signed request is only honored once `[team] required_approvals`
//! distinct admins have approved it, each through the public CLI.

mod support;

use muster_cloud::protocol::{DeviceApproveRequest, DeviceDecision};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

use support::stack;

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

/// A fresh home directory for one machine.
fn new_home() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("muster-cli-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    home
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

async fn machine_list_json(home: &std::path::Path) -> serde_json::Value {
    let out = run_cli(home, &["--format", "json", "machine", "list"]).await;
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn pending_of(list: &serde_json::Value, name: &str) -> serde_json::Value {
    list["result"]["pending"]
        .as_array()
        .and_then(|all| all.iter().find(|p| p["name"] == name))
        .cloned()
        .unwrap_or_else(|| panic!("request '{name}' must still be pending: {list}"))
}

/// Enrollment over the Cloud transport needs `[team] required_approvals`
/// DISTINCT admins: one approval leaves the requester pending and powerless;
/// the second activates it as a fleet writer. A duplicate approval by the
/// same admin and the requester's own approval are both refused.
#[tokio::test]
async fn quorum_enrollment_needs_two_distinct_admin_approvals_over_cloud() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws1 = create_workspace(&stack, &alice, "Quorum fleet").await;

    // Machine A bootstraps the fleet and publishes it.
    let a = new_home();
    device_login(&stack, &alice, &a).await;
    assert!(run_cli(&a, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    assert!(run_cli(
        &a,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    assert!(run_cli(&a, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let show_a: serde_json::Value = serde_json::from_slice(
        &run_cli(&a, &["--format", "json", "trust", "show"])
            .await
            .stdout,
    )
    .unwrap();
    let signer = show_a["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_string();

    // Machine B joins under the default single approval (required_approvals
    // is still 1, exactly like the documented transition path to team mode).
    let b = new_home();
    device_login(&stack, &alice, &b).await;
    assert!(run_cli(&b, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    assert!(run_cli(
        &b,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
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
    assert!(
        run_cli(&b, &["--format", "json", "enroll", "--name", "second"])
            .await
            .status
            .success()
    );
    pull_json(&a).await;
    assert!(run_cli(&a, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    assert!(run_cli(&a, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    pull_json(&b).await;
    // B publishes its own annotations (the surviving enrollment-request
    // sibling and the applied-profile record) so the fleet history is linear
    // before the governance transition below.
    assert!(run_cli(&b, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    pull_json(&a).await;

    // Governance transition to team mode with a quorum of two: both existing
    // machines become machines of active admins, plus a plain member that
    // will own the joining machine. Accepted because the prior state required
    // only one approval.
    let manifest_path = a.join("config/muster/muster.toml");
    let current = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        format!(
            "{current}\n\
             [team]\nrequired_approvals = 2\n\n\
             [members.alice]\nrole = \"admin\"\nstatus = \"active\"\nmachines = [\"laptop\"]\n\n\
             [members.bob]\nrole = \"admin\"\nstatus = \"active\"\nmachines = [\"second\"]\n\n\
             [members.charlie]\nrole = \"member\"\nstatus = \"active\"\nmachines = []\n"
        ),
    )
    .unwrap();
    assert!(run_cli(&a, &["--yes", "trust", "resign"])
        .await
        .status
        .success());
    assert!(run_cli(&a, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    pull_json(&b).await;

    // ---- Step 1: C links, pins the fleet, and requests enrollment.
    let c = new_home();
    device_login(&stack, &alice, &c).await;
    assert!(run_cli(&c, &["--yes", "init", "--machine", "third"])
        .await
        .status
        .success());
    assert!(run_cli(
        &c,
        &["--format", "json", "cloud", "link", "--workspace", &ws1.id]
    )
    .await
    .status
    .success());
    let pinned = run_cli(
        &c,
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
        pinned.status.success(),
        "{} {}",
        String::from_utf8_lossy(&pinned.stdout),
        String::from_utf8_lossy(&pinned.stderr)
    );

    // C is not a fleet member: a content change is refused (it cannot sign
    // fleet state), and a full bundle push is refused by the service.
    let unauthorized = run_cli_stdin(
        &c,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/pre",
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

    let enroll = run_cli(&c, &["--format", "json", "enroll", "--name", "third"]).await;
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
    let still_forbidden = run_cli(&c, &["--format", "json", "sync", "--push"]).await;
    assert!(
        !still_forbidden.status.success(),
        "an unenrolled machine must not publish bundles: {}",
        String::from_utf8_lossy(&still_forbidden.stdout)
    );

    // C stays pending across syncs.
    pull_json(&c).await;
    let list_c = machine_list_json(&c).await;
    let pending_c = pending_of(&list_c, "third");
    assert_eq!(pending_c["approvals_received"], 0, "{pending_c}");
    assert_eq!(pending_c["approvals_required"], 2, "{pending_c}");

    // ---- Step 2: A approves (1 of 2) and publishes; C is still powerless.
    pull_json(&a).await;
    let first = run_cli(
        &a,
        &[
            "--format", "json", "approve", "third", "--member", "charlie", "--yes",
        ],
    )
    .await;
    assert!(
        first.status.success(),
        "{} {}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );
    let first_json: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert_eq!(first_json["result"]["status"], "pending", "{first_json}");
    assert_eq!(
        first_json["result"]["approvals_received"], 1,
        "{first_json}"
    );
    assert_eq!(
        first_json["result"]["approvals_required"], 2,
        "{first_json}"
    );
    assert!(run_cli(&a, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // C pulls the 1-of-2 state: still pending, still cannot publish.
    pull_json(&c).await;
    let list_c2 = machine_list_json(&c).await;
    let pending_c2 = pending_of(&list_c2, "third");
    assert_eq!(pending_c2["approvals_received"], 1, "{pending_c2}");
    let unauthorized2 = run_cli_stdin(
        &c,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/pre2",
            "--value-stdin",
        ],
        b"still-early",
    )
    .await;
    assert!(
        !unauthorized2.status.success(),
        "a pending machine must not sign fleet state after one approval: {}",
        String::from_utf8_lossy(&unauthorized2.stdout)
    );

    // ---- Step 4a: the same admin cannot approve twice.
    let duplicate = run_cli(
        &a,
        &[
            "--format", "json", "approve", "third", "--member", "charlie", "--yes",
        ],
    )
    .await;
    assert!(
        !duplicate.status.success(),
        "a duplicate approval by the same admin must be refused: {} {}",
        String::from_utf8_lossy(&duplicate.stdout),
        String::from_utf8_lossy(&duplicate.stderr)
    );
    let duplicate_text = format!(
        "{}{}",
        String::from_utf8_lossy(&duplicate.stdout),
        String::from_utf8_lossy(&duplicate.stderr)
    );
    assert!(
        duplicate_text.contains("already_approved_by_caller"),
        "{duplicate_text}"
    );

    // ---- Step 4b: C cannot approve its own request.
    let self_approval = run_cli(
        &c,
        &[
            "--format", "json", "approve", "third", "--member", "charlie", "--yes",
        ],
    )
    .await;
    assert!(
        !self_approval.status.success(),
        "a machine cannot approve its own enrollment: {} {}",
        String::from_utf8_lossy(&self_approval.stdout),
        String::from_utf8_lossy(&self_approval.stderr)
    );
    let self_text = format!(
        "{}{}",
        String::from_utf8_lossy(&self_approval.stdout),
        String::from_utf8_lossy(&self_approval.stderr)
    );
    assert!(self_text.contains("self-approval"), "{self_text}");

    // ---- Step 3: B's approval reaches the quorum; C becomes a writer.
    pull_json(&b).await;
    let second = run_cli(
        &b,
        &[
            "--format", "json", "approve", "third", "--member", "charlie", "--yes",
        ],
    )
    .await;
    assert!(
        second.status.success(),
        "{} {}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );
    let second_json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second_json["result"]["status"], "active", "{second_json}");
    assert_eq!(
        second_json["result"]["approvals_received"], 2,
        "{second_json}"
    );
    let approvers: Vec<String> = second_json["result"]["approvals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(approvers, vec!["alice", "bob"], "{second_json}");
    assert!(run_cli(&b, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // C pulls its enrollment: it is now an enrolled fleet machine.
    pull_json(&c).await;
    let list_c3 = machine_list_json(&c).await;
    assert!(
        list_c3["result"]["machines"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["name"] == "third"),
        "C must be enrolled after the second approval: {list_c3}"
    );
    assert!(
        !list_c3["result"]["pending"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "third"),
        "no pending request for third may remain: {list_c3}"
    );

    // The enrolled machine publishes content, and A and B receive it.
    let add = run_cli_stdin(
        &c,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"quorum-value",
    )
    .await;
    assert!(
        add.status.success(),
        "{} {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr)
    );
    let content_push = run_cli(&c, &["--format", "json", "sync", "--push"]).await;
    assert!(
        content_push.status.success(),
        "{} {}",
        String::from_utf8_lossy(&content_push.stdout),
        String::from_utf8_lossy(&content_push.stderr)
    );
    pull_json(&a).await;
    pull_json(&b).await;
    let show_a2: serde_json::Value = serde_json::from_slice(
        &run_cli(&a, &["--format", "json", "trust", "show"])
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
        "A must have received C's published vault group (secret path app/token): {show_a2}"
    );

    std::fs::remove_dir_all(a).unwrap();
    std::fs::remove_dir_all(b).unwrap();
    std::fs::remove_dir_all(c).unwrap();
}

// ---------------------------------------------------------------------------
// Convergence of quorum enrollment with plain `sync --pull`.
// ---------------------------------------------------------------------------

use axum::response::IntoResponse;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

async fn login_at(
    stack: &support::Stack,
    alice: &support::UserSession,
    home: &Path,
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
    assert!(output.status.success(), "{}", text(&output));
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

async fn ok_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let output = run_cli(home, &full).await;
    assert!(output.status.success(), "{args:?}: {}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

async fn failing_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let output = run_cli(home, &full).await;
    assert!(
        !output.status.success(),
        "{args:?} must fail: {}",
        text(&output)
    );
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{args:?}: no JSON envelope ({e}): {}", text(&output)))
}

fn messages(envelope: &Value) -> String {
    envelope["diagnostics"]
        .as_array()
        .map(|all| {
            all.iter()
                .map(|d| format!("[{}] {}", d["code"], d["message"]))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn config_of(home: &Path) -> PathBuf {
    home.join("config/muster")
}

fn git_in(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn head_of(home: &Path) -> String {
    git_in(&config_of(home), &["rev-parse", "HEAD"])
}

fn accepted_revision_of(home: &Path) -> String {
    let accepted: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    accepted["revision"].as_str().unwrap().to_owned()
}

fn is_enrolled(list: &Value, name: &str) -> bool {
    list["result"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["name"] == name)
}

fn is_pending(list: &Value, name: &str) -> bool {
    list["result"]["pending"]
        .as_array()
        .is_some_and(|all| all.iter().any(|p| p["name"] == name))
}

async fn approve(home: &Path, name: &str) -> std::process::Output {
    run_cli(
        home,
        &[
            "--format", "json", "approve", name, "--member", "charlie", "--yes",
        ],
    )
    .await
}

/// A (alice) and B (bob) are the two active admins of a fleet that requires
/// two approvals; charlie is a plain member without machines yet. Returns
/// `(a, b, signer)` converged on the accepted head.
async fn team_fleet(
    stack: &support::Stack,
    alice: &support::UserSession,
    workspace: &str,
    endpoint: &str,
) -> (PathBuf, PathBuf, String) {
    let a = new_home();
    login_at(stack, alice, &a, endpoint).await;
    assert!(run_cli(&a, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    ok_json(&a, &["cloud", "link", "--workspace", workspace]).await;
    ok_json(&a, &["sync", "--push"]).await;
    let show = ok_json(&a, &["trust", "show"]).await;
    let signer = show["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_owned();
    let b = new_home();
    login_at(stack, alice, &b, endpoint).await;
    assert!(run_cli(&b, &["--yes", "init", "--machine", "second"])
        .await
        .status
        .success());
    ok_json(&b, &["cloud", "link", "--workspace", workspace]).await;
    ok_json(&b, &["sync", "--pull", "--expect-signer", &signer]).await;
    ok_json(&b, &["enroll", "--name", "second"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(run_cli(&a, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    ok_json(&b, &["sync", "--push"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    let manifest_path = config_of(&a).join("muster.toml");
    let current = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        format!(
            "{current}\n\
             [team]\nrequired_approvals = 2\n\n\
             [members.alice]\nrole = \"admin\"\nstatus = \"active\"\nmachines = [\"laptop\"]\n\n\
             [members.bob]\nrole = \"admin\"\nstatus = \"active\"\nmachines = [\"second\"]\n\n\
             [members.charlie]\nrole = \"member\"\nstatus = \"active\"\nmachines = []\n"
        ),
    )
    .unwrap();
    assert!(run_cli(&a, &["--yes", "trust", "resign"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    (a, b, signer)
}

/// Machine C links, pins the fleet and requests enrollment as `name`.
async fn requester(
    stack: &support::Stack,
    alice: &support::UserSession,
    workspace: &str,
    endpoint: &str,
    signer: &str,
    name: &str,
) -> PathBuf {
    let c = new_home();
    login_at(stack, alice, &c, endpoint).await;
    assert!(run_cli(&c, &["--yes", "init", "--machine", name])
        .await
        .status
        .success());
    ok_json(&c, &["cloud", "link", "--workspace", workspace]).await;
    ok_json(&c, &["sync", "--pull", "--expect-signer", signer]).await;
    ok_json(&c, &["enroll", "--name", name]).await;
    c
}

/// After the quorum C is a writer: what it publishes reaches the admins.
async fn assert_c_is_a_writer(a: &Path, c: &Path) {
    let add = run_cli_stdin(
        c,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"quorum-value",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(c, &["sync", "--push"]).await;
    ok_json(a, &["sync", "--pull"]).await;
    let show = ok_json(a, &["trust", "show"]).await;
    assert!(
        show["result"]["fleet_state"]["vault_groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g == "app"),
        "{show}"
    );
}

#[tokio::test]
async fn requester_pulling_only_after_both_approvals_is_enrolled() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum late pull").await;
    let (a, b, signer) = team_fleet(&stack, &alice, &ws.id, &stack.cloud.base).await;
    let c = requester(&stack, &alice, &ws.id, &stack.cloud.base, &signer, "third").await;

    ok_json(&a, &["sync", "--pull"]).await;
    assert!(approve(&a, "third").await.status.success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    let second = approve(&b, "third").await;
    assert!(second.status.success(), "{}", text(&second));
    ok_json(&b, &["sync", "--push"]).await;

    // C never saw the 1-of-2 state: one plain pull enrolls it.
    let pull = ok_json(&c, &["sync", "--pull"]).await;
    assert_eq!(pull["result"]["transport"]["pulled"], "updated", "{pull}");
    let list = machine_list_json(&c).await;
    assert!(is_enrolled(&list, "third"), "{list}");
    assert!(!is_pending(&list, "third"), "{list}");
    assert!(!config_of(&c).join("enroll/third.toml").exists());
    assert_eq!(
        git_in(&config_of(&c), &["status", "--porcelain"]),
        "",
        "nothing is left uncommitted"
    );
    assert_c_is_a_writer(&a, &c).await;
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(&a), accepted_revision_of(&b));
}

/// B's final approval (2 of 2: it enrolls C, removes the request and
/// re-seals) loses the race against A's secret. The request removal belongs
/// to that governance change and is never replayed alone: B keeps the
/// request, and repeating the approval on the accepted state enrolls C.
#[tokio::test]
async fn lost_final_approval_keeps_the_request_until_approved_again() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum lost final approval").await;
    let (a, b, signer) = team_fleet(&stack, &alice, &ws.id, &stack.cloud.base).await;
    let c = requester(&stack, &alice, &ws.id, &stack.cloud.base, &signer, "third").await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(approve(&a, "third").await.status.success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    let request = config_of(&b).join("enroll/third.toml");
    assert!(request.exists(), "the 1-of-2 request is in the fleet");

    let add = run_cli_stdin(
        &a,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"from-alice",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(&a, &["sync", "--push"]).await;
    let last = approve(&b, "third").await;
    assert!(last.status.success(), "{}", text(&last));
    assert!(!request.exists());
    let lost = failing_json(&b, &["sync", "--push"]).await;
    assert!(messages(&lost).contains("revision_conflict"), "{lost}");

    let pull = failing_json(&b, &["sync", "--pull"]).await;
    let notes = messages(&pull);
    assert!(
        notes.contains("enroll/third.toml") && notes.contains("muster approve third"),
        "{notes}"
    );
    assert!(
        request.exists(),
        "the removal is not replayed without the enrollment"
    );
    let manifest = std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    assert!(!manifest.contains("[machines.third]"), "{manifest}");

    // Approvals bind to the state they were given on, and A's secret moved
    // it: B approves again, then A, and C is enrolled.
    let again = approve(&b, "third").await;
    assert!(again.status.success(), "{}", text(&again));
    ok_json(&b, &["sync", "--push"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    let pending = machine_list_json(&a).await;
    if !is_enrolled(&pending, "third") {
        assert!(approve(&a, "third").await.status.success());
        ok_json(&a, &["sync", "--push"]).await;
    }
    let pull = ok_json(&c, &["sync", "--pull"]).await;
    let list = machine_list_json(&c).await;
    assert!(is_enrolled(&list, "third"), "{pull}\n{list}");
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(&a), accepted_revision_of(&b));
}

#[tokio::test]
async fn concurrent_admin_approvals_converge_by_repeating_the_command() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum concurrent").await;
    let (a, b, signer) = team_fleet(&stack, &alice, &ws.id, &stack.cloud.base).await;
    let c = requester(&stack, &alice, &ws.id, &stack.cloud.base, &signer, "third").await;

    // A approves (1 of 2) and publishes; B approves before pulling it.
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(approve(&a, "third").await.status.success());
    ok_json(&a, &["sync", "--push"]).await;
    let early = approve(&b, "third").await;
    assert!(early.status.success(), "{}", text(&early));
    let lost = failing_json(&b, &["sync", "--push"]).await;
    assert!(messages(&lost).contains("revision_conflict"), "{lost}");

    // B's pull is an honest conflict that names the request and the command.
    let pull = failing_json(&b, &["sync", "--pull"]).await;
    let said = messages(&pull);
    assert!(
        said.contains("enroll/third.toml") && said.contains("muster approve third"),
        "{said}"
    );
    let status = ok_json(&b, &["status"]).await;
    assert_eq!(status["result"]["sync"]["result"], "conflict", "{status}");
    assert_eq!(head_of(&b), head_of(&a), "B sits on the accepted state");
    assert_eq!(git_in(&config_of(&b), &["status", "--porcelain"]), "");

    // Repeating the command completes the quorum.
    let again = approve(&b, "third").await;
    assert!(again.status.success(), "{}", text(&again));
    let again: Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(again["result"]["status"], "active", "{again}");
    ok_json(&b, &["sync", "--push"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    ok_json(&c, &["sync", "--pull"]).await;
    let list = machine_list_json(&c).await;
    assert!(is_enrolled(&list, "third"), "{list}");
    assert!(!is_pending(&list, "third"), "{list}");
    assert_c_is_a_writer(&a, &c).await;
}

/// A name already bound to another machine's recipient, later revoked, never
/// makes this machine drop its own request.
#[tokio::test]
async fn request_under_a_name_enrolled_with_another_key_is_kept() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum name taken").await;
    let base = stack.cloud.base.clone();
    let a = new_home();
    login_at(&stack, &alice, &a, &base).await;
    assert!(run_cli(&a, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    ok_json(&a, &["cloud", "link", "--workspace", &ws.id]).await;
    ok_json(&a, &["sync", "--push"]).await;
    let show = ok_json(&a, &["trust", "show"]).await;
    let signer = show["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_owned();

    // C pins the fleet first; D then takes the name `third` and is approved.
    let c = new_home();
    login_at(&stack, &alice, &c, &base).await;
    assert!(run_cli(&c, &["--yes", "init", "--machine", "third"])
        .await
        .status
        .success());
    ok_json(&c, &["cloud", "link", "--workspace", &ws.id]).await;
    ok_json(&c, &["sync", "--pull", "--expect-signer", &signer]).await;
    let d = new_home();
    login_at(&stack, &alice, &d, &base).await;
    assert!(run_cli(&d, &["--yes", "init", "--machine", "dee"])
        .await
        .status
        .success());
    ok_json(&d, &["cloud", "link", "--workspace", &ws.id]).await;
    ok_json(&d, &["sync", "--pull", "--expect-signer", &signer]).await;
    ok_json(&d, &["enroll", "--name", "third"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(run_cli(&a, &["--yes", "approve", "third"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&d, &["sync", "--pull"]).await;

    // C, still behind, requests the same name with its own key.
    ok_json(&c, &["enroll", "--name", "third"]).await;
    let request = config_of(&c).join("enroll/third.toml");
    let mine = std::fs::read_to_string(&request).unwrap();
    ok_json(&c, &["sync", "--pull"]).await;
    assert_eq!(
        std::fs::read_to_string(&request).unwrap(),
        mine,
        "the name belongs to another key: C keeps its own request"
    );
    assert!(
        git_in(&config_of(&c), &["log", "--format=%s", "-3"])
            .contains("enrollment request for third"),
        "C's request commit is rebased, not dropped"
    );

    // The other `third` is revoked: still not C's request to drop.
    assert!(run_cli(&a, &["--yes", "revoke", "third"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&c, &["sync", "--pull"]).await;
    assert_eq!(std::fs::read_to_string(&request).unwrap(), mine);
}

// ---------------------------------------------------------------------------
// Malicious server: a forwarding proxy that can serve a forged bundle.
// ---------------------------------------------------------------------------

type Served = (Vec<u8>, String);

#[derive(Clone)]
struct Proxy {
    upstream: String,
    client: reqwest::Client,
    serve: Arc<Mutex<Option<Served>>>,
    /// When set, `GET .../proposals` lists every proposal as created by
    /// another device.
    foreign_proposals: Arc<std::sync::atomic::AtomicBool>,
    /// Every `POST .../proposals/{id}/resolve` that went through.
    resolutions: Arc<Mutex<Vec<String>>>,
}

async fn proxy_handler(
    axum::extract::State(proxy): axum::extract::State<Proxy>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    if method == axum::http::Method::GET && path.ends_with("/sync") {
        let served = proxy.serve.lock().unwrap().clone();
        if let Some((bundle, revision)) = served {
            return axum::response::Response::builder()
                .status(200)
                .header("content-type", "application/x-git-bundle")
                .header("x-muster-revision", revision)
                .body(axum::body::Body::from(bundle))
                .unwrap();
        }
    }
    if method == axum::http::Method::POST && path.ends_with("/resolve") {
        proxy.resolutions.lock().unwrap().push(path.clone());
    }
    let lists_proposals = method == axum::http::Method::GET && path.ends_with("/proposals");
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let mut request = proxy
        .client
        .request(method, format!("{}{path}{query}", proxy.upstream));
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
                format!("upstream: {e}"),
            )
                .into_response()
        }
    };
    let mut out = axum::response::Response::builder().status(response.status());
    for (name, value) in response.headers() {
        if !matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length"
        ) {
            out = out.header(name, value);
        }
    }
    let mut bytes = response.bytes().await.expect("upstream body").to_vec();
    if lists_proposals
        && proxy
            .foreign_proposals
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        let mut listed: serde_json::Value = serde_json::from_slice(&bytes).expect("proposal list");
        for proposal in listed.as_array_mut().expect("proposal array") {
            proposal["device_id"] = serde_json::json!("another-device");
        }
        bytes = serde_json::to_vec(&listed).unwrap();
    }
    out.body(axum::body::Body::from(bytes)).unwrap()
}

async fn start_proxy(upstream: String) -> (String, Proxy) {
    let state = Proxy {
        upstream,
        client: support::http(),
        serve: Arc::new(Mutex::new(None)),
        foreign_proposals: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resolutions: Arc::new(Mutex::new(Vec::new())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .fallback(proxy_handler)
        .with_state(state.clone());
    tokio::spawn(async move {
        axum::serve(listener, app.into_make_service()).await.ok();
    });
    (format!("http://127.0.0.1:{port}"), state)
}

/// A forged tip: the accepted history of `home` plus one commit made by
/// `edit` in a scratch clone (unsigned files only, so it still verifies).
/// Returns the bundle and the forged commit.
fn forge(home: &Path, edit: impl FnOnce(&Path)) -> Served {
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    git_in(
        scratch.path(),
        &[
            "clone",
            "--quiet",
            config_of(home).to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    edit(&work);
    git_in(&work, &["add", "-A"]);
    git_in(&work, &["commit", "--quiet", "-m", "forged by the server"]);
    let tip = git_in(&work, &["rev-parse", "HEAD"]);
    let file = scratch.path().join("forged.bundle");
    git_in(&work, &["update-ref", "refs/heads/main", &tip]);
    git_in(
        &work,
        &[
            "bundle",
            "create",
            file.to_str().unwrap(),
            "refs/heads/main",
        ],
    );
    (std::fs::read(file).unwrap(), tip)
}

#[tokio::test]
async fn forged_enroll_paths_never_delete_or_commit_anything() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum forged").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b, signer) = team_fleet(&stack, &alice, &ws.id, &endpoint).await;
    let c = requester(&stack, &alice, &ws.id, &endpoint, &signer, "third").await;
    let request = std::fs::read_to_string(config_of(&c).join("enroll/third.toml")).unwrap();
    assert_eq!(head_of(&a), accepted_revision_of(&a));

    // (a) `enroll` is a symlink to the repository root, where a copy of C's
    // own request sits as `third.toml` next to muster.toml.
    let (bundle, tip) = forge(&a, |work| {
        let _ = std::fs::remove_dir_all(work.join("enroll"));
        std::os::unix::fs::symlink(".", work.join("enroll")).unwrap();
        std::fs::write(work.join("third.toml"), &request).unwrap();
    });
    *proxy.serve.lock().unwrap() = Some((bundle, tip.clone()));
    let before = head_of(&c);
    let pulled = run_cli(&c, &["--format", "json", "sync", "--pull"]).await;
    let config = config_of(&c);
    assert!(config.join("muster.toml").is_file(), "{}", text(&pulled));
    assert!(config.join("fleet.state.json").is_file());
    let head = head_of(&c);
    assert!(
        head == before || head == tip,
        "no local commit may be created: {head} ({})",
        text(&pulled)
    );
    if head == tip {
        assert_eq!(
            std::fs::read_to_string(config.join("third.toml")).unwrap(),
            request,
            "nothing outside the machine's own request is deleted"
        );
    }
    assert_eq!(git_in(&config, &["status", "--porcelain"]), "");

    // (b) an injected regular request under an admin's own name is pulled
    // as is: not deleted, no local commit, even with an unpublished change.
    std::fs::write(config_of(&b).join("AGENTS.md"), "B's local note\n").unwrap();
    let b_head = head_of(&b);
    let b_manifest = std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    let field = |key: &str| {
        b_manifest
            .split("[machines.second]")
            .nth(1)
            .and_then(|section| {
                section
                    .lines()
                    .find_map(|l| l.strip_prefix(&format!("{key} = ")))
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| panic!("no {key} for second in {b_manifest}"))
    };
    let injected = format!(
        "name = \"second\"\nrecipient = {}\nsigning_key = {}\nnonce = \"00\"\n\
         platform = \"linux-x86_64\"\nmuster_version = \"0.1.0\"\n\
         created_at = \"2026-01-01T00:00:00Z\"\n",
        field("recipient"),
        field("signing_key")
    );
    let (bundle, tip) = forge(&a, |work| {
        std::fs::create_dir_all(work.join("enroll")).unwrap();
        std::fs::write(work.join("enroll/second.toml"), &injected).unwrap();
    });
    *proxy.serve.lock().unwrap() = Some((bundle, tip.clone()));
    let pulled = ok_json(&b, &["sync", "--pull"]).await;
    let config = config_of(&b);
    assert_eq!(
        std::fs::read_to_string(config.join("enroll/second.toml")).unwrap(),
        injected,
        "{pulled}"
    );
    // Only B's own commits (its sync commit and the applied-profile record)
    // sit on the served tip; none touches enroll/.
    assert_ne!(head_of(&b), b_head);
    git_in(&config, &["merge-base", "--is-ancestor", &tip, "HEAD"]);
    let own = git_in(&config, &["log", "--format=%s", &format!("{tip}..HEAD")]);
    assert!(
        own.lines()
            .all(|s| s == "chore(muster): sync" || s.contains("applied the approved profile")),
        "{own}"
    );
    let touched = git_in(&config, &["diff", "--name-only", &tip, "HEAD"]);
    assert!(
        touched
            .lines()
            .all(|p| p == "AGENTS.md" || p == "muster.toml"),
        "{touched}"
    );
    assert_eq!(git_in(&config, &["status", "--porcelain"]), "");
    *proxy.serve.lock().unwrap() = None;
}

/// Bob (admin) has an unpublished secret seal when the server serves him
/// Alice's governance proposal, still short of Bob's co-signature, as the
/// accepted head (Cloud itself keeps such a publication as a proposal; only
/// a lying server serves it as the head). Bob's pull stops cleanly in
/// `governance_pending`: nothing of his is replayed or committed on top of
/// the unaccepted proposal, and his commits are kept under a backup ref.
/// He can still co-sign it and then redo his change.
#[tokio::test]
async fn pending_governance_tip_stops_cleanly_without_replay() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum pending fork").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b, _signer) = team_fleet(&stack, &alice, &ws.id, &endpoint).await;

    let add = run_cli_stdin(
        &b,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "ops/key",
            "--value-stdin",
        ],
        b"from-bob",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    let own = head_of(&b);
    let b_accepted = accepted_revision_of(&b);
    ok_json(&a, &["--yes", "member", "rm", "charlie"]).await;
    let proposal = head_of(&a);
    *proxy.serve.lock().unwrap() = Some(served_head(&a));

    let pull = ok_json(&b, &["sync", "--pull"]).await;
    *proxy.serve.lock().unwrap() = None;
    assert_eq!(pull["result"]["governance_pending"], true, "{pull}");
    let notes = messages(&pull);
    assert!(notes.contains("signed_fork_pending"), "{notes}");
    assert!(!notes.contains("signed_change_conflict"), "{notes}");
    assert!(notes.contains("cloud_revision_unaccepted"), "{notes}");
    assert_eq!(
        accepted_revision_of(&b),
        b_accepted,
        "a proposal still short of its quorum is never recorded as accepted"
    );
    assert_eq!(head_of(&b), proposal, "nothing committed on top of it");
    let backup = git_in(
        &config_of(&b),
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/muster/abandoned/",
        ],
    );
    assert_eq!(backup.trim(), own, "Bob's commits are kept");

    // Back on the honest server, Bob co-signs and redoes his change.
    let honest = ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(honest["result"]["governance_pending"], true, "{honest}");
    ok_json(&b, &["--yes", "trust", "cosign"]).await;
    ok_json(&b, &["sync", "--push"]).await;
    let redo = run_cli_stdin(
        &b,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "ops/key",
            "--value-stdin",
        ],
        b"from-bob",
    )
    .await;
    assert!(redo.status.success(), "{}", text(&redo));
    ok_json(&b, &["sync", "--push"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(&a), accepted_revision_of(&b));
}

/// The full history of `home`'s HEAD as the served `main`.
fn served_head(home: &Path) -> Served {
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    git_in(
        scratch.path(),
        &[
            "clone",
            "--quiet",
            config_of(home).to_str().unwrap(),
            work.to_str().unwrap(),
        ],
    );
    let tip = git_in(&work, &["rev-parse", "HEAD"]);
    git_in(&work, &["update-ref", "refs/heads/main", &tip]);
    let file = scratch.path().join("served.bundle");
    git_in(
        &work,
        &[
            "bundle",
            "create",
            file.to_str().unwrap(),
            "refs/heads/main",
        ],
    );
    (std::fs::read(file).unwrap(), tip)
}

/// Review scenario A: Bob pulled Alice's proposal (an instruction edit plus
/// a governance change, 1 of 2: pending, Bob's pin stays on the accepted
/// state) from a server that served it as the head. Then a valid newer tip
/// arrives that does not contain it. The proposal's commits are not Bob's
/// own seals: they are never replayed (not even their unsigned files) nor
/// re-sealed under Bob's key; the pull is refused cleanly.
#[tokio::test]
async fn pending_foreign_proposal_is_never_replayed_onto_a_newer_tip() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum foreign pending").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b, _signer) = team_fleet(&stack, &alice, &ws.id, &endpoint).await;
    let fork_base = head_of(&a);
    let a_pin_file = a.join("state/muster/trust.json");
    let a_pin = std::fs::read(&a_pin_file).unwrap();

    std::fs::write(config_of(&a).join("AGENTS.md"), "proposal instruction\n").unwrap();
    ok_json(&a, &["--yes", "member", "rm", "charlie"]).await;
    *proxy.serve.lock().unwrap() = Some(served_head(&a));
    let held = ok_json(&b, &["sync", "--pull"]).await;
    *proxy.serve.lock().unwrap() = None;
    assert_eq!(held["result"]["governance_pending"], true, "{held}");

    // Alice gives the proposal up and publishes a secret from the base.
    git_in(&config_of(&a), &["reset", "--quiet", "--hard", &fork_base]);
    std::fs::write(&a_pin_file, a_pin).unwrap();
    let add = run_cli_stdin(
        &a,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/token",
            "--value-stdin",
        ],
        b"from-alice",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(&a, &["sync", "--push"]).await;

    let before = head_of(&b);
    let refused = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(head_of(&b), before, "{refused}");
    let backups = git_in(
        &config_of(&b),
        &[
            "for-each-ref",
            "--format=%(refname)",
            "refs/muster/abandoned/",
        ],
    );
    assert!(backups.trim().is_empty(), "{backups}");
    let log = git_in(
        &config_of(&b),
        &["log", "--format=%s", &format!("{before}..HEAD")],
    );
    assert!(log.trim().is_empty(), "nothing committed: {log}");
}

/// A secret published by Bob wins the race against Alice's governance
/// change, which in a 2-of-2 fleet is sealed pending on her machine (her pin
/// stays on the accepted state). Returns `(a, b, bob's accepted head)`.
async fn pending_change_loses(
    stack: &support::Stack,
    alice: &support::UserSession,
    workspace: &str,
    change: &[&str],
    stdin: Option<&[u8]>,
) -> (PathBuf, PathBuf, String) {
    let base = stack.cloud.base.clone();
    let (a, b, _signer) = team_fleet(stack, alice, workspace, &base).await;
    let add = run_cli_stdin(
        &b,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/other",
            "--value-stdin",
        ],
        b"from-bob",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(&b, &["sync", "--push"]).await;
    let accepted = head_of(&b);
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(change);
    let proposed = match stdin {
        Some(value) => run_cli_stdin(&a, &full, value).await,
        None => run_cli(&a, &full).await,
    };
    assert!(proposed.status.success(), "{}", text(&proposed));
    let lost = failing_json(&a, &["sync", "--push"]).await;
    assert!(messages(&lost).contains("revision_conflict"), "{lost}");
    (a, b, accepted)
}

/// After the race Alice is on Bob's accepted head, and her change is (again)
/// a pending proposal on top of it: publishing it is held for the quorum,
/// like any 2-of-2 proposal, and the next pull is ordinary.
async fn reproposed_on_the_accepted_head(a: &Path, accepted: &str) {
    assert_eq!(
        git_in(&config_of(a), &["rev-parse", "HEAD~1"]),
        accepted,
        "the proposal sits directly on the accepted head"
    );
    held_push(a, accepted).await;
    ok_json(a, &["sync", "--pull"]).await;
}

/// A publication Cloud holds for the quorum (202) is a held proposal, not an
/// error: the accepted head did not move and nothing is left in flight.
async fn held_push(a: &Path, accepted: &str) {
    let held = ok_json(a, &["sync", "--push"]).await;
    let proposal = &held["result"]["transport"]["held"];
    assert_eq!(proposal["kind"], "pending_quorum", "{held}");
    assert!(proposal["proposal_id"].is_string(), "{held}");
    assert_eq!(held["result"]["transport"]["pushed"], false, "{held}");
    assert!(messages(&held).contains("cloud_proposal_held"), "{held}");
    assert!(!a.join("state/muster/cloud/push-intent.json").exists());
    assert_eq!(accepted_revision_of(a), accepted);
}

#[tokio::test]
async fn pending_member_change_lost_in_a_race_converges_by_rerunning_it() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum pending member race").await;
    // Removing a member needs both admins: sealed pending on Alice's machine.
    let change = ["--yes", "member", "rm", "charlie"];
    let (a, _b, accepted) = pending_change_loses(&stack, &alice, &ws.id, &change, None).await;

    let pull = failing_json(&a, &["sync", "--pull"]).await;
    let notes = messages(&pull);
    assert!(notes.contains("muster.toml governance"), "{notes}");
    assert!(!notes.contains("resolve manually"), "{notes}");
    assert_eq!(head_of(&a), accepted, "on the accepted head");

    ok_json(&a, &change).await;
    reproposed_on_the_accepted_head(&a, &accepted).await;
}

#[tokio::test]
async fn pending_allow_change_lost_in_a_race_is_reproposed_on_its_own() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum pending allow race").await;
    let change = [
        "secret",
        "add",
        "ops/key",
        "--allow",
        "claude",
        "--value-stdin",
    ];
    let (a, _b, accepted) =
        pending_change_loses(&stack, &alice, &ws.id, &change, Some(b"from-alice")).await;

    let pull = ok_json(&a, &["sync", "--pull"]).await;
    assert_eq!(
        pull["result"]["transport"]["pulled"], "reconciled",
        "{pull}"
    );
    assert_eq!(pull["result"]["governance_pending"], true, "{pull}");
    let manifest = std::fs::read_to_string(config_of(&a).join("muster.toml")).unwrap();
    assert!(manifest.contains("native://ops/key"), "{manifest}");
    reproposed_on_the_accepted_head(&a, &accepted).await;
}

/// C's own request already reached the fleet (A's 1-of-2 approval) and C's
/// other local change collides textually with A's. The pull is rebuilt like
/// any fork instead of refusing with "retry" forever; the backup keeps C's
/// original history, request included.
#[tokio::test]
async fn superseded_request_with_a_textual_conflict_is_rebuilt_not_retried() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum superseded conflict").await;
    let base = stack.cloud.base.clone();
    let (a, _b, signer) = team_fleet(&stack, &alice, &ws.id, &base).await;
    let c = requester(&stack, &alice, &ws.id, &base, &signer, "third").await;
    let request_commit = head_of(&c);
    std::fs::write(config_of(&c).join("AGENTS.md"), "written on C\n").unwrap();

    ok_json(&a, &["sync", "--pull"]).await;
    assert!(approve(&a, "third").await.status.success());
    std::fs::write(config_of(&a).join("AGENTS.md"), "written on A\n").unwrap();
    ok_json(&a, &["sync", "--push"]).await;

    let pull = failing_json(&c, &["sync", "--pull"]).await;
    let notes = messages(&pull);
    assert!(notes.contains("AGENTS.md"), "{notes}");
    assert!(!notes.contains("retry muster sync --pull"), "{notes}");
    let backup = git_in(
        &config_of(&c),
        &[
            "for-each-ref",
            "--format=%(objectname)",
            "refs/muster/abandoned/",
        ],
    );
    let backup = backup.trim();
    assert!(!backup.is_empty(), "{notes}");
    git_in(
        &config_of(&c),
        &["merge-base", "--is-ancestor", &request_commit, backup],
    );
    assert_eq!(
        std::fs::read_to_string(config_of(&c).join("enroll/third.toml")).unwrap(),
        std::fs::read_to_string(config_of(&a).join("enroll/third.toml")).unwrap(),
        "the fleet's copy of the request (with A's approval) is kept"
    );
    assert_eq!(
        std::fs::read_to_string(config_of(&c).join("AGENTS.md")).unwrap(),
        "written on A\n"
    );
    // Not stuck: the next pull is ordinary.
    ok_json(&c, &["sync", "--pull"]).await;
}

/// Alice's governance change reaches Cloud first and is held for the quorum
/// (202); Bob then advances the accepted head. Alice's next pull converges on
/// its own: her change is re-proposed on top of Bob's head.
#[tokio::test]
async fn held_proposal_then_a_newer_head_converges_on_the_next_pull() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum held then advanced").await;
    let base = stack.cloud.base.clone();
    let (a, b, _signer) = team_fleet(&stack, &alice, &ws.id, &base).await;
    let before = accepted_revision_of(&a);

    let proposed = run_cli_stdin(
        &a,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "ops/key",
            "--allow",
            "claude",
            "--value-stdin",
        ],
        b"from-alice",
    )
    .await;
    assert!(proposed.status.success(), "{}", text(&proposed));
    held_push(&a, &before).await;

    let add = run_cli_stdin(
        &b,
        &[
            "--format",
            "json",
            "secret",
            "add",
            "app/other",
            "--value-stdin",
        ],
        b"from-bob",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(&b, &["sync", "--push"]).await;
    let accepted = head_of(&b);

    let pull = ok_json(&a, &["sync", "--pull"]).await;
    assert_eq!(
        pull["result"]["transport"]["pulled"], "reconciled",
        "{pull}"
    );
    assert_eq!(pull["result"]["governance_pending"], true, "{pull}");
    assert_eq!(accepted_revision_of(&a), accepted);
    reproposed_on_the_accepted_head(&a, &accepted).await;
}

/// The open proposals of the workspace, as `(id, kind, revision)`.
async fn open_proposals(home: &Path) -> Vec<(String, String, String)> {
    let listed = ok_json(home, &["cloud", "proposals"]).await;
    listed["result"]["proposals"]
        .as_array()
        .unwrap_or_else(|| panic!("{listed}"))
        .iter()
        .map(|p| {
            (
                p["id"].as_str().unwrap().to_owned(),
                p["kind"].as_str().unwrap().to_owned(),
                p["revision"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// Pushing a held proposal again is idempotent: while it is open for the
/// same commit no new proposal is created. A changed commit supersedes it
/// on its own: the old one is closed, so exactly one stays open.
#[tokio::test]
async fn repeated_push_of_a_held_proposal_never_duplicates_it() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum held idempotent").await;
    let base = stack.cloud.base.clone();
    let (a, _b, _signer) = team_fleet(&stack, &alice, &ws.id, &base).await;
    let accepted = accepted_revision_of(&a);
    let before = open_proposals(&a).await;

    ok_json(&a, &["--yes", "member", "rm", "charlie"]).await;
    let first = ok_json(&a, &["sync", "--push"]).await;
    let id = first["result"]["transport"]["held"]["proposal_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{first}"))
        .to_owned();
    let again = ok_json(&a, &["sync", "--push"]).await;
    assert_eq!(
        again["result"]["transport"]["held"]["proposal_id"], id,
        "{again}"
    );
    let open: Vec<_> = open_proposals(&a)
        .await
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect();
    assert_eq!(
        open,
        vec![(id.clone(), "pending_quorum".into(), head_of(&a))]
    );
    assert_eq!(accepted_revision_of(&a), accepted);

    // A new commit on top replaces the held proposal.
    std::fs::write(config_of(&a).join("AGENTS.md"), "while held\n").unwrap();
    let changed = ok_json(&a, &["sync", "--push"]).await;
    let newer = changed["result"]["transport"]["held"]["proposal_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{changed}"))
        .to_owned();
    assert_ne!(newer, id, "{changed}");
    let open: Vec<_> = open_proposals(&a)
        .await
        .into_iter()
        .filter(|p| !before.contains(p))
        .collect();
    assert_eq!(open, vec![(newer, "pending_quorum".into(), head_of(&a))]);
}

/// The proposal Cloud lists under the held record's id was not created by
/// this machine (its device id differs): replacing the held publication must
/// never close it. The record is forgotten with a warning; the foreign
/// proposal stays open.
#[tokio::test]
async fn a_held_proposal_of_another_device_is_never_rejected() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Quorum held foreign").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, _b, _signer) = team_fleet(&stack, &alice, &ws.id, &endpoint).await;

    ok_json(&a, &["--yes", "member", "rm", "charlie"]).await;
    let first = ok_json(&a, &["sync", "--push"]).await;
    let id = first["result"]["transport"]["held"]["proposal_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{first}"))
        .to_owned();

    // Cloud now lists that proposal as another device's.
    proxy
        .foreign_proposals
        .store(true, std::sync::atomic::Ordering::SeqCst);
    std::fs::write(config_of(&a).join("AGENTS.md"), "while held\n").unwrap();
    let changed = ok_json(&a, &["sync", "--push"]).await;
    assert!(
        messages(&changed).contains("cloud_proposal_not_ours"),
        "{changed}"
    );
    assert!(
        !messages(&changed).contains("cloud_proposal_not_superseded"),
        "{changed}"
    );
    assert!(
        proxy.resolutions.lock().unwrap().is_empty(),
        "a proposal of another device must never be rejected: {:?}",
        proxy.resolutions.lock().unwrap()
    );
    let newer = changed["result"]["transport"]["held"]["proposal_id"]
        .as_str()
        .unwrap_or_else(|| panic!("{changed}"))
        .to_owned();
    assert_ne!(newer, id, "{changed}");
    let record: Value = serde_json::from_str(
        &std::fs::read_to_string(a.join("state/muster/cloud/held.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        record["proposal_id"],
        newer.as_str(),
        "the old record is forgotten"
    );

    // Both proposals are open in the real service.
    proxy
        .foreign_proposals
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let open: Vec<String> = open_proposals(&a).await.into_iter().map(|p| p.0).collect();
    assert!(open.contains(&id) && open.contains(&newer), "{open:?}");
}
