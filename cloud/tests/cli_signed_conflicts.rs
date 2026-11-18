//! Two machines seal the same fleet epoch concurrently: the one whose push
//! lost the race recovers with `sync --pull` alone. Its abandoned commits are
//! kept under a backup ref, its unpublished signed operations are re-applied
//! and re-sealed on top of the Cloud-accepted state, and a same-key collision
//! is reported honestly without leaving the machine stuck. A server that
//! serves a fork of a state Cloud already accepted is still refused. Real
//! `muster` binary against the real service over HTTP (a forwarding proxy
//! plays the malicious server); nothing is mocked and no row is asserted.
mod support;

use axum::response::IntoResponse;
use muster_cloud::protocol::{DeviceApproveRequest, DeviceDecision};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, BufReader};

use support::stack;

fn cli_command(home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(
        std::env::var("MUSTER_CLI_BIN").expect("MUSTER_CLI_BIN must point to the built CLI"),
    );
    command
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join("config"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_DATA_HOME", home.join("data"))
        .env("XDG_CACHE_HOME", home.join("cache"));
    command
}

async fn run_cli(home: &Path, args: &[&str]) -> std::process::Output {
    cli_command(home).args(args).output().await.unwrap()
}

async fn run_cli_stdin(home: &Path, args: &[&str], value: &[u8]) -> std::process::Output {
    use tokio::io::AsyncWriteExt;
    let mut child = cli_command(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(value).await.unwrap();
        stdin.shutdown().await.unwrap();
    }
    child.wait_with_output().await.unwrap()
}

fn text(output: &std::process::Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Runs a command that must succeed and returns its JSON envelope.
async fn ok_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let output = run_cli(home, &full).await;
    assert!(output.status.success(), "{args:?}: {}", text(&output));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Runs a command that must fail and returns its exit code and JSON envelope.
async fn failing_json(home: &Path, args: &[&str]) -> (i32, Value) {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let output = run_cli(home, &full).await;
    assert!(
        !output.status.success(),
        "{args:?} must fail: {}",
        text(&output)
    );
    let envelope = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{args:?}: stdout is not a JSON envelope ({e}): {}",
            text(&output)
        )
    });
    (output.status.code().unwrap_or(-1), envelope)
}

fn diagnostics_text(envelope: &Value) -> String {
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

fn has_diagnostic(envelope: &Value, code: &str) -> bool {
    envelope["diagnostics"]
        .as_array()
        .is_some_and(|all| all.iter().any(|d| d["code"] == code))
}

async fn device_login(
    stack: &support::Stack,
    user: &support::UserSession,
    home: &Path,
    endpoint: &str,
) {
    let mut child = cli_command(home)
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
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &user.csrf_token)
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

fn new_home() -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "muster-cli-signed-conflicts-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir(&home).unwrap();
    home
}

fn config_of(home: &Path) -> PathBuf {
    home.join("config/muster")
}

fn git_out(home: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(config_of(home))
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
    git_out(home, &["rev-parse", "HEAD"])
}

fn accepted_revision_of(home: &Path) -> String {
    let accepted: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    accepted["revision"].as_str().unwrap().to_owned()
}

/// Every backup ref of abandoned local history, as `<commit> <refname>`.
fn abandoned_refs(home: &Path) -> Vec<(String, String)> {
    git_out(
        home,
        &[
            "for-each-ref",
            "--format=%(objectname) %(refname)",
            "refs/muster/abandoned/",
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(|l| {
        let (commit, name) = l.split_once(' ').unwrap();
        (commit.to_owned(), name.to_owned())
    })
    .collect()
}

/// The machine's trust pin and the signed state in its repository.
async fn trust(home: &Path) -> (u64, String, u64, String) {
    let show = ok_json(home, &["trust", "show"]).await;
    let result = &show["result"];
    assert_eq!(result["status"], "verified", "{show}");
    (
        result["trust_pin"]["epoch"].as_u64().unwrap(),
        result["trust_pin"]["payload_hash"]
            .as_str()
            .unwrap()
            .to_owned(),
        result["fleet_state"]["epoch"].as_u64().unwrap_or_default(),
        result["fleet_state"]["signer"].as_str().unwrap().to_owned(),
    )
}

async fn add_secret(home: &Path, reference: &str, value: &str) {
    let add = run_cli_stdin(
        home,
        &[
            "--format",
            "json",
            "secret",
            "add",
            reference,
            "--value-stdin",
        ],
        value.as_bytes(),
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
}

async fn rotate_secret(home: &Path, reference: &str, value: &str) {
    let rotate = run_cli_stdin(
        home,
        &[
            "--format",
            "json",
            "secret",
            "rotate",
            reference,
            "--value-stdin",
        ],
        value.as_bytes(),
    )
    .await;
    assert!(rotate.status.success(), "{}", text(&rotate));
}

async fn secret_absent(home: &Path, reference: &str) {
    let native = format!("native://{reference}");
    let got = run_cli(
        home,
        &[
            "--format", "json", "--yes", "secret", "get", &native, "--reveal",
        ],
    )
    .await;
    assert!(
        !got.status.success(),
        "{reference} must be gone: {}",
        text(&got)
    );
}

fn git_env(home: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(config_of(home))
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

/// A refused pull: an error, and nothing about the machine moved.
async fn assert_refused_untouched(home: &Path) -> Value {
    let before = (head_of(home), trust(home).await, accepted_revision_of(home));
    let (_, refused) = failing_json(home, &["sync", "--pull"]).await;
    assert!(abandoned_refs(home).is_empty(), "{refused}");
    assert_eq!(
        (head_of(home), trust(home).await, accepted_revision_of(home)),
        before,
        "{refused}"
    );
    refused
}

async fn secret_value(home: &Path, reference: &str) -> String {
    let native = format!("native://{reference}");
    let got = ok_json(home, &["--yes", "secret", "get", &native, "--reveal"]).await;
    got["result"]
        .as_str()
        .unwrap_or_else(|| panic!("no value for {reference}: {got}"))
        .to_owned()
}

/// Machine setup: login, init and link `home` to `workspace`.
async fn join_workspace(
    stack: &support::Stack,
    user: &support::UserSession,
    workspace: &str,
    machine: &str,
    endpoint: &str,
) -> PathBuf {
    let home = new_home();
    device_login(stack, user, &home, endpoint).await;
    assert!(run_cli(&home, &["--yes", "init", "--machine", machine])
        .await
        .status
        .success());
    ok_json(&home, &["cloud", "link", "--workspace", workspace]).await;
    home
}

async fn fleet_signer(home: &Path) -> String {
    let show = ok_json(home, &["trust", "show"]).await;
    show["result"]["fleet_state"]["signer"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Machine A bootstraps and publishes the fleet; machine B joins it, enrolls,
/// is approved by A and pulls the approval. Both are writers converged on the
/// same accepted head. Returns `(a, b)`.
async fn two_writers(
    stack: &support::Stack,
    user: &support::UserSession,
    workspace: &str,
    endpoint: &str,
) -> (PathBuf, PathBuf) {
    let a = join_workspace(stack, user, workspace, "laptop", endpoint).await;
    ok_json(&a, &["sync", "--push"]).await;
    let signer = fleet_signer(&a).await;
    let b = join_workspace(stack, user, workspace, "second", endpoint).await;
    ok_json(&b, &["sync", "--pull", "--expect-signer", &signer]).await;
    ok_json(&b, &["enroll", "--name", "second"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(run_cli(&a, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(&a), accepted_revision_of(&b));
    (a, b)
}

/// B's push must lose the race with an honest `conflict`.
async fn losing_push(b: &Path) {
    let (code, push) = failing_json(b, &["sync", "--push"]).await;
    assert_eq!(code, 1, "{push}");
    assert!(has_diagnostic(&push, "revision_conflict"), "{push}");
}

/// After a recovery pull B publishes and A pulls: both machines hold the same
/// accepted head and the same pinned state, one linear chain.
async fn publish_and_converge(a: &Path, b: &Path) {
    ok_json(b, &["sync", "--push"]).await;
    ok_json(a, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(a), accepted_revision_of(b));
    let (a_epoch, a_hash, _, _) = trust(a).await;
    let (b_epoch, b_hash, _, _) = trust(b).await;
    assert_eq!((a_epoch, &a_hash), (b_epoch, &b_hash));
}

/// The recovery pull itself: B is rebuilt on top of the accepted head, its
/// abandoned commit is kept under a backup ref, and its new state is the next
/// epoch after the accepted one.
async fn recover(a: &Path, b: &Path) -> Value {
    let abandoned = head_of(b);
    let (accepted_epoch, _, _, _) = trust(a).await;
    let pull = ok_json(b, &["sync", "--pull"]).await;
    assert_eq!(
        pull["result"]["transport"]["pulled"], "reconciled",
        "{pull}"
    );
    assert!(has_diagnostic(&pull, "signed_fork_reconciled"), "{pull}");
    let refs = abandoned_refs(b);
    assert!(
        refs.iter().any(|(commit, _)| *commit == abandoned),
        "the abandoned commit {abandoned} must be kept: {refs:?}"
    );
    let accepted = accepted_revision_of(b);
    assert_eq!(
        accepted,
        head_of(a),
        "B records A's publication as accepted"
    );
    let resealed = git_out(b, &["log", "-1", "--format=%H", "--", "fleet.state.json"]);
    assert_eq!(
        git_out(b, &["rev-parse", &format!("{resealed}~1")]),
        accepted,
        "the re-sealed change sits directly on the accepted head"
    );
    let (pin_epoch, _, state_epoch, _) = trust(b).await;
    assert_eq!(state_epoch, accepted_epoch + 1, "{pull}");
    assert_eq!(pin_epoch, state_epoch);
    pull
}

#[tokio::test]
async fn different_secrets_in_different_groups_are_both_kept() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork groups").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;

    recover(&a, &b).await;
    assert_eq!(secret_value(&b, "app/token").await, "from-a");
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "app/token").await, "from-a");
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
    // A later ordinary change still publishes from either side.
    rotate_secret(&a, "ops/key", "rotated-by-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(secret_value(&b, "ops/key").await, "rotated-by-a");
}

#[tokio::test]
async fn different_secrets_in_the_same_group_are_both_kept() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork same group").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    // The group exists on both sides before the race, so both rewrite the
    // same non-deterministic ciphertext and sidecar.
    add_secret(&a, "app/base", "shared").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "app/other", "from-b").await;
    rotate_secret(&b, "app/base", "rotated-by-b").await;
    losing_push(&b).await;

    recover(&a, &b).await;
    for home in [&b] {
        assert_eq!(secret_value(home, "app/token").await, "from-a");
        assert_eq!(secret_value(home, "app/other").await, "from-b");
        assert_eq!(secret_value(home, "app/base").await, "rotated-by-b");
    }
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "app/token").await, "from-a");
    assert_eq!(secret_value(&a, "app/other").await, "from-b");
    assert_eq!(secret_value(&a, "app/base").await, "rotated-by-b");
}

#[tokio::test]
async fn same_secret_with_different_values_is_an_honest_conflict_and_not_stuck() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork collision").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "app/token", "from-b").await;
    add_secret(&b, "app/extra", "extra-from-b").await;
    losing_push(&b).await;
    let abandoned = head_of(&b);

    // Nothing is guessed: the colliding key is named, B stays usable.
    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(
        messages.contains("app/token") && messages.contains("conflict"),
        "the collision must name the key: {messages}"
    );
    assert!(
        !messages.contains("from-b"),
        "values never leak: {messages}"
    );
    assert!(
        has_diagnostic(&pull, "signed_change_conflict"),
        "{messages}"
    );
    let status = ok_json(&b, &["status"]).await;
    assert_eq!(status["result"]["sync"]["result"], "conflict", "{status}");
    assert!(abandoned_refs(&b).iter().any(|(c, _)| *c == abandoned));
    assert_eq!(accepted_revision_of(&b), head_of(&a));

    // B is on the accepted state (A's value) plus its non-colliding change.
    assert_eq!(secret_value(&b, "app/token").await, "from-a");
    assert_eq!(secret_value(&b, "app/extra").await, "extra-from-b");
    let (pin_epoch, _, state_epoch, _) = trust(&b).await;
    assert_eq!(pin_epoch, state_epoch);

    // Not stuck: the next pull is ordinary, and keeping B's value is a
    // deliberate re-run of the command.
    let again = ok_json(&b, &["sync", "--pull"]).await;
    assert_ne!(again["result"]["transport"]["pulled"], "reconciled");
    rotate_secret(&b, "app/token", "from-b").await;
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "app/token").await, "from-b");
    assert_eq!(secret_value(&a, "app/extra").await, "extra-from-b");
}

#[tokio::test]
async fn non_signed_change_in_the_abandoned_commit_is_kept() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork with files").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;

    // One unpublished commit carries both a signed and a non-signed change.
    std::fs::write(
        config_of(&b).join("AGENTS.md"),
        "Instruction written on machine B.\n",
    )
    .unwrap();
    add_secret(&b, "ops/key", "from-b").await;
    let files = git_out(&b, &["show", "--name-only", "--format=", "HEAD"]);
    assert!(files.contains("AGENTS.md"), "{files}");
    assert!(files.contains("fleet.state.json"), "{files}");
    // And a raw local commit with a non-signed change on top of it.
    std::fs::write(config_of(&b).join("NOTES.md"), "notes from B\n").unwrap();
    git_out(&b, &["add", "NOTES.md"]);
    let commit = std::process::Command::new("git")
        .current_dir(config_of(&b))
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(["commit", "-qm", "raw local note"])
        .status()
        .unwrap();
    assert!(commit.success());
    losing_push(&b).await;

    recover(&a, &b).await;
    assert_eq!(
        std::fs::read_to_string(config_of(&b).join("AGENTS.md")).unwrap(),
        "Instruction written on machine B.\n"
    );
    assert_eq!(
        std::fs::read_to_string(config_of(&b).join("NOTES.md")).unwrap(),
        "notes from B\n"
    );
    assert_eq!(secret_value(&b, "app/token").await, "from-a");
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    publish_and_converge(&a, &b).await;
    assert_eq!(
        std::fs::read_to_string(config_of(&a).join("AGENTS.md")).unwrap(),
        "Instruction written on machine B.\n"
    );
    assert_eq!(
        std::fs::read_to_string(config_of(&a).join("NOTES.md")).unwrap(),
        "notes from B\n"
    );
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

// ---------------------------------------------------------------------------
// Malicious server: a forwarding proxy that can serve a captured bundle.
// ---------------------------------------------------------------------------

/// A bundle and the revision header to serve with it.
type Served = (Vec<u8>, String);

#[derive(Clone)]
struct Proxy {
    upstream: String,
    client: reqwest::Client,
    /// Body of the last `POST .../sync` (a publication bundle).
    last_publication: Arc<Mutex<Option<Vec<u8>>>>,
    /// When set, every `GET .../sync` is answered with this bundle and
    /// revision instead of the service's response.
    serve: Arc<Mutex<Option<Served>>>,
    /// When set, the next `POST .../sync` never reaches the service and is
    /// answered `502`: the publication's fate is unknown to the machine.
    drop_next_publication: Arc<std::sync::atomic::AtomicBool>,
    /// When set, the service really accepts the next `POST .../sync` but the
    /// `200` the machine sees names a different revision.
    skew_next_acceptance: Arc<std::sync::atomic::AtomicBool>,
}

async fn proxy_handler(
    axum::extract::State(proxy): axum::extract::State<Proxy>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    let path = uri.path().to_owned();
    let is_sync = path.ends_with("/sync");
    if method == axum::http::Method::GET && is_sync {
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
    if method == axum::http::Method::POST && is_sync {
        *proxy.last_publication.lock().unwrap() = Some(body.to_vec());
        if proxy
            .drop_next_publication
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return (axum::http::StatusCode::BAD_GATEWAY, "upstream timed out").into_response();
        }
    }
    let method_is_publication = method == axum::http::Method::POST && is_sync;
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
    let status = response.status();
    let mut bytes = response.bytes().await.expect("upstream body").to_vec();
    if method_is_publication
        && status == 200
        && proxy
            .skew_next_acceptance
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        let mut accepted: Value = serde_json::from_slice(&bytes).expect("accepted body");
        accepted["revision"] = Value::String("d".repeat(40));
        bytes = serde_json::to_vec(&accepted).unwrap();
    }
    out.body(axum::body::Body::from(bytes)).unwrap()
}

/// A full bundle whose `main` is `commit` of `home`'s repository.
fn bundle_of(home: &Path, commit: &str) -> Vec<u8> {
    let bare = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .current_dir(bare.path())
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    run(&["init", "--bare", "--quiet"]);
    run(&[
        "fetch",
        "--quiet",
        config_of(home).to_str().unwrap(),
        &format!("{commit}:refs/heads/main"),
    ]);
    let file = bare.path().join("out.bundle");
    run(&[
        "bundle",
        "create",
        file.to_str().unwrap(),
        "refs/heads/main",
    ]);
    std::fs::read(file).unwrap()
}

async fn start_proxy(upstream: String) -> (String, Proxy) {
    let state = Proxy {
        upstream,
        client: support::http(),
        last_publication: Arc::new(Mutex::new(None)),
        serve: Arc::new(Mutex::new(None)),
        drop_next_publication: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        skew_next_acceptance: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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

/// B wins the race, so Cloud accepted B's own seal. A server that then serves
/// A's losing fork to B must still be refused: the recovery rule only ever
/// lets a machine give up its OWN never-accepted state. A itself (whose seal
/// was never accepted) recovers normally afterwards.
#[tokio::test]
async fn fork_of_an_accepted_own_state_is_still_refused() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork malicious").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;

    add_secret(&b, "ops/key", "from-b").await;
    ok_json(&b, &["sync", "--push"]).await;
    let b_accepted = head_of(&b);
    assert_eq!(accepted_revision_of(&b), b_accepted);

    add_secret(&a, "app/token", "from-a").await;
    *proxy.last_publication.lock().unwrap() = None;
    losing_push(&a).await;
    let a_fork = proxy
        .last_publication
        .lock()
        .unwrap()
        .clone()
        .expect("A's publication bundle was captured");
    let a_head = head_of(&a);
    let b_pin = trust(&b).await;

    // The malicious server answers B with A's never-accepted fork.
    *proxy.serve.lock().unwrap() = Some((a_fork, a_head.clone()));
    let (_, refused) = failing_json(&b, &["sync", "--pull"]).await;
    let messages = diagnostics_text(&refused);
    assert!(
        messages.contains("differs from the one this machine accepted")
            || messages.contains("forked"),
        "{messages}"
    );
    assert_eq!(head_of(&b), b_accepted, "nothing was applied");
    assert_eq!(trust(&b).await, b_pin, "the pin did not move");
    assert_eq!(accepted_revision_of(&b), b_accepted);
    assert!(abandoned_refs(&b).is_empty());
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    // The honest server: A (whose seal Cloud never accepted) recovers, and
    // the fleet converges on one chain with both secrets.
    *proxy.serve.lock().unwrap() = None;
    recover(&b, &a).await;
    publish_and_converge(&b, &a).await;
    for home in [&a, &b] {
        assert_eq!(secret_value(home, "app/token").await, "from-a");
        assert_eq!(secret_value(home, "ops/key").await, "from-b");
    }

    // Replaying the stale fork afterwards is a plain rollback, refused on
    // both machines — even on A, whose own abandoned history it is.
    let stale = {
        let refs = abandoned_refs(&a);
        assert_eq!(refs.len(), 1, "{refs:?}");
        refs[0].0.clone()
    };
    assert_eq!(stale, a_head);
    for home in [&a, &b] {
        let before = (head_of(home), trust(home).await);
        *proxy.serve.lock().unwrap() = Some((bundle_of(&a, &stale), stale.clone()));
        let (_, refused) = failing_json(home, &["sync", "--pull"]).await;
        let messages = diagnostics_text(&refused);
        assert!(messages.contains("rollback refused"), "{messages}");
        assert_eq!((head_of(home), trust(home).await), before);
    }
    *proxy.serve.lock().unwrap() = None;
}

/// Cloud accepts the publication but answers `200` naming another revision.
/// The machine must not stay stuck replaying that cached answer: the pending
/// publication is dropped with a clear diagnostic, the commit stays in the
/// local history, and the next pull reconciles with what Cloud accepted.
#[tokio::test]
async fn acceptance_naming_another_revision_heals_on_the_next_pull() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed unexpected revision").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;
    let before = accepted_revision_of(&a);

    std::fs::write(config_of(&a).join("AGENTS.md"), "Published by A.\n").unwrap();
    proxy
        .skew_next_acceptance
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let (code, push) = failing_json(&a, &["sync", "--push"]).await;
    assert_eq!(code, 1, "{push}");
    assert!(
        has_diagnostic(&push, "cloud_publication_unconfirmed"),
        "{push}"
    );
    let committed = head_of(&a);
    assert_ne!(committed, before, "the local commit exists");
    assert!(
        !a.join("state/muster/cloud/push-intent.json").exists(),
        "the pending publication was dropped: {push}"
    );
    assert!(!a.join("state/muster/cloud/push-intent.bundle").exists());
    assert_eq!(accepted_revision_of(&a), before, "nothing was recorded");

    // The next pull reconciles; the commit was never lost.
    ok_json(&a, &["sync", "--pull"]).await;
    // (`sync` may add its own bookkeeping commit on top.)
    git_out(&a, &["merge-base", "--is-ancestor", &committed, "HEAD"]);
    assert_eq!(accepted_revision_of(&a), committed);
    let again = ok_json(&a, &["sync", "--push"]).await;
    assert_eq!(again["result"]["transport"]["pushed"], true, "{again}");

    // Cloud did accept it: the other machine receives it.
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(
        std::fs::read_to_string(config_of(&b).join("AGENTS.md")).unwrap(),
        "Published by A.\n"
    );
}

/// Only B sealed (A published a non-signed edit): the served tip carries the
/// state B had accepted before its own seal, so B's pull is a plain update
/// that keeps both changes.
#[tokio::test]
async fn own_seal_over_an_unsigned_publication_is_a_plain_update() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork one side").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    std::fs::write(
        config_of(&a).join("AGENTS.md"),
        "Instruction written on machine A.\n",
    )
    .unwrap();
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let pin_before = trust(&b).await;

    let pull = ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(pull["result"]["transport"]["pulled"], "updated", "{pull}");
    assert_eq!(trust(&b).await.1, pin_before.1, "B keeps its own seal");
    assert_eq!(
        std::fs::read_to_string(config_of(&b).join("AGENTS.md")).unwrap(),
        "Instruction written on machine A.\n"
    );
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

#[tokio::test]
async fn secret_removal_is_replayed_on_the_accepted_state() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork removal").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    add_secret(&a, "app/old", "old-value").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["--yes", "secret", "rm", "app/old"]).await;
    losing_push(&b).await;

    recover(&a, &b).await;
    secret_absent(&b, "app/old").await;
    assert_eq!(secret_value(&b, "app/token").await, "from-a");
    publish_and_converge(&a, &b).await;
    secret_absent(&a, "app/old").await;
    assert_eq!(secret_value(&a, "app/token").await, "from-a");
}

/// The accepted history added and removed the key this machine added: it
/// ends absent on both sides, yet replaying the add would resurrect what the
/// fleet deleted. Reported instead, and a deliberate re-run keeps it.
#[tokio::test]
async fn secret_deleted_in_accepted_history_is_not_resurrected() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork resurrection").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "ops/key", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&a, &["--yes", "secret", "rm", "ops/key"]).await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    add_secret(&b, "app/extra", "extra-from-b").await;
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(messages.contains("ops/key"), "{messages}");
    assert!(
        !messages.contains("from-b"),
        "values never leak: {messages}"
    );
    secret_absent(&b, "ops/key").await;
    assert_eq!(secret_value(&b, "app/extra").await, "extra-from-b");

    add_secret(&b, "ops/key", "from-b").await;
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
    assert_eq!(secret_value(&a, "app/extra").await, "extra-from-b");
}

#[tokio::test]
async fn secret_policy_changed_on_both_sides_is_an_honest_conflict() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork policy").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    add_secret(&a, "app/token", "shared").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;

    let allow = |agent: &'static str| {
        [
            "--yes",
            "secret",
            "allow",
            "native://app/token",
            "--entry",
            agent,
        ]
    };
    ok_json(&a, &allow("claude@alpha")).await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &allow("claude@beta")).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(
        messages.contains("policy") && messages.contains("app/token"),
        "{messages}"
    );
    let manifest = || std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    assert!(manifest().contains("claude@alpha") && !manifest().contains("claude@beta"));
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    ok_json(&b, &allow("claude@beta")).await;
    publish_and_converge(&a, &b).await;
    let on_a = std::fs::read_to_string(config_of(&a).join("muster.toml")).unwrap();
    assert!(on_a.contains("claude@beta"), "{on_a}");
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

/// A governance change lost in the race is never replayed (it would need its
/// own signatures), and the other `muster.toml` edits that came with it are
/// reported as dropped too, not silently lost.
#[tokio::test]
async fn governance_change_lost_in_a_race_is_an_honest_conflict() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork governance").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    let manifest_path = config_of(&b).join("muster.toml");
    let current = std::fs::read_to_string(&manifest_path).unwrap();
    std::fs::write(
        &manifest_path,
        format!("{current}\n[mcp.notes]\ncommand = \"notes-tool\"\n"),
    )
    .unwrap();
    ok_json(&b, &["member", "add", "worker", "--role", "member"]).await;
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(messages.contains("muster.toml governance"), "{messages}");
    assert!(
        messages.contains("other muster.toml edits"),
        "the edits made with it are reported: {messages}"
    );
    let manifest = std::fs::read_to_string(&manifest_path).unwrap();
    assert!(!manifest.contains("worker") && !manifest.contains("notes-tool"));
    assert_eq!(secret_value(&b, "app/token").await, "from-a");
    let (pin_epoch, _, state_epoch, _) = trust(&b).await;
    assert_eq!(pin_epoch, state_epoch);

    // Re-running the command on the accepted state publishes it.
    ok_json(&b, &["member", "add", "worker", "--role", "member"]).await;
    publish_and_converge(&a, &b).await;
    let on_a = std::fs::read_to_string(config_of(&a).join("muster.toml")).unwrap();
    assert!(on_a.contains("worker"), "{on_a}");
}

/// The process dies after the replay rewrote the working tree and before
/// the seal: the next command rolls the pull back on its own and the next
/// pull redoes it. Uses the debug-only fault hook (compiled out of release
/// builds).
#[tokio::test]
async fn crash_in_the_middle_of_a_reconcile_recovers_on_its_own() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork crash").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    std::fs::write(config_of(&b).join("NOTES.md"), "notes from B\n").unwrap();
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let abandoned = head_of(&b);
    let pin = trust(&b).await;

    let crashed = cli_command(&b)
        .env("MUSTER_TEST_PULL_CRASH", "after-replay")
        .args(["--format", "json", "sync", "--pull"])
        .output()
        .await
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", text(&crashed));
    assert!(b.join("state/muster/cloud/pull-journal.json").exists());

    // Any later command finds the machine as it was before the pull.
    let status = ok_json(&b, &["status"]).await;
    assert!(has_diagnostic(&status, "cloud_pull_recovered"), "{status}");
    assert!(!b.join("state/muster/cloud/pull-journal.json").exists());
    assert_eq!(head_of(&b), abandoned);
    assert_eq!(trust(&b).await, pin);
    assert_eq!(git_out(&b, &["status", "--porcelain"]), "");
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");

    recover(&a, &b).await;
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
    assert_eq!(
        std::fs::read_to_string(config_of(&a).join("NOTES.md")).unwrap(),
        "notes from B\n"
    );
}

/// B holds and pinned a seal of A's that Cloud never accepted (taken from A
/// through a side channel). Every other part of the rollback rule holds, but
/// the seal is not B's own: B never rebuilds it under its key.
#[tokio::test]
async fn foreign_unpublished_seal_is_never_rebuilt_with_this_machines_key() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork foreign seal").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    let fork_base = head_of(&a);
    let a_pin_file = a.join("state/muster/trust.json");
    let a_pin = std::fs::read(&a_pin_file).unwrap();

    add_secret(&a, "app/token", "from-a").await;
    let sealed_by_a = head_of(&a);
    git_out(
        &b,
        &[
            "fetch",
            "--quiet",
            config_of(&a).to_str().unwrap(),
            &sealed_by_a,
        ],
    );
    git_out(&b, &["reset", "--quiet", "--hard", &sealed_by_a]);
    ok_json(&b, &["status"]).await;
    assert_eq!(trust(&b).await.1, trust(&a).await.1, "B pinned A's seal");

    // A gives that commit up and publishes another seal from the same base.
    git_out(&a, &["reset", "--quiet", "--hard", &fork_base]);
    std::fs::write(&a_pin_file, a_pin).unwrap();
    add_secret(&a, "app/other", "other-from-a").await;
    ok_json(&a, &["sync", "--push"]).await;

    assert_refused_untouched(&b).await;
}

/// B's publication got no answer: Cloud may have accepted it. B's pull does
/// not rebuild it until the retried push settles its fate (a `conflict`),
/// and then recovers normally.
#[tokio::test]
async fn seal_whose_publication_may_have_been_accepted_is_not_rebuilt() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork unknown push").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    proxy
        .drop_next_publication
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let lost = run_cli(&b, &["--format", "json", "sync", "--push"]).await;
    assert!(!lost.status.success(), "{}", text(&lost));
    let intent = b.join("state/muster/cloud/push-intent.json");
    assert!(intent.exists(), "the unanswered publication stays pending");

    assert_refused_untouched(&b).await;

    losing_push(&b).await;
    assert!(!intent.exists());
    recover(&a, &b).await;
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

/// The server serves B's own never-accepted state under rewritten commits
/// (same trees, new commit ids). The served history holds B's own seal, so
/// it is not B's to rebuild: the pull is a plain update onto it, nothing is
/// abandoned or re-sealed.
#[tokio::test]
async fn own_state_served_under_rewritten_commits_is_not_rebuilt() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork rewritten").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let own = head_of(&b);
    // Everything B did since its last accepted revision, squashed into one
    // commit with B's exact tree.
    let fork_base = accepted_revision_of(&b);
    assert_eq!(git_out(&b, &["merge-base", &own, &fork_base]), fork_base);
    let pin = trust(&b).await;

    let tree = format!("{own}^{{tree}}");
    let rewritten = git_env(
        &b,
        &[
            "commit-tree",
            &tree,
            "-p",
            &fork_base,
            "-m",
            "rewritten by the server",
        ],
    );
    git_out(&b, &["update-ref", "refs/test/rewritten", &rewritten]);
    let bundle = bundle_of(&b, &rewritten);
    git_out(&b, &["update-ref", "-d", "refs/test/rewritten"]);
    *proxy.serve.lock().unwrap() = Some((bundle, rewritten.clone()));

    let pull = ok_json(&b, &["sync", "--pull"]).await;
    *proxy.serve.lock().unwrap() = None;
    assert_eq!(pull["result"]["transport"]["pulled"], "updated", "{pull}");
    assert!(abandoned_refs(&b).is_empty(), "{pull}");
    // On the served commit (plus at most sync bookkeeping), never re-sealed.
    git_out(&b, &["merge-base", "--is-ancestor", &rewritten, "HEAD"]);
    let range = format!("{rewritten}..HEAD");
    assert_eq!(
        git_out(
            &b,
            &[
                "log",
                "--format=%H",
                &range,
                "--",
                "fleet.state.json",
                "vault"
            ]
        ),
        ""
    );
    assert_eq!(trust(&b).await, pin, "no new seal");
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");
}

/// B's approval of `third` (a governance change: it enrolls the machine and
/// re-seals) loses the race: reported, not replayed, and repeating the
/// approval on the accepted state enrolls the machine.
#[tokio::test]
async fn lost_approval_converges_by_approving_again() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork approval").await;
    let base = stack.cloud.base.clone();
    let (a, b) = two_writers(&stack, &alice, &ws.id, &base).await;
    let signer = fleet_signer(&a).await;
    let c = join_workspace(&stack, &alice, &ws.id, "third", &base).await;
    ok_json(&c, &["sync", "--pull", "--expect-signer", &signer]).await;
    ok_json(&c, &["enroll", "--name", "third"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    ok_json(&b, &["sync", "--pull"]).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    assert!(run_cli(&b, &["--yes", "approve", "third"])
        .await
        .status
        .success());
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(messages.contains("muster.toml governance"), "{messages}");
    let manifest = std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    assert!(!manifest.contains("[machines.third]"), "{manifest}");
    assert_eq!(secret_value(&b, "app/token").await, "from-a");

    assert!(run_cli(&b, &["--yes", "approve", "third"])
        .await
        .status
        .success());
    publish_and_converge(&a, &b).await;
    let on_a = std::fs::read_to_string(config_of(&a).join("muster.toml")).unwrap();
    assert!(on_a.contains("[machines.third]"), "{on_a}");
    ok_json(&c, &["sync", "--pull"]).await;
}

// ---------------------------------------------------------------------------
// Pass 2: authenticated accepted history, policy fate, self-healing windows.
// ---------------------------------------------------------------------------

/// Runs git in `dir` (any repository) with a fixed identity.
fn git_at(dir: &Path, args: &[&str], index: Option<&Path>) -> String {
    let mut command = std::process::Command::new("git");
    command
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Server")
        .env("GIT_AUTHOR_EMAIL", "server@example.com")
        .env("GIT_COMMITTER_NAME", "Server")
        .env("GIT_COMMITTER_EMAIL", "server@example.com")
        .args(args);
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A server-side rewrite of `home`'s history: `build` gets a scratch clone
/// and returns the commit to serve as `main`.
fn forge_served(home: &Path, build: impl FnOnce(&Path, &Path) -> String) -> Served {
    let scratch = tempfile::tempdir().unwrap();
    let work = scratch.path().join("work");
    git_at(
        scratch.path(),
        &[
            "clone",
            "--quiet",
            config_of(home).to_str().unwrap(),
            work.to_str().unwrap(),
        ],
        None,
    );
    let index = scratch.path().join("forge.index");
    let tip = build(&work, &index);
    git_at(&work, &["update-ref", "refs/heads/main", &tip], None);
    let file = scratch.path().join("forged.bundle");
    git_at(
        &work,
        &[
            "bundle",
            "create",
            file.to_str().unwrap(),
            "refs/heads/main",
        ],
        None,
    );
    (std::fs::read(file).unwrap(), tip)
}

/// `commit`'s tree with the `ops` vault group files taken from `from`.
fn tree_with_group_of(work: &Path, index: &Path, commit: &str, from: &str) -> String {
    git_at(work, &["read-tree", commit], Some(index));
    for path in ["vault/ops.age", "vault/ops.meta.json"] {
        let blob = git_at(work, &["rev-parse", &format!("{from}:{path}")], None);
        git_at(
            work,
            &[
                "update-index",
                "--cacheinfo",
                &format!("100644,{blob},{path}"),
            ],
            Some(index),
        );
    }
    git_at(work, &["write-tree"], Some(index))
}

fn commit_tree(work: &Path, tree: &str, parents: &[&str], message: &str) -> String {
    let mut args = vec!["commit-tree", tree];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    args.extend(["-m", message]);
    git_at(work, &args, None)
}

/// A and B share group `ops`; A then adds `ops/key` and removes it again
/// (both published), while B adds its own `ops/key` and loses the race.
/// Returns the fork base and A's two commits.
async fn added_then_removed(a: &Path, b: &Path) -> (String, String, String) {
    add_secret(a, "ops/base", "shared").await;
    ok_json(a, &["sync", "--push"]).await;
    ok_json(b, &["sync", "--pull"]).await;
    let fork_base = head_of(a);
    add_secret(a, "ops/key", "from-a").await;
    let added = head_of(a);
    ok_json(a, &["sync", "--push"]).await;
    ok_json(a, &["--yes", "secret", "rm", "ops/key"]).await;
    let removed = head_of(a);
    ok_json(a, &["sync", "--push"]).await;
    add_secret(b, "ops/key", "from-b").await;
    losing_push(b).await;
    (fork_base, added, removed)
}

/// Whatever the outcome, `ops/key` is not resurrected on B.
async fn never_resurrected(b: &Path) {
    let pull = run_cli(b, &["--format", "json", "sync", "--pull"]).await;
    assert!(
        !pull.status.success(),
        "the add must not be replayed: {}",
        text(&pull)
    );
    assert!(!text(&pull).contains("from-b"), "values never leak");
    secret_absent(b, "ops/key").await;
    assert_eq!(secret_value(b, "ops/base").await, "shared");
}

/// The server rewrites the intermediate commit that added `ops/key`: same
/// signed state, but the fork base's vault blob. The accepted history then
/// looks as if the key was never there; only the signed vault digests show
/// it was.
#[tokio::test]
async fn rewritten_intermediate_vault_blob_never_resurrects_a_deleted_secret() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork forged blob").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;
    let (fork_base, added, removed) = added_then_removed(&a, &b).await;

    let served = forge_served(&a, |work, index| {
        let mut parent = fork_base.clone();
        let range = format!("{fork_base}..{removed}");
        let commits: Vec<String> = git_at(work, &["rev-list", "--reverse", &range], None)
            .lines()
            .map(str::to_owned)
            .collect();
        assert!(commits.contains(&added));
        for commit in &commits {
            let tree = if *commit == removed {
                format!("{removed}^{{tree}}")
            } else {
                tree_with_group_of(work, index, commit, &fork_base)
            };
            parent = commit_tree(work, &tree, &[&parent], "rewritten by the server");
        }
        parent
    });
    *proxy.serve.lock().unwrap() = Some(served);
    never_resurrected(&b).await;
    *proxy.serve.lock().unwrap() = None;
}

/// The server serves a merge whose vault files are TREESAME to a side
/// branch without the change: history simplification would hide the commit
/// that added `ops/key`. The signed chain still runs through it.
#[tokio::test]
async fn merge_hiding_the_change_never_resurrects_a_deleted_secret() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork forged merge").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;
    let (fork_base, added, removed) = added_then_removed(&a, &b).await;

    let served = forge_served(&a, |work, index| {
        let side_tree = tree_with_group_of(work, index, &fork_base, &removed);
        let side = commit_tree(work, &side_tree, &[&fork_base], "side branch");
        let tree = format!("{removed}^{{tree}}");
        commit_tree(work, &tree, &[&added, &side], "merge served by the server")
    });
    *proxy.serve.lock().unwrap() = Some(served);
    never_resurrected(&b).await;
    *proxy.serve.lock().unwrap() = None;
}

/// A publishes `ops/key` with no agent allowed; B, losing the race, added
/// its own `ops/key` allowed to an agent. The value collides and A's stays,
/// so B's allow list (written for B's value) must not land on A's secret.
#[tokio::test]
async fn allow_list_of_a_colliding_secret_is_never_injected() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork allow escalation").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "ops/key", "prod-from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    let add = run_cli_stdin(
        &b,
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
        b"from-b",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    assert!(std::fs::read_to_string(config_of(&b).join("muster.toml"))
        .unwrap()
        .contains("native://ops/key"));
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    let messages = diagnostics_text(&pull);
    assert!(
        messages.contains("ops/key") && messages.contains("policy native://ops/key"),
        "{messages}"
    );
    let manifest = std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    assert!(!manifest.contains("native://ops/key"), "{manifest}");
    assert_eq!(secret_value(&b, "ops/key").await, "prod-from-a");

    publish_and_converge(&a, &b).await;
    let on_a = std::fs::read_to_string(config_of(&a).join("muster.toml")).unwrap();
    assert!(!on_a.contains("native://ops/key"), "{on_a}");
}

/// B only changed the policy of `ops/key`; the accepted history removed the
/// secret meanwhile. The policy follows its secret and is not resurrected.
#[tokio::test]
async fn policy_of_a_secret_removed_in_accepted_history_is_not_resurrected() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork policy fate").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    add_secret(&a, "ops/key", "shared").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;

    ok_json(&a, &["--yes", "secret", "rm", "ops/key"]).await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(
        &b,
        &[
            "--yes",
            "secret",
            "allow",
            "native://ops/key",
            "--entry",
            "claude",
        ],
    )
    .await;
    losing_push(&b).await;

    let (code, pull) = failing_json(&b, &["sync", "--pull"]).await;
    assert_eq!(code, 1, "{pull}");
    assert!(
        diagnostics_text(&pull).contains("policy native://ops/key"),
        "{pull}"
    );
    let manifest = std::fs::read_to_string(config_of(&b).join("muster.toml")).unwrap();
    assert!(!manifest.contains("native://ops/key"), "{manifest}");
    secret_absent(&b, "ops/key").await;
}

/// The reconciling pull's answer names no (or the wrong) accepted revision:
/// HEAD and the pin moved, the accepted record is gone. The next honest
/// pull heals it and the machine publishes normally.
#[tokio::test]
async fn lost_accepted_record_after_a_reconcile_heals_on_the_next_pull() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork lost record").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    *proxy.serve.lock().unwrap() = Some((bundle_of(&a, &head_of(&a)), "0".repeat(40)));
    let pull = ok_json(&b, &["sync", "--pull"]).await;
    *proxy.serve.lock().unwrap() = None;
    assert_eq!(
        pull["result"]["transport"]["pulled"], "reconciled",
        "{pull}"
    );
    assert!(has_diagnostic(&pull, "cloud_revision_mismatch"), "{pull}");
    assert!(!b.join("state/muster/cloud/accepted.json").exists());

    let healed = ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(
        healed["result"]["transport"]["pulled"], "up_to_date",
        "{healed}"
    );
    assert_eq!(accepted_revision_of(&b), head_of(&a));
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

/// A crash after the reconcile finished but before the accepted revision
/// was recorded heals on the next pull.
#[tokio::test]
async fn crash_before_recording_the_accepted_head_heals_on_the_next_pull() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork crash record").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let crashed = cli_command(&b)
        .env("MUSTER_TEST_PULL_CRASH", "before-accepted")
        .args(["--format", "json", "sync", "--pull"])
        .output()
        .await
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", text(&crashed));
    assert_ne!(accepted_revision_of(&b), head_of(&a));

    let healed = ok_json(&b, &["sync", "--pull"]).await;
    assert_ne!(
        healed["result"]["transport"]["pulled"], "reconciled",
        "{healed}"
    );
    assert_eq!(accepted_revision_of(&b), head_of(&a));
    publish_and_converge(&a, &b).await;
    assert_eq!(secret_value(&a, "ops/key").await, "from-b");
}

/// A first join crashes between the checkout and the pin: the scaffold pin
/// faces another fleet's history. The next command rolls it back and the
/// join is simply repeated.
#[tokio::test]
async fn crash_while_joining_a_fleet_recovers_on_its_own() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork crash join").await;
    let base = stack.cloud.base.clone();
    let a = join_workspace(&stack, &alice, &ws.id, "laptop", &base).await;
    ok_json(&a, &["sync", "--push"]).await;
    let signer = fleet_signer(&a).await;
    let b = join_workspace(&stack, &alice, &ws.id, "second", &base).await;
    let scaffold = head_of(&b);

    let crashed = cli_command(&b)
        .env("MUSTER_TEST_PULL_CRASH", "after-checkout")
        .args([
            "--format",
            "json",
            "sync",
            "--pull",
            "--expect-signer",
            &signer,
        ])
        .output()
        .await
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", text(&crashed));
    assert_eq!(head_of(&b), head_of(&a), "the checkout happened");

    let status = ok_json(&b, &["status"]).await;
    assert!(has_diagnostic(&status, "cloud_pull_recovered"), "{status}");
    assert_eq!(head_of(&b), scaffold);
    ok_json(&b, &["sync", "--pull", "--expect-signer", &signer]).await;
    assert_eq!(head_of(&b), head_of(&a));
    assert_eq!(trust(&b).await.1, trust(&a).await.1);
}

/// The interrupted pull ran on B's repository; the next command runs with
/// `--config` on another one. The recovery acts on the repository the pull
/// ran on, never on the other.
#[tokio::test]
async fn interrupted_pull_is_recovered_in_its_own_repository() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork journal repo").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let abandoned = head_of(&b);
    let crashed = cli_command(&b)
        .env("MUSTER_TEST_PULL_CRASH", "after-replay")
        .args(["--format", "json", "sync", "--pull"])
        .output()
        .await
        .unwrap();
    assert_eq!(crashed.status.code(), Some(86), "{}", text(&crashed));

    let other = tempfile::tempdir().unwrap();
    let clone = other.path().join("clone");
    git_at(
        other.path(),
        &[
            "clone",
            "--quiet",
            config_of(&a).to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
        None,
    );
    let other_head = git_at(&clone, &["rev-parse", "HEAD"], None);
    let out = run_cli(
        &b,
        &[
            "--format",
            "json",
            "--config",
            clone.to_str().unwrap(),
            "status",
        ],
    )
    .await;
    assert!(
        text(&out).contains("cloud_pull_recovered"),
        "{}",
        text(&out)
    );
    assert_eq!(git_at(&clone, &["rev-parse", "HEAD"], None), other_head);
    assert_eq!(git_at(&clone, &["status", "--porcelain"], None), "");
    assert!(!b.join("state/muster/cloud/pull-journal.json").exists());
    assert_eq!(head_of(&b), abandoned);
    recover(&a, &b).await;
}

/// A journal that cannot be read never blocks diagnosis: it is set aside
/// (kept, not deleted) and reported.
#[tokio::test]
async fn corrupt_pull_journal_never_blocks_doctor_or_trust_show() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork corrupt journal").await;
    let (_a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;
    let journal = b.join("state/muster/cloud/pull-journal.json");
    let aside = || {
        std::fs::read_dir(b.join("state/muster/cloud"))
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with("pull-journal.unusable-")
            })
            .count()
    };

    std::fs::write(&journal, b"{ not a journal").unwrap();
    let show = ok_json(&b, &["trust", "show"]).await;
    assert!(
        has_diagnostic(&show, "cloud_pull_journal_unusable"),
        "{show}"
    );
    assert!(!journal.exists());
    assert_eq!(aside(), 1);

    std::fs::write(&journal, b"\x00\x01").unwrap();
    let doctor = run_cli(&b, &["--format", "json", "doctor", "--offline"]).await;
    assert!(
        text(&doctor).contains("cloud_pull_journal_unusable"),
        "{}",
        text(&doctor)
    );
    assert!(!journal.exists());
    assert_eq!(aside(), 2);
    ok_json(&b, &["status"]).await;
}

/// A server (or cache) that serves an OLDER accepted head, with a matching
/// revision header, never moves the accepted record backwards: receipts
/// keep their provenance and the next publication keeps its base. Without
/// any record, an older head than the pinned state is not recorded either;
/// the honest head is.
#[tokio::test]
async fn older_served_head_never_moves_the_accepted_record_back() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed stale head").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;
    let accepted_file = b.join("state/muster/cloud/accepted.json");

    add_secret(&a, "app/one", "first").await;
    ok_json(&a, &["sync", "--push"]).await;
    let older = head_of(&a);
    add_secret(&a, "app/two", "second").await;
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    let current = head_of(&a);
    assert_eq!(accepted_revision_of(&b), current);
    let record = std::fs::read(&accepted_file).unwrap();
    let b_head = head_of(&b);

    *proxy.serve.lock().unwrap() = Some((bundle_of(&b, &older), older.clone()));
    let stale = ok_json(&b, &["sync", "--pull"]).await;
    assert!(has_diagnostic(&stale, "cloud_revision_stale"), "{stale}");
    assert!(
        !diagnostics_text(&stale).contains("not covered by this machine's pin"),
        "{stale}"
    );
    assert_eq!(std::fs::read(&accepted_file).unwrap(), record);
    assert_eq!(head_of(&b), b_head);

    // Without a record, the older head is not recorded either.
    std::fs::remove_file(&accepted_file).unwrap();
    let unrecorded = ok_json(&b, &["sync", "--pull"]).await;
    assert!(
        has_diagnostic(&unrecorded, "cloud_revision_stale"),
        "{unrecorded}"
    );
    assert!(!accepted_file.exists());
    *proxy.serve.lock().unwrap() = None;
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(
        accepted_revision_of(&b),
        current,
        "the honest head heals it"
    );

    // The next publication is based on the real head (not a stale 409).
    add_secret(&b, "ops/key", "from-b").await;
    ok_json(&b, &["sync", "--push"]).await;
    assert_eq!(accepted_revision_of(&b), head_of(&b));
}

/// B's own seal was accepted. A server that first serves the fork base (an
/// ancestor of B's HEAD) and then A's losing fork from that base is refused
/// exactly like serving the fork directly: the older head never erased the
/// record that B's seal was accepted.
#[tokio::test]
async fn older_served_head_never_opens_the_fork_of_an_accepted_seal() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed stale then fork").await;
    let (endpoint, proxy) = start_proxy(stack.cloud.base.clone()).await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &endpoint).await;
    let fork_base = head_of(&b);

    add_secret(&b, "ops/key", "from-b").await;
    ok_json(&b, &["sync", "--push"]).await;
    let b_accepted = head_of(&b);
    add_secret(&a, "app/token", "from-a").await;
    *proxy.last_publication.lock().unwrap() = None;
    losing_push(&a).await;
    let a_fork = proxy
        .last_publication
        .lock()
        .unwrap()
        .clone()
        .expect("A's publication bundle was captured");

    *proxy.serve.lock().unwrap() = Some((bundle_of(&b, &fork_base), fork_base.clone()));
    ok_json(&b, &["sync", "--pull"]).await;
    assert_eq!(accepted_revision_of(&b), b_accepted);

    *proxy.serve.lock().unwrap() = Some((a_fork, head_of(&a)));
    assert_refused_untouched(&b).await;
    assert_eq!(head_of(&b), b_accepted);
    assert_eq!(secret_value(&b, "ops/key").await, "from-b");
    *proxy.serve.lock().unwrap() = None;
}

/// Backup refs of abandoned history and set-aside journals are bounded: the
/// oldest are pruned automatically, the newest one is always kept.
#[tokio::test]
async fn abandoned_backups_and_unusable_journals_are_pruned_automatically() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Signed fork pruning").await;
    let (a, b) = two_writers(&stack, &alice, &ws.id, &stack.cloud.base).await;

    // Twelve old backups (dated 2020) from earlier recoveries.
    let tree = git_out(&b, &["rev-parse", "HEAD^{tree}"]);
    let mut old = Vec::new();
    for day in 1..=12 {
        let date = format!("2020-01-{day:02}T00:00:00Z");
        let output = std::process::Command::new("git")
            .current_dir(config_of(&b))
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .args(["commit-tree", &tree, "-m", &format!("old {day}")])
            .output()
            .unwrap();
        assert!(output.status.success());
        let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        git_out(
            &b,
            &[
                "update-ref",
                &format!("refs/muster/abandoned/{commit}"),
                &commit,
            ],
        );
        old.push(commit);
    }

    add_secret(&a, "app/token", "from-a").await;
    ok_json(&a, &["sync", "--push"]).await;
    add_secret(&b, "ops/key", "from-b").await;
    losing_push(&b).await;
    let abandoned = head_of(&b);
    recover(&a, &b).await;
    let refs: Vec<String> = abandoned_refs(&b).into_iter().map(|(c, _)| c).collect();
    assert_eq!(refs.len(), 10, "{refs:?}");
    assert!(refs.contains(&abandoned), "the newest backup is kept");
    assert!(!refs.contains(&old[0]) && !refs.contains(&old[1]) && !refs.contains(&old[2]));
    assert!(refs.contains(&old[11]));

    let cloud = b.join("state/muster/cloud");
    for stamp in 1..=7 {
        std::fs::write(
            cloud.join(format!("pull-journal.unusable-{stamp}.json")),
            b"old",
        )
        .unwrap();
    }
    std::fs::write(cloud.join("pull-journal.json"), b"{ not a journal").unwrap();
    let show = ok_json(&b, &["trust", "show"]).await;
    assert!(
        has_diagnostic(&show, "cloud_pull_journal_unusable"),
        "{show}"
    );
    let mut aside: Vec<String> = std::fs::read_dir(&cloud)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("pull-journal.unusable-"))
        .collect();
    aside.sort();
    assert_eq!(aside.len(), 5, "{aside:?}");
    assert!(
        aside
            .iter()
            .any(|n| std::fs::read(cloud.join(n)).unwrap() == b"{ not a journal"),
        "the newest set-aside journal is kept: {aside:?}"
    );
    for gone in ["1", "2", "3"] {
        assert!(
            !aside.contains(&format!("pull-journal.unusable-{gone}.json")),
            "{aside:?}"
        );
    }
}
