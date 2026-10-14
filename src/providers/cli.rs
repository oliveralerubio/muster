//! Optional CLI-backed providers (`op`, `bws`, `doppler`, `infisical`).
//!
//! Each shells out to its vendor CLI, capturing the stdout value. If the CLI
//! is missing from PATH, resolution fails with [`ProviderErrorKind::Unavailable`]
//! (exit 4). Stderr is classified into typed provider errors (unauthorized,
//! not found, rate-limited) without ever leaking secret values.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use secrecy::zeroize::Zeroize;

use super::{Description, Provider, ProviderError, ProviderErrorKind};
use crate::errors::MusterError;
use crate::secrets::{Scheme, SecretRef, SecretValue};

pub struct CliSpec {
    pub scheme: Scheme,
    pub name: &'static str,
    pub binary: &'static str,
    pub check_args: &'static [&'static str],
    pub args: fn(&SecretRef) -> Vec<String>,
    /// Turns the CLI's stdout (one trailing newline removed) into the value.
    pub extract: fn(&SecretRef, String) -> Result<String, ProviderError>,
}

fn plain(_: &SecretRef, stdout: String) -> Result<String, ProviderError> {
    Ok(stdout)
}

pub static OP: CliSpec = CliSpec {
    scheme: Scheme::Op,
    name: "1Password",
    binary: "op",
    check_args: &["--version"],
    args: |r| vec!["read".into(), "--no-newline".into(), r.as_str().into()],
    extract: plain,
};

pub static BWS: CliSpec = CliSpec {
    scheme: Scheme::Bws,
    name: "Bitwarden Secrets Manager",
    binary: "bws",
    check_args: &["--version"],
    args: |r| {
        vec![
            "secret".into(),
            "get".into(),
            r.segments()[0].clone(),
            "--output".into(),
            "json".into(),
        ]
    },
    extract: bws_field,
};

/// `bws secret get -o json` prints the whole secret object; pick the field
/// (`value` unless the reference names `key` or `note`). Non-JSON output is
/// taken as the value itself.
fn bws_field(reference: &SecretRef, stdout: String) -> Result<String, ProviderError> {
    let field = reference.segments().get(1).map_or("value", String::as_str);
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(serde_json::Value::Object(object)) => match object.get(field) {
            Some(serde_json::Value::String(value)) => Ok(value.clone()),
            _ => Err(ProviderError::new(
                ProviderErrorKind::NotFound,
                format!("Bitwarden secret {reference} has no `{field}` field"),
            )),
        },
        _ => Ok(stdout),
    }
}

/// Every secret of a Bitwarden project (`bws secret list [PROJECT] --output
/// json`) as `(key, value)` pairs, for `muster import bws`. The CLI output is
/// zeroized; parse errors never quote it.
pub fn bws_list(project: Option<&str>) -> Result<Vec<(String, SecretValue)>, MusterError> {
    #[derive(serde::Deserialize)]
    struct Item {
        key: String,
        value: String,
    }
    let mut args = vec!["secret".to_string(), "list".to_string()];
    args.extend(project.map(str::to_string));
    args.extend(["--output".to_string(), "json".to_string()]);
    let mut stdout = Cli::new(&BWS).run(&args)?;
    let parsed = serde_json::from_str::<Vec<Item>>(&stdout);
    stdout.zeroize();
    let items = parsed.map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Unavailable,
            "bws secret list did not return a JSON array of {key, value} objects",
        )
    })?;
    Ok(items
        .into_iter()
        .map(|item| (item.key, SecretValue::new(item.value)))
        .collect())
}

pub static DOPPLER: CliSpec = CliSpec {
    scheme: Scheme::Doppler,
    name: "Doppler",
    binary: "doppler",
    check_args: &["--version"],
    args: |r| {
        let s = r.segments();
        vec![
            "secrets".into(),
            "get".into(),
            s[2].clone(),
            "--project".into(),
            s[0].clone(),
            "--config".into(),
            s[1].clone(),
            "--plain".into(),
        ]
    },
    extract: plain,
};

pub static INFISICAL: CliSpec = CliSpec {
    scheme: Scheme::Infisical,
    name: "Infisical",
    binary: "infisical",
    check_args: &["--version"],
    args: |r| {
        let s = r.segments();
        let (project, env) = (&s[0], &s[1]);
        let secret = s.last().expect("at least 3 segments").clone();
        let mut args = vec![
            "secrets".into(),
            "get".into(),
            secret,
            "--projectId".into(),
            project.clone(),
            "--env".into(),
            env.clone(),
            "--plain".into(),
        ];
        if s.len() > 3 {
            let path = format!("/{}", s[2..s.len() - 1].join("/"));
            args.extend(["--path".into(), path]);
        }
        args
    },
    extract: plain,
};

pub struct Cli {
    spec: &'static CliSpec,
}

impl Cli {
    pub fn new(spec: &'static CliSpec) -> Self {
        Self { spec }
    }

    fn which(&self) -> Result<PathBuf, ProviderError> {
        find_on_path(self.spec.binary).ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Unavailable,
                format!(
                    "{} provider requires the `{}` CLI, but it was not found on PATH",
                    self.spec.name, self.spec.binary
                ),
            )
        })
    }

    fn run(&self, args: &[String]) -> Result<String, ProviderError> {
        let exe = self.which()?;
        let mut output = Command::new(&exe)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| {
                ProviderError::new(
                    ProviderErrorKind::Unavailable,
                    format!("cannot run {}: {e}", exe.display()),
                )
            })?;
        if output.status.success() {
            let text = String::from_utf8(std::mem::take(&mut output.stdout));
            let mut text = text.map_err(|e| {
                let mut bytes = e.into_bytes();
                bytes.zeroize();
                ProviderError::new(
                    ProviderErrorKind::Unavailable,
                    format!("{} returned a value that is not UTF-8", self.spec.binary),
                )
            })?;
            // Drop a single trailing newline common in CLI output.
            if text.ends_with('\n') {
                text.pop();
                if text.ends_with('\r') {
                    text.pop();
                }
            }
            return Ok(text);
        }

        output.stdout.zeroize();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let lower = stderr.to_ascii_lowercase();
        let kind = if lower.contains("unauthorized")
            || lower.contains("not signed in")
            || lower.contains("not logged in")
            || lower.contains("authentication")
            || lower.contains("unauthenticated")
            || lower.contains("permission denied")
            || lower.contains("forbidden")
            || lower.contains("session expired")
        {
            ProviderErrorKind::Unauthorized
        } else if lower.contains("not found")
            || lower.contains("doesn't exist")
            || lower.contains("does not exist")
            || lower.contains("unknown secret")
            || lower.contains("unknown item")
        {
            ProviderErrorKind::NotFound
        } else if lower.contains("rate limit")
            || lower.contains("too many requests")
            || lower.contains("throttl")
        {
            ProviderErrorKind::RateLimited
        } else {
            ProviderErrorKind::Unavailable
        };

        let message = if stderr.is_empty() {
            format!(
                "{} CLI exited with code {:?}",
                self.spec.binary,
                output.status.code()
            )
        } else {
            // First line only, bounded: enough to diagnose, never a dump.
            let line: String = stderr
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(200)
                .collect();
            format!("{} CLI failed: {line}", self.spec.name)
        };
        Err(ProviderError::new(kind, message))
    }
}

impl Provider for Cli {
    fn describe(&self) -> Description {
        Description {
            scheme: self.spec.scheme,
            name: self.spec.name,
            builtin: false,
            binary: Some(self.spec.binary),
        }
    }

    fn check(&mut self) -> Result<(), MusterError> {
        self.which()?;
        let args: Vec<String> = self.spec.check_args.iter().map(|s| (*s).into()).collect();
        self.run(&args)?;
        Ok(())
    }

    fn resolve(&mut self, reference: &SecretRef) -> Result<SecretValue, MusterError> {
        let args = (self.spec.args)(reference);
        let stdout = self.run(&args)?;
        Ok(SecretValue::new((self.spec.extract)(reference, stdout)?))
    }
}

pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.is_file()
            && path
                .metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
