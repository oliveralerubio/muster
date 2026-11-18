mod support;

use muster_cloud::protocol::{
    DiffChange, RevisionDiff, RevisionItem, HEADER_EPOCH, HEADER_EXPORT_MANIFEST, HEADER_REVISION,
};
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::{http, stack, UserSession};

async fn session_get<T: serde::de::DeserializeOwned>(
    s: &support::Stack,
    u: &UserSession,
    path: &str,
) -> (reqwest::StatusCode, Option<T>) {
    let resp = http()
        .get(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    let status = resp.status();
    let body = if status.is_success() {
        Some(resp.json().await.unwrap())
    } else {
        None
    };
    (status, body)
}

#[tokio::test]
async fn export_serves_complete_git_bundle_verifiable_offline_by_oss() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Export Test").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;

    // Create genesis and push it.
    let mut repo = FleetRepo::genesis(&key_a);
    let head = repo.head();
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    // Advance to epoch 2.
    let key_b = MachineKey::new("node-b");
    let head2 = repo.add_machine(&key_a, &key_b);
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(Some(&head)), Some(&head)))
            .await
            .status(),
        200
    );

    // Download export via session GET /v1/workspaces/{ws}/export.
    let export_path = format!("/v1/workspaces/{}/export", ws.id);
    let resp = http()
        .get(s.url(&export_path))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()[HEADER_REVISION].to_str().unwrap(), head2);
    assert_eq!(resp.headers()[HEADER_EPOCH].to_str().unwrap(), "2");
    assert!(resp.headers().contains_key(HEADER_EXPORT_MANIFEST));

    let bundle_bytes = resp.bytes().await.unwrap();

    // Verify completely offline: clone from bundle and inspect full history.
    let restored = FleetRepo::clone_from_bundle(&bundle_bytes);
    assert_eq!(restored.head(), head2);

    // Check that history contains both commits.
    let log = support::fleet::git(&restored.config, &["log", "--oneline"]);
    assert!(log.contains("genesis"));
    assert!(log.contains("add node-b"));
}

#[tokio::test]
async fn revisions_list_and_diff_redacts_vault_secrets() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "History Test").await;

    let key_a = MachineKey::new("node-a");
    let dev_a = link(&s, &token, &ws.id, &key_a).await;

    let repo = FleetRepo::genesis(&key_a);
    let head1 = repo.head();
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    // Add a normal config file and a vault ciphertext file.
    let head2 = repo.touch("settings.toml", "[agent]\nmodel = 'opus'\n");
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo.bundle(Some(&head1)), Some(&head1)))
            .await
            .status(),
        200
    );

    // List revisions: newest first.
    let revisions_path = format!("/v1/workspaces/{}/revisions", ws.id);
    let (status, list) = session_get::<Vec<RevisionItem>>(&s, &alice, &revisions_path).await;
    assert_eq!(status, 200);
    let list = list.unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(list[0].revision, head2);
    assert_eq!(list[0].message, "touch settings.toml");
    assert_eq!(list[0].parent_revision.as_deref(), Some(head1.as_str()));
    assert_eq!(list[0].signer, key_a.public());
    assert_eq!(list[1].parent_revision, None);
    assert_eq!(list[1].revision, head1);

    // Diff of head2: non-vault shows patch; vault files are redacted if modified.
    let diff_path = format!("/v1/workspaces/{}/revisions/{}/diff", ws.id, head2);
    let (status, diff) = session_get::<RevisionDiff>(&s, &alice, &diff_path).await;
    assert_eq!(status, 200);
    let diff = diff.unwrap();
    assert_eq!(diff.revision, head2);
    let settings = diff
        .files
        .iter()
        .find(|f| f.path == "settings.toml")
        .unwrap();
    assert!(settings.patch.contains("model = 'opus'"));
    assert!(!settings.is_vault);
    assert_eq!(settings.change, DiffChange::Added);

    // Diff of genesis (contains vault/default.age).
    let genesis_diff_path = format!("/v1/workspaces/{}/revisions/{}/diff", ws.id, head1);
    let (status, diff_gen) = session_get::<RevisionDiff>(&s, &alice, &genesis_diff_path).await;
    assert_eq!(status, 200);
    let diff_gen = diff_gen.unwrap();
    let vault = diff_gen
        .files
        .iter()
        .find(|f| f.path.contains(".age"))
        .unwrap();
    assert!(vault.is_vault);
    assert_eq!(vault.patch, "[ciphertext changed]");
    assert!(!vault.patch.contains("synthetic-ciphertext"));
}
