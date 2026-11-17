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
    // Everything in state_dir/cloud that is scoped to the LINK goes with it:
    // the accepted revision and any pending publication belong to the old
    // (workspace, device) pair and would block or misdirect a relink to
    // another workspace. The account (login) and the fleet repository are
    // untouched — unlink is local and reversible by linking again.
    let dir = state_dir.join("cloud");
    for name in [
        "link.json",
        "accepted.json",
        "push-intent.json",
        "push-intent.bundle",
        "pull.bundle",
    ] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {
                return Err(MusterError::Operational(
                    "cannot remove local Cloud link state".into(),
                ))
            }
        }
    }
    Ok(())
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

/// The newest receipt sequence the service holds for `device` (`0` when it
/// has none), read with the account session from the workspace's device
/// listing. Any failure is an error: the caller must not guess a sequence.
pub fn device_last_receipt_sequence(
    account: &CloudAccount,
    workspace: &str,
    device: &str,
) -> Result<u64, MusterError> {
    let unavailable =
        || MusterError::Operational("cannot read the device's receipt history from Cloud".into());
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let path = crate::cloud_protocol::routes::WORKSPACE_DEVICES.replace("{ws}", workspace);
    let mut response = agent
        .get(format!("{}{path}", account.endpoint))
        .header("Authorization", format!("Bearer {}", account.token()))
        .call()
        .map_err(|_| unavailable())?;
    if !response.status().is_success() {
        return Err(unavailable());
    }
    let devices: Vec<crate::cloud_protocol::Device> =
        response.body_mut().read_json().map_err(|_| unavailable())?;
    let found = devices
        .iter()
        .find(|d| d.id == device)
        .ok_or_else(unavailable)?;
    Ok(found.last_receipt.as_ref().map_or(0, |r| r.sequence))
}

/// Sends one device-signed request over the transport and returns
/// `(status, body bytes)` — the caller inspects both. The signature binds
/// method, canonical path, workspace, device, body hash, base revision,
/// idempotency key, timestamp and nonce: the same request scheme the sync
/// transport uses.
pub(crate) fn device_request(
    link: &CloudLink,
    signer: &crate::signing::Signer,
    method: &str,
    path_and_query: &str,
    body: &[u8],
    base_revision: &str,
    idempotency_key: &str,
) -> Result<(u16, Vec<u8>), MusterError> {
    let hash = crate::cloud_protocol::body_sha256_hex(body);
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
    let nonce = {
        let mut nonce = [0u8; 16];
        getrandom::getrandom(&mut nonce)
            .map_err(|e| MusterError::Operational(format!("no randomness available: {e}")))?;
        signing_hex(&nonce)
    };
    let signed = crate::cloud_protocol::SignedRequestV1 {
        method,
        path_and_query,
        workspace_id: &link.workspace,
        device_id: &link.device,
        body_sha256_hex: &hash,
        base_revision,
        idempotency_key,
        timestamp: &timestamp,
        nonce: &nonce,
    };
    let signature = format!(
        "v1={}",
        signer.sign(&crate::cloud_protocol::canonical_request_v1(&signed))
    );
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let url = format!("{}{path_and_query}", link.endpoint);
    macro_rules! headers {
        ($request:expr) => {
            $request
                .header("Authorization", format!("Bearer {}", link.token()))
                .header(crate::cloud_protocol::HEADER_DEVICE_ID, &link.device)
                .header(crate::cloud_protocol::HEADER_TIMESTAMP, &timestamp)
                .header(crate::cloud_protocol::HEADER_NONCE, &nonce)
                .header(crate::cloud_protocol::HEADER_CONTENT_SHA256, &hash)
                .header(crate::cloud_protocol::HEADER_BASE_REVISION, base_revision)
                .header(
                    crate::cloud_protocol::HEADER_IDEMPOTENCY_KEY,
                    idempotency_key,
                )
                .header(crate::cloud_protocol::HEADER_SIGNATURE, &signature)
        };
    }
    let outcome = match method {
        "GET" => headers!(agent.get(&url)).call(),
        "POST" => headers!(agent.post(&url)).send(body),
        _ => {
            return Err(MusterError::Operational(format!(
                "unsupported device request method {method}"
            )))
        }
    };
    let response = outcome
        .map_err(|_| MusterError::Operational("the Cloud service did not answer; retry".into()))?;
    let status = response.status().as_u16();
    let bytes = response
        .into_body()
        .read_to_vec()
        .map_err(|_| MusterError::Operational("cannot read the Cloud service response".into()))?;
    Ok((status, bytes))
}

fn signing_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Publishes the machine's signed enrollment request as an OPEN enrollment
/// proposal: a non-member cannot push bundles, so the proposal channel is
/// the only way its request reaches the fleet's approvers.
pub fn submit_enrollment_proposal(
    state_dir: &Path,
    link: &CloudLink,
    request: &serde_json::Value,
    idempotency_key: &str,
) -> Result<String, MusterError> {
    let signer = crate::signing::Signer::load(state_dir)?
        .ok_or_else(|| MusterError::Operational("machine signing key missing".into()))?;
    let body = serde_json::json!({
        "kind": "enrollment",
        "base_revision": "none",
        "payload": request,
    });
    let bytes = serde_json::to_vec(&body)
        .map_err(|_| MusterError::Operational("cannot encode the enrollment proposal".into()))?;
    let path = crate::cloud_protocol::routes::WORKSPACE_PROPOSALS.replace("{ws}", &link.workspace);
    let (status, body) = device_request(
        link,
        &signer,
        "POST",
        &path,
        &bytes,
        "none",
        idempotency_key,
    )?;
    if status != 200 && status != 201 {
        let detail = String::from_utf8_lossy(&body);
        return Err(MusterError::Operational(format!(
            "the Cloud service refused the enrollment proposal (HTTP {status}){}",
            if detail.trim().is_empty() {
                String::new()
            } else {
                format!(": {}", detail.trim())
            }
        )));
    }
    let accepted: crate::cloud_protocol::Proposal = serde_json::from_slice(&body)
        .map_err(|_| MusterError::Operational("invalid enrollment proposal response".into()))?;
    Ok(accepted.id)
}

/// Fetches the machine's OPEN enrollment proposal from the transport and
/// materializes it into the config repository, so the local approval flow
/// can run on it. `Ok(None)` when no open proposal exists for this machine.
pub fn fetch_open_enrollment_proposal(
    state_dir: &Path,
    config_dir: &Path,
    link: &CloudLink,
    name: &str,
) -> Result<Option<String>, MusterError> {
    let signer = crate::signing::Signer::load(state_dir)?
        .ok_or_else(|| MusterError::Operational("machine signing key missing".into()))?;
    let path = crate::cloud_protocol::routes::WORKSPACE_PROPOSALS.replace("{ws}", &link.workspace);
    let (status, body) = device_request(link, &signer, "GET", &path, &[], "none", "")?;
    if status != 200 {
        return Err(MusterError::Operational(format!(
            "cannot list enrollment proposals (HTTP {status})"
        )));
    }
    let proposals: Vec<crate::cloud_protocol::Proposal> = serde_json::from_slice(&body)
        .map_err(|_| MusterError::Operational("invalid proposal list response".into()))?;
    for proposal in proposals {
        if proposal.status != crate::cloud_protocol::ProposalStatus::Open {
            continue;
        }
        let Some(payload) = proposal.payload else {
            continue;
        };
        let request: crate::fleet::EnrollmentRequest = match serde_json::from_value(payload) {
            Ok(request) => request,
            Err(_) => continue,
        };
        if request.name == name {
            request.save(config_dir)?;
            return Ok(Some(proposal.id));
        }
    }
    Ok(None)
}

/// The service's own refusal, rendered for the operator: HTTP status, the
/// stable error code and its message (the wording is the service's).
fn api_refusal(action: &str, status: u16, body: &[u8]) -> MusterError {
    let detail = match serde_json::from_slice::<crate::cloud_protocol::ApiError>(body) {
        Ok(api) => format!(", {}): {}", api.error.code, api.error.message),
        Err(_) => ")".to_owned(),
    };
    MusterError::Operational(format!(
        "the Cloud service refused to {action} (HTTP {status}{detail}"
    ))
}

fn signed_link_request(
    state_dir: &Path,
    link: &CloudLink,
    method: &str,
    path: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), MusterError> {
    let signer = crate::signing::Signer::load(state_dir)?
        .ok_or_else(|| MusterError::Operational("machine signing key missing".into()))?;
    device_request(link, &signer, method, path, body, "none", "")
}

/// Every proposal of the linked workspace, newest first (any linked device may
/// read them).
pub fn list_proposals(
    state_dir: &Path,
    link: &CloudLink,
) -> Result<Vec<crate::cloud_protocol::Proposal>, MusterError> {
    let path = crate::cloud_protocol::routes::WORKSPACE_PROPOSALS.replace("{ws}", &link.workspace);
    let (status, body) = signed_link_request(state_dir, link, "GET", &path, &[])?;
    if status != 200 {
        return Err(api_refusal("list proposals", status, &body));
    }
    serde_json::from_slice(&body)
        .map_err(|_| MusterError::Operational("invalid proposal list response".into()))
}

/// Closes the open proposal `id` as rejected. Who may do that is decided by
/// the service (owner/admin role or a machine the signed governance authorizes).
pub fn reject_proposal(
    state_dir: &Path,
    link: &CloudLink,
    id: &str,
) -> Result<crate::cloud_protocol::Proposal, MusterError> {
    let path = crate::cloud_protocol::routes::WORKSPACE_PROPOSAL_RESOLVE
        .replace("{ws}", &link.workspace)
        .replace("{id}", id);
    let body = serde_json::to_vec(&crate::cloud_protocol::ResolveProposal {
        result_revision: None,
    })
    .map_err(|_| MusterError::Operational("cannot encode the proposal resolution".into()))?;
    let (status, response) = signed_link_request(state_dir, link, "POST", &path, &body)?;
    if status != 200 {
        return Err(api_refusal("resolve the proposal", status, &response));
    }
    serde_json::from_slice(&response)
        .map_err(|_| MusterError::Operational("invalid proposal resolution response".into()))
}
