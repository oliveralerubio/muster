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
    /// Development only (`MUSTER_DEV_STORAGE_LIMIT_BYTES`): replaces every
    /// plan's `max_storage_bytes`, in `GET /billing` and in the guard alike,
    /// so tests can reach the limit. Refused in production.
    pub dev_storage_limit: Option<u64>,
    /// Development only (`MUSTER_DEV_DELETION_GRACE_SECS`): primary grace of
    /// new deletion requests in seconds (backup cap keeps the 30:35 ratio),
    /// so `due` can be reached in tests. Refused in production.
    pub dev_deletion_grace_secs: Option<u64>,
    /// Development only (`MUSTER_DEV_LIFECYCLE_CRASH`, `after_journal` or
    /// `after_witness`): abort while journaling a deletion request, to test
    /// replay. Refused in production.
    pub dev_lifecycle_crash: Option<crate::control::CrashPoint>,
    /// Development only (`MUSTER_DEV_PURGE_CRASH`): aborts the process at a
    /// purge step (`after_tombstone`, `after_rename`, `after_db`,
    /// `after_trash`). Refused in production.
    pub dev_purge_crash: Option<crate::lifecycle_purge::PurgeCrash>,
    /// Development only (`MUSTER_DEV_ACK_PAUSE_FILE`): while this file
    /// exists, a write request (any method but GET/HEAD) waits after its
    /// handler and before its response is sent, writing `<file>.reached`
    /// first, so tests can fence an instance under an in-flight write.
    /// Refused in production.
    pub dev_ack_pause_file: Option<std::path::PathBuf>,
    /// Development only (`MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS`): how long
    /// idempotency records are kept, instead of
    /// [`crate::maintenance::IDEMPOTENCY_RETENTION`], so tests can see them
    /// expire. Never shortens the replay-nonce retention. Refused in
    /// production.
    pub dev_idempotency_retention_secs: Option<u64>,
    /// Lifecycle authority (`crate::control`): journal directory and its
    /// independent witness file. Mandatory everywhere; never created by
    /// the service.
    pub control: crate::control::ControlPaths,
    /// Request rate limits per minute (`MUSTER_RATE_LIMIT_*_PER_MINUTE`;
    /// `0` disables one).
    pub rate_limits: crate::rate_limit::RateLimitConfig,
    /// Reverse proxies whose `X-Forwarded-For` names the client
    /// (`MUSTER_TRUSTED_PROXIES`: comma-separated addresses or CIDR
    /// networks). Empty: the TCP peer is the client.
    pub trusted_proxies: Vec<crate::rate_limit::IpNet>,
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
                // Paddle: "separate sandbox and live environments. Each has its
                // own base URL and its own set of API keys."
                // (developer.paddle.com/api-reference/about)
                let environment = get("MUSTER_PADDLE_ENVIRONMENT")
                    .ok_or(ConfigError::Missing("MUSTER_PADDLE_ENVIRONMENT"))?;
                let (official_url, key_prefix) = match environment.as_str() {
                    "sandbox" => ("https://sandbox-api.paddle.com", "pdl_sdbx_apikey_"),
                    "live" => ("https://api.paddle.com", "pdl_live_apikey_"),
                    _ => {
                        return Err(ConfigError::Invalid {
                            name: "MUSTER_PADDLE_ENVIRONMENT",
                            reason: "expected sandbox or live".into(),
                        })
                    }
                };
                if environment == "live" && !env.is_production() {
                    return Err(ConfigError::Invalid {
                        name: "MUSTER_PADDLE_ENVIRONMENT",
                        reason: "live billing requires MUSTER_ENV=production".into(),
                    });
                }
                if !api_key.starts_with(key_prefix) {
                    // Never echo the key itself.
                    return Err(ConfigError::Invalid {
                        name: "MUSTER_PADDLE_API_KEY",
                        reason: format!("is not a {environment} API key"),
                    });
                }
                let base_url = match get("MUSTER_PADDLE_BASE_URL") {
                    None => official_url.to_string(),
                    Some(url) if url.trim_end_matches('/') == official_url => official_url.to_string(),
                    Some(_) if env.is_production() => {
                        return Err(ConfigError::Invalid {
                            name: "MUSTER_PADDLE_BASE_URL",
                            reason: format!(
                                "only the official {environment} API ({official_url}) is allowed in production"
                            ),
                        })
                    }
                    // Development only: a local provider fixture.
                    Some(url) => url.trim_end_matches('/').to_string(),
                };
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

        let dev_storage_limit = match get("MUSTER_DEV_STORAGE_LIMIT_BYTES") {
            None => None,
            Some(_) if env.is_production() => {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_DEV_STORAGE_LIMIT_BYTES",
                    reason: "is a development-only setting; plan limits apply in production".into(),
                })
            }
            Some(v) => Some(v.parse::<u64>().map_err(|e| ConfigError::Invalid {
                name: "MUSTER_DEV_STORAGE_LIMIT_BYTES",
                reason: e.to_string(),
            })?),
        };

        let dev_deletion_grace_secs =
            match get("MUSTER_DEV_DELETION_GRACE_SECS") {
                None => None,
                Some(_) if env.is_production() => return Err(ConfigError::Invalid {
                    name: "MUSTER_DEV_DELETION_GRACE_SECS",
                    reason:
                        "is a development-only setting; the deletion grace is fixed in production"
                            .into(),
                }),
                Some(v) => Some(v.parse::<u64>().ok().filter(|s| *s >= 1).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "MUSTER_DEV_DELETION_GRACE_SECS",
                        reason: "expected a whole number of seconds, at least 1".into(),
                    }
                })?),
            };
        let dev_lifecycle_crash =
            match get("MUSTER_DEV_LIFECYCLE_CRASH") {
                None => None,
                Some(_) if env.is_production() => {
                    return Err(ConfigError::Invalid {
                        name: "MUSTER_DEV_LIFECYCLE_CRASH",
                        reason: "is a development-only crash injection".into(),
                    })
                }
                Some(v) => Some(crate::control::CrashPoint::parse(&v).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "MUSTER_DEV_LIFECYCLE_CRASH",
                        reason: "expected after_journal or after_witness".into(),
                    }
                })?),
            };

        let dev_purge_crash = match get("MUSTER_DEV_PURGE_CRASH") {
            None => None,
            Some(_) if env.is_production() => {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_DEV_PURGE_CRASH",
                    reason: "is a development-only crash injection".into(),
                })
            }
            Some(v) => Some(
                crate::lifecycle_purge::PurgeCrash::parse(&v).ok_or_else(|| {
                    ConfigError::Invalid {
                        name: "MUSTER_DEV_PURGE_CRASH",
                        reason: "expected after_tombstone, after_rename, after_db or after_trash"
                            .into(),
                    }
                })?,
            ),
        };

        let dev_ack_pause_file = match get("MUSTER_DEV_ACK_PAUSE_FILE") {
            None => None,
            Some(_) if env.is_production() => {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_DEV_ACK_PAUSE_FILE",
                    reason: "is a development-only test hook".into(),
                })
            }
            Some(v) => Some(std::path::PathBuf::from(v)),
        };

        let dev_idempotency_retention_secs = match get("MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS") {
            None => None,
            Some(_) if env.is_production() => {
                return Err(ConfigError::Invalid {
                    name: "MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS",
                    reason: "is a development-only setting; the retention is fixed in production"
                        .into(),
                })
            }
            Some(v) => Some(v.parse::<u64>().ok().filter(|s| *s >= 1).ok_or_else(|| {
                ConfigError::Invalid {
                    name: "MUSTER_DEV_IDEMPOTENCY_RETENTION_SECS",
                    reason: "expected a whole number of seconds, at least 1".into(),
                }
            })?),
        };

        let control = control_paths(&get, &repo_root)?;

        let per_minute = |name: &'static str, default: u32| -> Result<u32, ConfigError> {
            match get(name) {
                None => Ok(default),
                Some(v) => v.trim().parse::<u32>().map_err(|_| ConfigError::Invalid {
                    name,
                    reason: "expected a whole number of requests per minute (0 disables)".into(),
                }),
            }
        };
        let defaults = crate::rate_limit::RateLimitConfig::default();
        let rate_limits = crate::rate_limit::RateLimitConfig {
            device_per_minute: per_minute(
                "MUSTER_RATE_LIMIT_DEVICE_PER_MINUTE",
                defaults.device_per_minute,
            )?,
            link_codes_per_minute: per_minute(
                "MUSTER_RATE_LIMIT_LINK_CODES_PER_MINUTE",
                defaults.link_codes_per_minute,
            )?,
            device_token_per_minute: per_minute(
                "MUSTER_RATE_LIMIT_DEVICE_TOKEN_PER_MINUTE",
                defaults.device_token_per_minute,
            )?,
        };
        let trusted_proxies = match get("MUSTER_TRUSTED_PROXIES") {
            None => Vec::new(),
            Some(list) => list
                .split(',')
                .filter(|p| !p.trim().is_empty())
                .map(|p| {
                    crate::rate_limit::IpNet::parse(p).ok_or_else(|| ConfigError::Invalid {
                        name: "MUSTER_TRUSTED_PROXIES",
                        reason: format!("{:?} is not an IP address or CIDR network", p.trim()),
                    })
                })
                .collect::<Result<_, _>>()?,
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
            dev_storage_limit,
            dev_deletion_grace_secs,
            dev_lifecycle_crash,
            dev_purge_crash,
            dev_ack_pause_file,
            dev_idempotency_retention_secs,
            control,
            rate_limits,
            trusted_proxies,
        })
    }

    pub fn database_url(&self) -> &str {
        &self.database_url
    }

    pub fn oidc_client_secret(&self) -> &str {
        &self.oidc_client_secret
    }
}

/// `a` is `b` or lies inside it (component-wise; both already resolved).
fn within(a: &std::path::Path, b: &std::path::Path) -> bool {
    a.starts_with(b)
}

/// The real filesystem identity of an absolute path that may not exist yet
/// (first init): `..` components are refused outright (ambiguous through
/// symbolic links); the longest existing ancestor is canonicalised (every
/// symbolic link resolved) and the missing leaves appended. A dangling
/// symbolic link is refused: writing through it would land wherever it
/// points.
fn resolve_identity(path: &std::path::Path) -> Result<std::path::PathBuf, String> {
    use std::path::Component;
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("must not contain '..' components".into());
    }
    let mut existing = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(&existing) {
            Ok(mut real) => {
                real.extend(missing.iter().rev());
                return Ok(real);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(&existing).is_ok() {
                    return Err(format!(
                        "{} is a dangling symbolic link",
                        existing.display()
                    ));
                }
                let Some(name) = existing.file_name().map(|n| n.to_os_string()) else {
                    return Err("has no existing ancestor".into());
                };
                missing.push(name);
                existing.pop();
            }
            Err(e) => return Err(format!("cannot be resolved: {e}")),
        }
    }
}

/// Validates `MUSTER_CONTROL_DIR` / `MUSTER_CONTROL_WITNESS` (shared by the
/// service and `muster-cloud-ops`).
pub fn control_paths(
    get: &impl Fn(&str) -> Option<String>,
    repo_root: &std::path::Path,
) -> Result<crate::control::ControlPaths, ConfigError> {
    let dir = std::path::PathBuf::from(
        get("MUSTER_CONTROL_DIR").ok_or(ConfigError::Missing("MUSTER_CONTROL_DIR"))?,
    );
    let witness = std::path::PathBuf::from(
        get("MUSTER_CONTROL_WITNESS").ok_or(ConfigError::Missing("MUSTER_CONTROL_WITNESS"))?,
    );
    let invalid = |name: &'static str, reason: &str| ConfigError::Invalid {
        name,
        reason: reason.into(),
    };
    if !dir.is_absolute() {
        return Err(invalid("MUSTER_CONTROL_DIR", "must be an absolute path"));
    }
    if !witness.is_absolute() {
        return Err(invalid(
            "MUSTER_CONTROL_WITNESS",
            "must be an absolute path",
        ));
    }
    // Separation is decided on real identities, never on spelling: an alias
    // (`..`, symbolic link) must not put the authority into the Git store or
    // its backups. The service and ops then use the resolved paths.
    let dir = resolve_identity(&dir).map_err(|r| invalid("MUSTER_CONTROL_DIR", &r))?;
    let witness = resolve_identity(&witness).map_err(|r| invalid("MUSTER_CONTROL_WITNESS", &r))?;
    let repo_root = &resolve_identity(repo_root).map_err(|r| invalid("MUSTER_REPO_ROOT", &r))?;
    if within(&dir, repo_root) || within(repo_root, &dir) {
        return Err(invalid(
            "MUSTER_CONTROL_DIR",
            "must be disjoint from MUSTER_REPO_ROOT (neither inside the other)",
        ));
    }
    if within(&witness, &dir) || within(&witness, repo_root) {
        return Err(invalid(
            "MUSTER_CONTROL_WITNESS",
            "must be outside MUSTER_CONTROL_DIR and MUSTER_REPO_ROOT",
        ));
    }
    Ok(crate::control::ControlPaths { dir, witness })
}

/// `MUSTER_BACKUP_ROOT`, resolved: absolute and disjoint from the
/// repository root and from the lifecycle authority (journal and witness).
pub fn backup_root(
    get: &impl Fn(&str) -> Option<String>,
    repo_root: &std::path::Path,
    control: &crate::control::ControlPaths,
) -> Result<std::path::PathBuf, ConfigError> {
    let name = "MUSTER_BACKUP_ROOT";
    let invalid = |reason: &str| ConfigError::Invalid {
        name,
        reason: reason.into(),
    };
    let root = std::path::PathBuf::from(get(name).ok_or(ConfigError::Missing(name))?);
    if !root.is_absolute() {
        return Err(invalid("must be an absolute path"));
    }
    let root = resolve_identity(&root).map_err(|r| invalid(&r))?;
    let repo = resolve_identity(repo_root).map_err(|r| ConfigError::Invalid {
        name: "MUSTER_REPO_ROOT",
        reason: r,
    })?;
    for other in [&repo, &control.dir, &control.witness] {
        if within(&root, other) || within(other, &root) {
            return Err(invalid(
                "must be disjoint from MUSTER_REPO_ROOT, MUSTER_CONTROL_DIR and MUSTER_CONTROL_WITNESS",
            ));
        }
    }
    Ok(root)
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
            .field("paddle", &self.paddle)
            .field("control", &self.control)
            .field("rate_limits", &self.rate_limits)
            .field("trusted_proxies", &self.trusted_proxies)
            .finish()
    }
}
