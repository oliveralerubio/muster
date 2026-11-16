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
