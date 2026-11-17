//! Concurrent publication from the terminal: the machine whose push lost the
//! race gets an honest, actionable `conflict` result, keeps its local commits,
//! can see and close the retained Cloud proposal, and converges with
//! `sync --pull` + `sync --push`. Real `muster` binary against the real
//! service over HTTP; nothing is mocked and no database row is asserted.
mod support;

use muster_cloud::protocol::{DeviceApproveRequest, DeviceDecision};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};

async fn stack() -> support::Stack {
    static START_LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = START_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    support::stack().await
}

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

/// Runs a command that must fail and returns its JSON envelope.
async fn failing_json(home: &Path, args: &[&str]) -> Value {
    let mut full = vec!["--format", "json"];
    full.extend_from_slice(args);
    let output = run_cli(home, &full).await;
    assert!(
        !output.status.success(),
        "{args:?} must fail: {}",
        text(&output)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "{args:?}: stdout is not a JSON envelope ({e}): {}",
            text(&output)
        )
    })
}

fn diagnostic<'a>(envelope: &'a Value, code: &str) -> &'a Value {
    envelope["diagnostics"]
        .as_array()
        .and_then(|all| all.iter().find(|d| d["code"] == code))
        .unwrap_or_else(|| panic!("no `{code}` diagnostic in {envelope}"))
}

async fn device_login(stack: &support::Stack, user: &support::UserSession, home: &Path) {
    let mut child = cli_command(home)
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
    let home = std::env::temp_dir().join(format!("muster-cli-conflicts-{}", uuid::Uuid::new_v4()));
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
    assert!(output.status.success(), "git {args:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn head_of(home: &Path) -> String {
    git_out(home, &["rev-parse", "HEAD"])
}

fn cloud_device_id(home: &Path) -> String {
    let link: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/link.json")).unwrap(),
    )
    .unwrap();
    link["device"].as_str().unwrap().to_owned()
}

fn accepted_revision_of(home: &Path) -> String {
    let accepted: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join("state/muster/cloud/accepted.json")).unwrap(),
    )
    .unwrap();
    accepted["revision"].as_str().unwrap().to_owned()
}

/// Machine setup: login, init and link `home` to `workspace`.
async fn join_workspace(
    stack: &support::Stack,
    user: &support::UserSession,
    workspace: &str,
    machine: &str,
) -> PathBuf {
    let home = new_home();
    device_login(stack, user, &home).await;
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
/// is approved locally by A and pulls the approval. Both are writers and
/// converged on the same accepted head. Returns `(a, b)`.
async fn two_writers(
    stack: &support::Stack,
    alice: &support::UserSession,
    workspace: &str,
) -> (PathBuf, PathBuf) {
    let a = join_workspace(stack, alice, workspace, "laptop").await;
    ok_json(&a, &["sync", "--push"]).await;
    let signer = fleet_signer(&a).await;
    let b = join_workspace(stack, alice, workspace, "second").await;
    ok_json(&b, &["sync", "--pull", "--expect-signer", &signer]).await;
    ok_json(&b, &["enroll", "--name", "second"]).await;
    ok_json(&a, &["sync", "--pull"]).await;
    assert!(run_cli(&a, &["--yes", "approve", "second"])
        .await
        .status
        .success());
    ok_json(&a, &["sync", "--push"]).await;
    ok_json(&b, &["sync", "--pull"]).await;
    (a, b)
}

/// A publishes a secret (accepted head advances) while B, still on the old
/// head, edits an instruction file and loses the race. Returns B's failed
/// push envelope plus B's HEAD at the time of the push.
struct Race {
    a: PathBuf,
    b: PathBuf,
    push: Value,
    b_head_before: String,
    b_base: String,
}

async fn lost_race(stack: &support::Stack, alice: &support::UserSession, workspace: &str) -> Race {
    let (a, b) = two_writers(stack, alice, workspace).await;
    let b_base = accepted_revision_of(&b);

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
        b"from-a",
    )
    .await;
    assert!(add.status.success(), "{}", text(&add));
    ok_json(&a, &["sync", "--push"]).await;

    std::fs::write(
        config_of(&b).join("AGENTS.md"),
        "Instruction written on machine B.\n",
    )
    .unwrap();
    let push = failing_json(&b, &["sync", "--push"]).await;
    let b_head_before = head_of(&b);
    Race {
        a,
        b,
        push,
        b_head_before,
        b_base,
    }
}

fn open_conflicts(listing: &Value) -> Vec<&Value> {
    listing["result"]["proposals"]
        .as_array()
        .unwrap_or_else(|| panic!("no proposals array in {listing}"))
        .iter()
        .filter(|p| p["kind"] == "conflict")
        .collect()
}

#[tokio::test]
async fn losing_push_is_reported_as_conflict_with_proposal_id_and_keeps_local_commits() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Conflict outcome").await;
    let race = lost_race(&stack, &alice, &ws.id).await;

    // The refusal is a structured conflict, not a generic error.
    assert_eq!(race.push["ok"], false, "{}", race.push);
    let conflict = diagnostic(&race.push, "revision_conflict");
    let message = conflict["message"].as_str().unwrap();
    let listing = ok_json(&race.b, &["cloud", "proposals"]).await;
    let open = open_conflicts(&listing);
    assert_eq!(open.len(), 1, "{listing}");
    let proposal_id = open[0]["id"].as_str().unwrap();
    assert!(
        message.contains(proposal_id),
        "the diagnostic must name the retained proposal {proposal_id}: {message}"
    );
    assert!(
        message.contains("sync --pull"),
        "the diagnostic must name the next step: {message}"
    );

    // `muster status` and the recorded last sync outcome say `conflict`.
    let status = ok_json(&race.b, &["status"]).await;
    assert_eq!(status["result"]["sync"]["result"], "conflict", "{status}");

    // Nothing local was lost: same HEAD, the sync commit and the edit survive.
    assert_eq!(head_of(&race.b), race.b_head_before);
    assert!(git_out(&race.b, &["log", "--format=%s"]).contains("chore(muster): sync"));
    assert_eq!(
        std::fs::read_to_string(config_of(&race.b).join("AGENTS.md")).unwrap(),
        "Instruction written on machine B.\n"
    );
}

#[tokio::test]
async fn cloud_proposals_lists_the_open_conflict_with_author_and_revisions() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Conflict listing").await;
    let race = lost_race(&stack, &alice, &ws.id).await;

    // Both machines see the same open proposal.
    for home in [&race.a, &race.b] {
        let listing = ok_json(home, &["cloud", "proposals"]).await;
        let open = open_conflicts(&listing);
        assert_eq!(open.len(), 1, "{listing}");
        let proposal = open[0];
        assert_eq!(proposal["status"], "open");
        assert_eq!(proposal["device_id"], cloud_device_id(&race.b));
        assert_eq!(proposal["base_revision"], race.b_base);
        assert_eq!(proposal["revision"], race.b_head_before);
        assert!(proposal["created_at"]
            .as_str()
            .is_some_and(|t| t.ends_with('Z')));
    }

    // The human summary carries the same facts.
    let human = run_cli(&race.b, &["cloud", "proposals"]).await;
    assert!(human.status.success(), "{}", text(&human));
    let human = text(&human);
    for needle in [
        "conflict",
        &race.b_base[..12],
        &race.b_head_before[..12],
        &cloud_device_id(&race.b),
    ] {
        assert!(human.contains(needle), "missing `{needle}` in:\n{human}");
    }
}

#[tokio::test]
async fn pull_then_push_converges_and_rejecting_closes_the_stale_proposal() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&stack, &alice, "Conflict convergence").await;
    let race = lost_race(&stack, &alice, &ws.id).await;
    let listing = ok_json(&race.b, &["cloud", "proposals"]).await;
    let proposal_id = open_conflicts(&listing)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    let local_commit = head_of(&race.b);

    // Pull rebases the unpublished local commit on the new accepted head.
    let pull = ok_json(&race.b, &["sync", "--pull"]).await;
    assert_eq!(pull["result"]["transport"]["pulled"], "updated", "{pull}");
    let log = git_out(&race.b, &["log", "--format=%s"]);
    assert!(log.contains("chore(muster): sync"), "{log}");
    assert_ne!(
        head_of(&race.b),
        local_commit,
        "the local commit is rebased"
    );
    // The rebased sync commit descends from the accepted head the pull
    // recorded (it was rebuilt on top of A's publication, not merged aside).
    let rebased = git_out(
        &race.b,
        &[
            "log",
            "-1",
            "--format=%H",
            "--grep=chore(muster): sync",
            "--fixed-strings",
        ],
    );
    let on_accepted = std::process::Command::new("git")
        .current_dir(config_of(&race.b))
        .args([
            "merge-base",
            "--is-ancestor",
            &accepted_revision_of(&race.b),
            &rebased,
        ])
        .status()
        .unwrap();
    assert!(
        on_accepted.success(),
        "the local commit must sit on the accepted head"
    );

    // The next push publishes it and the other machine receives it.
    ok_json(&race.b, &["sync", "--push"]).await;
    ok_json(&race.a, &["sync", "--pull"]).await;
    assert_eq!(
        std::fs::read_to_string(config_of(&race.a).join("AGENTS.md")).unwrap(),
        "Instruction written on machine B.\n"
    );
    // Both machines agree on the accepted head: the fleet converged.
    assert_eq!(accepted_revision_of(&race.a), accepted_revision_of(&race.b));

    // The stale proposal is still open until someone closes it.
    let before = ok_json(&race.b, &["cloud", "proposals"]).await;
    assert_eq!(open_conflicts(&before).len(), 1, "{before}");
    let rejected = ok_json(
        &race.b,
        &["cloud", "proposals", "resolve", &proposal_id, "--reject"],
    )
    .await;
    assert_eq!(rejected["result"]["id"], proposal_id.as_str(), "{rejected}");
    assert_eq!(rejected["result"]["status"], "rejected", "{rejected}");
    let after = ok_json(&race.a, &["cloud", "proposals"]).await;
    assert!(open_conflicts(&after).is_empty(), "{after}");

    // Closing twice is refused honestly by the service.
    let again = failing_json(
        &race.b,
        &["cloud", "proposals", "resolve", &proposal_id, "--reject"],
    )
    .await;
    let refusal = again["diagnostics"][0]["message"].as_str().unwrap();
    assert!(
        refusal.contains("409") && refusal.contains("proposal_closed"),
        "{refusal}"
    );
}

#[tokio::test]
async fn member_machine_without_rights_gets_403_and_unknown_proposal_gets_404() {
    let stack = stack().await;
    let alice = stack.login_user("alice", "alice@example.com").await;
    let bob = stack.login_user("bob", "bob@example.com").await;
    let ws = create_workspace(&stack, &alice, "Conflict rights").await;
    let race = lost_race(&stack, &alice, &ws.id).await;
    let listing = ok_json(&race.b, &["cloud", "proposals"]).await;
    let proposal_id = open_conflicts(&listing)[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();

    // Bob is invited as a plain member and links his own machine, which pins
    // the fleet but is not authorized by its signed governance.
    let invitation = support::http()
        .post(stack.url(&format!("/v1/workspaces/{}/invitations", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&serde_json::json!({"email": "bob@example.com", "role": "member"}))
        .send()
        .await
        .unwrap();
    assert_eq!(invitation.status(), 201);
    let invitation: Value = invitation.json().await.unwrap();
    let accepted = support::http()
        .post(stack.url(invitation["accept_path"].as_str().unwrap()))
        .header("Cookie", format!("__Host-mc_session={}", bob.cookie))
        .header("Origin", stack.origin())
        .header("X-CSRF-Token", &bob.csrf_token)
        .json(&serde_json::json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(accepted.status(), 200);
    let signer = fleet_signer(&race.a).await;
    let c = join_workspace(&stack, &bob, &ws.id, "bobs-machine").await;
    ok_json(&c, &["sync", "--pull", "--expect-signer", &signer]).await;

    // Reading is allowed for any linked machine; closing is not.
    let seen = ok_json(&c, &["cloud", "proposals"]).await;
    assert_eq!(open_conflicts(&seen).len(), 1, "{seen}");
    let denied = failing_json(
        &c,
        &["cloud", "proposals", "resolve", &proposal_id, "--reject"],
    )
    .await;
    let refusal = denied["diagnostics"][0]["message"].as_str().unwrap();
    assert!(
        refusal.contains("403") && refusal.contains("device_not_approved"),
        "the server's refusal must be surfaced: {refusal}"
    );
    let still = ok_json(&race.a, &["cloud", "proposals"]).await;
    assert_eq!(open_conflicts(&still).len(), 1, "{still}");

    // An id the workspace does not have is a 404, not a silent success.
    let missing = uuid::Uuid::new_v4().to_string();
    let gone = failing_json(
        &race.a,
        &["cloud", "proposals", "resolve", &missing, "--reject"],
    )
    .await;
    let refusal = gone["diagnostics"][0]["message"].as_str().unwrap();
    assert!(refusal.contains("404"), "{refusal}");

    // Without --reject there is nothing to do: the command refuses to guess.
    let unspecified = run_cli(&race.a, &["cloud", "proposals", "resolve", &proposal_id]).await;
    assert!(!unspecified.status.success(), "{}", text(&unspecified));
}
