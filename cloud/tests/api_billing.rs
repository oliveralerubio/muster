mod support;

use muster_cloud::protocol::{
    error_codes, ApiError, CheckoutSession, CreateCheckout, CreateInvitation, CreatedInvitation,
    CustomerPortalSession, PlanStatus, Role, UpdatePlan, WorkspaceBilling,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::paddle_fixture::{
    start as start_paddle, PaddleFixture, SubscriptionEvent, PRICE_PERSONAL,
};
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
async fn checkout_transaction_creation_for_personal_and_team_plans() {
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
    assert_eq!(status, 200);
    let checkout: CheckoutSession = resp.json().await.unwrap();
    assert!(checkout
        .checkout_url
        .contains("sandbox-checkout.paddle.com"));
    assert!(!checkout.transaction_id.is_empty());

    // Team checkout with 5 seats.
    let (status, resp) = session_post(
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

    // 0. The owner starts a checkout: the server creates and records the
    // transaction for this workspace.
    let billing_path = format!("/v1/workspaces/{}/billing", ws.id);
    let txn = checkout(&s, &alice, &ws.id, "personal", 1).await;

    // 1. The customer pays; Paddle sends `subscription.created`.
    let sub_id = "sub_01h8vzp8f4kek3yd4g1wrksa31";
    let cust_id = "ctm_01h8vzp8f4kek3yd4g1wrksa31";
    let created = paddle.complete_checkout(&txn, sub_id, cust_id);
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan, "personal");
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert_eq!(billing.seats, 1);
    assert!(billing.can_push);

    // 2. Redelivery of the same event_id: acknowledged, nothing re-applied.
    let gets = paddle.calls("get_subscription");
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);
    assert_eq!(paddle.calls("get_subscription"), gets);

    // 3. Forged signature is refused with 401.
    let now_ts = time::OffsetDateTime::now_utc().unix_timestamp();
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

    // 4. Customer portal session for the bound customer.
    let portal_path = format!("{billing_path}/portal");
    let (status, resp) = session_post(&s, &alice, &portal_path, &serde_json::json!({})).await;
    assert_eq!(status, 200);
    let portal: CustomerPortalSession = resp.json().await.unwrap();
    assert!(portal.portal_url.contains("customer-portal.paddle.com"));

    // 5. Cancellation at period end: still paid through the period.
    let cancel_path = format!("{billing_path}/cancel");
    let (status, _) = session_post(&s, &alice, &cancel_path, &serde_json::json!({})).await;
    assert_eq!(status, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert!(billing.cancel_at_period_end);
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert!(billing.can_push);

    // 6. The period ends; Paddle cancels the subscription.
    let canceled = paddle.update_subscription(sub_id, "subscription.canceled", |sub| {
        sub["status"] = "canceled".into();
        sub["canceled_at"] = support::paddle_fixture::now_rfc3339().into();
        sub["scheduled_change"] = serde_json::Value::Null;
    });
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::Canceled);
    assert!(!billing.can_push);
    assert!(billing.can_pull);
    assert!(billing.can_export);

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

    // Pull and export keep working without payment.
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(resp.status(), 200);
    let export_path = format!("/v1/workspaces/{}/export", ws.id);
    let resp = http()
        .get(s.url(&export_path))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// A correctly signed event only proves Paddle sent it, not who it belongs to.
/// Without a checkout transaction this server created for the workspace,
/// `custom_data` (which a browser checkout can set freely) grants nothing.
#[tokio::test]
async fn webhook_without_server_side_checkout_association_grants_nothing() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Victim WS").await;
    let billing_path = format!("/v1/workspaces/{}/billing", ws.id);

    for (n, txn) in [(1, None), (2, Some("txn_01h8vzp8f4kek3yd4g1notours"))] {
        let event_id = format!("evt_01h8vzp8f4kek3yd4g1forged{n}");
        let sub_id = format!("sub_01h8vzp8f4kek3yd4g1forged{n}");
        let event = paddle.subscription_event(&SubscriptionEvent {
            event_id: &event_id,
            event_type: "subscription.created",
            sub_id: &sub_id,
            customer_id: "ctm_01h8vzp8f4kek3yd4g1attackr",
            ws_id: &ws.id,
            plan: "team",
            seats: 50,
            status: "active",
            occurred_at: "2026-11-14T00:00:00Z",
            period_start: "2026-11-14T00:00:00Z",
            period_end: "2027-11-14T00:00:00Z",
            transaction_id: txn,
        });
        let bytes = serde_json::to_vec(&event).unwrap();
        let sig = paddle.sign_webhook(&bytes, time::OffsetDateTime::now_utc().unix_timestamp());
        let resp = http()
            .post(s.url("/v1/billing/webhook"))
            .header("Paddle-Signature", &sig)
            .body(bytes)
            .send()
            .await
            .unwrap();
        // Authentic delivery: acknowledged so Paddle stops retrying.
        assert_eq!(resp.status(), 200);

        let (_, billing) = session_get::<WorkspaceBilling>(&s, &alice, &billing_path).await;
        let billing = billing.unwrap();
        assert_eq!(billing.plan, "trial", "case {n}");
        assert_eq!(billing.plan_status, PlanStatus::Trialing, "case {n}");
        assert_eq!(billing.seats, 1, "case {n}");
    }
}

async fn post_webhook(s: &Stack, paddle: &PaddleFixture, event: &serde_json::Value) -> u16 {
    let bytes = serde_json::to_vec(event).unwrap();
    let sig = paddle.sign_webhook(&bytes, time::OffsetDateTime::now_utc().unix_timestamp());
    http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &sig)
        .body(bytes)
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn checkout(s: &Stack, u: &UserSession, ws_id: &str, plan: &str, seats: u32) -> String {
    let (status, resp) = session_post(
        s,
        u,
        &format!("/v1/workspaces/{ws_id}/billing/checkout"),
        &CreateCheckout {
            plan: plan.into(),
            seats,
        },
    )
    .await;
    assert_eq!(status, 200);
    resp.json::<CheckoutSession>().await.unwrap().transaction_id
}

async fn billing_of(s: &Stack, u: &UserSession, ws_id: &str) -> WorkspaceBilling {
    let (status, billing) =
        session_get::<WorkspaceBilling>(s, u, &format!("/v1/workspaces/{ws_id}/billing")).await;
    assert_eq!(status, 200);
    billing.unwrap()
}

/// Entitlements come from the subscription Paddle returns on
/// `GET /subscriptions/{id}`, with the plan derived from the configured
/// price ids, not from the (possibly stale) event payload or custom_data.
#[tokio::test]
async fn billing_follows_canonical_subscription_not_event_payload() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Canonical WS").await;

    let txn = checkout(&s, &alice, &ws.id, "team", 3).await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1canon01";
    let mut created = paddle.complete_checkout(&txn, sub_id, "ctm_01h8vzp8f4kek3yd4g1canon01");
    // A stale snapshot in the payload must not decide anything.
    created["data"]["items"] = serde_json::json!([
        { "status": "active", "quantity": 1, "price": { "id": PRICE_PERSONAL } }
    ]);
    created["data"]["status"] = "past_due".into();
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);

    assert!(paddle.calls("get_subscription") >= 1);
    let canonical = paddle.subscription(sub_id).unwrap();
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan, "team");
    assert_eq!(billing.seats, 3);
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert!(billing.can_push);
    assert_eq!(
        billing.current_period_end.as_deref(),
        canonical["current_billing_period"]["ends_at"].as_str()
    );
    assert_eq!(billing.trial_ends_at, None);
}

/// Late, duplicate and equal-timestamp deliveries converge on Paddle's
/// current state: an old `subscription.updated` arriving after the
/// cancellation (same `occurred_at`) does not resurrect the plan.
#[tokio::test]
async fn out_of_order_and_equal_timestamp_events_converge_on_canonical_state() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Order WS").await;
    let txn = checkout(&s, &alice, &ws.id, "team", 2).await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1order01";
    let created = paddle.complete_checkout(&txn, sub_id, "ctm_01h8vzp8f4kek3yd4g1order01");

    // Paddle changes the subscription twice; the webhooks race.
    let updated = paddle.update_subscription(sub_id, "subscription.updated", |sub| {
        sub["items"][0]["quantity"] = 4.into();
    });
    let mut canceled = paddle.update_subscription(sub_id, "subscription.canceled", |sub| {
        sub["status"] = "canceled".into();
        sub["scheduled_change"] = serde_json::Value::Null;
    });
    canceled["occurred_at"] = updated["occurred_at"].clone();

    // Delivered newest first, then the older ones (created arrives last).
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);
    assert_eq!(post_webhook(&s, &paddle, &updated).await, 200);
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);
    assert_eq!(post_webhook(&s, &paddle, &updated).await, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::Canceled);
    assert_eq!(billing.plan, "team");
    assert_eq!(billing.seats, 4);
    assert!(!billing.can_push);
}

/// Once a workspace is bound to a Paddle customer, a subscription of a
/// different customer grants it nothing, even through a checkout it created.
#[tokio::test]
async fn subscription_of_a_foreign_customer_grants_nothing() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Bound WS").await;

    let txn1 = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let first = paddle.complete_checkout(
        &txn1,
        "sub_01h8vzp8f4kek3yd4g1bound01",
        "ctm_01h8vzp8f4kek3yd4g1ownerA",
    );
    assert_eq!(post_webhook(&s, &paddle, &first).await, 200);
    let canceled = paddle.update_subscription(
        "sub_01h8vzp8f4kek3yd4g1bound01",
        "subscription.canceled",
        |sub| {
            sub["status"] = "canceled".into();
        },
    );
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);
    assert_eq!(
        billing_of(&s, &alice, &ws.id).await.plan_status,
        PlanStatus::Canceled
    );

    // Someone else pays a checkout of this workspace with their own customer.
    let txn2 = checkout(&s, &alice, &ws.id, "team", 10).await;
    let foreign = paddle.complete_checkout(
        &txn2,
        "sub_01h8vzp8f4kek3yd4g1foreig1",
        "ctm_01h8vzp8f4kek3yd4g1otherB",
    );
    assert_eq!(post_webhook(&s, &paddle, &foreign).await, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::Canceled);
    assert_eq!(billing.plan, "personal");
    assert!(!billing.can_push);
}

/// A workspace that canceled and subscribes again (same customer) gets the
/// new subscription's entitlements, not the old canceled one's.
#[tokio::test]
async fn resubscribing_after_cancellation_restores_entitlements() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Again WS").await;
    let customer = "ctm_01h8vzp8f4kek3yd4g1again01";

    let txn1 = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let old_sub = "sub_01h8vzp8f4kek3yd4g1again01";
    let first = paddle.complete_checkout(&txn1, old_sub, customer);
    assert_eq!(post_webhook(&s, &paddle, &first).await, 200);
    let canceled = paddle.update_subscription(old_sub, "subscription.canceled", |sub| {
        sub["status"] = "canceled".into();
    });
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);
    assert_eq!(
        billing_of(&s, &alice, &ws.id).await.plan_status,
        PlanStatus::Canceled
    );

    let txn2 = checkout(&s, &alice, &ws.id, "team", 3).await;
    let again = paddle.complete_checkout(&txn2, "sub_01h8vzp8f4kek3yd4g1again02", customer);
    assert_eq!(post_webhook(&s, &paddle, &again).await, 200);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert_eq!(billing.plan, "team");
    assert_eq!(billing.seats, 3);
    assert!(billing.can_push);

    // The sync gate agrees with the billing view.
    let key = MachineKey::new("node-again");
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
}

/// Paddle being down when the event arrives does not lose the payment: the
/// reconciliation is queued durably, survives a service restart, and is
/// retried until Paddle answers, without waiting for another webhook.
#[tokio::test]
async fn reconciliation_is_retried_durably_across_restart_without_new_events() {
    let paddle = start_paddle().await;
    let mut s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Retry WS").await;
    let txn = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let created = paddle.complete_checkout(
        &txn,
        "sub_01h8vzp8f4kek3yd4g1retry001",
        "ctm_01h8vzp8f4kek3yd4g1retry001",
    );

    paddle.fail_next_gets(usize::MAX);
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);
    assert!(paddle.calls("get_subscription") >= 1);
    // Nothing granted from the payload while Paddle cannot confirm it.
    assert_eq!(billing_of(&s, &alice, &ws.id).await.plan, "trial");

    s.restart().await;
    paddle.fail_next_gets(0);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let billing = billing_of(&s, &alice, &ws.id).await;
        if billing.plan == "personal" {
            assert_eq!(billing.plan_status, PlanStatus::Active);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "reconciliation never retried (still {:?} {:?})",
            billing.plan,
            billing.plan_status
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

async fn active_subscription(
    s: &Stack,
    paddle: &PaddleFixture,
    u: &UserSession,
    ws_id: &str,
    sub_id: &str,
) {
    let txn = checkout(s, u, ws_id, "personal", 1).await;
    let customer = format!("ctm_{}", &sub_id[4..]);
    let created = paddle.complete_checkout(&txn, sub_id, &customer);
    assert_eq!(post_webhook(s, paddle, &created).await, 200);
    assert_eq!(
        billing_of(s, u, ws_id).await.plan_status,
        PlanStatus::Active
    );
}

/// Cancelling asks Paddle once, keeps the paid-through period from Paddle's
/// answer, and a repeated request is a no-op instead of a second cancel.
#[tokio::test]
async fn cancel_is_confirmed_by_paddle_and_idempotent() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Cancel WS").await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1cancel1";
    active_subscription(&s, &paddle, &alice, &ws.id, sub_id).await;

    let cancel_path = format!("/v1/workspaces/{}/billing/cancel", ws.id);
    for _ in 0..2 {
        let (status, _) = session_post(&s, &alice, &cancel_path, &serde_json::json!({})).await;
        assert_eq!(status, 200);
    }
    assert_eq!(paddle.calls("cancel"), 1);

    let canonical = paddle.subscription(sub_id).unwrap();
    assert_eq!(canonical["scheduled_change"]["action"], "cancel");
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert!(billing.cancel_at_period_end);
    assert_eq!(billing.plan_status, PlanStatus::Active);
    assert!(billing.can_push);
    assert_eq!(
        billing.current_period_end.as_deref(),
        canonical["scheduled_change"]["effective_at"].as_str()
    );
}

/// A plan or seat change is applied from Paddle's canonical subscription
/// right after Paddle accepts it, not left until some later webhook.
#[tokio::test]
async fn plan_change_is_applied_from_paddle_without_waiting_for_a_webhook() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Upgrade WS").await;
    active_subscription(
        &s,
        &paddle,
        &alice,
        &ws.id,
        "sub_01h8vzp8f4kek3yd4g1upgrd01",
    )
    .await;

    let (status, _) = session_post(
        &s,
        &alice,
        &format!("/v1/workspaces/{}/billing/plan", ws.id),
        &UpdatePlan {
            plan: "team".into(),
            seats: 4,
        },
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(paddle.calls("update"), 1);

    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan, "team");
    assert_eq!(billing.seats, 4);
}

/// Owners and admins manage billing; members can read it but not change it.
#[tokio::test]
async fn only_owners_and_admins_manage_billing() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let carol = s.login_user("carol", "carol@example.com").await;
    let ws = create_workspace(&s, &alice, "Roles WS").await;
    for (who, email, role) in [
        (&bob, "bob@example.com", Role::Member),
        (&carol, "carol@example.com", Role::Admin),
    ] {
        let (status, resp) = session_post(
            &s,
            &alice,
            &format!("/v1/workspaces/{}/invitations", ws.id),
            &CreateInvitation {
                email: email.into(),
                role,
            },
        )
        .await;
        assert_eq!(status, 201);
        let inv: CreatedInvitation = resp.json().await.unwrap();
        let (status, _) = session_post(&s, who, &inv.accept_path, &serde_json::json!({})).await;
        assert!(status.is_success());
    }
    active_subscription(
        &s,
        &paddle,
        &alice,
        &ws.id,
        "sub_01h8vzp8f4kek3yd4g1roles001",
    )
    .await;

    let base = format!("/v1/workspaces/{}/billing", ws.id);
    let actions: [(&str, serde_json::Value); 4] = [
        (
            "checkout",
            serde_json::json!({ "plan": "team", "seats": 2 }),
        ),
        ("portal", serde_json::json!({})),
        ("plan", serde_json::json!({ "plan": "team", "seats": 2 })),
        ("cancel", serde_json::json!({})),
    ];
    for (action, body) in &actions {
        let (status, resp) = session_post(&s, &bob, &format!("{base}/{action}"), body).await;
        assert_eq!(status, 403, "member {action}");
        assert_eq!(
            resp.json::<ApiError>().await.unwrap().error.code,
            error_codes::FORBIDDEN_ROLE
        );
    }
    assert_eq!(billing_of(&s, &bob, &ws.id).await.plan, "personal");
    for (action, body) in &actions {
        let (status, _) = session_post(&s, &carol, &format!("{base}/{action}"), body).await;
        // Authorized; checkout is then refused because a subscription is live.
        let expected = if *action == "checkout" { 409 } else { 200 };
        assert_eq!(status, expected, "admin {action}");
    }
}

/// A second checkout while a subscription is live would bill the customer
/// twice: it is refused before any transaction is created at Paddle.
#[tokio::test]
async fn checkout_is_refused_while_a_subscription_is_live() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Live WS").await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1livesb1";
    active_subscription(&s, &paddle, &alice, &ws.id, sub_id).await;
    let created_before = paddle.calls("create_transaction");

    let (status, resp) = session_post(
        &s,
        &alice,
        &format!("/v1/workspaces/{}/billing/checkout", ws.id),
        &CreateCheckout {
            plan: "team".into(),
            seats: 2,
        },
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::SUBSCRIPTION_EXISTS
    );
    assert_eq!(paddle.calls("create_transaction"), created_before);

    // Once Paddle has canceled it, a new checkout is possible again.
    let canceled = paddle.update_subscription(sub_id, "subscription.canceled", |sub| {
        sub["status"] = "canceled".into();
    });
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);
    checkout(&s, &alice, &ws.id, "team", 2).await;
}

/// Entitlements follow Paddle's status with no locally invented windows,
/// and the billing view and the sync gate agree:
/// - `past_due` (Paddle is retrying the payment) keeps push, even after the
///   period end; Paddle itself later cancels or pauses the subscription;
/// - `paused` restricts push (pull and export stay available).
#[tokio::test]
async fn past_due_and_paused_follow_paddle_and_agree_with_the_sync_gate() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Dunning WS").await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1dunnin1";
    active_subscription(&s, &paddle, &alice, &ws.id, sub_id).await;

    let key = MachineKey::new("node-dunning");
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    let mut head = repo.head();
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    let past = (time::OffsetDateTime::now_utc() - time::Duration::days(10))
        .replace_nanosecond(0)
        .unwrap()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let past_due = paddle.update_subscription(sub_id, "subscription.past_due", |sub| {
        sub["status"] = "past_due".into();
        sub["current_billing_period"]["ends_at"] = past.clone().into();
    });
    assert_eq!(post_webhook(&s, &paddle, &past_due).await, 200);
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::PastDue);
    assert!(billing.can_push);
    let next = repo.touch("dunning.txt", "still paying");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 200, "past_due push");
    head = next;

    let paused = paddle.update_subscription(sub_id, "subscription.paused", |sub| {
        sub["status"] = "paused".into();
    });
    assert_eq!(post_webhook(&s, &paddle, &paused).await, 200);
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan_status, PlanStatus::Restricted);
    assert!(!billing.can_push);
    assert!(billing.can_pull && billing.can_export);
    repo.touch("paused.txt", "no");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 402, "paused push");
    assert_eq!(dev.send(&s, &dev.get(&dev.sync_path())).await.status(), 200);
}

/// Double clicks, client retries and two admins racing on the same workspace
/// produce one checkout: the same Paddle transaction for everyone, and only
/// one transaction created at Paddle.
#[tokio::test]
async fn concurrent_checkouts_for_the_same_plan_share_one_paddle_transaction() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Race WS").await;

    let attempts = (0..8).map(|_| checkout(&s, &alice, &ws.id, "team", 3));
    let txns = futures::future::join_all(attempts).await;

    assert!(txns.iter().all(|t| t == &txns[0]), "{txns:?}");
    assert_eq!(paddle.calls("create_transaction"), 1);
}

/// A paid checkout is never offered again: after its subscription ends, a
/// new checkout of the same plan gets a new Paddle transaction.
#[tokio::test]
async fn a_paid_checkout_is_not_offered_again() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Paid WS").await;
    let sub_id = "sub_01h8vzp8f4kek3yd4g1paidonc";
    let first_txn = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let created = paddle.complete_checkout(&first_txn, sub_id, "ctm_01h8vzp8f4kek3yd4g1paidonc");
    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);
    let canceled = paddle.update_subscription(sub_id, "subscription.canceled", |sub| {
        sub["status"] = "canceled".into();
    });
    assert_eq!(post_webhook(&s, &paddle, &canceled).await, 200);

    let second_txn = checkout(&s, &alice, &ws.id, "personal", 1).await;
    assert_ne!(second_txn, first_txn);
    assert_eq!(paddle.calls("create_transaction"), 2);
}

/// Asking for a different plan while a checkout is open cancels the old
/// transaction at Paddle first, so its payment link can no longer be paid.
#[tokio::test]
async fn changing_plan_before_paying_cancels_the_previous_transaction_at_paddle() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Switch WS").await;

    let personal = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let team = checkout(&s, &alice, &ws.id, "team", 3).await;
    assert_ne!(personal, team);
    assert_eq!(paddle.transaction(&personal).unwrap()["status"], "canceled");
    assert_eq!(paddle.transaction(&team).unwrap()["status"], "draft");
    assert_eq!(paddle.calls("cancel_transaction"), 1);

    // The new choice is now the one reused.
    assert_eq!(checkout(&s, &alice, &ws.id, "team", 3).await, team);
}

/// The open checkout was already paid but Paddle's webhook has not arrived:
/// Paddle refuses to cancel it, so no second transaction is created, and the
/// late webhook still grants the plan through the kept association.
#[tokio::test]
async fn a_checkout_paid_before_its_webhook_blocks_a_second_checkout() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Inflight WS").await;

    let paid = checkout(&s, &alice, &ws.id, "personal", 1).await;
    let created = paddle.complete_checkout(
        &paid,
        "sub_01h8vzp8f4kek3yd4g1inflt01",
        "ctm_01h8vzp8f4kek3yd4g1inflt01",
    );

    let (status, resp) = session_post(
        &s,
        &alice,
        &format!("/v1/workspaces/{}/billing/checkout", ws.id),
        &CreateCheckout {
            plan: "team".into(),
            seats: 5,
        },
    )
    .await;
    assert_eq!(status, 409);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::SUBSCRIPTION_EXISTS
    );
    assert_eq!(paddle.calls("create_transaction"), 1);

    assert_eq!(post_webhook(&s, &paddle, &created).await, 200);
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.plan, "personal");
    assert_eq!(billing.plan_status, PlanStatus::Active);
}

/// Concurrent requests for different plans on one workspace: every request
/// gets an answer, and at the end exactly one transaction is still payable
/// at Paddle, the one the server offers now.
#[tokio::test]
async fn concurrent_checkouts_for_different_plans_leave_one_payable_transaction() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Flip WS").await;

    let attempts = (0..8).map(|i| {
        let (plan, seats) = if i % 2 == 0 {
            ("personal", 1)
        } else {
            ("team", 2)
        };
        checkout(&s, &alice, &ws.id, plan, seats)
    });
    futures::future::join_all(attempts).await;

    let payable = paddle.payable_transactions();
    assert_eq!(payable.len(), 1, "{payable:?}");
    let offered_now = checkout(&s, &alice, &ws.id, "team", 2).await;
    let offered_again = checkout(&s, &alice, &ws.id, "team", 2).await;
    assert_eq!(offered_now, offered_again);
    assert_eq!(paddle.payable_transactions(), vec![offered_now]);
}
