use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};
use serde_json::{json, Value};
use std::path::Path;

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

pub const EXPORT: CommandSpec = CommandSpec {
    name: "cloud.export",
    about: "Download a full verified export bundle of a Cloud workspace",
    params: &[
        crate::registry::ParamSpec {
            name: "output",
            about: "File to write the bundle to [default: muster-export-<revision>.bundle]",
            kind: crate::registry::ParamKind::String,
            required: false,
            positional: false,
        },
        crate::registry::ParamSpec {
            name: "workspace",
            about: "Workspace ID to export [default: the linked workspace]",
            kind: crate::registry::ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: export,
};

/// `muster cloud export`: downloads the workspace's complete Git bundle with
/// the account session (`workspaces.read` scope; a member-only route), checks
/// it against the `x-muster-export-manifest` header, and only then moves a
/// 0600 staging file into place. The verified bundle is the OSS exit: it can
/// restore a fleet without Cloud (see docs/guides/cloud.md). Tokens travel in
/// the Authorization header only — never in argv, URLs, logs or the result.
fn export(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    use crate::cloud_protocol::{self as protocol, ExportManifest};
    let account = crate::cloud_client::CloudAccount::load(&ctx.state_dir)?;
    // The endpoint needs only the account token; the local link merely
    // supplies the DEFAULT workspace. `--workspace` therefore works on
    // login-only machines and on machines that already left Cloud.
    let workspace = match params.get_string("workspace") {
        Some(id) => {
            if id.is_empty()
                || id.len() > 64
                || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return Err(MusterError::Usage("invalid workspace id".into()));
            }
            id.to_owned()
        }
        None => {
            let link = crate::cloud_client::CloudLink::load(&ctx.state_dir)?.ok_or_else(|| {
                MusterError::Usage(
                    "this machine has no Cloud link; pass --workspace <id> or run \
                     muster cloud link first"
                        .into(),
                )
            })?;
            link.workspace.clone()
        }
    };
    if ctx.dry_run {
        return Ok(json!({"status": "would_export", "workspace": workspace}));
    }
    let path = format!("/v1/workspaces/{workspace}/export");
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let mut response = crate::cloud_client::send_with_rate_limit(true, || {
        agent
            .get(format!("{}{path}", account.endpoint))
            .header("Authorization", format!("Bearer {}", account.token()))
            .call()
            .map_err(|_| MusterError::Operational("the Cloud service did not answer; retry".into()))
    })?;
    let status = response.status().as_u16();
    if status == 204 {
        return Err(MusterError::Operational(
            "the workspace has no accepted history to export yet".into(),
        ));
    }
    if status != 200 {
        return Err(MusterError::Operational(format!(
            "the Cloud service refused the export of workspace {workspace} (HTTP {status}); \
             no export was written"
        )));
    }
    let media_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(|v| v.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if media_type != protocol::GIT_BUNDLE_MEDIA_TYPE {
        return Err(MusterError::Security(
            "the Cloud service answered with an unexpected content type; no export was written"
                .into(),
        ));
    }
    let manifest: ExportManifest = response
        .headers()
        .get(protocol::HEADER_EXPORT_MANIFEST)
        .and_then(|v| v.to_str().ok())
        .map(serde_json::from_str)
        .transpose()
        .map_err(|_| MusterError::Security("invalid Cloud export manifest".into()))?
        .ok_or_else(|| {
            MusterError::Security(
                "the Cloud export carried no export manifest; no export was written".into(),
            )
        })?;
    let served_revision = response
        .headers()
        .get(protocol::HEADER_REVISION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default();
    let served_epoch = response
        .headers()
        .get(protocol::HEADER_EPOCH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok());
    if served_revision != manifest.revision || served_epoch != Some(manifest.epoch) {
        return Err(MusterError::Security(
            "the Cloud export manifest contradicts the served revision or epoch; \
             no export was written"
                .into(),
        ));
    }
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|_| MusterError::Operational("cannot read the Cloud export response".into()))?;
    if protocol::body_sha256_hex(&bytes) != manifest.sha256 {
        return Err(MusterError::Security(
            "the export bundle does not match its export manifest (sha256); \
             no export was written"
                .into(),
        ));
    }
    if !crate::sync_transport::is_canonical_object_id(&manifest.revision) {
        return Err(MusterError::Security(
            "the Cloud export manifest names a non-canonical revision; no export was written"
                .into(),
        ));
    }
    // Absolute target: a path with a leading dash must never reach git as a
    // flag, and the staging file next to it must not depend on the cwd.
    let target = match params.get_string("output") {
        Some(file) => std::path::absolute(file)
            .map_err(|_| MusterError::Operational("cannot resolve the export path".into()))?,
        None => std::env::current_dir()
            .map_err(|_| MusterError::Operational("cannot resolve the working directory".into()))?
            .join(format!("muster-export-{}.bundle", &manifest.revision[..12])),
    };
    let parent = target
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| MusterError::Usage("the export target has no parent directory".into()))?;
    std::fs::create_dir_all(&parent)
        .map_err(|_| MusterError::Operational("cannot create the export directory".into()))?;
    // Never replace a directory or a symlink: the final rename does not
    // follow symlinks, so following one would silently bypass it, and a
    // directory target can never hold the bundle.
    if let Ok(meta) = std::fs::symlink_metadata(&target) {
        if meta.is_dir() {
            return Err(MusterError::Usage(format!(
                "the export target {} is a directory; pass a file path",
                target.display()
            )));
        }
        if meta.file_type().is_symlink() {
            return Err(MusterError::Usage(format!(
                "the export target {} is a symlink; muster does not replace symlinks",
                target.display()
            )));
        }
    }
    // Validate the bundle BEFORE anything exists at the target: the bytes are
    // staged privately (0600), the manifest revision is checked against the
    // bundle tip, and only a fully verified export is moved into place.
    let staging = parent.join(format!(".muster-export-{}", random_suffix()?));
    let result = crate::vault::write_atomic(&staging, &bytes)
        .map_err(|_| MusterError::Operational("cannot stage the export bundle".into()))
        .and_then(|()| verify_bundle_tip(&staging, &manifest.revision))
        .and_then(|()| {
            std::fs::rename(&staging, &target)
                .map_err(|_| MusterError::Operational("cannot write the export bundle".into()))
        })
        .and_then(|()| sync_directory(&parent));
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result?;
    Ok(json!({
        "path": target.display().to_string(),
        "revision": manifest.revision,
        "workspace": workspace,
        "manifest": manifest,
    }))
}

/// Fsyncs the directory holding the renamed bundle, so the verified export
/// survives a crash like the other fleet artifacts do.
fn sync_directory(parent: &Path) -> Result<(), MusterError> {
    #[cfg(unix)]
    {
        std::fs::File::open(parent)
            .and_then(|handle| handle.sync_all())
            .map_err(|e| MusterError::Operational(format!("cannot flush the export: {e}")))
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
        Ok(())
    }
}

/// Whether `bundle` is a readable Git bundle listing exactly one
/// `refs/heads/main` whose tip is exactly `revision`. Every (sha, refname)
/// pair is parsed: filtering by pattern alone would tail-match decoy names
/// such as `refs/tags/refs/heads/main`, and taking the first line would
/// depend on git's listing order.
fn verify_bundle_tip(bundle: &Path, revision: &str) -> Result<(), MusterError> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--", &bundle.to_string_lossy()])
        .output()
        .map_err(|_| MusterError::Operational("cannot run git to verify the export".into()))?;
    if !output.status.success() {
        return Err(MusterError::Security(
            "the export is not a readable Git bundle; no export was written".into(),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut mains = 0;
    for line in stdout.lines() {
        let Some((sha, refname)) = line.split_once('\t') else {
            continue;
        };
        if refname.trim() != "refs/heads/main" {
            continue;
        }
        mains += 1;
        if mains > 1 || sha.trim().to_ascii_lowercase() != revision {
            return Err(MusterError::Security(
                "the export bundle tip does not match the export manifest revision; \
                 no export was written"
                    .into(),
            ));
        }
    }
    if mains == 0 {
        return Err(MusterError::Security(
            "the export bundle carries no refs/heads/main; no export was written".into(),
        ));
    }
    Ok(())
}

fn random_suffix() -> Result<String, MusterError> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes)
        .map_err(|_| MusterError::Operational("no randomness available".into()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
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
    // Not keyed: a second request would start a second authorization, so a
    // limit is reported, never retried.
    let mut response = crate::cloud_client::send_with_rate_limit(false, || {
        agent
            .post(format!("{endpoint}{}", protocol::routes::DEVICE_AUTHORIZE))
            .send_json(&DeviceAuthorizeRequest {
                client_name: "muster CLI".into(),
            })
            .map_err(|_| MusterError::Operational("Cloud authorization request failed".into()))
    })?;
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
            // Polling slower is the answer to both: the loop only ever
            // widens its interval, so it cannot spin against a limit.
            protocol::error_codes::SLOW_DOWN | protocol::error_codes::RATE_LIMITED => {
                interval = interval.saturating_add(5)
            }
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
    let mut response = crate::cloud_client::send_with_rate_limit(true, || {
        agent
            .get(format!(
                "{}{}",
                account.endpoint,
                crate::cloud_protocol::routes::WORKSPACES
            ))
            .header("Authorization", format!("Bearer {}", account.token()))
            .call()
            .map_err(|_| MusterError::Operational("cannot list Cloud workspaces".into()))
    })?;
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
    // Keyed by the persisted link intent: repeating it is safe.
    let mut response = crate::cloud_client::send_with_rate_limit(true, || {
        agent
            .post(format!(
                "{}/v1/workspaces/{}/devices",
                account.endpoint, workspace.id
            ))
            .header("Authorization", format!("Bearer {}", account.token()))
            .header("Idempotency-Key", &intent)
            .send_json(&request)
            .map_err(|_| {
                MusterError::Operational("Cloud link request failed; retry the same command".into())
            })
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
    let remote = match crate::cloud_client::device_last_receipt_sequence(
        account,
        &link.workspace,
        &link.device,
    ) {
        Ok(sequence) => Some(sequence),
        // Not fatal (the sequence stays deferred), but say why.
        Err(e @ MusterError::RateLimited(_)) => {
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Warning,
                e.code(),
                e.to_string(),
            ));
            None
        }
        Err(_) => None,
    };
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

pub const PROPOSALS: CommandSpec = CommandSpec {
    name: "cloud.proposals",
    about: "List the open Cloud proposals of the linked workspace (conflicts, enrollment, quorum)",
    params: &[],
    mutating: false,
    handler: proposals,
};

fn linked(ctx: &Context) -> Result<crate::cloud_client::CloudLink, MusterError> {
    crate::cloud_client::CloudLink::load(&ctx.state_dir)?
        .ok_or_else(|| MusterError::Usage("link this machine first: muster cloud link".into()))
}

fn proposals(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let link = linked(ctx)?;
    let open: Vec<crate::cloud_protocol::Proposal> =
        crate::cloud_client::list_proposals(&ctx.state_dir, &link)?
            .into_iter()
            .filter(|p| p.status == crate::cloud_protocol::ProposalStatus::Open)
            .collect();
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(proposals_text(&open)));
    }
    Ok(json!({"proposals": open}))
}

fn proposals_text(open: &[crate::cloud_protocol::Proposal]) -> String {
    if open.is_empty() {
        return "No open Cloud proposals.".to_owned();
    }
    let short = |rev: &str| match rev {
        "" => "-".to_owned(),
        rev => rev.chars().take(12).collect(),
    };
    let mut text = String::from("ID  KIND  AUTHOR DEVICE  BASE  REVISION  CREATED\n");
    for p in open {
        text.push_str(&format!(
            "{}  {}  {}  {}  {}  {}\n",
            p.id,
            p.kind.as_str(),
            p.device_id.as_deref().unwrap_or("-"),
            short(&p.base_revision),
            short(&p.revision),
            p.created_at
        ));
    }
    text.trim_end().to_owned()
}

pub const PROPOSALS_RESOLVE: CommandSpec = CommandSpec {
    name: "cloud.proposals.resolve",
    about: "Close an open Cloud proposal (the service decides who may)",
    params: &[
        crate::registry::ParamSpec {
            name: "id",
            about: "Proposal ID (see muster cloud proposals)",
            kind: crate::registry::ParamKind::String,
            required: true,
            positional: true,
        },
        crate::registry::ParamSpec {
            name: "reject",
            about: "Mark the proposal rejected without incorporating it",
            kind: crate::registry::ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: proposals_resolve,
};

fn proposals_resolve(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let id = params.get_string("id").unwrap_or_default();
    if !is_uuid(id) {
        return Err(MusterError::Usage(
            "the proposal ID is a UUID; list them with muster cloud proposals".into(),
        ));
    }
    if !params.get_bool("reject") {
        return Err(MusterError::Usage(
            "pass --reject: closing a proposal without incorporating it is the only resolution \
             this command offers"
                .into(),
        ));
    }
    let link = linked(ctx)?;
    if ctx.dry_run {
        return Ok(json!({"id": id, "status": "would_reject"}));
    }
    let proposal = crate::cloud_client::reject_proposal(&ctx.state_dir, &link, id)?;
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format!(
            "Proposal {} is now {}.",
            proposal.id,
            proposal.status.as_str()
        )));
    }
    Ok(serde_json::to_value(&proposal).unwrap_or(Value::Null))
}

fn is_uuid(value: &str) -> bool {
    value.len() == 36
        && value.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        })
}
