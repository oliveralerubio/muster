mod support;

use muster_cloud::protocol::{ApiError, CreateWorkspace, PlanStatus, Role, Workspace};
use support::{http, stack};

#[tokio::test]
async fn multi_tenant_workspace_isolation_and_lifecycle() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let client = http();

    let alice_cookie = format!("__Host-mc_session={}", alice.cookie);
    let bob_cookie = format!("__Host-mc_session={}", bob.cookie);

    // 1. Alice creates a workspace "Acme Corp"
    let create_payload = CreateWorkspace {
        name: "Acme Corp".into(),
    };
    let create_resp = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", &alice_cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&create_payload)
        .send()
        .await
        .unwrap();

    assert_eq!(create_resp.status(), 201);
    let ws_a: Workspace = create_resp.json().await.unwrap();
    assert_eq!(ws_a.name, "Acme Corp");
    assert_eq!(ws_a.role, Role::Owner);
    assert_eq!(ws_a.plan_status, PlanStatus::Trialing);

    // Trial must be 14 days
    let created = time::OffsetDateTime::parse(
        &ws_a.created_at,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let trial_ends = time::OffsetDateTime::parse(
        &ws_a.trial_ends_at,
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let duration = trial_ends - created;
    assert_eq!(duration.whole_days(), 14, "trial must be 14 days");

    // 2. Alice lists workspaces: sees Acme Corp
    let alice_list_resp = client
        .get(s.url("/v1/workspaces"))
        .header("Cookie", &alice_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(alice_list_resp.status(), 200);
    let alice_workspaces: Vec<Workspace> = alice_list_resp.json().await.unwrap();
    assert_eq!(alice_workspaces.len(), 1);
    assert_eq!(alice_workspaces[0].id, ws_a.id);

    // 3. Bob lists workspaces: sees empty array (not Alice's workspace)
    let bob_list_resp = client
        .get(s.url("/v1/workspaces"))
        .header("Cookie", &bob_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(bob_list_resp.status(), 200);
    let bob_workspaces: Vec<Workspace> = bob_list_resp.json().await.unwrap();
    assert!(
        bob_workspaces.is_empty(),
        "Bob should not see Alice's workspaces"
    );

    // 4. Alice details her workspace: 200 OK
    let alice_get_resp = client
        .get(s.url(&format!("/v1/workspaces/{}", ws_a.id)))
        .header("Cookie", &alice_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(alice_get_resp.status(), 200);
    let detailed_a: Workspace = alice_get_resp.json().await.unwrap();
    assert_eq!(detailed_a.id, ws_a.id);

    // 5. ISOLATION: Bob attempts to detail Alice's workspace -> strictly 404 Not Found
    let bob_get_resp = client
        .get(s.url(&format!("/v1/workspaces/{}", ws_a.id)))
        .header("Cookie", &bob_cookie)
        .send()
        .await
        .unwrap();
    assert_eq!(
        bob_get_resp.status(),
        404,
        "foreign tenant must return 404 not_found"
    );
    let err: ApiError = bob_get_resp.json().await.unwrap();
    assert_eq!(err.error.code, "not_found");
}

#[tokio::test]
async fn workspace_creation_idempotency_and_mismatch_rejection() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let client = http();
    let cookie = format!("__Host-mc_session={}", alice.cookie);

    // Without an Idempotency-Key the mutation is refused (it is mandatory).
    let missing = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", &cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&CreateWorkspace {
            name: "No key".into(),
        })
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), 400);
    let err: ApiError = missing.json().await.unwrap();
    assert_eq!(err.error.code, "bad_request");

    let idem_key = uuid::Uuid::new_v4().to_string();

    let payload_1 = CreateWorkspace {
        name: "Unique Workspace".into(),
    };

    // 1. First creation with idempotency key
    let resp_1 = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", &cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", &idem_key)
        .json(&payload_1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_1.status(), 201);
    let ws_1: Workspace = resp_1.json().await.unwrap();

    // 2. Retry identical request with same key -> same response, no duplicate workspace
    let resp_2 = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", &cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", &idem_key)
        .json(&payload_1)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_2.status(), 201);
    let ws_2: Workspace = resp_2.json().await.unwrap();
    assert_eq!(ws_1.id, ws_2.id);

    // Ensure only 1 workspace exists
    let list_resp = client
        .get(s.url("/v1/workspaces"))
        .header("Cookie", &cookie)
        .send()
        .await
        .unwrap();
    let workspaces: Vec<Workspace> = list_resp.json().await.unwrap();
    assert_eq!(workspaces.len(), 1);

    // 3. Reusing the same key with DIFFERENT payload -> 422 idempotency_mismatch
    let payload_2 = CreateWorkspace {
        name: "Different Name".into(),
    };
    let resp_3 = client
        .post(s.url("/v1/workspaces"))
        .header("Cookie", &cookie)
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", &idem_key)
        .json(&payload_2)
        .send()
        .await
        .unwrap();
    assert_eq!(resp_3.status(), 422);
    let err: ApiError = resp_3.json().await.unwrap();
    assert_eq!(err.error.code, "idempotency_mismatch");
}

// The deletion request (owner confirmation, idempotency, deadlines, guards,
// provider cancellation) is covered in `api_lifecycle.rs`.
#[tokio::test]
async fn workspace_deletion_is_an_owner_confirmed_request() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let client = http();

    let ws = support::fleet::create_workspace(&s, &alice, "To Delete").await;
    let body = serde_json::json!({
        "confirm_workspace_id": ws.id,
        "confirm_workspace_name": "To Delete",
    });

    // Not a member (Bob): 404.
    let bob_del = client
        .delete(s.url(&format!("/v1/workspaces/{}", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", bob.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &bob.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(bob_del.status(), 404);

    // Owner (Alice) with the exact confirmation: 202 with the lifecycle.
    let del_resp = client
        .delete(s.url(&format!("/v1/workspaces/{}", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(del_resp.status(), 202);
    let del: muster_cloud::protocol::WorkspaceLifecycle = del_resp.json().await.unwrap();
    assert_eq!(del.workspace_id, ws.id);
    assert_eq!(
        del.state,
        muster_cloud::protocol::LifecycleState::DeletionScheduled
    );
    let d = del.deletion.expect("deletion status");
    assert_eq!(d.export_until, d.primary.purge_at);
    assert_eq!(d.backups.status, "unknown");
    assert!(d.backups.deadline.is_none());
}
