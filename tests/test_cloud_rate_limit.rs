//! Cloud answers `429 rate_limited` with a `Retry-After` (seconds): the CLI
//! waits that long (capped) and retries once when the request is safe to
//! repeat, and otherwise reports a `cloud_rate_limited` diagnostic with when
//! to retry. Real `muster` binary against a minimal local HTTP stub.
mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::MusterEnv;

/// One scripted answer of the stub.
struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn rate_limited(retry_after: Option<&str>) -> Reply {
    Reply {
        status: 429,
        headers: retry_after
            .map(|v| vec![("Retry-After", v.to_owned())])
            .unwrap_or_default(),
        body: r#"{"error":{"code":"rate_limited","message":"slow down","details":null}}"#.into(),
    }
}

fn ok(status: u16, body: &str) -> Reply {
    Reply {
        status,
        headers: vec![("Content-Type", "application/json".into())],
        body: body.into(),
    }
}

/// A request the stub saw: its head (request line + headers) and when.
struct Seen {
    head: String,
    at: Instant,
}

struct Stub {
    endpoint: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Stub {
    /// Answers the requests in order; once the script is exhausted every
    /// further request gets a `500`, so a loop would be visible.
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            let mut script = script.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head_end = loop {
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        break i + 4;
                    }
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break buf.len(),
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                };
                let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                let length = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                while buf.len() < head_end + length {
                    match stream.read(&mut chunk) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                log.lock().unwrap().push(Seen {
                    head,
                    at: Instant::now(),
                });
                let reply = script
                    .next()
                    .unwrap_or_else(|| ok(500, r#"{"error":"script exhausted"}"#));
                let mut out = format!("HTTP/1.1 {} X\r\n", reply.status);
                for (name, value) in &reply.headers {
                    out.push_str(&format!("{name}: {value}\r\n"));
                }
                out.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    reply.body.len(),
                    reply.body
                ));
                let _ = stream.write_all(out.as_bytes());
            }
        });
        Self { endpoint, seen }
    }

    fn count(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn gap(&self) -> Duration {
        let seen = self.seen.lock().unwrap();
        seen[1].at.duration_since(seen[0].at)
    }

    fn header(&self, index: usize, name: &str) -> Option<String> {
        let seen = self.seen.lock().unwrap();
        seen[index].head.lines().find_map(|l| {
            let (key, value) = l.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

fn login(env: &MusterEnv, endpoint: &str) {
    let cloud = env.path().join(".local/state/muster/cloud");
    std::fs::create_dir_all(&cloud).unwrap();
    std::fs::write(
        cloud.join("account.json"),
        format!(r#"{{"endpoint":"{endpoint}","account_id":"alice","token":"mca_test_token"}}"#),
    )
    .unwrap();
}

/// A machine with a real signing key and a Cloud link to `endpoint`.
fn linked_machine(env: &MusterEnv, endpoint: &str) {
    let init = env
        .run(&["--yes", "init", "--machine", "laptop"])
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let cloud = env.path().join(".local/state/muster/cloud");
    std::fs::create_dir_all(&cloud).unwrap();
    std::fs::write(
        cloud.join("link.json"),
        format!(
            r#"{{"endpoint":"{endpoint}","workspace":"ws-1","device":"dev-1","token":"mcd_test_token"}}"#
        ),
    )
    .unwrap();
}

fn has_diagnostic(envelope: &support::Envelope, code: &str) -> bool {
    envelope
        .diagnostics()
        .as_array()
        .is_some_and(|all| all.iter().any(|d| d["code"] == code))
}

fn diagnostic_message(envelope: &support::Envelope, code: &str) -> String {
    envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["code"] == code)
        .map(|d| d["message"].as_str().unwrap().to_owned())
        .unwrap_or_default()
}

#[test]
fn a_rate_limited_read_waits_the_retry_after_and_retries_once() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), ok(200, "[]")]);
    login(&env, &stub.endpoint);
    let started = Instant::now();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "workspaces"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "one retry, no more");
    assert!(stub.gap() >= Duration::from_millis(950), "{:?}", stub.gap());
    assert!(started.elapsed() < Duration::from_secs(20));
}

#[test]
fn a_read_still_rate_limited_after_the_retry_reports_when_to_retry() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), rate_limited(Some("7"))]);
    login(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "workspaces"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "never a third request");
    assert!(
        has_diagnostic(&envelope, "cloud_rate_limited"),
        "{}",
        envelope.raw
    );
    let message = diagnostic_message(&envelope, "cloud_rate_limited");
    assert!(message.contains("7 s"), "{message}");
}

#[test]
fn a_retry_after_beyond_the_cap_is_reported_without_waiting() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("3600"))]);
    login(&env, &stub.endpoint);
    let started = Instant::now();
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "workspaces"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 1, "nothing is sent while the limit holds");
    assert!(started.elapsed() < Duration::from_secs(30));
    let message = diagnostic_message(&envelope, "cloud_rate_limited");
    assert!(message.contains("3600 s"), "{message}");
}

#[test]
fn a_rate_limit_without_retry_after_still_retries_once_after_a_short_pause() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(None), ok(200, "[]")]);
    login(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "workspaces"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2);
    assert!(stub.gap() >= Duration::from_millis(900), "{:?}", stub.gap());
}

#[test]
fn a_rate_limited_pull_is_retried_with_a_fresh_signature() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), ok(204, "")]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "sync", "--pull"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2);
    assert_ne!(
        stub.header(0, "x-muster-nonce"),
        stub.header(1, "x-muster-nonce"),
        "a replayed nonce would be refused"
    );
    assert!(stub.header(0, "x-muster-nonce").is_some());
}

#[test]
fn a_rate_limited_publication_retries_with_the_same_key_then_keeps_its_intent() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), rate_limited(Some("2"))]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "sync", "--push"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "one retry, no more");
    assert!(
        has_diagnostic(&envelope, "cloud_rate_limited"),
        "{}",
        envelope.raw
    );
    assert_eq!(
        stub.header(0, "idempotency-key"),
        stub.header(1, "idempotency-key")
    );
    assert!(stub.header(0, "idempotency-key").is_some());
    assert_ne!(
        stub.header(0, "x-muster-nonce"),
        stub.header(1, "x-muster-nonce")
    );
    // The same key and bytes stay pending: the next run replays them.
    assert!(env
        .path()
        .join(".local/state/muster/cloud/push-intent.json")
        .exists());
}

#[test]
fn a_rate_limited_request_that_is_not_safe_to_repeat_is_reported_not_retried() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1"))]);
    login(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "logout"]);
    // Logout always completes locally; the remote call is best effort and is
    // never repeated.
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 1);
}

// ---------------------------------------------------------------------------
// Device-signed requests (proposals, enrollment) and the receipt history read.
// ---------------------------------------------------------------------------

const PROPOSAL_ID: &str = "6e40f3bc-68e7-4854-9343-c72bd5553ee9";

fn workspace_json() -> String {
    r#"{"id":"ws-1","name":"W","role":"owner","plan_status":"active","trial_ends_at":"2030-01-01T00:00:00Z","created_at":"2030-01-01T00:00:00Z"}"#.into()
}

fn device_json() -> String {
    r#"{"id":"dev-1","machine_name":"laptop","signing_key":"k","recipient":"r","status":"linked","bootstrap":false,"linked_by":"alice","created_at":"2030-01-01T00:00:00Z","last_seen_at":null,"last_receipt":null}"#.into()
}

fn proposal_json() -> String {
    format!(
        r#"{{"id":"{PROPOSAL_ID}","kind":"enrollment","status":"open","base_revision":"none","revision":"","created_at":"2030-01-01T00:00:00Z"}}"#
    )
}

#[test]
fn a_rate_limited_proposal_listing_is_retried_with_a_fresh_signature() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), ok(200, "[]")]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "proposals"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "one retry, no more");
    assert!(stub.gap() >= Duration::from_millis(950), "{:?}", stub.gap());
    assert_ne!(
        stub.header(0, "x-muster-nonce"),
        stub.header(1, "x-muster-nonce")
    );
}

#[test]
fn a_proposal_listing_limited_beyond_the_cap_is_reported_without_waiting() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("3600"))]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&["--format", "json", "cloud", "proposals"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 1);
    let message = diagnostic_message(&envelope, "cloud_rate_limited");
    assert!(message.contains("3600 s"), "{}", envelope.raw);
}

#[test]
fn closing_a_proposal_is_not_keyed_so_a_limit_is_reported_not_retried() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), ok(200, &proposal_json())]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) = env.run_json_strict(&[
        "--format",
        "json",
        "cloud",
        "proposals",
        "resolve",
        PROPOSAL_ID,
        "--reject",
    ]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 1, "an unkeyed write is never repeated");
    assert!(
        has_diagnostic(&envelope, "cloud_rate_limited"),
        "{}",
        envelope.raw
    );
}

#[test]
fn a_rate_limited_enrollment_proposal_retries_with_the_same_key() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), ok(201, &proposal_json())]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) =
        env.run_json_strict(&["--format", "json", "--yes", "enroll", "--name", "second"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "one retry, no more");
    assert!(stub.header(0, "idempotency-key").is_some());
    assert_eq!(
        stub.header(0, "idempotency-key"),
        stub.header(1, "idempotency-key")
    );
    assert_ne!(
        stub.header(0, "x-muster-nonce"),
        stub.header(1, "x-muster-nonce")
    );
    assert_eq!(envelope.result()["proposal"], PROPOSAL_ID);
}

#[test]
fn an_enrollment_proposal_still_limited_after_the_retry_reports_when_to_retry() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![rate_limited(Some("1")), rate_limited(Some("9"))]);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) =
        env.run_json_strict(&["--format", "json", "--yes", "enroll", "--name", "second"]);
    assert!(!output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2);
    let message = diagnostic_message(&envelope, "cloud_rate_limited");
    assert!(message.contains("9 s"), "{}", envelope.raw);
}

#[test]
fn a_rate_limited_receipt_history_read_is_retried_once() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![
        ok(200, &format!("[{}]", workspace_json())),
        rate_limited(Some("1")),
        ok(200, &format!("[{}]", device_json())),
    ]);
    login(&env, &stub.endpoint);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) =
        env.run_json_strict(&["--format", "json", "cloud", "link", "--workspace", "ws-1"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 3, "list, limited read, one retry");
    assert!(
        !has_diagnostic(&envelope, "receipt_sequence_deferred"),
        "{}",
        envelope.raw
    );
}

#[test]
fn a_receipt_history_read_limited_beyond_the_cap_is_reported_and_deferred() {
    let env = MusterEnv::new();
    let stub = Stub::start(vec![
        ok(200, &format!("[{}]", workspace_json())),
        rate_limited(Some("3600")),
    ]);
    login(&env, &stub.endpoint);
    linked_machine(&env, &stub.endpoint);
    let (output, envelope) =
        env.run_json_strict(&["--format", "json", "cloud", "link", "--workspace", "ws-1"]);
    assert!(output.status.success(), "{}", envelope.raw);
    assert_eq!(stub.count(), 2, "no retry while the limit holds");
    let message = diagnostic_message(&envelope, "cloud_rate_limited");
    assert!(message.contains("3600 s"), "{}", envelope.raw);
    assert!(
        has_diagnostic(&envelope, "receipt_sequence_deferred"),
        "{}",
        envelope.raw
    );
}
