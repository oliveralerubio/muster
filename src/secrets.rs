//! Secret values that never print their plaintext, and the references
//! (`native://group/name`, `env://VAR`, `op://...`) the manifest stores instead.

use std::fmt;

use secrecy::{ExposeSecret, SecretString};

use crate::providers::{ProviderError, ProviderErrorKind};

/// A plaintext secret. `Debug` and `Display` always render `<redacted>`;
/// only [`SecretValue::expose`] yields the text.
#[derive(Clone)]
pub struct SecretValue(SecretString);

impl SecretValue {
    pub fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    /// The plaintext. Call it at the last possible moment.
    pub fn expose(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

impl fmt::Display for SecretValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// Where a secret reference is resolved. The scheme is the reference prefix
/// (`native://`, `env://`, ...).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Scheme {
    /// Muster's own vault (the default provider).
    Native,
    /// A process environment variable.
    Env,
    /// 1Password via the `op` CLI.
    Op,
    /// Bitwarden Secrets Manager via the `bws` CLI.
    Bws,
    /// Doppler via the `doppler` CLI.
    Doppler,
    /// Infisical via the `infisical` CLI.
    Infisical,
}

impl Scheme {
    pub const ALL: [Scheme; 6] = [
        Self::Native,
        Self::Env,
        Self::Op,
        Self::Bws,
        Self::Doppler,
        Self::Infisical,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Env => "env",
            Self::Op => "op",
            Self::Bws => "bws",
            Self::Doppler => "doppler",
            Self::Infisical => "infisical",
        }
    }

    /// Expected shape, used in error messages.
    fn shape(self) -> &'static str {
        match self {
            Self::Native => "native://<group>/<name>",
            Self::Env => "env://<VARIABLE>",
            Self::Op => "op://<vault>/<item>/[<section>/]<field>",
            Self::Bws => "bws://<secret-id>[/<field>]",
            Self::Doppler => "doppler://<project>/<config>/<NAME>",
            Self::Infisical => "infisical://<project-id>/<environment>/[<folder>/...]<NAME>",
        }
    }
}

/// A parsed secret reference such as `native://app/api` or `op://vault/item/field`.
/// References are not secret; they are what the manifest stores instead of values.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SecretRef {
    scheme: Scheme,
    raw: String,
    segments: Vec<String>,
}

impl SecretRef {
    /// Parses and validates a reference. Unknown schemes and malformed paths are
    /// [`ProviderErrorKind::InvalidReference`] (exit 2).
    pub fn parse(raw: &str) -> Result<Self, ProviderError> {
        let invalid = |why: String| {
            ProviderError::new(
                ProviderErrorKind::InvalidReference,
                format!("invalid secret reference '{raw}': {why}"),
            )
        };
        let (prefix, rest) = raw
            .split_once("://")
            .ok_or_else(|| invalid("expected <provider>://<path>".into()))?;
        let scheme = Scheme::ALL
            .into_iter()
            .find(|s| s.as_str() == prefix)
            .ok_or_else(|| {
                let known: Vec<_> = Scheme::ALL.iter().map(|s| s.as_str()).collect();
                invalid(format!(
                    "unknown provider '{prefix}' (known: {})",
                    known.join(", ")
                ))
            })?;
        let segments: Vec<String> = rest.split('/').map(str::to_owned).collect();
        let count_ok = match scheme {
            Scheme::Native | Scheme::Env => true,
            Scheme::Op => matches!(segments.len(), 3 | 4),
            Scheme::Bws => matches!(segments.len(), 1 | 2),
            Scheme::Doppler => segments.len() == 3,
            Scheme::Infisical => segments.len() >= 3,
        };
        let segments_ok = count_ok
            && match scheme {
                Scheme::Native => segments.len() == 2 && segments.iter().all(|s| is_name(s)),
                Scheme::Env => segments.len() == 1 && is_env_var(&segments[0]),
                // External segments become CLI arguments: no empty parts, no
                // option-looking parts, no control characters.
                _ => segments.iter().all(|s| {
                    !s.is_empty() && !s.starts_with('-') && !s.chars().any(char::is_control)
                }),
            };
        if !segments_ok {
            return Err(invalid(format!("expected {}", scheme.shape())));
        }
        Ok(Self {
            scheme,
            raw: raw.to_owned(),
            segments,
        })
    }

    /// `native://<group>/<name>`. Both parts must be valid vault names.
    pub fn native(group: &str, name: &str) -> Result<Self, ProviderError> {
        Self::parse(&format!("native://{group}/{name}"))
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// Path parts after `<scheme>://`.
    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

/// Whether `value` uses a secret-provider scheme (and so must never be written as-is).
pub fn is_reference(value: &str) -> bool {
    value
        .split_once("://")
        .is_some_and(|(prefix, _)| Scheme::ALL.iter().any(|s| s.as_str() == prefix))
}

/// Which agent may receive which secret, for one render target: the
/// manifest's `[secrets."<reference>"] allow` lists plus the project being
/// rendered (`None` = user scope). Deny by default.
#[derive(Debug, Clone, Default)]
pub struct SecretAccess {
    policies: std::collections::BTreeMap<String, crate::manifest::SecretPolicy>,
    project: Option<String>,
}

impl SecretAccess {
    pub fn new(
        policies: std::collections::BTreeMap<String, crate::manifest::SecretPolicy>,
        project: Option<String>,
    ) -> Self {
        Self { policies, project }
    }

    /// Whether `agent` may receive the secret behind `reference`.
    pub fn allows(&self, agent: &str, reference: &str) -> bool {
        self.policies
            .get(reference)
            .is_some_and(|p| p.allows(agent, self.project.as_deref()))
    }
}

/// Vault group and secret names: letters, digits, `_`, `-`, `.`; no leading dot.
pub fn is_name(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

fn is_env_var(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_scheme_and_rejects_malformed_references() {
        let valid = [
            ("native://app/api", Scheme::Native),
            ("env://GITHUB_TOKEN", Scheme::Env),
            ("op://Private/GitHub/token", Scheme::Op),
            ("op://Private/GitHub/section/token", Scheme::Op),
            ("bws://0b6e1f3a-1111-2222-3333-444455556666", Scheme::Bws),
            ("bws://item/key", Scheme::Bws),
            ("doppler://backend/prd/API_KEY", Scheme::Doppler),
            ("infisical://proj/dev/API_KEY", Scheme::Infisical),
            ("infisical://proj/dev/team/svc/API_KEY", Scheme::Infisical),
        ];
        for (raw, scheme) in valid {
            let parsed = SecretRef::parse(raw).unwrap_or_else(|e| panic!("{raw}: {e}"));
            assert_eq!(parsed.scheme(), scheme);
            assert_eq!(parsed.to_string(), raw);
        }
        let invalid = [
            "unknown://x",
            "no-scheme",
            "native://only-group",
            "native://a/b/c",
            "native://../x",
            "env://1BAD",
            "env://A/B",
            "op://vault/item",
            "op://vault//field",
            "bws://id/value/extra",
            "doppler://p/--config/NAME",
            "infisical://p/dev",
        ];
        for raw in invalid {
            let err = SecretRef::parse(raw).expect_err(raw);
            assert_eq!(err.kind, ProviderErrorKind::InvalidReference, "{raw}");
        }
    }

    #[test]
    fn detects_references_by_known_scheme_only() {
        assert!(is_reference("native://core/api_key"));
        assert!(is_reference("op://v/i/f"));
        assert!(!is_reference("https://example.com"));
        assert!(!is_reference("${GITHUB_TOKEN}"));
    }
}
