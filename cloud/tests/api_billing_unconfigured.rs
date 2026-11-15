mod support;

use muster_cloud::protocol::{error_codes, ApiError, CreateCheckout, PlanStatus, WorkspaceBilling};
use support::fleet::create_workspace;
use support::{http, stack, Stack, UserSession};

async fn session_get<T: serde::de::DeserializeOwned>(
    s: &Stack,
    u: &UserSession,
    path: &str,
) -> (reqwest::StatusCode, Option<T>) {
    let resp = http()
        .get(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = if status.is_success() {
        Some(resp.json().await.unwrap())
    } else {
        None
    };
    (status, body)
}

async fn session_post(
    s: &Stack,
    u: &UserSession,
    path: &str,
    body: &impl serde::Serialize,
) -> (reqwest::StatusCode, reqwest::Response) {
    let resp = http()
        .post(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .json(body)
        .send()
        .await
        .unwrap();
    (resp.status(), resp)
}

#[tokio::test]
async fn unconfigured_billing_serves_trial_and_refuses_mutations_with_503() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Trial Workspace").await;

    // 1. GET /v1/workspaces/{ws}/billing: reports trial and unconfigured status.
    let billing_path = format!("/v1/workspaces/{}/billing", ws.id);
    let (status, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
    assert_eq!(status, 200);
    let billing = billing.unwrap();
    assert_eq!(billing.workspace_id, ws.id);
    assert!(!billing.billing_configured);
    assert_eq!(billing.plan, "trial");
    assert_eq!(billing.plan_status, PlanStatus::Trialing);
    assert_eq!(billing.seats, 1);
    assert!(billing.trial_ends_at.is_some());
    assert!(billing.can_push);
    assert!(billing.can_pull);
    assert!(billing.can_export);
    assert_eq!(billing.max_devices, 3);
    assert_eq!(billing.max_bundle_bytes, 10 * 1024 * 1024);
    assert_eq!(billing.max_storage_bytes, 1024 * 1024 * 1024);

    // 2. Mutations return 503 billing_unconfigured.
    let checkout_path = format!("{billing_path}/checkout");
    let (status, resp) = session_post(
        &s,
        &alice,
        &checkout_path,
        &CreateCheckout {
            plan: "personal".into(),
            seats: 1,
        },
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::BILLING_UNCONFIGURED
    );

    let portal_path = format!("{billing_path}/portal");
    let (status, resp) = session_post(&s, &alice, &portal_path, &serde_json::json!({})).await;
    assert_eq!(status, 503);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::BILLING_UNCONFIGURED
    );

    let cancel_path = format!("{billing_path}/cancel");
    let (status, resp) = session_post(&s, &alice, &cancel_path, &serde_json::json!({})).await;
    assert_eq!(status, 503);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::BILLING_UNCONFIGURED
    );

    let plan_path = format!("{billing_path}/plan");
    let (status, resp) = session_post(
        &s,
        &alice,
        &plan_path,
        &serde_json::json!({"plan": "team", "seats": 2}),
    )
    .await;
    assert_eq!(status, 503);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::BILLING_UNCONFIGURED
    );

    // Webhook without configuration returns 503.
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .body(b"{}".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::BILLING_UNCONFIGURED
    );
}
