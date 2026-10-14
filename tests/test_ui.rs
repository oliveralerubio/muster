mod support;

use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Ui {
    child: Child,
    token: String,
    address: String,
}

impl Ui {
    fn start(env: &support::MusterEnv) -> Self {
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("muster"));
        command
            .args(["ui", "--no-open", "--port", "0"])
            .env("HOME", env.home.path())
            .env("XDG_CONFIG_HOME", env.home.path().join(".config"))
            .env("XDG_STATE_HOME", env.home.path().join(".local/state"))
            .env("XDG_DATA_HOME", env.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", env.home.path().join(".cache"))
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    env.bin.display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, _) in std::env::vars() {
            if key.starts_with("MUSTER_") {
                command.env_remove(key);
            }
        }
        let mut child = command.spawn().expect("start muster ui");
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        reader.read_line(&mut line).expect("read UI startup line");
        if line.is_empty() {
            let output = child.wait_with_output().expect("collect failed UI output");
            panic!(
                "muster ui must start and print its URL; exit={:?}, stderr={}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let (url, address) = parse_url(line.trim());
        let token = url.split("token=").nth(1).expect("URL token").to_string();
        // Retain the startup pipe's reader through the child handle; the line is the contract.
        Self {
            child,
            token,
            address,
        }
    }

    fn request(
        &mut self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: Option<&str>,
    ) -> (u16, String, String) {
        let mut stream = TcpStream::connect(&self.address).expect("connect to UI localhost");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let body = body.unwrap_or("");
        write!(
            stream,
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
            self.address
        )
        .unwrap();
        if let Some(cookie) = cookie {
            write!(stream, "Cookie: {cookie}\r\n").unwrap();
        }
        if !body.is_empty() {
            write!(
                stream,
                "Content-Type: application/json\r\nContent-Length: {}\r\n",
                body.len()
            )
            .unwrap();
        }
        write!(stream, "\r\n{body}").unwrap();
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .expect("read HTTP response");
        let (head, content) = response.split_once("\r\n\r\n").expect("HTTP headers");
        let status = head
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        (status, head.to_string(), content.to_string())
    }

    fn authorized(&mut self, path: &str) -> (u16, String, String) {
        self.request("GET", &format!("{path}?token={}", self.token), None, None)
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_url(url: &str) -> (String, String) {
    let url = url.trim_start_matches('{').trim_end_matches('}');
    let url = url
        .split('"')
        .find(|part| part.starts_with("http://127.0.0.1:"))
        .unwrap_or(url);
    assert!(
        url.starts_with("http://127.0.0.1:"),
        "UI URL must bind localhost: {url}"
    );
    let authority = url
        .strip_prefix("http://")
        .unwrap()
        .split('/')
        .next()
        .unwrap();
    (url.to_string(), authority.to_string())
}

#[test]
fn ui_binds_localhost_and_uses_one_time_token_and_session_cookie() {
    let env = support::MusterEnv::new();
    let mut ui = Ui::start(&env);
    let (status, _, _) = ui.request("GET", "/", None, None);
    assert_eq!(status, 401);
    let (status, headers, _) = ui.authorized("/");
    assert_eq!(status, 200);
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("session cookie")
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (status, _, _) = ui.request("GET", &format!("/?token={}", ui.token), None, None);
    assert_eq!(status, 401);
    let (status, _, _) = ui.request("GET", "/", Some(&cookie), None);
    assert_eq!(status, 200);
}

#[test]
fn ui_assets_have_no_external_origins() {
    let env = support::MusterEnv::new();
    let mut ui = Ui::start(&env);
    let (status, _, html) = ui.authorized("/");
    assert_eq!(status, 200);
    let assets = html;
    for attribute in ["src=\"", "href=\"", "@import ", "url("] {
        let mut rest = assets.as_str();
        while let Some((_, tail)) = rest.split_once(attribute) {
            let value = tail.split(['\"', '\'', ')', ' ', ';']).next().unwrap_or("");
            assert!(
                !value.starts_with("http://") && !value.starts_with("https://")
                    || value.starts_with("http://127.0.0.1"),
                "external asset URL: {value}"
            );
            rest = tail;
        }
    }
    assert!(!assets.is_empty());
}

#[test]
fn ui_status_api_matches_cli_status() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let (_, cli) = env.run_json(&["--format", "json", "status"]);
    let mut ui = Ui::start(&env);
    let (status, _, body) = ui.authorized("/api/status");
    assert_eq!(status, 200);
    let api: Value = serde_json::from_str(&body).expect("status API JSON");
    assert_eq!(api["result"].clone(), cli.result().clone());
}

#[test]
fn ui_secret_reveal_requires_confirmation_and_is_recorded() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let sentinel = "ui-test-secret-never-leak";
    env.run(&["--yes", "secret", "add", "ui/test", "--value-stdin"])
        .write_stdin(sentinel)
        .assert()
        .success();
    let mut ui = Ui::start(&env);
    let (_, headers, _) = ui.authorized("/");
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (status, _, body) = ui.request(
        "POST",
        "/api/secret/reveal",
        Some(&cookie),
        Some(r#"{"name":"ui/test"}"#),
    );
    assert!(status == 400 || status == 403);
    assert!(!body.contains(sentinel));
    for path in ["/api/status", "/api/doctor", "/api/secrets", "/api/history"] {
        let (_, _, body) = ui.request("GET", path, Some(&cookie), None);
        assert!(!body.contains(sentinel), "secret leaked from {path}");
    }
    let (status, _, body) = ui.request(
        "POST",
        "/api/secret/reveal",
        Some(&cookie),
        Some(r#"{"name":"ui/test","confirm":true}"#),
    );
    assert_eq!(status, 200);
    assert!(body.contains(sentinel));
    let (_, _, history) = ui.request("GET", "/api/history", Some(&cookie), None);
    assert!(
        history.contains("reveal") || history.contains("secret"),
        "reveal must appear in history"
    );
}

#[test]
fn ui_action_responses_include_equivalent_cli_command() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let (_, headers, _) = ui.authorized("/");
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    let (status, _, body) = ui.request("POST", "/api/sync", Some(&cookie), Some("{}"));
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).expect("action JSON");
    assert!(
        response["cli"]
            .as_str()
            .is_some_and(|cli| cli.contains("muster")),
        "action response must include CLI equivalent: {response}"
    );
}

#[test]
fn ui_dashboard_smoke_and_color_scheme_support() {
    let env = support::MusterEnv::new();
    let mut ui = Ui::start(&env);
    let (status, headers, html) = ui.authorized("/");
    assert_eq!(status, 200);
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(
        html.to_ascii_lowercase().contains("panel")
            || html.to_ascii_lowercase().contains("dashboard")
    );
    let (status, _, css) = ui.request("GET", "/style.css", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(
        css.contains("prefers-color-scheme"),
        "CSS must support light/dark system theme"
    );
}

#[test]
fn web_secret_add_and_rotate_encrypt_values_and_do_not_leak() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let payload =
        serde_json::json!({"path":"web/token", "value":"web-sentinel-initial", "confirm":true})
            .to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/add", None, Some(&payload))
            .0,
        401
    );
    let (_, headers, _) = ui.authorized("/");
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    for (action, value) in [
        ("add", "web-sentinel-initial"),
        ("rotate", "web-sentinel-rotated"),
    ] {
        let path = format!("/api/secret/{action}");
        let unconfirmed = serde_json::json!({"path":"web/token", "value":value}).to_string();
        let (status, _, response) = ui.request("POST", &path, Some(&cookie), Some(&unconfirmed));
        assert_eq!(status, 400);
        assert!(!response.contains(value));
        let payload =
            serde_json::json!({"path":"web/token", "value":value, "confirm":true}).to_string();
        let (status, _, response) = ui.request("POST", &path, Some(&cookie), Some(&payload));
        assert_eq!(status, 200);
        assert!(!response.contains(value));
        let response: Value = serde_json::from_str(&response).unwrap();
        assert!(response["cli"].as_str().unwrap().contains("--value-stdin"));
        let (output, revealed) = env.run_json(&[
            "--format",
            "json",
            "--yes",
            "secret",
            "get",
            "native://web/token",
            "--reveal",
        ]);
        assert!(output.status.success());
        assert!(revealed
            .result()
            .as_str()
            .is_some_and(|actual| actual == value));
        let encrypted = std::fs::read(env.config_dir().join("vault/web.age")).unwrap();
        assert!(!String::from_utf8_lossy(&encrypted).contains(value));
        let (_, _, history) = ui.request("GET", "/api/history", Some(&cookie), None);
        assert!(!history.contains(value));
        assert!(history.contains(&format!("secret.{action}")));
        let audit =
            std::fs::read_to_string(env.path().join(".local/state/muster/history.jsonl")).unwrap();
        assert!(!audit.contains(value));
    }
}

#[test]
fn web_secret_writes_reject_stdin_and_oversize_bodies() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let (_, headers, html) = ui.authorized("/");
    assert!(html.contains("type=\"password\" autocomplete=\"new-password\""));
    let cookie = headers
        .lines()
        .find(|line| line.to_ascii_lowercase().starts_with("set-cookie:"))
        .unwrap()
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_owned();
    let body = serde_json::json!({"name":"token", "group":"web", "value":"test-input", "value-stdin":true, "confirm":true}).to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/add", Some(&cookie), Some(&body))
            .0,
        400
    );
    let body =
        serde_json::json!({"name":"token", "group":"web", "value":"test-input", "confirm":true})
            .to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/add", Some(&cookie), Some(&body))
            .0,
        200
    );
    let body = serde_json::json!({"path":"web/token", "value":"x".repeat(65536), "confirm":true})
        .to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/rotate", Some(&cookie), Some(&body))
            .0,
        413
    );
    assert_eq!(
        ui.request("GET", "/api/secret/add", Some(&cookie), None).0,
        405
    );
}
