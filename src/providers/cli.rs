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
fn bws_field(reference: &SecretRef, mut stdout: String) -> Result<String, ProviderError> {
    let field = reference.segments().get(1).map_or("value", String::as_str);
    match serde_json::from_str::<serde_json::Value>(&stdout) {
        Ok(serde_json::Value::Object(mut object)) => {
            // The parsed copy is moved, never cloned; every other string and
            // the raw CLI output are wiped.
            stdout.zeroize();
            let found = object.remove(field);
            object.into_iter().for_each(|(_, v)| wipe(v));
            match found {
                Some(serde_json::Value::String(value)) => Ok(value),
                other => {
                    other.into_iter().for_each(wipe);
                    Err(ProviderError::new(
                        ProviderErrorKind::NotFound,
                        format!("Bitwarden secret {reference} has no `{field}` field"),
                    ))
                }
            }
        }
        _ => Ok(stdout),
    }
}

/// Zeroize every string inside a parsed JSON value before it is dropped.
fn wipe(value: serde_json::Value) {
    match value {
        serde_json::Value::String(mut s) => s.zeroize(),
        serde_json::Value::Array(items) => items.into_iter().for_each(wipe),
        serde_json::Value::Object(map) => map.into_iter().for_each(|(_, v)| wipe(v)),
        _ => {}
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
    if let Some(project) = project {
        validate_bws_project(project)?;
    }
    let mut args = vec![
        "secret".to_string(),
        "list".to_string(),
        "--output".to_string(),
        "json".to_string(),
    ];
    if let Some(project) = project {
        args.extend(["--".to_string(), project.to_string()]);
    }
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

pub fn validate_bws_project(project: &str) -> Result<(), MusterError> {
    if project.starts_with('-')
        || project.len() > 64
        || project.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(MusterError::Usage("invalid Bitwarden project ID".into()));
    }
    Ok(())
}

/// `doppler://<project>/<config>/<NAME>` selects exactly one config secret.
/// CLI flags: https://github.com/DopplerHQ/cli/blob/master/pkg/cmd/secrets.go
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

/// `infisical://<project-id>/<environment>/[<folder>/...]<NAME>`.
/// No folder means `/`, regardless of local CLI configuration. Credentials
/// come from the vendor login or environment, never command-line arguments.
/// https://infisical.com/docs/cli/reference#sm-secrets-get
pub static INFISICAL: CliSpec = CliSpec {
    scheme: Scheme::Infisical,
    name: "Infisical",
    binary: "infisical",
    check_args: &["--version"],
    args: |r| {
        let s = r.segments();
        let (project, env) = (&s[0], &s[1]);
        let secret = s.last().expect("at least 3 segments").clone();
        let path = format!("/{}", s[2..s.len() - 1].join("/"));
        vec![
            "sm".into(),
            "secrets".into(),
            "get".into(),
            secret,
            "--projectId".into(),
            project.clone(),
            "--env".into(),
            env.clone(),
            "--path".into(),
            path,
            "--output".into(),
            "json".into(),
            "--silent".into(),
        ]
    },
    extract: infisical_value,
};

/// JSON preserves empty/multiline values. The CLI represents missing names
/// with the reserved value `*not found*`; plain output silently omits them.
fn infisical_value(reference: &SecretRef, mut stdout: String) -> Result<String, ProviderError> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Secret {
        secret_key: String,
        secret_value: String,
    }
    let parsed = serde_json::from_str::<Vec<Secret>>(&stdout);
    stdout.zeroize();
    let secrets = parsed.map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Unavailable,
            "Infisical CLI did not return a JSON array of secretKey/secretValue objects",
        )
    })?;
    let name = reference.segments().last().expect("validated secret name");
    let mut value = None;
    for mut secret in secrets {
        if &secret.secret_key == name && secret.secret_value != "*not found*" && value.is_none() {
            value = Some(std::mem::take(&mut secret.secret_value));
        }
        secret.secret_value.zeroize();
    }
    value.ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::NotFound,
            "Infisical secret was not found",
        )
    })
}

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
            output.stderr.zeroize();
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
        let mut lower = String::from_utf8_lossy(&output.stderr).to_ascii_lowercase();
        let kind = if lower.contains("unauthorized")
            || lower.contains("not signed in")
            || lower.contains("not logged in")
            || lower.contains("authentication")
            || lower.contains("unauthenticated")
            || lower.contains("permission denied")
            || lower.contains("forbidden")
            || lower.contains("session expired")
            || lower.contains("session has expired")
            || lower.contains("must be logged in")
            || lower.contains("unable to authenticate")
            || lower.contains("you must provide a token")
            || lower.contains("invalid service token")
        {
            ProviderErrorKind::Unauthorized
        } else if lower.contains("not found")
            || lower.contains("could not find requested secret")
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

        output.stderr.zeroize();
        lower.zeroize();
        // Vendor errors can echo tokens or secret values, even on the first
        // line. Report only our classification, never any captured output.
        let authentication = match self.spec.scheme {
            Scheme::Op => "run `op signin` or check its credentials and permissions",
            Scheme::Bws => "check BWS_ACCESS_TOKEN and its permissions",
            Scheme::Doppler => "run `doppler login` or check its credentials and permissions",
            Scheme::Infisical => "run `infisical login` or check its credentials and permissions",
            _ => "check its credentials and permissions",
        };
        let message = match kind {
            ProviderErrorKind::Unauthorized => format!(
                "{} CLI is not authenticated or access was denied; {authentication}",
                self.spec.name
            ),
            ProviderErrorKind::NotFound => format!(
                "{} secret was not found; check the reference",
                self.spec.name
            ),
            ProviderErrorKind::RateLimited => {
                format!("{} CLI was rate limited; try again later", self.spec.name)
            }
            _ => format!(
                "{} CLI exited with code {:?}",
                self.spec.binary,
                output.status.code()
            ),
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

#[cfg(test)]
mod project_argument_security_tests {
    use super::validate_bws_project;

    #[test]
    fn rejects_options_and_control_characters() {
        assert!(validate_bws_project("project-123").is_ok());
        assert!(validate_bws_project("--help").is_err());
        assert!(validate_bws_project("project\nother").is_err());
        assert!(validate_bws_project("project\0other").is_err());
    }
}
