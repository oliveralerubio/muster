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
    let header = SecretValue::new(format!(
        "header = \"Authorization: Bearer {}\"\n",
        key.expose()
    ));
    match request(&header, &endpoint, local, false) {
        Some(response) => inspect(&response),
        None => Check::skipped("api_keys"),
    }
}

/// Credential headers travel through stdin, never argv. No redirects, curl
/// config files or caller-selected remote destinations are allowed.
fn request(
    header: &SecretValue,
    endpoint: &str,
    local: bool,
    discard_body: bool,
) -> Option<Zeroizing<Vec<u8>>> {
    let curl = crate::machine::find_executable("curl")?;
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
        if discard_body { "1048576" } else { "16384" },
        "--proto",
        if local { "=http" } else { "=https" },
        "--write-out",
        "\n%{http_code}",
        "--config",
        "-",
        "--url",
        endpoint,
    ]);
    if local {
        command.args(["--noproxy", "*"]);
    }
    if discard_body {
        // Model catalogues can exceed the metadata response bound. Only
        // the HTTP status is relevant, so curl discards the bounded body.
        command.args(["--output", if cfg!(windows) { "NUL" } else { "/dev/null" }]);
    }
    let Ok(mut child) = command
        .env_remove("OPENROUTER_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return None;
    };
    // Send the authorization header through stdin, never argv or a file.
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
        return None;
    }
    let Ok(status) = child.wait() else {
        return None;
    };
    if !status.success() {
        return None;
    }
    Some(response)
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

/// Cheap documented metadata requests to official endpoints only. Custom
/// gateways are not probed: sending their credentials elsewhere is unsafe.
pub(super) fn check_declared(ctx: &crate::context::Context, enabled: bool, offline: bool) -> Check {
    if !enabled || offline {
        return Check::skipped("provider_api_keys");
    }
    let Ok(manifest) = crate::manifest::Manifest::load(&ctx.config_dir) else {
        return Check::skipped("provider_api_keys");
    };
    let merged = crate::engine::layer::merge(&crate::engine::layer::global_layers(
        &ctx.config_dir,
        &manifest,
        crate::machine::load_name(&ctx.state_dir).as_deref(),
    ));
    let mut secrets = crate::providers::Providers::new(&ctx.config_dir, &ctx.state_dir);
    let mut diagnostics = Vec::new();
    for (id, provider) in &merged.providers {
        let Some(reference) = &provider.key else {
            continue;
        };
        use crate::adapters::config::ProviderKind;
        let (base, endpoint, header_name, bearer) = match provider.kind {
            ProviderKind::Openai => (
                "https://api.openai.com/v1",
                "https://api.openai.com/v1/models",
                "Authorization",
                "Bearer ",
            ),
            ProviderKind::Anthropic => (
                "https://api.anthropic.com",
                "https://api.anthropic.com/v1/models",
                "x-api-key",
                "",
            ),
            ProviderKind::Google => (
                "https://generativelanguage.googleapis.com/v1beta",
                "https://generativelanguage.googleapis.com/v1beta/models",
                "x-goog-api-key",
                "",
            ),
            _ => continue,
        };
        if provider
            .base_url
            .as_ref()
            .is_some_and(|url| url.trim_end_matches('/') != base)
        {
            continue;
        }
        let Ok(key) = secrets.resolve(reference) else {
            continue;
        };
        if key.expose().is_empty()
            || key.expose().len() > 4096
            || !key
                .expose()
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
        {
            diagnostics.push(
                Diagnostic::error(
                    "api_key_invalid",
                    format!("provider `{id}` key has an invalid token format"),
                )
                .with_path(format!("providers.{id}")),
            );
            continue;
        }
        let mut header = format!("header = \"{header_name}: {bearer}{}\"\n", key.expose());
        if provider.kind == ProviderKind::Anthropic {
            header.push_str("header = \"anthropic-version: 2023-06-01\"\n");
        }
        let response = request(&SecretValue::new(header), endpoint, false, true);
        let status = response.as_deref().and_then(|bytes| {
            bytes
                .iter()
                .rposition(|b| *b == b'\n')
                .map(|index| &bytes[index + 1..])
        });
        let (severity, code, message) = match status {
            Some(b"200") => continue,
            Some(b"401" | b"403") => (Severity::Error, "api_key_invalid", "rejected its key"),
            Some(b"429") => (
                Severity::Warning,
                "api_key_rate_limited",
                "key check was rate limited",
            ),
            _ => (
                Severity::Warning,
                "api_key_check_unavailable",
                "did not return usable key metadata",
            ),
        };
        diagnostics.push(
            Diagnostic::new(severity, code, format!("provider `{id}` {message}"))
                .with_path(format!("providers.{id}")),
        );
    }
    Check::new("provider_api_keys", diagnostics)
}
