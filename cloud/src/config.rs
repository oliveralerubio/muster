//! Service configuration from environment variables.
//!
//! Safe by default: without `MUSTER_ENV` the service runs in production mode.
//! Nothing here falls back to a development value, and `Debug` never prints
//! passwords, tokens or secrets.

use std::fmt;
use std::net::SocketAddr;

use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Production,
    Development,
}

impl Environment {
    pub fn is_production(self) -> bool {
        matches!(self, Environment::Production)
    }
}

#[derive(Clone)]
pub struct Config {
    pub env: Environment,
    pub bind: SocketAddr,
    database_url: String,
    pub public_url: Url,
    pub oidc_issuer: Url,
    pub oidc_client_id: String,
    oidc_client_secret: String,
    /// Absolute directory holding one bare Git repository per workspace.
    pub repo_root: std::path::PathBuf,
    pub paddle: Option<PaddleConfig>,
}

#[derive(Clone)]
pub struct PaddleConfig {
    pub environment: String,
    pub api_key: String,
    pub webhook_secret: String,
    pub price_personal: String,
    pub price_team_seat: String,
    pub base_url: String,
}

impl std::fmt::Debug for PaddleConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PaddleConfig")
            .field("environment", &self.environment)
            .field("api_key", &"[REDACTED]")
            .field("webhook_secret", &"[REDACTED]")
            .field("price_personal", &self.price_personal)
            .field("price_team_seat", &self.price_team_seat)
            .field("base_url", &self.base_url)
            .finish()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{name} is invalid: {reason}")]
    Invalid { name: &'static str, reason: String },
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let env = match get("MUSTER_ENV").as_deref() {
            None | Some("production") => Environment::Production,
            Some("development") => Environment::Development,
            Some(other) => {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_ENV",
                    reason: format!("expected production or development, got {other:?}"),
                })
            }
        };

        let bind = get("MUSTER_BIND")
            .unwrap_or_else(|| "127.0.0.1:8080".into())
            .parse()
            .map_err(|err: std::net::AddrParseError| ConfigError::Invalid {
                name: "MUSTER_BIND",
                reason: err.to_string(),
            })?;

        let database_url =
            get("MUSTER_DATABASE_URL").ok_or(ConfigError::Missing("MUSTER_DATABASE_URL"))?;

        let public_url_str =
            get("MUSTER_PUBLIC_URL").ok_or(ConfigError::Missing("MUSTER_PUBLIC_URL"))?;
        let public_url = Url::parse(&public_url_str).map_err(|err| ConfigError::Invalid {
            name: "MUSTER_PUBLIC_URL",
            reason: err.to_string(),
        })?;

        let oidc_issuer_str =
            get("MUSTER_OIDC_ISSUER").ok_or(ConfigError::Missing("MUSTER_OIDC_ISSUER"))?;
        let oidc_issuer = Url::parse(&oidc_issuer_str).map_err(|err| ConfigError::Invalid {
            name: "MUSTER_OIDC_ISSUER",
            reason: err.to_string(),
        })?;

        let oidc_client_id =
            get("MUSTER_OIDC_CLIENT_ID").ok_or(ConfigError::Missing("MUSTER_OIDC_CLIENT_ID"))?;
        let oidc_client_secret = get("MUSTER_OIDC_CLIENT_SECRET")
            .ok_or(ConfigError::Missing("MUSTER_OIDC_CLIENT_SECRET"))?;

        let repo_root = std::path::PathBuf::from(
            get("MUSTER_REPO_ROOT").ok_or(ConfigError::Missing("MUSTER_REPO_ROOT"))?,
        );
        if !repo_root.is_absolute() {
            return Err(ConfigError::Invalid {
                name: "MUSTER_REPO_ROOT",
                reason: "must be an absolute path".into(),
            });
        }

        if env.is_production() {
            if public_url.scheme() != "https" {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_PUBLIC_URL",
                    reason: "must use https:// in production".into(),
                });
            }
            if oidc_issuer.scheme() != "https" {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_OIDC_ISSUER",
                    reason: "must use https:// in production".into(),
                });
            }
            if is_loopback(&oidc_issuer) {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_OIDC_ISSUER",
                    reason: "cannot be a loopback address in production".into(),
                });
            }
        }

        let paddle = match get("MUSTER_PADDLE_API_KEY") {
            Some(api_key) => {
                let webhook_secret = get("MUSTER_PADDLE_WEBHOOK_SECRET")
                    .ok_or(ConfigError::Missing("MUSTER_PADDLE_WEBHOOK_SECRET"))?;
                let price_personal = get("MUSTER_PADDLE_PRICE_PERSONAL")
                    .ok_or(ConfigError::Missing("MUSTER_PADDLE_PRICE_PERSONAL"))?;
                let price_team_seat = get("MUSTER_PADDLE_PRICE_TEAM_SEAT")
                    .ok_or(ConfigError::Missing("MUSTER_PADDLE_PRICE_TEAM_SEAT"))?;
                let environment =
                    get("MUSTER_PADDLE_ENVIRONMENT").unwrap_or_else(|| "sandbox".into());
                let base_url = get("MUSTER_PADDLE_BASE_URL").unwrap_or_else(|| {
                    if environment == "production" {
                        "https://api.paddle.com".into()
                    } else {
                        "https://sandbox-api.paddle.com".into()
                    }
                });
                Some(PaddleConfig {
                    environment,
                    api_key,
                    webhook_secret,
                    price_personal,
                    price_team_seat,
                    base_url,
                })
            }
            None => None,
        };

        Ok(Self {
            env,
            bind,
            database_url,
            public_url,
            oidc_issuer,
            oidc_client_id,
            oidc_client_secret,
            repo_root,
            paddle,
        })
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn oidc_client_secret(&self) -> &str {
        &self.oidc_client_secret
    }
}

/// True for `localhost`, `*.localhost`, 127.0.0.0/8 and `[::1]`.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => {
            let name = name.trim_end_matches('.').to_ascii_lowercase();
            name == "localhost" || name.ends_with(".localhost")
        }
        Some(url::Host::Ipv4(ip)) => ip.is_loopback() || ip.is_unspecified(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback() || ip.is_unspecified(),
        None => true,
    }
}

/// Database URL with any password removed, for logs.
fn redact_url(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(mut url) => {
            if url.password().is_some() {
                let _ = url.set_password(Some("REDACTED"));
            }
            url.set_query(None);
            url.to_string()
        }
        Err(_) => "<unparseable>".into(),
    }
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Config")
            .field("env", &self.env)
            .field("bind", &self.bind)
            .field("database_url", &redact_url(&self.database_url))
            .field("public_url", &self.public_url.as_str())
            .field("oidc_issuer", &self.oidc_issuer.as_str())
            .field("oidc_client_id", &self.oidc_client_id)
            .field("oidc_client_secret", &"[REDACTED]")
            .field("repo_root", &self.repo_root)
            .finish()
    }
}
