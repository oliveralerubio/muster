//! Primary purge after the grace of a deletion request, over the public
//! boundaries: HTTP, real service processes (restarts, injected crashes),
//! `muster-cloud-ops` (purge and its residue report) and the repository
//! root as an artifact. SQL is never used to assert behaviour: erasure is
//! proven by `muster-cloud-ops lifecycle-report`, whose counts are shown
//! non-zero before the purge.
mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use muster_cloud::protocol::{canonical_link_v1, AccountToken, LinkDeviceRequest};
use serde_json::{json, Value};
use support::fleet::{
    account_token, fresh_nonce, link, now, Device, FleetRepo, MachineKey, Signed,
};
use support::{http, run_ops, stack_with_env, Stack, UserSession};

const GRACE: (&str, &str) = ("MUSTER_DEV_DELETION_GRACE_SECS", "1");

fn grace() -> Vec<(&'static str, String)> {
    vec![(GRACE.0, GRACE.1.into())]
}

fn value(s: &Stack, name: &str) -> String {
    s.env
        .iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.clone())
        .unwrap()
}

fn repo_root(s: &Stack) -> PathBuf {
    PathBuf::from(value(s, "MUSTER_REPO_ROOT"))
}

async fn session_get(s: &Stack, u: &UserSession, path: &str) -> (u16, Value) {
    let resp = http()
        .get(s.url(path))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

/// `POST /v1/workspaces` with a chosen key (to replay it later).
async fn create_with_key(s: &Stack, u: &UserSession, name: &str, key: &str) -> (u16, Value) {
    let resp = http()
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .header("Idempotency-Key", key)
        .json(&json!({ "name": name }))
        .send()
        .await
        .unwrap();
    let status = resp.status().as_u16();
    (status, resp.json().await.unwrap_or(Value::Null))
}

async fn link_with_key(
    s: &Stack,
    token: &AccountToken,
    ws: &str,
    key: &MachineKey,
    idem: &str,
) -> u16 {
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
        .header("Idempotency-Key", idem)
        .json(&LinkDeviceRequest {
            machine_name: key.name.clone(),
            signing_key: pk,
            recipient: key.recipient.clone(),
            proof,
        })
        .send()
        .await
        .unwrap()
        .status()
        .as_u16()
}

async fn request_deletion(s: &Stack, u: &UserSession, ws: &str, name: &str) -> Value {
    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{ws}")))
        .header("Cookie", format!("__Host-mc_session={}", u.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &u.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&json!({"confirm_workspace_id": ws, "confirm_workspace_name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    resp.json().await.unwrap()
}

/// `muster-cloud-ops lifecycle-report --workspace ws` (JSON).
async fn report(env: &[(&str, String)], ws: &str) -> Value {
    let out = run_ops(env, &["lifecycle-report", "--workspace", ws]).await;
    assert!(
        out.status.success(),
        "lifecycle-report: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Polls the requester's lifecycle view until `primary.status == want`.
async fn wait_primary(s: &Stack, u: &UserSession, ws: &str, want: &str, secs: u64) -> Value {
    let path = format!("/v1/workspaces/{ws}/lifecycle");
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let (status, body) = session_get(s, u, &path).await;
        assert_eq!(status, 200, "requester lifecycle view: {body}");
        if body["deletion"]["primary"]["status"] == want {
            return body;
        }
        assert!(Instant::now() < deadline, "never {want}: {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn again(req: &Signed) -> Signed {
    Signed {
        nonce: fresh_nonce(),
        timestamp: now(),
        ..req.clone()
    }
}

/// A tenant with a linked machine and one accepted push.
struct Tenant<'a> {
    id: String,
    name: String,
    create_key: String,
    link_key: String,
    dev: Device<'a>,
    repo: FleetRepo,
    push: Signed,
}

async fn tenant<'a>(
    s: &Stack,
    u: &UserSession,
    token: &AccountToken,
    name: &str,
    key: &'a MachineKey,
) -> Tenant<'a> {
    let create_key = uuid::Uuid::new_v4().to_string();
    let (status, ws) = create_with_key(s, u, name, &create_key).await;
    assert_eq!(status, 201, "{ws}");
    let id = ws["id"].as_str().unwrap().to_string();
    let dev = link(s, token, &id, key).await;
    let repo = FleetRepo::genesis(key);
    let push = dev.push(repo.bundle(None), None);
    assert_eq!(dev.send(s, &push).await.status(), 200);
    // A second link with a known key, to replay after the purge.
    let link_key = uuid::Uuid::new_v4().to_string();
    let extra = Box::leak(Box::new(MachineKey::new(&format!(
        "{}-extra",
        name.to_lowercase().replace(' ', "-")
    ))));
    assert_eq!(link_with_key(s, token, &id, extra, &link_key).await, 201);
    Tenant {
        id,
        name: name.into(),
        create_key,
        link_key,
        dev,
        repo,
        push,
    }
}

fn head(resp: &reqwest::Response) -> String {
    resp.headers()[muster_cloud::protocol::HEADER_REVISION]
        .to_str()
        .unwrap()
        .to_string()
}

/// Tenant B keeps its accepted head and accepts a new signed push; the
/// shared account session still works.
async fn assert_untouched(s: &Stack, alice: &UserSession, b: &Tenant<'_>) {
    let (status, me) = session_get(s, alice, "/v1/me").await;
    assert_eq!(status, 200, "session lost: {me}");
    let resp = b.dev.send(s, &b.dev.get(&b.dev.sync_path())).await;
    assert_eq!(resp.status(), 200);
    let current = head(&resp);
    let next = b
        .repo
        .touch(&format!("n{}.txt", uuid::Uuid::new_v4().simple()), "x");
    let push = b.dev.push(b.repo.bundle(Some(&current)), Some(&current));
    let resp = b.dev.send(s, &push).await;
    assert_eq!(resp.status(), 200, "tenant B push");
    let resp = b.dev.send(s, &b.dev.get(&b.dev.sync_path())).await;
    assert_eq!(head(&resp), next);
}

/// Everything about tenant A is gone or denied, including cached requests.
async fn assert_gone(s: &Stack, alice: &UserSession, token: &AccountToken, a: &Tenant<'_>) {
    let r = report(&s.env, &a.id).await;
    assert_eq!(r["residue"], false, "{r}");
    assert_eq!(r["db"]["tenant_rows"], 0, "{r}");
    assert_eq!(r["db"]["unredacted_idempotency"], 0, "{r}");
    assert_eq!(r["db"]["billing_event_payloads"], 0, "{r}");
    assert_eq!(r["fs"]["repo"], false, "{r}");
    assert_eq!(r["fs"]["trash"], 0, "{r}");
    assert!(!repo_root(s).join(format!("{}.git", a.id)).exists());

    let (_, list) = session_get(s, alice, "/v1/workspaces").await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == a.id.as_str()),
        "{list}"
    );
    for path in [
        format!("/v1/workspaces/{}", a.id),
        format!("/v1/workspaces/{}/export", a.id),
        format!("/v1/workspaces/{}/revisions", a.id),
        format!("/v1/workspaces/{}/members", a.id),
        format!("/v1/workspaces/{}/billing", a.id),
    ] {
        let (status, body) = session_get(s, alice, &path).await;
        assert_eq!(status, 404, "{path}: {body}");
    }
    // Cached requests never replay: creation, link, push.
    let (status, body) = create_with_key(s, alice, &a.name, &a.create_key).await;
    assert_eq!(status, 404, "creation replay: {body}");
    let extra = MachineKey::new("replay-extra");
    assert_eq!(
        link_with_key(s, token, &a.id, &extra, &a.link_key).await,
        404
    );
    let status = a.dev.send(s, &again(&a.push)).await.status().as_u16();
    assert!(status == 401 || status == 404, "push replay: {status}");
    let status = a
        .dev
        .send(s, &a.dev.get(&a.dev.sync_path()))
        .await
        .status()
        .as_u16();
    assert!(status == 401 || status == 404, "pull: {status}");
}

#[tokio::test]
async fn a_due_request_is_purged_only_after_verified_erasure_and_the_other_tenant_is_untouched() {
    let mut s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let a = tenant(&s, &alice, &token, "Tenant A", &ka).await;
    let b = tenant(&s, &alice, &token, "Tenant B", &kb).await;

    // The report sees real residue before anything happens.
    let before = report(&s.env, &a.id).await;
    assert_eq!(before["residue"], true, "{before}");
    assert!(
        before["db"]["tenant_rows"].as_u64().unwrap() > 0,
        "{before}"
    );
    assert!(
        before["db"]["unredacted_idempotency"].as_u64().unwrap() > 0,
        "{before}"
    );
    assert_eq!(before["fs"]["repo"], true, "{before}");

    let requested = request_deletion(&s, &alice, &a.id, &a.name).await;
    assert_eq!(requested["deletion"]["primary"]["status"], "scheduled");
    let purged = wait_primary(&s, &alice, &a.id, "purged", 20).await;
    let p = &purged["deletion"]["primary"];
    assert!(p["purge_committed_at"].is_string(), "{p}");
    assert!(p["purged_at"].is_string(), "{p}");
    assert!(p["purge_committed_at"].as_str() <= p["purged_at"].as_str());
    // Deadlines and the request never move; no backup deadline appears.
    let d = &purged["deletion"];
    assert_eq!(d["request_id"], requested["deletion"]["request_id"]);
    assert_eq!(d["export_until"], requested["deletion"]["export_until"]);
    assert_eq!(p["purge_at"], requested["deletion"]["primary"]["purge_at"]);
    assert_eq!(d["backups"]["status"], "unknown");
    assert!(d["backups"]["deadline"].is_null());
    assert_eq!(
        d["backups"]["cap_at"],
        requested["deletion"]["backups"]["cap_at"]
    );
    assert_eq!(d["pull_allowed"], false);
    assert_eq!(d["export_allowed"], false);

    assert_gone(&s, &alice, &token, &a).await;
    assert_untouched(&s, &alice, &b).await;
    s.restart().await;
    assert_gone(&s, &alice, &token, &a).await;
    assert_untouched(&s, &alice, &b).await;
    // The requester still sees the same terminal status after a restart.
    let after = wait_primary(&s, &alice, &a.id, "purged", 5).await;
    assert_eq!(after["deletion"], purged["deletion"]);
}

/// A live Git reader in ANOTHER process (this test, holding the
/// workspace's shared lock) blocks the erasure: the tombstone commits and
/// access ends, but nothing is erased and nothing claims `purged` until the
/// reader drains.
#[tokio::test]
async fn a_live_reader_in_another_process_is_drained_before_anything_is_erased() {
    let s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let a = tenant(&s, &alice, &token, "Held", &ka).await;

    let locks = repo_root(&s).join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(format!("{}.lock", a.id)))
        .unwrap();
    held.lock_shared().unwrap();

    request_deletion(&s, &alice, &a.id, &a.name).await;
    let committed = wait_primary(&s, &alice, &a.id, "purge_committed", 20).await;
    // Access already ended...
    let (status, _) = session_get(&s, &alice, &format!("/v1/workspaces/{}/export", a.id)).await;
    assert_eq!(status, 404);
    let (status, _) = session_get(&s, &alice, &format!("/v1/workspaces/{}/members", a.id)).await;
    assert_eq!(status, 404);
    let (_, list) = session_get(&s, &alice, "/v1/workspaces").await;
    assert!(
        !list
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == a.id.as_str()),
        "listed after the tombstone: {list}"
    );
    // ...but the reader still holds the data and nothing claims purged.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (_, view) = session_get(&s, &alice, &format!("/v1/workspaces/{}/lifecycle", a.id)).await;
    assert_eq!(
        view["deletion"]["primary"]["status"], "purge_committed",
        "{view}"
    );
    assert!(view["deletion"]["primary"]["purged_at"].is_null());
    assert!(
        view["deletion"]["primary"]["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("drain")),
        "{view}"
    );
    let r = report(&s.env, &a.id).await;
    assert_eq!(r["fs"]["repo"], true, "erased under a live reader: {r}");
    assert_eq!(r["residue"], true);
    assert_eq!(
        committed["deletion"]["primary"]["purge_committed_at"],
        view["deletion"]["primary"]["purge_committed_at"]
    );

    drop(held);
    wait_primary(&s, &alice, &a.id, "purged", 20).await;
    assert_gone(&s, &alice, &token, &a).await;
}

/// The service's own Git access takes part in the cross-process lock: while
/// another process (the test, standing in for a purge elsewhere) holds a
/// workspace's lock exclusive, a pull and a push of that workspace wait,
/// and complete once it is released.
#[tokio::test]
async fn git_access_waits_while_another_process_holds_the_workspace_lock() {
    let s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let a = tenant(&s, &alice, &token, "Waits", &ka).await;
    let resp = a.dev.send(&s, &a.dev.get(&a.dev.sync_path())).await;
    assert_eq!(resp.status(), 200);
    let current = head(&resp);
    a.repo.touch("w.txt", "x");
    let push = a.dev.push(a.repo.bundle(Some(&current)), Some(&current));
    let pull = a.dev.get(&a.dev.sync_path());

    let locks = repo_root(&s).join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(format!("{}.lock", a.id)))
        .unwrap();
    held.lock().unwrap();
    let start = Instant::now();
    let release = async {
        tokio::time::sleep(Duration::from_millis(1500)).await;
        drop(held);
    };
    let (dev, stack) = (&a.dev, &s);
    let timed = move |req| async move {
        let resp = dev.send(stack, req).await;
        (resp.status(), start.elapsed())
    };
    // Review F3: the billing view measures the Git storage too.
    let billing_path = format!("/v1/workspaces/{}/billing", a.id);
    let billing = async {
        let (status, _) = session_get(stack, &alice, &billing_path).await;
        (status, start.elapsed())
    };
    let ((pulled, pull_at), (pushed, push_at), (billed, billing_at), ()) =
        tokio::join!(timed(&pull), timed(&push), billing, release);
    assert_eq!(pulled, 200, "pull after {pull_at:?}");
    assert_eq!(pushed, 200, "push after {push_at:?}");
    assert_eq!(billed, 200, "billing after {billing_at:?}");
    let waited = Duration::from_millis(1400);
    assert!(
        billing_at >= waited,
        "billing view answered after {billing_at:?}, under the lock"
    );
    assert!(
        pull_at >= waited,
        "pull answered after {pull_at:?}, under the lock"
    );
    assert!(
        push_at >= waited,
        "push answered after {push_at:?}, under the lock"
    );
}

/// Pulls and pushes keep hitting the tenant while its grace expires and
/// two service processes (both running the purge worker) race to purge
/// it: every answer is a success or a denial, never a server error, and the
/// end state is fully erased.
#[tokio::test]
async fn concurrent_readers_and_pushes_are_drained_safely_across_two_processes() {
    let s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let a = tenant(&s, &alice, &token, "Busy", &ka).await;
    let b = tenant(&s, &alice, &token, "Calm", &kb).await;
    // A second service process on the same database, root and authority.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut env2 = s.env.clone();
    env2.retain(|(k, _)| *k != "MUSTER_BIND" && *k != "MUSTER_PUBLIC_URL");
    env2.push(("MUSTER_BIND", format!("127.0.0.1:{port}")));
    env2.push(("MUSTER_PUBLIC_URL", format!("http://127.0.0.1:{port}")));
    let _second = support::spawn_cloud(&env2).await;

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let traffic = {
        let (s, a, stop) = (&s, &a, stop.clone());
        async move {
            let mut statuses = Vec::new();
            let mut base = a.repo.head();
            let mut i = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                i += 1;
                let pull_req = a.dev.get(&a.dev.sync_path());
                let pull = a.dev.send(s, &pull_req);
                let next = a.repo.touch(&format!("f{i}.txt"), "busy");
                let push_req = a.dev.push(a.repo.bundle(Some(&base)), Some(&base));
                let push = a.dev.send(s, &push_req);
                let (pull, push) = tokio::join!(pull, push);
                statuses.push(pull.status().as_u16());
                let ps = push.status().as_u16();
                if ps == 200 {
                    base = next;
                }
                statuses.push(ps);
            }
            statuses
        }
    };
    let watcher = async {
        request_deletion(&s, &alice, &a.id, &a.name).await;
        wait_primary(&s, &alice, &a.id, "purged", 30).await;
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
    };
    let (statuses, ()) = tokio::join!(traffic, watcher);
    assert!(!statuses.is_empty());
    assert!(
        statuses.iter().all(|s| *s < 500 || *s == 503),
        "server errors during the purge: {statuses:?}"
    );
    assert!(statuses.contains(&200), "no traffic before the purge");
    assert_gone(&s, &alice, &token, &a).await;
    assert_untouched(&s, &alice, &b).await;
}

/// The service dies at `point` while purging; the next process resumes and
/// converges to a verified, fully erased `purged`.
/// One injected abort at a time in this binary: several processes dumping
/// core at once coincided with unrelated processes (a PostgreSQL backend)
/// being killed in this environment.
static ONE_CRASH_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn crash_and_resume(point: &str) {
    let _one = ONE_CRASH_AT_A_TIME.lock().await;
    let mut s = stack_with_env(&[
        (GRACE.0, GRACE.1.into()),
        ("MUSTER_DEV_PURGE_CRASH", point.into()),
    ])
    .await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let a = tenant(&s, &alice, &token, "Crashy", &ka).await;
    let b = tenant(&s, &alice, &token, "Steady", &kb).await;
    request_deletion(&s, &alice, &a.id, &a.name).await;
    // The worker aborts the process at the crash point.
    let deadline = Instant::now() + Duration::from_secs(20);
    while http().get(s.url("/healthz")).send().await.is_ok() {
        assert!(Instant::now() < deadline, "no crash at {point}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    s.env.retain(|(k, _)| *k != "MUSTER_DEV_PURGE_CRASH");
    s.restart().await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
    wait_primary(&s, &alice, &a.id, "purged", 20).await;
    assert_gone(&s, &alice, &token, &a).await;
    assert_untouched(&s, &alice, &b).await;
}

#[tokio::test]
async fn a_crash_right_after_the_tombstone_is_journaled_resumes() {
    crash_and_resume("after_tombstone").await;
}

#[tokio::test]
async fn a_crash_after_the_repository_moved_to_trash_resumes() {
    crash_and_resume("after_rename").await;
}

#[tokio::test]
async fn a_crash_after_the_database_erasure_resumes() {
    crash_and_resume("after_db").await;
}

#[tokio::test]
async fn a_crash_after_the_trash_erasure_resumes() {
    crash_and_resume("after_trash").await;
}

/// `muster-cloud-ops purge-due` runs the same purge (no service needed for
/// the purge itself) and reports it.
#[tokio::test]
async fn the_ops_command_purges_due_requests() {
    let mut s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let a = tenant(&s, &alice, &token, "By Ops", &ka).await;
    request_deletion(&s, &alice, &a.id, &a.name).await;
    // Stop the service: only ops purges.
    s.cloud.kill().await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let out = run_ops(&s.env, &["purge-due"]).await;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ran: Value = serde_json::from_slice(&out.stdout).unwrap();
    let mine = ran["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["workspace_id"] == a.id.as_str())
        .unwrap_or_else(|| panic!("{ran}"))
        .clone();
    assert_eq!(mine["state"], "purged", "{ran}");
    let r = report(&s.env, &a.id).await;
    assert_eq!(r["residue"], false, "{r}");
    assert_eq!(r["lifecycle"]["primary"], "purged", "{r}");
    // Running again is a no-op.
    let out = run_ops(&s.env, &["purge-due"]).await;
    assert!(out.status.success());
    s.restart().await;
    assert_gone(&s, &alice, &token, &a).await;
}

#[tokio::test]
async fn the_purge_crash_setting_is_refused_in_production() {
    let env: Vec<(&str, String)> = vec![
        ("MUSTER_ENV", "production".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "https://app.example.com".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
        ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client_123".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "secret_123".into()),
        ("MUSTER_DEV_PURGE_CRASH", "after_rename".into()),
    ];
    let (code, out) = support::run_cloud_expecting_exit(&env).await;
    assert_eq!(code, Some(78), "{out}");
    assert!(out.contains("MUSTER_DEV_PURGE_CRASH"), "{out}");
}

// ---------------------------------------------------------------------------
// The request and its provider cancellation outlive the purge
// ---------------------------------------------------------------------------

use support::paddle_fixture::{start as start_paddle, PaddleFixture};

async fn post_webhook(s: &Stack, paddle: &PaddleFixture, event: &Value) {
    let bytes = serde_json::to_vec(event).unwrap();
    let sig = paddle.sign_webhook(&bytes, time::OffsetDateTime::now_utc().unix_timestamp());
    let resp = http()
        .post(s.url("/v1/billing/webhook"))
        .header("Paddle-Signature", &sig)
        .body(bytes)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

/// Paddle is unreachable while the tenant is purged: the purge completes
/// on its own, the pending cancellation (kept outside the tenant rows)
/// is still retried and confirmed afterwards, a late webhook about it
/// leaves no payload behind, and retrying the deletion neither revives nor
/// changes the request.
#[tokio::test]
async fn the_request_and_a_pending_provider_cancellation_survive_the_purge() {
    let paddle = start_paddle().await;
    let mut s = support::stack_with_paddle(&paddle).await;
    s.restart_with_env(&[(GRACE.0, GRACE.1.into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let (status, ws) =
        create_with_key(&s, &alice, "Paid Gone", &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(status, 201);
    let id = ws["id"].as_str().unwrap().to_string();
    let resp = http()
        .post(s.url(&format!("/v1/workspaces/{id}/billing/checkout")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .json(&json!({"plan": "personal", "seats": 1}))
        .send()
        .await
        .unwrap();
    let c: Value = resp.json().await.unwrap();
    let sub = "sub_01h8vzp8f4kek3yd4g1purge001";
    let created = paddle.complete_checkout(
        c["transaction_id"].as_str().unwrap(),
        sub,
        "ctm_01h8vzp8f4kek3yd4g1purge001",
    );
    post_webhook(&s, &paddle, &created).await;
    let before = report(&s.env, &id).await;
    assert!(
        before["db"]["billing_event_payloads"].as_u64().unwrap() > 0,
        "{before}"
    );

    paddle.fail_next_gets(2);
    let requested = request_deletion(&s, &alice, &id, "Paid Gone").await;
    let purged = wait_primary(&s, &alice, &id, "purged", 20).await;
    assert_eq!(
        purged["deletion"]["request_id"],
        requested["deletion"]["request_id"]
    );
    let r = report(&s.env, &id).await;
    assert_eq!(r["residue"], false, "{r}");
    assert_eq!(r["db"]["billing_event_payloads"], 0, "{r}");
    assert_eq!(
        r["provider_items"]["pending"].as_u64().unwrap()
            + r["provider_items"]["confirmed"].as_u64().unwrap(),
        1,
        "{r}"
    );

    // The cancellation is confirmed after the purge.
    let deadline = Instant::now() + Duration::from_secs(40);
    let confirmed = loop {
        let (_, v) = session_get(&s, &alice, &format!("/v1/workspaces/{id}/lifecycle")).await;
        if v["deletion"]["provider"]["status"] == "confirmed" {
            break v;
        }
        assert!(Instant::now() < deadline, "{v}");
        tokio::time::sleep(Duration::from_millis(300)).await;
    };
    assert_eq!(paddle.subscription(sub).unwrap()["status"], "canceled");
    assert_eq!(confirmed["deletion"]["primary"]["status"], "purged");

    // A late webhook about the canceled subscription leaves no payload.
    let late = paddle.update_subscription(sub, "subscription.canceled", |_| {});
    post_webhook(&s, &paddle, &late).await;
    let r = report(&s.env, &id).await;
    assert_eq!(r["db"]["billing_event_payloads"], 0, "{r}");

    // Retrying the deletion cannot revive or reschedule anything.
    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{id}")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&json!({"confirm_workspace_id": id, "confirm_workspace_name": "Paid Gone"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let (_, v) = session_get(&s, &alice, &format!("/v1/workspaces/{id}/lifecycle")).await;
    assert_eq!(
        v["deletion"]["request_id"],
        requested["deletion"]["request_id"]
    );
    assert_eq!(
        v["deletion"]["export_until"],
        requested["deletion"]["export_until"]
    );
    // Somebody else never sees it.
    let mallory = s.login_user("mallory", "mallory@example.com").await;
    let (status, _) = session_get(&s, &mallory, &format!("/v1/workspaces/{id}/lifecycle")).await;
    assert_eq!(status, 404);
}

// ---------------------------------------------------------------------------
// Review findings (F1, F2, F4)
// ---------------------------------------------------------------------------

/// `cp -a src dst`: the test acting as an operator restoring files.
fn copy_tree(src: &std::path::Path, dst: &std::path::Path) {
    let st = std::process::Command::new("cp")
        .arg("-a")
        .arg(src)
        .arg(dst)
        .status()
        .unwrap();
    assert!(st.success());
}

async fn readyz(s: &Stack) -> u16 {
    http()
        .get(s.url("/readyz"))
        .send()
        .await
        .map(|r| r.status().as_u16())
        .unwrap_or(0)
}

/// Review F1: the repository of a purged workspace comes back on disk only
/// (a repository root restored on its own). The service is not ready while
/// it is there, nothing needs an operator: it is erased on its own once
/// live access drains, then the service is ready again.
#[tokio::test]
async fn restored_repository_files_of_a_purged_workspace_are_erased_on_their_own() {
    let mut s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let a = tenant(&s, &alice, &token, "Back", &ka).await;
    let b = tenant(&s, &alice, &token, "Stays", &kb).await;
    let root = repo_root(&s);
    let saved = tempfile::tempdir().unwrap();
    copy_tree(&root.join(format!("{}.git", a.id)), saved.path());

    request_deletion(&s, &alice, &a.id, &a.name).await;
    wait_primary(&s, &alice, &a.id, "purged", 20).await;
    assert_eq!(report(&s.env, &a.id).await["residue"], false);

    // The files come back; a live reader keeps the purge from draining.
    copy_tree(&saved.path().join(format!("{}.git", a.id)), &root);
    std::fs::create_dir_all(root.join("locks")).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join("locks").join(format!("{}.lock", a.id)))
        .unwrap();
    held.lock_shared().unwrap();
    s.restart().await;
    for _ in 0..10 {
        assert_eq!(
            readyz(&s).await,
            503,
            "ready with a purged workspace on disk"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let r = report(&s.env, &a.id).await;
    assert_eq!(r["residue"], true, "{r}");
    assert_eq!(r["fs"]["repo"], true, "{r}");

    drop(held);
    let deadline = Instant::now() + Duration::from_secs(30);
    while readyz(&s).await != 200 {
        assert!(Instant::now() < deadline, "never ready again");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let r = report(&s.env, &a.id).await;
    assert_eq!(r["residue"], false, "{r}");
    assert_eq!(r["fs"]["lock"], false, "{r}");
    assert!(!root.join(format!("{}.git", a.id)).exists());
    assert_untouched(&s, &alice, &b).await;
}

/// Review F4: a lock file of a purged workspace (left by a request that
/// raced the purge) is erased on its own.
#[tokio::test]
async fn a_lock_file_left_after_the_purge_is_erased_on_its_own() {
    let s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let a = tenant(&s, &alice, &token, "Lockless", &ka).await;
    request_deletion(&s, &alice, &a.id, &a.name).await;
    wait_primary(&s, &alice, &a.id, "purged", 20).await;
    let lock = repo_root(&s).join("locks").join(format!("{}.lock", a.id));
    std::fs::write(&lock, b"").unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let r = report(&s.env, &a.id).await;
        if r["fs"]["lock"] == false {
            assert_eq!(r["residue"], false, "{r}");
            break;
        }
        assert!(Instant::now() < deadline, "lock file kept: {r}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!lock.exists());
}

/// Review F4: requests about workspaces the caller cannot see (unknown ids,
/// or a workspace of someone else) never create lock files.
#[tokio::test]
async fn unknown_or_foreign_workspaces_get_no_lock_files() {
    let s = stack_with_env(&grace()).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let mallory = s.login_user("mallory", "mallory@example.com").await;
    let (status, ws) =
        create_with_key(&s, &alice, "Private", &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(status, 201);
    let private = ws["id"].as_str().unwrap().to_string();
    let random = uuid::Uuid::new_v4().to_string();
    for (who, id) in [(&mallory, &private), (&mallory, &random), (&alice, &random)] {
        for path in [
            format!("/v1/workspaces/{id}/export"),
            format!("/v1/workspaces/{id}/revisions/{}/diff", "0".repeat(40)),
            format!("/v1/workspaces/{id}/billing"),
        ] {
            let (status, _) = session_get(&s, who, &path).await;
            assert_eq!(status, 404, "{path}");
        }
    }
    let locks = repo_root(&s).join("locks");
    for id in [&private, &random] {
        assert!(
            !locks.join(format!("{id}.lock")).exists(),
            "lock file created for {id}"
        );
    }
}

/// Review F2: billing events about the workspace that are not linked to it
/// through its rows (a transaction or an old subscription naming it only in
/// `custom_data`) leave no payload behind, before or after the purge.
#[tokio::test]
async fn billing_events_naming_the_workspace_leave_no_payload() {
    let paddle = start_paddle().await;
    let mut s = support::stack_with_paddle(&paddle).await;
    s.restart_with_env(&[(GRACE.0, GRACE.1.into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let (status, ws) =
        create_with_key(&s, &alice, "Billed Gone", &uuid::Uuid::new_v4().to_string()).await;
    assert_eq!(status, 201);
    let id = ws["id"].as_str().unwrap().to_string();
    let named = |event_id: &str, event_type: &str, object: &str| {
        json!({
            "event_id": event_id,
            "event_type": event_type,
            "occurred_at": support::paddle_fixture::now_rfc3339(),
            "data": {
                "id": object,
                "status": "completed",
                "customer_id": "ctm_01h8vzp8f4kek3yd4g1named01",
                "custom_data": { "workspace_id": id, "plan": "personal" }
            }
        })
    };
    post_webhook(
        &s,
        &paddle,
        &named(
            "evt_01h8vzp8f4kek3yd4g1named01",
            "transaction.completed",
            "txn_01h8vzp8f4kek3yd4g1named01",
        ),
    )
    .await;
    post_webhook(
        &s,
        &paddle,
        &named(
            "evt_01h8vzp8f4kek3yd4g1named02",
            "subscription.canceled",
            "sub_01h8vzp8f4kek3yd4g1named02",
        ),
    )
    .await;

    request_deletion(&s, &alice, &id, "Billed Gone").await;
    wait_primary(&s, &alice, &id, "purged", 20).await;
    post_webhook(
        &s,
        &paddle,
        &named(
            "evt_01h8vzp8f4kek3yd4g1named03",
            "transaction.updated",
            "txn_01h8vzp8f4kek3yd4g1named03",
        ),
    )
    .await;

    let r = report(&s.env, &id).await;
    assert_eq!(r["db"]["billing_event_payloads"], 0, "{r}");
    assert_eq!(r["residue"], false, "{r}");
}
