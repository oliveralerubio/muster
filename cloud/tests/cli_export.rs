//! The Cloud OSS exit, over the real CLI and the real service: a linked and
//! synced machine downloads a full verified export (`muster cloud export`),
//! leaves Cloud (`cloud unlink`), and a fresh machine restores the fleet from
//! that exported bundle using only existing open-source commands (a Git
//! remote join from the bundle path, enrollment and approval over plain Git
//! remotes), then keeps working locally — including secret access. Refusals
//! never write a file; exports work with or without a local Cloud link when a
//! workspace is named.

mod support;

use axum::response::IntoResponse;
use muster_cloud::protocol::{body_sha256_hex, DeviceApproveRequest, DeviceDecision};
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};

use support::stack;

/// A fresh home directory for one machine.
fn new_home() -> std::path::PathBuf {
    let home = std::env::temp_dir().join(format!("muster-cli-export-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&home).unwrap();
    home
}

/// Runs the CLI binary for one machine HOME with a clean environment.
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

/// Runs the CLI with `value` on stdin (secrets, recovery keys: never argv).
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

/// stdout+stderr as one string: every channel a token or a refusal must not leak through.
fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// No account or device token may appear on any output channel.
fn assert_no_tokens(out: &std::process::Output) {
    let shown = text(out);
    assert!(!shown.contains("mca_"), "account token leaked: {shown}");
    assert!(!shown.contains("mcd_"), "device token leaked: {shown}");
}

/// The CLI's JSON envelope (asserting success first).
fn envelope(out: &std::process::Output) -> serde_json::Value {
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    serde_json::from_slice(&out.stdout).expect("JSON envelope")
}

fn git_run(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// Completes a real device-flow login for one machine HOME, approved through
/// `alice`'s browser session, through an explicit endpoint.
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
    assert!(
        !instruction.contains("mca_") && !instruction.contains("mcd_"),
        "{instruction}"
    );
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
    assert_no_tokens(&output);
}

async fn device_login(
    stack: &support::Stack,
    alice: &support::UserSession,
    home: &std::path::Path,
) {
    device_login_at(stack, alice, home, &stack.cloud.base).await;
}

/// Creates a workspace through the service API (setup, not the behavior under test).
async fn create_workspace(
    stack: &support::Stack,
    user: &support::UserSession,
    name: &str,
) -> muster_cloud::protocol::Workspace {
    let created = support::http()
        .post(stack.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &user.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(created.status(), 201);
    created.json().await.unwrap()
}

/// The accepted revision recorded by a machine's last verified push.
fn accepted_revision_of(home: &std::path::Path) -> String {
    let accepted: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    accepted["revision"].as_str().unwrap().to_owned()
}

/// The fleet state hash a joining machine must name (`--expect-state`), read
/// over the public CLI on the trusted machine.
async fn expected_state_of(home: &std::path::Path) -> String {
    let show = run_cli(home, &["--format", "json", "trust", "show"]).await;
    envelope(&show)["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Adds a secret on `home` through stdin (never argv).
async fn add_secret(home: &std::path::Path, name: &str, value: &str) {
    let add = run_cli_stdin(
        home,
        &["--format", "json", "secret", "add", name, "--value-stdin"],
        value.as_bytes(),
    )
    .await;
    assert!(
        add.status.success(),
        "{} {}",
        text(&add),
        String::from_utf8_lossy(&add.stderr)
    );
}

/// Reads `native://<name>` on `home`, asserting the exact plaintext.
async fn assert_secret_reads(home: &std::path::Path, name: &str, expected: &str) {
    let got = run_cli(
        home,
        &[
            "--format",
            "json",
            "secret",
            "get",
            &format!("native://{name}"),
            "--reveal",
            "--yes",
        ],
    )
    .await;
    assert!(
        got.status.success(),
        "{} {}",
        text(&got),
        String::from_utf8_lossy(&got.stderr)
    );
    let envelope = envelope(&got);
    assert_eq!(
        envelope["result"], expected,
        "the restored machine reads the same secret value: {envelope}"
    );
}

/// The full exit journey: a linked machine publishes a fleet WITH a secret,
/// exports a verified bundle, unlinks, and a FRESH machine (no init, no
/// Cloud link) restores from that bundle with existing OSS commands only,
/// enrolls over plain Git remotes, applies the same configuration, verifies
/// trust, reads the secret, and commits locally.
#[tokio::test]
async fn export_verifies_manifest_and_restores_an_offline_fleet() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Exit fleet").await;

    // Machine A bootstraps the fleet and publishes it over Cloud.
    let home = new_home();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    let link = run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws.id],
    )
    .await;
    assert!(link.status.success(), "{}", text(&link));
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // A fleet secret the exported bundle must carry.
    add_secret(&home, "app/token", "restored-value").await;
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());

    // Trust anchors, read over the public CLI before leaving Cloud.
    let show = run_cli(&home, &["--format", "json", "trust", "show"]).await;
    let shown = envelope(&show);
    let signer = shown["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_owned();
    let state_hash = shown["result"]["trust_pin"]["payload_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    let epoch = shown["result"]["fleet_state"]["epoch"].as_u64().unwrap();
    let accepted = accepted_revision_of(&home);

    // The export: a verified bundle plus its manifest, written privately.
    let bundle = home.join("export").join("fleet.bundle");
    let export = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            bundle.to_str().unwrap(),
        ],
    )
    .await;
    assert_no_tokens(&export);
    let result = envelope(&export)["result"].clone();
    assert_eq!(result["revision"], accepted.as_str(), "{result}");
    assert_eq!(
        result["manifest"]["revision"],
        accepted.as_str(),
        "{result}"
    );
    assert_eq!(result["manifest"]["signer"], signer.as_str(), "{result}");
    assert_eq!(result["manifest"]["epoch"], epoch, "{result}");
    assert_eq!(
        result["path"],
        bundle.display().to_string(),
        "the reported path is the file that was written: {result}"
    );
    let bytes = std::fs::read(&bundle).expect("the export bundle was written");
    assert_eq!(
        result["manifest"]["sha256"],
        body_sha256_hex(&bytes),
        "the manifest hash describes the exact bytes on disk"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&bundle).unwrap().permissions().mode() & 0o777,
            0o600,
            "the export bundle is private"
        );
    }

    // A target that is a directory or a symlink is refused, unchanged.
    let dir_target = home.join("dir-target");
    std::fs::create_dir(&dir_target).unwrap();
    let refused = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            dir_target.to_str().unwrap(),
        ],
    )
    .await;
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(dir_target.is_dir() && dir_target.read_dir().unwrap().next().is_none());
    let symlink_target = home.join("symlink-target.bundle");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&bundle, &symlink_target).unwrap();
    #[cfg(not(unix))]
    std::fs::copy(&bundle, &symlink_target).unwrap();
    let refused = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            symlink_target.to_str().unwrap(),
        ],
    )
    .await;
    assert!(!refused.status.success(), "{}", text(&refused));
    #[cfg(unix)]
    {
        let link = std::fs::read_link(&symlink_target).expect("the symlink survives");
        assert_eq!(link, bundle, "the symlink was not followed");
    }

    // A dash-prefixed directory in the path never confuses git.
    let dash_bundle = home.join("-dash-dir").join("out.bundle");
    let dash_arg = format!("--output={}", dash_bundle.display());
    let dashed = run_cli(
        &home,
        &["--format", "json", "cloud", "export", dash_arg.as_str()],
    )
    .await;
    assert!(dashed.status.success(), "{}", text(&dashed));
    assert!(dash_bundle.is_file(), "the dash-prefixed path was written");

    // The same file restores offline with plain Git (existing OSS commands).
    let scratch = home.join("oss-check");
    std::fs::create_dir(&scratch).unwrap();
    git_run(&scratch, &["init", "--quiet"]);
    git_run(
        &scratch,
        &[
            "fetch",
            "--quiet",
            bundle.to_str().unwrap(),
            "refs/heads/main:refs/heads/verified",
        ],
    );
    let restored =
        String::from_utf8_lossy(&git_run(&scratch, &["rev-parse", "refs/heads/verified"]).stdout)
            .trim()
            .to_owned();
    assert_eq!(restored, accepted, "the bundle tip is the accepted head");

    // Machine A leaves Cloud: the link goes, the fleet and keys stay.
    let unlink = run_cli(&home, &["--format", "json", "cloud", "unlink"]).await;
    assert!(unlink.status.success(), "{}", text(&unlink));
    assert!(!home.join("state/muster/cloud/link.json").exists());

    // A FRESH machine restores from the exported bundle: no init, no link —
    // the join creates the repo on the fleet's branch and pins the fleet it
    // named with `--expect-state`.
    let fresh = new_home();
    let join = run_cli(
        &fresh,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            bundle.to_str().unwrap(),
            "--expect-state",
            &state_hash,
        ],
    )
    .await;
    assert!(join.status.success(), "{}", text(&join));
    let joined = envelope(&join)["result"].clone();
    assert_eq!(joined["joined"], true, "{joined}");
    assert_eq!(joined["applied"], false, "{joined}");
    let manifest = std::fs::read_to_string(fresh.join("config/muster/muster.toml")).unwrap();
    assert!(
        manifest.contains("[machines.laptop]"),
        "the bundle restored the fleet manifest: {manifest}"
    );

    // Detach from the read-only bundle: from here on the machine is OSS-only.
    git_run(
        &fresh.join("config/muster"),
        &["remote", "remove", "origin"],
    );
    let enroll = run_cli(&fresh, &["--format", "json", "enroll", "--name", "laptop3"]).await;
    assert!(enroll.status.success(), "{}", text(&enroll));
    assert_no_tokens(&enroll);
    assert_eq!(envelope(&enroll)["result"]["status"], "pending");

    // The request travels over plain Git: the author pulls it from the
    // restored machine's repository and approves LOCALLY with its own key
    // (the approval rekeys the vault group for the new machine).
    let pull_request = run_cli(
        &home,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", fresh.join("config/muster").display()),
        ],
    )
    .await;
    assert!(pull_request.status.success(), "{}", text(&pull_request));
    git_run(&home.join("config/muster"), &["remote", "remove", "origin"]);
    let approve = run_cli(&home, &["--format", "json", "--yes", "approve", "laptop3"]).await;
    assert!(approve.status.success(), "{}", text(&approve));
    assert_eq!(
        envelope(&approve)["result"]["status"],
        "active",
        "approve result: {}",
        text(&approve)
    );

    // The restored machine fetches the approval and applies the same
    // configuration locally; the signature chain is verified against its pin,
    // and the approved secret is readable for real.
    let apply = run_cli(
        &fresh,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            &format!("file://{}", home.join("config/muster").display()),
        ],
    )
    .await;
    assert!(apply.status.success(), "{}", text(&apply));
    assert_no_tokens(&apply);
    let applied = envelope(&apply)["result"].clone();
    assert_eq!(applied["applied"], true, "{applied}");
    let manifest = std::fs::read_to_string(fresh.join("config/muster/muster.toml")).unwrap();
    assert!(
        manifest.contains("[machines.laptop3]"),
        "the approval enrolled the restored machine: {manifest}"
    );
    let log_output = git_run(&fresh.join("config/muster"), &["log", "--oneline"]);
    let log = String::from_utf8_lossy(&log_output.stdout);
    assert!(
        log.contains("laptop3 applied the approved profile"),
        "the applied-profile commit is a local commit on the restored machine: {log}"
    );
    assert_secret_reads(&fresh, "app/token", "restored-value").await;

    // Plain local `muster sync` keeps working with no remote and no Cloud.
    git_run(
        &fresh.join("config/muster"),
        &["remote", "remove", "origin"],
    );
    let local = run_cli(&fresh, &["--format", "json", "sync"]).await;
    assert!(local.status.success(), "{}", text(&local));
    let verified = run_cli(&fresh, &["--format", "json", "trust", "show"]).await;
    let verified = envelope(&verified);
    assert_eq!(
        verified["result"]["fleet_state"]["signer"],
        signer.as_str(),
        "the restored fleet verifies against the same signer: {verified}"
    );
    assert!(verified["result"]["trust_pin"]["payload_hash"].is_string());

    std::fs::remove_dir_all(&home).unwrap();
    std::fs::remove_dir_all(&fresh).unwrap();
}

/// A workspace the account cannot read is refused honestly (404) and an
/// empty workspace is refused honestly (204): in neither case is a file
/// written, and no token appears anywhere.
#[tokio::test]
async fn export_refuses_unreadable_or_empty_workspaces_without_writing() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let bob = stack.login_user("bob", "bob@example.com").await;
    let empty = create_workspace(&stack, &alice, "Empty export fleet").await;
    let foreign = create_workspace(&stack, &bob, "Bob private fleet").await;

    let home = new_home();
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
            &empty.id,
        ],
    )
    .await;
    assert!(link.status.success(), "{}", text(&link));

    // Another user's workspace: the service answers 404 (membership is not
    // disclosed), the CLI reports the honest HTTP status and writes nothing.
    let target = home.join("foreign.bundle");
    let refused = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--workspace",
            &foreign.id,
            "--output",
            target.to_str().unwrap(),
        ],
    )
    .await;
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("404"),
        "the refusal names the honest status: {}",
        text(&refused)
    );
    assert!(
        text(&refused).contains("no export was written"),
        "{}",
        text(&refused)
    );
    assert_no_tokens(&refused);
    assert!(!target.exists(), "no file may be written for a refusal");

    // An own workspace with no accepted history: 204, still no file.
    let empty_target = home.join("empty.bundle");
    let nothing = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--workspace",
            &empty.id,
            "--output",
            empty_target.to_str().unwrap(),
        ],
    )
    .await;
    assert!(!nothing.status.success(), "{}", text(&nothing));
    assert!(
        text(&nothing).contains("no accepted history"),
        "{}",
        text(&nothing)
    );
    assert_no_tokens(&nothing);
    assert!(
        !empty_target.exists(),
        "no file may be written for a refusal"
    );

    std::fs::remove_dir_all(&home).unwrap();
}

/// The export endpoint needs only the ACCOUNT: `--workspace` works on a
/// login-only machine and on a machine that already left Cloud (unlinked).
/// Only the default (linked) workspace requires the link.
#[tokio::test]
async fn export_by_workspace_works_without_a_link() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Unlinked export fleet").await;

    // Machine A bootstraps and publishes; machine B only logs in.
    let home = new_home();
    device_login(&stack, &alice, &home).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    let link = run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws.id],
    )
    .await;
    assert!(link.status.success(), "{}", text(&link));
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let accepted = accepted_revision_of(&home);

    let login_only = new_home();
    device_login(&stack, &alice, &login_only).await;
    assert!(
        !login_only.join("state/muster/cloud/link.json").exists(),
        "precondition: no Cloud link"
    );
    let from_login_only = login_only.join("no-link.bundle");
    let export = run_cli(
        &login_only,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--workspace",
            &ws.id,
            "--output",
            from_login_only.to_str().unwrap(),
        ],
    )
    .await;
    assert_no_tokens(&export);
    assert_eq!(
        envelope(&export)["result"]["revision"],
        accepted.as_str(),
        "a login-only machine exports a named workspace"
    );
    assert_eq!(
        envelope(&export)["result"]["manifest"]["sha256"],
        body_sha256_hex(&std::fs::read(&from_login_only).unwrap()),
    );

    // After leaving Cloud, the same still works — but the default workspace
    // (no --workspace) honestly requires the link it used to read.
    let after_unlink = home.join("after-unlink.bundle");
    assert!(run_cli(&home, &["--format", "json", "cloud", "unlink"])
        .await
        .status
        .success());
    let export = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--workspace",
            &ws.id,
            "--output",
            after_unlink.to_str().unwrap(),
        ],
    )
    .await;
    assert_no_tokens(&export);
    assert_eq!(
        envelope(&export)["result"]["revision"],
        accepted.as_str(),
        "an unlinked machine exports a named workspace"
    );
    let default_refused = run_cli(&home, &["--format", "json", "cloud", "export"]).await;
    assert!(
        !default_refused.status.success(),
        "{}",
        text(&default_refused)
    );
    assert!(
        text(&default_refused).contains("no Cloud link"),
        "{}",
        text(&default_refused)
    );

    std::fs::remove_dir_all(&home).unwrap();
    std::fs::remove_dir_all(&login_only).unwrap();
}

/// Proxy in front of the service with injectable export tampering.
#[derive(Clone)]
struct ExportProxy {
    upstream: String,
    client: reqwest::Client,
    /// Corrupt the bundle body so it no longer matches the manifest hash.
    corrupt_body: Arc<std::sync::atomic::AtomicBool>,
    /// Replace the manifest revision (and the revision header) with another
    /// canonical-looking hash that does not match the bundle tip.
    fake_revision: Arc<std::sync::Mutex<Option<String>>>,
    /// Prepend a decoy ref (`refs/0decoy/refs/heads/main`) to the bundle
    /// header: it sorts before the real ref and tail-matches the pattern, so
    /// only exact refname parsing stays correct.
    decoy_ref: Arc<std::sync::atomic::AtomicBool>,
}

async fn export_proxy_handler(
    axum::extract::State(proxy): axum::extract::State<ExportProxy>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    use muster_cloud::protocol::{HEADER_EXPORT_MANIFEST, HEADER_REVISION};
    use std::sync::atomic::Ordering;
    let query = uri.query().map(|q| format!("?{q}")).unwrap_or_default();
    let url = format!("{}{}{query}", proxy.upstream, uri.path());
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
    let mut bytes: Vec<u8> = response.bytes().await.expect("upstream body").to_vec();
    let is_export = method == axum::http::Method::GET && uri.path().ends_with("/export");
    let mut decoy_manifest: Option<serde_json::Value> = None;
    if is_export && status.is_success() && proxy.corrupt_body.load(Ordering::Relaxed) {
        if let Some(last) = bytes.last_mut() {
            *last ^= 0xff;
        }
    }
    if is_export && status.is_success() && proxy.decoy_ref.load(Ordering::Relaxed) {
        let magic = b"# v2 git bundle\n";
        let served_revision = response_headers
            .get(HEADER_REVISION)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        if bytes.starts_with(magic) && !served_revision.is_empty() {
            let mut tampered = Vec::with_capacity(bytes.len() + served_revision.len() + 64);
            tampered.extend_from_slice(magic);
            tampered.extend(std::iter::repeat_n(b'b', served_revision.len()));
            tampered.extend_from_slice(b" refs/0decoy/refs/heads/main\n");
            tampered.extend_from_slice(&bytes[magic.len()..]);
            // The manifest stays honest about the tampered body (hash and
            // revision describe exactly these bytes): the only fault is the
            // decoy ref listed first in the header.
            let manifest = response_headers
                .get(HEADER_EXPORT_MANIFEST)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
                .expect("a manifest to tamper with");
            let mut manifest = manifest;
            manifest["sha256"] =
                serde_json::Value::String(muster_cloud::protocol::body_sha256_hex(&tampered));
            bytes = tampered;
            decoy_manifest = Some(manifest);
        }
    }
    let fake_revision = if is_export {
        proxy.fake_revision.lock().unwrap().clone()
    } else {
        None
    };
    let mut out = axum::response::Response::builder().status(status);
    for (name, value) in &response_headers {
        if matches!(
            name.as_str(),
            "connection" | "transfer-encoding" | "content-length"
        ) {
            continue;
        }
        if fake_revision.is_some()
            && matches!(name.as_str(), HEADER_REVISION | HEADER_EXPORT_MANIFEST)
        {
            continue;
        }
        if decoy_manifest.is_some() && name.as_str() == HEADER_EXPORT_MANIFEST {
            continue;
        }
        out = out.header(name, value);
    }
    if let Some(rev) = fake_revision {
        let manifest = response_headers
            .get(HEADER_EXPORT_MANIFEST)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| serde_json::from_str::<serde_json::Value>(v).ok())
            .expect("a manifest to tamper with");
        let mut tampered = manifest;
        tampered["revision"] = serde_json::Value::String(rev.clone());
        out = out
            .header(
                HEADER_REVISION,
                axum::http::HeaderValue::from_str(&rev).unwrap(),
            )
            .header(
                HEADER_EXPORT_MANIFEST,
                axum::http::HeaderValue::from_str(&tampered.to_string()).unwrap(),
            );
    }
    if let Some(manifest) = decoy_manifest {
        out = out.header(
            HEADER_EXPORT_MANIFEST,
            axum::http::HeaderValue::from_str(&manifest.to_string()).unwrap(),
        );
    }
    out.body(axum::body::Body::from(bytes)).unwrap()
}

/// A running [`ExportProxy`]: its base URL and fault controls.
struct ExportProxyHandle {
    url: String,
    corrupt_body: Arc<std::sync::atomic::AtomicBool>,
    fake_revision: Arc<std::sync::Mutex<Option<String>>>,
    decoy_ref: Arc<std::sync::atomic::AtomicBool>,
    _task: tokio::task::JoinHandle<()>,
}

async fn start_export_proxy(upstream: String) -> ExportProxyHandle {
    use std::sync::atomic::AtomicBool;
    let state = ExportProxy {
        upstream,
        client: support::http(),
        corrupt_body: Arc::new(AtomicBool::new(false)),
        fake_revision: Arc::new(std::sync::Mutex::new(None)),
        decoy_ref: Arc::new(AtomicBool::new(false)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind export proxy");
    let port = listener.local_addr().unwrap().port();
    let app = axum::Router::new()
        .fallback(export_proxy_handler)
        .with_state(state.clone());
    let task = tokio::spawn(async move {
        axum::serve(listener, app.into_make_service()).await.ok();
    });
    ExportProxyHandle {
        url: format!("http://127.0.0.1:{port}"),
        corrupt_body: state.corrupt_body,
        fake_revision: state.fake_revision,
        decoy_ref: state.decoy_ref,
        _task: task,
    }
}

/// A tampered export must never reach the disk: a body that does not match
/// the manifest hash is refused, a manifest naming another revision than the
/// bundle tip is refused — both leave no file. A bundle whose header lists a
/// decoy ref first (tail-matching `refs/heads/main`) is accepted because the
/// check parses every ref name exactly. An intact export through the same
/// proxy still succeeds.
#[tokio::test]
async fn export_refuses_a_tampered_export_and_parses_refs_exactly() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Tampered export fleet").await;
    let proxy = start_export_proxy(stack.cloud.base.clone()).await;

    // Machine A links and publishes THROUGH the proxy, so its export goes
    // through the same wire the faults are injected on.
    let home = new_home();
    device_login_at(&stack, &alice, &home, &proxy.url).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", "laptop"])
        .await
        .status
        .success());
    let link = run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws.id],
    )
    .await;
    assert!(link.status.success(), "{}", text(&link));
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let accepted = accepted_revision_of(&home);

    // Tampered body: the bundle no longer matches its manifest sha256.
    let tampered_body = home.join("tampered-body.bundle");
    proxy
        .corrupt_body
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let refused = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            tampered_body.to_str().unwrap(),
        ],
    )
    .await;
    proxy
        .corrupt_body
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("sha256"),
        "the refusal names the failed hash check: {}",
        text(&refused)
    );
    assert!(
        text(&refused).contains("no export was written"),
        "{}",
        text(&refused)
    );
    assert_no_tokens(&refused);
    assert!(!tampered_body.exists(), "no file may be written");

    // Tampered manifest: the named revision does not match the bundle tip.
    let tampered_revision = home.join("tampered-revision.bundle");
    *proxy.fake_revision.lock().unwrap() = Some("a".repeat(64));
    let refused = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            tampered_revision.to_str().unwrap(),
        ],
    )
    .await;
    *proxy.fake_revision.lock().unwrap() = None;
    assert!(!refused.status.success(), "{}", text(&refused));
    assert!(
        text(&refused).contains("tip"),
        "the refusal names the failed tip check: {}",
        text(&refused)
    );
    assert!(
        text(&refused).contains("no export was written"),
        "{}",
        text(&refused)
    );
    assert!(!tampered_revision.exists(), "no file may be written");

    // Decoy ref listed first: only exact refname parsing accepts this.
    let with_decoy = home.join("with-decoy.bundle");
    proxy
        .decoy_ref
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let accepted_export = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            with_decoy.to_str().unwrap(),
        ],
    )
    .await;
    proxy
        .decoy_ref
        .store(false, std::sync::atomic::Ordering::Relaxed);
    assert_no_tokens(&accepted_export);
    assert_eq!(
        envelope(&accepted_export)["result"]["revision"],
        accepted.as_str(),
        "a decoy ref first must not fool the tip check: {}",
        text(&accepted_export)
    );
    assert!(
        with_decoy.is_file(),
        "the verified decoy bundle was written"
    );

    // Sanity: with no faults the same proxy serves a verified export.
    let intact = home.join("intact.bundle");
    let ok = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            intact.to_str().unwrap(),
        ],
    )
    .await;
    assert_no_tokens(&ok);
    assert_eq!(envelope(&ok)["result"]["revision"], accepted.as_str());
    assert!(intact.exists());

    std::fs::remove_dir_all(&home).unwrap();
}

/// The exit with NO surviving machine: the only artifacts are the exported
/// bundle and the offline recovery key. A fresh machine joins from the
/// bundle, enrolls, approves itself with `--recovery-key-stdin` (break-glass),
/// applies the fleet, and reads the secret the bundle carries.
#[tokio::test]
async fn recovery_key_restores_a_fleet_when_no_machine_survives() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "No survivors fleet").await;

    // Machine A bootstraps, publishes, and adds a secret.
    let home = new_home();
    device_login(&stack, &alice, &home).await;
    let init = run_cli(
        &home,
        &["--format", "json", "--yes", "init", "--machine", "laptop"],
    )
    .await;
    let recovery_key = envelope(&init)["result"]["recovery_key"]
        .as_str()
        .expect("non-interactive init returns the recovery key")
        .to_owned();
    let link = run_cli(
        &home,
        &["--format", "json", "cloud", "link", "--workspace", &ws.id],
    )
    .await;
    assert!(link.status.success(), "{}", text(&link));
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    add_secret(&home, "app/token", "break-glass-value").await;
    assert!(run_cli(&home, &["--format", "json", "sync", "--push"])
        .await
        .status
        .success());
    let state_hash = expected_state_of(&home).await;

    // The exit artifacts: one bundle (kept OUTSIDE the dying machine's home)
    // and one recovery key. A does not survive.
    let bundle = std::env::temp_dir().join(format!(
        "muster-export-recovery-{}.bundle",
        uuid::Uuid::new_v4()
    ));
    let export = run_cli(
        &home,
        &[
            "--format",
            "json",
            "cloud",
            "export",
            "--output",
            bundle.to_str().unwrap(),
        ],
    )
    .await;
    assert!(export.status.success(), "{}", text(&export));
    std::fs::remove_dir_all(&home).unwrap();

    // A fresh machine joins from the bundle alone.
    let fresh = new_home();
    let join = run_cli(
        &fresh,
        &[
            "--format",
            "json",
            "sync",
            "--pull",
            "--remote",
            bundle.to_str().unwrap(),
            "--expect-state",
            &state_hash,
        ],
    )
    .await;
    assert!(join.status.success(), "{}", text(&join));
    assert_eq!(envelope(&join)["result"]["joined"], true);
    git_run(
        &fresh.join("config/muster"),
        &["remote", "remove", "origin"],
    );
    let enroll = run_cli(&fresh, &["--format", "json", "enroll", "--name", "sole"]).await;
    assert!(enroll.status.success(), "{}", text(&enroll));

    // No fleet member exists to approve: the recovery key is the path.
    let approve = run_cli_stdin(
        &fresh,
        &[
            "--format",
            "json",
            "--yes",
            "approve",
            "sole",
            "--recovery-key-stdin",
        ],
        recovery_key.as_bytes(),
    )
    .await;
    assert!(
        approve.status.success(),
        "{} {}",
        text(&approve),
        String::from_utf8_lossy(&approve.stderr)
    );
    assert!(
        !text(&approve).contains(&recovery_key),
        "the key never prints"
    );
    assert_eq!(envelope(&approve)["result"]["status"], "active");

    // The fleet applies locally and the secret reads for real.
    let local = run_cli(&fresh, &["--format", "json", "sync"]).await;
    assert!(local.status.success(), "{}", text(&local));
    assert_secret_reads(&fresh, "app/token", "break-glass-value").await;

    std::fs::remove_dir_all(&fresh).unwrap();
    std::fs::remove_file(&bundle).unwrap();
}
