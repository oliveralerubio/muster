//! Lifecycle authority (control journal + independent witness + database
//! binding), observed only through the real `muster-cloud-ops` binary and
//! the real service (startup exit code, `/readyz`). Control files are
//! touched only the way an operator or a failure would: deleted or replaced
//! whole, never parsed by the test.
mod support;

use std::path::Path;

use support::{fresh_database, http, run_cloud_expecting_exit, run_ops, stack, Env};

fn json(out: &std::process::Output) -> serde_json::Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "ops stdout is not JSON ({e}); stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn code(out: &std::process::Output) -> Option<i32> {
    out.status.code()
}

fn value(env: &Env, name: &str) -> String {
    env.iter()
        .find(|(k, _)| *k == name)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| panic!("{name} not in env"))
}

/// Operator environment for `muster-cloud-ops` on fresh, private paths.
fn ops_env(root: &Path, database_url: &str) -> Env {
    let repos = root.join("repos");
    let witness = root.join("witness");
    std::fs::create_dir_all(&repos).unwrap();
    std::fs::create_dir_all(&witness).unwrap();
    vec![
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_REPO_ROOT", repos.display().to_string()),
        ("MUSTER_DATABASE_URL", database_url.into()),
        (
            "MUSTER_CONTROL_DIR",
            root.join("control").display().to_string(),
        ),
        (
            "MUSTER_CONTROL_WITNESS",
            witness.join("witness.json").display().to_string(),
        ),
    ]
}

fn with(env: &Env, name: &'static str, v: &str) -> Env {
    let mut e: Env = env.iter().filter(|(k, _)| *k != name).cloned().collect();
    e.push((name, v.into()));
    e
}

#[tokio::test]
async fn what_ops_init_writes_is_read_back_by_ops_and_the_service() {
    let mut s = stack().await;
    let db = value(&s.env, "MUSTER_DATABASE_URL");

    let out = run_ops(&s.env, &["status", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let status = json(&out);
    assert_eq!(status["v"], 1);
    assert_eq!(status["sequence"], 2);
    assert_eq!(status["db"]["role"], "active");
    assert_eq!(status["db"]["applied_sequence"], 2);
    assert_eq!(status["active_incarnation"], status["db"]["incarnation_id"]);

    let out = run_ops(&s.env, &["restore-check", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200);
    // A fresh process reads the same journal and witness at startup.
    s.restart().await;
    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 200);
}

#[tokio::test]
async fn a_missing_witness_refuses_readiness_startup_status_and_restore() {
    let mut s = stack().await;
    let db = value(&s.env, "MUSTER_DATABASE_URL");
    std::fs::remove_file(value(&s.env, "MUSTER_CONTROL_WITNESS")).unwrap();

    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 503);
    let body: serde_json::Value = ready.json().await.unwrap();
    assert!(
        body.to_string().contains("control witness missing"),
        "{body}"
    );

    let out = run_ops(&s.env, &["status", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run_ops(&s.env, &["restore-check", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    s.cloud.kill().await;
    let (exit, text) = run_cloud_expecting_exit(&s.env).await;
    assert_eq!(exit, Some(78), "{text}");
    assert!(text.contains("control witness missing"), "{text}");
}

#[tokio::test]
async fn a_missing_journal_refuses_readiness_and_startup() {
    let mut s = stack().await;
    let db = value(&s.env, "MUSTER_DATABASE_URL");
    std::fs::remove_dir_all(value(&s.env, "MUSTER_CONTROL_DIR")).unwrap();

    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 503);
    let out = run_ops(&s.env, &["restore-check", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    s.cloud.kill().await;
    let (exit, text) = run_cloud_expecting_exit(&s.env).await;
    assert_eq!(exit, Some(78), "{text}");
}

#[tokio::test]
async fn init_is_idempotent_while_pristine_and_refuses_targets_with_data() {
    let s = stack().await;
    let db = value(&s.env, "MUSTER_DATABASE_URL");
    let before = json(&run_ops(&s.env, &["status", "--database-url", &db]).await);
    // Same complete authority, still no tenants: a no-op.
    let out = run_ops(&s.env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let after = json(&run_ops(&s.env, &["status", "--database-url", &db]).await);
    assert_eq!(before["active_incarnation"], after["active_incarnation"]);
    assert_eq!(before["deployment_id"], after["deployment_id"]);
    assert_eq!(after["sequence"], 2);

    // The bound database with brand-new control paths: refused.
    let root = tempfile::tempdir().unwrap();
    let env = ops_env(root.path(), &db);
    let out = run_ops(&env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(66),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!Path::new(&value(&env, "MUSTER_CONTROL_WITNESS")).exists());

    // Once an account exists, even the same targets are refused.
    s.login_user("owner", "owner@example.com").await;
    let out = run_ops(&s.env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(66),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // A fresh database, but the repository root already holds a repository.
    let root = tempfile::tempdir().unwrap();
    let env = ops_env(root.path(), &fresh_database().await);
    std::fs::create_dir(Path::new(&value(&env, "MUSTER_REPO_ROOT")).join("w.git")).unwrap();
    let out = run_ops(&env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(66),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!Path::new(&value(&env, "MUSTER_CONTROL_WITNESS")).exists());
}

async fn active(env: &Env, database_url: &str) -> bool {
    let out = run_ops(env, &["status", "--database-url", database_url]).await;
    out.status.success() && json(&out)["db"]["role"] == "active"
}

/// An init interrupted at any point (`after_journal`: journal and database
/// binding written, no witness; `after_witness`: everything written, no
/// answer) finishes on its own database when nobody interferes, and never
/// leaves two active databases or a second copy of an authority elsewhere.
#[tokio::test]
async fn an_interrupted_init_never_yields_two_active_databases() {
    for point in ["after_journal", "after_witness"] {
        // Plain resume on the same targets.
        let root = tempfile::tempdir().unwrap();
        let a = fresh_database().await;
        let env_a = ops_env(root.path(), &a);
        let out = run_ops(&with(&env_a, "MUSTER_DEV_CONTROL_CRASH", point), &["init"]).await;
        assert_ne!(
            code(&out),
            Some(0),
            "{point}: the crash point must interrupt init"
        );
        assert!(
            !active(&env_a, &a).await || point == "after_witness",
            "{point}"
        );
        let out = run_ops(&env_a, &["init"]).await;
        assert_eq!(
            code(&out),
            Some(0),
            "{point}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(active(&env_a, &a).await, "{point}");

        // Interference: another pristine database on the same control
        // paths, and the interrupted database on brand-new control paths.
        let root = tempfile::tempdir().unwrap();
        let a = fresh_database().await;
        let env_a = ops_env(root.path(), &a);
        let _ = run_ops(&with(&env_a, "MUSTER_DEV_CONTROL_CRASH", point), &["init"]).await;
        let b = fresh_database().await;
        let env_b = with(&env_a, "MUSTER_DATABASE_URL", &b);
        let other_root = tempfile::tempdir().unwrap();
        let env_a_elsewhere = ops_env(other_root.path(), &a);
        // Before the witness nothing was ever served, so the other database
        // may take a fresh authority there; after it, it is refused.
        let out = run_ops(&env_b, &["init"]).await;
        let expected = if point == "after_witness" {
            vec![Some(66)]
        } else {
            vec![Some(0), Some(66)]
        };
        assert!(
            expected.contains(&code(&out)),
            "{point}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let out = run_ops(&env_a_elsewhere, &["init"]).await;
        assert_eq!(
            code(&out),
            Some(66),
            "{point}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let _ = run_ops(&env_a, &["init"]).await;
        let actives = [
            active(&env_a, &a).await,
            active(&env_b, &b).await,
            active(&env_a_elsewhere, &a).await,
        ];
        assert_eq!(
            actives.iter().filter(|a| **a).count(),
            1,
            "{point}: {actives:?}"
        );
        assert!(!actives[2], "{point}: a second copy of an authority");
        if point == "after_witness" {
            assert!(
                actives[0],
                "{point}: the witnessed database keeps its authority"
            );
        }
    }
}

#[tokio::test]
async fn development_crash_points_are_refused_in_production() {
    let root = tempfile::tempdir().unwrap();
    let env = with(
        &with(
            &ops_env(root.path(), &fresh_database().await),
            "MUSTER_ENV",
            "production",
        ),
        "MUSTER_DEV_CONTROL_CRASH",
        "after_journal",
    );
    let out = run_ops(&env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(78),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!Path::new(&value(&env, "MUSTER_CONTROL_WITNESS")).exists());
}

// ---- Phase B: incarnations, crash recovery, stale/divergent/foreign/ahead ----

/// A copy of the whole control store (journal directory + witness) on new
/// private paths, as a backup or a stale disk would hold it.
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

/// Replaces the journal directory of `to` with the one of `from` (a
/// rolled-back or swapped disk), keeping `to`'s witness.
fn swap_journal(from: &Env, to: &Env) {
    let target = value(to, "MUSTER_CONTROL_DIR");
    std::fs::remove_dir_all(&target).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    for entry in std::fs::read_dir(value(from, "MUSTER_CONTROL_DIR")).unwrap() {
        let entry = entry.unwrap();
        std::fs::copy(entry.path(), Path::new(&target).join(entry.file_name())).unwrap();
    }
}

async fn ops_ok(env: &Env, args: &[&str]) -> serde_json::Value {
    let out = run_ops(env, args).await;
    assert_eq!(
        code(&out),
        Some(0),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    json(&out)
}

/// Registers a new database as a candidate incarnation; returns (url, id).
async fn register(env: &Env) -> (String, String) {
    let db = fresh_database().await;
    let status = ops_ok(env, &["register", "--database-url", &db]).await;
    assert_eq!(status["db"]["role"], "candidate");
    (
        db,
        status["db"]["incarnation_id"].as_str().unwrap().to_string(),
    )
}

/// Every gate refuses this authority/database pair: readiness of the
/// running service when it runs on exactly this pair, service startup, ops
/// status and restore-check.
async fn assert_refused_everywhere(
    s: &mut support::Stack,
    env: &Env,
    db: &str,
    what: &str,
    reason: &str,
) {
    let running_on_pair = value(&s.env, "MUSTER_CONTROL_DIR") == value(env, "MUSTER_CONTROL_DIR")
        && value(&s.env, "MUSTER_DATABASE_URL") == db;
    if running_on_pair {
        let ready = http().get(s.url("/readyz")).send().await.unwrap();
        assert_eq!(ready.status(), 503, "{what}: readyz");
    }
    for cmd in ["status", "restore-check"] {
        let out = run_ops(env, &[cmd, "--database-url", db]).await;
        assert_eq!(
            code(&out),
            Some(65),
            "{what}: {cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(reason), "{what}: {cmd}: {stderr}");
    }
    s.cloud.kill().await;
    let (exit, text) = run_cloud_expecting_exit(&with(env, "MUSTER_DATABASE_URL", db)).await;
    assert_eq!(exit, Some(78), "{what}: startup: {text}");
    assert!(
        text.contains("lifecycle authority refused") && text.contains(reason),
        "{what}: {text}"
    );
}

#[tokio::test]
async fn activating_a_new_incarnation_fences_the_old_one() {
    let mut s = stack().await;
    let (b, b_id) = register(&s.env).await;
    // A candidate never serves.
    let out = run_ops(
        &with(&s.env, "MUSTER_DATABASE_URL", &b),
        &["status", "--database-url", &b],
    )
    .await;
    assert_eq!(json(&out)["db"]["role"], "candidate");
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );

    let status = ops_ok(
        &s.env,
        &["activate", "--incarnation", &b_id, "--database-url", &b],
    )
    .await;
    assert_eq!(status["db"]["role"], "active");
    assert_eq!(status["active_incarnation"], b_id.as_str());

    // The old incarnation is fenced: not ready, and refused at startup.
    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 503);
    let body: serde_json::Value = ready.json().await.unwrap();
    assert!(body.to_string().contains("fenced"), "{body}");
    s.cloud.kill().await;
    let (exit, text) = run_cloud_expecting_exit(&s.env).await;
    assert_eq!(exit, Some(78), "{text}");

    // The new one serves.
    s.restart_with_env(&[("MUSTER_DATABASE_URL", b)]).await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

/// Crash after the activation reached the journal but before the witness:
/// the activation was never acknowledged, so it never takes effect. The
/// next reader discards it (the source keeps serving, nobody is left
/// without an active instance) and rerunning `activate` completes the
/// cutover.
#[tokio::test]
async fn an_activation_interrupted_before_the_witness_never_fences_the_source() {
    let mut s = stack().await;
    let (b, b_id) = register(&s.env).await;
    let out = run_ops(
        &with(&s.env, "MUSTER_DEV_CONTROL_CRASH", "after_journal"),
        &["activate", "--incarnation", &b_id, "--database-url", &b],
    )
    .await;
    assert_ne!(
        code(&out),
        Some(0),
        "the crash point must interrupt activate"
    );
    // The running source is never fenced by the unacknowledged record
    // (its requests read the witnessed prefix only), and the next open
    // discards it.
    assert_ne!(
        http().get(s.url("/v1/me")).send().await.unwrap().status(),
        503
    );
    let status = ops_ok(&s.env, &["status"]).await;
    assert_eq!(status["sequence"], 3, "{status}");
    assert_ne!(status["active_incarnation"], b_id.as_str(), "{status}");
    assert_eq!(status["discarded_activation"], b_id.as_str(), "{status}");
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );

    // Rerunning the activation completes the cutover.
    let done = ops_ok(
        &s.env,
        &["activate", "--incarnation", &b_id, "--database-url", &b],
    )
    .await;
    assert_eq!(done["db"]["role"], "active", "{done}");
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        503
    );
    s.restart_with_env(&[("MUSTER_DATABASE_URL", b.clone())])
        .await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

/// Crash after journal and witness, before the database projection: the
/// projection is behind and is replayed before readiness.
#[tokio::test]
async fn a_projection_behind_after_a_crash_is_replayed_before_readiness() {
    let mut s = stack().await;
    let (b, b_id) = register(&s.env).await;
    let out = run_ops(
        &with(&s.env, "MUSTER_DEV_CONTROL_CRASH", "after_witness"),
        &["activate", "--incarnation", &b_id, "--database-url", &b],
    )
    .await;
    assert_ne!(
        code(&out),
        Some(0),
        "the crash point must interrupt activate"
    );
    let status = ops_ok(&s.env, &["status"]).await;
    assert_eq!(status["witness_advanced_from"], serde_json::Value::Null);
    assert_eq!(status["sequence"], 4);

    let check = ops_ok(&s.env, &["restore-check", "--database-url", &b]).await;
    assert_eq!(
        check["db"]["applied_sequence"], 3,
        "restore-check never writes"
    );
    let replay = ops_ok(&s.env, &["status", "--database-url", &b]).await;
    assert_eq!(replay["db"]["replayed"], 1);
    assert_eq!(replay["db"]["applied_sequence"], 4);

    // The same happens inside the service: A replays the activation into
    // its own projection on the next probe and learns it is fenced.
    let ready = http().get(s.url("/readyz")).send().await.unwrap();
    assert_eq!(ready.status(), 503);
    s.restart_with_env(&[("MUSTER_DATABASE_URL", b)]).await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

#[tokio::test]
async fn a_rolled_back_journal_is_refused_everywhere() {
    let mut s = stack().await;
    let (_old_root, old) = copy_authority(&s.env);
    register(&s.env).await; // witness now at 3
    swap_journal(&old, &s.env); // journal back at 2
    let env = s.env.clone();
    let db = value(&env, "MUSTER_DATABASE_URL");
    assert_refused_everywhere(
        &mut s,
        &env,
        &db,
        "rolled back",
        "journal is behind its witness",
    )
    .await;
}

#[tokio::test]
async fn a_divergent_journal_is_refused_everywhere() {
    let mut s = stack().await;
    let (fork_root, fork) = copy_authority(&s.env);
    register(&s.env).await; // record 3 here...
    register(&fork).await; // ...and a different record 3 in the fork
    swap_journal(&fork, &s.env);
    let env = s.env.clone();
    let db = value(&env, "MUSTER_DATABASE_URL");
    assert_refused_everywhere(
        &mut s,
        &env,
        &db,
        "divergent",
        "journal diverges from its witness",
    )
    .await;
    drop(fork_root);
}

#[tokio::test]
async fn a_database_of_another_deployment_is_refused_everywhere() {
    let mut s = stack().await;
    let other = stack().await;
    let foreign = value(&other.env, "MUSTER_DATABASE_URL");
    let env = s.env.clone();
    assert_refused_everywhere(
        &mut s,
        &env,
        &foreign,
        "foreign",
        "belongs to another deployment",
    )
    .await;
}

#[tokio::test]
async fn a_database_ahead_of_the_journal_is_refused_everywhere() {
    let mut s = stack().await;
    let (old_root, old) = copy_authority(&s.env); // control store at 2
    register(&s.env).await;
    // A's projection catches up to 3 on its next probe.
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
    let db = value(&s.env, "MUSTER_DATABASE_URL");
    assert_eq!(
        ops_ok(&s.env, &["restore-check", "--database-url", &db]).await["db"]["applied_sequence"],
        3
    );
    // Pointed at the old control store, A is ahead of it.
    assert_refused_everywhere(
        &mut s,
        &old,
        &db,
        "database ahead",
        "ahead of the control journal",
    )
    .await;
    drop(old_root);
}

#[tokio::test]
async fn a_missing_or_foreign_witness_is_refused_everywhere() {
    let mut s = stack().await;
    let other = stack().await;
    std::fs::copy(
        value(&other.env, "MUSTER_CONTROL_WITNESS"),
        value(&s.env, "MUSTER_CONTROL_WITNESS"),
    )
    .unwrap();
    let env = s.env.clone();
    let db = value(&env, "MUSTER_DATABASE_URL");
    assert_refused_everywhere(
        &mut s,
        &env,
        &db,
        "foreign witness",
        "witness and journal belong to different deployments",
    )
    .await;
}

/// Same deployment, same sequence, different content: the database was
/// projected from a fork of the control store (e.g. a restored copy that
/// kept being written).
#[tokio::test]
async fn a_database_projected_from_a_forked_store_is_refused_everywhere() {
    let mut s = stack().await;
    let (_fork_root, fork) = copy_authority(&s.env);
    let (b, _) = register(&s.env).await; // B projected at record 3 here
    register(&fork).await; // a different record 3 in the fork
    assert_refused_everywhere(
        &mut s,
        &fork,
        &b,
        "forked projection",
        "projection diverges",
    )
    .await;
}

/// A write cut off mid-line (power loss during an append) was never
/// acknowledged: the torn bytes are dropped and the authority serves on.
#[tokio::test]
async fn a_torn_journal_tail_is_truncated_and_service_continues() {
    const TORN: &[u8] = b"{\"v\":1,\"seq\":3,\"pre";
    let mut s = stack().await;
    let journal = std::fs::read_dir(value(&s.env, "MUSTER_CONTROL_DIR"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .expect("journal file");
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&journal)
        .unwrap()
        .write_all(TORN)
        .unwrap();
    let status = ops_ok(&s.env, &["status"]).await;
    assert_eq!(status["truncated_torn_bytes"], TORN.len());
    assert_eq!(status["sequence"], 2);
    assert_eq!(ops_ok(&s.env, &["status"]).await["truncated_torn_bytes"], 0);
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
    s.restart().await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

/// Liveness: the authority's cross-process file lock must never stall the
/// service's async runtime. With a single worker thread, concurrent
/// readiness probes (each holding the lock while it waits on PostgreSQL)
/// must all answer, and the service must stay responsive.
#[tokio::test]
async fn concurrent_readiness_probes_never_stall_a_single_worker_runtime() {
    let s = support::stack_with_env(&[("TOKIO_WORKER_THREADS", "1".into())]).await;
    let client = http();
    let probes = (0..32).map(|_| {
        let client = client.clone();
        let url = s.url("/readyz");
        async move {
            tokio::time::timeout(std::time::Duration::from_secs(15), client.get(url).send())
                .await
                .map(|r| r.map(|r| r.status().as_u16()).ok())
        }
    });
    let results = futures::future::join_all(probes).await;
    let stalled = results.iter().filter(|r| r.is_err()).count();
    assert_eq!(stalled, 0, "probes stalled past 15s: {results:?}");
    assert!(
        results.iter().all(|r| matches!(r, Ok(Some(200)))),
        "{results:?}"
    );
    let health = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.get(s.url("/healthz")).send(),
    )
    .await;
    assert!(
        matches!(health, Ok(Ok(ref r)) if r.status() == 200),
        "{health:?}"
    );
}

/// Another process holding the control store (e.g. an ops command stuck on
/// its database) makes readiness answer "not ready" within a bounded time,
/// never hang; once released, the service is ready again.
#[tokio::test]
async fn a_control_store_held_by_another_process_bounds_readiness() {
    let s = stack().await;
    let lock = Path::new(&value(&s.env, "MUSTER_CONTROL_DIR")).join("control.lock");
    let mut holder = tokio::process::Command::new("flock")
        .args(["--exclusive", "--no-fork"])
        .arg(&lock)
        .args(["sleep", "60"])
        .kill_on_drop(true)
        .spawn()
        .expect("flock(1)");
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    let ready = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        http().get(s.url("/readyz")).send(),
    )
    .await
    .expect("readiness must answer while the control store is held")
    .unwrap();
    assert_eq!(ready.status(), 503);
    let body: serde_json::Value = ready.json().await.unwrap();
    assert!(body.to_string().contains("control store busy"), "{body}");
    assert!(started.elapsed() < std::time::Duration::from_secs(20));
    holder.kill().await.unwrap();
    let _ = holder.wait().await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

/// First init on a fresh deployment: control dir and witness directory do
/// not exist yet, and the control dir is reached through a symbolic link to
/// a disjoint location. Init creates them; the service is ready on them.
#[tokio::test]
async fn first_init_creates_missing_disjoint_leaves_even_through_a_symlink() {
    let root = tempfile::tempdir().unwrap();
    let base = root.path().canonicalize().unwrap();
    std::fs::create_dir_all(base.join("volumes/control-volume")).unwrap();
    std::os::unix::fs::symlink(base.join("volumes"), base.join("vol-link")).unwrap();
    let mut s = stack().await;
    let db = support::fresh_database().await;
    let repos = base.join("repos");
    std::fs::create_dir_all(&repos).unwrap();
    let env = with(
        &with(
            &with(
                &with(&s.env, "MUSTER_DATABASE_URL", &db),
                "MUSTER_REPO_ROOT",
                &repos.display().to_string(),
            ),
            "MUSTER_CONTROL_DIR",
            &base
                .join("vol-link/control-volume/new/control")
                .display()
                .to_string(),
        ),
        "MUSTER_CONTROL_WITNESS",
        &base
            .join("witness-volume/not-yet/witness.json")
            .display()
            .to_string(),
    );
    let out = run_ops(&env, &["init"]).await;
    assert_eq!(
        code(&out),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    s.cloud.kill().await;
    s.env = env;
    s.restart().await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    );
}

/// Fixture only: every new session on `database_url`'s database starts in
/// read-only transactions (as a read replica or a restored snapshot would
/// be served). Nothing is read back from the database by the test.
async fn make_database_read_only(database_url: &str) {
    use sqlx::Connection;
    let name = url::Url::parse(database_url)
        .unwrap()
        .path()
        .trim_start_matches('/')
        .to_string();
    let mut admin = sqlx::PgConnection::connect(&support::test_pg_url())
        .await
        .unwrap();
    sqlx::query(&format!(
        "ALTER DATABASE {name} SET default_transaction_read_only = on"
    ))
    .execute(&mut admin)
    .await
    .unwrap();
}

/// `restore-check` is a read-only gate: it must give its verdict on a
/// database that refuses every write (no migrations, no row locks), accept
/// a bound database of this authority, and refuse ahead, foreign and
/// uninitialised ones there too.
#[tokio::test]
async fn restore_check_gives_its_verdict_on_a_read_only_database() {
    let mut s = stack().await;
    let (_old_root, old) = copy_authority(&s.env); // control store at 2
    register(&s.env).await;
    assert_eq!(
        http().get(s.url("/readyz")).send().await.unwrap().status(),
        200
    ); // A at 3
    s.cloud.kill().await;
    let db = value(&s.env, "MUSTER_DATABASE_URL");
    make_database_read_only(&db).await;

    let out = run_ops(&s.env, &["restore-check", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(0),
        "bound: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(json(&out)["db"]["applied_sequence"], 3);

    let out = run_ops(&old, &["restore-check", "--database-url", &db]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "ahead: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("ahead of the control journal"));

    let other = stack().await;
    let foreign = value(&other.env, "MUSTER_DATABASE_URL");
    drop(other);
    make_database_read_only(&foreign).await;
    let out = run_ops(&s.env, &["restore-check", "--database-url", &foreign]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "foreign: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("another deployment"));

    let empty = support::fresh_database().await;
    make_database_read_only(&empty).await;
    let out = run_ops(&s.env, &["restore-check", "--database-url", &empty]).await;
    assert_eq!(
        code(&out),
        Some(65),
        "uninitialised: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("not bound to a lifecycle authority"));
}
