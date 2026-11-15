//! Browser integration test runner: boots the real `muster-cloud` binary,
//! the external OIDC fixture and an ephemeral PostgreSQL database, then runs
//! the Playwright browser suite against the live `http://localhost:<port>` URL.

#[path = "support/mod.rs"]
mod support;

use std::fs;
use std::path::PathBuf;
use std::process::Stdio;

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

    let browser_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/browser");
    let script = browser_dir.join("run.mjs");

    let mut cmd = Command::new("node");
    cmd.arg(&script)
        .current_dir(&browser_dir)
        .env("MUSTER_CLOUD_URL", &public_url)
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

    // Keep `cloud`, `idp`, and `repo_root` alive across the entire browser run.
    drop(cloud);
    drop(idp);
    drop(repo_root);

    assert!(
        status.success(),
        "browser suite failed against {public_url} with status {status}"
    );
}
