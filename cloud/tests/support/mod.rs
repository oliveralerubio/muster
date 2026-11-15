//! Test harness: runs the real `muster-cloud` binary, a real external OIDC
//! provider (`oidc_fixture`) and a fresh database on the ephemeral
//! PostgreSQL cluster started by `deploy/pg-test.sh -- cargo test ...`.
//!
//! Tests talk to the service only over HTTP. The database is created here
//! (fixture boundary) and never read to assert behaviour.
#![allow(dead_code)]

pub mod fleet;
pub mod oidc_fixture;
pub mod paddle_fixture;

use std::process::Stdio;
use std::time::Duration;

use sqlx::Connection;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};

pub struct Cloud {
    pub base: String,
    child: Child,
}

/// Environment for a service process with the given base URL known up front.
pub type Env = Vec<(&'static str, String)>;

/// URL of the shared ephemeral cluster. Missing means the test was not run
/// through `deploy/pg-test.sh`; that is a hard failure, never a skip.
pub fn test_pg_url() -> String {
    std::env::var("MUSTER_TEST_PG").expect(
        "MUSTER_TEST_PG is not set: run through `deploy/pg-test.sh -- cargo test -p muster-cloud`",
    )
}

/// Creates an empty database `t_<uuid>` and returns its URL.
pub async fn fresh_database() -> String {
    let admin = test_pg_url();
    let mut conn = sqlx::PgConnection::connect(&admin)
        .await
        .expect("connect to MUSTER_TEST_PG");
    let name = format!("t_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&mut conn)
        .await
        .expect("create test database");
    let mut url = url::Url::parse(&admin).expect("MUSTER_TEST_PG url");
    url.set_path(&format!("/{name}"));
    url.to_string()
}

/// Starts the service binary with exactly `env` (plus `PATH`) and waits for
/// its listening line.
pub async fn spawn_cloud(env: &[(&str, String)]) -> Cloud {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_muster-cloud"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("MUSTER_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn muster-cloud");
    let mut lines = BufReader::new(child.stdout.take().expect("stdout")).lines();
    let wait = async {
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(addr) = line.strip_prefix("muster-cloud listening on ") {
                return Some(addr.to_string());
            }
        }
        None
    };
    let base = match tokio::time::timeout(Duration::from_secs(60), wait).await {
        Ok(Some(base)) => base,
        _ => {
            let _ = child.start_kill();
            let status = child.wait().await.ok();
            panic!("muster-cloud did not report a listening address (exit: {status:?})");
        }
    };
    // Keep draining stdout so the child never blocks on a full pipe.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    Cloud { base, child }
}

/// Runs the service binary to completion with `env` and returns
/// (exit code, stdout+stderr). For configuration refusal tests.
pub async fn run_cloud_expecting_exit(env: &[(&str, String)]) -> (Option<i32>, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_muster-cloud"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .kill_on_drop(true);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let output = tokio::time::timeout(Duration::from_secs(30), cmd.output())
        .await
        .expect("service should exit on invalid configuration")
        .expect("run muster-cloud");
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    (output.status.code(), text)
}

/// Service + IdP + fresh database, configured as a development deployment
/// reached at its loopback address.
pub struct Stack {
    pub cloud: Cloud,
    pub idp: oidc_fixture::Idp,
    /// Exact environment the service runs with, to restart it on the same
    /// database and Git storage (durability tests).
    pub env: Env,
    _repos: tempfile::TempDir,
}

impl Stack {
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.cloud.base, path)
    }

    pub fn origin(&self) -> String {
        self.cloud.base.clone()
    }

    /// Helper to complete a real login flow for a user and return their session details.
    pub async fn login_user(&self, subject: &str, email: &str) -> UserSession {
        let client = http();
        let login_resp = client.get(self.url("/auth/login")).send().await.unwrap();
        let (login_cookie, _) = set_cookie(&login_resp, "__Host-mc_login").unwrap();
        let auth_url = format!(
            "{}&login_as={subject}&email={email}",
            login_resp.headers()["location"].to_str().unwrap()
        );
        let idp_resp = client.get(&auth_url).send().await.unwrap();
        let cb_url = idp_resp.headers()["location"].to_str().unwrap();
        let cb_resp = client
            .get(cb_url)
            .header("Cookie", format!("__Host-mc_login={login_cookie}"))
            .send()
            .await
            .unwrap();
        let (session_cookie, _) = set_cookie(&cb_resp, "__Host-mc_session").unwrap();
        let me_resp = client
            .get(self.url("/v1/me"))
            .header("Cookie", format!("__Host-mc_session={session_cookie}"))
            .send()
            .await
            .unwrap();
        let me: muster_cloud::protocol::Me = me_resp.json().await.unwrap();
        UserSession {
            cookie: session_cookie,
            csrf_token: me.csrf_token,
            user_id: me.id,
            email: me.email,
        }
    }
}

pub struct UserSession {
    pub cookie: String,
    pub csrf_token: String,
    pub user_id: String,
    pub email: String,
}

/// Picks a free loopback port for the service. The service must know its
/// public URL (OIDC redirect URI) before it starts, so the port is chosen
/// here; the tiny race with other processes is acceptable in tests.
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

pub async fn stack() -> Stack {
    stack_with_env(&[]).await
}

/// [`stack`] with extra service configuration (explicit test preconditions).
pub async fn stack_with_env(extra: &[(&'static str, String)]) -> Stack {
    let idp = oidc_fixture::start().await;
    let database_url = fresh_database().await;
    let port = free_port();
    let public = format!("http://127.0.0.1:{port}");
    let repos = tempfile::tempdir().expect("repo root");
    let mut env: Env = vec![
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_REPO_ROOT", repos.path().display().to_string()),
        ("MUSTER_BIND", format!("127.0.0.1:{port}")),
        ("MUSTER_PUBLIC_URL", public),
        ("MUSTER_DATABASE_URL", database_url),
        ("MUSTER_OIDC_ISSUER", idp.issuer.clone()),
        ("MUSTER_OIDC_CLIENT_ID", oidc_fixture::CLIENT_ID.into()),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            oidc_fixture::CLIENT_SECRET.into(),
        ),
    ];
    env.extend(extra.iter().cloned());
    let cloud = spawn_cloud(&env).await;
    Stack {
        cloud,
        idp,
        env,
        _repos: repos,
    }
}

pub async fn stack_with_paddle(paddle: &paddle_fixture::PaddleFixture) -> Stack {
    let idp = oidc_fixture::start().await;
    let database_url = fresh_database().await;
    let port = free_port();
    let public = format!("http://127.0.0.1:{port}");
    let repos = tempfile::tempdir().expect("repo root");
    let env: Env = vec![
        ("MUSTER_ENV", "development".into()),
        ("MUSTER_REPO_ROOT", repos.path().display().to_string()),
        ("MUSTER_BIND", format!("127.0.0.1:{port}")),
        ("MUSTER_PUBLIC_URL", public),
        ("MUSTER_DATABASE_URL", database_url),
        ("MUSTER_OIDC_ISSUER", idp.issuer.clone()),
        ("MUSTER_OIDC_CLIENT_ID", oidc_fixture::CLIENT_ID.into()),
        (
            "MUSTER_OIDC_CLIENT_SECRET",
            oidc_fixture::CLIENT_SECRET.into(),
        ),
        ("MUSTER_PADDLE_API_KEY", paddle_fixture::API_KEY.into()),
        (
            "MUSTER_PADDLE_WEBHOOK_SECRET",
            paddle_fixture::WEBHOOK_SECRET.into(),
        ),
        (
            "MUSTER_PADDLE_PRICE_PERSONAL",
            paddle_fixture::PRICE_PERSONAL.into(),
        ),
        (
            "MUSTER_PADDLE_PRICE_TEAM_SEAT",
            paddle_fixture::PRICE_TEAM_SEAT.into(),
        ),
        ("MUSTER_PADDLE_BASE_URL", paddle.base_url.clone()),
        ("MUSTER_PADDLE_ENVIRONMENT", "sandbox".into()),
    ];
    let cloud = spawn_cloud(&env).await;
    Stack {
        cloud,
        idp,
        env,
        _repos: repos,
    }
}

impl Stack {
    /// Stops the service process and starts it again with the same
    /// environment (database, Git storage, port).
    pub async fn restart(&mut self) {
        self.cloud.kill().await;
        self.cloud = spawn_cloud(&self.env).await;
    }
}

impl Cloud {
    pub async fn kill(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("http client")
}

/// Value of cookie `name` from the response's `Set-Cookie` headers, with the
/// full header line for attribute assertions.
pub fn set_cookie(resp: &reqwest::Response, name: &str) -> Option<(String, String)> {
    resp.headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|line| {
            let (pair, _) = line.split_once(';').unwrap_or((line, ""));
            let (k, v) = pair.split_once('=')?;
            (k.trim() == name).then(|| (v.trim().to_string(), line.to_string()))
        })
}
