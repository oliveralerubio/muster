use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};
use serde_json::{json, Value};

pub const STATUS: CommandSpec = CommandSpec {
    name: "cloud.status",
    about: "Show local Cloud link status",
    params: &[],
    mutating: false,
    handler: status,
};

fn status(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    match crate::cloud_client::CloudLink::load(&ctx.state_dir)? {
        Some(link) => Ok(
            json!({"status": "linked", "endpoint": link.endpoint, "workspace": link.workspace, "device": link.device}),
        ),
        None => Ok(json!({"status": "unlinked"})),
    }
}

pub const UNLINK: CommandSpec = CommandSpec {
    name: "cloud.unlink",
    about: "Remove the local Cloud link",
    params: &[],
    mutating: true,
    handler: unlink,
};

fn unlink(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    if !ctx.dry_run {
        crate::cloud_client::unlink(&ctx.state_dir)?;
    }
    Ok(json!({"status": "unlinked"}))
}
pub const LOGIN: CommandSpec = CommandSpec {
    name: "cloud.login",
    about: "Sign in to a Cloud account",
    params: &[
        crate::registry::ParamSpec {
            name: "endpoint",
            about: "Cloud service URL",
            kind: crate::registry::ParamKind::String,
            required: true,
            positional: false,
        },
        crate::registry::ParamSpec {
            name: "no-browser",
            about: "Do not open a browser",
            kind: crate::registry::ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: login,
};

fn login(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    use crate::cloud_protocol::{
        self as protocol, AccountToken, ApiError, DeviceAuthorization, DeviceAuthorizeRequest,
        DeviceTokenRequest,
    };
    let endpoint = params.get_string("endpoint").unwrap().trim_end_matches('/');
    crate::cloud_client::validate_endpoint(endpoint)?;
    if ctx.dry_run {
        return Ok(json!({"status": "would_login"}));
    }
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(format!("{endpoint}{}", protocol::routes::DEVICE_AUTHORIZE))
        .send_json(&DeviceAuthorizeRequest {
            client_name: "muster CLI".into(),
        })
        .map_err(|_| MusterError::Operational("Cloud authorization request failed".into()))?;
    if !response.status().is_success() {
        return Err(MusterError::Operational(
            "Cloud authorization request failed".into(),
        ));
    }
    let authorization: DeviceAuthorization = response
        .body_mut()
        .read_json()
        .map_err(|_| MusterError::Operational("invalid Cloud authorization response".into()))?;
    crate::cloud_client::validate_endpoint(&authorization.verification_uri)?;
    eprintln!(
        "Open {} and enter code {}",
        authorization.verification_uri, authorization.user_code
    );
    if !params.get_bool("no-browser") {
        let program = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let _ = std::process::Command::new(program)
            .arg(&authorization.verification_uri)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    let deadline =
        std::time::Instant::now() + std::time::Duration::from_secs(authorization.expires_in);
    let mut interval = authorization.interval.max(1);
    loop {
        let wait = std::time::Duration::from_secs(interval);
        if std::time::Instant::now()
            .checked_add(wait)
            .is_none_or(|next| next >= deadline)
        {
            return Err(MusterError::Operational(
                "Cloud login expired; try again".into(),
            ));
        }
        std::thread::sleep(wait);
        let mut response = agent
            .post(format!("{endpoint}{}", protocol::routes::DEVICE_TOKEN))
            .send_json(&DeviceTokenRequest {
                grant_type: protocol::DEVICE_CODE_GRANT_TYPE.into(),
                device_code: authorization.device_code.clone(),
            })
            .map_err(|_| MusterError::Operational("Cloud login request failed".into()))?;
        if response.status().is_success() {
            let token: AccountToken = response
                .body_mut()
                .read_json()
                .map_err(|_| MusterError::Operational("invalid Cloud login response".into()))?;
            crate::cloud_client::save_account(&ctx.state_dir, endpoint, &token)?;
            return Ok(json!({"status": "logged_in", "account_id": token.account_id}));
        }
        let error: ApiError = response
            .body_mut()
            .read_json()
            .map_err(|_| MusterError::Operational("Cloud login request failed".into()))?;
        match error.error.code.as_str() {
            protocol::error_codes::AUTHORIZATION_PENDING => {}
            protocol::error_codes::SLOW_DOWN => interval = interval.saturating_add(5),
            protocol::error_codes::EXPIRED_TOKEN => {
                return Err(MusterError::Operational(
                    "Cloud login expired; try again".into(),
                ))
            }
            protocol::error_codes::ACCESS_DENIED => {
                return Err(MusterError::Operational("Cloud login was denied".into()))
            }
            _ => {
                return Err(MusterError::Operational(
                    "Cloud login request failed".into(),
                ))
            }
        }
    }
}
pub const LOGOUT: CommandSpec = CommandSpec {
    name: "cloud.logout",
    about: "Remove the local Cloud account",
    params: &[],
    mutating: true,
    handler: logout,
};

fn logout(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    if ctx.dry_run {
        return Ok(json!({"status": "would_logout"}));
    }
    let revoked = if let Ok(account) = crate::cloud_client::CloudAccount::load(&ctx.state_dir) {
        let agent: ureq::Agent = ureq::config::Config::builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(std::time::Duration::from_secs(5)))
            .build()
            .into();
        agent
            .post(format!(
                "{}{}",
                account.endpoint,
                crate::cloud_protocol::routes::LOGOUT
            ))
            .header("Authorization", format!("Bearer {}", account.token()))
            .send_empty()
            .is_ok_and(|r| r.status().is_success() || r.status().as_u16() == 401)
    } else {
        false
    };
    match std::fs::remove_file(ctx.state_dir.join("cloud/account.json")) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {
            return Err(MusterError::Operational(
                "cannot remove local Cloud account".into(),
            ))
        }
    }
    Ok(json!({"status": "logged_out_locally", "server_revoked": revoked}))
}
pub const WORKSPACES: CommandSpec = CommandSpec {
    name: "cloud.workspaces",
    about: "List Cloud workspaces",
    params: &[],
    mutating: false,
    handler: workspaces,
};

fn workspaces(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let account = crate::cloud_client::CloudAccount::load(&ctx.state_dir)?;
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .get(format!(
            "{}{}",
            account.endpoint,
            crate::cloud_protocol::routes::WORKSPACES
        ))
        .header("Authorization", format!("Bearer {}", account.token()))
        .call()
        .map_err(|_| MusterError::Operational("cannot list Cloud workspaces".into()))?;
    if !response.status().is_success() {
        return Err(MusterError::Operational(
            "cannot list Cloud workspaces".into(),
        ));
    }
    let workspaces: Vec<crate::cloud_protocol::Workspace> = response
        .body_mut()
        .read_json()
        .map_err(|_| MusterError::Operational("invalid Cloud workspace response".into()))?;
    Ok(json!({"workspaces": workspaces}))
}
pub const LINK: CommandSpec = CommandSpec {
    name: "cloud.link",
    about: "Link this machine to a Cloud workspace",
    params: &[crate::registry::ParamSpec {
        name: "workspace",
        about: "Workspace ID (required when there is more than one)",
        kind: crate::registry::ParamKind::String,
        required: false,
        positional: false,
    }],
    mutating: true,
    handler: link,
};

fn link(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    use crate::cloud_protocol::{canonical_link_v1, LinkDeviceRequest, LinkedDevice};
    let account = crate::cloud_client::CloudAccount::load(&ctx.state_dir)?;
    let listed = workspaces(ctx, &Params::default())?;
    let workspaces: Vec<crate::cloud_protocol::Workspace> =
        serde_json::from_value(listed["workspaces"].clone())
            .map_err(|_| MusterError::Operational("invalid workspace list".into()))?;
    let workspace = match params.get_string("workspace") {
        Some(id) => workspaces
            .iter()
            .find(|w| w.id == id)
            .ok_or_else(|| MusterError::Usage("workspace not found in your account".into()))?,
        None if workspaces.len() == 1 => &workspaces[0],
        None if workspaces.is_empty() => {
            return Err(MusterError::Usage(
                "create a workspace in Cloud before linking".into(),
            ))
        }
        None => {
            return Err(MusterError::Usage(
                "choose a workspace with --workspace; run muster cloud workspaces to list them"
                    .into(),
            ))
        }
    };
    if let Some(existing) = crate::cloud_client::CloudLink::load(&ctx.state_dir)? {
        if existing.endpoint == account.endpoint && existing.workspace == workspace.id {
            if !ctx.dry_run {
                // Re-running `link` on an established link reconciles the
                // receipt sequence (e.g. after the local receipt state was lost).
                establish_receipt_sequence(ctx, &account, &existing, false)?;
            }
            return Ok(
                json!({"status":"linked", "workspace": existing.workspace, "device": existing.device}),
            );
        }
        return Err(MusterError::Usage(
            "unlink the current Cloud workspace first".into(),
        ));
    }
    let machine = crate::machine::load_name(&ctx.state_dir).ok_or_else(|| {
        MusterError::Usage("initialize or import a fleet before linking (run muster init)".into())
    })?;
    let identity = crate::identity::Identity::load(&ctx.state_dir).map_err(|_| {
        MusterError::Usage(
            "machine identity missing; initialize or import a fleet before linking".into(),
        )
    })?;
    let signer = crate::signing::Signer::load(&ctx.state_dir)?.ok_or_else(|| {
        MusterError::Usage(
            "machine signing key missing; initialize or import a fleet before linking".into(),
        )
    })?;
    let signing_key = signer.public();
    let recipient = identity.recipient();
    let proof = signer.sign(&canonical_link_v1(
        &workspace.id,
        &account.account_id,
        &machine,
        &signing_key,
        &recipient,
    ));
    let request = LinkDeviceRequest {
        machine_name: machine,
        signing_key,
        recipient,
        proof,
    };
    if ctx.dry_run {
        return Ok(json!({"status":"would_link", "workspace":workspace.id}));
    }
    let intent =
        crate::cloud_client::link_intent(&ctx.state_dir, &account, &workspace.id, &request)?;
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(format!(
            "{}/v1/workspaces/{}/devices",
            account.endpoint, workspace.id
        ))
        .header("Authorization", format!("Bearer {}", account.token()))
        .header("Idempotency-Key", &intent)
        .send_json(&request)
        .map_err(|_| {
            MusterError::Operational("Cloud link request failed; retry the same command".into())
        })?;
    if !response.status().is_success() {
        return Err(MusterError::Operational(
            "Cloud link request failed; retry the same command".into(),
        ));
    }
    let linked: LinkedDevice = response.body_mut().read_json().map_err(|_| {
        MusterError::Operational("invalid Cloud link response; retry the same command".into())
    })?;
    if linked.workspace_id != workspace.id {
        return Err(MusterError::Security(
            "Cloud returned a different workspace".into(),
        ));
    }
    let link = crate::cloud_client::CloudLink::new(
        account.endpoint.clone(),
        linked.workspace_id,
        linked.device_id.clone(),
        linked.device_token,
    )?;
    // The service re-links the same signing key to the SAME device, so the
    // receipt counter is reconciled, never reset, under the sync lock.
    establish_receipt_sequence(ctx, &account, &link, true)?;
    crate::cloud_client::clear_link_intent(&ctx.state_dir)?;
    Ok(
        json!({"status": linked.status, "bootstrap": linked.bootstrap, "workspace":workspace.id, "device":linked.device_id}),
    )
}

/// Persists the link (when `save`) and establishes its durable receipt
/// counter in ONE critical section under `sync.lock`: the queue is shared
/// with a running `muster sync`, which holds the same lock.
fn establish_receipt_sequence(
    ctx: &mut Context,
    account: &crate::cloud_client::CloudAccount,
    link: &crate::cloud_client::CloudLink,
    save: bool,
) -> Result<(), MusterError> {
    let remote =
        crate::cloud_client::device_last_receipt_sequence(account, &link.workspace, &link.device)
            .ok();
    let state_dir = ctx.state_dir.clone();
    let sequence = crate::receipts::with_sync_lock(&state_dir, || {
        if save {
            link.save(&state_dir)?;
        }
        crate::receipts::init_for_link(&state_dir, link, remote)
    })?;
    if sequence == crate::receipts::LinkSequence::Deferred {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_sequence_deferred",
            "this machine has no receipt history for the device and the service's receipt \
             history could not be read; apply receipts are deferred until `muster cloud link` \
             succeeds again"
                .to_string(),
        ));
    }
    Ok(())
}
