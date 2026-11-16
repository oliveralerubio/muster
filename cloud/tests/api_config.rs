mod support;

use support::{run_cloud_expecting_exit, run_ops};

#[tokio::test]
async fn production_refuses_plain_http_public_url() {
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_ENV", "production".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "http://example.com".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
        ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client_123".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "secret_123".into()),
    ])
    .await;
    assert_ne!(code, Some(0));
    assert!(
        output.contains("must use https:// in production"),
        "output: {output}"
    );
}

#[tokio::test]
async fn production_refuses_loopback_oidc_issuer() {
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_ENV", "production".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "https://app.example.com".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
        ("MUSTER_OIDC_ISSUER", "https://127.0.0.1:8443".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client_123".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "secret_123".into()),
    ])
    .await;
    assert_ne!(code, Some(0));
    assert!(
        output.contains("cannot be a loopback address in production"),
        "output: {output}"
    );
}

#[tokio::test]
async fn unset_environment_means_production_rules() {
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "http://app.example.com".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
        ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".into()),
    ])
    .await;
    assert_eq!(code, Some(78), "output: {output}");
    assert!(
        output.contains("must use https:// in production"),
        "output: {output}"
    );
}

#[tokio::test]
async fn startup_output_never_contains_database_password_or_client_secret() {
    // Valid production config whose database is unreachable: the service
    // logs its configuration, fails to connect and exits.
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_ENV", "production".into()),
        ("MUSTER_LOG", "info".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "https://app.example.com".into()),
        (
            "MUSTER_DATABASE_URL",
            "postgres://svc:synthetic-db-pw-7f3a@127.0.0.1:1/muster".into(),
        ),
        ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client".into()),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            "synthetic-oidc-secret-9c2e".into(),
        ),
        ("MUSTER_CONTROL_DIR", "/nonexistent/muster-control".into()),
        (
            "MUSTER_CONTROL_WITNESS",
            "/nonexistent/muster-witness/w.json".into(),
        ),
    ])
    .await;
    assert_eq!(code, Some(78), "output: {output}");
    assert!(
        output.contains("starting"),
        "config should be logged: {output}"
    );
    assert!(
        !output.contains("synthetic-db-pw-7f3a"),
        "password leaked: {output}"
    );
    assert!(
        !output.contains("synthetic-oidc-secret-9c2e"),
        "secret leaked: {output}"
    );
}

#[tokio::test]
async fn git_storage_root_is_required_and_absolute() {
    let base = |root: Option<&str>| {
        let mut env = vec![
            ("MUSTER_ENV", "development".to_string()),
            ("MUSTER_BIND", "127.0.0.1:0".to_string()),
            ("MUSTER_PUBLIC_URL", "http://127.0.0.1:1".to_string()),
            ("MUSTER_DATABASE_URL", "postgres://localhost/db".to_string()),
            ("MUSTER_OIDC_ISSUER", "http://127.0.0.1:2".to_string()),
            ("MUSTER_OIDC_CLIENT_ID", "client".to_string()),
            ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".to_string()),
        ];
        if let Some(root) = root {
            env.push(("MUSTER_REPO_ROOT", root.to_string()));
        }
        env
    };
    let (code, output) = run_cloud_expecting_exit(&base(None)).await;
    assert_eq!(code, Some(78), "output: {output}");
    assert!(
        output.contains("MUSTER_REPO_ROOT is required"),
        "output: {output}"
    );
    let (code, output) = run_cloud_expecting_exit(&base(Some("relative/repos"))).await;
    assert_eq!(code, Some(78), "output: {output}");
    assert!(
        output.contains("must be an absolute path"),
        "output: {output}"
    );
}

/// (case name, MUSTER_ENV, Paddle variables, expected text in the refusal)
type PaddleCase<'a> = (&'a str, &'a str, Vec<(&'a str, String)>, &'a str);

/// Paddle billing needs an explicit environment (`sandbox` or `live`), a key
/// of that environment, and talks only to that environment's official API.
/// Refusals never echo the key or the webhook secret.
#[tokio::test]
async fn paddle_environment_is_explicit_and_provider_url_is_safe() {
    // Synthetic values assembled at runtime; never real credentials.
    let sandbox_key = format!(
        "pdl_{}_apikey_{}",
        "sdbx", "01synthetic0000000000000000_synthetic000000000000000_zz"
    );
    let live_key = format!(
        "pdl_{}_apikey_{}",
        "live", "01synthetic0000000000000000_synthetic000000000000000_zz"
    );
    let secret = format!(
        "pdl_ntfset_{}",
        "01synthetic0000000000000000_synthetic0000000000000"
    );

    let cases: Vec<PaddleCase> = vec![
        (
            "missing environment",
            "production",
            vec![("MUSTER_PADDLE_API_KEY", sandbox_key.clone())],
            "MUSTER_PADDLE_ENVIRONMENT",
        ),
        (
            "legacy name",
            "production",
            vec![
                ("MUSTER_PADDLE_API_KEY", live_key.clone()),
                ("MUSTER_PADDLE_ENVIRONMENT", "production".into()),
            ],
            "MUSTER_PADDLE_ENVIRONMENT",
        ),
        (
            "sandbox key on live",
            "production",
            vec![
                ("MUSTER_PADDLE_API_KEY", sandbox_key.clone()),
                ("MUSTER_PADDLE_ENVIRONMENT", "live".into()),
            ],
            "MUSTER_PADDLE_API_KEY",
        ),
        (
            "live key on sandbox",
            "production",
            vec![
                ("MUSTER_PADDLE_API_KEY", live_key.clone()),
                ("MUSTER_PADDLE_ENVIRONMENT", "sandbox".into()),
            ],
            "MUSTER_PADDLE_API_KEY",
        ),
        (
            "provider url override in production",
            "production",
            vec![
                ("MUSTER_PADDLE_API_KEY", sandbox_key.clone()),
                ("MUSTER_PADDLE_ENVIRONMENT", "sandbox".into()),
                ("MUSTER_PADDLE_BASE_URL", "http://127.0.0.1:9".into()),
            ],
            "MUSTER_PADDLE_BASE_URL",
        ),
        (
            "live outside production",
            "development",
            vec![
                ("MUSTER_PADDLE_API_KEY", live_key.clone()),
                ("MUSTER_PADDLE_ENVIRONMENT", "live".into()),
            ],
            "MUSTER_PADDLE_ENVIRONMENT",
        ),
    ];

    for (name, muster_env, paddle, expected) in cases {
        let mut env: Vec<(&str, String)> = vec![
            ("MUSTER_ENV", muster_env.into()),
            ("MUSTER_BIND", "127.0.0.1:0".into()),
            ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
            ("MUSTER_PUBLIC_URL", "https://app.example.com".into()),
            ("MUSTER_DATABASE_URL", "postgres://localhost:1/db".into()),
            ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
            ("MUSTER_OIDC_CLIENT_ID", "client".into()),
            ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".into()),
            ("MUSTER_PADDLE_WEBHOOK_SECRET", secret.clone()),
            (
                "MUSTER_PADDLE_PRICE_PERSONAL",
                "pri_01synthetic00000000000personal".into(),
            ),
            (
                "MUSTER_PADDLE_PRICE_TEAM_SEAT",
                "pri_01synthetic00000000000teamseat".into(),
            ),
        ];
        env.extend(paddle);
        let (code, output) = run_cloud_expecting_exit(&env).await;
        assert_eq!(code, Some(78), "{name}: {output}");
        assert!(
            output.contains("configuration error") && output.contains(expected),
            "{name}: {output}"
        );
        for leaked in [&sandbox_key, &live_key, &secret] {
            assert!(
                !output.contains(leaked.as_str()),
                "{name}: output leaks a credential"
            );
        }
    }
}

/// A valid billing configuration is logged at startup with its credentials
/// redacted.
#[tokio::test]
async fn startup_log_redacts_paddle_credentials() {
    let key = format!(
        "pdl_{}_apikey_{}",
        "sdbx", "01synthetic0000000000000000_synthetic000000000000000_yy"
    );
    let secret = format!(
        "pdl_ntfset_{}",
        "01synthetic0000000000000000_synthetic0000000000yy"
    );
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "http://127.0.0.1:1".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost:1/db".into()),
        ("MUSTER_OIDC_ISSUER", "http://127.0.0.1:2".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".into()),
        ("MUSTER_PADDLE_API_KEY", key.clone()),
        ("MUSTER_PADDLE_WEBHOOK_SECRET", secret.clone()),
        ("MUSTER_PADDLE_ENVIRONMENT", "sandbox".into()),
        (
            "MUSTER_PADDLE_PRICE_PERSONAL",
            "pri_01synthetic00000000000personal".into(),
        ),
        (
            "MUSTER_PADDLE_PRICE_TEAM_SEAT",
            "pri_01synthetic00000000000teamseat".into(),
        ),
        ("MUSTER_CONTROL_DIR", "/nonexistent/muster-control".into()),
        (
            "MUSTER_CONTROL_WITNESS",
            "/nonexistent/muster-witness/w.json".into(),
        ),
    ])
    .await;
    assert_eq!(code, Some(78), "{output}");
    assert!(
        output.contains("PaddleConfig"),
        "config was not logged: {output}"
    );
    assert!(output.contains("sandbox-api.paddle.com"), "{output}");
    assert!(
        !output.contains(&key) && !output.contains(&secret),
        "credential leaked"
    );
}

/// The small storage limit tests use is development-only: production refuses
/// to start with it, so plan limits can never be replaced there.
#[tokio::test]
async fn production_refuses_the_development_storage_limit() {
    let (code, output) = run_cloud_expecting_exit(&[
        ("MUSTER_ENV", "production".into()),
        ("MUSTER_BIND", "127.0.0.1:0".into()),
        ("MUSTER_REPO_ROOT", "/nonexistent/muster-repos".into()),
        ("MUSTER_PUBLIC_URL", "https://cloud.example.com".into()),
        ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
        ("MUSTER_OIDC_ISSUER", "https://idp.example.com".into()),
        ("MUSTER_OIDC_CLIENT_ID", "client_123".into()),
        ("MUSTER_OIDC_CLIENT_SECRET", "secret_123".into()),
        ("MUSTER_DEV_STORAGE_LIMIT_BYTES", "1024".into()),
    ])
    .await;
    assert_eq!(code, Some(78), "output: {output}");
    assert!(
        output.contains("MUSTER_DEV_STORAGE_LIMIT_BYTES is invalid"),
        "output: {output}"
    );
}

/// Lifecycle authority is mandatory in every environment (the service never
/// creates it): an absolute control directory and an independent witness
/// file, disjoint from each other and from the Git storage root.
#[tokio::test]
async fn control_authority_paths_are_required_absolute_and_disjoint() {
    let base = |control: Option<(&str, &str)>| {
        let mut env = vec![
            ("MUSTER_ENV", "development".to_string()),
            ("MUSTER_BIND", "127.0.0.1:0".to_string()),
            ("MUSTER_PUBLIC_URL", "http://127.0.0.1:1".to_string()),
            ("MUSTER_DATABASE_URL", "postgres://localhost/db".to_string()),
            ("MUSTER_OIDC_ISSUER", "http://127.0.0.1:2".to_string()),
            ("MUSTER_OIDC_CLIENT_ID", "client".to_string()),
            ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".to_string()),
            ("MUSTER_REPO_ROOT", "/srv/muster/repos".to_string()),
        ];
        if let Some((dir, witness)) = control {
            env.push(("MUSTER_CONTROL_DIR", dir.to_string()));
            env.push(("MUSTER_CONTROL_WITNESS", witness.to_string()));
        }
        env
    };
    /// (case, control dir + witness, expected refusal)
    type Case<'a> = (&'a str, Option<(&'a str, &'a str)>, &'a str);
    let cases: [Case; 5] = [
        ("missing", None, "MUSTER_CONTROL_DIR is required"),
        (
            "relative",
            Some(("control", "/srv/witness/w.json")),
            "MUSTER_CONTROL_DIR is invalid",
        ),
        (
            "witness inside control",
            Some(("/srv/control", "/srv/control/w.json")),
            "MUSTER_CONTROL_WITNESS is invalid",
        ),
        (
            "control inside repo root",
            Some(("/srv/muster/repos/control", "/srv/witness/w.json")),
            "MUSTER_CONTROL_DIR is invalid",
        ),
        (
            "repo root inside control",
            Some(("/srv/muster", "/srv/witness/w.json")),
            "MUSTER_CONTROL_DIR is invalid",
        ),
    ];
    for (name, control, expected) in cases {
        let (code, output) = run_cloud_expecting_exit(&base(control)).await;
        assert_eq!(code, Some(78), "{name}: {output}");
        assert!(output.contains(expected), "{name}: {output}");
    }
}

/// Separation is decided on the real filesystem identity of the paths, not
/// their spelling: parent traversal is refused outright, and symbolic links
/// (in either direction, or hiding the witness) are resolved before
/// comparing. Both the service and the ops tool refuse with 78.
#[tokio::test]
async fn control_paths_are_compared_by_real_identity_not_spelling() {
    use std::os::unix::fs::symlink;
    let tmp = tempfile::tempdir().unwrap();
    let p = tmp.path().canonicalize().unwrap();
    let repos = p.join("repos");
    let ctl = p.join("ctl");
    std::fs::create_dir_all(&repos).unwrap();
    std::fs::create_dir_all(&ctl).unwrap();
    std::fs::create_dir_all(p.join("alias")).unwrap();
    std::fs::create_dir_all(p.join("wit")).unwrap();
    std::fs::create_dir_all(p.join("dangling")).unwrap();
    symlink(&repos, p.join("to-repos")).unwrap();
    symlink(&ctl, p.join("to-ctl")).unwrap();
    symlink(repos.join("w.json"), p.join("dangling/w.json")).unwrap();
    let s = |x: std::path::PathBuf| x.display().to_string();
    let good_witness = s(p.join("wit/w.json"));
    // (case, repo root, control dir, witness, variable refused)
    let cases = [
        (
            "control via ..",
            s(repos.clone()),
            s(p.join("alias/../repos/control")),
            good_witness.clone(),
            "MUSTER_CONTROL_DIR",
        ),
        (
            "control via symlink into repo",
            s(repos.clone()),
            s(p.join("to-repos/control")),
            good_witness.clone(),
            "MUSTER_CONTROL_DIR",
        ),
        (
            "repo via symlink into control",
            s(p.join("to-ctl/repos")),
            s(ctl.clone()),
            good_witness.clone(),
            "MUSTER_CONTROL_DIR",
        ),
        (
            "control is a symlink to repo",
            s(repos.clone()),
            s(p.join("to-repos")),
            good_witness.clone(),
            "MUSTER_CONTROL_DIR",
        ),
        (
            "repo via ..",
            s(p.join("ctl/x/../repos")),
            s(ctl.clone()),
            good_witness.clone(),
            "MUSTER_REPO_ROOT",
        ),
        (
            "witness via symlink into control",
            s(repos.clone()),
            s(ctl.clone()),
            s(p.join("to-ctl/w.json")),
            "MUSTER_CONTROL_WITNESS",
        ),
        (
            "witness via symlink into repo",
            s(repos.clone()),
            s(ctl.clone()),
            s(p.join("to-repos/w.json")),
            "MUSTER_CONTROL_WITNESS",
        ),
        (
            "witness via ..",
            s(repos.clone()),
            s(ctl.clone()),
            s(p.join("wit/../ctl/w.json")),
            "MUSTER_CONTROL_WITNESS",
        ),
        (
            "witness is a dangling symlink into repo",
            s(repos.clone()),
            s(ctl.clone()),
            s(p.join("dangling/w.json")),
            "MUSTER_CONTROL_WITNESS",
        ),
    ];
    for (name, repo, control, witness, refused) in cases {
        let env: Vec<(&'static str, String)> = vec![
            ("MUSTER_ENV", "development".into()),
            ("MUSTER_BIND", "127.0.0.1:0".into()),
            ("MUSTER_PUBLIC_URL", "http://127.0.0.1:1".into()),
            ("MUSTER_DATABASE_URL", "postgres://localhost/db".into()),
            ("MUSTER_OIDC_ISSUER", "http://127.0.0.1:2".into()),
            ("MUSTER_OIDC_CLIENT_ID", "client".into()),
            ("MUSTER_OIDC_CLIENT_SECRET", "synthetic".into()),
            ("MUSTER_REPO_ROOT", repo),
            ("MUSTER_CONTROL_DIR", control),
            ("MUSTER_CONTROL_WITNESS", witness),
        ];
        let (code, output) = run_cloud_expecting_exit(&env).await;
        assert_eq!(code, Some(78), "service, {name}: {output}");
        assert!(
            output.contains(&format!("{refused} is invalid")),
            "service, {name}: {output}"
        );
        let out = run_ops(&env, &["status"]).await;
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(78), "ops, {name}: {err}");
        assert!(
            err.contains(&format!("{refused} is invalid")),
            "ops, {name}: {err}"
        );
    }
}
