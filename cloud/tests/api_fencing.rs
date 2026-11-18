//! Fencing beyond the request entry: once another incarnation is activated,
//! a still-running instance acknowledges no in-flight write and its
//! background workers claim nothing and call no provider; an active
//! instance is never refused because of normal concurrent appends.
//! Observed through the real service, the real `muster-cloud-ops` binary
//! and the external Paddle fixture only.
mod support;

use serde_json::json;
use support::paddle_fixture::{start, PaddleFixture};
use support::{fresh_database, http, run_ops, Env, Stack, UserSession};
use time::OffsetDateTime;

fn text(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A fresh database registered and activated: the stack's own incarnation
/// is fenced from here on.
async fn fence(env: &Env) {
    let db = fresh_database().await;
    let out = run_ops(env, &["register", "--database-url", &db]).await;
    assert!(out.status.success(), "{}", text(&out));
    let status: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let id = status["db"]["incarnation_id"].as_str().unwrap().to_string();
    let out = run_ops(
        env,
        &["activate", "--incarnation", &id, "--database-url", &db],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
}

fn write(
    s: &Stack,
    u: &UserSession,
    method: reqwest::Method,
    path: &str,
) -> reqwest::RequestBuilder {
    http()
        .request(method, s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
}

/// A write that passed the entry check before the activation is never
/// acknowledged by the fenced instance: its client is told to retry (503).
#[tokio::test]
async fn a_fenced_instance_never_acknowledges_an_in_flight_write() {
    let dir = tempfile::tempdir().unwrap();
    let pause = dir.path().join("pause");
    let s = support::stack_with_env(&[("MUSTER_DEV_ACK_PAUSE_FILE", pause.display().to_string())])
        .await;
    let alice = s.login_user("alice", "alice@example.com").await;
    std::fs::write(&pause, b"").unwrap();
    let request = write(&s, &alice, reqwest::Method::POST, "/v1/workspaces")
        .json(&json!({"name": "In flight"}))
        .send();
    let in_flight = tokio::spawn(request);
    let reached = pause.with_extension("reached");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while !reached.exists() {
        assert!(std::time::Instant::now() < deadline, "the write never ran");
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    fence(&s.env).await;
    std::fs::remove_file(&pause).unwrap();
    let resp = in_flight.await.unwrap().unwrap();
    assert_eq!(resp.status(), 503, "an in-flight write was acknowledged");
}

async fn post_webhook(s: &Stack, paddle: &PaddleFixture, event: &serde_json::Value) {
    let bytes = serde_json::to_vec(event).unwrap();
    let sig = paddle.sign_webhook(&bytes, OffsetDateTime::now_utc().unix_timestamp());
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &sig)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// The background workers of a fenced instance claim nothing and call no
/// provider: a cancellation left pending (Paddle failing) is not retried
/// there once another incarnation is active.
#[tokio::test]
async fn a_fenced_instance_calls_no_provider_from_its_workers() {
    let paddle = start().await;
    let s = support::stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = support::fleet::create_workspace(&s, &alice, "Paid")
        .await
        .id;
    let resp = write(
        &s,
        &alice,
        reqwest::Method::POST,
        &format!("/v1/workspaces/{ws}/billing/checkout"),
    )
    .json(&json!({"plan": "personal", "seats": 1}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 200);
    let c: serde_json::Value = resp.json().await.unwrap();
    let sub = "sub_01h8vzp8f4kek3yd4g1fence01";
    let created = paddle.complete_checkout(
        c["transaction_id"].as_str().unwrap(),
        sub,
        "ctm_01h8vzp8f4kek3yd4g1fence01",
    );
    post_webhook(&s, &paddle, &created).await;

    // Paddle cannot be read for a while: the cancellation stays pending and
    // the worker retries it with backoff (2 s, 4 s, ...).
    paddle.fail_next_gets(3);
    let resp = write(
        &s,
        &alice,
        reqwest::Method::DELETE,
        &format!("/v1/workspaces/{ws}"),
    )
    .json(&json!({"confirm_workspace_id": ws, "confirm_workspace_name": "Paid"}))
    .send()
    .await
    .unwrap();
    assert_eq!(resp.status(), 202);

    fence(&s.env).await;
    let gets = paddle.calls("get_subscription");
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    assert_eq!(
        paddle.calls("get_subscription"),
        gets,
        "a fenced worker called the provider"
    );
    assert_eq!(paddle.calls("cancel"), 0);
    assert_eq!(paddle.subscription(sub).unwrap()["status"], "active");
}

/// Normal appends (here: incarnations registered by an operator) while the
/// active instance serves never make it refuse a request.
#[tokio::test]
async fn an_active_instance_is_never_refused_during_concurrent_appends() {
    let s = support::stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut readers = Vec::new();
    for _ in 0..4 {
        let url = s.url("/v1/workspaces");
        let cookie = format!("__Host-mc_session={}", alice.cookie);
        let stop = stop.clone();
        readers.push(tokio::spawn(async move {
            let mut refused = Vec::new();
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let status = http()
                    .get(&url)
                    .header("Cookie", &cookie)
                    .send()
                    .await
                    .unwrap()
                    .status();
                n += 1;
                if status != 200 {
                    refused.push(status.as_u16());
                }
            }
            (n, refused)
        }));
    }
    for _ in 0..8 {
        let db = fresh_database().await;
        let out = run_ops(&s.env, &["register", "--database-url", &db]).await;
        assert!(out.status.success(), "{}", text(&out));
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let mut total = 0;
    for r in readers {
        let (n, refused) = r.await.unwrap();
        total += n;
        assert!(refused.is_empty(), "refused during appends: {refused:?}");
    }
    assert!(total > 0);
}
