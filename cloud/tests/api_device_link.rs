mod support;

use muster::signing::Signer;
use muster_cloud::protocol::{
    canonical_link_v1, error_codes, AccountToken, ApiError, CreateWorkspace, Device,
    DeviceAuthorization, DeviceAuthorizeRequest, DeviceDecision, DeviceStatus, LinkDeviceRequest,
    LinkedDevice, Workspace,
};
use support::{http, stack, UserSession};
use tempfile::tempdir;

async fn account_token(s: &support::Stack, user: &UserSession) -> AccountToken {
    let client = http();
    let auth: DeviceAuthorization = client
        .post(s.url("/v1/device/authorize"))
        .json(&DeviceAuthorizeRequest {
            client_name: "cli".into(),
        })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let approve = muster_cloud::protocol::DeviceApproveRequest {
        user_code: auth.user_code,
        decision: DeviceDecision::Approve,
    };
    client
        .post(s.url("/v1/device/approve"))
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &user.csrf_token)
        .json(&approve)
        .send()
        .await
        .unwrap();

    let token_req = muster_cloud::protocol::DeviceTokenRequest {
        grant_type: muster_cloud::protocol::DEVICE_CODE_GRANT_TYPE.into(),
        device_code: auth.device_code,
    };
    client
        .post(s.url("/v1/device/token"))
        .json(&token_req)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn fake_signer() -> (Signer, String) {
    let dir = tempdir().unwrap();
    let signer = Signer::load_or_create(dir.path()).unwrap();
    let pk = signer.public();
    (signer, pk)
}

#[tokio::test]
async fn machine_enrollment_owner_bootstrap_and_isolation() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let client = http();

    let alice_token = account_token(&s, &alice).await;
    let bob_token = account_token(&s, &bob).await;
    let alice_bearer = format!("Bearer {}", alice_token.access_token);
    let bob_bearer = format!("Bearer {}", bob_token.access_token);

    // 1. Alice creates workspace "Prod Fleet"
    let create_payload = CreateWorkspace {
        name: "Prod Fleet".into(),
    };
    let ws_resp = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&create_payload)
        .send()
        .await
        .unwrap();
    assert_eq!(ws_resp.status(), 201);
    let ws: Workspace = ws_resp.json().await.unwrap();

    let (signer_a, pk_a) = fake_signer();
    let (signer_b, pk_b) = fake_signer();

    // 2. Proof signature validation
    let bad_proof_req = LinkDeviceRequest {
        machine_name: "node-a".into(),
        signing_key: pk_a.clone(),
        recipient: "age1nodea".into(),
        proof: "badhex".into(),
    };
    let bad_resp = client
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &alice_bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&bad_proof_req)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_resp.status(), 401);
    let err: ApiError = bad_resp.json().await.unwrap();
    assert_eq!(err.error.code, error_codes::INVALID_SIGNATURE);

    // 3. Owner links first machine to empty workspace: claims bootstrap atomically
    let valid_msg_a = canonical_link_v1(&ws.id, &alice.user_id, "node-a", &pk_a, "age1nodea");
    let proof_a = signer_a.sign(&valid_msg_a);

    let link_a_req = LinkDeviceRequest {
        machine_name: "node-a".into(),
        signing_key: pk_a.clone(),
        recipient: "age1nodea".into(),
        proof: proof_a.clone(),
    };
    let link_a_resp = client
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &alice_bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&link_a_req)
        .send()
        .await
        .unwrap();

    assert_eq!(link_a_resp.status(), 201);
    let dev_a: LinkedDevice = link_a_resp.json().await.unwrap();
    // Linking never approves: only accepted signed governance does.
    assert_eq!(dev_a.status, DeviceStatus::Linked);
    assert!(dev_a.bootstrap, "first machine of owner must be bootstrap");
    assert!(dev_a.device_token.starts_with("mcd_"));
    assert!(dev_a.head.is_none());

    // 4. Second machine linked by owner is pending (bootstrap already claimed)
    let valid_msg_b = canonical_link_v1(&ws.id, &alice.user_id, "node-b", &pk_b, "age1nodeb");
    let proof_b = signer_b.sign(&valid_msg_b);
    let link_b_req = LinkDeviceRequest {
        machine_name: "node-b".into(),
        signing_key: pk_b.clone(),
        recipient: "age1nodeb".into(),
        proof: proof_b,
    };
    let link_b_resp = client
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &alice_bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&link_b_req)
        .send()
        .await
        .unwrap();
    assert_eq!(link_b_resp.status(), 201);
    let dev_b: LinkedDevice = link_b_resp.json().await.unwrap();
    assert_eq!(dev_b.status, DeviceStatus::Linked);
    assert!(!dev_b.bootstrap);

    // 5. Re-linking node-a by Alice rotates token, preserves device_id and bootstrap
    let relink_resp = client
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &alice_bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&link_a_req)
        .send()
        .await
        .unwrap();
    assert_eq!(relink_resp.status(), 201);
    let relink_a: LinkedDevice = relink_resp.json().await.unwrap();
    assert_eq!(relink_a.device_id, dev_a.device_id);
    assert!(relink_a.bootstrap);
    assert_ne!(relink_a.device_token, dev_a.device_token);

    // 6. Foreign tenant Bob cannot link to Alice's workspace: 404 not_found
    let valid_msg_bob = canonical_link_v1(&ws.id, &bob.user_id, "node-bob", &pk_b, "age1nodeb");
    let proof_bob = signer_b.sign(&valid_msg_bob);
    let link_bob_req = LinkDeviceRequest {
        machine_name: "node-bob".into(),
        signing_key: pk_b.clone(),
        recipient: "age1nodeb".into(),
        proof: proof_bob,
    };
    let bob_resp = client
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &bob_bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&link_bob_req)
        .send()
        .await
        .unwrap();
    assert_eq!(bob_resp.status(), 404);

    // 7. List devices: Alice sees node-a and node-b
    let list_resp = client
        .get(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &alice_bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(list_resp.status(), 200);
    let devices: Vec<Device> = list_resp.json().await.unwrap();
    assert_eq!(devices.len(), 2);
    assert_eq!(devices[0].machine_name, "node-a");
    assert!(devices[0].bootstrap);
    assert_eq!(devices[0].signing_key, pk_a);
    assert_eq!(devices[0].linked_by, alice.user_id);
    assert_eq!(devices[1].machine_name, "node-b");
    assert!(!devices[1].bootstrap);
    assert!(devices.iter().all(|d| d.status == DeviceStatus::Linked));

    // 8. Bob's own token cannot list Alice's devices either.
    let resp = client
        .get(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
        .header("Authorization", &bob_bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
}

async fn new_workspace(s: &support::Stack, user: &UserSession, name: &str) -> Workspace {
    http()
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", user.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &user.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&CreateWorkspace { name: name.into() })
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

fn link_body(ws: &str, account: &str, name: &str, signer: &Signer) -> LinkDeviceRequest {
    let pk = signer.public();
    let recipient = format!("age1{name}");
    LinkDeviceRequest {
        machine_name: name.into(),
        proof: signer.sign(&canonical_link_v1(ws, account, name, &pk, &recipient)),
        signing_key: pk,
        recipient,
    }
}

#[tokio::test]
async fn concurrent_links_claim_bootstrap_exactly_once() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let bearer = format!("Bearer {}", token.access_token);
    let ws = new_workspace(&s, &alice, "Race").await;
    let signers: Vec<Signer> = (0..6).map(|_| fake_signer().0).collect();
    let futures = signers.iter().enumerate().map(|(i, signer)| {
        let body = link_body(&ws.id, &alice.user_id, &format!("m{i}"), signer);
        let req = http()
            .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
            .header("Authorization", &bearer)
            .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
            .json(&body);
        async move { req.send().await.unwrap() }
    });
    let responses = futures::future::join_all(futures).await;
    let mut claims = 0;
    for resp in responses {
        assert_eq!(resp.status(), 201);
        let linked: LinkedDevice = resp.json().await.unwrap();
        claims += usize::from(linked.bootstrap);
    }
    assert_eq!(claims, 1, "exactly one machine holds the bootstrap claim");
}

#[tokio::test]
async fn linking_requires_idempotency_key_and_rejects_key_reuse_for_another_body() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let bearer = format!("Bearer {}", token.access_token);
    let ws = new_workspace(&s, &alice, "Keys").await;
    let url = s.url(&format!("/v1/workspaces/{}/devices", ws.id));
    let (signer, _) = fake_signer();
    let body = link_body(&ws.id, &alice.user_id, "laptop", &signer);

    let resp = http()
        .post(&url)
        .header("Authorization", &bearer)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 400);

    let key = uuid::Uuid::new_v4().to_string();
    let first: LinkedDevice = http()
        .post(&url)
        .header("Authorization", &bearer)
        .header("Idempotency-Key", &key)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Same key, same body: same device (the token is rotated, never stored).
    let again: LinkedDevice = http()
        .post(&url)
        .header("Authorization", &bearer)
        .header("Idempotency-Key", &key)
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(again.device_id, first.device_id);

    let (other, _) = fake_signer();
    let resp = http()
        .post(&url)
        .header("Authorization", &bearer)
        .header("Idempotency-Key", &key)
        .json(&link_body(&ws.id, &alice.user_id, "desktop", &other))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 422);
    let err: ApiError = resp.json().await.unwrap();
    assert_eq!(err.error.code, error_codes::IDEMPOTENCY_MISMATCH);
}

#[tokio::test]
async fn proof_is_bound_to_workspace_and_account_and_keys_are_per_workspace() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let bearer = format!("Bearer {}", token.access_token);
    let ws1 = new_workspace(&s, &alice, "One").await;
    let ws2 = new_workspace(&s, &alice, "Two").await;
    let (signer, _) = fake_signer();

    // A proof made for ws1 does not link into ws2.
    let resp = http()
        .post(s.url(&format!("/v1/workspaces/{}/devices", ws2.id)))
        .header("Authorization", &bearer)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&link_body(&ws1.id, &alice.user_id, "laptop", &signer))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);

    // The same machine key links separately into each workspace.
    let mut ids = Vec::new();
    for ws in [&ws1, &ws2] {
        let linked: LinkedDevice = http()
            .post(s.url(&format!("/v1/workspaces/{}/devices", ws.id)))
            .header("Authorization", &bearer)
            .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
            .json(&link_body(&ws.id, &alice.user_id, "laptop", &signer))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(linked.workspace_id, ws.id);
        ids.push(linked.device_id);
    }
    assert_ne!(ids[0], ids[1]);
}
