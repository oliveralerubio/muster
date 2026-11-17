//! The limits `GET /billing` declares are the limits the service enforces.
//! Every expected number is read from `/billing`, never repeated here.

mod support;

use muster_cloud::protocol::{
    canonical_link_v1, error_codes, AccountToken, ApiError, CheckoutSession, CreateCheckout,
    CreateInvitation, CreateProposal, CreatedInvitation, Device, DeviceStatus, LinkDeviceRequest,
    PlanStatus, ProposalKind, Role, WorkspaceBilling,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::paddle_fixture::PaddleFixture;
use support::{http, stack, stack_with_env, stack_with_paddle, Stack, UserSession};

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

/// A candidate the verifier refuses (422) does not keep consuming storage:
/// usage is unchanged and the accepted head is intact.
#[tokio::test]
async fn an_invalid_candidate_consumes_no_storage() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    // An unrelated history (another fleet's genesis) carrying 512 KiB.
    let outsider = MachineKey::new("outsider");
    let alien = FleetRepo::genesis(&outsider);
    std::fs::write(alien.config.join("x.bin"), noise(512 * 1024, 8)).unwrap();
    alien.commit("x");
    let resp = dev
        .send(&s, &dev.push(alien.bundle(None), Some(&head)))
        .await;
    assert_eq!(resp.status(), 422);
    assert_eq!(billing_of(&s, &alice, &ws).await.storage_used_bytes, before);
    assert_eq!(pulled_head(&s, &dev).await, head);
}

/// The storage guard counts the bytes really kept (pack, index, refs), not
/// the request body: with exactly `bundle.len()` bytes free the push does
/// not fit, is refused with 507, and nothing is stored. With room for what
/// is really stored, it is accepted and usage stays within the limit.
#[tokio::test]
async fn the_storage_guard_counts_bytes_really_stored_not_the_body() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let used = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    std::fs::write(repo.config.join("a.bin"), noise(256 * 1024, 9)).unwrap();
    let next = repo.commit("a");
    let bundle = repo.bundle(Some(&head));
    let limit = used + bundle.len() as u64;
    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", limit.to_string())])
        .await;
    let resp = dev.send(&s, &dev.push(bundle.clone(), Some(&head))).await;
    assert_eq!(resp.status(), 507);
    let billing = billing_of(&s, &alice, &ws).await;
    assert_eq!(billing.storage_used_bytes, used);
    assert_eq!(pulled_head(&s, &dev).await, head);

    let limit = used + 2 * bundle.len() as u64;
    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", limit.to_string())])
        .await;
    let resp = dev.send(&s, &dev.push(bundle, Some(&head))).await;
    assert_eq!(resp.status(), 200);
    let billing = billing_of(&s, &alice, &ws).await;
    assert!(billing.storage_used_bytes > used + 256 * 1024);
    assert!(billing.storage_used_bytes <= billing.max_storage_bytes);
    assert_eq!(pulled_head(&s, &dev).await, next);
}

/// A proposal is measured as it is stored and counted (PostgreSQL's JSON
/// text, which spaces separators), not as compact JSON: with only its
/// compact size free it does not fit and is refused with 507.
#[tokio::test]
async fn the_proposal_guard_counts_the_size_it_is_stored_with() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let used = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    // Many short array items: the stored text form is clearly larger.
    let mut proposal = restore_proposal(&head, 0);
    proposal.payload["note"] = serde_json::json!(vec![0u8; 20_000]);
    let compact = serde_json::to_vec(&proposal.payload).unwrap().len() as u64;
    s.restart_with_env(&[(
        "MUSTER_DEV_STORAGE_LIMIT_BYTES",
        (used + compact).to_string(),
    )])
    .await;
    let path = format!("/v1/workspaces/{ws}/proposals");
    let resp = dev.send(&s, &dev.post_json(&path, &proposal)).await;
    assert_eq!(resp.status(), 507);
    let billing = billing_of(&s, &alice, &ws).await;
    assert_eq!(billing.storage_used_bytes, used);
    assert!(billing.storage_used_bytes <= billing.max_storage_bytes);
}

/// More simultaneous pushes than the service has database connections, at
/// the storage limit: every request gets a definite answer (accepted,
/// retained conflict or 507; never a 500 from a starved or deadlocked
/// pool), usage ends within the limit, and head, pull and export hold.
#[tokio::test]
async fn concurrent_pushes_at_the_storage_limit_stay_within_it() {
    const PUSHES: usize = 24;
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let used = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    // Room for about five of the pushes below.
    let limit = used + 5 * 70 * 1024;
    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", limit.to_string())])
        .await;

    let bundle = repo.bundle(None);
    let mut requests = Vec::new();
    let mut tips = Vec::new();
    for i in 0..PUSHES {
        let fork = FleetRepo::clone_from_bundle(&bundle);
        git_reset(&fork, &head);
        std::fs::write(fork.config.join("f.bin"), noise(64 * 1024, 100 + i as u64)).unwrap();
        tips.push(fork.commit(&format!("f{i}")));
        requests.push(dev.push(fork.bundle(Some(&head)), Some(&head)));
    }
    let responses = futures::future::join_all(requests.iter().map(|r| dev.send(&s, r))).await;
    let mut counts = std::collections::BTreeMap::new();
    for resp in responses {
        *counts.entry(resp.status().as_u16()).or_insert(0) += 1;
    }
    assert!(
        counts.keys().all(|c| [200, 409, 507].contains(c)),
        "statuses: {counts:?}"
    );
    assert_eq!(counts.get(&200), Some(&1), "statuses: {counts:?}");
    assert!(counts.contains_key(&507), "statuses: {counts:?}");

    let billing = billing_of(&s, &alice, &ws).await;
    assert!(
        billing.storage_used_bytes <= billing.max_storage_bytes,
        "{} > {}",
        billing.storage_used_bytes,
        billing.max_storage_bytes
    );
    let pulled = pulled_head(&s, &dev).await;
    assert!(tips.contains(&pulled));
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/export")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Simultaneous proposals at the storage limit: exactly as many as fit are
/// kept, the rest get 507, and usage ends within the limit.
#[tokio::test]
async fn concurrent_proposals_at_the_storage_limit_keep_exactly_what_fits() {
    const FIT: u64 = 4;
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let head = repo.head();
    let path = format!("/v1/workspaces/{ws}/proposals");
    let proposal = restore_proposal(&head, 32 * 1024);

    // The stored size of one such proposal, measured through the API.
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    let resp = dev.send(&s, &dev.post_json(&path, &proposal)).await;
    assert_eq!(resp.status(), 201);
    let used = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    let each = used - before;

    let limit = used + FIT * each + each / 2;
    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", limit.to_string())])
        .await;
    const ATTEMPTS: u64 = 32;
    let requests: Vec<_> = (0..ATTEMPTS)
        .map(|_| dev.post_json(&path, &proposal))
        .collect();
    let responses = futures::future::join_all(requests.iter().map(|r| dev.send(&s, r))).await;
    let mut counts = std::collections::BTreeMap::new();
    for resp in responses {
        *counts.entry(resp.status().as_u16()).or_insert(0u64) += 1;
    }
    assert_eq!(counts.get(&201), Some(&FIT), "statuses: {counts:?}");
    assert_eq!(
        counts.get(&507),
        Some(&(ATTEMPTS - FIT)),
        "statuses: {counts:?}"
    );
    let billing = billing_of(&s, &alice, &ws).await;
    assert_eq!(billing.storage_used_bytes, used + FIT * each);
    assert!(billing.storage_used_bytes <= billing.max_storage_bytes);
    assert_eq!(pulled_head(&s, &dev).await, head);
}

/// Subscribes `ws` to `plan` the way a customer does: server-created
/// checkout, payment at Paddle (fixture over HTTP), signed webhook.
async fn subscribe(
    s: &Stack,
    paddle: &PaddleFixture,
    u: &UserSession,
    ws: &str,
    plan: &str,
    seats: u32,
) -> WorkspaceBilling {
    let resp = session_send(
        s,
        u,
        reqwest::Method::POST,
        &format!("/v1/workspaces/{ws}/billing/checkout"),
        &serde_json::to_value(CreateCheckout {
            plan: plan.into(),
            seats,
        })
        .unwrap(),
    )
    .await;
    assert_eq!(resp.status(), 200);
    let txn = resp.json::<CheckoutSession>().await.unwrap().transaction_id;
    let sub = format!("sub_{}", uuid::Uuid::new_v4().simple());
    let customer = format!("ctm_{}", uuid::Uuid::new_v4().simple());
    let event = paddle.complete_checkout(&txn, &sub, &customer);
    let bytes = serde_json::to_vec(&event).unwrap();
    let sig = paddle.sign_webhook(&bytes, time::OffsetDateTime::now_utc().unix_timestamp());
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", sig)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let billing = billing_of(s, u, ws).await;
    assert_eq!(
        (billing.plan.as_str(), billing.plan_status),
        (plan, PlanStatus::Active)
    );
    billing
}

/// Links machines until the declared limit, then expects `507`.
async fn fill_machine_limit(s: &Stack, token: &AccountToken, ws: &str, already: usize, max: usize) {
    let keys: Vec<MachineKey> = (already..max)
        .map(|i| MachineKey::new(&format!("node-{i}")))
        .collect();
    for key in &keys {
        assert_eq!(try_link(s, token, ws, key).await.status(), 201);
    }
    let extra = MachineKey::new("one-too-many");
    let resp = try_link(s, token, ws, &extra).await;
    assert_eq!(resp.status(), 507);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::QUOTA_EXCEEDED
    );
    assert_eq!(devices_of(s, token, ws).await.len(), max);
}

/// A paid personal plan gets its own declared limits, enforced: its machine
/// limit, and pushes up to its larger bundle limit that the trial refuses.
#[tokio::test]
async fn a_personal_subscription_enforces_the_personal_limits() {
    let paddle = support::paddle_fixture::start().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let trial = billing_of(&s, &alice, &ws).await;
    let billing = subscribe(&s, &paddle, &alice, &ws, "personal", 1).await;
    assert!(billing.max_devices > trial.max_devices);
    assert!(billing.max_bundle_bytes > trial.max_bundle_bytes);

    // A push over the trial's per-push limit is within personal's.
    let head = repo.head();
    std::fs::write(
        repo.config.join("big.bin"),
        noise(trial.max_bundle_bytes + 1024 * 1024, 11),
    )
    .unwrap();
    let next = repo.commit("big");
    let bundle = repo.bundle(Some(&head));
    assert!(bundle.len() > trial.max_bundle_bytes && bundle.len() <= billing.max_bundle_bytes);
    let resp = dev.send(&s, &dev.push(bundle, Some(&head))).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(pulled_head(&s, &dev).await, next);

    fill_machine_limit(&s, &token, &ws, 1, billing.max_devices as usize).await;
}

/// A paid team plan's machine limit follows its seats.
#[tokio::test]
async fn a_team_subscription_enforces_its_per_seat_machine_limit() {
    let paddle = support::paddle_fixture::start().await;
    let s = stack_with_paddle(&paddle).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Team").await;
    let trial = billing_of(&s, &alice, &ws.id).await;
    let billing = subscribe(&s, &paddle, &alice, &ws.id, "team", 1).await;
    assert_eq!(billing.seats, 1);
    assert!(billing.max_devices > trial.max_devices);
    fill_machine_limit(&s, &token, &ws.id, 0, billing.max_devices as usize).await;
}

/// A refused first push writes nothing at all: the workspace still uses
/// zero bytes (no repository skeleton is created for it).
#[tokio::test]
async fn a_refused_first_push_creates_no_storage() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Empty").await;
    let key = MachineKey::new("node-0");
    let dev = link(&s, &token, &ws.id, &key).await;
    assert_eq!(billing_of(&s, &alice, &ws.id).await.storage_used_bytes, 0);

    // A legacy (unsigned) history is refused by the verifier.
    let legacy = FleetRepo::legacy(&key);
    let resp = dev.send(&s, &dev.push(legacy.bundle(None), None)).await;
    assert_eq!(resp.status(), 422);
    assert_eq!(billing_of(&s, &alice, &ws.id).await.storage_used_bytes, 0);
    assert_eq!(dev.send(&s, &dev.get(&dev.sync_path())).await.status(), 204);
}

/// The first accepted upload must include the bare repository's metadata
/// in its quota, even though the repository is created only after approval.
#[tokio::test]
async fn the_first_upload_counts_repository_metadata_in_its_quota() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let key = MachineKey::new("node-0");
    let (baseline, _, repo) = genesis_workspace(&s, &alice, &key).await;
    let needed = billing_of(&s, &alice, &baseline).await.storage_used_bytes;
    let ws = create_workspace(&s, &alice, "First upload boundary").await;
    let dev = link(&s, &token, &ws.id, &key).await;

    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", (needed - 1).to_string())])
        .await;
    let response = dev.send(&s, &dev.push(repo.bundle(None), None)).await;
    assert_eq!(response.status(), 507);
    assert_eq!(billing_of(&s, &alice, &ws.id).await.storage_used_bytes, 0);
    assert_eq!(dev.send(&s, &dev.get(&dev.sync_path())).await.status(), 204);

    s.restart_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", needed.to_string())])
        .await;
    let response = dev.send(&s, &dev.push(repo.bundle(None), None)).await;
    assert_eq!(response.status(), 200);
    let billing = billing_of(&s, &alice, &ws.id).await;
    assert_eq!(billing.storage_used_bytes, needed);
    assert!(billing.storage_used_bytes <= billing.max_storage_bytes);
}

/// A linked machine the accepted governance does not authorize cannot make
/// the service keep anything: its stale push is `403 device_not_approved`,
/// no proposal is retained and storage use is unchanged.
#[tokio::test]
async fn an_unauthorized_machine_cannot_retain_proposals() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let genesis = repo.head();
    let token = account_token(&s, &alice).await;
    let outsider_key = MachineKey::new("node-x");
    let outsider = link(&s, &token, &ws, &outsider_key).await;
    let next = repo.touch("next.txt", "moves the head");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 200);
    let before = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    let stale = FleetRepo::clone_from_bundle(&repo.bundle(None));
    git_reset(&stale, &genesis);
    std::fs::write(stale.config.join("junk.bin"), noise(512 * 1024, 11)).unwrap();
    stale.commit("junk");
    let resp = outsider
        .send(
            &s,
            &outsider.push(stale.bundle(Some(&genesis)), Some(&genesis)),
        )
        .await;
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::DEVICE_NOT_APPROVED
    );
    let resp = dev
        .send(&s, &dev.get(&format!("/v1/workspaces/{ws}/proposals")))
        .await;
    let list: Vec<muster_cloud::protocol::Proposal> = resp.json().await.unwrap();
    assert!(list.is_empty(), "retained: {list:?}");
    assert_eq!(billing_of(&s, &alice, &ws).await.storage_used_bytes, before);
    assert_eq!(pulled_head(&s, &dev).await, next);
}

/// Closing a retained proposal gives its storage back without an operator
/// step: once rejected, its bytes stop counting and a push the limit
/// refused fits again. Open proposals and the accepted history stay intact.
/// Precondition: the development-only small storage limit.
#[tokio::test]
async fn closed_proposals_release_their_storage() {
    const LIMIT: u64 = 1536 * 1024;
    let s = stack_with_env(&[("MUSTER_DEV_STORAGE_LIMIT_BYTES", LIMIT.to_string())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let key = MachineKey::new("node-0");
    let (ws, dev, repo) = genesis_workspace(&s, &alice, &key).await;
    let genesis = repo.head();
    let head = repo.touch("next.txt", "moves the head");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 200);
    let baseline = billing_of(&s, &alice, &ws).await.storage_used_bytes;

    // A small open conflict that must survive, then a big one that fills.
    let stale_push = |text: &str, bytes: usize, seed: u64| {
        let stale = FleetRepo::clone_from_bundle(&repo.bundle(None));
        git_reset(&stale, &genesis);
        std::fs::write(stale.config.join("big.bin"), noise(bytes, seed)).unwrap();
        let tip = stale.touch("why.txt", text);
        (tip, dev.push(stale.bundle(Some(&genesis)), Some(&genesis)))
    };
    let conflict_id = |e: ApiError| e.error.details["proposal_id"].as_str().unwrap().to_string();
    let (keep_tip, push) = stale_push("kept", 4 * 1024, 20);
    let resp = dev.send(&s, &push).await;
    assert_eq!(resp.status(), 409);
    let kept = conflict_id(resp.json().await.unwrap());
    let (_, push) = stale_push("big", 1 << 20, 21);
    let resp = dev.send(&s, &push).await;
    assert_eq!(resp.status(), 409);
    let big = conflict_id(resp.json().await.unwrap());
    let full = billing_of(&s, &alice, &ws).await.storage_used_bytes;
    assert!(full >= baseline + (1 << 20));
    let (_, refused) = stale_push("another", 1 << 20, 22);
    let resp = dev.send(&s, &refused).await;
    assert_eq!(resp.status(), 507);

    // Rejecting the big proposal releases its bytes (automatically).
    let resolve = format!("/v1/workspaces/{ws}/proposals/{big}/resolve");
    let resp = dev
        .send(
            &s,
            &dev.post_json(
                &resolve,
                &muster_cloud::protocol::ResolveProposal {
                    result_revision: None,
                },
            ),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let used = billing_of(&s, &alice, &ws).await.storage_used_bytes;
        if used + (1 << 20) <= full {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "storage not released: {used} (was {full})"
        );
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    // The refused push fits now (fresh key: the refusal was not stored).
    let (_, retry) = stale_push("another", 1 << 20, 22);
    let resp = dev.send(&s, &retry).await;
    assert_eq!(resp.status(), 409);

    // The open proposal and the accepted history are intact.
    let bundle = format!("/v1/workspaces/{ws}/proposals/{kept}/bundle");
    let resp = dev.send(&s, &dev.get(&bundle)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap()).head(),
        keep_tip
    );
    assert_eq!(pulled_head(&s, &dev).await, head);
    let resp = dev.send(&s, &dev.get(&dev.sync_path())).await;
    assert_eq!(
        FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap()).head(),
        head
    );
    // The closed proposal is still listed, without a downloadable bundle.
    let resp = dev
        .send(
            &s,
            &dev.get(&format!("/v1/workspaces/{ws}/proposals/{big}")),
        )
        .await;
    assert_eq!(resp.status(), 200);
    let resp = dev
        .send(
            &s,
            &dev.get(&format!("/v1/workspaces/{ws}/proposals/{big}/bundle")),
        )
        .await;
    assert_eq!(resp.status(), 404);
}
