//! Read native AI endpoint settings, never credential stores or plaintext keys.
use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use crate::adapters::config::{AiProvider, AiProviderKind};
use crate::errors::MusterError;

pub const NATIVE_FILES: &[&str] = &[
    ".claude/settings.json",
    ".codex/config.toml",
    ".config/opencode/opencode.json",
];

pub fn scan(dir: &Path) -> Result<BTreeMap<String, AiProvider>, MusterError> {
    let mut ai_providers = BTreeMap::new();
    for file in NATIVE_FILES {
        let text = match std::fs::read_to_string(dir.join(file)) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Err(invalid(file)),
        };
        ai_providers.extend(detect(file, &text)?);
    }
    Ok(ai_providers)
}

fn invalid(file: &str) -> MusterError {
    // Parser errors can quote secret-bearing input. Deliberately omit them.
    MusterError::InvalidConfig {
        message: "cannot read native AI provider configuration".into(),
        path: Some(file.into()),
    }
}

pub fn detect(file: &str, text: &str) -> Result<BTreeMap<String, AiProvider>, MusterError> {
    let mut ai_providers = BTreeMap::new();
    if !NATIVE_FILES.contains(&file) {
        return Ok(ai_providers);
    }
    let value: Value = if file.ends_with(".toml") {
        let value: toml::Value = toml::from_str(text).map_err(|_| invalid(file))?;
        serde_json::to_value(value).map_err(|_| invalid(file))?
    } else {
        serde_json::from_str(text).map_err(|_| invalid(file))?
    };
    let mut insert = |id: &str, kind, url: Option<&str>, model: Option<&str>, has_key: bool| {
        if crate::machine::validate_name(id).is_err() {
            return;
        }
        // URLs may embed credentials. Do not copy userinfo, query or fragments.
        let base_url = url
            .filter(|url| !url.contains(['@', '?', '#']))
            .map(str::to_owned);
        let key = has_key.then(|| {
            format!(
                "env://MUSTER_IMPORTED_{}_KEY",
                id.to_ascii_uppercase().replace('-', "_")
            )
        });
        ai_providers.insert(
            id.to_owned(),
            AiProvider {
                kind,
                base_url,
                default_model: model.map(str::to_owned),
                key,
                api_key_helper: None,
            },
        );
    };
    match file {
        ".claude/settings.json" => {
            let env = &value["env"];
            if env["ANTHROPIC_BASE_URL"].is_string()
                || env["ANTHROPIC_AUTH_TOKEN"].is_string()
                || env["ANTHROPIC_API_KEY"].is_string()
            {
                insert(
                    "claude",
                    AiProviderKind::Anthropic,
                    env["ANTHROPIC_BASE_URL"].as_str(),
                    value["model"].as_str().or(env["ANTHROPIC_MODEL"].as_str()),
                    env["ANTHROPIC_AUTH_TOKEN"].is_string() || env["ANTHROPIC_API_KEY"].is_string(),
                );
            }
        }
        ".codex/config.toml" => {
            if let Some(entries) = value["model_providers"].as_object() {
                for (id, entry) in entries {
                    let model = (value["model_provider"].as_str() == Some(id))
                        .then(|| value["model"].as_str())
                        .flatten();
                    insert(
                        id,
                        AiProviderKind::OpenaiCompatible,
                        entry["base_url"].as_str(),
                        model,
                        entry["env_key"].is_string()
                            || entry["experimental_bearer_token"].is_string(),
                    );
                }
            }
        }
        _ => {
            if let Some(entries) = value["provider"].as_object() {
                for (id, entry) in entries {
                    let kind = match entry["npm"].as_str() {
                        Some("@ai-sdk/anthropic") => AiProviderKind::Anthropic,
                        Some("@ai-sdk/google") => AiProviderKind::Google,
                        Some("@ai-sdk/openai") => AiProviderKind::Openai,
                        _ => AiProviderKind::OpenaiCompatible,
                    };
                    let prefix = format!("{id}/");
                    insert(
                        id,
                        kind,
                        entry["options"]["baseURL"].as_str(),
                        value["model"]
                            .as_str()
                            .and_then(|m| m.strip_prefix(&prefix)),
                        entry["options"]["apiKey"].is_string(),
                    );
                }
            }
        }
    }
    Ok(ai_providers)
}

/// Native login state is device-bound and must never enter an import proposal.
pub fn is_credential_store(target: &str) -> bool {
    matches!(
        target,
        ".claude/.credentials.json"
            | ".codex/auth.json"
            | ".local/share/opencode/auth.json"
            | ".config/opencode/auth.json"
    )
}
