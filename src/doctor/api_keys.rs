//! Opt-in, read-only OpenRouter key metadata. See docs/security/network.md.

use std::io::{Read, Write};
use std::net::SocketAddr;
use std::process::{Command, Stdio};

use secrecy::zeroize::Zeroizing;
use serde::Deserialize;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use super::Check;
use crate::output::{Diagnostic, Severity};
use crate::secrets::SecretValue;

const ENDPOINT: &str = "https://openrouter.ai/api/v1/key";
// 16 KiB body plus curl's appended newline and three-digit HTTP status.
const MAX_RESPONSE_BYTES: u64 = 16_384 + 4;

pub(super) fn check(enabled: bool, offline: bool) -> Check {
    if !enabled || offline {
        return Check::skipped("api_keys");
    }
    let Some(key) = std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .map(SecretValue::new)
    else {
        return Check::skipped("api_keys");
    };
    // Restrict the header to token characters; reject config/header injection.
    if key.expose().len() > 4096
        || !key
            .expose()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    {
        return Check::new(
            "api_keys",
            vec![Diagnostic::error(
                "api_key_invalid",
                "OpenRouter key has an invalid token format",
            )
            .with_suggestion("replace OPENROUTER_API_KEY with a valid key")],
        );
    }
    let endpoint =
        std::env::var("MUSTER_OPENROUTER_KEY_CHECK_URL").unwrap_or_else(|_| ENDPOINT.to_owned());
    let local = endpoint
        .strip_prefix("http://")
        .and_then(|rest| rest.split_once('/'))
        .and_then(|(authority, _)| authority.parse::<SocketAddr>().ok())
        .is_some_and(|address| address.ip().is_loopback());
    if endpoint != ENDPOINT && !local {
        return Check::new(
            "api_keys",
            vec![Diagnostic::error(
                "api_key_endpoint_invalid",
                "Key checks require the official endpoint or an HTTP loopback test endpoint",
            )
            .with_suggestion("unset MUSTER_OPENROUTER_KEY_CHECK_URL")],
        );
    }
    let Some(curl) = crate::machine::find_executable("curl") else {
        return Check::skipped("api_keys");
    };
    let mut command = Command::new(curl);
    command.args([
        "--disable",
        "--silent",
        "--globoff",
        "--connect-timeout",
        "2",
        "--max-time",
        "5",
        "--max-filesize",
        "16384",
        "--proto",
        if local { "=http" } else { "=https" },
        "--write-out",
        "\n%{http_code}",
        "--config",
        "-",
        "--url",
        &endpoint,
    ]);
    if local {
        command.args(["--noproxy", "*"]);
    }
    let Ok(mut child) = command
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return Check::skipped("api_keys");
    };
    // Send the authorization header through stdin, never argv or a file.
    let header = SecretValue::new(format!(
        "header = \"Authorization: Bearer {}\"\n",
        key.expose()
    ));
    let written = child
        .stdin
        .take()
        .unwrap()
        .write_all(header.expose().as_bytes());
    // Bound the pipe too: older curl versions only enforce --max-filesize
    // when Content-Length is known, so a chunked response could bypass it.
    let mut response = Zeroizing::new(Vec::new());
    let read = child
        .stdout
        .take()
        .unwrap()
        .take(MAX_RESPONSE_BYTES + 1)
        .read_to_end(&mut response);
    if written.is_err() || read.is_err() || response.len() as u64 > MAX_RESPONSE_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        return Check::skipped("api_keys");
    }
    let Ok(status) = child.wait() else {
        return Check::skipped("api_keys");
    };
    if !status.success() {
        return Check::skipped("api_keys");
    }
    inspect(&response)
}

#[derive(Deserialize)]
struct Response {
    data: KeyMetadata,
}

#[derive(Deserialize)]
struct KeyMetadata {
    limit: Option<f64>,
    limit_remaining: Option<f64>,
    expires_at: Option<String>,
}

fn inspect(bytes: &[u8]) -> Check {
    let Some(separator) = bytes.iter().rposition(|b| *b == b'\n') else {
        return unavailable();
    };
    let (body, status) = (&bytes[..separator], &bytes[separator + 1..]);
    if status == b"401" || status == b"403" {
        return Check::new(
            "api_keys",
            vec![Diagnostic::error(
                "api_key_invalid",
                "OpenRouter rejected the key (invalid, revoked or expired)",
            )
            .with_suggestion("replace OPENROUTER_API_KEY with an active key")],
        );
    }
    if status == b"429" {
        return Check::new(
            "api_keys",
            vec![Diagnostic::new(
                Severity::Warning,
                "api_key_rate_limited",
                "OpenRouter key check was rate limited",
            )
            .with_suggestion("wait before checking the key again")],
        );
    }
    if status != b"200" {
        return unavailable();
    }
    let Ok(response) = serde_json::from_slice::<Response>(body) else {
        return unavailable();
    };
    let mut diags = Vec::new();
    if let Some(expiry) = &response.data.expires_at {
        let Ok(expiry) = OffsetDateTime::parse(expiry, &Rfc3339) else {
            return unavailable();
        };
        if expiry <= OffsetDateTime::now_utc() {
            diags.push(
                Diagnostic::error("api_key_expired", "OpenRouter key has expired")
                    .with_suggestion("replace OPENROUTER_API_KEY with an active key"),
            );
        }
    }
    if let (Some(limit), Some(remaining)) = (response.data.limit, response.data.limit_remaining) {
        if remaining <= 0.0 {
            diags.push(
                Diagnostic::error(
                    "api_key_limit_reached",
                    "OpenRouter key spending limit is exhausted",
                )
                .with_suggestion("review the key spending limit in OpenRouter"),
            );
        } else if limit > 0.0 && remaining <= limit * 0.1 {
            diags.push(
                Diagnostic::new(
                    Severity::Warning,
                    "api_key_near_limit",
                    "OpenRouter key has at most 10% of its spending limit remaining",
                )
                .with_suggestion("review the key spending limit in OpenRouter"),
            );
        }
    }
    Check::new("api_keys", diags)
}

fn unavailable() -> Check {
    Check::new(
        "api_keys",
        vec![Diagnostic::new(
            Severity::Warning,
            "api_key_check_unavailable",
            "OpenRouter did not return usable key metadata",
        )
        .with_suggestion("retry later or use --offline to skip the optional check")],
    )
}
