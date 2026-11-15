//! Browser integration test runner: boots the real `muster-cloud` binary,
//! the external OIDC fixture and an ephemeral PostgreSQL database, then runs
//! the Playwright browser suite against the live `http://localhost:<port>` URL.

#[path = "support/mod.rs"]
mod support;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State as AxumState};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use muster_cloud::protocol::{
    body_sha256_hex, canonical_link_v1, canonical_request_v1, AccountToken, ApiError,
    CreateInvitation, CreateProposal, CreateWorkspace, CreatedInvitation, DeviceApproveRequest,
    DeviceAuthorization, DeviceAuthorizeRequest, DeviceDecision, DeviceTokenRequest,
    LinkDeviceRequest, LinkedDevice, Proposal, ProposalKind, SignedRequestV1, SyncHeld, Workspace,
    BASE_REVISION_NONE, DEVICE_CODE_GRANT_TYPE, HEADER_BASE_REVISION, HEADER_CONTENT_SHA256,
    HEADER_DEVICE_ID, HEADER_IDEMPOTENCY_KEY, HEADER_NONCE, HEADER_SIGNATURE, HEADER_TIMESTAMP,
};
use serde::Serialize;
use support::fleet::{fresh_nonce, now, FleetRepo, MachineKey};
use tokio::process::Command;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedingRevision {
    revision: String,
    message: String,
    epoch: u64,
    signer: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrowserFixtures {
    history_workspace_id: String,
    history_workspace_name: String,
    revisions: Vec<SeedingRevision>,
    conflict_proposal_id: String,
    conflict_revision: String,
    enrollment_proposal_id: String,
    enrollment_machine_name: String,
    quorum_workspace_id: String,
    quorum_workspace_name: String,
    pending_quorum_proposal_id: String,
    pending_quorum_revision: String,
}

struct Session {
    cookie: String,
    csrf_token: String,
}

struct SignedDevice<'a> {
    workspace: String,
    id: String,
    token: String,
    key: &'a MachineKey,
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

async fn login_user(base: &str, subject: &str, email: &str) -> Session {
    let client = support::http();
    let login_resp = client
        .get(format!("{base}/auth/login"))
        .send()
        .await
        .unwrap();
    let (login_cookie, _) = support::set_cookie(&login_resp, "__Host-mc_login").unwrap();
    let auth_url = format!(
        "{}&login_as={subject}&email={email}",
        login_resp.headers()["location"].to_str().unwrap()
    );
    let idp_resp = client.get(&auth_url).send().await.unwrap();
    let cb_url = idp_resp.headers()["location"].to_str().unwrap();
    let cb_resp = client
        .get(cb_url)
        .header("Cookie", format!("__Host-mc_login={login_cookie}"))
        .send()
        .await
        .unwrap();
    let (session_cookie, _) = support::set_cookie(&cb_resp, "__Host-mc_session").unwrap();
    let me_resp = client
        .get(format!("{base}/v1/me"))
        .header("Cookie", format!("__Host-mc_session={session_cookie}"))
        .send()
        .await
        .unwrap();
    let me: muster_cloud::protocol::Me = me_resp.json().await.unwrap();
    Session {
        cookie: session_cookie,
        csrf_token: me.csrf_token,
    }
}

async fn account_token(base: &str, session: &Session) -> AccountToken {
    let client = support::http();
    let auth: DeviceAuthorization = client
        .post(format!("{base}/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "seed-cli".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let resp = client
        .post(format!("{base}/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", session.cookie))
        .header("Origin", base)
        .header("X-CSRF-Token", &session.csrf_token)
        .json(&DeviceApproveRequest {
            user_code: auth.user_code,
            decision: DeviceDecision::Approve,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    client
        .post(format!("{base}/v1/device/token"))
        .json(&DeviceTokenRequest {
            grant_type: DEVICE_CODE_GRANT_TYPE.into(),
            device_code: auth.device_code,
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn create_workspace(base: &str, session: &Session, name: &str) -> Workspace {
    let resp = support::http()
        .post(format!("{base}/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", session.cookie))
        .header("Origin", base)
        .header("X-CSRF-Token", &session.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&CreateWorkspace { name: name.into() })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    resp.json().await.unwrap()
}

async fn invite_and_accept(
    base: &str,
    owner: &Session,
    workspace_id: &str,
    member: &Session,
    email: &str,
    role: muster_cloud::protocol::Role,
) {
    let inv_resp = support::http()
        .post(format!("{base}/v1/workspaces/{workspace_id}/invitations"))
        .header("Cookie", format!("__Host-mc_session={}", owner.cookie))
        .header("Origin", base)
        .header("X-CSRF-Token", &owner.csrf_token)
        .json(&CreateInvitation {
            email: email.into(),
            role,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(inv_resp.status(), 201);
    let created: CreatedInvitation = inv_resp.json().await.unwrap();
    let accept_resp = support::http()
        .post(format!("{base}/v1/invitations/{}/accept", created.token))
        .header("Cookie", format!("__Host-mc_session={}", member.cookie))
        .header("Origin", base)
        .header("X-CSRF-Token", &member.csrf_token)
        .send()
        .await
        .unwrap();
    assert_eq!(accept_resp.status(), 200);
}

async fn link_device<'a>(
    base: &str,
    token: &AccountToken,
    workspace: &str,
    key: &'a MachineKey,
) -> SignedDevice<'a> {
    let pk = key.public();
    let proof = key.signer.sign(&canonical_link_v1(
        workspace,
        &token.account_id,
        &key.name,
        &pk,
        &key.recipient,
    ));
    let resp = support::http()
        .post(format!("{base}/v1/workspaces/{workspace}/devices"))
        .header("Authorization", format!("Bearer {}", token.access_token))
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&LinkDeviceRequest {
            machine_name: key.name.clone(),
            signing_key: pk,
            recipient: key.recipient.clone(),
            proof,
        })
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    let linked: LinkedDevice = resp.json().await.unwrap();
    SignedDevice {
        workspace: workspace.into(),
        id: linked.device_id,
        token: linked.device_token,
        key,
    }
}

async fn push_bundle(
    base: &str,
    dev: &SignedDevice<'_>,
    bundle: Vec<u8>,
    base_rev: Option<&str>,
) -> reqwest::Response {
    let path = format!("/v1/workspaces/{}/sync", dev.workspace);
    let sha = body_sha256_hex(&bundle);
    let base_val = base_rev.unwrap_or(BASE_REVISION_NONE);
    let idem = uuid::Uuid::new_v4().to_string();
    let ts = now().to_string();
    let nonce = fresh_nonce();
    let sig_bytes = canonical_request_v1(&SignedRequestV1 {
        method: "POST",
        path_and_query: &path,
        workspace_id: &dev.workspace,
        device_id: &dev.id,
        body_sha256_hex: &sha,
        base_revision: base_val,
        idempotency_key: &idem,
        timestamp: &ts,
        nonce: &nonce,
    });
    let signature = format!("v1={}", dev.key.signer.sign(&sig_bytes));
    support::http()
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {}", dev.token))
        .header(HEADER_DEVICE_ID, &dev.id)
        .header(HEADER_TIMESTAMP, &ts)
        .header(HEADER_NONCE, &nonce)
        .header(HEADER_CONTENT_SHA256, &sha)
        .header(HEADER_SIGNATURE, signature)
        .header(HEADER_BASE_REVISION, base_val)
        .header(HEADER_IDEMPOTENCY_KEY, &idem)
        .body(bundle)
        .send()
        .await
        .unwrap()
}

async fn signed_post_json(
    base: &str,
    dev: &SignedDevice<'_>,
    path: &str,
    body: &impl Serialize,
) -> reqwest::Response {
    let bytes = serde_json::to_vec(body).unwrap();
    let sha = body_sha256_hex(&bytes);
    let ts = now().to_string();
    let nonce = fresh_nonce();
    let sig_bytes = canonical_request_v1(&SignedRequestV1 {
        method: "POST",
        path_and_query: path,
        workspace_id: &dev.workspace,
        device_id: &dev.id,
        body_sha256_hex: &sha,
        base_revision: "",
        idempotency_key: "",
        timestamp: &ts,
        nonce: &nonce,
    });
    let signature = format!("v1={}", dev.key.signer.sign(&sig_bytes));
    support::http()
        .post(format!("{base}{path}"))
        .header("Authorization", format!("Bearer {}", dev.token))
        .header(HEADER_DEVICE_ID, &dev.id)
        .header(HEADER_TIMESTAMP, &ts)
        .header(HEADER_NONCE, &nonce)
        .header(HEADER_CONTENT_SHA256, &sha)
        .header(HEADER_SIGNATURE, signature)
        .header("Content-Type", "application/json")
        .body(bytes)
        .send()
        .await
        .unwrap()
}

async fn seed_browser_fixtures(base: &str) -> BrowserFixtures {
    let olivia = login_user(base, "olivia-sub-1", "olivia@example.com").await;
    let peter = login_user(base, "peter-sub-1", "peter@example.com").await;
    let olivia_token = account_token(base, &olivia).await;

    // 1. History & Export workspace: genesis -> sealed config+vault commit -> conflict -> enrollment
    let ws_name = "History & Export Fleet";
    let ws = create_workspace(base, &olivia, ws_name).await;
    invite_and_accept(
        base,
        &olivia,
        &ws.id,
        &peter,
        "peter@example.com",
        muster_cloud::protocol::Role::Member,
    )
    .await;

    let key_a = MachineKey::new("alpha-box");
    let dev_a = link_device(base, &olivia_token, &ws.id, &key_a).await;

    let mut repo = FleetRepo::genesis(&key_a);
    let head1 = repo.head();
    let genesis_bundle = repo.bundle(None);
    let push1 = push_bundle(base, &dev_a, genesis_bundle.clone(), None).await;
    assert_eq!(push1.status(), 200);

    fs::write(
        repo.config.join("settings.toml"),
        "[agent]\nmodel = \"opus\"\n",
    )
    .unwrap();
    fs::write(
        repo.config.join("vault/default.age"),
        b"SUPER_SECRET_CIPHERTEXT_MARKER_999",
    )
    .unwrap();
    let head2 = repo.seal_commit(&key_a, "update config and vault");
    let push2 = push_bundle(base, &dev_a, repo.bundle(Some(&head1)), Some(&head1)).await;
    assert_eq!(push2.status(), 200);

    // Real CAS conflict: divergent commit based on stale head1 -> 409 revision_conflict
    let stale_branch = FleetRepo::clone_from_bundle(&genesis_bundle);
    let conflict_rev = stale_branch.touch("notes.txt", "divergent branch notes");
    let conflict_resp = push_bundle(
        base,
        &dev_a,
        stale_branch.bundle(Some(&head1)),
        Some(&head1),
    )
    .await;
    assert_eq!(conflict_resp.status(), 409);
    let conflict_err: ApiError = conflict_resp.json().await.unwrap();
    let conflict_proposal_id = conflict_err.error.details["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Real enrollment proposal from unapproved second machine
    let key_b = MachineKey::new("beta-box");
    let dev_b = link_device(base, &olivia_token, &ws.id, &key_b).await;
    let enroll_req = CreateProposal {
        kind: ProposalKind::Enrollment,
        base_revision: head2.clone(),
        payload: serde_json::json!({
            "name": key_b.name,
            "recipient": key_b.recipient,
            "signing_key": key_b.public(),
            "nonce": "n0nce-browser-seed",
            "platform": "linux-x86_64",
            "muster_version": "0.1.0",
            "agents": [],
            "created_at": "2026-11-14T00:00:00Z",
        }),
    };
    let enroll_resp = signed_post_json(
        base,
        &dev_b,
        &format!("/v1/workspaces/{}/proposals", ws.id),
        &enroll_req,
    )
    .await;
    assert_eq!(enroll_resp.status(), 201);
    let enroll_prop: Proposal = enroll_resp.json().await.unwrap();

    // 2. Quorum governance workspace: 2-of-2 admins, 1-of-2 revoke -> 202 pending_quorum
    let q_name = "Quorum Governance Fleet";
    let q_ws = create_workspace(base, &olivia, q_name).await;
    let key_q1 = MachineKey::new("quorum-a");
    let key_q2 = MachineKey::new("quorum-b");
    let dev_q1 = link_device(base, &olivia_token, &q_ws.id, &key_q1).await;

    let mut q_repo = FleetRepo::team_genesis(&key_q1, &key_q2);
    let q_head1 = q_repo.head();
    let q_push1 = push_bundle(base, &dev_q1, q_repo.bundle(None), None).await;
    assert_eq!(q_push1.status(), 200);

    let pending_rev = q_repo.revoke_recipient(&key_q1, "age1gone");
    let q_push2 = push_bundle(base, &dev_q1, q_repo.bundle(Some(&q_head1)), Some(&q_head1)).await;
    assert_eq!(q_push2.status(), 202);
    let held: SyncHeld = q_push2.json().await.unwrap();

    BrowserFixtures {
        history_workspace_id: ws.id,
        history_workspace_name: ws_name.into(),
        revisions: vec![
            SeedingRevision {
                revision: head2,
                message: "update config and vault".into(),
                epoch: 2,
                signer: key_a.public(),
            },
            SeedingRevision {
                revision: head1,
                message: "genesis".into(),
                epoch: 1,
                signer: key_a.public(),
            },
        ],
        conflict_proposal_id,
        conflict_revision: conflict_rev,
        enrollment_proposal_id: enroll_prop.id,
        enrollment_machine_name: key_b.name,
        quorum_workspace_id: q_ws.id,
        quorum_workspace_name: q_name.into(),
        pending_quorum_proposal_id: held.proposal_id,
        pending_quorum_revision: pending_rev,
    }
}

#[derive(Default)]
struct FaultRule {
    remaining: usize,
    path_contains: String,
}

#[derive(Clone)]
struct PaddleProxyState {
    upstream_base: String,
    client: reqwest::Client,
    fault: Arc<Mutex<FaultRule>>,
}

#[derive(Clone)]
struct PaddleControlState {
    paddle: support::paddle_fixture::PaddleFixture,
    billing_cloud_url: String,
    client: reqwest::Client,
    fault: Arc<Mutex<FaultRule>>,
}

#[derive(Deserialize)]
struct CompleteCheckoutReq {
    transaction_id: String,
    subscription_id: String,
    customer_id: String,
}

#[derive(Deserialize)]
struct UpdateSubscriptionReq {
    subscription_id: String,
    event_type: String,
    status: Option<String>,
    cancel_at_period_end: Option<bool>,
}

#[derive(Deserialize)]
struct FailNextReq {
    count: usize,
    #[serde(default)]
    path_contains: String,
}

async fn paddle_proxy_handler(
    AxumState(state): AxumState<PaddleProxyState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or_else(|| uri.path());
    {
        let mut fault = state.fault.lock().unwrap();
        if fault.remaining > 0
            && (fault.path_contains.is_empty() || path_and_query.contains(&fault.path_contains))
        {
            fault.remaining -= 1;
            return (StatusCode::SERVICE_UNAVAILABLE, "simulated provider outage").into_response();
        }
    }

    let target = format!("{}{}", state.upstream_base, path_and_query);
    let req_method = reqwest::Method::from_bytes(method.as_str().as_bytes()).unwrap();
    let mut req = state.client.request(req_method, target);
    if let Some(auth) = headers.get(axum::http::header::AUTHORIZATION) {
        if let Ok(s) = auth.to_str() {
            req = req.header("Authorization", s);
        }
    }
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        if let Ok(s) = ct.to_str() {
            req = req.header("Content-Type", s);
        }
    }
    if !body.is_empty() {
        req = req.body(body.to_vec());
    }

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let ct = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let bytes = resp.bytes().await.unwrap_or_default();
            let mut out = (status, bytes).into_response();
            if let Some(ct) = ct {
                if let Ok(hv) = axum::http::HeaderValue::from_str(&ct) {
                    out.headers_mut()
                        .insert(axum::http::header::CONTENT_TYPE, hv);
                }
            }
            out
        }
        Err(err) => (StatusCode::BAD_GATEWAY, format!("proxy error: {err}")).into_response(),
    }
}

async fn send_signed_webhook(state: &PaddleControlState, event: &serde_json::Value) -> StatusCode {
    let bytes = serde_json::to_vec(event).unwrap();
    let ts = time::OffsetDateTime::now_utc().unix_timestamp();
    let sig = state.paddle.sign_webhook(&bytes, ts);
    let resp = state
        .client
        .post(format!("{}/v1/billing/webhook", state.billing_cloud_url))
        .header("Paddle-Signature", sig)
        .header("Content-Type", "application/json")
        .body(bytes)
        .send()
        .await
        .expect("deliver webhook to billing cloud");
    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
}

async fn control_complete_checkout(
    AxumState(state): AxumState<PaddleControlState>,
    Json(req): Json<CompleteCheckoutReq>,
) -> Response {
    let event =
        state
            .paddle
            .complete_checkout(&req.transaction_id, &req.subscription_id, &req.customer_id);
    let status = send_signed_webhook(&state, &event).await;
    (
        status,
        Json(serde_json::json!({
            "webhook_status": status.as_u16(),
            "event": event,
        })),
    )
        .into_response()
}

async fn control_update_subscription(
    AxumState(state): AxumState<PaddleControlState>,
    Json(req): Json<UpdateSubscriptionReq>,
) -> Response {
    let next_status = req.status;
    let cancel_at_period_end = req.cancel_at_period_end;
    let event = state
        .paddle
        .update_subscription(&req.subscription_id, &req.event_type, |sub| {
            if let Some(st) = next_status {
                sub["status"] = st.into();
            }
            if cancel_at_period_end == Some(false) {
                sub["scheduled_change"] = serde_json::Value::Null;
            }
        });
    let status = send_signed_webhook(&state, &event).await;
    (
        status,
        Json(serde_json::json!({
            "webhook_status": status.as_u16(),
            "event": event,
        })),
    )
        .into_response()
}

async fn control_calls(
    AxumState(state): AxumState<PaddleControlState>,
    AxumPath(name): AxumPath<String>,
) -> Response {
    Json(serde_json::json!({
        "name": name,
        "count": state.paddle.calls(&name),
    }))
    .into_response()
}

async fn control_fail_next(
    AxumState(state): AxumState<PaddleControlState>,
    Json(req): Json<FailNextReq>,
) -> Response {
    let mut fault = state.fault.lock().unwrap();
    fault.remaining = req.count;
    fault.path_contains = req.path_contains;
    StatusCode::OK.into_response()
}

#[tokio::test]
async fn browser_journeys() {
    let idp = support::oidc_fixture::start().await;
    let database_url = support::fresh_database().await;
    let repo_root = tempfile::tempdir().expect("repo root");
    let port = free_port();
    let public_url = format!("http://localhost:{port}");

    let cloud = support::spawn_cloud(&[
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_REPO_ROOT", repo_root.path().display().to_string()),
        ("MUSTER_BIND", format!("127.0.0.1:{port}")),
        ("MUSTER_PUBLIC_URL", public_url.clone()),
        ("MUSTER_DATABASE_URL", database_url),
        ("MUSTER_OIDC_ISSUER", idp.issuer.clone()),
        (
            "MUSTER_OIDC_CLIENT_ID",
            support::oidc_fixture::CLIENT_ID.into(),
        ),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            support::oidc_fixture::CLIENT_SECRET.into(),
        ),
    ])
    .await;

    let fixtures = seed_browser_fixtures(&public_url).await;
    let fixtures_json = serde_json::to_string(&fixtures).unwrap();

    // Second cloud instance configured with the external Paddle HTTP fixture + fault proxy.
    let paddle = support::paddle_fixture::start().await;
    let fault = Arc::new(Mutex::new(FaultRule::default()));
    let proxy_state = PaddleProxyState {
        upstream_base: paddle.base_url.clone(),
        client: support::http(),
        fault: fault.clone(),
    };
    let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind paddle proxy");
    let proxy_port = proxy_listener.local_addr().unwrap().port();
    let proxy_url = format!("http://127.0.0.1:{proxy_port}");
    let proxy_app = Router::new()
        .fallback(paddle_proxy_handler)
        .with_state(proxy_state);
    let proxy_task = tokio::spawn(async move {
        axum::serve(
            proxy_listener,
            proxy_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });

    let billing_database_url = support::fresh_database().await;
    let billing_repo_root = tempfile::tempdir().expect("billing repo root");
    let billing_port = free_port();
    let billing_public_url = format!("http://localhost:{billing_port}");
    let billing_cloud = support::spawn_cloud(&[
        ("MUSTER_ENV", "development".into()),
        (
            "MUSTER_REPO_ROOT",
            billing_repo_root.path().display().to_string(),
        ),
        ("MUSTER_BIND", format!("127.0.0.1:{billing_port}")),
        ("MUSTER_PUBLIC_URL", billing_public_url.clone()),
        ("MUSTER_DATABASE_URL", billing_database_url),
        ("MUSTER_OIDC_ISSUER", idp.issuer.clone()),
        (
            "MUSTER_OIDC_CLIENT_ID",
            support::oidc_fixture::CLIENT_ID.into(),
        ),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            support::oidc_fixture::CLIENT_SECRET.into(),
        ),
        (
            "MUSTER_PADDLE_API_KEY",
            support::paddle_fixture::API_KEY.into(),
        ),
        (
            "MUSTER_PADDLE_WEBHOOK_SECRET",
            support::paddle_fixture::WEBHOOK_SECRET.into(),
        ),
        (
            "MUSTER_PADDLE_PRICE_PERSONAL",
            support::paddle_fixture::PRICE_PERSONAL.into(),
        ),
        (
            "MUSTER_PADDLE_PRICE_TEAM_SEAT",
            support::paddle_fixture::PRICE_TEAM_SEAT.into(),
        ),
        ("MUSTER_PADDLE_BASE_URL", proxy_url),
        ("MUSTER_PADDLE_ENVIRONMENT", "sandbox".into()),
    ])
    .await;

    let control_state = PaddleControlState {
        paddle: paddle.clone(),
        billing_cloud_url: billing_public_url.clone(),
        client: support::http(),
        fault: fault.clone(),
    };
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind paddle control");
    let control_port = control_listener.local_addr().unwrap().port();
    let control_url = format!("http://127.0.0.1:{control_port}");
    let control_app = Router::new()
        .route("/complete-checkout", post(control_complete_checkout))
        .route("/update-subscription", post(control_update_subscription))
        .route("/calls/{name}", get(control_calls))
        .route("/fail-next", post(control_fail_next))
        .with_state(control_state);
    let control_task = tokio::spawn(async move {
        axum::serve(
            control_listener,
            control_app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .ok();
    });

    let browser_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/browser");
    let script = browser_dir.join("run.mjs");

    let mut cmd = Command::new("node");
    cmd.arg(&script)
        .current_dir(&browser_dir)
        .env("MUSTER_CLOUD_URL", &public_url)
        .env("MUSTER_BILLING_CLOUD_URL", &billing_public_url)
        .env("MUSTER_PADDLE_CONTROL_URL", &control_url)
        .env("MUSTER_BROWSER_FIXTURES", &fixtures_json)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    if let Ok(evidence) = std::env::var("MUSTER_EVIDENCE_DIR") {
        cmd.env("MUSTER_EVIDENCE_DIR", evidence);
    }
    if let Ok(filter) = std::env::var("MUSTER_BROWSER_FILTER") {
        cmd.env("MUSTER_BROWSER_FILTER", filter);
    }

    let status = cmd
        .status()
        .await
        .expect("failed to execute node browser suite (is Node.js installed?)");

    // Keep servers, fixtures, and temp dirs alive across the entire browser run.
    control_task.abort();
    proxy_task.abort();
    drop(billing_cloud);
    drop(billing_repo_root);
    drop(paddle);
    drop(cloud);
    drop(idp);
    drop(repo_root);

    assert!(
        status.success(),
        "browser suite failed against {public_url} with status {status}"
    );
}
