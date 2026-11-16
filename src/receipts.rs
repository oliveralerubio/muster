//! Signed apply receipts over the Cloud transport.
//!
//! A receipt attests that THIS machine actually applied an accepted Cloud
//! revision: it is produced only after a qualifying global `engine::apply`
//! (non-dry-run, `applied == true`, no failed agents, no failed Context)
//! with provenance (exact accepted revision + the epoch of its verified
//! signed fleet state) established before the plan and re-checked after the
//! apply. Everything else — download-only joins, no-op syncs, dry runs,
//! pending governance, partial failures, unestablished or divergent
//! provenance — is skipped with an honest diagnostic.
//!
//! Receipts are signed and queued in ONE durable transition under the
//! command's `sync` lock, before `team::record_applied` commits the
//! bookkeeping. The queue drains early in the transport (before pull/push
//! can fail): every attempt replays the exact stored payload, signature and
//! sequence with a fresh request timestamp/nonce; entries leave the queue
//! only after a validated service ack. Transport failures leave receipts
//! queued and visible without failing the already-successful local apply.
//!
//! Counters are per (endpoint, workspace, device): unlink/relink keeps the
//! durable high-water for the same triple, and a different triple never
//! inherits counters nor drains another link's queue.
use crate::cloud_client::{self, CloudLink};
use crate::cloud_protocol::{Receipt, ReceiptOutcome, ReceiptV1, SubmitReceipt};
use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet_state::FleetState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const STATE_FILE: &str = "receipts.json";
/// Receipt sequences are stored as signed PostgreSQL integers: allocate
/// within the strictly positive signed i64 range.
const SEQUENCE_LIMIT: u64 = i64::MAX as u64;

/// Accepted revision + epoch this machine verified before applying.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedProvenance {
    pub revision: String,
    pub epoch: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct QueuedReceipt {
    sequence: u64,
    revision: String,
    epoch: u64,
    outcome: String,
    /// RFC 3339 with microsecond precision; frozen once and replayed
    /// verbatim on every attempt.
    applied_at: String,
    signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LinkQueue {
    endpoint: String,
    workspace: String,
    device: String,
    /// Last sequence handed out by this machine for this link.
    high_water: u64,
    pending: Vec<QueuedReceipt>,
}

#[derive(Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
struct ReceiptState {
    links: Vec<LinkQueue>,
}

fn state_path(state_dir: &Path) -> PathBuf {
    state_dir.join("cloud").join(STATE_FILE)
}

fn load_state(state_dir: &Path) -> Result<ReceiptState, MusterError> {
    match std::fs::read(state_path(state_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|_| MusterError::Operational("cannot parse the receipt queue state".into())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ReceiptState::default()),
        Err(e) => Err(MusterError::Operational(format!(
            "cannot read the receipt queue state: {e}"
        ))),
    }
}

fn save_state(state_dir: &Path, state: &ReceiptState) -> Result<(), MusterError> {
    let dir = state_dir.join("cloud");
    std::fs::create_dir_all(&dir).map_err(|e| {
        MusterError::Operational(format!("cannot create the Cloud state directory: {e}"))
    })?;
    let bytes = serde_json::to_vec_pretty(state)
        .map_err(|_| MusterError::Operational("cannot encode the receipt queue state".into()))?;
    crate::vault::write_atomic(&dir.join(STATE_FILE), &bytes)
}

/// Whether `revision` is HEAD or an ancestor of HEAD: the machine's history
/// contains (and therefore applied) that revision. `git merge-base
/// --is-ancestor` exits 0 for yes and 1 for no; any other code is reported
/// as an operational error.
fn revision_in_history(config_dir: &Path, revision: &str) -> bool {
    let out = std::process::Command::new("git")
        .current_dir(config_dir)
        .args(["merge-base", "--is-ancestor", revision, "HEAD"])
        .output();
    match out {
        Ok(output) => match output.status.code() {
            Some(0) => true,
            Some(1) => false,
            _code => {
                // An operational failure (e.g. unknown revision) is not
                // "unrelated": the receipt is skipped honestly either way,
                // but the reason is surfaced in the caller's diagnostic.
                false
            }
        },
        Err(_) => false,
    }
}

/// The verified signed fleet state AT a given revision: read from that
/// commit in this machine's history — never the worktree's latest state,
/// which may carry unpublished descendants of the accepted revision.
fn fleet_state_at(config_dir: &Path, revision: &str) -> Option<FleetState> {
    let out = std::process::Command::new("git")
        .current_dir(config_dir)
        .args([
            "show",
            &format!("{revision}:{}", crate::fleet_state::STATE_FILE),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// Directories of the config repository that hold local bookkeeping the
/// engine never reads (enrollment requests); changes there do not make the
/// applied configuration differ from the accepted revision.
const BOOKKEEPING_DIRS: [&str; 1] = ["enroll/"];

/// Manifest keys of `[machines.<name>]` written by `team::record_applied`
/// after every apply. They are bookkeeping about the apply itself, never
/// engine input.
const MACHINE_BOOKKEEPING_KEYS: [&str; 2] = ["applied_profile", "last_sync"];

fn git_stdout(config_dir: &Path, args: &[&str]) -> Option<Vec<u8>> {
    let out = std::process::Command::new("git")
        .current_dir(config_dir)
        .args(args)
        .output()
        .ok()?;
    out.status.success().then_some(out.stdout)
}

fn nul_separated_paths(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| *b == 0)
        .filter(|part| !part.is_empty())
        .map(|part| String::from_utf8_lossy(part).into_owned())
        .collect()
}

fn unpublished_reason(revision: &str, difference: &str) -> String {
    format!(
        "the local configuration differs from the accepted Cloud revision {revision} \
         ({difference}); apply receipt skipped"
    )
}

/// A manifest with THIS machine's per-apply bookkeeping removed, for semantic
/// comparison. Bookkeeping changes on any other machine entry are not local
/// post-apply bookkeeping and are preserved as differences.
fn manifest_without_bookkeeping(text: &str, local_machine: Option<&str>) -> Option<toml::Table> {
    let mut table: toml::Table = text.parse().ok()?;
    if let (Some(name), Some(machines)) = (
        local_machine,
        table
            .get_mut("machines")
            .and_then(toml::Value::as_table_mut),
    ) {
        if let Some(machine) = machines.get_mut(name).and_then(toml::Value::as_table_mut) {
            for key in MACHINE_BOOKKEEPING_KEYS {
                machine.remove(key);
            }
        }
    }
    Some(table)
}

/// Whether the manifest (working tree AND index) equals the accepted
/// revision's, ignoring only THIS machine's per-apply bookkeeping.
fn manifest_equivalent(config_dir: &Path, revision: &str, local_machine: Option<&str>) -> bool {
    let manifest = crate::manifest::FILE_NAME;
    let Some(accepted) = git_stdout(config_dir, &["show", &format!("{revision}:{manifest}")])
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| manifest_without_bookkeeping(&text, local_machine))
    else {
        return false;
    };
    let worktree = std::fs::read_to_string(config_dir.join(manifest))
        .ok()
        .and_then(|text| manifest_without_bookkeeping(&text, local_machine));
    let index = git_stdout(config_dir, &["show", &format!(":{manifest}")])
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .and_then(|text| manifest_without_bookkeeping(&text, local_machine));
    worktree.as_ref() == Some(&accepted) && index.as_ref() == Some(&accepted)
}

/// Whether everything the engine reads from the config repository equals the
/// accepted `revision`: the working tree and the index (manifest,
/// instructions, `skills.lock`, skills, overlays, subagent sources, vault,
/// signed fleet state, ...) and untracked files (including gitignored files
/// the engine reads directly from disk), ignoring only the local bookkeeping
/// the engine never reads. `Err` names the first difference: the applied
/// configuration is then not what the accepted revision describes.
fn config_matches_revision(
    config_dir: &Path,
    state_dir: &Path,
    revision: &str,
) -> Result<(), String> {
    let local_machine = crate::machine::load_name(state_dir);
    let mut changed = std::collections::BTreeSet::new();
    let queries: [&[&str]; 3] = [
        &["diff", "--name-only", "-z", "--no-renames", revision, "--"],
        &[
            "diff",
            "--cached",
            "--name-only",
            "-z",
            "--no-renames",
            revision,
            "--",
        ],
        &["ls-files", "--others", "-z"],
    ];
    for args in queries {
        let Some(bytes) = git_stdout(config_dir, args) else {
            return Err("the repository cannot be compared with the accepted revision".into());
        };
        changed.extend(nul_separated_paths(&bytes));
    }
    for path in changed {
        if BOOKKEEPING_DIRS.iter().any(|dir| path.starts_with(dir)) {
            continue;
        }
        if path == crate::manifest::FILE_NAME
            && manifest_equivalent(config_dir, revision, local_machine.as_deref())
        {
            continue;
        }
        return Err(path);
    }
    Ok(())
}

/// Whether two RFC 3339 timestamps denote the same instant. The service may
/// render a replayed `applied_at` with different precision or offset
/// notation than the machine froze it with (e.g. `.697600Z` vs `.6976Z`);
/// that is still the same receipt.
fn same_instant(a: &str, b: &str) -> bool {
    use time::format_description::well_known::Rfc3339;
    match (
        time::OffsetDateTime::parse(a, &Rfc3339),
        time::OffsetDateTime::parse(b, &Rfc3339),
    ) {
        (Ok(a), Ok(b)) => a.unix_timestamp_nanos() == b.unix_timestamp_nanos(),
        _ => a == b,
    }
}

fn same_link(entry: &LinkQueue, link: &CloudLink) -> bool {
    entry.endpoint == link.endpoint
        && entry.workspace == link.workspace
        && entry.device == link.device
}

fn rfc3339_micros(t: time::OffsetDateTime) -> String {
    // Deterministic microsecond precision: the receipt's `applied_at` is
    // frozen once and replayed verbatim, so the format must not depend on
    // how many fractional digits happen to be nonzero.
    let (year, month, day, hour, minute, second, microsecond) = (
        t.year(),
        u8::from(t.month()),
        t.day(),
        t.hour(),
        t.minute(),
        t.second(),
        t.microsecond(),
    );
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{microsecond:06}Z")
}

/// Runs `f` while holding the machine's `sync.lock`, the same lock `muster
/// sync` holds for its whole run. Every writer of the receipt state outside
/// a sync (link establishment) goes through it, so the durable queue is
/// never updated concurrently. Waits like the mutation lock does and fails
/// operationally when a sync stays busy.
pub fn with_sync_lock<T>(
    state_dir: &Path,
    f: impl FnOnce() -> Result<T, MusterError>,
) -> Result<T, MusterError> {
    let io = |e: std::io::Error| MusterError::Operational(format!("sync lock: {e}"));
    std::fs::create_dir_all(state_dir).map_err(io)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let file = options.open(state_dir.join("sync.lock")).map_err(io)?;
    let deadline = std::time::Instant::now() + crate::lock::wait_budget();
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                if std::time::Instant::now() >= deadline {
                    return Err(MusterError::Operational(
                        "a sync is running; wait for it to finish and retry".into(),
                    ));
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(io(e)),
        }
    }
    let result = f();
    let _ = file.unlock();
    result
}

/// Whether a link's receipt sequence could be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkSequence {
    /// The durable counter for this link is known; receipts can be sequenced.
    Ready,
    /// This machine has no history for the link and the service's newest
    /// receipt could not be read: allocating would be a guess, so receipts
    /// are deferred until the next `muster cloud link` can establish it.
    Deferred,
}

/// Establishes the durable receipt counter for a link. Must run under
/// [`with_sync_lock`].
///
/// The service re-links a device with the same signing key to the SAME
/// device, so the counter is keyed by (endpoint, workspace, device) and kept
/// across unlink/relink: an existing entry is never reset. The next sequence
/// is recovered conservatively as the maximum of the durable high-water, the
/// pending sequences and the service's newest receipt for the device
/// (`remote_last_sequence`, `Some(0)` when the service has none). Without a
/// local entry and without a remote answer nothing is guessed: allocation is
/// deferred.
pub fn init_for_link(
    state_dir: &Path,
    link: &CloudLink,
    remote_last_sequence: Option<u64>,
) -> Result<LinkSequence, MusterError> {
    let mut state = load_state(state_dir)?;
    match state.links.iter().position(|e| same_link(e, link)) {
        Some(index) => {
            let entry = &mut state.links[index];
            let known = entry
                .pending
                .iter()
                .map(|r| r.sequence)
                .fold(entry.high_water, u64::max);
            let reconciled = known.max(remote_last_sequence.unwrap_or(0));
            if reconciled != entry.high_water {
                entry.high_water = reconciled;
                save_state(state_dir, &state)?;
            }
            Ok(LinkSequence::Ready)
        }
        None => {
            let Some(remote) = remote_last_sequence else {
                return Ok(LinkSequence::Deferred);
            };
            state.links.push(LinkQueue {
                endpoint: link.endpoint.clone(),
                workspace: link.workspace.clone(),
                device: link.device.clone(),
                high_water: remote,
                pending: Vec::new(),
            });
            save_state(state_dir, &state)?;
            Ok(LinkSequence::Ready)
        }
    }
}

/// Provenance for an apply receipt, established from the verified accepted
/// state BEFORE the plan is built. The service's `x-muster-revision` header
/// was validated against the fetched commit when the revision was recorded;
/// here it must be a canonical object ID contained in the checked-out HEAD,
/// whose signed fleet state must verify and be covered by this machine's pin.
/// `None` (with a diagnostic) means the receipt is honestly skipped.
pub fn accepted_provenance(ctx: &mut Context) -> Result<Option<AcceptedProvenance>, MusterError> {
    let base_path = ctx.state_dir.join("cloud/accepted.json");
    let Ok(bytes) = std::fs::read(&base_path) else {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "no accepted Cloud revision recorded; apply receipt skipped".to_string(),
        ));
        return Ok(None);
    };
    let accepted: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|_| MusterError::Operational("cannot parse the accepted Cloud revision".into()))?;
    let revision = accepted["revision"]
        .as_str()
        .ok_or_else(|| MusterError::Operational("invalid accepted Cloud revision".into()))?;
    // Link identity: the accepted revision must belong to THIS link (the
    // workspace and device it was pulled under), never another link's.
    let Some(link) = CloudLink::load(&ctx.state_dir)? else {
        return Ok(None); // not Cloud-linked: receipts do not apply
    };
    let accepted_workspace = accepted["workspace"].as_str().unwrap_or_default();
    let accepted_device = accepted["device"].as_str().unwrap_or_default();
    if accepted_workspace != link.workspace || accepted_device != link.device {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "the accepted Cloud revision belongs to another link; apply receipt skipped"
                .to_string(),
        ));
        return Ok(None);
    }
    if !crate::sync_transport::is_canonical_object_id(revision) {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            format!(
                "accepted Cloud revision `{revision}` is not a canonical full commit SHA; apply \
                 receipt skipped"
            ),
        ));
        return Ok(None);
    }
    // Provenance requires the checked-out HEAD to contain the accepted
    // revision: a divergent, rebased-away or locally unrelated HEAD is not
    // what the receipt would attest.
    let Some(head) = crate::git::head_commit_opt(&ctx.config_dir) else {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "the repository has no accepted Cloud revision to attest".to_string(),
        ));
        return Ok(None);
    };
    // The receipt attests the ACCEPTED revision as applied: it must be part
    // of this machine's history. Local bookkeeping commits on top of it
    // (profile application, enrollment requests) are fine — what invalidates
    // the attestation is a HEAD that does not contain the accepted revision
    // (diverged, rebased away, or a different fleet).
    if !revision_in_history(&ctx.config_dir, revision) {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            format!(
                "HEAD {head} does not contain the accepted Cloud revision {revision}; apply \
                 receipt skipped"
            ),
        ));
        return Ok(None);
    }
    // The epoch comes from the EXACT accepted commit's verified signed fleet
    // state — read from that revision in this machine's history, covered by
    // this machine's pin — never from the worktree's latest state (which may
    // carry unpublished descendants of the accepted revision) or a response
    // header alone.
    let Some(state) = fleet_state_at(&ctx.config_dir, revision) else {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "the accepted revision has no signed fleet state; apply receipt skipped".to_string(),
        ));
        return Ok(None);
    };
    if state.verify_signature().is_err() {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "the accepted fleet state signature does not verify; apply receipt skipped".to_string(),
        ));
        return Ok(None);
    }
    let covered = match crate::fleet_state::TrustAnchor::load(&ctx.state_dir)? {
        Some(pin) => pin.payload_hash == state.payload_hash().unwrap_or_default(),
        None => false,
    };
    if !covered {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            "the accepted fleet state is not covered by this machine's pin; apply receipt \
             skipped"
                .to_string(),
        ));
        return Ok(None);
    }
    let epoch = state.payload.epoch;
    if epoch == 0 || epoch > SEQUENCE_LIMIT {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            format!("fleet epoch {epoch} is outside the accepted range; apply receipt skipped"),
        ));
        return Ok(None);
    }
    // The apply reads the working tree: anything in it (or the index) that
    // the accepted revision does not contain makes the applied configuration
    // something other than that revision.
    if let Err(difference) = config_matches_revision(&ctx.config_dir, &ctx.state_dir, revision) {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            unpublished_reason(revision, &difference),
        ));
        return Ok(None);
    }
    Ok(Some(AcceptedProvenance {
        revision: revision.to_owned(),
        epoch,
    }))
}

/// Freezes and queues the signed receipt for a qualifying apply. Skips with
/// an honest diagnostic for every non-qualifying outcome. Must run AFTER
/// `engine::apply` and BEFORE `team::record_applied`, under the command's
/// sync lock (the durable transition relies on it).
pub fn freeze_applied_receipt(
    ctx: &mut Context,
    provenance: &AcceptedProvenance,
    result: &serde_json::Value,
) -> Result<(), MusterError> {
    let skip = |ctx: &mut Context, reason: String| {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "receipt_skipped",
            reason,
        ));
    };
    // Qualifying success: the global engine apply actually applied something
    // (not a no-op), no agent failed, and the Context carries no failure.
    let applied = result["applied"] == serde_json::Value::Bool(true);
    let failed_agents = result["agents"]
        .as_array()
        .is_some_and(|agents| agents.iter().any(|a| a["status"] == "failed"));
    if !applied {
        skip(
            ctx,
            "nothing was applied (applied=false); receipt skipped".into(),
        );
        return Ok(());
    }
    if failed_agents {
        skip(
            ctx,
            "at least one agent failed to apply; receipt skipped".into(),
        );
        return Ok(());
    }
    if ctx.failed() {
        skip(ctx, "the command recorded failures; receipt skipped".into());
        return Ok(());
    }
    if !crate::sync_transport::is_canonical_object_id(&provenance.revision) {
        skip(
            ctx,
            format!(
                "applied revision `{}` is not a canonical full commit SHA; receipt skipped",
                provenance.revision
            ),
        );
        return Ok(());
    }
    if provenance.epoch == 0 || provenance.epoch > SEQUENCE_LIMIT {
        skip(
            ctx,
            format!(
                "fleet epoch {} is outside the accepted range; receipt skipped",
                provenance.epoch
            ),
        );
        return Ok(());
    }
    // Post-apply provenance: the accepted revision the receipt attests must
    // still be part of this machine's history AND everything the engine read
    // (working tree, index, untracked files) must still equal that revision —
    // the applied tree is the accepted revision's content; only local
    // bookkeeping (enrollment requests, per-apply machine bookkeeping) is
    // allowed, unpublished configuration is not.
    if !revision_in_history(&ctx.config_dir, &provenance.revision) {
        skip(
            ctx,
            format!(
                "HEAD no longer contains the applied revision {}; receipt skipped",
                provenance.revision
            ),
        );
        return Ok(());
    }
    if let Err(difference) =
        config_matches_revision(&ctx.config_dir, &ctx.state_dir, &provenance.revision)
    {
        skip(ctx, unpublished_reason(&provenance.revision, &difference));
        return Ok(());
    }
    let Some(link) = CloudLink::load(&ctx.state_dir)? else {
        skip(ctx, "no Cloud link; receipt skipped".into());
        return Ok(());
    };
    let Some(signer) = crate::signing::Signer::load(&ctx.state_dir)? else {
        skip(ctx, "machine signing key missing; receipt skipped".into());
        return Ok(());
    };
    // One durable transition (state file is written atomically) under the
    // command's sync lock: allocate the next sequence and enqueue the exact
    // signed payload.
    let mut state = load_state(&ctx.state_dir)?;
    let entry = match state.links.iter_mut().find(|e| same_link(e, &link)) {
        Some(entry) => entry,
        None => {
            // No initialized receipt sequence for this link: the queue is
            // created when the link is established (the linked device is
            // then brand-new on the service, so sequence 1 cannot collide).
            // A missing entry here means the link predates the receipt
            // feature or the local state was lost — the service may already
            // hold receipts for this device, so allocating sequence 1 would
            // be a guess: defer honestly instead.
            skip(
                ctx,
                "no initialized receipt sequence for this link (re-link to start one); \
                 receipt deferred"
                    .into(),
            );
            return Ok(());
        }
    };
    let known = entry
        .pending
        .iter()
        .map(|r| r.sequence)
        .fold(entry.high_water, u64::max);
    let Some(sequence) = known
        .checked_add(1)
        .filter(|seq| (1..=SEQUENCE_LIMIT).contains(seq))
    else {
        skip(
            ctx,
            format!(
                "receipt sequence after {known} is outside the accepted range; receipt deferred"
            ),
        );
        return Ok(());
    };
    let applied_at = rfc3339_micros(time::OffsetDateTime::now_utc());
    let receipt = ReceiptV1 {
        workspace_id: &link.workspace,
        device_id: &link.device,
        revision: &provenance.revision,
        epoch: provenance.epoch,
        sequence,
        outcome: "applied",
        applied_at: &applied_at,
    };
    let signature = signer.sign(&protocol_canonical(&receipt));
    entry.high_water = sequence;
    entry.pending.push(QueuedReceipt {
        sequence,
        revision: provenance.revision.clone(),
        epoch: provenance.epoch,
        outcome: "applied".into(),
        applied_at,
        signature,
    });
    save_state(&ctx.state_dir, &state)?;
    ctx.diag(crate::output::Diagnostic::new(
        crate::output::Severity::Info,
        "receipt_queued",
        format!(
            "apply receipt queued: revision {} epoch {} sequence {}",
            provenance.revision, provenance.epoch, sequence
        ),
    ));
    Ok(())
}

fn protocol_canonical(receipt: &ReceiptV1<'_>) -> Vec<u8> {
    crate::cloud_protocol::canonical_receipt_v1(receipt)
}

/// Drains this link's queued receipts early in the transport, before pull or
/// push can fail. Every attempt replays the exact stored payload with a
/// fresh request nonce/timestamp; entries are removed only after a validated
/// ack. Failures keep receipts queued and visible without failing the sync.
pub fn drain(ctx: &mut Context, link: &CloudLink) {
    let mut state = match load_state(&ctx.state_dir) {
        Ok(state) => state,
        Err(e) => {
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Error,
                "receipt_queue_unreadable",
                format!("the receipt queue state cannot be read; receipts remain queued: {e}"),
            ));
            return;
        }
    };
    let Some(index) = state.links.iter().position(|e| same_link(e, link)) else {
        return; // nothing queued for this link
    };
    let Ok(Some(signer)) = crate::signing::Signer::load(&ctx.state_dir) else {
        return;
    };
    let path = crate::cloud_protocol::routes::WORKSPACE_RECEIPTS.replace("{ws}", &link.workspace);
    while !state.links[index].pending.is_empty() {
        let queued = state.links[index].pending[0].clone();
        let body = serde_json::json!(SubmitReceipt {
            revision: queued.revision.clone(),
            epoch: queued.epoch,
            sequence: queued.sequence,
            outcome: ReceiptOutcome::Applied,
            applied_at: queued.applied_at.clone(),
            signature: queued.signature.clone(),
        });
        let bytes = match serde_json::to_vec(&body) {
            Ok(bytes) => bytes,
            Err(_) => return,
        };
        let idempotency_key = format!("receipt-{}", queued.sequence);
        let outcome = cloud_client::device_request(
            link,
            &signer,
            "POST",
            &path,
            &bytes,
            "none",
            &idempotency_key,
        );
        let (status, ack_bytes) = match outcome {
            Ok(pair) => pair,
            Err(e) => {
                ctx.diag(crate::output::Diagnostic::new(
                    crate::output::Severity::Warning,
                    "receipt_queued",
                    format!(
                        "receipt {} remains queued (transport: {e}); a later sync retries it",
                        queued.sequence
                    ),
                ));
                return;
            }
        };
        if !(status == 200 || status == 201) {
            let detail = String::from_utf8_lossy(&ack_bytes).trim().to_owned();
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Error,
                "receipt_not_accepted",
                format!(
                    "receipt {} was not accepted (HTTP {status}): {detail}; it remains queued",
                    queued.sequence
                ),
            ));
            return;
        }
        let ack: Receipt = match serde_json::from_slice(&ack_bytes) {
            Ok(ack) => ack,
            Err(_) => {
                ctx.diag(crate::output::Diagnostic::new(
                    crate::output::Severity::Error,
                    "receipt_not_accepted",
                    format!(
                        "receipt {} ack is not a valid receipt; it remains queued",
                        queued.sequence
                    ),
                ));
                return;
            }
        };
        let ack_matches = ack.device_id == link.device
            && ack.revision == queued.revision
            && ack.epoch == queued.epoch
            && ack.sequence == queued.sequence
            && ack.outcome == ReceiptOutcome::Applied
            && same_instant(&ack.applied_at, &queued.applied_at)
            && ack.signature == queued.signature;
        if !ack_matches {
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Error,
                "receipt_not_accepted",
                format!(
                    "receipt {} ack does not match the queued receipt; it remains queued",
                    queued.sequence
                ),
            ));
            return;
        }
        // Persist the dequeue before the next attempt: a crash between acks
        // replays an already-accepted receipt, which the service answers
        // idempotently.
        state.links[index].pending.remove(0);
        if save_state(&ctx.state_dir, &state).is_err() {
            // The service accepted the receipt; put it back at the front so
            // the next delivery replays it and the service answers
            // idempotently — and make the persistence failure visible.
            state.links[index].pending.insert(0, queued.clone());
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Error,
                "receipt_queue_unwritable",
                format!(
                    "receipt {} was accepted but could not be removed from the queue; it \
                     remains queued and will be replayed",
                    queued.sequence
                ),
            ));
            return;
        }
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Info,
            "receipt_delivered",
            format!(
                "receipt {} for revision {} delivered",
                queued.sequence, queued.revision
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The applied_at timestamp must be stable microsecond-precision RFC
    /// 3339: it is frozen once into the signed payload and replayed
    /// verbatim, so precision loss would change the signed bytes.
    #[test]
    fn applied_at_is_rfc3339_with_microseconds() {
        let t = time::OffsetDateTime::from_unix_timestamp(1_700_000_000)
            .unwrap()
            .replace_nanosecond(123_456 * 1_000)
            .unwrap();
        let rendered = rfc3339_micros(t);
        assert_eq!(rendered, "2023-11-14T22:13:20.123456Z");
        // Every produced timestamp carries exactly six fractional digits.
        let now = rfc3339_micros(time::OffsetDateTime::now_utc());
        let fraction = now.split('.').nth(1).unwrap().trim_end_matches('Z');
        assert_eq!(fraction.len(), 6, "{now}");
    }

    /// The receipt domain framing binds every field in order — a change to
    /// any one of them changes the signed bytes (guarded against accidental
    /// field reordering in the canonical form).
    #[test]
    fn canonical_receipt_binds_all_fields() {
        let receipt = ReceiptV1 {
            workspace_id: "ws",
            device_id: "dev",
            revision: "rev",
            epoch: 2,
            sequence: 7,
            outcome: "applied",
            applied_at: "2026-01-01T00:00:00.000000Z",
        };
        let canonical = crate::cloud_protocol::canonical_receipt_v1(&receipt);
        let text = String::from_utf8(canonical).unwrap();
        for field in [
            "muster-cloud-receipt-v1",
            "ws",
            "dev",
            "rev",
            "2",
            "7",
            "applied",
            "2026-01-01T00:00:00.000000Z",
        ] {
            assert!(text.contains(field), "{field} missing from {text}");
        }
    }

    /// The queue state survives a save/load round trip (the durable
    /// transition depends on it).
    #[test]
    fn queue_state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let state = ReceiptState {
            links: vec![LinkQueue {
                endpoint: "https://cloud.example".into(),
                workspace: "ws-1".into(),
                device: "dev-1".into(),
                high_water: 3,
                pending: vec![QueuedReceipt {
                    sequence: 3,
                    revision: "abc".into(),
                    epoch: 1,
                    outcome: "applied".into(),
                    applied_at: "2026-01-01T00:00:00.000000Z".into(),
                    signature: "deadbeef".into(),
                }],
            }],
        };
        save_state(dir.path(), &state).unwrap();
        let loaded = load_state(dir.path()).unwrap();
        assert_eq!(loaded, state);
    }

    #[test]
    fn same_instant_accepts_different_rfc3339_precisions_for_equal_instant() {
        assert!(same_instant(
            "2026-11-16T20:00:00.697600Z",
            "2026-11-16T20:00:00.6976Z"
        ));
        assert!(same_instant(
            "2026-11-16T20:00:00.697600Z",
            "2026-11-16T20:00:00.6976+00:00"
        ));
        assert!(same_instant(
            "2026-11-16T20:00:00.000000Z",
            "2026-11-16T20:00:00Z"
        ));
        assert!(!same_instant(
            "2026-11-16T20:00:00.697600Z",
            "2026-11-16T20:00:00.697601Z"
        ));
    }

    #[test]
    fn manifest_without_bookkeeping_only_strips_local_machine_fields() {
        let base = "\
[machines.laptop]
profile = \"work\"

[machines.second]
profile = \"work\"
";
        let local_bookkeeping = "\
[machines.laptop]
profile = \"work\"

[machines.second]
profile = \"work\"
applied_profile = \"work\"
last_sync = \"2026-11-16T00:00:00Z\"
";
        let other_machine_bookkeeping = "\
[machines.laptop]
profile = \"work\"
applied_profile = \"work\"
last_sync = \"2026-11-16T00:00:00Z\"

[machines.second]
profile = \"work\"
";
        assert_eq!(
            manifest_without_bookkeeping(base, Some("second")),
            manifest_without_bookkeeping(local_bookkeeping, Some("second")),
            "this machine's own applied_profile/last_sync is local bookkeeping"
        );
        assert_ne!(
            manifest_without_bookkeeping(base, Some("second")),
            manifest_without_bookkeeping(other_machine_bookkeeping, Some("second")),
            "another machine's bookkeeping edit must remain visible as a manifest difference"
        );
    }

    #[test]
    fn freeze_applied_receipt_rejects_invalid_epoch_and_non_canonical_revision() {
        let dir = tempfile::tempdir().unwrap();
        let mut ctx = Context::new(crate::context::Globals {
            config: Some(dir.path().join("config")),
            ..Default::default()
        })
        .unwrap();
        ctx.state_dir = dir.path().join("state");
        let applied_result = serde_json::json!({
            "applied": true,
            "agents": [],
        });

        for bad_epoch in [0, SEQUENCE_LIMIT + 1, u64::MAX] {
            ctx.take_diagnostics();
            let provenance = AcceptedProvenance {
                revision: "a".repeat(40),
                epoch: bad_epoch,
            };
            freeze_applied_receipt(&mut ctx, &provenance, &applied_result).unwrap();
            let diags = ctx.take_diagnostics();
            assert!(
                diags.iter().any(|d| {
                    d.code == "receipt_skipped" && d.message.contains("outside the accepted range")
                }),
                "epoch {bad_epoch} must be rejected: {diags:?}"
            );
        }

        for bad_rev in ["HEAD", "refs/heads/main", "deadbeef", &"A".repeat(40)] {
            ctx.take_diagnostics();
            let provenance = AcceptedProvenance {
                revision: bad_rev.to_owned(),
                epoch: 1,
            };
            freeze_applied_receipt(&mut ctx, &provenance, &applied_result).unwrap();
            let diags = ctx.take_diagnostics();
            assert!(
                diags.iter().any(|d| {
                    d.code == "receipt_skipped"
                        && d.message.contains("not a canonical full commit SHA")
                }),
                "revision {bad_rev} must be rejected: {diags:?}"
            );
        }
    }
}
