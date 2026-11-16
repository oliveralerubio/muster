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

/// A backup of the stack's database and repository root.
async fn backup(s: &support::Stack, out_dir: &Path) -> PathBuf {
    let out = run(
        &script("pg-backup.sh"),
        &[
            "--database-url",
            &value(&s.env, "MUSTER_DATABASE_URL"),
            "--repo-root",
            &value(&s.env, "MUSTER_REPO_ROOT"),
            "--output-dir",
            &out_dir.display().to_string(),
            "--plaintext",
        ],
        &[],
    )
    .await;
    assert!(out.status.success(), "backup: {}", text(&out));
    std::fs::read_dir(out_dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("muster-backup-")
        })
        .expect("backup dir")
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

/// The restored targets pass the ops gate and a real service on them is
/// ready under the same (unchanged) authority.
async fn assert_restored_serves(s: &support::Stack, t: &Target) {
    let gate = run_ops(&s.env, &["restore-check", "--database-url", &t.url]).await;
    assert!(gate.status.success(), "{}", text(&gate));
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let env = with(
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
    );
    let cloud = spawn_cloud(&env).await;
    let ready = http()
        .get(format!("{}/readyz", cloud.base))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), 200);
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
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let env = with(
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
    );
    let cloud = spawn_cloud(&env).await;
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
    assert!(
        t.repos.join(format!("{}.git", a.id)).exists(),
        "the backup does hold A"
    );
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let env = with(
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
    );
    let cloud = spawn_cloud(&env).await;
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

/// Review F5: on a database restored from before the request, the
/// operator commands reapply the external journal themselves (no service
/// started): `lifecycle-report` reports what the journal says (purged)
/// with the restored residue, and `purge-due` erases it.
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
    let before = report(env.clone()).await;
    assert_eq!(before["lifecycle"]["primary"], "purged", "{before}");
    assert_eq!(before["residue"], true, "{before}");

    let out = run_ops(&env, &["purge-due"]).await;
    assert!(out.status.success(), "{}", text(&out));
    let ran: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        ran["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["workspace_id"] == a.id.as_str() && w["state"] == "purged"),
        "{ran}"
    );
    let after = report(env.clone()).await;
    assert_eq!(after["residue"], false, "{after}");
    assert!(!t.repos.join(format!("{}.git", a.id)).exists());
}
