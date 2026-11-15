mod support;

use support::run_cloud_expecting_exit;

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
