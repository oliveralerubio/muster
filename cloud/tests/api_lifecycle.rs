//! Workspace lifecycle over the public HTTP API: the owner-confirmed
//! deletion request (one idempotent operation with fixed deadlines), the
//! writes it denies, the pull/export grace it keeps, and the immediate
//! provider cancellation it drives against the external Paddle fixture.
//!
//! Responses are read as raw JSON on purpose: the wire shape is the frozen
//! contract. SQL is never used to assert behaviour.
mod support;

use serde_json::{json, Value};
use support::fleet::create_workspace;
use support::{http, stack, Stack, UserSession};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

async fn session_get(s: &Stack, u: &UserSession, path: &str) -> (u16, Value) {
    let resp = http()
        .get(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn session_post(s: &Stack, u: &UserSession, path: &str, body: &Value) -> (u16, Value) {
    let resp = http()
        .post(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(body)
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `DELETE /v1/workspaces/{ws}` with raw body bytes and an optional key.
async fn delete_raw(
    s: &Stack,
    u: &UserSession,
    ws: &str,
    key: Option<&str>,
    body: Option<Vec<u8>>,
) -> (u16, Value) {
    let mut req = http()
        .delete(s.url(&format!("/v1/workspaces/{ws}")))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token);
    if let Some(key) = key {
        req = req.header("Idempotency-Key", key);
    }
    if let Some(body) = body {
        req = req.header("Content-Type", "application/json").body(body);
    }
    let resp = req.send().await.unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn confirm(id: &str, name: &str) -> Vec<u8> {
    serde_json::to_vec(&json!({"confirm_workspace_id": id, "confirm_workspace_name": name}))
        .unwrap()
}

async fn request_deletion(s: &Stack, u: &UserSession, id: &str, name: &str) -> Value {
    let key = uuid::Uuid::new_v4().to_string();
    let (status, body) = delete_raw(s, u, id, Some(&key), Some(confirm(id, name))).await;
    assert_eq!(status, 202, "{body}");
    body
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("<none>")
}

fn ts(v: &Value) -> OffsetDateTime {
    OffsetDateTime::parse(
        v.as_str().unwrap_or_else(|| panic!("not a timestamp: {v}")),
        &Rfc3339,
    )
    .unwrap()
}

/// Invites `who` into `ws` with `role` and accepts the invitation.
async fn add_member(s: &Stack, owner: &UserSession, who: &UserSession, ws: &str, role: &str) {
    let (status, created) = session_post(
        s,
        owner,
        &format!("/v1/workspaces/{ws}/invitations"),
        &json!({"email": who.email, "role": role}),
    )
    .await;
    assert_eq!(status, 201, "{created}");
    let (status, body) =
        session_post(s, who, created["accept_path"].as_str().unwrap(), &json!({})).await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn an_active_workspace_reports_an_active_lifecycle_to_members_only() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let mallory = s.login_user("mallory", "mallory@example.com").await;
    let ws = create_workspace(&s, &alice, "Alive").await;
    let path = format!("/v1/workspaces/{}/lifecycle", ws.id);

    let (status, body) = session_get(&s, &alice, &path).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["workspace_id"], ws.id.as_str());
    assert_eq!(body["state"], "active");
    assert!(body["deletion"].is_null(), "{body}");

    let (status, body) = session_get(&s, &alice, &format!("/v1/workspaces/{}", ws.id)).await;
    assert_eq!(status, 200);
    assert_eq!(body["lifecycle_state"], "active", "{body}");

    let (status, body) = session_get(&s, &mallory, &path).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn deletion_needs_the_owner_a_key_and_the_exact_workspace_id_and_name() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let carol = s.login_user("carol", "carol@example.com").await;
    let mallory = s.login_user("mallory", "mallory@example.com").await;
    let ws = create_workspace(&s, &alice, "Precious Data").await;
    add_member(&s, &alice, &bob, &ws.id, "admin").await;
    add_member(&s, &alice, &carol, &ws.id, "member").await;
    let key = || Some(uuid::Uuid::new_v4().to_string());
    let good = || Some(confirm(&ws.id, "Precious Data"));

    // Not a member: indistinguishable from a missing workspace.
    let (status, body) = delete_raw(&s, &mallory, &ws.id, key().as_deref(), good()).await;
    assert_eq!(status, 404, "{body}");
    // Admins and members cannot delete.
    for who in [&bob, &carol] {
        let (status, body) = delete_raw(&s, who, &ws.id, key().as_deref(), good()).await;
        assert_eq!(status, 403, "{body}");
        assert_eq!(code(&body), "forbidden_role");
    }
    // The owner must send a key and an exact confirmation.
    let (status, body) = delete_raw(&s, &alice, &ws.id, None, good()).await;
    assert_eq!(status, 400, "{body}");
    assert_eq!(body["error"]["details"]["header"], "Idempotency-Key");
    let (status, body) = delete_raw(&s, &alice, &ws.id, key().as_deref(), None).await;
    assert_eq!(status, 400, "{body}");
    let other = uuid::Uuid::new_v4().to_string();
    for bad in [
        confirm(&ws.id, "precious data"),
        confirm(&ws.id, "Precious Data "),
        confirm(&ws.id, ""),
        confirm(&other, "Precious Data"),
        confirm(&ws.id.to_uppercase(), "Precious Data"),
    ] {
        let (status, body) = delete_raw(&s, &alice, &ws.id, key().as_deref(), Some(bad)).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(code(&body), "confirmation_mismatch", "{body}");
    }

    // Nothing above scheduled anything.
    let (status, body) =
        session_get(&s, &alice, &format!("/v1/workspaces/{}/lifecycle", ws.id)).await;
    assert_eq!(status, 200);
    assert_eq!(body["state"], "active", "{body}");
}

#[tokio::test]
async fn one_idempotent_request_with_deadlines_fixed_at_request_time() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let ws = create_workspace(&s, &alice, "Doomed").await;
    add_member(&s, &alice, &bob, &ws.id, "member").await;

    let before = OffsetDateTime::now_utc();
    let key = uuid::Uuid::new_v4().to_string();
    let (status, first) = delete_raw(
        &s,
        &alice,
        &ws.id,
        Some(&key),
        Some(confirm(&ws.id, "Doomed")),
    )
    .await;
    let after = OffsetDateTime::now_utc();
    assert_eq!(status, 202, "{first}");
    assert_eq!(first["workspace_id"], ws.id.as_str());
    assert_eq!(first["state"], "deletion_scheduled");
    let d = &first["deletion"];
    let requested_at = ts(&d["requested_at"]);
    assert!(
        requested_at >= before - time::Duration::seconds(2)
            && requested_at <= after + time::Duration::seconds(2),
        "{d}"
    );
    assert!(d["request_id"].as_str().is_some_and(|v| !v.is_empty()));
    assert_eq!(
        ts(&d["export_until"]),
        requested_at + time::Duration::days(30)
    );
    assert_eq!(d["primary"]["status"], "scheduled");
    assert_eq!(
        ts(&d["primary"]["purge_at"]),
        requested_at + time::Duration::days(30)
    );
    assert!(d["primary"]["purged_at"].is_null());
    // No backup catalog exists yet: honest unknown, only the policy cap.
    assert_eq!(d["backups"]["status"], "unknown");
    assert!(d["backups"]["deadline"].is_null(), "{d}");
    assert!(d["backups"]["purged_at"].is_null());
    assert_eq!(
        ts(&d["backups"]["cap_at"]),
        requested_at + time::Duration::days(35)
    );
    assert!(d["backups"]["reason"]
        .as_str()
        .is_some_and(|r| !r.is_empty()));
    // No billing: nothing to cancel.
    assert_eq!(d["provider"]["status"], "none", "{d}");
    assert_eq!(d["writes_allowed"], false);
    assert_eq!(d["pull_allowed"], true);
    assert_eq!(d["export_allowed"], true);

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    // Same key, same body: the same request.
    let (status, replay) = delete_raw(
        &s,
        &alice,
        &ws.id,
        Some(&key),
        Some(confirm(&ws.id, "Doomed")),
    )
    .await;
    assert_eq!(status, 202, "{replay}");
    assert_eq!(replay["deletion"], first["deletion"]);
    // Same key, different (still valid) body bytes: refused.
    let spaced = format!(
        "{{ \"confirm_workspace_id\": \"{}\", \"confirm_workspace_name\": \"Doomed\" }}",
        ws.id
    );
    let (status, body) =
        delete_raw(&s, &alice, &ws.id, Some(&key), Some(spaced.into_bytes())).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(code(&body), "idempotency_mismatch");
    // A new key never re-schedules or extends: the same request comes back.
    let again = request_deletion(&s, &alice, &ws.id, "Doomed").await;
    assert_eq!(again["deletion"], first["deletion"]);

    // Any member reads the same state; the workspace advertises it.
    let path = format!("/v1/workspaces/{}/lifecycle", ws.id);
    let (status, seen) = session_get(&s, &bob, &path).await;
    assert_eq!(status, 200);
    assert_eq!(seen, first);
    let (_, detail) = session_get(&s, &bob, &format!("/v1/workspaces/{}", ws.id)).await;
    assert_eq!(detail["lifecycle_state"], "deletion_scheduled", "{detail}");
    let (_, list) = session_get(&s, &alice, "/v1/workspaces").await;
    assert_eq!(list[0]["lifecycle_state"], "deletion_scheduled", "{list}");
}

#[tokio::test]
async fn the_request_survives_a_restart_unchanged() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Durable").await;
    let first = request_deletion(&s, &alice, &ws.id, "Durable").await;
    s.restart().await;
    let path = format!("/v1/workspaces/{}/lifecycle", ws.id);
    let (status, body) = session_get(&s, &alice, &path).await;
    assert_eq!(status, 200);
    assert_eq!(body, first);
}

/// With the development grace shortened, the primary purge becomes due (not
/// purged: nothing is purged in this unit) and the cap keeps its ratio.
#[tokio::test]
async fn an_expired_grace_is_due_not_purged() {
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Quick").await;
    let first = request_deletion(&s, &alice, &ws.id, "Quick").await;
    let d = &first["deletion"];
    let requested_at = ts(&d["requested_at"]);
    assert_eq!(
        ts(&d["primary"]["purge_at"]),
        requested_at + time::Duration::seconds(1)
    );
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let (_, body) = session_get(&s, &alice, &format!("/v1/workspaces/{}/lifecycle", ws.id)).await;
    assert_eq!(body["deletion"]["primary"]["status"], "due", "{body}");
    assert!(body["deletion"]["primary"]["purged_at"].is_null());
    assert_eq!(body["deletion"]["export_until"], d["export_until"]);
}

// ---------------------------------------------------------------------------
// Writes refused after the request; pull and export stay available
// ---------------------------------------------------------------------------

use muster_cloud::protocol::{
    canonical_link_v1, canonical_receipt_v1, AccountToken, CreateProposal, LinkDeviceRequest,
    ProposalKind, ReceiptOutcome, ReceiptV1, ResolveProposal, SubmitReceipt,
};
use support::fleet::{
    account_token, fresh_nonce, link, now, Device, FleetRepo, MachineKey, Signed,
};

/// `POST /devices` with a chosen `Idempotency-Key` (to replay it later).
async fn link_request(
    s: &Stack,
    token: &AccountToken,
    ws: &str,
    key: &MachineKey,
    idem: &str,
) -> (u16, Value) {
    let pk = key.public();
    let proof = key.signer.sign(&canonical_link_v1(
        ws,
        &token.account_id,
        &key.name,
        &pk,
        &key.recipient,
    ));
    let resp = http()
        .post(s.url(&format!("/v1/workspaces/{ws}/devices")))
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Idempotency-Key", idem)
        .json(&LinkDeviceRequest {
            machine_name: key.name.clone(),
            signing_key: pk,
            recipient: key.recipient.clone(),
            proof,
        })
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn receipt(dev: &Device<'_>, revision: &str, sequence: u64) -> SubmitReceipt {
    let outcome = ReceiptOutcome::Applied;
    let applied_at = "2026-11-16T12:00:00Z";
    let signature = dev.key.signer.sign(&canonical_receipt_v1(&ReceiptV1 {
        workspace_id: &dev.workspace,
        device_id: &dev.id,
        revision,
        epoch: 1,
        sequence,
        outcome: outcome.as_str(),
        applied_at,
    }));
    SubmitReceipt {
        revision: revision.into(),
        epoch: 1,
        sequence,
        outcome,
        applied_at: applied_at.into(),
        signature,
    }
}

/// The same signed request again (same body and `Idempotency-Key`), with a
/// fresh nonce and timestamp: what a client retry sends.
fn again(req: &Signed) -> Signed {
    Signed {
        nonce: fresh_nonce(),
        timestamp: now(),
        ..req.clone()
    }
}

async fn status_and_body(resp: reqwest::Response) -> (u16, Value) {
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

fn assert_denied(what: &str, (status, body): (u16, Value), request_id: &Value) {
    assert_eq!(status, 409, "{what}: {body}");
    assert_eq!(
        code(&body),
        "workspace_deletion_scheduled",
        "{what}: {body}"
    );
    assert_eq!(
        &body["error"]["details"]["request_id"], request_id,
        "{what}"
    );
}

#[tokio::test]
async fn writes_and_their_replays_are_refused_once_requested_while_pull_and_export_remain() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let carol = s.login_user("carol", "carol@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Closing").await;
    let key_a = MachineKey::new("node-a");
    let dev = link(&s, &token, &ws.id, &key_a).await;
    let repo = FleetRepo::genesis(&key_a);
    let genesis = repo.head();

    // Successful requests made BEFORE the deletion request, kept to replay.
    let push = dev.push(repo.bundle(None), None);
    assert_eq!(dev.send(&s, &push).await.status(), 200);
    let proposals = format!("/v1/workspaces/{}/proposals", ws.id);
    let restore = CreateProposal {
        kind: ProposalKind::Restore,
        base_revision: genesis.clone(),
        payload: json!({"target_revision": genesis, "paths": ["muster.toml"]}),
    };
    let resp = dev.send(&s, &dev.post_json(&proposals, &restore)).await;
    let (status, proposal) = status_and_body(resp).await;
    assert_eq!(status, 201, "{proposal}");
    let receipts = format!("/v1/workspaces/{}/receipts", ws.id);
    let first_receipt = dev.post_json(&receipts, &receipt(&dev, &genesis, 1));
    assert!(dev.send(&s, &first_receipt).await.status().is_success());
    let key_b = MachineKey::new("node-b");
    let link_key = uuid::Uuid::new_v4().to_string();
    let (status, body) = link_request(&s, &token, &ws.id, &key_b, &link_key).await;
    assert_eq!(status, 201, "{body}");
    let linked: muster_cloud::protocol::LinkedDevice = serde_json::from_value(body).unwrap();
    let dev_b = Device {
        key: &key_b,
        workspace: ws.id.clone(),
        id: linked.device_id.clone(),
        token: linked.device_token.clone(),
        linked,
    };
    // The pending machine proposes its own enrollment (idempotent).
    let enroll = CreateProposal {
        kind: ProposalKind::Enrollment,
        base_revision: genesis.clone(),
        payload: json!({
            "name": key_b.name, "recipient": key_b.recipient, "signing_key": key_b.public(),
            "nonce": "n0nce-1234567890abcdef", "platform": "linux-x86_64",
            "muster_version": "0.1.0", "agents": [], "created_at": "2026-11-16T00:00:00Z",
        }),
    };
    let resp = dev_b.send(&s, &dev_b.post_json(&proposals, &enroll)).await;
    assert_eq!(resp.status(), 201);
    let (status, invitation) = session_post(
        &s,
        &alice,
        &format!("/v1/workspaces/{}/invitations", ws.id),
        &json!({"email": bob.email, "role": "member"}),
    )
    .await;
    assert_eq!(status, 201, "{invitation}");

    let requested = request_deletion(&s, &alice, &ws.id, "Closing").await;
    let rid = &requested["deletion"]["request_id"];

    // Fleet writes, new and replayed.
    assert_denied(
        "push replay",
        status_and_body(dev.send(&s, &again(&push)).await).await,
        rid,
    );
    repo.touch("notes.txt", "late change");
    let late = dev.push(repo.bundle(Some(&genesis)), Some(&genesis));
    assert_denied(
        "new push",
        status_and_body(dev.send(&s, &late).await).await,
        rid,
    );
    // Proposals: replayed create, new create, resolve.
    assert_denied(
        "proposal replay",
        status_and_body(dev.send(&s, &dev.post_json(&proposals, &restore)).await).await,
        rid,
    );
    assert_denied(
        "enrollment replay",
        status_and_body(dev_b.send(&s, &dev_b.post_json(&proposals, &enroll)).await).await,
        rid,
    );
    let other = CreateProposal {
        payload: json!({"target_revision": genesis, "paths": ["other.toml"]}),
        ..restore.clone()
    };
    assert_denied(
        "new proposal",
        status_and_body(dev.send(&s, &dev.post_json(&proposals, &other)).await).await,
        rid,
    );
    let resolve = format!("{proposals}/{}/resolve", proposal["id"].as_str().unwrap());
    let reject = ResolveProposal {
        result_revision: None,
    };
    assert_denied(
        "resolve",
        status_and_body(dev.send(&s, &dev.post_json(&resolve, &reject)).await).await,
        rid,
    );
    // Receipts: replayed and new.
    assert_denied(
        "receipt replay",
        status_and_body(dev.send(&s, &again(&first_receipt)).await).await,
        rid,
    );
    assert_denied(
        "new receipt",
        status_and_body(
            dev.send(&s, &dev.post_json(&receipts, &receipt(&dev, &genesis, 2)))
                .await,
        )
        .await,
        rid,
    );
    // Links: replayed and new.
    assert_denied(
        "link replay",
        link_request(&s, &token, &ws.id, &key_b, &link_key).await,
        rid,
    );
    let key_c = MachineKey::new("node-c");
    assert_denied(
        "new link",
        link_request(
            &s,
            &token,
            &ws.id,
            &key_c,
            &uuid::Uuid::new_v4().to_string(),
        )
        .await,
        rid,
    );
    // Invitations: new and accepting one issued before.
    assert_denied(
        "new invitation",
        session_post(
            &s,
            &alice,
            &format!("/v1/workspaces/{}/invitations", ws.id),
            &json!({"email": carol.email, "role": "member"}),
        )
        .await,
        rid,
    );
    assert_denied(
        "accept",
        session_post(
            &s,
            &bob,
            invitation["accept_path"].as_str().unwrap(),
            &json!({}),
        )
        .await,
        rid,
    );

    // The grace stays meaningful: the device still pulls the accepted head,
    // the owner still exports it, and nothing moved.
    let resp = dev.send(&s, &dev.get(&dev.sync_path())).await;
    assert_eq!(resp.status(), 200, "pull after the request");
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        genesis
    );
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{}/export", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "export after the request");
    assert!(!resp.bytes().await.unwrap().is_empty());
    let (status, _) = session_get(&s, &alice, &format!("/v1/workspaces/{}/revisions", ws.id)).await;
    assert_eq!(status, 200);
}

// ---------------------------------------------------------------------------
// Immediate provider cancellation (external Paddle fixture)
// ---------------------------------------------------------------------------

use support::paddle_fixture::{start as start_paddle, PaddleFixture};
use support::stack_with_paddle;

async fn post_webhook(s: &Stack, paddle: &PaddleFixture, event: &Value) {
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

async fn checkout(s: &Stack, u: &UserSession, ws: &str) -> (u16, Value) {
    session_post(
        s,
        u,
        &format!("/v1/workspaces/{ws}/billing/checkout"),
        &json!({"plan": "personal", "seats": 1}),
    )
    .await
}

async fn billing_plan_status(s: &Stack, u: &UserSession, ws: &str) -> String {
    let (status, b) = session_get(s, u, &format!("/v1/workspaces/{ws}/billing")).await;
    assert_eq!(status, 200, "{b}");
    b["plan_status"].as_str().unwrap().to_string()
}

/// A workspace with a live Paddle subscription `sub` (checkout paid and its
/// `subscription.created` delivered).
async fn subscribed(
    s: &Stack,
    paddle: &PaddleFixture,
    u: &UserSession,
    name: &str,
    sub: &str,
) -> String {
    let ws = create_workspace(s, u, name).await;
    let (status, c) = checkout(s, u, &ws.id).await;
    assert_eq!(status, 200, "{c}");
    let created = paddle.complete_checkout(
        c["transaction_id"].as_str().unwrap(),
        sub,
        "ctm_01h8vzp8f4kek3yd4g1lifec01",
    );
    post_webhook(s, paddle, &created).await;
    assert_eq!(billing_plan_status(s, u, &ws.id).await, "active");
    ws.id
}

/// Polls the lifecycle until the provider cancellation is no longer pending.
async fn provider_settled(s: &Stack, u: &UserSession, ws: &str, secs: u64) -> Value {
    let path = format!("/v1/workspaces/{ws}/lifecycle");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let (status, body) = session_get(s, u, &path).await;
        assert_eq!(status, 200, "{body}");
        if body["deletion"]["provider"]["status"] != "pending" {
            return body;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "still pending: {body}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

fn item<'a>(lifecycle: &'a Value, kind: &str, id: &str) -> &'a Value {
    lifecycle["deletion"]["provider"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == kind && i["id"] == id)
        .unwrap_or_else(|| panic!("no {kind} {id}: {lifecycle}"))
}

#[tokio::test]
async fn a_live_subscription_is_canceled_immediately_and_confirmed_by_paddle() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let sub = "sub_01h8vzp8f4kek3yd4g1lifec01";
    let ws = subscribed(&s, &paddle, &alice, "Paid", sub).await;

    let requested = request_deletion(&s, &alice, &ws, "Paid").await;
    let settled = provider_settled(&s, &alice, &ws, 10).await;
    assert_eq!(
        settled["deletion"]["provider"]["status"], "confirmed",
        "{settled}"
    );
    let it = item(&settled, "subscription", sub);
    assert_eq!(it["status"], "confirmed");
    assert!(it["confirmed_at"].is_string());
    // Canceled NOW at Paddle, not at the end of the period.
    let canonical = paddle.subscription(sub).unwrap();
    assert_eq!(canonical["status"], "canceled", "{canonical}");
    assert!(canonical["scheduled_change"].is_null());
    assert_eq!(paddle.calls("cancel"), 1);
    // Deadlines are untouched by the provider work.
    assert_eq!(
        settled["deletion"]["export_until"],
        requested["deletion"]["export_until"]
    );

    // A retried request never cancels again.
    request_deletion(&s, &alice, &ws, "Paid").await;
    assert_eq!(paddle.calls("cancel"), 1);

    // Billing changes are refused like any other write.
    let rid = &requested["deletion"]["request_id"];
    assert_denied("checkout", checkout(&s, &alice, &ws).await, rid);
    assert_denied(
        "plan",
        session_post(
            &s,
            &alice,
            &format!("/v1/workspaces/{ws}/billing/plan"),
            &json!({"plan": "team", "seats": 3}),
        )
        .await,
        rid,
    );
    assert_denied(
        "cancel",
        session_post(
            &s,
            &alice,
            &format!("/v1/workspaces/{ws}/billing/cancel"),
            &json!({}),
        )
        .await,
        rid,
    );
    assert_eq!(paddle.calls("update"), 0);
}

#[tokio::test]
async fn a_paddle_failure_keeps_the_cancellation_pending_and_it_is_retried_after_a_restart() {
    let paddle = start_paddle().await;
    let mut s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let sub = "sub_01h8vzp8f4kek3yd4g1lifec02";
    let ws = subscribed(&s, &paddle, &alice, "Flaky", sub).await;

    // Paddle cannot be read: nothing may be reported canceled.
    paddle.fail_next_gets(1);
    let requested = request_deletion(&s, &alice, &ws, "Flaky").await;
    let p = &requested["deletion"]["provider"];
    assert_eq!(p["status"], "pending", "{requested}");
    let it = item(&requested, "subscription", sub);
    assert_eq!(it["status"], "pending");
    assert!(
        it["last_error"].as_str().is_some_and(|e| !e.is_empty()),
        "{it}"
    );
    assert!(it["confirmed_at"].is_null());
    assert_eq!(paddle.subscription(sub).unwrap()["status"], "active");

    // The pending cancellation is durable and retried by the new process.
    s.restart().await;
    let settled = provider_settled(&s, &alice, &ws, 20).await;
    assert_eq!(
        settled["deletion"]["provider"]["status"], "confirmed",
        "{settled}"
    );
    assert_eq!(paddle.subscription(sub).unwrap()["status"], "canceled");
    assert_eq!(
        settled["deletion"]["requested_at"],
        requested["deletion"]["requested_at"]
    );
}

#[tokio::test]
async fn an_open_checkout_is_canceled_and_one_paid_meanwhile_cancels_its_subscription() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;

    // Open checkout, never paid: no longer payable.
    let ws = create_workspace(&s, &alice, "Open").await;
    let (_, c) = checkout(&s, &alice, &ws.id).await;
    let txn = c["transaction_id"].as_str().unwrap().to_string();
    request_deletion(&s, &alice, &ws.id, "Open").await;
    let settled = provider_settled(&s, &alice, &ws.id, 10).await;
    assert_eq!(
        item(&settled, "checkout", &txn)["status"],
        "confirmed",
        "{settled}"
    );
    assert_eq!(paddle.transaction(&txn).unwrap()["status"], "canceled");
    assert!(paddle.payable_transactions().is_empty());

    // Paid before the request, its webhook not seen yet: the subscription
    // Paddle created from it is found and canceled too.
    let ws2 = create_workspace(&s, &alice, "Raced").await;
    let (_, c) = checkout(&s, &alice, &ws2.id).await;
    let txn2 = c["transaction_id"].as_str().unwrap().to_string();
    let sub2 = "sub_01h8vzp8f4kek3yd4g1lifec03";
    let created = paddle.complete_checkout(&txn2, sub2, "ctm_01h8vzp8f4kek3yd4g1lifec03");
    request_deletion(&s, &alice, &ws2.id, "Raced").await;
    let settled = provider_settled(&s, &alice, &ws2.id, 10).await;
    assert_eq!(
        settled["deletion"]["provider"]["status"], "confirmed",
        "{settled}"
    );
    assert_eq!(item(&settled, "checkout", &txn2)["status"], "confirmed");
    assert_eq!(item(&settled, "subscription", sub2)["status"], "confirmed");
    assert_eq!(paddle.subscription(sub2).unwrap()["status"], "canceled");
    // The late webhook changes nothing.
    post_webhook(&s, &paddle, &created).await;
    let after = provider_settled(&s, &alice, &ws2.id, 10).await;
    assert_eq!(
        after["deletion"]["provider"],
        settled["deletion"]["provider"]
    );
}

#[tokio::test]
async fn a_subscription_that_turns_live_after_the_request_is_canceled_too() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let sub = "sub_01h8vzp8f4kek3yd4g1lifec04";
    let ws = subscribed(&s, &paddle, &alice, "Revived", sub).await;
    // Canceled before the request: nothing to cancel at request time.
    let ev = paddle.update_subscription(sub, "subscription.canceled", |v| {
        v["status"] = "canceled".into();
    });
    post_webhook(&s, &paddle, &ev).await;
    assert_eq!(billing_plan_status(&s, &alice, &ws).await, "canceled");
    let requested = request_deletion(&s, &alice, &ws, "Revived").await;
    assert_eq!(
        requested["deletion"]["provider"]["status"], "none",
        "{requested}"
    );

    // Paddle reports it live again afterwards: it is registered and canceled.
    let ev = paddle.update_subscription(sub, "subscription.activated", |v| {
        v["status"] = "active".into();
        v["canceled_at"] = Value::Null;
    });
    post_webhook(&s, &paddle, &ev).await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let settled = loop {
        let (_, body) = session_get(&s, &alice, &format!("/v1/workspaces/{ws}/lifecycle")).await;
        if body["deletion"]["provider"]["status"] == "confirmed" {
            break body;
        }
        assert!(std::time::Instant::now() < deadline, "{body}");
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    };
    assert_eq!(item(&settled, "subscription", sub)["status"], "confirmed");
    assert_eq!(paddle.subscription(sub).unwrap()["status"], "canceled");
}

/// The request and a checkout race on several workspaces: whichever wins,
/// no payable payment link survives a deletion request.
#[tokio::test]
async fn a_checkout_racing_the_request_never_leaves_a_payable_link() {
    let paddle = start_paddle().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let mut names = Vec::new();
    for i in 0..6 {
        let name = format!("Race {i}");
        let ws = create_workspace(&s, &alice, &name).await;
        names.push((ws.id, name));
    }
    let races = names.iter().map(|(id, name)| {
        let (s, alice) = (&s, &alice);
        async move {
            let key = uuid::Uuid::new_v4().to_string();
            let (del, co) = tokio::join!(
                delete_raw(s, alice, id, Some(&key), Some(confirm(id, name))),
                checkout(s, alice, id),
            );
            assert_eq!(del.0, 202, "{}", del.1);
            assert!(
                co.0 == 200 || (co.0 == 409 && code(&co.1) == "workspace_deletion_scheduled"),
                "{co:?}"
            );
        }
    });
    futures::future::join_all(races).await;
    for (id, _) in &names {
        let settled = provider_settled(&s, &alice, id, 15).await;
        assert_ne!(settled["deletion"]["provider"]["status"], "pending");
    }
    assert!(
        paddle.payable_transactions().is_empty(),
        "payable after deletion: {:?}",
        paddle.payable_transactions()
    );
}

// ---------------------------------------------------------------------------
// Durability: journaled before acknowledged, replayed before readiness
// ---------------------------------------------------------------------------

/// The service dies right after journaling the request (development crash
/// point), before projecting or answering. The next process replays the
/// record before it is ready: the request exists, writes are refused, and
/// a retry returns that same request.
#[tokio::test]
async fn a_request_journaled_before_a_crash_is_replayed_before_readiness() {
    let mut s =
        support::stack_with_env(&[("MUSTER_DEV_LIFECYCLE_CRASH", "after_journal".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = create_workspace(&s, &alice, "Crashy").await;
    let sent = http()
        .delete(s.url(&format!("/v1/workspaces/{}", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .header("Content-Type", "application/json")
        .body(confirm(&ws.id, "Crashy"))
        .send()
        .await;
    assert!(
        sent.is_err(),
        "the process must die before answering: {sent:?}"
    );

    s.env.retain(|(k, _)| *k != "MUSTER_DEV_LIFECYCLE_CRASH");
    s.restart().await;
    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200);
    let (status, seen) =
        session_get(&s, &alice, &format!("/v1/workspaces/{}/lifecycle", ws.id)).await;
    assert_eq!(status, 200);
    assert_eq!(seen["state"], "deletion_scheduled", "{seen}");
    let rid = seen["deletion"]["request_id"].clone();
    assert_denied(
        "invitation after replay",
        session_post(
            &s,
            &alice,
            &format!("/v1/workspaces/{}/invitations", ws.id),
            &json!({"email": "bob@example.com", "role": "member"}),
        )
        .await,
        &rid,
    );
    let retried = request_deletion(&s, &alice, &ws.id, "Crashy").await;
    assert_eq!(retried["deletion"], seen["deletion"]);
}

#[tokio::test]
async fn lifecycle_development_settings_are_refused_in_production() {
    for (name, value) in [
        ("MUSTER_DEV_DELETION_GRACE_SECS", "1"),
        ("MUSTER_DEV_LIFECYCLE_CRASH", "after_journal"),
        ("MUSTER_DEV_ACK_PAUSE_FILE", "/tmp/muster-ack-pause"),
    ] {
        // An otherwise acceptable production configuration.
        let env: Vec<(&str, String)> = vec![
            ("MUSTER_ENV", "production".into()),
            ("MUSTER_BIND", "127.0.0.1:0".into()),
            ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
            ("MUSTER_PUBLIC_URL", "https://app.example.com".into()),
            ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
            ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
            ("MUSTER_OIDC_CLIENT_ID", "client_123".into()),
            ("MUSTER_OIDC_CLIENT_SECRET", "secret_123".into()),
            (name, value.into()),
        ];
        let (code, out) = support::run_cloud_expecting_exit(&env).await;
        assert_eq!(code, Some(78), "{name}: {out}");
        assert!(out.contains(name), "{name}: {out}");
    }
    // Malformed values are refused in development too.
    for (name, value) in [
        ("MUSTER_DEV_DELETION_GRACE_SECS", "0"),
        ("MUSTER_DEV_LIFECYCLE_CRASH", "sometimes"),
    ] {
        let s = stack().await;
        let mut env = s.env.clone();
        env.push((name, value.into()));
        let (code, out) = support::run_cloud_expecting_exit(&env).await;
        assert_eq!(code, Some(78), "{name}: {out}");
        assert!(out.contains(name), "{name}: {out}");
    }
}
