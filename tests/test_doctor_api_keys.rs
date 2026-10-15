mod support;

use std::time::Duration;
use support::{Envelope, MusterEnv};
use tiny_http::{Header, Response, Server};

// Synthetic credential; no real provider is contacted by these tests.
const KEY: &str = "sk-or-v1-doctor-test-key";

fn machine() -> MusterEnv {
    let env = MusterEnv::new();
    env.link_system_exe("git");
    env.link_system_exe("curl");
    env.run_isolated(&["--format", "json", "--yes", "init"])
        .assert()
        .code(0);
    env
}

fn run(env: &MusterEnv, endpoint: &str, flags: &[&str]) -> (std::process::Output, Envelope) {
    let mut args = vec!["--format", "json", "doctor"];
    args.extend_from_slice(flags);
    let mut command = env.run_isolated(&args);
    command
        .env("OPENROUTER_API_KEY", KEY)
        .env("MUSTER_OPENROUTER_KEY_CHECK_URL", endpoint);
    let result = support::json(command);
    for stream in [&result.0.stdout, &result.0.stderr] {
        assert!(!String::from_utf8_lossy(stream).contains(KEY), "key leaked");
    }
    result
}

fn status(envelope: &Envelope) -> &str {
    envelope.result()["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check["name"] == "api_keys")
        .unwrap()["status"]
        .as_str()
        .unwrap()
}

fn has_code(envelope: &Envelope, code: &str) -> bool {
    envelope
        .diagnostics()
        .as_array()
        .unwrap()
        .iter()
        .any(|d| d["code"] == code)
}

fn mock_check(http_status: u16, body: &str) -> (std::process::Output, Envelope) {
    let env = machine();
    let server = Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api/v1/key", server.server_addr());
    let body = body.to_owned();
    let thread = std::thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .expect("key check request");
        assert_eq!(request.method().as_str(), "GET");
        assert_eq!(request.url(), "/api/v1/key");
        assert!(
            request
                .headers()
                .iter()
                .any(|h| h.field.equiv("Authorization")
                    && h.value.as_str() == format!("Bearer {KEY}")),
            "missing authorization"
        );
        // The size guard may close the socket before a large response finishes.
        let _ = request.respond(
            Response::from_string(body)
                .with_status_code(http_status)
                .with_chunked_threshold(0),
        );
    });
    let result = run(&env, &endpoint, &["--check-keys"]);
    thread.join().unwrap();
    result
}

#[test]
fn key_checks_are_opt_in_and_skipped_offline_without_network() {
    let env = machine();
    let server = Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api/v1/key", server.server_addr());
    for flags in [vec![], vec!["--check-keys", "--offline"]] {
        let (output, envelope) = run(&env, &endpoint, &flags);
        assert!(output.status.success());
        assert_eq!(status(&envelope), "skipped");
        assert!(
            server
                .recv_timeout(Duration::from_millis(25))
                .unwrap()
                .is_none(),
            "unexpected network request"
        );
    }
}

#[test]
fn default_offline_and_missing_credentials_never_start_curl() {
    let env = machine();
    std::fs::remove_file(env.bin.join("curl")).unwrap();
    env.fake_exe("curl", ": > \"$HOME/key-check-called\"\nexit 99");
    for flags in [vec![], vec!["--check-keys", "--offline"]] {
        let (output, envelope) = run(&env, "https://openrouter.ai/api/v1/key", &flags);
        assert!(output.status.success());
        assert_eq!(status(&envelope), "skipped");
    }
    for key in [None, Some("")] {
        let mut command = env.run_isolated(&["--format", "json", "doctor", "--check-keys"]);
        command.env_remove("OPENROUTER_API_KEY");
        if let Some(key) = key {
            command.env("OPENROUTER_API_KEY", key);
        }
        let (output, envelope) = support::json(command);
        assert!(output.status.success());
        assert_eq!(status(&envelope), "skipped");
    }
    assert!(!env.path().join("key-check-called").exists());
}

#[test]
fn malformed_credentials_are_rejected_before_starting_curl() {
    let env = machine();
    std::fs::remove_file(env.bin.join("curl")).unwrap();
    env.fake_exe("curl", ": > \"$HOME/key-check-called\"\nexit 99");
    for key in [format!("{KEY}\nheader-injection"), "a".repeat(4097)] {
        let mut command = env.run_isolated(&["--format", "json", "doctor", "--check-keys"]);
        command.env("OPENROUTER_API_KEY", &key);
        let (output, envelope) = support::json(command);
        assert_eq!(output.status.code(), Some(1));
        assert!(has_code(&envelope, "api_key_invalid"));
        for stream in [&output.stdout, &output.stderr] {
            assert!(
                !String::from_utf8_lossy(stream).contains(&key),
                "key leaked"
            );
        }
    }
    assert!(!env.path().join("key-check-called").exists());
}

#[test]
fn valid_unlimited_and_future_expiring_keys_are_healthy() {
    for body in [
        r#"{"data":{"limit":100,"limit_remaining":80,"expires_at":"2999-01-01T00:00:00Z"}}"#,
        r#"{"data":{"limit":null,"limit_remaining":null,"expires_at":null}}"#,
    ] {
        let (output, envelope) = mock_check(200, body);
        assert!(output.status.success());
        assert_eq!(status(&envelope), "ok");
    }
}

#[test]
fn reports_near_limit_exhausted_expired_and_rejected_keys_without_values() {
    let cases = [
        (
            200,
            r#"{"data":{"limit":100,"limit_remaining":10}}"#,
            "api_key_near_limit",
            0,
        ),
        (
            200,
            r#"{"data":{"limit":100,"limit_remaining":0}}"#,
            "api_key_limit_reached",
            1,
        ),
        (
            200,
            r#"{"data":{"expires_at":"2000-01-01T00:00:00Z"}}"#,
            "api_key_expired",
            1,
        ),
        (401, KEY, "api_key_invalid", 1),
        (403, KEY, "api_key_invalid", 1),
        (429, KEY, "api_key_rate_limited", 0),
        (500, KEY, "api_key_check_unavailable", 0),
        (200, KEY, "api_key_check_unavailable", 0),
    ];
    for (http_status, body, code, exit) in cases {
        let (output, envelope) = mock_check(http_status, body);
        assert_eq!(output.status.code(), Some(exit));
        assert!(has_code(&envelope, code), "missing diagnostic {code}");
    }
}

#[test]
fn unreachable_endpoint_is_skipped_and_remote_overrides_are_rejected() {
    let env = machine();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api/v1/key", listener.local_addr().unwrap());
    drop(listener);
    let (output, envelope) = run(&env, &endpoint, &["--check-keys"]);
    assert!(output.status.success());
    assert_eq!(status(&envelope), "skipped");
    for endpoint in [
        "https://example.com/api/v1/key",
        "http://127.0.0.1.example.com:80/key",
    ] {
        let (output, envelope) = run(&env, endpoint, &["--check-keys"]);
        assert_eq!(output.status.code(), Some(1));
        assert!(has_code(&envelope, "api_key_endpoint_invalid"));
    }
}

#[test]
fn redirects_are_not_followed_and_curl_config_is_ignored() {
    let env = machine();
    let server = Server::http("127.0.0.1:0").unwrap();
    let redirect = Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api/v1/key", server.server_addr());
    let location = format!("http://{}/leak", redirect.server_addr());
    // A user's curl config must not enable redirects or expose headers in output.
    std::fs::write(env.path().join(".curlrc"), "location\nverbose\n").unwrap();
    let thread = std::thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .expect("key check request");
        request
            .respond(
                Response::from_string(KEY)
                    .with_status_code(302)
                    .with_header(Header::from_bytes("Location", location).unwrap()),
            )
            .unwrap();
    });
    let (output, envelope) = run(&env, &endpoint, &["--check-keys"]);
    thread.join().unwrap();
    assert!(output.status.success());
    assert!(has_code(&envelope, "api_key_check_unavailable"));
    assert!(redirect
        .recv_timeout(Duration::from_millis(25))
        .unwrap()
        .is_none());
}

#[test]
fn oversized_responses_and_missing_curl_are_skipped() {
    let (output, envelope) = mock_check(200, &KEY.repeat(1024));
    assert!(output.status.success());
    assert_eq!(status(&envelope), "skipped");

    let env = machine();
    std::fs::remove_file(env.bin.join("curl")).unwrap();
    let (output, envelope) = run(&env, "https://openrouter.ai/api/v1/key", &["--check-keys"]);
    assert!(output.status.success());
    assert_eq!(status(&envelope), "skipped");
}

#[test]
fn slow_provider_is_skipped_within_the_total_timeout() {
    let env = machine();
    let server = Server::http("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api/v1/key", server.server_addr());
    let thread = std::thread::spawn(move || {
        let request = server
            .recv_timeout(Duration::from_secs(8))
            .unwrap()
            .unwrap();
        // Keep the connection open until the client times out.
        std::thread::sleep(Duration::from_secs(6));
        drop(request);
    });
    let start = std::time::Instant::now();
    let (output, envelope) = run(&env, &endpoint, &["--check-keys"]);
    assert!(start.elapsed() < Duration::from_secs(8));
    assert!(output.status.success());
    assert_eq!(status(&envelope), "skipped");
    thread.join().unwrap();
}
