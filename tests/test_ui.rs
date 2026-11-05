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
        // Doctor probes agents on PATH; use only this simulated machine's executables.
        env.link_system_exe("git");
        let mut command = Command::new(assert_cmd::cargo::cargo_bin("muster"));
        command
            .args(["ui", "--no-open", "--port", "0"])
            .env("HOME", env.home.path())
            .env("XDG_CONFIG_HOME", env.home.path().join(".config"))
            .env("XDG_STATE_HOME", env.home.path().join(".local/state"))
            .env("XDG_DATA_HOME", env.home.path().join(".local/share"))
            .env("XDG_CACHE_HOME", env.home.path().join(".cache"))
            .env("PATH", &env.bin)
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

    /// Consume the one-time token and return the session cookie for later calls.
    fn session(&mut self) -> String {
        let (_, headers, _) = self.authorized("/");
        headers
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
            .to_string()
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
fn ui_effective_api_matches_cli_effective() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let (_, cli) = env.run_json(&["--format", "json", "effective"]);
    let mut ui = Ui::start(&env);
    let (status, _, body) = ui.authorized("/api/effective");
    assert_eq!(status, 200);
    let api: Value = serde_json::from_str(&body).expect("effective API JSON");
    assert_eq!(api["result"], *cli.result());
}

#[test]
fn ui_doctor_uses_only_the_simulated_machine_path() {
    let env = support::MusterEnv::new();
    env.link_system_exe("git");
    env.fake_exe("claude", "echo '2.1.0 (Claude Code)'");
    env.run_isolated(&["--yes", "init"]).assert().success();

    let mut ui = Ui::start(&env);
    let (status, _, body) = ui.authorized("/api/doctor");
    assert_eq!(status, 200);
    let api: Value = serde_json::from_str(&body).expect("doctor API JSON");
    let diagnostics = api["diagnostics"].as_array().unwrap();
    for agent in ["codex", "opencode", "antigravity"] {
        assert!(
            diagnostics
                .iter()
                .any(|d| d["code"] == "missing_binary" && d["path"] == agent),
            "{agent} is absent from the simulated machine"
        );
    }
    assert!(!diagnostics
        .iter()
        .any(|d| d["code"] == "missing_binary" && d["path"] == "claude"));
    assert_eq!(api["ok"], true);
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
    let cookie = ui.session();
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
fn ui_secret_reveal_rejects_env_scheme() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui.session();
    let (status, _, body) = ui.request(
        "POST",
        "/api/secret/reveal",
        Some(&cookie),
        Some(r#"{"name":"env://PATH","confirm":true}"#),
    );
    assert_eq!(status, 400);
    assert!(body.contains("cannot reveal"));
}

#[test]
fn web_reject_routes_remove_pending_requests_with_session_cookie() {
    let owner = support::MusterEnv::new();
    let joiner = support::MusterEnv::new();
    owner
        .run(&["--yes", "init", "--machine", "owner-laptop"])
        .assert()
        .success();
    let config = owner.config_dir();
    let config_str = config.to_str().unwrap();
    // A pending machine enrollment request, written through the shared repo.
    joiner
        .run(&[
            "--format",
            "json",
            "--config",
            config_str,
            "enroll",
            "--name",
            "web-laptop",
        ])
        .assert()
        .success();
    // A pending member invitation.
    owner
        .run(&[
            "--format", "json", "--config", config_str, "member", "add", "webbie",
        ])
        .assert()
        .success();
    assert!(config.join("enroll/web-laptop.toml").exists());

    let mut ui = Ui::start(&owner);
    let cookie = ui.session();

    // Without confirm the write is refused.
    let (status, _, _) = ui.request(
        "POST",
        "/api/member/reject",
        Some(&cookie),
        Some(r#"{"name":"webbie"}"#),
    );
    assert_eq!(status, 400);
    let (status, _, body) = ui.request(
        "POST",
        "/api/member/reject",
        Some(&cookie),
        Some(r#"{"name":"webbie","confirm":true}"#),
    );
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).expect("member reject JSON");
    assert_eq!(response["result"]["rejected"], "webbie", "{response}");
    assert_eq!(response["result"]["kind"], "member", "{response}");
    assert!(
        response["cli"].as_str().unwrap().contains("member reject"),
        "{response}"
    );

    let (status, _, body) = ui.request(
        "POST",
        "/api/reject",
        Some(&cookie),
        Some(r#"{"name":"web-laptop","confirm":true}"#),
    );
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).expect("machine reject JSON");
    assert_eq!(response["result"]["rejected"], "web-laptop", "{response}");
    assert_eq!(response["result"]["kind"], "machine", "{response}");
    assert!(
        response["cli"].as_str().unwrap().contains("muster reject"),
        "{response}"
    );
    assert!(!config.join("enroll/web-laptop.toml").exists());

    let (_, members) = owner.run_json(&["--format", "json", "member", "list"]);
    assert!(
        !members.raw.to_string().contains("webbie"),
        "{}",
        members.raw
    );
}

#[test]
fn ui_machines_api_lists_pending_enrollment_requests_for_the_panel() {
    let owner = support::MusterEnv::new();
    let joiner = support::MusterEnv::new();
    owner
        .run(&["--yes", "init", "--machine", "owner-laptop"])
        .assert()
        .success();
    let config = owner.config_dir();
    let config_str = config.to_str().unwrap();
    joiner
        .run(&[
            "--format",
            "json",
            "--config",
            config_str,
            "enroll",
            "--name",
            "ui-pending-laptop",
        ])
        .assert()
        .success();

    let mut ui = Ui::start(&owner);
    let cookie = ui.session();
    let (status, _, body) = ui.request("GET", "/api/machines", Some(&cookie), None);
    assert_eq!(status, 200);
    let api: Value = serde_json::from_str(&body).expect("machines API JSON");
    let pending = api["result"]["pending"]
        .as_array()
        .expect("the machines route must serve a pending array");
    let entry = pending
        .iter()
        .find(|p| p["name"] == "ui-pending-laptop")
        .expect("the pending machine request must be served to the UI");
    assert_eq!(entry["approvals_received"], 0, "{entry}");
    assert_eq!(entry["approvals_required"], 1, "{entry}");
    assert!(entry["platform"].as_str().is_some(), "{entry}");
    assert!(entry["version"].as_str().is_some(), "{entry}");
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

// Shared helpers for the edit-route tests below.
fn ui_session_cookie(ui: &mut Ui) -> String {
    let (_, headers, _) = ui.authorized("/");
    headers
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
        .to_string()
}

fn ui_post(ui: &mut Ui, cookie: &str, path: &str, body: &Value) -> (u16, Value) {
    let (status, _, content) = ui.request("POST", path, Some(cookie), Some(&body.to_string()));
    let json: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
    (status, json)
}

#[test]
fn web_profile_edit_previews_diff_then_saves_and_removes() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let manifest = || std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    let profile_toml = "[agents.models]\nclaude = \"sonnet\"\n\n[mcp.docs]\nurl = \"https://docs.example/mcp\"\n[mcp.docs.headers]\nAuthorization = \"native://app/docs-token\"\n";

    // Unknown profile: the show route answers 400 until it exists.
    assert_eq!(
        ui.request(
            "GET",
            "/api/profile?name=dev&toml=true",
            Some(&cookie),
            None
        )
        .0,
        400
    );

    // Preview: dry run returns a diff and writes nothing.
    let before = manifest();
    let (status, preview) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/set",
        &serde_json::json!({"name": "dev", "toml": profile_toml, "dry_run": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(preview["ok"], true, "{preview}");
    assert!(preview["result"]["diff"]
        .as_str()
        .unwrap()
        .contains("+[agents.models]"));
    assert!(preview["cli"]
        .as_str()
        .unwrap()
        .contains("muster profile set dev"));
    assert!(preview["cli"].as_str().unwrap().contains("--dry-run"));
    assert_eq!(manifest(), before, "preview must not write");

    // Saving needs confirm: true, like the CLI needs --yes.
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/set",
        &serde_json::json!({"name": "dev", "toml": profile_toml}),
    );
    assert_eq!(status, 400, "{refused}");
    assert_eq!(manifest(), before, "unconfirmed save must not write");

    // Save: applies after confirmation.
    let (status, saved) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/set",
        &serde_json::json!({"name": "dev", "toml": profile_toml, "confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(saved["ok"], true, "{saved}");
    let after = manifest();
    assert!(after.contains("[profiles.dev"), "{after}");
    // Secret references are stored as references, verbatim.
    assert!(after.contains("native://app/docs-token"), "{after}");

    // The edit form loads the profile back as TOML.
    let (status, _, shown) = ui.request(
        "GET",
        "/api/profile?name=dev&toml=true",
        Some(&cookie),
        None,
    );
    assert_eq!(status, 200);
    let shown: Value = serde_json::from_str(&shown).unwrap();
    assert!(shown["result"]["toml"].as_str().unwrap().contains("sonnet"));
    let (_, _, list) = ui.request("GET", "/api/profiles", Some(&cookie), None);
    assert!(list.contains("dev"));

    // Missing parameters are usage errors (exit 2 → 400).
    let (status, _) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/set",
        &serde_json::json!({"toml": "x = 1\n"}),
    );
    assert_eq!(status, 400);

    // Removal needs the confirmation flag, like the CLI needs --yes.
    let (status, _) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/rm",
        &serde_json::json!({"name": "dev"}),
    );
    assert_eq!(status, 400);
    let (status, removed) = ui_post(
        &mut ui,
        &cookie,
        "/api/profile/rm",
        &serde_json::json!({"name": "dev", "confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(removed["result"]["removed"], "dev");
    assert!(!manifest().contains("[profiles.dev"));
}

#[test]
fn web_mcp_server_routes_add_list_and_rm() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let manifest = || std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();

    let (status, _, empty) = ui.request("GET", "/api/mcp-servers", Some(&cookie), None);
    assert_eq!(status, 200);
    let empty: Value = serde_json::from_str(&empty).unwrap();
    assert_eq!(empty["result"]["servers"].as_array().unwrap().len(), 0);

    // Preview first: a dry run with a diff and no write.
    let before = manifest();
    let (status, preview) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/add",
        &serde_json::json!({"name": "context", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-context"], "env": ["API_KEY=native://app/api-key"], "dry_run": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(preview["ok"], true, "{preview}");
    assert!(preview["result"]["diff"]
        .as_str()
        .unwrap()
        .contains("+command = \"npx\""));
    assert_eq!(manifest(), before, "preview must not write");

    // Saving needs confirm: true, like the CLI needs --yes.
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/add",
        &serde_json::json!({"name": "context", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-context"], "env": ["API_KEY=native://app/api-key"]}),
    );
    assert_eq!(status, 400, "{refused}");
    assert_eq!(manifest(), before, "unconfirmed save must not write");

    let (status, saved) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/add",
        &serde_json::json!({"name": "context", "command": "npx", "args": ["-y", "@modelcontextprotocol/server-context"], "env": ["API_KEY=native://app/api-key"], "confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(saved["ok"], true, "{saved}");
    assert!(saved["cli"]
        .as_str()
        .unwrap()
        .contains("muster mcp-server add context"));
    let after = manifest();
    assert!(after.contains("[mcp.context]"), "{after}");
    assert!(
        after.contains("API_KEY = \"native://app/api-key\""),
        "{after}"
    );

    let (status, _, listed) = ui.request("GET", "/api/mcp-servers", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(listed.contains("context"));
    assert!(listed.contains("stdio"));

    // Wrong method and invalid server definitions fail cleanly.
    assert_eq!(
        ui.request("GET", "/api/mcp-server/add", Some(&cookie), None)
            .0,
        405
    );
    let (status, _) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/add",
        &serde_json::json!({"name": "broken", "confirm": true}),
    );
    assert_eq!(status, 422, "neither command nor url is a config error");

    // Removal mirrors the CLI: confirm required, then gone.
    let (status, _) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/rm",
        &serde_json::json!({"name": "context"}),
    );
    assert_eq!(status, 400);
    let (status, removed) = ui_post(
        &mut ui,
        &cookie,
        "/api/mcp-server/rm",
        &serde_json::json!({"name": "context", "confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(removed["result"]["removed"], "context");
    assert!(!manifest().contains("[mcp.context]"));
}

#[test]
fn web_schedule_routes_report_enable_and_disable() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);

    let (status, _, status_body) = ui.request("GET", "/api/schedule", Some(&cookie), None);
    assert_eq!(status, 200);
    let status_json: Value = serde_json::from_str(&status_body).unwrap();
    assert_eq!(status_json["result"]["enabled"], false);

    // The manual backend is deterministic in tests (no systemd or launchd).
    // Applying needs confirm, like the CLI needs --yes.
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/schedule/enable",
        &serde_json::json!({"interval": 30, "backend": "manual"}),
    );
    assert_eq!(status, 400, "{refused}");
    let (status, enabled) = ui_post(
        &mut ui,
        &cookie,
        "/api/schedule/enable",
        &serde_json::json!({"interval": 30, "backend": "manual", "confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(enabled["ok"], true, "{enabled}");
    assert_eq!(enabled["result"]["backend"], "manual");
    assert!(enabled["cli"]
        .as_str()
        .unwrap()
        .contains("muster schedule enable"));

    let (status, disabled) = ui_post(
        &mut ui,
        &cookie,
        "/api/schedule/disable",
        &serde_json::json!({"confirm": true}),
    );
    assert_eq!(status, 200);
    assert_eq!(disabled["ok"], true, "{disabled}");
    let (_, _, status_body) = ui.request("GET", "/api/schedule", Some(&cookie), None);
    let status_json: Value = serde_json::from_str(&status_body).unwrap();
    assert_eq!(status_json["result"]["enabled"], false);
}

#[test]
fn web_profile_set_rejects_from_file_in_both_spellings() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let manifest = || std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    let before = manifest();

    // A local file an attacker would like the panel to read for them.
    let outside = env.path().join("outside.toml");
    std::fs::write(&outside, "[agents]\nenabled = [\"claude\"]\n").unwrap();

    // The route takes inline `toml` text only: both `from-file` and the
    // normalized `from_file` spelling are unknown parameters (400), whatever
    // the body's `confirm` says, and the file is never read.
    for key in ["from-file", "from_file"] {
        let body = serde_json::json!({
            "name": "dev",
            key: outside.to_str().unwrap(),
            "confirm": true,
        });
        let (status, reply) = ui_post(&mut ui, &cookie, "/api/profile/set", &body);
        assert_eq!(status, 400, "{key}: {reply}");
        assert!(
            !reply.to_string().contains("claude"),
            "{key} leaked file content: {reply}"
        );
        assert_eq!(manifest(), before, "{key} must not write");
    }

    // The same file through the CLI keeps working.
    let (out, set) = env.run_json(&[
        "--format",
        "json",
        "--yes",
        "profile",
        "set",
        "dev",
        "--from-file",
        outside.to_str().unwrap(),
    ]);
    assert!(out.status.success(), "{}", set.raw);
    assert!(manifest().contains("[profiles.dev"), "{}", manifest());
}

#[test]
fn web_mcp_server_edit_of_unchanged_values_round_trips_to_an_empty_diff() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let manifest = || std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();

    // Values that comma-splitting used to corrupt.
    let body = serde_json::json!({
        "name": "context",
        "command": "npx",
        "args": ["-y", "--cache=/tmp/a,b", "@modelcontextprotocol/server-context"],
        "env": ["MSG=hello, world", "API_KEY=native://app/api-key"],
        "confirm": true,
    });
    let (status, saved) = ui_post(&mut ui, &cookie, "/api/mcp-server/add", &body);
    assert_eq!(status, 200, "{saved}");
    let stored = manifest();

    // The edit form loads the server back (what the panel puts in the form).
    let (status, _, listed) = ui.request("GET", "/api/mcp-servers", Some(&cookie), None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_str(&listed).unwrap();
    let server = &listed["result"]["servers"][0];
    assert_eq!(server["name"], "context");
    assert_eq!(server["server"]["env"]["MSG"], "hello, world");
    assert_eq!(server["server"]["args"][1], "--cache=/tmp/a,b");

    // Re-adding exactly the shown values (the edit→save flow) must be a
    // no-op: an empty diff and a byte-identical manifest.
    let again = serde_json::json!({
        "name": server["name"],
        "command": server["server"]["command"],
        "args": server["server"]["args"],
        "env": server["server"]["env"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| format!("{k}={}", v.as_str().unwrap()))
            .collect::<Vec<_>>(),
        "dry_run": true,
    });
    let (status, preview) = ui_post(&mut ui, &cookie, "/api/mcp-server/add", &again);
    assert_eq!(status, 200, "{preview}");
    assert_eq!(preview["ok"], true, "{preview}");
    assert_eq!(preview["result"]["diff"], "", "unchanged edit → empty diff");
    let mut apply = again;
    apply["dry_run"] = Value::Bool(false);
    apply["confirm"] = Value::Bool(true);
    let (status, applied) = ui_post(&mut ui, &cookie, "/api/mcp-server/add", &apply);
    assert_eq!(status, 200, "{applied}");
    assert_eq!(
        manifest(),
        stored,
        "saving an unchanged server must not touch the manifest"
    );
}

#[test]
fn ui_frontend_assets_render_deep_flatten_and_navigation_guard() {
    let env = support::MusterEnv::new();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let (status, _, js) = ui.request("GET", "/app.js", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(
        js.contains(".flat(Infinity)"),
        "render() must deeply flatten blocks with flat(Infinity) so nested arrays do not stringify as [object ...]"
    );
    assert!(
        js.contains("flatMap"),
        "profiles() must build planBlocks flat"
    );
    assert!(
        js.contains("navToken"),
        "app.js must track navigation token to guard against tab-switch races"
    );
    assert!(
        js.contains("token !== navToken"),
        "show() and view renderers must guard with token !== navToken before rendering"
    );
}

#[test]
fn web_provider_routes_add_select_list_remove_but_do_not_launch_login() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let (status, added) = ui_post(
        &mut ui,
        &cookie,
        "/api/provider/add",
        &serde_json::json!({"id":"gateway","kind":"anthropic","base-url":"https://example.invalid","default-model":"example","key":"env://EXAMPLE_KEY","confirm":true}),
    );
    assert_eq!(status, 200);
    assert_eq!(added["ok"], true);
    let (status, selected) = ui_post(
        &mut ui,
        &cookie,
        "/api/provider/use",
        &serde_json::json!({"id":"gateway","agent":"claude","confirm":true}),
    );
    assert_eq!(status, 200);
    assert_eq!(selected["result"]["agent"], "claude");
    let (status, _, listed) = ui.request("GET", "/api/providers", Some(&cookie), None);
    assert_eq!(status, 200);
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["result"]["providers"][0]["key"], "env://EXAMPLE_KEY");
    assert_eq!(
        listed["result"]["providers"][0]["selected_agents"],
        serde_json::json!(["claude"])
    );
    let (status, _) = ui_post(
        &mut ui,
        &cookie,
        "/api/provider/login",
        &serde_json::json!({"id":"claude","confirm":true}),
    );
    assert_eq!(status, 404);
    let (status, removed) = ui_post(
        &mut ui,
        &cookie,
        "/api/provider/rm",
        &serde_json::json!({"id":"gateway","confirm":true}),
    );
    assert_eq!(status, 200);
    assert_eq!(removed["result"]["removed"], "gateway");
    let (_, _, listed) = ui.request("GET", "/api/providers", Some(&cookie), None);
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed["result"]["providers"], serde_json::json!([]));
}

#[test]
fn secret_approval_requires_a_human_and_leases_expire_and_revoke() {
    use serde_json::json;
    let env = support::MusterEnv::new();
    env.run(&["init", "--yes"]).assert().success();
    let value = "lease-test-private-value";
    env.run(&["secret", "add", "app/key", "--value-stdin"])
        .write_stdin(value)
        .assert()
        .success();
    let manifest = env.config_dir().join("muster.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str("\n[secrets.'native://app/key']\nallow = ['claude']\napproval = 'each'\napproval_ttl = '15m'\n");
    std::fs::write(&manifest, &text).unwrap();
    env.run(&["trust", "resign", "--yes"]).assert().success();
    let state = env.path().join(".local/state/muster");
    let store_path = state.join("secret-leases.json");
    let reveal = [
        "secret",
        "get",
        "native://app/key",
        "--agent",
        "claude",
        "--reveal",
        "--yes",
        "--format",
        "json",
    ];
    for _ in 0..2 {
        let (out, envelope) = env.run_json(&reveal);
        assert_eq!(out.status.code(), Some(5));
        assert!(!String::from_utf8_lossy(&out.stdout).contains(value));
        assert!(!String::from_utf8_lossy(&out.stderr).contains(value));
        assert!(envelope
            .diagnostics()
            .as_array()
            .unwrap()
            .iter()
            .any(|d| d["code"] == "secret_approval_required"
                && d["suggestion"]
                    .as_str()
                    .unwrap()
                    .starts_with("muster secret approve ")));
    }
    let (_, requests) = env.run_json(&["secret", "requests", "--format", "json"]);
    assert_eq!(requests.result().as_array().unwrap().len(), 1);
    let first = requests.result()[0]["id"].as_str().unwrap();
    env.run(&["secret", "approve", first, "--yes"])
        .assert()
        .code(5);
    let denied = env
        .run(&[
            "secret",
            "get",
            "native://app/key",
            "--agent",
            "codex",
            "--reveal",
            "--yes",
            "--format",
            "json",
        ])
        .output()
        .unwrap();
    assert_eq!(denied.status.code(), Some(5));
    assert!(String::from_utf8_lossy(&denied.stdout).contains("secret_not_allowed_for_agent"));
    assert!(!String::from_utf8_lossy(&denied.stdout).contains(value));

    let mut ui = Ui::start(&env);
    let payload = json!({"id": first, "confirm": true}).to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/approve", None, Some(&payload))
            .0,
        401
    );
    let cookie = ui.session();
    let unconfirmed = json!({"id": first}).to_string();
    assert_ne!(
        ui.request(
            "POST",
            "/api/secret/approve",
            Some(&cookie),
            Some(&unconfirmed)
        )
        .0,
        200
    );
    let too_long = json!({"id": first, "ttl":"25h", "confirm":true}).to_string();
    assert_ne!(
        ui.request(
            "POST",
            "/api/secret/approve",
            Some(&cookie),
            Some(&too_long)
        )
        .0,
        200
    );
    for cycle in 0..3 {
        let (_, _, body) = ui.request("GET", "/api/secret/requests", Some(&cookie), None);
        let pending: Value = serde_json::from_str(&body).unwrap();
        let id = pending["result"][0]["id"].as_str().unwrap();
        let payload = json!({"id":id, "confirm":true}).to_string();
        let (status, _, body) =
            ui.request("POST", "/api/secret/approve", Some(&cookie), Some(&payload));
        assert_eq!(status, 200, "{body}");
        assert!(!body.contains(value));
        let approved: Value = serde_json::from_str(&body).unwrap();
        let lease_id = approved["result"]["id"].as_str().unwrap();
        let (out, envelope) = env.run_json(&reveal);
        assert!(out.status.success(), "{}", envelope.raw);
        assert_eq!(envelope.result(), value);
        let stored = std::fs::read_to_string(&store_path).unwrap();
        assert!(!stored.contains(value));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&store_path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        if cycle == 0 {
            std::fs::write(&manifest, text.replace("allow = ['claude']", "allow = []")).unwrap();
            env.run(&["trust", "resign", "--yes"]).assert().success();
            let (out, envelope) = env.run_json(&reveal);
            assert_eq!(out.status.code(), Some(5));
            assert!(envelope
                .diagnostics()
                .as_array()
                .unwrap()
                .iter()
                .any(|d| d["code"] == "secret_not_allowed_for_agent"));
            std::fs::write(&manifest, &text).unwrap();
            env.run(&["trust", "resign", "--yes"]).assert().success();
            let mut data: Value = serde_json::from_str(&stored).unwrap();
            data["leases"][0]["expires_at"] = json!(1);
            std::fs::write(&store_path, data.to_string()).unwrap();
        } else if cycle == 1 {
            env.run(&["secret", "revoke", lease_id, "--yes"])
                .assert()
                .success();
        } else {
            let payload = json!({"all":true, "confirm":true}).to_string();
            let (status, _, body) =
                ui.request("POST", "/api/secret/revoke", Some(&cookie), Some(&payload));
            assert_eq!(status, 200, "{body}");
        }
        let (out, envelope) = env.run_json(&reveal);
        assert_eq!(out.status.code(), Some(5), "{}", envelope.raw);
    }
    let (_, pending) = env.run_json(&["secret", "requests", "--format", "json"]);
    let id = pending.result()[0]["id"].as_str().unwrap();
    let payload = json!({"id": id, "confirm": true}).to_string();
    assert_eq!(
        ui.request("POST", "/api/secret/deny", Some(&cookie), Some(&payload))
            .0,
        200
    );
    let (_, requests) = env.run_json(&["secret", "requests", "--format", "json"]);
    assert_eq!(requests.result(), &json!([]));
    // `--yes` alone never satisfies a per-use approval policy: an unnamed
    // caller is refused instead of bypassing the lease.
    let bare = env
        .run(&["secret", "get", "native://app/key", "--reveal", "--yes"])
        .output()
        .unwrap();
    assert_eq!(bare.status.code(), Some(5));
    assert!(!String::from_utf8_lossy(&bare.stdout).contains(value));
    let events = muster::audit::events(&state).unwrap();
    for action in [
        "secret.request",
        "secret.approve",
        "secret.reveal",
        "secret.revoke",
        "secret.deny",
    ] {
        let matching: Vec<_> = events.iter().filter(|e| e["action"] == action).collect();
        assert!(!matching.is_empty(), "missing {action}");
        assert!(matching.iter().all(|e| e["actor"].is_string()));
    }
    assert!(events
        .iter()
        .any(|e| e["action"] == "secret.request" && e["actor"] == "claude"));
    assert!(events
        .iter()
        .any(|e| e["action"] == "secret.reveal" && e["actor"] == "claude"));
    assert!(!serde_json::to_string(&events).unwrap().contains(value));
}

#[test]
fn ui_why_api_matches_cli() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    std::fs::write(env.config_dir().join("AGENTS.md"), "Searchable rule\n").unwrap();
    let (_, cli) = env.run_json(&["--format", "json", "why", "Searchable"]);
    let mut ui = Ui::start(&env);
    let cookie = ui.session();
    let (status, _, body) = ui.request("GET", "/api/why?text=Searchable", Some(&cookie), None);
    assert_eq!(status, 200);
    let api: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(api["result"], *cli.result());
}

#[test]
fn web_toggle_routes_match_cli() {
    let env = support::MusterEnv::new();
    std::fs::create_dir_all(env.config_dir()).unwrap();
    std::fs::write(env.config_dir().join("muster.toml"), "version = 1\n").unwrap();
    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);
    let manifest = || std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();

    // Unconfirmed calls are refused like the CLI without --yes, and write nothing.
    let before = manifest();
    for path in ["/api/disable", "/api/enable"] {
        let (status, body) = ui_post(
            &mut ui,
            &cookie,
            path,
            &serde_json::json!({"item":"skill:pdf"}),
        );
        assert_eq!(status, 400, "{path}: {body}");
        assert_eq!(
            manifest(),
            before,
            "{path}: unconfirmed toggle must not write"
        );
    }
    // `project` is not accepted: the panel may not toggle arbitrary paths.
    let (status, body) = ui_post(
        &mut ui,
        &cookie,
        "/api/disable",
        &serde_json::json!({"item":"skill:pdf", "project":"/tmp", "confirm":true}),
    );
    assert_eq!(status, 400, "{body}");
    assert!(
        body["diagnostics"]
            .to_string()
            .contains("unknown parameter"),
        "{body}"
    );
    assert_eq!(manifest(), before, "project must stay hidden");

    let (status, body) = ui_post(
        &mut ui,
        &cookie,
        "/api/disable",
        &serde_json::json!({"item":"skill:pdf", "confirm":true}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["action"], "disable");
    let (status, body) = ui_post(
        &mut ui,
        &cookie,
        "/api/enable",
        &serde_json::json!({"item":"skill:pdf", "confirm":true}),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["result"]["action"], "enable");
}

#[test]
fn usage_api_matches_cli() {
    let env = support::MusterEnv::new();
    let dir = env.path().join(".claude/projects/example");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("session.jsonl"), "{\"type\":\"assistant\",\"timestamp\":\"2099-01-01T00:00:00Z\",\"sessionId\":\"session\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Skill\",\"input\":{\"skill\":\"review\"}}]}}\n").unwrap();
    let (_, cli) = env.run_json(&["--format", "json", "usage"]);
    assert_eq!(cli.result()["items"][0]["name"], "review");
    let mut ui = Ui::start(&env);
    let cookie = ui.session();
    let (status, _, body) = ui.request("GET", "/api/usage", Some(&cookie), None);
    assert_eq!(status, 200);
    let response: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(response["result"], *cli.result());
}

#[test]
fn usage_api_rejects_project_and_max_file_bytes() {
    let env = support::MusterEnv::new();
    let mut ui = Ui::start(&env);
    let cookie = ui.session();
    for query in ["project=/tmp", "max-file-bytes=1"] {
        let (status, _, body) =
            ui.request("GET", &format!("/api/usage?{query}"), Some(&cookie), None);
        assert_eq!(status, 400, "{query}: {body}");
        assert!(body.contains("unknown parameter"), "{query}: {body}");
    }
}

#[test]
fn ui_sessions_persist_across_restarts_on_the_same_port() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let cookie = ui.session();
    let (status, _, body) = ui.request("GET", "/api/status", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"));

    let port: u16 = ui.address.split(':').nth(1).unwrap().parse().unwrap();
    let _ = ui.child.kill();
    let _ = ui.child.wait();

    let mut command = Command::new(assert_cmd::cargo::cargo_bin("muster"));
    command
        .args(["ui", "--no-open", "--port", &port.to_string()])
        .env("HOME", env.home.path())
        .env("XDG_CONFIG_HOME", env.home.path().join(".config"))
        .env("XDG_STATE_HOME", env.home.path().join(".local/state"))
        .env("XDG_DATA_HOME", env.home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", env.home.path().join(".cache"))
        .env("PATH", &env.bin)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, _) in std::env::vars() {
        if key.starts_with("MUSTER_") {
            command.env_remove(key);
        }
    }
    let mut child = command.spawn().expect("restart muster ui");
    let stdout = child.stdout.take().expect("piped stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read startup line");

    let mut restarted = Ui {
        child,
        token: String::new(),
        address: format!("127.0.0.1:{port}"),
    };
    let (status, _, body) = restarted.request("GET", "/api/status", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"));
}

#[test]
fn ui_second_invocation_hands_off_fresh_login_to_running_server() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let mut ui = Ui::start(&env);
    let port: u16 = ui.address.split(':').nth(1).unwrap().parse().unwrap();

    let mut second = Command::new(assert_cmd::cargo::cargo_bin("muster"));
    second
        .args(["ui", "--no-open", "--port", &port.to_string()])
        .env("HOME", env.home.path())
        .env("XDG_CONFIG_HOME", env.home.path().join(".config"))
        .env("XDG_STATE_HOME", env.home.path().join(".local/state"))
        .env("XDG_DATA_HOME", env.home.path().join(".local/share"))
        .env("XDG_CACHE_HOME", env.home.path().join(".cache"))
        .env("PATH", &env.bin);
    for (key, _) in std::env::vars() {
        if key.starts_with("MUSTER_") {
            second.env_remove(key);
        }
    }
    let output = second.output().expect("second invocation");
    assert!(
        output.status.success(),
        "exit={:?}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let handed_token = stdout
        .split("token=")
        .nth(1)
        .expect("URL token")
        .trim()
        .to_string();

    let (status, headers, _) = ui.request("GET", &format!("/?token={handed_token}"), None, None);
    assert_eq!(status, 200);
    let cookie = headers
        .lines()
        .find(|h| h.to_ascii_lowercase().starts_with("set-cookie:"))
        .expect("Set-Cookie header on handed-off login")
        .split_once(':')
        .unwrap()
        .1
        .trim()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let (status, _, body) = ui.request("GET", "/api/status", Some(&cookie), None);
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"));

    let (status, _, _) = ui.request("GET", &format!("/?token={handed_token}"), None, None);
    assert_eq!(status, 401);
}

#[test]
fn web_skill_file_edit_and_pin_enforce_confirmation_and_confinement() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    env.run(&[
        "--yes",
        "skill",
        "add",
        "demo",
        "--description",
        "A demo skill",
    ])
    .assert()
    .success();

    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);

    // List files
    let (status, _, body) = ui.request("GET", "/api/skills/files?name=demo", Some(&cookie), None);
    assert_eq!(status, 200);
    let json: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["result"]["name"], "demo");
    assert_eq!(json["result"]["files"][0]["path"], "SKILL.md");

    // Read SKILL.md
    let (status, _, body) = ui.request(
        "GET",
        "/api/skills/file?name=demo&path=SKILL.md",
        Some(&cookie),
        None,
    );
    assert_eq!(status, 200);
    let json: Value = serde_json::from_str(&body).unwrap();
    assert!(json["result"]["content"]
        .as_str()
        .unwrap()
        .contains("name: demo"));

    // Traversal is blocked with 400, whether or not the target exists
    for path in ["../../muster.toml", "../../nonexistent-file"] {
        let (status, _, body) = ui.request(
            "GET",
            &format!("/api/skills/file?name=demo&path={path}"),
            Some(&cookie),
            None,
        );
        assert_eq!(status, 400, "{path}: {body}");
        assert!(body.contains("path escapes the skill directory"), "{body}");
    }

    // Write without confirm is refused with 400
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/file",
        &serde_json::json!({
            "name": "demo",
            "path": "SKILL.md",
            "content": "---\nname: demo\ndescription: Modified\n---\nbody\n",
        }),
    );
    assert_eq!(status, 400);
    assert_eq!(refused["ok"], false);

    // Write with confirm: true succeeds
    let (status, written) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/file",
        &serde_json::json!({
            "name": "demo",
            "path": "SKILL.md",
            "content": "---\nname: demo\ndescription: Modified\n---\nbody\n",
            "confirm": true,
        }),
    );
    assert_eq!(status, 200);
    assert_eq!(written["ok"], true);

    // Editing an own skill re-pins it, so it is not flagged modified
    let (status, _, body) = ui.request("GET", "/api/skills", Some(&cookie), None);
    assert_eq!(status, 200);
    let json: Value = serde_json::from_str(&body).unwrap();
    let skill = json["result"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "demo")
        .unwrap();
    assert_eq!(skill["modified"], false);

    // Pinning re-hashes and clears modified state
    let (status, pinned) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/pin",
        &serde_json::json!({ "name": "demo", "confirm": true }),
    );
    assert_eq!(status, 200);
    assert_eq!(pinned["ok"], true);

    let (status, _, body) = ui.request("GET", "/api/skills", Some(&cookie), None);
    assert_eq!(status, 200);
    let json: Value = serde_json::from_str(&body).unwrap();
    let skill = json["result"]["skills"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "demo")
        .unwrap();
    assert_eq!(skill["modified"], false);
}

#[cfg(unix)]
#[test]
fn web_skill_files_reject_symlink_escapes_with_400() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    env.run(&["--yes", "skill", "add", "my-skill", "--description", "d"])
        .assert()
        .success();
    let skills = env.config_dir().join("skills");
    std::os::unix::fs::symlink("/", skills.join("esc")).unwrap();
    std::os::unix::fs::symlink("/nonexistent", skills.join("my-skill/dangling")).unwrap();

    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);

    // A skill directory symlinked to `/` is refused on read and write.
    let (status, _, body) = ui.request(
        "GET",
        "/api/skills/file?name=esc&path=etc/passwd",
        Some(&cookie),
        None,
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("path escapes the skill directory"), "{body}");
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/file",
        &serde_json::json!({"name": "esc", "path": "tmp-muster-probe", "content": "x", "confirm": true}),
    );
    assert_eq!(status, 400, "{refused}");
    assert!(!std::path::Path::new("/tmp-muster-probe").exists());

    // A dangling symlink inside a skill is never written through.
    let (status, refused) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/file",
        &serde_json::json!({"name": "my-skill", "path": "dangling", "content": "x", "confirm": true}),
    );
    assert_eq!(status, 400, "{refused}");
    assert!(!std::path::Path::new("/nonexistent").exists());
}

#[test]
fn web_skill_import_matches_cli_and_rejects_path_param() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();

    let claude_skills = env.home.path().join(".claude/skills/local-tool");
    std::fs::create_dir_all(&claude_skills).unwrap();
    let skill_md = "---
name: local-tool
description: Test tool
---
Hello.
";
    std::fs::write(claude_skills.join("SKILL.md"), skill_md).unwrap();

    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);

    for key in ["path", "dir", "file", "source_path"] {
        let (status, reply) = ui_post(
            &mut ui,
            &cookie,
            "/api/skills/import",
            &serde_json::json!({
                "name": "local-tool",
                key: "/etc/passwd",
                "confirm": true,
            }),
        );
        assert_eq!(status, 400, "{key}: {reply}");
        assert!(
            reply
                .to_string()
                .contains(&format!("unknown parameter '{key}'")),
            "{key}: {reply}"
        );
    }

    let (status, reply) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/import",
        &serde_json::json!({
            "name": "local-tool",
            "confirm": true,
        }),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["ok"], true);
    assert_eq!(reply["result"]["name"], "local-tool");
    assert_eq!(reply["result"]["origin"], "local");
    assert!(
        reply["cli"]
            .as_str()
            .is_some_and(|cli| cli.contains("muster skill import local-tool")),
        "action response must include CLI equivalent: {reply}"
    );

    assert!(env.config_dir().join("skills/local-tool/SKILL.md").exists());
}

#[test]
fn web_skill_import_all_and_auto_update_routes_match_cli() {
    let env = support::MusterEnv::new();
    env.run(&["--yes", "init"]).assert().success();
    let dir = env.home.path().join(".claude/skills/local-tool");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: local-tool\ndescription: Test tool\n---\nHello.\n",
    )
    .unwrap();

    let mut ui = Ui::start(&env);
    let cookie = ui_session_cookie(&mut ui);

    let (status, reply) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/import",
        &serde_json::json!({"all": true, "dry_run": true}),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["result"]["imported"][0]["name"], "local-tool");
    assert!(!env.config_dir().join("skills/local-tool").exists());

    let (status, reply) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/import",
        &serde_json::json!({"all": true, "confirm": true}),
    );
    assert_eq!(status, 200, "{reply}");
    assert!(
        reply["cli"]
            .as_str()
            .is_some_and(|cli| cli.contains("muster skill import --all")),
        "{reply}"
    );
    assert!(env.config_dir().join("skills/local-tool/SKILL.md").exists());

    let (status, reply) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/auto-update",
        &serde_json::json!({"state": "on", "confirm": true}),
    );
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["result"]["auto_update"], true);
    assert!(
        reply["cli"]
            .as_str()
            .is_some_and(|cli| cli.contains("muster skill auto-update on")),
        "{reply}"
    );
    let manifest = std::fs::read_to_string(env.config_dir().join("muster.toml")).unwrap();
    assert!(manifest.contains("auto_update = true"), "{manifest}");

    let (status, reply) = ui_post(
        &mut ui,
        &cookie,
        "/api/skills/auto-update",
        &serde_json::json!({"state": "off", "path": "/etc", "confirm": true}),
    );
    assert_eq!(status, 400, "{reply}");
}
