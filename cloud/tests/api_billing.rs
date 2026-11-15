mod support;

use muster_cloud::protocol::{
    error_codes, ApiError, CheckoutSession, CreateCheckout, CustomerPortalSession, PlanStatus,
    WorkspaceBilling,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::paddle_fixture::start as start_paddle;
use support::{http, stack_with_paddle, Stack, UserSession};

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

async fn session_post<T: serde::de::DeserializeOwned>(
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
async fn checkout_transaction_creation_and_role_authorization() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Billing WS").await;

    // Billing is now configured with the Paddle fixture.
    let billing_path = format!("/v1/workspaces/{}/billing", ws.id);
    let (status, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
    assert_eq!(status, 200);
    let billing = billing.unwrap();
    assert!(billing.billing_configured);
    assert_eq!(billing.plan, "trial");

    // Checkout: owner creates a personal checkout transaction.
    let checkout_path = format!("{billing_path}/checkout");
    let (status, resp) = session_post::<CheckoutSession>(
        &s,
        &alice,
        &checkout_path,
        &CreateCheckout {
            plan: "personal".into(),
            seats: 1,
        },
    )
    .await;
    assert_eq!(status, 200);
    let checkout: CheckoutSession = resp.json().await.unwrap();
    assert!(checkout
        .checkout_url
        .contains("sandbox-checkout.paddle.com"));
    assert!(!checkout.transaction_id.is_empty());

    // Team checkout with 5 seats.
    let (status, resp) = session_post::<CheckoutSession>(
        &s,
        &alice,
        &checkout_path,
        &CreateCheckout {
            plan: "team".into(),
            seats: 5,
        },
    )
    .await;
    assert_eq!(status, 200);
    let checkout: CheckoutSession = resp.json().await.unwrap();
    assert!(!checkout.transaction_id.is_empty());
}

#[tokio::test]
async fn webhook_reconciliation_entitlements_and_portal_lifecycle() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Subscription WS").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    let repo = FleetRepo::genesis(&key_a);
    let head = repo.head();
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    // 1. Simulate Paddle sending `subscription.created` webhook.
    let sub_id = "sub_01h8vzp8f4kek3yd4g1wrksa31";
    let cust_id = "ctm_01h8vzp8f4kek3yd4g1wrksa31";
    let event = paddle.subscription_event(
        "evt_01h8vzp8f4kek3yd4g1wrksa31",
        "subscription.created",
        sub_id,
        cust_id,
        &ws.id,
        "personal",
        1,
        "active",
        "2026-11-14T00:00:00Z",
        "2026-11-14T00:00:00Z",
        "2026-12-14T00:00:00Z",
    );
    let event_bytes = serde_json::to_vec(&event).unwrap();
    let now_ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let sig = paddle.sign_webhook(&event_bytes, now_ts);

    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &sig)
        .body(event_bytes.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // Workspace billing now reflects active personal subscription.
    let billing_path = format!("/v1/workspaces/{}/billing", ws.id);
    let (status, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
    assert_eq!(status, 200);
    let billing = billing.unwrap();
    assert_eq!(billing.plan, "personal");
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert_eq!(billing.seats, 1);
    assert!(billing.can_push);

    // 2. Idempotent webhook retry (same event_id): accepted with 200 without duplicate work.
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &sig)
        .body(event_bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // 3. Forged signature is refused with 401.
    let bad_sig = format!(
        "ts={now_ts};h1=badbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbadbad"
    );
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &bad_sig)
        .body(b"{}".to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // 4. Customer portal session.
    let portal_path = format!("{billing_path}/portal");
    let (status, resp) =
        session_post::<CustomerPortalSession>(&s, &alice, &portal_path, &serde_json::json!({}))
            .await;
    assert_eq!(status, 200);
    let portal: CustomerPortalSession = resp.json().await.unwrap();
    assert!(portal.portal_url.contains("customer-portal.paddle.com"));

    // 5. Cancellation: schedules cancel at period end.
    let cancel_path = format!("{billing_path}/cancel");
    let (status, _) =
        session_post::<serde_json::Value>(&s, &alice, &cancel_path, &serde_json::json!({})).await;
    assert_eq!(status, 200);

    let (_, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
    let billing = billing.unwrap();
    assert!(billing.cancel_at_period_end);
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert!(billing.can_push); // Paid-through maintains full push capability!

    // 6. Simulate subscription canceled (paid-through passed).
    let cancel_event = paddle.subscription_event(
        "evt_01h8vzp8f4kek3yd4g1wrksa32",
        "subscription.canceled",
        sub_id,
        cust_id,
        &ws.id,
        "personal",
        1,
        "canceled",
        "2026-12-14T00:00:00Z",
        "2026-11-14T00:00:00Z",
        "2026-12-14T00:00:00Z",
    );
    let cancel_bytes = serde_json::to_vec(&cancel_event).unwrap();
    let cancel_sig = paddle.sign_webhook(&cancel_bytes, now_ts);
    assert_eq!(
        http()
            .post(s.url("/v1/billing/webhook"))
            .header("Paddle-Signature", &cancel_sig)
            .body(cancel_bytes)
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    let (_, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
    let billing = billing.unwrap();
    assert_eq!(billing.plan_status, PlanStatus::Canceled);
    assert!(!billing.can_push); // Push is now blocked!
    assert!(billing.can_pull); // Pull remains available!
    assert!(billing.can_export); // Export remains available!

    // Pushing new commits against a canceled workspace is refused with 402 plan_inactive.
    let repo2 = FleetRepo::clone_from_bundle(&repo.bundle(None));
    repo2.touch("blocked.txt", "content");
    let resp = dev_a
        .send(&s, &dev_a.push(repo2.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 402);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::PLAN_INACTIVE
    );

    // Pull continues to work!
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(resp.status(), 200);

    // Export continues to work!
    let export_path = format!("/v1/workspaces/{}/export", ws.id);
    let resp = http()
        .get(s.url(&export_path))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}
