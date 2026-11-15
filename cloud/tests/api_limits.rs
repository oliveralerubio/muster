//! The limits `GET /billing` declares are the limits the service enforces.
//! Every expected number is read from `/billing`, never repeated here.

mod support;

use muster_cloud::protocol::{
    canonical_link_v1, error_codes, AccountToken, ApiError, CreateInvitation, CreateProposal,
    CreatedInvitation, Device, DeviceStatus, LinkDeviceRequest, ProposalKind, Role,
    WorkspaceBilling,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::{http, stack, stack_with_env, Stack, UserSession};

async fn billing_of(s: &Stack, u: &UserSession, ws: &str) -> WorkspaceBilling {
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/billing")))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// `POST /devices` for `key`, returning the raw response.
async fn try_link(
    s: &Stack,
    token: &AccountToken,
    ws: &str,
    key: &MachineKey,
) -> reqwest::Response {
    let pk = key.public();
    let proof = key.signer.sign(&canonical_link_v1(
        ws,
        &token.account_id,
        &key.name,
        &pk,
        &key.recipient,
    ));
    http()
        .post(s.url(&format!("/v1/workspaces/{ws}/devices")))
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
        .unwrap()
}

async fn devices_of(s: &Stack, token: &AccountToken, ws: &str) -> Vec<Device> {
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/devices")))
        .header("Authorization", format!("Bearer {}", token.access_token))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// Concurrent link requests can never take a workspace past `max_devices`;
/// the refused ones get `507 quota_exceeded`, and the machines already in
/// keep their accepted head, pull and export.
#[tokio::test]
async fn concurrent_links_never_exceed_the_declared_machine_limit() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Limits").await;
    let max = billing_of(&s, &alice, &ws.id).await.max_devices as usize;
    assert!(max >= 1);

    let first_key = MachineKey::new("node-0");
    let first = link(&s, &token, &ws.id, &first_key).await;
    let repo = FleetRepo::genesis(&first_key);
    let head = repo.head();
    let resp = first.send(&s, &first.push(repo.bundle(None), None)).await;
    assert_eq!(resp.status(), 200);

    // Many more new machines than free slots, all at once.
    let keys: Vec<MachineKey> = (1..=8 * max)
        .map(|i| MachineKey::new(&format!("node-{i}")))
        .collect();
    let responses =
        futures::future::join_all(keys.iter().map(|k| try_link(&s, &token, &ws.id, k))).await;
    let mut linked = 0;
    for resp in responses {
        match resp.status().as_u16() {
            201 => linked += 1,
            507 => assert_eq!(
                resp.json::<ApiError>().await.unwrap().error.code,
                error_codes::QUOTA_EXCEEDED
            ),
            other => panic!("unexpected link status {other}"),
        }
    }
    assert_eq!(linked, max - 1, "links accepted beyond the declared limit");
    assert_eq!(devices_of(&s, &token, &ws.id).await.len(), max);

    // Refusals change nothing for machines already linked.
    let resp = first.send(&s, &first.get(&first.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        head
    );
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{}/export", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    // A machine already in can relink (token rotation) at the limit.
    assert_eq!(try_link(&s, &token, &ws.id, &first_key).await.status(), 201);
}

async fn session_send(
    s: &Stack,
    u: &UserSession,
    method: reqwest::Method,
    path: &str,
    body: &serde_json::Value,
) -> reqwest::Response {
    http()
        .request(method, s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .json(body)
        .send()
        .await
        .unwrap()
}

/// Revoked machines no longer use a slot: removing a member (which revokes
/// the machines they linked) lets the workspace link new ones again.
#[tokio::test]
async fn revoked_machines_do_not_count_against_the_machine_limit() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let bob = s.login_user("bob", "bob@example.com").await;
    let ws = create_workspace(&s, &alice, "Limits").await;
    let max = billing_of(&s, &alice, &ws.id).await.max_devices as usize;

    let invite = CreateInvitation {
        email: "bob@example.com".into(),
        role: Role::Member,
    };
    let resp = session_send(
        &s,
        &alice,
        reqwest::Method::POST,
        &format!("/v1/workspaces/{}/invitations", ws.id),
        &serde_json::to_value(&invite).unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 201);
    let created: CreatedInvitation = resp.json().await.unwrap();
    let resp = session_send(
        &s,
        &bob,
        reqwest::Method::POST,
        &created.accept_path,
        &serde_json::json!({}),
    )
    .await;
    assert!(resp.status().is_success());

    // Bob fills every slot.
    let alice_token = account_token(&s, &alice).await;
    let bob_token = account_token(&s, &bob).await;
    let bob_keys: Vec<MachineKey> = (0..max)
        .map(|i| MachineKey::new(&format!("bob-{i}")))
        .collect();
    for key in &bob_keys {
        link(&s, &bob_token, &ws.id, key).await;
    }
    let alice_key = MachineKey::new("alice-0");
    let resp = try_link(&s, &alice_token, &ws.id, &alice_key).await;
    assert_eq!(resp.status(), 507);

    // Removing Bob revokes his machines and frees their slots.
    let resp = session_send(
        &s,
        &alice,
        reqwest::Method::DELETE,
        &format!("/v1/workspaces/{}/members/{}", ws.id, bob.user_id),
        &serde_json::json!({}),
    )
    .await;
    assert_eq!(resp.status(), 204);
    let devices = devices_of(&s, &alice_token, &ws.id).await;
    assert!(devices.iter().all(|d| d.status == DeviceStatus::Revoked));
    assert_eq!(
        try_link(&s, &alice_token, &ws.id, &alice_key)
            .await
            .status(),
        201
    );
}

/// `len` incompressible bytes (xorshift), so a Git pack is about as big.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 8);
    while out.len() < len {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        out.extend_from_slice(&seed.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Accepted genesis plus a linked first machine.
async fn genesis_workspace<'a>(
    s: &Stack,
    u: &UserSession,
    key: &'a MachineKey,
) -> (String, support::fleet::Device<'a>, FleetRepo) {
    let token = account_token(s, u).await;
    let ws = create_workspace(s, u, "Limits").await;
    let dev = link(s, &token, &ws.id, key).await;
    let repo = FleetRepo::genesis(key);
    let resp = dev.send(s, &dev.push(repo.bundle(None), None)).await;
    assert_eq!(resp.status(), 200);
    (ws.id, dev, repo)
}

async fn pulled_head(s: &Stack, dev: &support::fleet::Device<'_>) -> String {
    let resp = dev.send(s, &dev.get(&dev.sync_path())).await;
    assert_eq!(resp.status(), 200);
    resp.headers()[muster_cloud::protocol::HEADER_REVISION]
        .to_str()
        .unwrap()
        .to_string()
}

/// A push larger than the plan's `max_bundle_bytes` is refused with
/// `413 payload_too_large` and leaves the accepted head as it was.
#[tokio::test]
async fn a_push_over_the_declared_bundle_limit_is_refused() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let max = billing_of(&s, &alice, &ws).await.max_bundle_bytes;

    std::fs::write(repo.config.join("big.bin"), noise(max + 1024 * 1024, 7)).unwrap();
    repo.commit("big");
    let bundle = repo.bundle(Some(&head));
    assert!(bundle.len() > max);
    let resp = dev.send(&s, &dev.push(bundle, Some(&head))).await;
    assert_eq!(resp.status(), 413);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::PAYLOAD_TOO_LARGE
    );
    assert_eq!(pulled_head(&s, &dev).await, head);
}

/// Storage use counts what the service keeps: accepted history and also a
/// conflicting push it retains as a proposal.
#[tokio::test]
async fn storage_use_counts_accepted_history_and_retained_conflicts() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let genesis = repo.head();
    let after_genesis = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    assert!(after_genesis > 0, "accepted history counts");

    // Accepted change with 1 MiB of incompressible content.
    std::fs::write(repo.config.join("a.bin"), noise(1 << 20, 1)).unwrap();
    let next = repo.commit("a");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 200);
    let after_accepted = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    assert!(
        after_accepted >= after_genesis + (1 << 20),
        "accepted push counts"
    );

    // A stale push is retained as a conflict proposal: it counts too.
    let stale = FleetRepo::clone_from_bundle(&repo.bundle(None));
    git_reset(&stale, &genesis);
    std::fs::write(stale.config.join("b.bin"), noise(1 << 20, 2)).unwrap();
    stale.commit("b");
    let resp = dev
        .send(&s, &dev.push(stale.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 409);
    let after_conflict = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    assert!(
        after_conflict >= after_accepted + (1 << 20),
        "retained conflict counts"
    );
    assert_eq!(pulled_head(&s, &dev).await, next);
}

fn git_reset(repo: &FleetRepo, rev: &str) {
    support::fleet::git(&repo.config, &["reset", "-q", "--hard", rev]);
}

/// A push refused by a plan limit stores nothing: storage use is unchanged.
#[tokio::test]
async fn a_push_refused_by_a_limit_consumes_no_storage() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let before = billing_of(&s, &alice, &ws).await;

    std::fs::write(
        repo.config.join("big.bin"),
        noise(before.max_bundle_bytes + 1024 * 1024, 3),
    )
    .unwrap();
    repo.commit("big");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 413);
    let after = billing_of(&s, &alice, &ws).await;
    assert_eq!(after.storage_used_bytes, before.storage_used_bytes);
}

/// A push that would take storage use past `max_storage_bytes` is refused
/// with `507 quota_exceeded`, also when it would only be retained as a
/// conflict; nothing is stored and head, pull and export are unchanged.
/// Precondition: a small limit through the development-only setting
/// (`/billing` must declare that same limit).
#[tokio::test]
async fn pushes_past_the_declared_storage_limit_are_refused() {
    const LIMIT: u64 = 1536 * 1024;
    let s = stack_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", LIMIT.to_string())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    assert_eq!(billing_of(&s, &alice, &ws).await.max_storage_bytes, LIMIT);

    // Fits: accepted.
    let genesis = repo.head();
    std::fs::write(repo.config.join("a.bin"), noise(512 * 1024, 4)).unwrap();
    let head = repo.commit("a");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 200);
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    // Would not fit: refused, as a fast-forward and as a stale conflict.
    let stale = FleetRepo::clone_from_bundle(&repo.bundle(None));
    std::fs::write(repo.config.join("b.bin"), noise(1 << 20, 5)).unwrap();
    repo.commit("b");
    git_reset(&stale, &genesis);
    std::fs::write(stale.config.join("c.bin"), noise(1 << 20, 6)).unwrap();
    stale.commit("c");
    for (bundle, base) in [
        (repo.bundle(Some(&head)), &head),
        (stale.bundle(Some(&genesis)), &genesis),
    ] {
        let resp = dev.send(&s, &dev.push(bundle, Some(base))).await;
        assert_eq!(resp.status(), 507);
        assert_eq!(
            resp.json::<ApiError>().await.unwrap().error.code,
            error_codes::QUOTA_EXCEEDED
        );
    }
    assert_eq!(billing_of(&s, &alice, &ws).await.storage_used_bytes, before);
    assert_eq!(pulled_head(&s, &dev).await, head);
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/export")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // A small change that still fits is accepted.
    let small = FleetRepo::clone_from_bundle(&repo.bundle(None));
    git_reset(&small, &head);
    let next = small.touch("small.txt", "fits");
    let resp = dev
        .send(&s, &dev.push(small.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(pulled_head(&s, &dev).await, next);
}

/// A restore proposal of `pad` extra bytes (kept verbatim with the proposal).
fn restore_proposal(target: &str, pad: usize) -> CreateProposal {
    CreateProposal {
        kind: ProposalKind::Restore,
        base_revision: target.into(),
        payload: serde_json::json!({
            "target_revision": target,
            "paths": ["muster.toml"],
            "note": "x".repeat(pad),
        }),
    }
}

/// Declarative proposals are stored too: what each keeps counts as storage.
#[tokio::test]
async fn retained_proposals_count_as_storage_use() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    let path = format!("/v1/workspaces/{ws}/proposals");
    let resp = dev
        .send(
            &s,
            &dev.post_json(&path, &restore_proposal(&repo.head(), 256 * 1024)),
        )
        .await;
    assert_eq!(resp.status(), 201);
    let after = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    assert!(
        after >= before + 256 * 1024,
        "proposal not counted: {before} -> {after}"
    );
}

/// Proposals cannot grow storage past the declared limit either: one that
/// does not fit is refused with `507 quota_exceeded`, and pull is unchanged.
/// Precondition: the development-only small storage limit.
#[tokio::test]
async fn proposals_past_the_declared_storage_limit_are_refused() {
    const LIMIT: u64 = 1024 * 1024;
    let s = stack_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", LIMIT.to_string())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let path = format!("/v1/workspaces/{ws}/proposals");

    for _ in 0..2 {
        let resp = dev
            .send(
                &s,
                &dev.post_json(&path, &restore_proposal(&head, 400 * 1024)),
            )
            .await;
        assert_eq!(resp.status(), 201);
    }
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    let resp = dev
        .send(
            &s,
            &dev.post_json(&path, &restore_proposal(&head, 400 * 1024)),
        )
        .await;
    assert_eq!(resp.status(), 507);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::QUOTA_EXCEEDED
    );
    assert_eq!(billing_of(&s, &alice, &ws).await.storage_used_bytes, before);
    assert_eq!(pulled_head(&s, &dev).await, head);
}
