//! Cloud transport state and signed publication, separate from fleet data.
use crate::{
    cloud_client::CloudLink, cloud_protocol as protocol, context::Context, errors::MusterError,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Serialize, Deserialize)]
struct Intent {
    workspace: String,
    device: String,
    base: String,
    revision: String,
    key: String,
}

fn failure(message: &str) -> MusterError {
    MusterError::Operational(message.into())
}
fn random_hex() -> Result<String, MusterError> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|_| failure("cannot create Cloud request nonce"))?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}
fn uuid() -> Result<String, MusterError> {
    let mut b = [0u8; 16];
    getrandom::getrandom(&mut b).map_err(|_| failure("cannot create Cloud publication key"))?;
    b[6] = (b[6] & 15) | 0x40;
    b[8] = (b[8] & 63) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &h[..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..]
    ))
}
fn git(dir: &Path, args: &[&str]) -> Result<(), MusterError> {
    let output = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|_| failure("cannot run git for Cloud publication"))?;
    if !output.status.success() {
        return Err(failure("cannot prepare Cloud publication"));
    }
    Ok(())
}

fn git_bool(dir: &Path, args: &[&str]) -> bool {
    std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

/// Outcome of a Cloud pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PullOutcome {
    /// The accepted head equals this machine's HEAD (or the workspace has no
    /// accepted history yet).
    UpToDate,
    /// New accepted commits were checked out on top of the local history.
    Updated,
    /// The fleet history replaced an unrelated local repository; the previous
    /// HEAD is kept in `refs/muster/pre-join` (the Git join backup ref).
    Joined,
}

impl PullOutcome {
    pub fn label(self) -> &'static str {
        match self {
            PullOutcome::UpToDate => "up_to_date",
            PullOutcome::Updated => "updated",
            PullOutcome::Joined => "joined",
        }
    }

    pub fn joined(self) -> bool {
        matches!(self, PullOutcome::Joined)
    }
}

/// Temporary ref holding the incoming tip between fetch and checkout.
const PULL_REF: &str = "refs/muster/cloud-pull";

/// Download the accepted history and verify the complete incoming chain
/// against this machine's pin (and `--expect-signer`/`--expect-state`) in a
/// staging clone BEFORE checking it out, then fetch it: an unrelated local
/// history joins the fleet (with the join backup), related history is
/// rebased on the incoming tip like a Git pull. Nothing is applied here.
pub fn pull(ctx: &mut Context, link: &CloudLink) -> Result<PullOutcome, MusterError> {
    let dir = ctx.state_dir.join("cloud");
    let bundle = dir.join("pull.bundle");
    let previous = crate::git::head_commit_opt(&ctx.config_dir);
    let since = load_accepted_for_link(link, &dir).filter(|rev| {
        is_canonical_object_id(rev)
            && previous.is_some()
            && git_bool(
                &ctx.config_dir,
                &["merge-base", "--is-ancestor", rev, "HEAD"],
            )
    });
    let query = since.as_deref().map(|rev| format!("since={rev}"));
    let path = format!("/v1/workspaces/{}/sync", link.workspace);
    let path_and_query = protocol::canonical_path_and_query(&path, query.as_deref());
    let hash = protocol::body_sha256_hex(b"");
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
    let nonce = random_hex()?;
    let signer = crate::signing::Signer::load(&ctx.state_dir)?
        .ok_or_else(|| failure("machine signing key missing"))?;
    let signed = protocol::SignedRequestV1 {
        method: "GET",
        path_and_query: &path_and_query,
        workspace_id: &link.workspace,
        device_id: &link.device,
        body_sha256_hex: &hash,
        base_revision: "",
        idempotency_key: "",
        timestamp: &timestamp,
        nonce: &nonce,
    };
    let signature = format!(
        "v1={}",
        signer.sign(&protocol::canonical_request_v1(&signed))
    );
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .get(format!("{}{}", link.endpoint, path_and_query))
        .header("Authorization", format!("Bearer {}", link.token()))
        .header(protocol::HEADER_DEVICE_ID, &link.device)
        .header(protocol::HEADER_TIMESTAMP, &timestamp)
        .header(protocol::HEADER_NONCE, &nonce)
        .header(protocol::HEADER_CONTENT_SHA256, &hash)
        .header(protocol::HEADER_SIGNATURE, &signature)
        .call()
        .map_err(|_| failure("Cloud download failed; retry muster sync --pull"))?;
    // The service names the accepted head it served; after the verified
    // checkout this becomes this machine's base revision for its next
    // publication (a machine that joined by pulling could otherwise never
    // publish: its recorded base would stay "none").
    let tip = response
        .headers()
        .get(protocol::HEADER_REVISION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned);
    if response.status().as_u16() == 204 {
        match (tip.as_deref(), since.as_deref()) {
            (Some(revision), Some(sent))
                if is_canonical_object_id(revision) && revision == sent =>
            {
                record_accepted(link, &dir, revision)?;
            }
            (Some(revision), _) => {
                clear_accepted_if_same_link(link, &dir);
                ctx.diag(crate::output::Diagnostic::new(
                    crate::output::Severity::Warning,
                    "cloud_revision_mismatch",
                    format!(
                        "Cloud reported revision `{revision}` on an up-to-date response \
                         that does not match the requested revision; accepted provenance \
                         was cleared"
                    ),
                ));
            }
            (None, _) => {
                clear_accepted_if_same_link(link, &dir);
                ctx.diag(crate::output::Diagnostic::new(
                    crate::output::Severity::Warning,
                    "cloud_revision_missing",
                    "Cloud up-to-date response omitted the x-muster-revision header; \
                     accepted provenance was cleared"
                        .to_string(),
                ));
            }
        }
        return Ok(PullOutcome::UpToDate);
    }
    if response.status().as_u16() != 200 {
        return Err(failure(
            "Cloud refused the download; local history is preserved",
        ));
    }
    let bytes = response
        .body_mut()
        .read_to_vec()
        .map_err(|_| failure("invalid Cloud bundle response"))?;
    crate::vault::write_atomic(&bundle, &bytes)
        .map_err(|_| failure("cannot save Cloud download"))?;
    let bundle_str = bundle
        .to_str()
        .ok_or_else(|| failure("invalid Cloud state path"))?;
    let staging = dir.join(format!("pull-verify-{}", random_hex()?));
    std::fs::create_dir(&staging).map_err(|_| failure("cannot prepare Cloud staging"))?;
    let staging_str = staging
        .to_str()
        .ok_or_else(|| failure("invalid Cloud state path"))?
        .to_owned();
    let outcome = verify_and_checkout(ctx, bundle_str, &staging_str, previous.as_deref());
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::process::Command::new("git")
        .current_dir(&ctx.config_dir)
        .args(["update-ref", "-d", PULL_REF])
        .output();
    match outcome {
        Ok((outcome, fetched_tip)) => {
            let _ = std::fs::remove_file(&bundle);
            match tip {
                Some(revision) if is_canonical_object_id(&revision) && revision == fetched_tip => {
                    record_accepted(link, &dir, &revision)?;
                }
                Some(revision) => {
                    clear_accepted_if_same_link(link, &dir);
                    ctx.diag(crate::output::Diagnostic::new(
                        crate::output::Severity::Warning,
                        "cloud_revision_mismatch",
                        format!(
                            "Cloud revision header `{revision}` does not match the verified \
                             fetched commit {fetched_tip}; accepted provenance was not recorded"
                        ),
                    ));
                }
                None => {
                    clear_accepted_if_same_link(link, &dir);
                    ctx.diag(crate::output::Diagnostic::new(
                        crate::output::Severity::Warning,
                        "cloud_revision_missing",
                        "Cloud response omitted the x-muster-revision header; accepted \
                         provenance was not recorded"
                            .to_string(),
                    ));
                }
            }
            Ok(outcome)
        }
        Err(message) => Err(failure(&message)),
    }
}

/// Whether `revision` is a canonical lowercase 40- or 64-hex Git object ID.
pub(crate) fn is_canonical_object_id(revision: &str) -> bool {
    matches!(revision.len(), 40 | 64)
        && revision
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn load_accepted_for_link(link: &CloudLink, dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join("accepted.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value["workspace"] != link.workspace || value["device"] != link.device {
        return None;
    }
    value["revision"].as_str().map(str::to_owned)
}

fn clear_accepted_if_same_link(link: &CloudLink, dir: &Path) {
    let path = dir.join("accepted.json");
    let Ok(bytes) = std::fs::read(&path) else {
        return;
    };
    let should_remove = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(value) => value["workspace"] == link.workspace && value["device"] == link.device,
        Err(_) => true,
    };
    if should_remove {
        let _ = std::fs::remove_file(path);
    }
}

/// Records the verified, checked-out head as this machine's accepted base
/// revision, mirroring what a successful publication stores.
fn record_accepted(link: &CloudLink, dir: &Path, revision: &str) -> Result<(), MusterError> {
    let state = serde_json::to_vec(&serde_json::json!({
        "workspace": link.workspace,
        "device": link.device,
        "revision": revision,
    }))
    .map_err(|_| failure("cannot encode Cloud revision"))?;
    crate::vault::write_atomic(&dir.join("accepted.json"), &state)
        .map_err(|_| failure("cannot save accepted Cloud revision"))
}

/// Staging verification and the checkout it guards. Returns the pull outcome
/// together with the exact commit hash fetched into `PULL_REF`. The error is a
/// plain message so the downloaded bundle and staging directory can be cleaned
/// up before the caller turns it into a `MusterError`.
fn verify_and_checkout(
    ctx: &mut Context,
    bundle: &str,
    staging: &str,
    previous: Option<&str>,
) -> Result<(PullOutcome, String), String> {
    // Fetch the incoming tip without touching the working tree, then decide
    // whether it continues this machine's history or is another fleet.
    if !git_bool(
        &ctx.config_dir,
        &[
            "fetch",
            "--quiet",
            bundle,
            &format!("refs/heads/main:{PULL_REF}"),
        ],
    ) {
        return Err("cannot read the Cloud bundle".into());
    }
    let fetched_out = std::process::Command::new("git")
        .current_dir(&ctx.config_dir)
        .args(["rev-parse", "--verify", &format!("{PULL_REF}^{{commit}}")])
        .output()
        .map_err(|e| format!("cannot resolve fetched Cloud commit: {e}"))?;
    if !fetched_out.status.success() {
        return Err("cannot resolve fetched Cloud commit".into());
    }
    let fetched_tip = String::from_utf8_lossy(&fetched_out.stdout)
        .trim()
        .to_ascii_lowercase();
    if !is_canonical_object_id(&fetched_tip) {
        return Err("fetched Cloud ref did not resolve to a canonical commit id".into());
    }
    // Whether the served tip shares history with this machine's HEAD decides
    // between a plain update and a confirmed join. `git merge-base A B`
    // exits 0 with a common ancestor and 1 without; anything else (>1) is an
    // operational error that must surface as such — never as "unrelated".
    // There is no --quiet flag for merge-base (unknown options exit 129,
    // which used to make every pull look like an unrelated join).
    let related = previous.is_some() && {
        let out = std::process::Command::new("git")
            .current_dir(&ctx.config_dir)
            .args(["merge-base", "HEAD", PULL_REF])
            .output()
            .map_err(|e| format!("cannot run git merge-base: {e}"))?;
        match out.status.code() {
            Some(0) => true,
            Some(1) => false,
            code => {
                return Err(format!(
                    "cannot relate the Cloud history to this machine's HEAD (git merge-base \
                     exit {:?}): {}",
                    code,
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
        }
    };
    let work = |args: &[&str]| git(&ctx.config_dir, args);
    let stage = |args: &[&str]| git(Path::new(staging), args);
    // The staging working tree gives the pure history verifier a complete
    // checkout while the user's repository stays untouched. The tip was
    // already fetched into this repository as `PULL_REF` — staging fetches
    // from THERE, not from the bundle file: the service serves incremental
    // bundles when the requesting revision is a known ancestor, and a fresh
    // staging clone lacks the parent objects an incremental bundle assumes.
    // Bundles carry only `refs/heads/main` (no HEAD), so a plain clone
    // checks out nothing: init, fetch and check the branch out explicitly.
    let source = ctx
        .config_dir
        .to_str()
        .ok_or_else(|| "invalid repository path; cannot stage the Cloud bundle".to_string())?;
    if stage(&["init", "--quiet"]).is_err()
        || stage(&[
            "fetch",
            "--quiet",
            source,
            &format!("{PULL_REF}:refs/heads/main"),
        ])
        .is_err()
        || stage(&["checkout", "--quiet", "main"]).is_err()
    {
        return Err("cannot stage the Cloud bundle for verification".into());
    }

    let manifest = crate::manifest::Manifest::load(Path::new(staging))
        .map_err(|e| format!("the Cloud history holds an invalid manifest: {e}"))?;
    let pin = crate::fleet_state::TrustAnchor::load(&ctx.state_dir)
        .map_err(|e| format!("cannot read the local trust pin: {e}"))?;
    // Related history is checked against the pin (anti-rollback, chain);
    // an unrelated history is a first-use join, checked from genesis with
    // the confirmation flags. The old pin is dropped only after this passes.
    let (verification, _) = crate::fleet_state::verify_against(
        Path::new(staging),
        &manifest,
        if related { pin.as_ref() } else { None },
        &crate::fleet_state::VerifyOptions {
            expect_signer: ctx.expect_signer.as_deref(),
            expect_state: ctx.expect_state.as_deref(),
            persist: false,
            check_content: false,
        },
    )
    .map_err(|e| e.to_string())?;
    if matches!(verification, crate::fleet_state::Verification::Legacy) {
        return Err(
            "the Cloud history is not a signed fleet state; legacy repositories are refused".into(),
        );
    }
    // Snapshot BEFORE the checkout replaces the working tree: whether the
    // pre-pull pin covered this machine's own scaffold (first join) or a
    // fleet accepted from elsewhere (antirollback boundary).
    let own_scaffold_pin = crate::trust::pin_covers_own_scaffold(ctx);
    // Checkout: the chain is verified, nothing unverified reaches the
    // working repository.
    if !related {
        if let Some(prev) = previous {
            let _ = work(&["update-ref", "refs/muster/pre-join", prev]);
        }
        work(&["reset", "--quiet", "--hard", PULL_REF])
            .map_err(|_| "cannot check out the Cloud history".to_string())?;
    } else if git_bool(
        &ctx.config_dir,
        &["merge-base", "--is-ancestor", "HEAD", PULL_REF],
    ) {
        work(&["reset", "--quiet", "--hard", PULL_REF])
            .map_err(|_| "cannot check out the Cloud history".to_string())?;
    } else {
        let mut args: Vec<&str> = Vec::new();
        if !git_bool(&ctx.config_dir, &["config", "user.email"]) {
            args.extend([
                "-c",
                "user.name=Muster",
                "-c",
                "user.email=muster@localhost",
            ]);
        }
        args.extend(["rebase", "--quiet", PULL_REF]);
        if work(&args).is_err() {
            let _ = work(&["rebase", "--abort"]);
            return Err(
                "local changes conflict with the Cloud history; resolve manually and retry \
                 muster sync --pull"
                    .into(),
            );
        }
    }
    // Pin bookkeeping and first-use confirmation on the real repository; a
    // failure here restores the previous HEAD (nothing was applied yet).
    crate::trust::verify_pulled(ctx, previous, !related, true, own_scaffold_pin)
        .map_err(|e| e.to_string())?;
    let outcome = if related {
        PullOutcome::Updated
    } else {
        PullOutcome::Joined
    };
    Ok((outcome, fetched_tip))
}

/// Publish a persistent exact bundle; a retry uses the same key and bytes.
pub fn push(ctx: &Context, link: &CloudLink) -> Result<(), MusterError> {
    let dir = ctx.state_dir.join("cloud");
    let meta = dir.join("push-intent.json");
    let bundle = dir.join("push-intent.bundle");
    let base_path = dir.join("accepted.json");
    let current = crate::git::head_commit(&ctx.config_dir)?;
    let intent: Intent = match std::fs::read(&meta) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| failure("invalid pending Cloud publication"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let base = match std::fs::read(&base_path) {
                Ok(bytes) => {
                    let value: serde_json::Value = serde_json::from_slice(&bytes)
                        .map_err(|_| failure("invalid Cloud revision state"))?;
                    if value["workspace"] != link.workspace || value["device"] != link.device {
                        return Err(failure("Cloud revision state belongs to another link"));
                    }
                    value["revision"]
                        .as_str()
                        .ok_or_else(|| failure("invalid Cloud revision state"))?
                        .to_owned()
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    protocol::BASE_REVISION_NONE.to_owned()
                }
                Err(_) => return Err(failure("cannot read Cloud revision state")),
            };
            if current == base {
                return Ok(());
            }
            // A private temporary bare repository gives the bundle the canonical
            // main ref without changing the user's branches or refs.
            let temp = dir.join(format!("bundle-repo-{}", random_hex()?));
            std::fs::create_dir(&temp).map_err(|_| failure("cannot prepare Cloud bundle"))?;
            let result: Result<(), MusterError> = (|| {
                git(&temp, &["init", "--bare", "--quiet"])?;
                let source = ctx
                    .config_dir
                    .to_str()
                    .ok_or_else(|| failure("invalid repository path"))?;
                git(
                    &temp,
                    &[
                        "fetch",
                        "--quiet",
                        source,
                        &format!("{current}:refs/heads/main"),
                    ],
                )?;
                let bundle_path = bundle
                    .to_str()
                    .ok_or_else(|| failure("invalid Cloud state path"))?;
                git(&temp, &["bundle", "create", bundle_path, "refs/heads/main"])?;
                Ok(())
            })();
            let _ = std::fs::remove_dir_all(&temp);
            result?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bundle, std::fs::Permissions::from_mode(0o600))
                    .map_err(|_| failure("cannot secure Cloud publication"))?;
            }
            let intent = Intent {
                workspace: link.workspace.clone(),
                device: link.device.clone(),
                base,
                revision: current,
                key: uuid()?,
            };
            let bytes = serde_json::to_vec(&intent)
                .map_err(|_| failure("cannot encode Cloud publication"))?;
            crate::vault::write_atomic(&meta, &bytes)
                .map_err(|_| failure("cannot save Cloud publication"))?;
            intent
        }
        Err(_) => return Err(failure("cannot read pending Cloud publication")),
    };
    if intent.workspace != link.workspace || intent.device != link.device {
        return Err(failure("pending publication belongs to another Cloud link"));
    }
    let body =
        std::fs::read(&bundle).map_err(|_| failure("cannot read pending Cloud publication"))?;
    let path = format!("/v1/workspaces/{}/sync", link.workspace);
    let hash = protocol::body_sha256_hex(&body);
    let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
    let nonce = random_hex()?;
    let signer = crate::signing::Signer::load(&ctx.state_dir)?
        .ok_or_else(|| failure("machine signing key missing"))?;
    let signed = protocol::SignedRequestV1 {
        method: "POST",
        path_and_query: &path,
        workspace_id: &link.workspace,
        device_id: &link.device,
        body_sha256_hex: &hash,
        base_revision: &intent.base,
        idempotency_key: &intent.key,
        timestamp: &timestamp,
        nonce: &nonce,
    };
    let signature = format!(
        "v1={}",
        signer.sign(&protocol::canonical_request_v1(&signed))
    );
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .post(format!("{}{path}", link.endpoint))
        .header("Authorization", format!("Bearer {}", link.token()))
        .header(protocol::HEADER_DEVICE_ID, &link.device)
        .header(protocol::HEADER_TIMESTAMP, &timestamp)
        .header(protocol::HEADER_NONCE, &nonce)
        .header(protocol::HEADER_CONTENT_SHA256, &hash)
        .header(protocol::HEADER_BASE_REVISION, &intent.base)
        .header(protocol::HEADER_IDEMPOTENCY_KEY, &intent.key)
        .header(protocol::HEADER_SIGNATURE, &signature)
        .header("Content-Type", protocol::GIT_BUNDLE_MEDIA_TYPE)
        .send(body.as_slice())
        .map_err(|_| failure("Cloud publication failed; retry muster sync --push"))?;
    let status = response.status().as_u16();
    if status != 200 {
        // Surface the server's own error body: the refusal reason (conflict,
        // authorization, quota) is information the operator needs; the local
        // history is preserved either way.
        let detail = response.body_mut().read_to_string().unwrap_or_default();
        let detail = detail.trim();
        // A definitive client-side refusal (4xx) invalidates the pending
        // publication: its base revision and bytes describe a state the
        // server will not accept, and keeping them would poison every later
        // push with a stale base. Network-level failures keep the intent so
        // the same key and bytes can be retried.
        if (400..500).contains(&status) {
            let _ = std::fs::remove_file(&meta);
            let _ = std::fs::remove_file(&bundle);
        }
        return Err(failure(&format!(
            "Cloud did not accept the publication (HTTP {status}){}; local history is preserved",
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            }
        )));
    }
    let accepted: protocol::SyncAccepted = response
        .body_mut()
        .read_json()
        .map_err(|_| failure("invalid Cloud publication response"))?;
    if accepted.revision != intent.revision {
        return Err(failure("Cloud returned an unexpected revision"));
    }
    let state = serde_json::to_vec(&serde_json::json!({"workspace": link.workspace, "device": link.device, "revision": accepted.revision})).map_err(|_| failure("cannot encode Cloud revision"))?;
    crate::vault::write_atomic(&base_path, &state)
        .map_err(|_| failure("cannot save accepted Cloud revision"))?;
    std::fs::remove_file(&meta).map_err(|_| failure("cannot finish Cloud publication"))?;
    std::fs::remove_file(&bundle).map_err(|_| failure("cannot finish Cloud publication"))?;
    Ok(())
}
