mod support;

use muster_cloud::protocol::{
    error_codes, ApiError, CreateProposal, Proposal, ProposalKind, ProposalStatus, ResolveProposal,
};
use support::fleet::{account_token, create_workspace, link, Device, FleetRepo, MachineKey};
use support::stack;

/// The JSON `muster enroll` writes (`muster::fleet::EnrollmentRequest`).
fn enrollment_request(key: &MachineKey) -> serde_json::Value {
    serde_json::json!({
        "name": key.name,
        "recipient": key.recipient,
        "signing_key": key.public(),
        "nonce": "n0nce-1234567890abcdef",
        "platform": "linux-x86_64",
        "muster_version": "0.1.0",
        "agents": [],
        "created_at": "2026-11-14T00:00:00Z",
    })
}

fn proposals_path(dev: &Device<'_>) -> String {
    format!("/v1/workspaces/{}/proposals", dev.workspace)
}

#[tokio::test]
async fn pending_machine_proposes_its_own_enrollment_and_admin_resolves_after_signing() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Enroll").await;
    let key_a = MachineKey::new("node-a");
    let key_b = MachineKey::new("node-b");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    let mut repo = FleetRepo::genesis(&key_a);
    let genesis = repo.head();
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    // Second machine: linked, not bootstrap, not in the signed governance.
    let dev_b = link(&s, &token, &ws.id, &key_b).await;
    assert!(!dev_b.linked.bootstrap);

    // It still cannot publish fleet changes: a governance change it signed
    // itself is refused by the verifier (its key is not in the accepted
    // governance), and the accepted head does not move...
    let mut own = FleetRepo::clone_from_bundle(&repo.bundle(None));
    own.add_machine(&key_b, &key_b);
    let resp = dev_b
        .send(&s, &dev_b.push(own.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 422);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::INVALID_HISTORY
    );
    let resp = dev_a.send(&s, &dev_a.get(&dev_a.sync_path())).await;
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        genesis
    );

    // ...but it may propose ITS OWN enrollment (signed request).
    let path = proposals_path(&dev_b);
    let create = CreateProposal {
        kind: ProposalKind::Enrollment,
        base_revision: genesis.clone(),
        payload: enrollment_request(&key_b),
    };
    let resp = dev_b.send(&s, &dev_b.post_json(&path, &create)).await;
    assert_eq!(resp.status(), 201);
    let proposal: Proposal = resp.json().await.unwrap();
    assert_eq!(proposal.kind, ProposalKind::Enrollment);
    assert_eq!(proposal.status, ProposalStatus::Open);
    assert_eq!(proposal.device_id.as_deref(), Some(dev_b.id.as_str()));
    assert_eq!(
        proposal.payload.as_ref().unwrap()["signing_key"],
        key_b.public()
    );

    // Same request again: idempotent, same proposal.
    let resp = dev_b.send(&s, &dev_b.post_json(&path, &create)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.json::<Proposal>().await.unwrap().id, proposal.id);

    // It cannot request enrollment of another machine's key.
    let mut foreign = create.clone();
    foreign.payload = enrollment_request(&key_a);
    let resp = dev_b.send(&s, &dev_b.post_json(&path, &foreign)).await;
    assert_eq!(resp.status(), 400);

    // Nor carry pre-filled admin approvals.
    let mut forged = create.clone();
    forged.payload["approvals"] = serde_json::json!([{"admin": "x", "signature": "y"}]);
    let resp = dev_b.send(&s, &dev_b.post_json(&path, &forged)).await;
    assert_eq!(resp.status(), 400);

    // A pending machine may not request a restore.
    let restore = CreateProposal {
        kind: ProposalKind::Restore,
        base_revision: genesis.clone(),
        payload: serde_json::json!({"target_revision": genesis, "paths": ["muster.toml"]}),
    };
    let resp = dev_b.send(&s, &dev_b.post_json(&path, &restore)).await;
    assert_eq!(resp.status(), 403);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::DEVICE_NOT_APPROVED
    );

    // The approved machine sees the request with its payload.
    let resp = dev_a.send(&s, &dev_a.get(&path)).await;
    let list: Vec<Proposal> = resp.json().await.unwrap();
    let seen = list.iter().find(|p| p.id == proposal.id).unwrap();
    assert_eq!(seen.payload.as_ref().unwrap()["name"], "node-b");

    // The pending machine cannot resolve anything.
    let resolve_path = format!("{path}/{}/resolve", proposal.id);
    let resp = dev_b
        .send(
            &s,
            &dev_b.post_json(
                &resolve_path,
                &ResolveProposal {
                    result_revision: None,
                },
            ),
        )
        .await;
    assert_eq!(resp.status(), 403);

    // A resolves with a revision that is not accepted: refused.
    let unaccepted = ResolveProposal {
        result_revision: Some("b".repeat(40)),
    };
    let resp = dev_a
        .send(&s, &dev_a.post_json(&resolve_path, &unaccepted))
        .await;
    assert_eq!(resp.status(), 422);

    // A signs B in LOCALLY and publishes; then resolves the request.
    let with_b = repo.add_machine(&key_a, &key_b);
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(Some(&genesis)), Some(&genesis)))
            .await
            .status(),
        200
    );
    let done = ResolveProposal {
        result_revision: Some(with_b.clone()),
    };
    let resp = dev_a.send(&s, &dev_a.post_json(&resolve_path, &done)).await;
    assert_eq!(resp.status(), 200);
    let resolved: Proposal = resp.json().await.unwrap();
    assert_eq!(resolved.status, ProposalStatus::Resolved);
    assert_eq!(resolved.result_revision.as_deref(), Some(with_b.as_str()));

    // Resolving again: 409 proposal_closed.
    let resp = dev_a.send(&s, &dev_a.post_json(&resolve_path, &done)).await;
    assert_eq!(resp.status(), 409);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::PROPOSAL_CLOSED
    );

    // Signed governance now authorizes B: it publishes normally.
    let b_repo = FleetRepo::clone_from_bundle(&repo.bundle(None));
    let b_head = b_repo.touch("notes.txt", "from b");
    let resp = dev_b
        .send(&s, &dev_b.push(b_repo.bundle(Some(&with_b)), Some(&with_b)))
        .await;
    assert_eq!(resp.status(), 200);
    let _ = b_head;
}
