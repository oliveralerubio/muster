mod support;

use support::{http, stack};

#[tokio::test]
async fn healthz_reports_process_alive() {
    let s = stack().await;
    let resp = http().get(s.url("/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.headers().contains_key("x-request-id"));
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"status": "ok"}));
}

#[tokio::test]
async fn readyz_reports_database_ready() {
    let s = stack().await;
    let resp = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let body: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(body, serde_json::json!({"status": "ready"}));
}

/// The harness chooses the service's port before starting it (its public
/// URL must be known up front). When something else takes that port first,
/// the start moves to a new port with a coherent public URL instead of
/// failing the test, and a restart keeps working.
#[tokio::test]
async fn a_service_whose_chosen_port_is_taken_starts_on_a_new_one() {
    let taken = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let mut s = support::stack_with_env(&[
        ("MUSTER_BIND", format!("127.0.0.1:{port}")),
        ("MUSTER_PUBLIC_URL", format!("http://127.0.0.1:{port}")),
    ])
    .await;

    let base = url::Url::parse(&s.cloud.base).unwrap();
    assert_ne!(base.port(), Some(port), "started on the taken port");
    let env_of = |s: &support::Stack, name: &str| -> Vec<String> {
        s.env
            .iter()
            .filter(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .collect()
    };
    let new_port = base.port().unwrap();
    assert_eq!(
        env_of(&s, "MUSTER_BIND"),
        vec![format!("127.0.0.1:{new_port}")]
    );
    assert_eq!(
        env_of(&s, "MUSTER_PUBLIC_URL"),
        vec![format!("http://127.0.0.1:{new_port}")]
    );
    // The public URL is the one OIDC redirects to: a login proves it.
    s.login_user("alice", "alice@example.com").await;
    s.restart().await;
    let resp = http().get(s.url("/healthz")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    drop(taken);
}

/// Strict-Transport-Security is sent on every response when the public URL is
/// https (the harness talks plain http to the process; only the configuration
/// differs), and never when it is http.
#[tokio::test]
async fn hsts_is_sent_only_when_the_public_url_is_https() {
    const PATHS: [&str; 4] = ["/healthz", "/", "/assets/app.js", "/no-such-route"];

    let plain = stack().await;
    for path in PATHS {
        let resp = http().get(plain.url(path)).send().await.unwrap();
        assert!(
            !resp.headers().contains_key("strict-transport-security"),
            "{path}: no HSTS over an http public URL"
        );
    }

    let s = support::stack_with_env(&[(
        "MUSTER_PUBLIC_URL",
        "https://cloud.example.test".to_string(),
    )])
    .await;
    for path in PATHS {
        let resp = http().get(s.url(path)).send().await.unwrap();
        assert_eq!(
            resp.headers()
                .get("strict-transport-security")
                .and_then(|v| v.to_str().ok()),
            Some("max-age=31536000"),
            "{path}: HSTS over an https public URL"
        );
    }
    // Next to the headers the console already sets.
    let resp = http().get(s.url("/")).send().await.unwrap();
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert!(resp.headers().contains_key("content-security-policy"));
}
