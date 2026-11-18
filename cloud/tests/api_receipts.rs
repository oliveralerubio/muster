mod support;

use muster_cloud::protocol::{
    canonical_receipt_v1, error_codes, ApiError, Device as DeviceDto, Receipt, ReceiptOutcome,
    ReceiptV1, SubmitReceipt,
};
use support::fleet::{
    account_token, create_workspace, fresh_nonce, link, Device, FleetRepo, MachineKey,
};
use support::{http, stack, Stack};

fn receipt(
    dev: &Device<'_>,
    revision: &str,
    epoch: u64,
    sequence: u64,
    applied_at: &str,
) -> SubmitReceipt {
    let outcome = ReceiptOutcome::Applied;
    let signature = dev.key.signer.sign(&canonical_receipt_v1(&ReceiptV1 {
        workspace_id: &dev.workspace,
        device_id: &dev.id,
        revision,
        epoch,
        sequence,
        outcome: outcome.as_str(),
        applied_at,
    }));
    SubmitReceipt {
        revision: revision.into(),
        epoch,
        sequence,
        outcome,
        applied_at: applied_at.into(),
        signature,
    }
}

fn receipts_path(dev: &Device<'_>) -> String {
    format!("/v1/workspaces/{}/receipts", dev.workspace)
}

async fn last_receipt(s: &Stack, token: &str, ws: &str, device_id: &str) -> Option<Receipt> {
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/devices")))
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let devices: Vec<DeviceDto> = resp.json().await.unwrap();
    devices
        .into_iter()
        .find(|d| d.id == device_id)
        .unwrap()
        .last_receipt
}

#[tokio::test]
async fn signed_receipts_keep_sequence_accept_old_offline_and_reject_tamper() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Receipts").await;
    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;
    let repo = FleetRepo::genesis(&key_a);
    let head = repo.head();
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    let path = receipts_path(&dev_a);

    // Newest receipt first (sequence 5).
    let newest = receipt(&dev_a, &head, 1, 5, "2026-11-13T12:00:00Z");
    let resp = dev_a.send(&s, &dev_a.post_json(&path, &newest)).await;
    assert_eq!(resp.status(), 201);
    let stored: Receipt = resp.json().await.unwrap();
    assert_eq!(
        (stored.sequence, stored.applied_at.as_str()),
        (5, "2026-11-13T12:00:00Z")
    );
    assert!(!stored.received_at.is_empty());

    // An old offline receipt (sequence 2, applied days ago) delivered late
    // with a fresh request signature is kept as history...
    let old = receipt(&dev_a, &head, 1, 2, "2026-11-01T08:00:00Z");
    let resp = dev_a.send(&s, &dev_a.post_json(&path, &old)).await;
    assert_eq!(resp.status(), 201);
    let stored_old: Receipt = resp.json().await.unwrap();
    assert_eq!(stored_old.applied_at, "2026-11-01T08:00:00Z");
    // ...without displacing the newest one.
    let last = last_receipt(&s, &token.access_token, &ws.id, &dev_a.id)
        .await
        .unwrap();
    assert_eq!(last.sequence, 5);

    // Same receipt again (fresh nonce): idempotent, same received_at.
    let resp = dev_a.send(&s, &dev_a.post_json(&path, &old)).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.json::<Receipt>().await.unwrap().received_at,
        stored_old.received_at
    );

    // Different receipt reusing a sequence: idempotency_mismatch.
    let other = receipt(&dev_a, &head, 1, 2, "2026-11-02T08:00:00Z");
    let resp = dev_a.send(&s, &dev_a.post_json(&path, &other)).await;
    assert_eq!(resp.status(), 422);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::IDEMPOTENCY_MISMATCH
    );

    // Tampered receipt (signature over another sequence): refused.
    let mut forged = receipt(&dev_a, &head, 1, 9, "2026-11-13T13:00:00Z");
    forged.sequence = 10;
    let resp = dev_a.send(&s, &dev_a.post_json(&path, &forged)).await;
    assert_eq!(resp.status(), 401);
    assert_eq!(
        resp.json::<ApiError>().await.unwrap().error.code,
        error_codes::INVALID_SIGNATURE
    );

    // Replayed request (same nonce) is refused even with a valid receipt.
    let fresh = receipt(&dev_a, &head, 1, 6, "2026-11-13T14:00:00Z");
    let req = dev_a.post_json(&path, &fresh);
    assert_eq!(dev_a.send(&s, &req).await.status(), 201);
    let mut replay = req.clone();
    replay.body =
        serde_json::to_vec(&receipt(&dev_a, &head, 1, 7, "2026-11-13T14:00:00Z")).unwrap();
    assert_eq!(dev_a.send(&s, &replay).await.status(), 401);
    let mut replay_same = req.clone();
    replay_same.nonce = req.nonce.clone();
    assert_eq!(dev_a.send(&s, &replay_same).await.status(), 401);
    let _ = fresh_nonce;

    // Malformed receipt fields.
    let zero = receipt(&dev_a, &head, 1, 0, "2026-11-13T14:00:00Z");
    assert_eq!(
        dev_a
            .send(&s, &dev_a.post_json(&path, &zero))
            .await
            .status(),
        400
    );
    let bad_time = receipt(&dev_a, &head, 1, 8, "yesterday");
    assert_eq!(
        dev_a
            .send(&s, &dev_a.post_json(&path, &bad_time))
            .await
            .status(),
        400
    );

    let last = last_receipt(&s, &token.access_token, &ws.id, &dev_a.id)
        .await
        .unwrap();
    assert_eq!(last.sequence, 6);
}

#[tokio::test]
async fn receipts_are_isolated_per_workspace() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws1 = create_workspace(&s, &alice, "One").await;
    let ws2 = create_workspace(&s, &alice, "Two").await;
    let key = MachineKey::new("node-a");
    let dev1 = link(&s, &token, &ws1.id, &key).await;

    // A device of ws1 posting to ws2's receipts route: uniform 404.
    let r = receipt(&dev1, &"a".repeat(40), 1, 1, "2026-11-13T12:00:00Z");
    let path = format!("/v1/workspaces/{}/receipts", ws2.id);
    let resp = dev1.send(&s, &dev1.post_json(&path, &r)).await;
    assert_eq!(resp.status(), 404);

    // A receipt signed for ws2 but delivered to ws1 fails its signature.
    let mut cross = Device {
        key: &key,
        workspace: ws2.id.clone(),
        id: dev1.id.clone(),
        token: dev1.token.clone(),
        linked: dev1.linked.clone(),
    };
    let signed_for_ws2 = receipt(&cross, &"a".repeat(40), 1, 1, "2026-11-13T12:00:00Z");
    cross.workspace = ws1.id.clone();
    let resp = dev1
        .send(&s, &dev1.post_json(&receipts_path(&dev1), &signed_for_ws2))
        .await;
    assert_eq!(resp.status(), 401);
    assert!(last_receipt(&s, &token.access_token, &ws1.id, &dev1.id)
        .await
        .is_none());
}
