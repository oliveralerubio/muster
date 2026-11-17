//! Bounded bookkeeping: idempotency records are kept for a retention
//! longer than any client retry and then removed automatically, while a
//! signed request's nonce is never forgotten inside its replay window.
//! Precondition: a short idempotency retention through the
//! development-only setting.

mod support;

use std::time::{Duration, Instant};

use muster_cloud::protocol::{error_codes, ApiError, CreateWorkspace};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::{http, stack_with_env, Stack, UserSession};

async fn create(s: &Stack, u: &UserSession, key: &str, name: &str) -> reqwest::Response {
    http()
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .header("Idempotency-Key", key)
        .json(&CreateWorkspace { name: name.into() })
        .send()
        .await
        .unwrap()
}

/// Past the retention, a key's record is gone: the same key with another
/// request is a new request instead of `422 idempotency_mismatch`, for
/// account requests and for pushes alike.
#[tokio::test]
async fn idempotency_records_expire_after_their_retention() {
    let s = stack_with_env(&[("MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS", "2".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = uuid::Uuid::new_v4().to_string();
    assert_eq!(create(&s, &alice, &key, "First").await.status(), 201);
    let resp = create(&s, &alice, &key, "Second").await;
    assert_eq!(resp.status(), 422, "inside the retention the key is bound");
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::IDEMPOTENCY_MISMATCH
    );

    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Pushes").await;
    let machine = MachineKey::new("node-a");
    let dev = link(&s, &token, &ws.id, &machine).await;
    let repo = FleetRepo::genesis(&machine);
    let genesis = repo.head();
    let mut push = dev.push(repo.bundle(None), None);
    assert_eq!(dev.send(&s, &push).await.status(), 200);
    let next = repo.touch("next.txt", "after the retention");
    push.body = repo.bundle(Some(&genesis));
    push.base = Some(genesis.clone());

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let resp = create(&s, &alice, &key, "Second").await;
        if resp.status() == 201 {
            break;
        }
        assert_eq!(resp.status(), 422);
        assert!(Instant::now() < deadline, "account record never expired");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    loop {
        push.nonce = support::fleet::fresh_nonce();
        push.timestamp = support::fleet::now();
        let resp = dev.send(&s, &push).await;
        if resp.status() == 200 {
            assert_eq!(
                resp.headers()[muster_cloud::protocol::HEADER_REVISION],
                next.as_str()
            );
            break;
        }
        assert_eq!(resp.status(), 422);
        assert!(Instant::now() < deadline, "push record never expired");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Cleanup never forgets a nonce still inside the replay window: a replay
/// is refused however many cleanup passes ran since it was first used.
#[tokio::test]
async fn a_nonce_inside_the_replay_window_is_never_forgotten() {
    let s = stack_with_env(&[("MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Nonces").await;
    let machine = MachineKey::new("node-a");
    let dev = link(&s, &token, &ws.id, &machine).await;
    let get = dev.get(&dev.sync_path());
    assert_eq!(dev.send(&s, &get).await.status(), 204);
    // Several cleanup passes (development: one a second).
    tokio::time::sleep(Duration::from_secs(4)).await;
    let resp = dev.send(&s, &get).await;
    assert_eq!(resp.status(), 401, "replayed nonce accepted");
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::INVALID_SIGNATURE
    );
}
