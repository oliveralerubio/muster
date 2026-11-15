//! Browser integration test runner: boots the real `muster-cloud` binary,
//! the external OIDC fixture and an ephemeral PostgreSQL database, then runs
//! the Playwright browser suite against the live `http://localhost:<port>` URL.

#[path = "support/mod.rs"]
mod support;

use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

#[tokio::test]
async fn browser_journeys() {
    let idp = support::oidc_fixture::start().await;
    let database_url = support::fresh_database().await;
    let port = free_port();
    let public_url = format!("http://localhost:{port}");

    let cloud = support::spawn_cloud(&[
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_BIND", format!("127.0.0.1:{port}")),
        ("MUSTER_PUBLIC_URL", public_url.clone()),
        ("MUSTER_DATABASE_URL", database_url),
        ("MUSTER_OIDC_ISSUER", idp.issuer.clone()),
        ("MUSTER_OIDC_CLIENT_ID", support::oidc_fixture::CLIENT_ID.into()),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            support::oidc_fixture::CLIENT_SECRET.into(),
        ),
    ])
    .await;

    let browser_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/browser");
    let script = browser_dir.join("run.mjs");

    let mut cmd = Command::new("node");
    cmd.arg(&script)
        .current_dir(&browser_dir)
        .env("MUSTER_CLOUD_URL", &public_url)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);

    if let Ok(evidence) = std::env::var("MUSTER_EVIDENCE_DIR") {
        cmd.env("MUSTER_EVIDENCE_DIR", evidence);
    }
    if let Ok(filter) = std::env::var("MUSTER_BROWSER_FILTER") {
        cmd.env("MUSTER_BROWSER_FILTER", filter);
    }

    let status = cmd
        .status()
        .await
        .expect("failed to execute node browser suite (is Node.js installed?)");

    // Keep `cloud` and `idp` alive across the entire browser run.
    drop(cloud);
    drop(idp);

    assert!(
        status.success(),
        "browser suite failed against {public_url} with status {status}"
    );
}
