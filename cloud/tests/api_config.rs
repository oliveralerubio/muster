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
