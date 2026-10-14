//! `muster.toml`: the manifest model and its JSON Schema.
//!
//! Validation is strict: unknown top-level keys are rejected unless they start
//! with `x-` (free extension space, preserved but ignored by Muster).

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::errors::MusterError;

pub const FILE_NAME: &str = "muster.toml";
const EXTENSION_PREFIX: &str = "x-";

/// Muster manifest (`muster.toml`). Top-level keys starting with `x-` are free extensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(
    deny_unknown_fields,
    extend("patternProperties" = { "^x-": {} })
)]
pub struct Manifest {
    /// Manifest format version.
    pub version: u32,
    // Later tickets add machines, members, groups and profiles next to `version`.
    #[serde(flatten)]
    #[schemars(skip)]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

impl Manifest {
    /// Parse and validate manifest text; `path` is only used in diagnostics.
    pub fn parse(text: &str, path: &str) -> Result<Self, MusterError> {
        let invalid = |message: String| MusterError::InvalidConfig {
            message,
            path: Some(path.to_string()),
        };
        let manifest: Self = toml::from_str(text).map_err(|e| invalid(e.message().to_string()))?;
        if let Some(key) = manifest
            .extensions
            .keys()
            .find(|k| !k.starts_with(EXTENSION_PREFIX))
        {
            return Err(invalid(format!(
                "unknown key `{key}` (extension keys must start with `{EXTENSION_PREFIX}`)"
            )));
        }
        Ok(manifest)
    }
}

/// JSON Schema of `muster.toml`, as exported by `muster schema`.
pub fn json_schema() -> Value {
    serde_json::to_value(schemars::schema_for!(Manifest)).expect("schema serializes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_version_and_extension_keys() {
        let m = Manifest::parse("version = 1\nx-team = \"core\"\n", "muster.toml").unwrap();
        assert_eq!(m.version, 1);
        assert!(m.extensions.contains_key("x-team"));
    }

    #[test]
    fn rejects_unknown_keys_as_invalid_config() {
        let err = Manifest::parse("version = 1\nbogus = true\n", "muster.toml").unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(err.to_string().contains("bogus"));
    }

    #[test]
    fn rejects_missing_version() {
        let err = Manifest::parse("", "muster.toml").unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }
}
