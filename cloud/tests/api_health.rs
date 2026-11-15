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
