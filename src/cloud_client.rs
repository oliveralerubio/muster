//! Local Cloud link, kept outside the fleet repository.
use crate::errors::MusterError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
pub struct CloudLink {
    pub endpoint: String,
    pub workspace: String,
    pub device: String,
    token: String,
}

impl std::fmt::Debug for CloudLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudLink")
            .field("endpoint", &self.endpoint)
            .field("workspace", &self.workspace)
            .field("device", &self.device)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl CloudLink {
    pub fn new(
        endpoint: String,
        workspace: String,
        device: String,
        token: String,
    ) -> Result<Self, MusterError> {
        let link = Self {
            endpoint,
            workspace,
            device,
            token,
        };
        link.validate()?;
        Ok(link)
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    fn path(state_dir: &Path) -> PathBuf {
        state_dir.join("cloud").join("link.json")
    }

    fn validate(&self) -> Result<(), MusterError> {
        validate_endpoint(&self.endpoint)
    }
}

pub fn validate_endpoint(endpoint: &str) -> Result<(), MusterError> {
    let uri = endpoint
        .parse::<ureq::http::Uri>()
        .map_err(|_| MusterError::Security("invalid Cloud endpoint".into()))?;
    let authority = uri
        .authority()
        .ok_or_else(|| MusterError::Security("invalid Cloud endpoint".into()))?;
    let host = authority.host();
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback());
    if authority.as_str().contains('@')
        || uri.query().is_some()
        || !(uri.scheme_str() == Some("https") || (uri.scheme_str() == Some("http") && loopback))
    {
        return Err(MusterError::Security(
            "Cloud endpoint must use HTTPS (HTTP is allowed only for loopback development)".into(),
        ));
    }
    Ok(())
}

impl CloudLink {
    pub fn load(state_dir: &Path) -> Result<Option<Self>, MusterError> {
        let bytes = match std::fs::read(Self::path(state_dir)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => {
                return Err(MusterError::Operational(
                    "cannot read local Cloud link".into(),
                ))
            }
        };
        let link: Self = serde_json::from_slice(&bytes)
            .map_err(|_| MusterError::Security("invalid local Cloud link".into()))?;
        link.validate()?;
        Ok(Some(link))
    }

    pub fn save(&self, state_dir: &Path) -> Result<(), MusterError> {
        self.validate()?;
        let dir = state_dir.join("cloud");
        std::fs::create_dir_all(&dir)
            .map_err(|_| MusterError::Operational("cannot create Cloud state directory".into()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |_| MusterError::Operational("cannot secure Cloud state directory".into()),
            )?;
        }
        #[derive(Serialize)]
        struct StoredLink<'a> {
            endpoint: &'a str,
            workspace: &'a str,
            device: &'a str,
            token: &'a str,
        }
        let bytes = serde_json::to_vec(&StoredLink {
            endpoint: &self.endpoint,
            workspace: &self.workspace,
            device: &self.device,
            token: &self.token,
        })
        .map_err(|_| MusterError::Operational("cannot encode local Cloud link".into()))?;
        crate::vault::write_atomic(&Self::path(state_dir), &bytes)
            .map_err(|_| MusterError::Operational("cannot save local Cloud link".into()))
    }
}

pub fn unlink(state_dir: &Path) -> Result<(), MusterError> {
    match std::fs::remove_file(CloudLink::path(state_dir)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(MusterError::Operational(
            "cannot remove local Cloud link".into(),
        )),
    }
}
pub fn save_account(
    state_dir: &Path,
    endpoint: &str,
    token: &crate::cloud_protocol::AccountToken,
) -> Result<(), MusterError> {
    let dir = state_dir.join("cloud");
    std::fs::create_dir_all(&dir)
        .map_err(|_| MusterError::Operational("cannot create Cloud state directory".into()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| MusterError::Operational("cannot secure Cloud state directory".into()))?;
    }
    let bytes = serde_json::to_vec(&serde_json::json!({"endpoint": endpoint, "account_id": token.account_id, "token": token.access_token}))
        .map_err(|_| MusterError::Operational("cannot encode Cloud account".into()))?;
    crate::vault::write_atomic(&dir.join("account.json"), &bytes)
        .map_err(|_| MusterError::Operational("cannot save Cloud account".into()))
}
#[derive(Deserialize)]
pub struct CloudAccount {
    pub endpoint: String,
    pub account_id: String,
    token: String,
}

impl CloudAccount {
    pub fn load(state_dir: &Path) -> Result<Self, MusterError> {
        let bytes = std::fs::read(state_dir.join("cloud/account.json")).map_err(|_| {
            MusterError::Operational("Cloud account not available; run muster cloud login".into())
        })?;
        let account: Self = serde_json::from_slice(&bytes)
            .map_err(|_| MusterError::Security("invalid local Cloud account".into()))?;
        validate_endpoint(&account.endpoint)?;
        Ok(account)
    }
    pub fn token(&self) -> &str {
        &self.token
    }
}
pub fn link_intent(
    state_dir: &Path,
    account: &CloudAccount,
    workspace: &str,
    request: &crate::cloud_protocol::LinkDeviceRequest,
) -> Result<String, MusterError> {
    let dir = state_dir.join("cloud");
    let path = dir.join("link-intent.json");
    let fingerprint = serde_json::json!({"endpoint": account.endpoint, "account_id": account.account_id, "workspace": workspace, "request": request});
    if let Ok(bytes) = std::fs::read(&path) {
        let value: serde_json::Value = serde_json::from_slice(&bytes)
            .map_err(|_| MusterError::Security("invalid Cloud link attempt".into()))?;
        if value["intent"] != fingerprint {
            return Err(MusterError::Operational(
                "a different Cloud link attempt is pending; retry it first".into(),
            ));
        }
        return value["key"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| MusterError::Security("invalid Cloud link attempt".into()));
    }
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| MusterError::Operational("cannot create Cloud link attempt".into()))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let key = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    std::fs::create_dir_all(&dir)
        .map_err(|_| MusterError::Operational("cannot create Cloud state directory".into()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|_| MusterError::Operational("cannot secure Cloud state directory".into()))?;
    }
    let value = serde_json::to_vec(&serde_json::json!({"intent":fingerprint, "key":key}))
        .map_err(|_| MusterError::Operational("cannot encode Cloud link attempt".into()))?;
    crate::vault::write_atomic(&path, &value)
        .map_err(|_| MusterError::Operational("cannot save Cloud link attempt".into()))?;
    Ok(key)
}

pub fn clear_link_intent(state_dir: &Path) -> Result<(), MusterError> {
    match std::fs::remove_file(state_dir.join("cloud/link-intent.json")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(MusterError::Operational(
            "cannot remove Cloud link attempt".into(),
        )),
    }
}
