//! `deploy/pg-restore.sh` with the lifecycle authority, driven as real
//! processes: a real service (synthetic authority from the real
//! `muster-cloud-ops init`), the real `deploy/pg-backup.sh`, the real
//! restore script, then the real ops gate and a real service on the
//! restored targets. Nothing is asserted by reading SQL or control files.
mod support;

use std::path::{Path, PathBuf};
use std::process::Output;

use support::{http, run_ops, spawn_cloud, stack, test_pg_url, Env};

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../deploy")
        .join(name)
}

fn value(env: &Env, name: &str) -> String {
    env.iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("{name} not in env"))
}

fn with(env: &Env, name: &'static str, v: &str) -> Env {
    let mut e: Env = env.iter().filter(|(k, _)| *k != name).cloned().collect();
    e.push((name, v.into()));
    e
}

async fn run(path: &Path, args: &[&str], extra: &[(&str, String)]) -> Output {
    let mut cmd = tokio::process::Command::new(path);
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env(
            "TMPDIR",
            std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()),
        )
        .env("MUSTER_CLOUD_OPS", env!("CARGO_BIN_EXE_muster-cloud-ops"))
        .args(args);
    for (k, v) in extra {
        cmd.env(k, v);
    }
    cmd.output().await.expect("run deploy script")
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// A synthetic age key pair at `<path>`: returns the recipient (public key).
fn age_key(path: &Path) -> String {
    let st = std::process::Command::new("age-keygen")
        .arg("-o")
        .arg(path)
        .stderr(std::process::Stdio::null())
        .status()
        .expect("age-keygen (install age)");
    assert!(st.success());
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .find_map(|l| l.strip_prefix("# public key: ").map(str::to_string))
        .expect("public key line")
}

/// The identity next to a managed backup root `<dir>`: `<dir>.key`.
fn identity_for(root: &Path) -> PathBuf {
    root.with_extension("key")
}

/// `muster-cloud-ops backup run` into the managed root `root` (encrypted to
/// the key at `<root>.key`, created on first use), with `extra` settings.
async fn backup_run(
    env: &Env,
    root: &Path,
    args: &[&str],
    extra: &[(&'static str, String)],
) -> Output {
    let key = identity_for(root);
    let recipient = if key.exists() {
        std::fs::read_to_string(&key)
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("# public key: ").map(str::to_string))
            .unwrap()
    } else {
        age_key(&key)
    };
    let mut env = with(
        &with(env, "MUSTER_BACKUP_ROOT", &root.display().to_string()),
        "MUSTER_BACKUP_AGE_RECIPIENT",
        &recipient,
    );
    for (k, v) in extra {
        env = with(&env, k, v);
    }
    let mut all = vec!["backup", "run"];
    all.extend_from_slice(args);
    run_ops(&env, &all).await
}

/// A published snapshot directory from `backup run`'s JSON output.
fn published(out: &Output) -> PathBuf {
    assert!(out.status.success(), "backup: {}", text(out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("backup output {e}: {}", text(out)));
    PathBuf::from(v["path"].as_str().unwrap_or_else(|| panic!("{v}")))
}

/// An encrypted backup of the stack's database and repository root.
async fn backup(s: &support::Stack, out_dir: &Path) -> PathBuf {
    published(&backup_run(&s.env, &out_dir.join("backups"), &[], &[]).await)
}

struct Target {
    db: String,
    url: String,
    repos: PathBuf,
    _dir: tempfile::TempDir,
}

fn new_target() -> Target {
    let dir = tempfile::tempdir().unwrap();
    let db = format!("r_{}", uuid::Uuid::new_v4().simple());
    let mut url = url::Url::parse(&test_pg_url()).unwrap();
    url.set_path(&format!("/{db}"));
    Target {
        db,
        url: url.to_string(),
        repos: dir.path().join("restored-repos"),
        _dir: dir,
    }
}

/// Runs the restore script into `t`, with the authority of `authority`
/// (its control dir and witness), or none.
async fn restore(bk: &Path, t: &Target, authority: Option<&Env>) -> Output {
    let mut args = vec![
        "--backup".to_string(),
        bk.display().to_string(),
        "--admin-url".into(),
        test_pg_url(),
        "--new-db".into(),
        t.db.clone(),
        "--new-repo-root".into(),
        t.repos.display().to_string(),
    ];
    if let Some(a) = authority {
        args.extend([
            "--control-dir".into(),
            value(a, "MUSTER_CONTROL_DIR"),
            "--control-witness".into(),
            value(a, "MUSTER_CONTROL_WITNESS"),
        ]);
    }
    // Encrypted snapshots: the identity of their managed root.
    let root = bk.parent().and_then(Path::parent).map(Path::to_path_buf);
    if let Some(key) = root.map(|r| identity_for(&r)).filter(|k| k.exists()) {
        args.extend(["--age-identity-file".into(), key.display().to_string()]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&script("pg-restore.sh"), &args, &[]).await
}

/// A copy of the whole control store (journal dir + witness) on new paths.
fn copy_authority(env: &Env) -> (tempfile::TempDir, Env) {
    let root = tempfile::tempdir().unwrap();
    let control = root.path().join("control");
    let witness_dir = root.path().join("witness");
    std::fs::create_dir_all(&control).unwrap();
    std::fs::create_dir_all(&witness_dir).unwrap();
    for entry in std::fs::read_dir(value(env, "MUSTER_CONTROL_DIR")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), control.join(entry.file_name())).unwrap();
    }
    let witness = witness_dir.join("witness.json");
    std::fs::copy(value(env, "MUSTER_CONTROL_WITNESS"), &witness).unwrap();
    let env = with(
        &with(env, "MUSTER_CONTROL_DIR", &control.display().to_string()),
        "MUSTER_CONTROL_WITNESS",
        &witness.display().to_string(),
    );
    (root, env)
}

async fn seeded_stack() -> support::Stack {
    let s = stack().await;
    s.login_user("alice", "alice@example.com").await;
    s
}

/// Environment of a service on the restored targets `t`.
fn target_env(s: &support::Stack, t: &Target) -> Env {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    with(
        &with(
            &with(
                &with(&s.env, "MUSTER_DATABASE_URL", &t.url),
                "MUSTER_REPO_ROOT",
                &t.repos.display().to_string(),
            ),
            "MUSTER_BIND",
            &format!("127.0.0.1:{port}"),
        ),
        "MUSTER_PUBLIC_URL",
        &format!("http://127.0.0.1:{port}"),
    )
}

/// `(incarnation_id, role)` of the database at `url`, from the read-only
/// ops gate.
async fn incarnation(env: &Env, url: &str) -> (String, String) {
    let out = run_ops(env, &["restore-check", "--database-url", url]).await;
    assert!(out.status.success(), "{}", text(&out));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    (
        v["db"]["incarnation_id"].as_str().unwrap().to_string(),
        v["db"]["role"].as_str().unwrap().to_string(),
    )
}

/// The cutover: the restored candidate is activated (fencing every other
/// incarnation, the source included) and a real service starts on it.
async fn cutover(s: &support::Stack, t: &Target) -> (support::Cloud, Env) {
    let (id, role) = incarnation(&s.env, &t.url).await;
    assert_eq!(role, "candidate", "a restore must join as a new candidate");
    let out = run_ops(
        &s.env,
        &["activate", "--incarnation", &id, "--database-url", &t.url],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let env = target_env(s, t);
    let cloud = spawn_cloud(&env).await;
    (cloud, env)
}

/// The restored targets pass the ops gate as a new candidate incarnation;
/// after the cutover a real service on them is ready.
async fn assert_restored_serves(s: &support::Stack, t: &Target) {
    let (cloud, _) = cutover(s, t).await;
    let ready = http()
        .get(format!("{}/readyz", cloud.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
}

/// Exit code of a service started with `env` (it must refuse to serve).
async fn refused_start(env: &Env) -> Option<i32> {
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_muster-cloud"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().unwrap();
    match tokio::time::timeout(std::time::Duration::from_secs(30), child.wait()).await {
        Ok(status) => status.unwrap().code(),
        Err(_) => panic!("the service kept running on a database it must not serve"),
    }
}

#[tokio::test]
async fn restore_requires_the_authority_and_creates_nothing_without_it() {
    let s = seeded_stack().await;
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;
    let t = new_target();

    let out = restore(&bk, &t, None).await;
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    assert!(text(&out).contains("--control-dir"), "{}", text(&out));
    assert!(
        !t.repos.exists(),
        "nothing may be created without authority"
    );

    // Same targets, with the authority: restored and served.
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert!(text(&out).contains("authority verified"), "{}", text(&out));
    assert_restored_serves(&s, &t).await;

    // Restoring into existing targets is still refused.
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(66), "{}", text(&out));
}

#[tokio::test]
async fn restore_refuses_a_missing_witness_and_tears_down_its_targets() {
    let s = seeded_stack().await;
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;
    let t = new_target();
    let (_copy, broken) = copy_authority(&s.env);
    std::fs::remove_file(value(&broken, "MUSTER_CONTROL_WITNESS")).unwrap();

    let out = restore(&bk, &t, Some(&broken)).await;
    assert_eq!(out.status.code(), Some(73), "{}", text(&out));
    assert!(
        text(&out).contains("control witness missing"),
        "{}",
        text(&out)
    );
    assert!(!t.repos.exists());
    // Torn down: the same targets restore fine with the real authority.
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

#[tokio::test]
async fn restore_refuses_a_foreign_authority() {
    let s = seeded_stack().await;
    let other = stack().await;
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;
    let t = new_target();

    let out = restore(&bk, &t, Some(&other.env)).await;
    assert_eq!(out.status.code(), Some(73), "{}", text(&out));
    assert!(text(&out).contains("another deployment"), "{}", text(&out));
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
}

/// A rolled-back journal (stale against its witness) is refused, and so is
/// an intact but older control store that the backup is ahead of.
#[tokio::test]
async fn restore_refuses_a_stale_authority() {
    let s = seeded_stack().await;
    let (_old_root, old) = copy_authority(&s.env); // at record 2
    let candidate = support::fresh_database().await;
    let reg = run_ops(&s.env, &["register", "--database-url", &candidate]).await;
    assert!(reg.status.success(), "{}", text(&reg));
    // The live database projects record 3 on its next probe.
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;

    // Intact older store: the backup is ahead of it.
    let t = new_target();
    let out = restore(&bk, &t, Some(&old)).await;
    assert_eq!(out.status.code(), Some(73), "{}", text(&out));
    assert!(
        text(&out).contains("ahead of the control journal"),
        "{}",
        text(&out)
    );

    // Rolled-back journal under the current witness.
    let (_rb_root, rolled_back) = copy_authority(&s.env);
    let dir = value(&rolled_back, "MUSTER_CONTROL_DIR");
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    for entry in std::fs::read_dir(value(&old, "MUSTER_CONTROL_DIR")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), Path::new(&dir).join(entry.file_name())).unwrap();
    }
    let out = restore(&bk, &t, Some(&rolled_back)).await;
    assert_eq!(out.status.code(), Some(73), "{}", text(&out));
    assert!(text(&out).contains("behind its witness"), "{}", text(&out));

    // The real authority restores and serves.
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    assert_restored_serves(&s, &t).await;
}

/// A deletion request made AFTER the backup survives restoring that older
/// backup: the record lives in the external journal, and the service on the
/// restored targets replays it before it is ready.
#[tokio::test]
async fn a_restored_older_backup_still_carries_a_later_deletion_request() {
    let s = stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let ws = support::fleet::create_workspace(&s, &alice, "Kept Gone").await;
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;

    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{}", ws.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({
            "confirm_workspace_id": ws.id,
            "confirm_workspace_name": "Kept Gone",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let requested: serde_json::Value = resp.json().await.unwrap();

    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let (cloud, _) = cutover(&s, &t).await;
    let resp = http()
        .get(format!("{}/v1/workspaces/{}/lifecycle", cloud.base, ws.id))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let seen: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(seen["state"], "deletion_scheduled", "{seen}");
    assert_eq!(
        seen["deletion"]["request_id"],
        requested["deletion"]["request_id"]
    );
    assert_eq!(
        seen["deletion"]["export_until"],
        requested["deletion"]["export_until"]
    );
}

/// A device request sent to another service base (a restored instance).
async fn send_at(
    base: &str,
    dev: &support::fleet::Device<'_>,
    req: &support::fleet::Signed,
) -> reqwest::Response {
    use muster_cloud::protocol::*;
    let url = format!("{base}{}", req.path_and_query);
    let client = http();
    let mut b = match req.method {
        "GET" => client.get(url),
        _ => client.post(url).body(req.body.clone()),
    };
    b = b
        .header("Authorization", format!("Bearer {}", dev.token))
        .header(HEADER_DEVICE_ID, &dev.id)
        .header(HEADER_TIMESTAMP, req.timestamp.to_string())
        .header(HEADER_NONCE, &req.nonce)
        .header(HEADER_CONTENT_SHA256, body_sha256_hex(&req.body))
        .header(HEADER_SIGNATURE, dev.signature(req));
    if let Some(base) = &req.base {
        b = b.header(HEADER_BASE_REVISION, base);
    }
    if let Some(key) = &req.idempotency_key {
        b = b.header(HEADER_IDEMPOTENCY_KEY, key);
    }
    b.send().await.unwrap()
}

/// The purge of tenant A is journaled outside the database: restoring a
/// backup taken BEFORE A's deletion request, the service on the restored
/// targets reapplies the request, the tombstone and the erasure before it
/// is ready. A stays gone (rows, repository, cached requests); tenant B,
/// in the same backup, keeps its exact head, the shared session and
/// accepts a new signed push.
#[tokio::test]
async fn a_pre_request_backup_restores_without_resurrecting_a_purged_tenant() {
    use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let a = create_workspace(&s, &alice, "Purged A").await;
    let b = create_workspace(&s, &alice, "Kept B").await;
    let dev_a = link(&s, &token, &a.id, &ka).await;
    let dev_b = link(&s, &token, &b.id, &kb).await;
    let repo_a = FleetRepo::genesis(&ka);
    let repo_b = FleetRepo::genesis(&kb);
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo_a.bundle(None), None))
            .await
            .status(),
        200
    );
    assert_eq!(
        dev_b
            .send(&s, &dev_b.push(repo_b.bundle(None), None))
            .await
            .status(),
        200
    );
    let b_head = repo_b.head();

    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;

    // Delete A and wait for the purge on the live instance.
    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{}", a.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"confirm_workspace_id": a.id, "confirm_workspace_name": "Purged A"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let v: serde_json::Value = http()
            .get(s.url(&format!("/v1/workspaces/{}/lifecycle", a.id)))
            .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if v["deletion"]["primary"]["status"] == "purged" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{v}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // Restore the PRE-request backup into fresh targets.
    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    // The restore itself reapplied the journal and erased A (the backup
    // held it) before its integrity gate: nothing waits for a service.
    assert!(
        !t.repos.join(format!("{}.git", a.id)).exists(),
        "A's repository restored"
    );
    let (cloud, env) = cutover(&s, &t).await;
    let ready = http()
        .get(format!("{}/readyz", cloud.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);

    // A: erased before the restored service served anything.
    assert!(
        !t.repos.join(format!("{}.git", a.id)).exists(),
        "A's repository resurrected"
    );
    let out = run_ops(&env, &["lifecycle-report", "--workspace", &a.id]).await;
    assert!(out.status.success(), "{}", text(&out));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["residue"], false, "{r}");
    assert_eq!(r["lifecycle"]["primary"], "purged", "{r}");
    let get = |path: String| {
        let cookie = alice.cookie.clone();
        let base = cloud.base.clone();
        async move {
            http()
                .get(format!("{base}{path}"))
                .header("Cookie", format!("__Host-mc_session={cookie}"))
                .send()
                .await
                .unwrap()
        }
    };
    let list: serde_json::Value = get("/v1/workspaces".into()).await.json().await.unwrap();
    let ids: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec![b.id.as_str()], "{list}");
    assert_eq!(get(format!("/v1/workspaces/{}", a.id)).await.status(), 404);
    assert_eq!(
        get(format!("/v1/workspaces/{}/export", a.id))
            .await
            .status(),
        404
    );
    let pull_a = send_at(&cloud.base, &dev_a, &dev_a.get(&dev_a.sync_path())).await;
    assert!(
        matches!(pull_a.status().as_u16(), 401 | 404),
        "{}",
        pull_a.status()
    );

    // B: same head, session works, new signed push accepted.
    let resp = send_at(&cloud.base, &dev_b, &dev_b.get(&dev_b.sync_path())).await;
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        b_head
    );
    let next = repo_b.touch("after-restore.txt", "kept");
    let push = dev_b.push(repo_b.bundle(Some(&b_head)), Some(&b_head));
    let resp = send_at(&cloud.base, &dev_b, &push).await;
    assert_eq!(resp.status(), 200);
    let resp = send_at(&cloud.base, &dev_b, &dev_b.get(&dev_b.sync_path())).await;
    assert_eq!(
        resp.headers()[muster_cloud::protocol::HEADER_REVISION]
            .to_str()
            .unwrap(),
        next
    );
}

/// Review F5, then unit 5: on a database restored from before the request,
/// the restore itself reapplies the external journal and erases the
/// restored residue (no service started); `lifecycle-report` agrees.
#[tokio::test]
async fn ops_reapply_the_journal_on_a_pre_request_restore_without_the_service() {
    use support::fleet::{account_token, create_workspace, link, FleetRepo, MachineKey};
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let a = create_workspace(&s, &alice, "Purged A").await;
    let dev_a = link(&s, &token, &a.id, &ka).await;
    let repo_a = FleetRepo::genesis(&ka);
    assert_eq!(
        dev_a
            .send(&s, &dev_a.push(repo_a.bundle(None), None))
            .await
            .status(),
        200
    );
    let bk_dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, bk_dir.path()).await;

    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{}", a.id)))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"confirm_workspace_id": a.id, "confirm_workspace_name": "Purged A"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let v: serde_json::Value = http()
            .get(s.url(&format!("/v1/workspaces/{}/lifecycle", a.id)))
            .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        if v["deletion"]["primary"]["status"] == "purged" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{v}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let env = with(
        &with(&s.env, "MUSTER_DATABASE_URL", &t.url),
        "MUSTER_REPO_ROOT",
        &t.repos.display().to_string(),
    );
    let report = |env: Vec<(&'static str, String)>| {
        let ws = a.id.clone();
        async move {
            let out = run_ops(&env, &["lifecycle-report", "--workspace", &ws]).await;
            assert!(out.status.success(), "{}", text(&out));
            serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()
        }
    };
    // The restore reapplied the journal and erased A's restored residue
    // itself, without any service; the report agrees with the journal.
    let after = report(env.clone()).await;
    assert_eq!(after["lifecycle"]["primary"], "purged", "{after}");
    assert_eq!(after["residue"], false, "{after}");
    assert_eq!(after["lifecycle"]["projection_behind"], false, "{after}");
    assert!(!t.repos.join(format!("{}.git", a.id)).exists());
    // A candidate never journals: purge-due refuses it (only the active
    // incarnation purges).
    let out = run_ops(&env, &["purge-due"]).await;
    assert_eq!(out.status.code(), Some(65), "{}", text(&out));
}

// ---------------------------------------------------------------------------
// Lifecycle unit 4: snapshot-bound encrypted capture, catalogue, retention
// ---------------------------------------------------------------------------

use support::fleet::{account_token, create_workspace, link, Device, FleetRepo, MachineKey};

fn sha256_file(path: &Path) -> String {
    use sha2::Digest;
    let bytes = std::fs::read(path).unwrap();
    sha2::Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Decrypts `file` with `identity` (the age CLI, as an operator would).
fn age_decrypt(identity: &Path, file: &Path, out: &Path) {
    let st = std::process::Command::new("age")
        .arg("-d")
        .arg("-i")
        .arg(identity)
        .arg("-o")
        .arg(out)
        .arg(file)
        .status()
        .unwrap();
    assert!(st.success(), "age -d {}", file.display());
}

fn tar_list(tar: &Path) -> Vec<String> {
    let out = std::process::Command::new("tar")
        .arg("-tf")
        .arg(tar)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn manifest(snapshot: &Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(snapshot.join("manifest.json")).unwrap()).unwrap()
}

fn tenant_ids(m: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = m["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["workspace_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// A workspace with a linked machine and a signed genesis push.
async fn pushed<'a>(
    s: &support::Stack,
    alice: &support::UserSession,
    token: &muster_cloud::protocol::AccountToken,
    name: &str,
    key: &'a MachineKey,
) -> (String, Device<'a>) {
    let ws = create_workspace(s, alice, name).await;
    let dev = link(s, token, &ws.id, key).await;
    let repo = FleetRepo::genesis(key);
    assert_eq!(
        dev.send(s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );
    (ws.id, dev)
}

async fn delete_ws(s: &support::Stack, alice: &support::UserSession, ws: &str, name: &str) {
    let resp = http()
        .delete(s.url(&format!("/v1/workspaces/{ws}")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"confirm_workspace_id": ws, "confirm_workspace_name": name}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 202);
}

async fn lifecycle(
    s: &support::Stack,
    alice: &support::UserSession,
    ws: &str,
) -> serde_json::Value {
    let resp = http()
        .get(s.url(&format!("/v1/workspaces/{ws}/lifecycle")))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    resp.json().await.unwrap()
}

/// Starts `backup run` paused right after its snapshot (development
/// `MUSTER_DEV_BACKUP_PAUSE_FILE`); returns once it is waiting there.
async fn paused_backup(env: &Env, root: &Path, pause: &Path) -> tokio::task::JoinHandle<Output> {
    paused_backup_at(env, root, pause, "after_snapshot").await
}

/// `backup run` paused at a development point (`MUSTER_DEV_BACKUP_PAUSE_AT`).
async fn paused_backup_at(
    env: &Env,
    root: &Path,
    pause: &Path,
    point: &str,
) -> tokio::task::JoinHandle<Output> {
    let (env, root) = (env.clone(), root.to_path_buf());
    let p = pause.display().to_string();
    let point = point.to_string();
    let task = tokio::spawn(async move {
        backup_run(
            &env,
            &root,
            &[],
            &[
                ("MUSTER_DEV_BACKUP_PAUSE_FILE", p),
                ("MUSTER_DEV_BACKUP_PAUSE_AT", point),
            ],
        )
        .await
    });
    let waiting = pause.with_extension("waiting");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !waiting.exists() {
        assert!(!task.is_finished(), "backup ended before pausing");
        assert!(std::time::Instant::now() < deadline, "backup never paused");
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    task
}

/// Encrypted by default, the archive holds exactly the snapshot's tenants:
/// not the root's scratch, not an orphan repository, not a tenant created
/// after the snapshot (while the capture was still running).
#[tokio::test]
async fn an_encrypted_snapshot_holds_exactly_its_tenants() {
    let s = support::stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb, kc) = (
        MachineKey::new("a-node"),
        MachineKey::new("b-node"),
        MachineKey::new("c-node"),
    );
    let (a, _) = pushed(&s, &alice, &token, "A", &ka).await;
    let (b, _) = pushed(&s, &alice, &token, "B", &kb).await;
    let repos = PathBuf::from(value(&s.env, "MUSTER_REPO_ROOT"));
    let orphan = uuid::Uuid::new_v4();
    std::fs::create_dir_all(repos.join(format!("{orphan}.git/objects"))).unwrap();
    std::fs::write(repos.join("stray.txt"), b"not a tenant").unwrap();

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let pause = dir.path().join("go");
    let task = paused_backup(&s.env, &root, &pause).await;
    let (c, _) = pushed(&s, &alice, &token, "C", &kc).await;
    std::fs::write(&pause, b"").unwrap();
    let out = task.await.unwrap();
    let snap = published(&out);

    let m = manifest(&snap);
    assert_eq!(m["format"], "muster-backup", "{m}");
    assert_eq!(m["version"], 2, "{m}");
    assert_eq!(m["encryption"]["scheme"], "age-x25519", "{m}");
    let mut want = vec![a.clone(), b.clone()];
    want.sort();
    assert_eq!(tenant_ids(&m), want, "{m}");
    for art in m["artifacts"].as_array().unwrap() {
        let file = snap.join(art["name"].as_str().unwrap());
        assert_eq!(sha256_file(&file), art["sha256"].as_str().unwrap(), "{art}");
        let head = std::fs::read(&file).unwrap();
        assert!(
            head.starts_with(b"age-encryption.org/v1"),
            "{} not age",
            file.display()
        );
    }
    let plain = dir.path().join("git.tar");
    age_decrypt(&identity_for(&root), &snap.join("git.tar.age"), &plain);
    let entries = tar_list(&plain);
    let allowed = [format!("{a}.git"), format!("{b}.git")];
    for e in &entries {
        let top = e.trim_start_matches("./").split('/').next().unwrap();
        assert!(
            allowed.iter().any(|x| x == top),
            "unexpected archive entry {e}"
        );
    }
    for x in &allowed {
        assert!(
            entries
                .iter()
                .any(|e| e.trim_start_matches("./").starts_with(&format!("{x}/"))),
            "{x} missing"
        );
    }
    assert!(!entries
        .iter()
        .any(|e| e.contains(&c) || e.contains(&orphan.to_string()) || e.contains("tmp/")));
    // The dump is that same snapshot: A and B, never C.
    let dump = dir.path().join("db.dump");
    age_decrypt(&identity_for(&root), &snap.join("db.dump.age"), &dump);
    let rows = std::process::Command::new("pg_restore")
        .args(["--data-only", "--table=workspaces", "--file=-"])
        .arg(&dump)
        .output()
        .unwrap();
    assert!(
        rows.status.success(),
        "{}",
        String::from_utf8_lossy(&rows.stderr)
    );
    let rows = String::from_utf8(rows.stdout).unwrap();
    assert!(
        rows.contains(&a) && rows.contains(&b),
        "dump lacks a snapshot tenant"
    );
    assert!(
        !rows.contains(&c),
        "dump holds a tenant created after the snapshot"
    );
}

/// Plaintext only as an explicit synthetic choice in development; never in
/// production, and never by default (no recipient → refused).
#[tokio::test]
async fn plaintext_is_refused_in_production_and_never_the_default() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let base = with(&s.env, "MUSTER_BACKUP_ROOT", &root.display().to_string());
    let no_key = run_ops(&base, &["backup", "run"]).await;
    assert_eq!(no_key.status.code(), Some(65), "{}", text(&no_key));
    let prod = with(&base, "MUSTER_ENV", "production");
    let refused = run_ops(&prod, &["backup", "run", "--plaintext-synthetic"]).await;
    assert_eq!(refused.status.code(), Some(65), "{}", text(&refused));
    assert!(
        !root.join("snapshots").exists()
            || std::fs::read_dir(root.join("snapshots"))
                .unwrap()
                .next()
                .is_none()
    );
    // The wrapper script maps --plaintext to the same refusal in production.
    let out = run(
        &script("pg-backup.sh"),
        &[
            "--database-url",
            &value(&s.env, "MUSTER_DATABASE_URL"),
            "--repo-root",
            &value(&s.env, "MUSTER_REPO_ROOT"),
            "--output-dir",
            &root.display().to_string(),
            "--plaintext",
        ],
        &[
            ("MUSTER_ENV", "production".into()),
            ("MUSTER_CONTROL_DIR", value(&s.env, "MUSTER_CONTROL_DIR")),
            (
                "MUSTER_CONTROL_WITNESS",
                value(&s.env, "MUSTER_CONTROL_WITNESS"),
            ),
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(65), "{}", text(&out));

    let ok = run_ops(&base, &["backup", "run", "--plaintext-synthetic"]).await;
    let snap = published(&ok);
    assert_eq!(manifest(&snap)["encryption"]["scheme"], "none");
}

/// Retention removes only catalogued, published, expired snapshots; it
/// leaves uncatalogued entries and live operations alone, cleans up dead
/// operations, and records an auditable result.
#[tokio::test]
async fn retention_removes_only_expired_catalogued_snapshots() {
    let s = support::stack_with_env(&[("MUSTER_DEV_BACKUP_RETENTION_SECS", "2".into())]).await;
    s.login_user("alice", "alice@example.com").await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let env = with(&s.env, "MUSTER_DEV_BACKUP_RETENTION_SECS", "2");
    let old = published(&backup_run(&env, &root, &[], &[]).await);
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    let fresh = published(&backup_run(&env, &root, &[], &[]).await);

    // Uncatalogued entries: never removed, always reported.
    std::fs::create_dir_all(root.join("snapshots/not-a-backup")).unwrap();
    std::fs::create_dir_all(root.join("muster-backup-20200101T000000Z")).unwrap();
    // A live operation (its lease held) and a dead one.
    let (live, dead) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
    for op in [live, dead] {
        std::fs::create_dir_all(root.join(format!("staging/{op}"))).unwrap();
        std::fs::write(root.join(format!("staging/{op}/part")), b"x").unwrap();
        std::fs::write(
            root.join(format!("ops/{op}.json")),
            serde_json::to_vec(&serde_json::json!({"v":1,"op_id":op,"kind":"backup","started_at":"2026-01-01T00:00:00Z","pid":1,"checkpoint":"db"})).unwrap(),
        )
        .unwrap();
    }
    let lease = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(format!("ops/{live}.lock")))
        .unwrap();
    lease.lock().unwrap();
    std::fs::write(root.join(format!("ops/{dead}.lock")), b"").unwrap();

    let out = run_ops(
        &with(&env, "MUSTER_BACKUP_ROOT", &root.display().to_string()),
        &["retention", "run"],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let old_id = old.file_name().unwrap().to_str().unwrap();
    let fresh_id = fresh.file_name().unwrap().to_str().unwrap();
    assert_eq!(r["removed"], serde_json::json!([old_id]), "{r}");
    assert!(
        r["kept"].as_array().unwrap().iter().any(|k| k == fresh_id),
        "{r}"
    );
    assert_eq!(r["uncatalogued"], 2, "{r}");
    assert!(!old.exists());
    assert!(fresh.exists());
    assert!(root.join("snapshots/not-a-backup").exists());
    assert!(root.join("muster-backup-20200101T000000Z").exists());
    assert!(
        root.join(format!("staging/{live}/part")).exists(),
        "live operation touched"
    );
    assert!(
        !root.join(format!("staging/{dead}")).exists(),
        "dead operation kept"
    );
    let audits: Vec<_> = std::fs::read_dir(root.join("audit")).unwrap().collect();
    assert_eq!(audits.len(), 1);
    let audit: serde_json::Value =
        serde_json::from_slice(&std::fs::read(audits[0].as_ref().unwrap().path()).unwrap())
            .unwrap();
    assert_eq!(audit["removed"], r["removed"]);
    drop(lease);
}

/// The deletion cap drives backup expiry and its status: a request made
/// while a capture runs still binds that snapshot; the deadline is never
/// later than the cap; `scheduled` → `due` → `purged` with `purged_at`.
/// Uncatalogued backups make it honestly `unknown`.
#[tokio::test]
async fn the_deletion_cap_expires_snapshots_and_drives_the_backup_status() {
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "6".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let (a, _) = pushed(&s, &alice, &token, "Capped", &ka).await;
    let (b, _) = pushed(&s, &alice, &token, "Other", &kb).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let ops_env = with(&s.env, "MUSTER_BACKUP_ROOT", &root.display().to_string());

    let pause = dir.path().join("go");
    let task = paused_backup(&s.env, &root, &pause).await;
    delete_ws(&s, &alice, &a, "Capped").await;
    std::fs::write(&pause, b"").unwrap();
    let snap = published(&task.await.unwrap());
    assert!(tenant_ids(&manifest(&snap)).contains(&a));

    let v = lifecycle(&s, &alice, &a).await;
    let bk = &v["deletion"]["backups"];
    assert_eq!(bk["status"], "scheduled", "{v}");
    assert_eq!(bk["deadline"], bk["cap_at"], "{v}");
    assert!(bk["purged_at"].is_null());

    // Past the cap, before retention ran: due, never extended.
    let cap = time::OffsetDateTime::parse(
        bk["cap_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    let wait = cap - time::OffsetDateTime::now_utc() + time::Duration::milliseconds(300);
    if wait.is_positive() {
        tokio::time::sleep(wait.unsigned_abs()).await;
    }
    let v = lifecycle(&s, &alice, &a).await;
    assert_eq!(v["deletion"]["backups"]["status"], "due", "{v}");
    assert_eq!(v["deletion"]["backups"]["deadline"], bk["deadline"]);

    let out = run_ops(&ops_env, &["retention", "run"]).await;
    assert!(out.status.success(), "{}", text(&out));
    assert!(!snap.exists(), "expired snapshot kept");
    let v = lifecycle(&s, &alice, &a).await;
    let after = &v["deletion"]["backups"];
    assert_eq!(after["status"], "purged", "{v}");
    assert!(after["purged_at"].is_string(), "{v}");

    // Uncatalogued backups: honest unknown, no promised deadline.
    std::fs::create_dir_all(root.join("snapshots/stranger")).unwrap();
    let out = run_ops(&ops_env, &["retention", "run"]).await;
    assert!(out.status.success(), "{}", text(&out));
    delete_ws(&s, &alice, &b, "Other").await;
    let v = lifecycle(&s, &alice, &b).await;
    assert_eq!(v["deletion"]["backups"]["status"], "unknown", "{v}");
    assert!(v["deletion"]["backups"]["deadline"].is_null(), "{v}");
    assert!(
        v["deletion"]["backups"]["reason"]
            .as_str()
            .unwrap()
            .contains("uncatalogued"),
        "{v}"
    );
}

/// The normal retention ends before the cap: the deadline is the earlier.
#[tokio::test]
async fn a_normal_expiry_earlier_than_the_cap_is_the_deadline() {
    let s = support::stack_with_env(&[
        ("MUSTER_DEV_DELETION_GRACE_SECS", "600".into()),
        ("MUSTER_DEV_BACKUP_RETENTION_SECS", "60".into()),
    ])
    .await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let (a, _) = pushed(&s, &alice, &token, "Early", &ka).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let env = with(&s.env, "MUSTER_DEV_BACKUP_RETENTION_SECS", "60");
    let snap = published(&backup_run(&env, &root, &[], &[]).await);
    delete_ws(&s, &alice, &a, "Early").await;
    let v = lifecycle(&s, &alice, &a).await;
    let bk = &v["deletion"]["backups"];
    assert_eq!(bk["status"], "scheduled", "{v}");
    assert_eq!(bk["deadline"], manifest(&snap)["normal_expires_at"], "{v}");
    assert_ne!(bk["deadline"], bk["cap_at"], "{v}");
}

/// A capture in progress holds the purge before it erases anything: the
/// purge waits (reported), the snapshot is consistent (the tenant's
/// repository is in it), and the purge completes afterwards on its own.
#[tokio::test]
async fn a_purge_waits_for_a_running_capture() {
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let (a, _) = pushed(&s, &alice, &token, "Racing", &ka).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let pause = dir.path().join("go");
    let task = paused_backup(&s.env, &root, &pause).await;
    delete_ws(&s, &alice, &a, "Racing").await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let v = lifecycle(&s, &alice, &a).await;
        let p = &v["deletion"]["primary"];
        assert_ne!(p["status"], "purged", "purged during a capture: {v}");
        if p["status"] == "purge_committed"
            && p["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("backup"))
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{v}");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    assert!(PathBuf::from(value(&s.env, "MUSTER_REPO_ROOT"))
        .join(format!("{a}.git"))
        .exists());
    std::fs::write(&pause, b"").unwrap();
    let snap = published(&task.await.unwrap());
    let plain = dir.path().join("git.tar");
    age_decrypt(&identity_for(&root), &snap.join("git.tar.age"), &plain);
    assert!(tar_list(&plain)
        .iter()
        .any(|e| e.trim_start_matches("./").starts_with(&format!("{a}.git/"))));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while lifecycle(&s, &alice, &a).await["deletion"]["primary"]["status"] != "purged" {
        assert!(std::time::Instant::now() < deadline, "purge never resumed");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

static ONE_BACKUP_CRASH_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A capture killed at any step publishes nothing; the next run cleans up
/// what it left and publishes a complete snapshot. The setting is refused
/// in production.
#[tokio::test]
async fn a_capture_killed_at_any_step_leaves_nothing_catalogued() {
    let _one = ONE_BACKUP_CRASH_AT_A_TIME.lock().await;
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    for point in ["after_snapshot", "after_db", "after_git", "before_publish"] {
        let out = backup_run(
            &s.env,
            &root,
            &[],
            &[("MUSTER_DEV_BACKUP_CRASH", point.into())],
        )
        .await;
        assert!(!out.status.success(), "{point}: {}", text(&out));
        let snaps = std::fs::read_dir(root.join("snapshots"))
            .map(|d| d.count())
            .unwrap_or(0);
        assert_eq!(snaps, 0, "{point}: something was published");
    }
    let snap = published(&backup_run(&s.env, &root, &[], &[]).await);
    assert!(snap.join("manifest.json").exists());
    assert_eq!(
        std::fs::read_dir(root.join("staging")).unwrap().count(),
        0,
        "dead captures left staging"
    );
    let prod = backup_run(
        &with(&s.env, "MUSTER_ENV", "production"),
        &root,
        &[],
        &[("MUSTER_DEV_BACKUP_CRASH", "after_db".into())],
    )
    .await;
    assert_eq!(prod.status.code(), Some(78), "{}", text(&prod));
}

/// The systemd units are staged (never installed for real) only from a
/// valid operator environment file.
#[tokio::test]
async fn the_timer_installer_validates_before_staging_units() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let env_file = dir.path().join("ops.env");
    let good = "MUSTER_DATABASE_URL=postgres://muster@localhost/muster\n\
MUSTER_REPO_ROOT=/var/lib/muster-cloud/repos\n\
MUSTER_CONTROL_DIR=/var/lib/muster-cloud/control\n\
MUSTER_CONTROL_WITNESS=/var/lib/muster-cloud-witness/witness.json\n\
MUSTER_BACKUP_ROOT=/var/backups/muster-cloud\n\
MUSTER_BACKUP_AGE_RECIPIENT=age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p\n";
    let install = |extra: Vec<String>| {
        let stage = stage.clone();
        let env_file = env_file.clone();
        async move {
            let mut args = vec![
                "--root".to_string(),
                stage.display().to_string(),
                "--env-file".into(),
                env_file.display().to_string(),
            ];
            args.extend(extra);
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            run(&script("install-systemd.sh"), &args, &[]).await
        }
    };
    use std::os::unix::fs::PermissionsExt;
    let write = |content: &str, mode: u32| {
        std::fs::write(&env_file, content).unwrap();
        std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(mode)).unwrap();
    };
    for (bad, why) in [
        (good.replace("MUSTER_BACKUP_AGE_RECIPIENT=age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p\n", ""), "recipient"),
        (good.replace("/var/backups/muster-cloud", "backups"), "absolute"),
        (format!("{good}MUSTER_DEV_BACKUP_RETENTION_SECS=1\n"), "MUSTER_DEV_"),
        (good.replace("/var/backups/muster-cloud", "/var/lib/muster-cloud/repos/backups"), "disjoint"),
    ] {
        write(&bad, 0o600);
        let out = install(vec![]).await;
        assert!(!out.status.success(), "accepted ({why})");
        assert!(text(&out).contains(why), "{why}: {}", text(&out));
        assert!(!stage.join("etc/systemd/system").exists(), "staged with a bad file ({why})");
    }
    write(good, 0o644);
    let out = install(vec![]).await;
    assert!(
        !out.status.success() && text(&out).contains("0600"),
        "{}",
        text(&out)
    );
    write(good, 0o600);
    let out = install(vec![]).await;
    assert!(out.status.success(), "{}", text(&out));
    for unit in [
        "muster-cloud-backup.service",
        "muster-cloud-backup.timer",
        "muster-cloud-retention.service",
        "muster-cloud-retention.timer",
    ] {
        let staged = stage.join("etc/systemd/system").join(unit);
        let body = std::fs::read_to_string(&staged).unwrap_or_else(|_| panic!("{unit} not staged"));
        if unit.ends_with(".timer") {
            assert!(body.contains("Persistent=true"), "{unit}");
        } else {
            assert!(body.contains("EnvironmentFile="), "{unit}");
            assert!(!body.contains("plaintext"), "{unit}");
        }
    }
}

/// A snapshot whose artifact no longer matches its manifest hash is refused
/// before anything is created.
#[tokio::test]
async fn a_tampered_snapshot_is_refused_before_any_target_exists() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let snap = backup(&s, dir.path()).await;
    let art = snap.join("git.tar.age");
    let mut bytes = std::fs::read(&art).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&art, &bytes).unwrap();
    let t = new_target();
    let out = restore(&snap, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(67), "{}", text(&out));
    assert!(text(&out).contains("sha256"), "{}", text(&out));
    assert!(!t.repos.exists(), "targets created for a tampered snapshot");
}

// ---------------------------------------------------------------------------
// Lifecycle unit 4, review fixes
// ---------------------------------------------------------------------------

/// The staged units can write everything their command touches under
/// `ProtectSystem=strict`: the backup root, the lifecycle authority (control
/// dir and the witness's dir) and, for the backup (which also purges what is
/// overdue), the repository root; the locks dir is staged, never optional.
#[tokio::test]
async fn the_staged_units_can_write_what_their_commands_touch() {
    let dir = tempfile::tempdir().unwrap();
    let stage = dir.path().join("stage");
    let env_file = dir.path().join("ops.env");
    std::fs::write(
        &env_file,
        "MUSTER_DATABASE_URL=postgres://muster@localhost/muster\n\
MUSTER_REPO_ROOT=/var/lib/muster-cloud/repos\n\
MUSTER_CONTROL_DIR=/var/lib/muster-cloud/control\n\
MUSTER_CONTROL_WITNESS=/var/lib/muster-cloud-witness/witness.json\n\
MUSTER_BACKUP_ROOT=/var/backups/muster-cloud\n\
MUSTER_BACKUP_AGE_RECIPIENT=age1ql3z7hjy54pw3hyww5ayyfg7zqgvc7w3j2elw8zmrj2kg5sfn9aqmcac8p\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&env_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = run(
        &script("install-systemd.sh"),
        &[
            "--root",
            &stage.display().to_string(),
            "--env-file",
            &env_file.display().to_string(),
        ],
        &[],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let writable = |unit: &str| -> Vec<String> {
        let body = std::fs::read_to_string(stage.join("etc/systemd/system").join(unit)).unwrap();
        body.lines()
            .filter_map(|l| l.strip_prefix("ReadWritePaths="))
            .flat_map(|l| l.split_whitespace().map(str::to_string).collect::<Vec<_>>())
            .collect()
    };
    for unit in [
        "muster-cloud-backup.service",
        "muster-cloud-retention.service",
    ] {
        let w = writable(unit);
        assert!(
            !w.iter().any(|p| p.starts_with('-')),
            "{unit}: optional path {w:?}"
        );
        for need in [
            "/var/backups/muster-cloud",
            "/var/lib/muster-cloud/control",
            "/var/lib/muster-cloud-witness",
        ] {
            assert!(
                w.iter().any(|p| p == need),
                "{unit}: {need} not writable: {w:?}"
            );
        }
    }
    let w = writable("muster-cloud-backup.service");
    assert!(
        w.iter().any(|p| p == "/var/lib/muster-cloud/repos"),
        "{w:?}"
    );
    let locks = stage.join("var/lib/muster-cloud/repos/locks");
    assert!(locks.is_dir(), "locks dir not staged");
}

/// A push accepted between the capture's file listing and its archive
/// (objects first, then refs) still yields an archive whose refs all have
/// their objects: the restore passes fsck and serves.
#[tokio::test]
async fn a_push_during_the_git_capture_still_restores_clean() {
    let s = support::stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let key = MachineKey::new("a-node");
    let ws = create_workspace(&s, &alice, "Busy").await;
    let dev = link(&s, &token, &ws.id, &key).await;
    let repo = FleetRepo::genesis(&key);
    let head = repo.head();
    assert_eq!(
        dev.send(&s, &dev.push(repo.bundle(None), None))
            .await
            .status(),
        200
    );

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let pause = dir.path().join("go");
    let task = paused_backup_at(&s.env, &root, &pause, "git_listed").await;
    repo.touch("during-capture.txt", "new objects, then refs");
    let resp = dev
        .send(&s, &dev.push(repo.bundle(Some(&head)), Some(&head)))
        .await;
    assert_eq!(resp.status(), 200);
    std::fs::write(&pause, b"").unwrap();
    let snap = published(&task.await.unwrap());

    let t = new_target();
    let out = restore(&snap, &t, Some(&s.env)).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_restored_serves(&s, &t).await;
}

/// `backup run` purges what is overdue before its snapshot: a workspace
/// past its backup cap never enters a new backup. It refuses (65) only
/// while such a workspace still cannot be purged (here a live reader blocks
/// the drain), and succeeds on its own afterwards.
#[tokio::test]
async fn a_backup_reapplies_the_journal_and_purges_what_is_overdue_first() {
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "1".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (ka, kb) = (MachineKey::new("a-node"), MachineKey::new("b-node"));
    let (a, _) = pushed(&s, &alice, &token, "Gone", &ka).await;
    let (b, _) = pushed(&s, &alice, &token, "Kept", &kb).await;
    let repos = PathBuf::from(value(&s.env, "MUSTER_REPO_ROOT"));
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("after");

    // A live reader on the repository: the purge cannot drain.
    let locks = repos.join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let held = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(locks.join(format!("{a}.lock")))
        .unwrap();
    held.lock_shared().unwrap();
    delete_ws(&s, &alice, &a, "Gone").await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(40);
    loop {
        let out = backup_run(&s.env, &root, &[], &[]).await;
        if out.status.code() == Some(65) {
            assert!(text(&out).contains("past its backup cap"), "{}", text(&out));
            break;
        }
        assert!(out.status.success(), "{}", text(&out));
        assert!(std::time::Instant::now() < deadline, "never past its cap");
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    drop(held);

    let snap = published(&backup_run(&s.env, &root, &[], &[]).await);
    assert_eq!(tenant_ids(&manifest(&snap)), vec![b.clone()]);
    assert!(
        !repos.join(format!("{a}.git")).exists(),
        "deleted tenant left in the root"
    );
}

/// A restore whose reconciliation cannot erase a tenant the journal
/// tombstoned (here not purged yet: the purge stopped right after its
/// tombstone) fails closed (73) and tears its targets down; the fault hook
/// that forces the erase failure is refused in production.
#[tokio::test]
async fn a_restore_that_cannot_erase_a_tombstoned_tenant_fails_closed() {
    let s = support::stack_with_env(&[
        ("MUSTER_DEV_DELETION_GRACE_SECS", "1".into()),
        ("MUSTER_DEV_PURGE_CRASH", "after_tombstone".into()),
    ])
    .await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let (a, _) = pushed(&s, &alice, &token, "Gone", &MachineKey::new("a-node")).await;
    let dir = tempfile::tempdir().unwrap();
    let before = backup(&s, dir.path()).await;
    delete_ws(&s, &alice, &a, "Gone").await;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let out = run_ops(&s.env, &["lifecycle-report", "--workspace", &a]).await;
        if out.status.success()
            && serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["lifecycle"]
                ["primary"]
                == "purge_committed"
        {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "never tombstoned");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    let t = new_target();
    let out = restore_with(
        &before,
        &t,
        &s.env,
        &[
            ("MUSTER_ENV", "development".into()),
            ("MUSTER_DEV_RESTORE_ERASE_FAIL", "1".into()),
        ],
    )
    .await;
    assert_eq!(out.status.code(), Some(73), "{}", text(&out));
    assert!(!t.repos.exists(), "targets left behind");

    let prod = with(
        &with(&target_env(&s, &t), "MUSTER_ENV", "production"),
        "MUSTER_DEV_RESTORE_ERASE_FAIL",
        "1",
    );
    let out = run_ops(&prod, &["restore-finalize"]).await;
    assert_eq!(out.status.code(), Some(78), "{}", text(&out));
    assert!(
        text(&out).contains("MUSTER_DEV_RESTORE_ERASE_FAIL"),
        "{}",
        text(&out)
    );
}

/// Every snapshot is verified unless the operator explicitly asks for the
/// legacy format: a directory whose manifest claims the legacy format (or
/// anything unverifiable) is refused before any target exists.
#[tokio::test]
async fn a_restore_verifies_every_snapshot_unless_legacy_is_explicit() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let first = backup(&s, dir.path()).await;
    let second = backup(&s, dir.path()).await;
    std::fs::copy(second.join("db.dump.age"), first.join("db.dump.age")).unwrap();
    std::fs::write(
        first.join("manifest.json"),
        br#"{"format_version": 1, "encrypted": true}"#,
    )
    .unwrap();
    let t = new_target();
    let out = restore(&first, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(67), "{}", text(&out));
    assert!(
        !t.repos.exists(),
        "targets created for an unverified snapshot"
    );
}

/// Unusable control paths are a usage error (64), whatever the snapshot.
#[tokio::test]
async fn restore_reports_unusable_control_paths_as_usage() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let snap = backup(&s, dir.path()).await;
    let mut t = new_target();
    // The new repository root inside the control dir: never disjoint.
    t.repos = PathBuf::from(value(&s.env, "MUSTER_CONTROL_DIR")).join("restored");
    let out = restore(&snap, &t, Some(&s.env)).await;
    assert_eq!(out.status.code(), Some(64), "{}", text(&out));
    assert!(!t.repos.exists());
}

/// A cleanup that runs while another operation is registering never takes
/// its lease for dead: the lease is held before it is visible.
#[tokio::test]
async fn a_lease_is_never_seen_unheld_by_a_concurrent_cleanup() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let pause = dir.path().join("go");
    let task = paused_backup_at(&s.env, &root, &pause, "lease_visible").await;
    let out = run_ops(
        &with(&s.env, "MUSTER_BACKUP_ROOT", &root.display().to_string()),
        &["retention", "run"],
    )
    .await;
    assert!(out.status.success(), "{}", text(&out));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["dead_operations_cleaned"], serde_json::json!([]), "{r}");
    std::fs::write(&pause, b"").unwrap();
    published(&task.await.unwrap());
}

/// A catalogued snapshot that stops validating is uncatalogued, not
/// removed: `purged_at` is only ever the time its files actually went away.
#[tokio::test]
async fn a_corrupt_snapshot_is_not_reported_removed_while_its_files_remain() {
    let s = support::stack_with_env(&[("MUSTER_DEV_DELETION_GRACE_SECS", "600".into())]).await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let token = account_token(&s, &alice).await;
    let ka = MachineKey::new("a-node");
    let (a, _) = pushed(&s, &alice, &token, "Doomed", &ka).await;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("backups");
    let ops_env = with(&s.env, "MUSTER_BACKUP_ROOT", &root.display().to_string());
    let snap = published(&backup_run(&s.env, &root, &[], &[]).await);
    delete_ws(&s, &alice, &a, "Doomed").await;

    let art = snap.join("git.tar.age");
    let len = std::fs::metadata(&art).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&art)
        .unwrap()
        .set_len(len - 1)
        .unwrap();
    let out = run_ops(&ops_env, &["retention", "run"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let v = lifecycle(&s, &alice, &a).await;
    assert_eq!(v["deletion"]["backups"]["status"], "unknown", "{v}");

    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    let removed = time::OffsetDateTime::now_utc();
    std::fs::remove_dir_all(&snap).unwrap();
    let out = run_ops(&ops_env, &["retention", "run"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let v = lifecycle(&s, &alice, &a).await;
    let bk = &v["deletion"]["backups"];
    assert_eq!(bk["status"], "purged", "{v}");
    let purged_at = time::OffsetDateTime::parse(
        bk["purged_at"].as_str().unwrap(),
        &time::format_description::well_known::Rfc3339,
    )
    .unwrap();
    assert!(
        purged_at >= removed,
        "purged_at {purged_at} before the files went away ({removed})"
    );
}

// ---------------------------------------------------------------------------
// Lifecycle unit 5: restore reconciliation and fencing
// ---------------------------------------------------------------------------

async fn restore_with(bk: &Path, t: &Target, authority: &Env, extra: &[(&str, String)]) -> Output {
    let mut args = vec![
        "--backup".to_string(),
        bk.display().to_string(),
        "--admin-url".into(),
        test_pg_url(),
        "--new-db".into(),
        t.db.clone(),
        "--new-repo-root".into(),
        t.repos.display().to_string(),
        "--control-dir".into(),
        value(authority, "MUSTER_CONTROL_DIR"),
        "--control-witness".into(),
        value(authority, "MUSTER_CONTROL_WITNESS"),
    ];
    let root = bk.parent().and_then(Path::parent).map(Path::to_path_buf);
    if let Some(key) = root.map(|r| identity_for(&r)).filter(|k| k.exists()) {
        args.extend(["--age-identity-file".into(), key.display().to_string()]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(&script("pg-restore.sh"), &args, extra).await
}

/// A restore joins the authority as a NEW candidate incarnation (never the
/// source's identity), and no service serves it until it is activated; the
/// source stays active and ready meanwhile.
#[tokio::test]
async fn a_restore_is_a_new_candidate_that_never_serves_until_activated() {
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, dir.path()).await;
    let (source, role) = incarnation(&s.env, &value(&s.env, "MUSTER_DATABASE_URL")).await;
    assert_eq!(role, "active");
    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert!(out.status.success(), "{}", text(&out));
    let (restored, role) = incarnation(&s.env, &t.url).await;
    assert_ne!(
        restored, source,
        "the restore kept the source's incarnation"
    );
    assert_eq!(role, "candidate");
    assert_eq!(refused_start(&target_env(&s, &t)).await, Some(78));
    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200, "the source stopped serving");
}

/// Activating a restore fences the old incarnation: its running service
/// refuses every request (reads and writes, not only readiness); its
/// database can never be activated again; operator backups refuse it.
#[tokio::test]
async fn activating_a_restore_fences_the_old_instance() {
    let s = seeded_stack().await;
    let alice = s.login_user("alice", "alice@example.com").await;
    let dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, dir.path()).await;
    let (source, _) = incarnation(&s.env, &value(&s.env, "MUSTER_DATABASE_URL")).await;
    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert!(out.status.success(), "{}", text(&out));
    let (cloud, _) = cutover(&s, &t).await;
    assert_eq!(
        http()
            .get(format!("{}/readyz", cloud.base))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // The old instance, still running: every request refused.
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        503
    );
    let write = http()
        .post(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .header("Origin", s.origin())
        .header("X-CSRF-Token", &alice.csrf_token)
        .header("Idempotency-Key", uuid::Uuid::new_v4().to_string())
        .json(&serde_json::json!({"name": "After the cutover"}))
        .send()
        .await
        .unwrap();
    assert_eq!(write.status(), 503, "the fenced instance accepted a write");
    let read = http()
        .get(s.url("/v1/workspaces"))
        .header("Cookie", format!("__Host-mc_session={}", alice.cookie))
        .send()
        .await
        .unwrap();
    assert_eq!(read.status(), 503, "the fenced instance served a read");

    // Never active again, and no operator backup of it.
    let src_url = value(&s.env, "MUSTER_DATABASE_URL");
    let again = run_ops(
        &s.env,
        &[
            "activate",
            "--incarnation",
            &source,
            "--database-url",
            &src_url,
        ],
    )
    .await;
    assert_eq!(again.status.code(), Some(65), "{}", text(&again));
    let refused = backup_run(&s.env, &dir.path().join("fenced"), &[], &[]).await;
    assert_eq!(refused.status.code(), Some(65), "{}", text(&refused));
}

static ONE_RESTORE_CRASH_AT_A_TIME: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A restore killed while joining the authority (after the journal, after
/// the witness) tears its targets down and leaves the source active; the
/// next restore succeeds.
#[tokio::test]
async fn a_restore_killed_while_joining_leaves_the_source_active() {
    let _one = ONE_RESTORE_CRASH_AT_A_TIME.lock().await;
    let s = seeded_stack().await;
    let dir = tempfile::tempdir().unwrap();
    let bk = backup(&s, dir.path()).await;
    for point in ["after_journal", "after_witness"] {
        let t = new_target();
        let out = restore_with(
            &bk,
            &t,
            &s.env,
            &[
                ("MUSTER_ENV", "development".into()),
                ("MUSTER_DEV_CONTROL_CRASH", point.into()),
            ],
        )
        .await;
        assert_eq!(out.status.code(), Some(73), "{point}: {}", text(&out));
        assert!(!t.repos.exists(), "{point}: targets left behind");
        assert_eq!(
            http().get(s.url("/readyz")).send().await.unwrap().status(),
            200,
            "{point}: the source stopped serving"
        );
    }
    let t = new_target();
    let out = restore(&bk, &t, Some(&s.env)).await;
    assert!(out.status.success(), "{}", text(&out));
    assert_restored_serves(&s, &t).await;
}
