//! Read a Cuartel directory (the tool Muster grew out of) into manifest
//! sections. Reads only; the caller merges and writes.
//!
//! Layout understood:
//! - `muster.toml` or `cuartel.toml`: `[agents]` and `[mcp.*]` tables. Other
//!   top-level keys (hosts, IDs, personal paths...) are reported, never copied.
//! - `AGENTS.md` (or `agents/AGENTS.md`): shared instructions.
//! - `mcp.json` / `.mcp.json`: `{"mcpServers": {name: {command, args, env,
//!   url, headers}}}`.
//!
//! MCP `env`/`headers` values: secret references stay; `${VAR}` / `$VAR`
//! become `env://VAR`; a literal under a secret-looking key is dropped (it
//! would land in git) with a warning to move it into the vault.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::adapters::config::{AgentsConfig, McpServer};
use crate::errors::MusterError;
use crate::output::{Diagnostic, Severity};
use crate::secrets;

#[derive(Default)]
pub struct Proposal {
    pub ai_providers: BTreeMap<String, crate::adapters::config::AiProvider>,
    pub agents: Option<AgentsConfig>,
    pub mcp: BTreeMap<String, McpServer>,
    pub instructions: Option<String>,
    pub diagnostics: Vec<Diagnostic>,
}

pub fn scan(dir: &Path) -> Result<Proposal, MusterError> {
    if !dir.is_dir() {
        return Err(MusterError::Usage(format!(
            "Cuartel directory {} does not exist",
            dir.display()
        )));
    }
    let mut proposal = Proposal::default();
    let mut found = false;

    for name in ["muster.toml", "cuartel.toml"] {
        let path = dir.join(name);
        if let Some(text) = read(&path)? {
            found = true;
            read_manifest(&path, &text, &mut proposal)?;
            break;
        }
    }
    for name in ["AGENTS.md", "agents/AGENTS.md"] {
        if let Some(text) = read(&dir.join(name))? {
            found = true;
            proposal.instructions = Some(text);
            break;
        }
    }
    for name in ["mcp.json", ".mcp.json"] {
        let path = dir.join(name);
        if let Some(text) = read(&path)? {
            found = true;
            read_mcp_json(&path, &text, &mut proposal)?;
        }
    }
    proposal.ai_providers = super::ai_providers::scan(dir)?;
    found |= !proposal.ai_providers.is_empty();
    if !found {
        return Err(MusterError::Usage(format!(
            "{} has no Cuartel configuration (muster.toml, AGENTS.md or mcp.json)",
            dir.display()
        )));
    }

    let mut mcp = std::mem::take(&mut proposal.mcp);
    for (name, server) in &mut mcp {
        sanitize(name, "env", &mut server.env, &mut proposal.diagnostics);
        sanitize(
            name,
            "headers",
            &mut server.headers,
            &mut proposal.diagnostics,
        );
    }
    proposal.mcp = mcp;
    Ok(proposal)
}

fn read_manifest(path: &Path, text: &str, proposal: &mut Proposal) -> Result<(), MusterError> {
    let invalid = |message: String| MusterError::InvalidConfig {
        message,
        path: Some(path.display().to_string()),
    };
    let table: toml::Table = text
        .parse()
        .map_err(|e| invalid(format!("not valid TOML: {e}")))?;
    for (key, value) in table {
        match key.as_str() {
            "version" => {}
            "agents" => {
                let agents: AgentsConfig = value
                    .try_into()
                    .map_err(|e| invalid(format!("[agents]: {e}")))?;
                proposal.agents = Some(agents);
            }
            "mcp" => {
                let mcp: BTreeMap<String, McpServer> = value
                    .try_into()
                    .map_err(|e| invalid(format!("[mcp]: {e}")))?;
                proposal.mcp.extend(mcp);
            }
            other => proposal.diagnostics.push(
                Diagnostic::new(
                    Severity::Info,
                    "import_key_ignored",
                    format!(
                        "`{other}` is not imported; personal values belong in your own \
                         manifest, add them by hand if you still need them"
                    ),
                )
                .with_path(path.display().to_string()),
            ),
        }
    }
    Ok(())
}

fn read_mcp_json(path: &Path, text: &str, proposal: &mut Proposal) -> Result<(), MusterError> {
    let invalid = |message: String| MusterError::InvalidConfig {
        message,
        path: Some(path.display().to_string()),
    };
    let value: Value =
        serde_json::from_str(text).map_err(|e| invalid(format!("not valid JSON: {e}")))?;
    let Some(servers) = value.get("mcpServers").and_then(Value::as_object) else {
        return Err(invalid("expected an `mcpServers` object".into()));
    };
    for (name, server) in servers {
        let string = |key: &str| server.get(key).and_then(Value::as_str).map(str::to_string);
        let map = |key: &str| -> BTreeMap<String, String> {
            server
                .get(key)
                .and_then(Value::as_object)
                .map(|o| {
                    o.iter()
                        .filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string())))
                        .collect()
                })
                .unwrap_or_default()
        };
        let args = server
            .get("args")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let parsed = McpServer {
            command: string("command"),
            args,
            env: map("env"),
            url: string("url"),
            headers: map("headers"),
        };
        if let Err(message) = parsed.transport() {
            return Err(invalid(format!("MCP server `{name}` {message}")));
        }
        proposal.mcp.insert(name.clone(), parsed);
    }
    Ok(())
}

/// Keep references, turn `${VAR}` into `env://VAR`, drop literal secrets.
fn sanitize(
    server: &str,
    field: &str,
    values: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    values.retain(|key, value| {
        if secrets::is_reference(value) {
            return true;
        }
        if let Some(var) = env_var(value) {
            *value = format!("env://{var}");
            return true;
        }
        if !looks_secret(key) {
            return true;
        }
        diagnostics.push(
            Diagnostic::new(
                Severity::Warning,
                "literal_secret_dropped",
                format!(
                    "MCP server `{server}` {field} `{key}` holds a literal value; it was not \
                     imported. Store it with `muster secret add <group>/<name> --value-stdin` \
                     and reference it as native://<group>/<name>"
                ),
            )
            .with_path(format!("mcp.{server}.{field}.{key}")),
        );
        false
    });
}

fn env_var(value: &str) -> Option<&str> {
    let name = value
        .strip_prefix("${")
        .and_then(|v| v.strip_suffix('}'))
        .or_else(|| value.strip_prefix('$'))?;
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    valid.then_some(name)
}

fn looks_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "AUTHORIZATION",
        "CREDENTIAL",
        "API_KEY",
        "APIKEY",
        "PRIVATE",
    ]
    .iter()
    .any(|word| key.contains(word))
        || key == "KEY"
        || key.ends_with("_KEY")
}

fn read(path: &Path) -> Result<Option<String>, MusterError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(MusterError::Operational(format!("{}: {e}", path.display()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_values_keep_references_and_drop_literal_secrets() {
        let mut values: BTreeMap<String, String> = [
            ("GITHUB_TOKEN", "ghp_literal"),
            ("API_KEY", "native://bws/API_KEY"),
            ("HOME_DIR", "${HOME}"),
            ("LOG_LEVEL", "debug"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let mut diagnostics = Vec::new();
        sanitize("github", "env", &mut values, &mut diagnostics);
        assert_eq!(values.get("GITHUB_TOKEN"), None);
        assert_eq!(values["API_KEY"], "native://bws/API_KEY");
        assert_eq!(values["HOME_DIR"], "env://HOME");
        assert_eq!(values["LOG_LEVEL"], "debug");
        assert_eq!(diagnostics.len(), 1);
        assert!(!diagnostics[0].message.contains("ghp_literal"));
    }
}
