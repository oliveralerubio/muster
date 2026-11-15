mod support;

use muster_cloud::protocol::{SyncAccepted, HEADER_EPOCH, HEADER_REVISION};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::stack;

#[tokio::test]
async fn push_genesis_bundle_and_pull_incremental_roundtrip() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Sync Test").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    assert!(dev_a.linked.bootstrap);

    // Genesis signed by the bootstrap machine, whose own governance lists it.
    let repo = FleetRepo::genesis(&key_a);
    let head = repo.head();
    let resp = dev_a.send(&s, &dev_a.push(repo.bundle(None), None)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), head);
    assert_eq!(resp.headers()[HEADER_EPOCH].to_str().unwrap(), "1");
    let accepted: SyncAccepted = resp.json().await.unwrap();
    assert_eq!(
        (accepted.revision.as_str(), accepted.epoch),
        (head.as_str(), 1)
    );

    // Full pull: a bundle whose main is the accepted head, full history.
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), head);
    assert_eq!(resp.headers()[HEADER_EPOCH].to_str().unwrap(), "1");
    let pulled = FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap());
    assert_eq!(pulled.head(), head);

    // Up to date: 204.
    let path = format!("{}?since={head}", dev_a.sync_path());
    let resp = dev_a.send(&s, &dev_a.get(&path)).await;
    assert_eq!(resp.status(), 204);
}

#[tokio::test]
async fn cas_mismatch_retains_proposal_and_rollback_or_unsigned_is_rejected() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "CAS & Rules").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;

    // A machine that does NOT hold the bootstrap claim cannot publish genesis.
    let key_b = MachineKey::new("node-b");
    let dev_b = link(&s, &token, &ws.id, &key_b).await;
    let repo_b = FleetRepo::genesis(&key_b);
    let resp = dev_b.send(&s, &dev_b.push(repo_b.bundle(None), None)).await;
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<muster_cloud::protocol::ApiError>()
            .await
            .unwrap()
            .error
            .code,
        muster_cloud::protocol::error_codes::DEVICE_NOT_APPROVED
    );

    // Genesis signed by a key its own governance does not list: the verifier
    // refuses the history (422), whoever uploads it.
    let repo_alien = FleetRepo::genesis_signed_by_outsider(&key_a, &key_b);
    let resp = dev_a
        .send(&s, &dev_a.push(repo_alien.bundle(None), None))
        .await;
    assert_eq!(resp.status(), 422);

    // Legacy unsigned repository is rejected with 422 invalid_history.
    let repo_legacy = FleetRepo::legacy(&key_a);
    let resp = dev_a
        .send(&s, &dev_a.push(repo_legacy.bundle(None), None))
        .await;
    assert_eq!(resp.status(), 422);
    assert_eq!(
        resp.json::<muster_cloud::protocol::ApiError>()
            .await
            .unwrap()
            .error
            .code,
        muster_cloud::protocol::error_codes::INVALID_HISTORY
    );

    // Legitimate genesis accepted.
    let mut repo_a = FleetRepo::genesis(&key_a);
    let genesis_head = repo_a.head();
    let genesis_bundle = repo_a.bundle(None);
    let resp = dev_a
        .send(&s, &dev_a.push(genesis_bundle.clone(), None))
        .await;
    assert_eq!(resp.status(), 200);

    // Epoch rollback: re-pushing an older state against the new head gives 422.
    // (Here: base claimed is the genesis head, but the bundle's content is an older genesis).
    let resp = dev_a
        .send(&s, &dev_a.push(repo_b.bundle(None), Some(&genesis_head)))
        .await;
    assert_eq!(resp.status(), 422);

    // Stale base (CAS mismatch): dev_a advances to epoch 2, dev_b tries to advance
    // from genesis. Returns 409 revision_conflict and retains the proposal.
    let epoch2_head = repo_a.add_machine(&key_a, &key_b);
    let resp = dev_a
        .send(
            &s,
            &dev_a.push(repo_a.bundle(Some(&genesis_head)), Some(&genesis_head)),
        )
        .await;
    assert_eq!(resp.status(), 200);

    // Stale push based on genesis:
    let stale_branch = FleetRepo::clone_from_bundle(&genesis_bundle);
    let stale_head = stale_branch.touch("notes.txt", "divergent");
    let stale_push = dev_a.push(
        stale_branch.bundle(Some(&genesis_head)),
        Some(&genesis_head),
    );
    let resp = dev_a.send(&s, &stale_push).await;
    assert_eq!(resp.status(), 409);
    let err: muster_cloud::protocol::ApiError = resp.json().await.unwrap();
    assert_eq!(
        err.error.code,
        muster_cloud::protocol::error_codes::REVISION_CONFLICT
    );
    assert_eq!(err.error.details["head"], epoch2_head);
    let proposal_id = err.error.details["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();

    // The head in the service remains at epoch 2.
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION],
        epoch2_head
    );

    // The retained proposal is readable via GET /v1/workspaces/{ws}/proposals.
    let proposals_path = format!("/v1/workspaces/{}/proposals", ws.id);
    let resp = dev_a.send(&s, &dev_a.get(&proposals_path)).await;
    assert_eq!(resp.status(), 200);
    let list: Vec<muster_cloud::protocol::Proposal> = resp.json().await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, proposal_id);
    assert_eq!(list[0].revision, stale_head);
    assert_eq!(list[0].base_revision, genesis_head);

    // Detail endpoint:
    let detail_path = format!("{proposals_path}/{proposal_id}");
    let resp = dev_a.send(&s, &dev_a.get(&detail_path)).await;
    assert_eq!(resp.status(), 200);
    let detail: muster_cloud::protocol::Proposal = resp.json().await.unwrap();
    assert_eq!(detail.id, proposal_id);

    // Retained bundle download roundtrip:
    let bundle_path = format!("{detail_path}/bundle");
    let resp = dev_a.send(&s, &dev_a.get(&bundle_path)).await;
    assert_eq!(resp.status(), 200);
    let recovered = FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap());
    assert_eq!(recovered.head(), stale_head);
}

#[tokio::test]
async fn sync_idempotency_and_durability_across_service_restarts() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Idem & Durability").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    let repo = FleetRepo::genesis(&key_a);
    let head = repo.head();
    let bundle = repo.bundle(None);

    let mut push = dev_a.push(bundle.clone(), None);
    let key = push.idempotency_key.clone().unwrap();

    let resp = dev_a.send(&s, &push).await;
    assert_eq!(resp.status(), 200);

    // Identical retry under the same idempotency key (fresh nonce and timestamp):
    push.nonce = support::fleet::fresh_nonce();
    push.timestamp = support::fleet::now();
    let resp = dev_a.send(&s, &push).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), head);

    // Same key with a different body is refused with 422 idempotency_mismatch.
    let (mut other_push, other_key) = (push.clone(), key.clone());
    let other_repo = FleetRepo::clone_from_bundle(&bundle);
    other_repo.touch("other.txt", "different");
    other_push.body = other_repo.bundle(None);
    other_push.nonce = support::fleet::fresh_nonce();
    other_push.idempotency_key = Some(other_key);
    let resp = dev_a.send(&s, &other_push).await;
    assert_eq!(resp.status(), 422);

    // DURABILITY: stop the service process and start a fresh one on the same
    // database and Git repo root. The accepted head and pull continue working.
    s.restart().await;

    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), head);
    let pulled = FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap());
    assert_eq!(pulled.head(), head);
}

#[tokio::test]
async fn pending_quorum_change_is_retained_as_proposal_not_accepted() {
    use muster_cloud::protocol::{Proposal, ProposalKind, ProposalStatus, SyncHeld};

    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Quorum").await;
    let key_a = MachineKey::new("node-a");
    let key_b = MachineKey::new("node-b");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;

    // Team genesis: two admins, two approvals required.
    let mut repo = FleetRepo::team_genesis(&key_a, &key_b);
    let genesis = repo.head();
    let resp = dev_a.send(&s, &dev_a.push(repo.bundle(None), None)).await;
    assert_eq!(resp.status(), 200);

    // A governance change signed by one admin only: held, not accepted.
    let pending = repo.revoke_recipient(&key_a, "age1gone");
    let resp = dev_a
        .send(&s, &dev_a.push(repo.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 202);
    let held: SyncHeld = resp.json().await.unwrap();
    assert_eq!(held.kind, ProposalKind::PendingQuorum);
    assert_eq!(held.status, ProposalStatus::Open);
    assert_eq!(held.head.as_ref().unwrap().revision, genesis);

    // The accepted head did not move, also after a restart.
    s.restart().await;
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), genesis);

    // The proposal and its complete history are retrievable.
    let detail = format!("/v1/workspaces/{}/proposals/{}", ws.id, held.proposal_id);
    let resp = dev_a.send(&s, &dev_a.get(&detail)).await;
    assert_eq!(resp.status(), 200);
    let proposal: Proposal = resp.json().await.unwrap();
    assert_eq!(proposal.kind, ProposalKind::PendingQuorum);
    assert_eq!(proposal.revision, pending);
    assert_eq!(proposal.base_revision, genesis);
    let resp = dev_a
        .send(&s, &dev_a.get(&format!("{detail}/bundle")))
        .await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        FleetRepo::clone_from_bundle(&resp.bytes().await.unwrap()).head(),
        pending
    );
}
