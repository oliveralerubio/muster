//! Vendor CLIs run as fake executables on an isolated PATH, without credentials
//! or network access. Exercise the public CLI and doctor diagnostics.
mod support;

use std::process::Output;
use support::{Envelope, MusterEnv};

const VALUE: &str = "fake-provider-value-71d2\nsecond line\n";
const PROVIDERS: [(&str, &str); 2] = [
    ("doppler", "doppler://backend/prd/API_KEY"),
    ("infisical", "infisical://project-id/prod/API_KEY"),
];

fn machine() -> MusterEnv {
    let env = MusterEnv::new();
    env.link_system_exe("git");
    env.run_isolated(&["--format", "json", "init", "--yes"])
        .assert()
        .success();
    env
}

fn run(env: &MusterEnv, args: &[&str]) -> (Output, Envelope) {
    let mut cmd = env.run_isolated(args);
    cmd.env("TEST_PROVIDER_VALUE", VALUE).env(
        "TEST_PROVIDER_JSON",
        serde_json::json!([{ "secretKey": "API_KEY", "secretValue": VALUE }]).to_string(),
    );
    support::json(cmd)
}

fn assert_private(output: &Output) {
    for bytes in [&output.stdout, &output.stderr] {
        assert!(!String::from_utf8_lossy(bytes).contains("fake-provider-value-71d2"));
    }
}

fn assert_no_persisted_values(path: &std::path::Path) {
    for entry in std::fs::read_dir(path).expect("read temporary machine") {
        let path = entry.unwrap().path();
        if path.is_symlink() {
            continue;
        }
        if path.is_dir() {
            assert_no_persisted_values(&path);
        } else {
            let data = std::fs::read(path).expect("read temporary file");
            assert!(!String::from_utf8_lossy(&data).contains("fake-provider-value-71d2"));
        }
    }
}

#[test]
fn resolves_single_secrets_with_explicit_scope_and_no_logging_or_history() {
    let cases = [
        (
            "doppler",
            "doppler://backend/prd/API_KEY",
            "secrets get API_KEY --project backend --config prd --plain",
            "printf '%s\\n' \"$TEST_PROVIDER_VALUE\"",
        ),
        (
            "infisical",
            "infisical://project-id/prod/API_KEY",
            "sm secrets get API_KEY --projectId project-id --env prod --path / --output json --silent",
            r#"printf '%s' "$TEST_PROVIDER_JSON""#,
        ),
        (
            "infisical",
            "infisical://project-id/prod/team/backend/API_KEY",
            "sm secrets get API_KEY --projectId project-id --env prod --path /team/backend --output json --silent",
            r#"printf '%s' "$TEST_PROVIDER_JSON""#,
        ),
    ];
    for (binary, reference, expected, value_script) in cases {
        let env = machine();
        // Compare every argument, including its boundary. Values and tokens
        // cannot accidentally become arguments without failing this shim.
        let args = expected
            .split(' ')
            .map(|arg| format!("[ \"$1\" = '{arg}' ] || exit 91\nshift\n"))
            .collect::<String>();
        env.fake_exe(
            binary,
            &format!("{args}[ $# -eq 0 ] || exit 92\n{value_script}"),
        );
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert!(out.status.success(), "{}", envelope.raw);
        assert_eq!(envelope.result()["resolved"], true);
        assert_private(&out);

        // Explicit reveal is the sole opt-in value output; validate exact
        // multiline bytes, including a trailing newline, without logging them.
        let (out, envelope) = run(
            &env,
            &[
                "--format", "json", "secret", "get", reference, "--reveal", "--yes",
            ],
        );
        assert!(out.status.success());
        assert!(envelope.result().as_str() == Some(VALUE));
        assert!(out.stderr.is_empty());
        assert_no_persisted_values(&env.path());
        let history = std::process::Command::new("git")
            .arg("-C")
            .arg(env.config_dir())
            .args(["log", "--all", "-p"])
            .output()
            .expect("git history");
        assert!(history.status.success());
        assert_private(&history);
    }
}

#[test]
fn missing_clis_report_installation_errors() {
    for (binary, reference) in PROVIDERS {
        let env = machine();
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(envelope.diagnostics()[0]["code"], "provider_unavailable");
        let message = envelope.diagnostics()[0]["message"].as_str().unwrap();
        assert!(message.contains(binary) && message.contains("PATH"));
        assert_private(&out);
    }
}

#[test]
fn vendor_failures_are_typed_and_never_echo_captured_values() {
    let failures = [
        ("unauthorized", "provider_unauthorized", 5),
        ("you must provide a token", "provider_unauthorized", 5),
        (
            "invalid service token entered. Please double check your service token and try again",
            "provider_unauthorized",
            5,
        ),
        (
            "You must be logged in to run this command. To login, run [infisical login]",
            "provider_unauthorized",
            5,
        ),
        ("Your login session has expired", "provider_unauthorized", 5),
        ("secret not found", "secret_not_found", 1),
        (
            "Could not find requested secret: API_KEY",
            "secret_not_found",
            1,
        ),
        ("rate limit exceeded", "provider_rate_limited", 4),
        ("unexpected failure", "provider_unavailable", 4),
    ];
    for (binary, reference) in PROVIDERS {
        let env = machine();
        for (reason, code, exit) in failures {
            env.fake_exe(binary, &format!("printf '%s' \"$TEST_PROVIDER_VALUE\"\nprintf '%s: %s' '{reason}' \"$TEST_PROVIDER_VALUE\" >&2\nexit 1"));
            let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
            assert_eq!(out.status.code(), Some(exit), "{}", envelope.raw);
            assert_eq!(envelope.diagnostics()[0]["code"], code);
            if exit == 5 {
                assert!(envelope.diagnostics()[0]["message"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("{binary} login")));
            }
            assert_private(&out);
        }
        assert_no_persisted_values(&env.path());
    }
}

#[test]
fn successful_stderr_and_invalid_utf8_never_leak_values() {
    for (binary, reference) in PROVIDERS {
        let env = machine();
        let response = if binary == "infisical" {
            r#"printf '%s' "$TEST_PROVIDER_JSON""#
        } else {
            "printf '%s\\n' \"$TEST_PROVIDER_VALUE\""
        };
        env.fake_exe(
            binary,
            &format!("printf '%s' \"$TEST_PROVIDER_VALUE\" >&2\n{response}"),
        );
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert!(out.status.success());
        assert_eq!(envelope.result()["resolved"], true);
        assert_private(&out);

        env.fake_exe(
            binary,
            "printf '%s' \"$TEST_PROVIDER_VALUE\"\nprintf '\\377'",
        );
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert_eq!(out.status.code(), Some(4));
        assert_eq!(envelope.diagnostics()[0]["code"], "provider_unavailable");
        assert_private(&out);
    }
}

#[test]
fn existing_cli_provider_errors_also_redact_vendor_output() {
    for (binary, reference, hint) in [
        ("op", "op://vault/item/field", "op signin"),
        ("bws", "bws://item/value", "BWS_ACCESS_TOKEN"),
    ] {
        let env = machine();
        env.fake_exe(
            binary,
            "printf '%s' \"$TEST_PROVIDER_VALUE\"\nprintf 'unauthorized: %s' \"$TEST_PROVIDER_VALUE\" >&2\nexit 1",
        );
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert_eq!(out.status.code(), Some(5));
        assert_eq!(envelope.diagnostics()[0]["code"], "provider_unauthorized");
        assert!(envelope.diagnostics()[0]["message"]
            .as_str()
            .unwrap()
            .contains(hint));
        assert_private(&out);
    }
}

#[test]
fn infisical_distinguishes_missing_empty_and_invalid_responses() {
    let env = machine();
    let reference = PROVIDERS[1].1;
    for response in [
        "[]",
        r#"[{"secretKey":"API_KEY","secretValue":"*not found*"}]"#,
        r#"[{"secretKey":"OTHER_KEY","secretValue":"fake-provider-value-71d2"}]"#,
    ] {
        env.fake_exe("infisical", &format!("printf '%s' '{response}'"));
        let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
        assert_eq!(out.status.code(), Some(1));
        assert_eq!(envelope.diagnostics()[0]["code"], "secret_not_found");
        assert_private(&out);
    }
    env.fake_exe(
        "infisical",
        r#"printf '%s' '[{"secretKey":"API_KEY","secretValue":""}]'"#,
    );
    let (out, envelope) = run(
        &env,
        &[
            "--format", "json", "secret", "get", reference, "--reveal", "--yes",
        ],
    );
    assert!(out.status.success());
    assert_eq!(envelope.result(), "");
    env.fake_exe("infisical", "printf '%s' \"$TEST_PROVIDER_VALUE\"");
    let (out, envelope) = run(&env, &["--format", "json", "secret", "get", reference]);
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(envelope.diagnostics()[0]["code"], "provider_unavailable");
    assert_private(&out);
}

#[test]
fn doctor_reports_optional_provider_health_and_safe_remediation() {
    for (binary, reference) in PROVIDERS {
        let env = machine();
        let manifest = env.config_dir().join("muster.toml");
        let mut text = std::fs::read_to_string(&manifest).unwrap();
        text.push_str(&format!(
            "\n[mcp.tools]\ncommand = \"tools\"\n[mcp.tools.env]\nAPI_KEY = \"{reference}\"\n"
        ));
        std::fs::write(manifest, text).unwrap();
        let (out, envelope) = run(&env, &["--format", "json", "doctor"]);
        let find = |envelope: &Envelope, code| {
            envelope
                .diagnostics()
                .as_array()
                .unwrap()
                .iter()
                .find(|d| d["code"] == code)
                .cloned()
                .expect("provider diagnostic")
        };
        let missing = find(&envelope, "provider_unavailable");
        assert!(missing["suggestion"].as_str().unwrap().contains(binary));
        assert_private(&out);

        env.fake_exe(binary, "if [ \"$1\" = --version ]; then echo 1.0.0; exit 0; fi\nprintf 'You must be logged in: %s' \"$TEST_PROVIDER_VALUE\" >&2\nexit 1");
        let (out, envelope) = run(&env, &["--format", "json", "doctor"]);
        let unauthorized = find(&envelope, "provider_unauthorized");
        assert!(unauthorized["suggestion"]
            .as_str()
            .unwrap()
            .contains("sign in"));
        assert_private(&out);

        let response = if binary == "infisical" {
            r#"printf '%s' "$TEST_PROVIDER_JSON""#
        } else {
            "printf '%s' \"$TEST_PROVIDER_VALUE\""
        };
        env.fake_exe(
            binary,
            &format!("if [ \"$1\" = --version ]; then echo 1.0.0; exit 0; fi\n{response}"),
        );
        let (out, envelope) = run(&env, &["--format", "json", "doctor"]);
        assert!(envelope
            .diagnostics()
            .as_array()
            .unwrap()
            .iter()
            .all(|d| !matches!(
                d["code"].as_str(),
                Some("provider_unavailable" | "provider_unauthorized" | "secret_not_found")
            )));
        assert_private(&out);
        assert_no_persisted_values(&env.path());
    }
}
