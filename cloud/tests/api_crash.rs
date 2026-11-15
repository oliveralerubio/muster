//! What a service process leaves behind when it dies mid-request, and what
//! the next process on the same storage root does about it.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use muster_cloud::protocol::WorkspaceBilling;
use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
use support::{http, spawn_cloud, stack, Stack, UserSession};

fn repo_root(s: &Stack) -> PathBuf {
    PathBuf::from(
        &s.env
            .iter()
            .find(|(k, _)| *k == "MUSTER_REPO_ROOT")
            .unwrap()
            .1,
    )
}

/// Every upload staging directory (`stage.git`) under `dir`, recursively.
fn staging_dirs(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            if entry.file_name() == "stage.git" {
                found.push(path);
            } else {
                found.extend(staging_dirs(&path));
            }
        }
    }
    found
}

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

/// The signed request `dev.send` would make, as an owned future that does
/// not borrow the stack (so the stack can be killed while it is in flight).
fn send_owned(
    url: String,
    dev: &support::fleet::Device<'_>,
    req: &support::fleet::Signed,
) -> impl std::future::Future<Output = reqwest::Result<reqwest::Response>> + 'static {
    use muster_cloud::protocol::*;
    let mut builder = http()
        .post(url)
        .body(req.body.clone())
        .header("Authorization", format!("Bearer {}", dev.token))
        .header(HEADER_DEVICE_ID, &dev.id)
        .header(HEADER_TIMESTAMP, req.timestamp.to_string())
        .header(HEADER_NONCE, &req.nonce)
        .header(HEADER_CONTENT_SHA256, body_sha256_hex(&req.body))
        .header(HEADER_SIGNATURE, dev.signature(req));
    if let Some(base) = &req.base {
        builder = builder.header(HEADER_BASE_REVISION, base);
    }
    if let Some(key) = &req.idempotency_key {
        builder = builder.header(HEADER_IDEMPOTENCY_KEY, key);
    }
    builder.send()
}

/// Sends a large push and SIGKILLs the service while that push is being
/// staged. Returns true if the kill landed mid-staging (staging left on
/// disk); false if the push finished first.
async fn kill_mid_staging(
    s: &mut Stack,
    dev: &support::fleet::Device<'_>,
    req: &support::fleet::Signed,
) -> bool {
    let root = repo_root(s).join("tmp");
    let send = send_owned(s.url(&req.path_and_query), dev, req);
    tokio::pin!(send);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        tokio::select! {
            _ = &mut send => return false,
            _ = tokio::time::sleep(Duration::from_millis(1)) => {
                if !staging_dirs(&root).is_empty() {
                    s.cloud.kill().await;
                    return true;
                }
                assert!(Instant::now() < deadline, "push never staged");
            }
        }
    }
}

/// A process killed while staging an upload leaves its staging on disk; the
/// next process on the same storage reclaims it at startup, and the
/// workspace (head, usage) is as before the interrupted push.
#[tokio::test]
async fn staging_left_by_a_killed_process_is_reclaimed_on_restart() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Crash").await;
    let key = MachineKey::new("node-0");
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    let head = repo.head();
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    let used = billing_of(&s, &alice, &ws.id).await.storage_used_bytes;

    let max = billing_of(&s, &alice, &ws.id).await.max_bundle_bytes;
    std::fs::write(repo.config.join("big.bin"), noise(max - 512 * 1024, 1)).unwrap();
    repo.commit("big");
    let req = dev.push(repo.bundle(Some(&head)), Some(&head));
    assert!(
        kill_mid_staging(&mut s, &dev, &req).await,
        "the push completed before the kill could land"
    );
    let tmp = repo_root(&s).join("tmp");
    assert!(
        !staging_dirs(&tmp).is_empty(),
        "precondition: staging left by the kill"
    );

    s.cloud = spawn_cloud(&s.env).await;
    assert_eq!(
        staging_dirs(&tmp),
        Vec::<PathBuf>::new(),
        "staging not reclaimed"
    );
    let resp = dev.send(&s, &dev.get(&dev.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        head
    );
    assert_eq!(
        billing_of(&s, &alice, &ws.id).await.storage_used_bytes,
        used
    );
}

/// Leases (`<uuid>.lease`) in the shared scratch area.
fn leases(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(root.join("tmp/instances"))
        .map(|d| {
            d.flatten()
                .filter_map(|e| e.file_name().into_string().ok())
                .filter(|n| n.ends_with(".lease"))
                .collect()
        })
        .unwrap_or_default();
    out.sort();
    out
}

/// A second process starting on the same storage root (and database) while
/// the first is alive and mid-push reclaims nothing of the first: its
/// scratch area and in-flight staging survive, the push completes, and the
/// first process keeps serving.
#[tokio::test]
async fn a_second_instance_never_reclaims_a_live_instances_staging() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Shared").await;
    let key = MachineKey::new("node-0");
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    let head = repo.head();
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    let root = repo_root(&s);
    let live = leases(&root);
    assert_eq!(live.len(), 1, "the first process holds one lease");

    // A large push to the first process, in flight while the second starts.
    let max = billing_of(&s, &alice, &ws.id).await.max_bundle_bytes;
    std::fs::write(repo.config.join("big.bin"), noise(max - 512 * 1024, 2)).unwrap();
    let next = repo.commit("big");
    let req = dev.push(repo.bundle(Some(&head)), Some(&head));
    let push = tokio::spawn(send_owned(s.url(&req.path_and_query), &dev, &req));
    let deadline = Instant::now() + Duration::from_secs(20);
    while staging_dirs(&root.join("tmp")).is_empty() && !push.is_finished() {
        assert!(Instant::now() < deadline, "push never staged");
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut env = s.env.clone();
    env.retain(|(k, _)| *k != "MUSTER_BIND");
    env.push(("MUSTER_BIND", format!("127.0.0.1:{port}")));
    let mut second = spawn_cloud(&env).await;

    // The first process's lease and area are untouched by the second start.
    let after = leases(&root);
    assert!(
        live.iter().all(|l| after.contains(l)),
        "live lease reclaimed: {after:?}"
    );
    let area = root
        .join("tmp/instances")
        .join(live[0].trim_end_matches(".lease"));
    assert!(area.is_dir(), "live scratch area reclaimed");
    assert_eq!(after.len(), 2, "each process holds its own lease");

    let resp = push.await.unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    let resp = dev.send(&s, &dev.get(&dev.sync_path())).await;
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        next
    );
    second.kill().await;
}

/// Reclaim finishes what an interrupted reclaim left (a lease without its
/// area, an area without its lease, a lease with a half-removed area) and
/// touches nothing it does not own: other names in its area and legacy
/// scratch elsewhere under `tmp/` stay. Preconditions are written on disk
/// as a crashed process or reclaim would have left them.
#[tokio::test]
async fn reclaim_finishes_interrupted_cleanups_and_only_touches_its_own_area() {
    let mut s = stack().await;
    let root = repo_root(&s);
    let area = root.join("tmp/instances");
    s.cloud.kill().await;

    let lease_only = uuid::Uuid::new_v4();
    std::fs::write(area.join(format!("{lease_only}.lease")), b"").unwrap();
    let area_only = uuid::Uuid::new_v4();
    std::fs::create_dir_all(area.join(format!("{area_only}/half/stage.git/objects"))).unwrap();
    let both = uuid::Uuid::new_v4();
    std::fs::write(area.join(format!("{both}.lease")), b"").unwrap();
    std::fs::create_dir_all(area.join(format!("{both}/x/stage.git"))).unwrap();
    std::fs::write(area.join(format!("{both}/x/upload.bundle")), b"partial").unwrap();
    std::fs::write(area.join("notes.txt"), b"not ours").unwrap();
    std::fs::create_dir_all(area.join("not-a-uuid")).unwrap();
    let legacy = root.join("tmp/.tmpLegacy/stage.git");
    std::fs::create_dir_all(&legacy).unwrap();

    s.cloud = spawn_cloud(&s.env).await;
    for gone in [
        area.join(format!("{lease_only}.lease")),
        area.join(area_only.to_string()),
        area.join(format!("{both}.lease")),
        area.join(both.to_string()),
    ] {
        assert!(!gone.exists(), "not reclaimed: {}", gone.display());
    }
    assert!(area.join("notes.txt").exists());
    assert!(area.join("not-a-uuid").is_dir());
    assert!(
        legacy.is_dir(),
        "legacy scratch outside the leased area touched"
    );
    assert_eq!(
        leases(&root).len(),
        1,
        "only the running process's lease remains"
    );

    // Running reclaim again (another restart) is a no-op on what remains.
    s.restart().await;
    assert!(area.join("notes.txt").exists() && legacy.is_dir());
    assert_eq!(leases(&root).len(), 1);
}

/// What the backup gate (`deploy/pg-restore.sh`) requires of a workspace
/// repository: `git fsck` clean and `rev` reachable from an upload ref.
fn assert_backup_gate(repo: &Path, rev: &str) {
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .unwrap()
    };
    let fsck = git(&["fsck", "--no-progress", "--no-dangling"]);
    assert!(
        fsck.status.success(),
        "fsck: {}",
        String::from_utf8_lossy(&fsck.stderr)
    );
    let refs = git(&[
        "for-each-ref",
        "--format=%(refname)",
        "refs/muster/uploads/",
    ]);
    let refs = String::from_utf8_lossy(&refs.stdout).into_owned();
    let reachable = refs.lines().any(|r| {
        git(&["merge-base", "--is-ancestor", rev, r])
            .status
            .success()
    });
    assert!(reachable, "{rev} not reachable from upload refs");
}

/// A process killed (SIGKILL) right after accepting a push and retaining a
/// conflict: the next process serves exactly that head and the retained
/// bundle, and the repository passes the backup gate. (A crash, not a
/// power loss: the explicit flushes are not provable this way.)
#[tokio::test]
async fn accepted_and_retained_uploads_survive_a_kill_right_after() {
    let mut s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ws = create_workspace(&s, &alice, "Durable").await;
    let key = MachineKey::new("node-0");
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    let genesis = repo.head();
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    let stale = FleetRepo::clone_from_bundle(&repo.bundle(None));
    let head = repo.touch("a.txt", "accepted");
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(Some(&genesis)), Some(&genesis)))
            .await
            .status(),
        200
    );
    let conflict = stale.touch("b.txt", "retained");
    let resp = dev
        .send(&s, &dev.push(stale.bundle(Some(&genesis)), Some(&genesis)))
        .await;
    assert_eq!(resp.status(), 409);
    let err: muster_cloud::protocol::ApiError = resp.json().await.unwrap();
    let proposal = err.error.details["proposal_id"]
        .as_str()
        .unwrap()
        .to_string();
    s.cloud.kill().await;

    s.cloud = spawn_cloud(&s.env).await;
    let resp = dev.send(&s, &dev.get(&dev.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        head
    );
    let path = format!("/v1/workspaces/{}/proposals/{proposal}/bundle", ws.id);
    assert_eq!(dev.send(&s, &dev.get(&path)).await.status(), 200);
    let store = repo_root(&s).join(format!("{}.git", ws.id));
    assert_backup_gate(&store, &head);
    assert_backup_gate(&store, &conflict);
}
