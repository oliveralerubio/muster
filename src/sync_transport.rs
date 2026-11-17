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
    /// Local commits forked from the accepted history (a lost publication
    /// race): they were kept under `refs/muster/abandoned/<commit>` and their
    /// changes re-applied and re-sealed on top of the accepted head.
    Reconciled,
}

impl PullOutcome {
    pub fn label(self) -> &'static str {
        match self {
            PullOutcome::UpToDate => "up_to_date",
            PullOutcome::Updated => "updated",
            PullOutcome::Joined => "joined",
            PullOutcome::Reconciled => "reconciled",
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
    let signer = crate::signing::Signer::load(&ctx.state_dir)?
        .ok_or_else(|| failure("machine signing key missing"))?;
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    // A read is safe to repeat; each attempt is signed afresh.
    let mut response = crate::cloud_client::send_with_rate_limit(true, || {
        let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
        let nonce = random_hex()?;
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
        agent
            .get(format!("{}{}", link.endpoint, path_and_query))
            .header("Authorization", format!("Bearer {}", link.token()))
            .header(protocol::HEADER_DEVICE_ID, &link.device)
            .header(protocol::HEADER_TIMESTAMP, &timestamp)
            .header(protocol::HEADER_NONCE, &nonce)
            .header(protocol::HEADER_CONTENT_SHA256, &hash)
            .header(protocol::HEADER_SIGNATURE, &signature)
            .call()
            .map_err(|_| failure("Cloud download failed; retry muster sync --pull"))
    })?;
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
    let outcome = verify_and_checkout(ctx, link, bundle_str, &staging_str, previous.as_deref());
    let _ = std::fs::remove_dir_all(&staging);
    let _ = std::process::Command::new("git")
        .current_dir(&ctx.config_dir)
        .args(["update-ref", "-d", PULL_REF])
        .output();
    match outcome {
        Ok((outcome, fetched_tip, reconciled, record)) => {
            let _ = std::fs::remove_file(&bundle);
            crate::signed_replay::injected_crash("before-accepted");
            match tip {
                _ if !record && outcome == PullOutcome::UpToDate => {
                    ctx.diag(crate::output::Diagnostic::new(
                        crate::output::Severity::Warning,
                        "cloud_revision_stale",
                        format!(
                            "Cloud served {fetched_tip}, which is older than this machine's \
                             verified state; nothing changed and the accepted revision was \
                             kept. Retry `muster sync --pull` later."
                        ),
                    ));
                }
                _ if !record => {
                    ctx.diag(crate::output::Diagnostic::new(
                        crate::output::Severity::Warning,
                        "cloud_revision_unaccepted",
                        format!(
                            "Cloud served {fetched_tip}, whose signed state this machine's pin \
                             does not cover (a governance change still waiting for \
                             co-signatures); it was not recorded as the accepted revision."
                        ),
                    ));
                }
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
            match reconciled {
                Some(report) => report_reconciled(ctx, report).map(|()| outcome),
                None => Ok(outcome),
            }
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

/// What a reconciled pull did, reported once the accepted revision is
/// recorded.
pub(crate) struct Reconciled {
    backup: String,
    local: String,
    accepted: String,
    replay: crate::signed_replay::Replay,
    /// The tip waits for governance co-signatures: nothing was replayed.
    held: bool,
}

/// How the fetched tip is brought into this machine's history.
enum Plan {
    /// HEAD is an ancestor of the tip (or the history is unrelated).
    Reset,
    /// Plain rebase of unpublished commits. A failed rebase is rebuilt on the
    /// tip only with a `fork`: with `anchor` (only this machine sealed, and
    /// the rollback rule held: the fork base it accepted before its own
    /// never-accepted seals, which the tip still carries) the signed changes
    /// are replayed and re-sealed; without one (nothing local was sealed)
    /// only files outside the signed state are replayed, never sealed.
    /// Without a `fork` a failed rebase is refused, as before.
    Rebase {
        fork: Option<crate::signed_replay::Fork>,
        anchor: Option<Box<crate::fleet_state::TrustAnchor>>,
        /// This machine's own enrollment request reached the fleet (see
        /// [`superseded_own_request`]): `fork.local` is the local history
        /// rewritten without it, checked out before the rebase.
        superseded: Option<String>,
    },
    /// Both sides sealed since they forked and the rollback rule held:
    /// rebuild on the tip, verified from `anchor` (the accepted state before
    /// this machine's own never-accepted seals).
    Reconcile {
        fork: crate::signed_replay::Fork,
        anchor: Box<crate::fleet_state::TrustAnchor>,
    },
}

/// Prefix of the backup refs that keep abandoned local history.
const ABANDONED_REF: &str = "refs/muster/abandoned";
/// How many backups of abandoned history are kept (newest commits first).
const KEEP_ABANDONED: usize = 10;
/// How many set-aside pull journals are kept (newest first).
const KEEP_UNUSABLE_JOURNALS: usize = 5;

/// Keeps the `KEEP_ABANDONED` backups whose commits are newest, always
/// including `keep` (the one just written); older ones are deleted.
fn prune_abandoned(dir: &Path, keep: &str) {
    let Ok(out) = std::process::Command::new("git")
        .current_dir(dir)
        .args([
            "for-each-ref",
            "--sort=-committerdate",
            "--format=%(refname)",
            &format!("{ABANDONED_REF}/"),
        ])
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let listed = String::from_utf8_lossy(&out.stdout);
    let others = listed.lines().filter(|r| !r.is_empty() && *r != keep);
    for stale in others.skip(KEEP_ABANDONED.saturating_sub(1)) {
        let _ = git(dir, &["update-ref", "-d", stale]);
    }
}

/// Keeps the newest `KEEP_UNUSABLE_JOURNALS` set-aside journals (by their
/// timestamp), always including `keep`; older ones are deleted.
fn prune_unusable_journals(cloud: &Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(cloud) else {
        return;
    };
    let mut others: Vec<(u128, String)> = entries
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|name| name != keep)
        .filter_map(|name| {
            let stamp = name
                .strip_prefix("pull-journal.unusable-")?
                .strip_suffix(".json")?
                .parse()
                .ok()?;
            Some((stamp, name))
        })
        .collect();
    others.sort_by(|a, b| b.cmp(a));
    for (_, stale) in others.iter().skip(KEEP_UNUSABLE_JOURNALS.saturating_sub(1)) {
        let _ = std::fs::remove_file(cloud.join(stale));
    }
}

/// Staging verification and the checkout it guards. Returns the pull outcome
/// together with the exact commit hash fetched into `PULL_REF`. The error is a
/// plain message so the downloaded bundle and staging directory can be cleaned
/// up before the caller turns it into a `MusterError`.
/// The last element tells whether the fetched commit may be recorded as the
/// accepted revision (`false`: an older head than this machine's verified
/// state was served; nothing changed).
#[allow(clippy::type_complexity)]
fn verify_and_checkout(
    ctx: &mut Context,
    link: &CloudLink,
    bundle: &str,
    staging: &str,
    previous: Option<&str>,
) -> Result<(PullOutcome, String, Option<Reconciled>, bool), String> {
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
    // The served head is already part of this machine's history: nothing to
    // check out. It is recorded as the accepted revision only when that can
    // never move the record backwards (see `served_ancestor_is_current`);
    // this is how a pull heals a record lost after HEAD and the pin moved.
    // Otherwise it is a stale (or forged) answer: nothing changes at all.
    if previous.is_some()
        && git_bool(
            &ctx.config_dir,
            &["merge-base", "--is-ancestor", PULL_REF, "HEAD"],
        )
    {
        let record = served_ancestor_is_current(ctx, link, &fetched_tip);
        return Ok((PullOutcome::UpToDate, fetched_tip, None, record));
    }
    // Whether the served tip shares history with this machine's HEAD decides
    // between a plain update and a confirmed join. `git merge-base A B`
    // exits 0 with a common ancestor and 1 without; anything else (>1) is an
    // operational error that must surface as such — never as "unrelated".
    // There is no --quiet flag for merge-base (unknown options exit 129,
    // which used to make every pull look like an unrelated join).
    let mut base = None;
    let related = previous.is_some() && {
        let out = std::process::Command::new("git")
            .current_dir(&ctx.config_dir)
            .args(["merge-base", "HEAD", PULL_REF])
            .output()
            .map_err(|e| format!("cannot run git merge-base: {e}"))?;
        match out.status.code() {
            Some(0) => {
                base = Some(String::from_utf8_lossy(&out.stdout).trim().to_owned());
                true
            }
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
    let plan = match (related, previous, base) {
        (true, Some(local), Some(base))
            if !git_bool(
                &ctx.config_dir,
                &["merge-base", "--is-ancestor", "HEAD", PULL_REF],
            ) =>
        {
            let mut fork = crate::signed_replay::Fork {
                base,
                local: local.to_owned(),
                accepted: fetched_tip.clone(),
            };
            let superseded = superseded_own_request(ctx, &fork).and_then(|path| {
                let scratch = ctx.state_dir.join("cloud");
                crate::signed_replay::without_path(
                    &ctx.config_dir,
                    &scratch,
                    &fork.base,
                    &fork.local,
                    &path,
                )
                .ok()
                .flatten()
                .map(|tip| {
                    fork.local = tip;
                    path
                })
            });
            let state = crate::fleet_state::STATE_FILE;
            let dir = &ctx.config_dir;
            let local_sealed = crate::signed_replay::changed(dir, &fork.base, &fork.local, state);
            let tip_sealed = crate::signed_replay::changed(dir, &fork.base, &fork.accepted, state);
            if !local_sealed {
                Plan::Rebase {
                    fork: Some(fork),
                    anchor: None,
                    superseded,
                }
            } else {
                // Local seals are only ever rebuilt when they are this
                // machine's own, never accepted ones; anything else (seals
                // pulled from elsewhere, a publication that may have been
                // accepted, a served history holding them) is a plain
                // rebase checked against the pin, refused on conflict.
                match own_rollback_anchor(ctx, link, &fork, !tip_sealed).map(Box::new) {
                    Some(anchor) if tip_sealed => Plan::Reconcile { fork, anchor },
                    Some(anchor) => Plan::Rebase {
                        fork: Some(fork),
                        anchor: Some(anchor),
                        superseded,
                    },
                    None => Plan::Rebase {
                        fork: None,
                        anchor: None,
                        superseded,
                    },
                }
            }
        }
        _ => Plan::Reset,
    };
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
    // A recovered fork is checked from the state this machine accepted
    // before its own never-accepted seals (`own_rollback_anchor`).
    let rollback = match &plan {
        Plan::Reconcile { anchor, .. }
        | Plan::Rebase {
            anchor: Some(anchor),
            ..
        } => Some(anchor.as_ref()),
        _ => None,
    };
    let (verification, _) = crate::fleet_state::verify_against(
        Path::new(staging),
        &manifest,
        if related {
            rollback.or(pin.as_ref())
        } else {
            None
        },
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
    // Rewriting local history is journaled first: a crash anywhere below is
    // rolled back by the next command (`recover_interrupted_pull`), and a
    // refusal is rolled back here, untracked leftovers included.
    let journal = begin_journal(ctx, previous).map_err(|e| e.to_string())?;
    let journal = Some(journal);
    let result = checkout(
        ctx,
        plan,
        previous,
        related,
        own_scaffold_pin,
        journal.as_ref(),
    );
    if let Some(journal) = &journal {
        if result.is_err() {
            roll_back(&ctx.state_dir, journal).map_err(|e| {
                format!(
                    "cannot undo the refused pull ({e}); it is undone by the next muster command"
                )
            })?;
        } else {
            end_journal(&ctx.state_dir);
        }
    }
    // A verified tip is recorded only when this machine's pin covers it: a
    // tip still waiting for governance co-signatures leaves the pin on the
    // accepted state, and Cloud never accepts such a head, so it is never
    // recorded as accepted (the record would otherwise outrank the real
    // head, which `served_ancestor_is_current` never moves back to).
    result.map(|(outcome, reconciled)| {
        let record = served_ancestor_is_current(ctx, link, &fetched_tip);
        (outcome, fetched_tip, reconciled, record)
    })
}

/// Whether a served head (in HEAD's history: an ancestor, or the verified
/// tip just checked out) may become the recorded accepted revision. Never
/// when it precedes the revision already
/// recorded for this link (a stale or replayed answer must not erase that
/// this machine's state was accepted: the rollback rule and receipts rely on
/// it). Otherwise only when its signed state is the pinned one, or the
/// states between it and HEAD are all this machine's own seals leading from
/// it to the pin (its own publications or never-published re-seals).
fn served_ancestor_is_current(ctx: &Context, link: &CloudLink, served: &str) -> bool {
    use crate::signed_replay::state_at;
    let dir = &ctx.config_dir;
    let cloud = ctx.state_dir.join("cloud");
    if let Some(accepted) = load_accepted_for_link(link, &cloud) {
        if accepted != served
            && is_canonical_object_id(&accepted)
            && git_bool(dir, &["merge-base", "--is-ancestor", served, &accepted])
        {
            return false;
        }
    }
    let Ok(Some(pin)) = crate::fleet_state::TrustAnchor::load(&ctx.state_dir) else {
        return false;
    };
    let Some(state) = state_at(dir, served) else {
        return false;
    };
    let Ok(start) = state.payload_hash() else {
        return false;
    };
    if state.verify_signature().is_err() {
        return false;
    }
    if start == pin.payload_hash && state.payload.epoch == pin.epoch {
        return true;
    }
    own_seals_lead_to_pin(ctx, served, &start, state.payload.epoch, &pin)
}

/// Whether every signed state in `from..HEAD` (other than `from`'s own) is
/// this machine's validly signed seal, and they chain from `start` (epoch
/// `epoch`) to the pin. Every commit is read: merges and path simplification
/// cannot hide a foreign state.
fn own_seals_lead_to_pin(
    ctx: &Context,
    from: &str,
    start: &str,
    epoch: u64,
    pin: &crate::fleet_state::TrustAnchor,
) -> bool {
    use crate::signed_replay::state_at;
    let dir = &ctx.config_dir;
    let Some(own) = crate::signing::Signer::load(&ctx.state_dir)
        .ok()
        .flatten()
        .map(|s| s.public())
    else {
        return false;
    };
    let Ok(out) = std::process::Command::new("git")
        .current_dir(dir)
        .args(["rev-list", &format!("{from}..HEAD")])
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let mut seals = std::collections::HashMap::new();
    for commit in String::from_utf8_lossy(&out.stdout).lines() {
        let Some(state) = state_at(dir, commit) else {
            return false;
        };
        let Ok(hash) = state.payload_hash() else {
            return false;
        };
        if hash == start {
            continue;
        }
        if state.payload.signer != own || state.verify_signature().is_err() {
            return false;
        }
        seals.insert(hash, state);
    }
    // Back from the pin along `prev` to the served state.
    let (mut hash, mut at) = (pin.payload_hash.clone(), pin.epoch);
    for _ in 0..=seals.len() {
        if hash == start {
            return at == epoch;
        }
        let Some(state) = seals.get(&hash) else {
            return false;
        };
        let Some(prev) = state.payload.prev.clone() else {
            return false;
        };
        if state.payload.epoch != at || at == 0 {
            return false;
        }
        hash = prev;
        at -= 1;
    }
    false
}

/// The checkout that `verify_and_checkout` guards: the chain is verified,
/// nothing unverified reaches the working repository.
fn checkout(
    ctx: &mut Context,
    plan: Plan,
    previous: Option<&str>,
    related: bool,
    own_scaffold_pin: bool,
    journal: Option<&PullJournal>,
) -> Result<(PullOutcome, Option<Reconciled>), String> {
    let work = |args: &[&str]| git(&ctx.config_dir, args);
    let mut fallback = None;
    let mut note = None;
    match plan {
        Plan::Reset => {
            if !related {
                if let Some(prev) = previous {
                    let _ = work(&["update-ref", "refs/muster/pre-join", prev]);
                }
            }
            work(&["reset", "--quiet", "--hard", PULL_REF])
                .map_err(|_| "cannot check out the Cloud history".to_string())?;
            crate::signed_replay::injected_crash("after-checkout");
        }
        Plan::Rebase {
            fork,
            anchor,
            superseded,
        } => {
            if let (Some(path), Some(fork)) = (&superseded, &fork) {
                // Only this machine's own request leaves its local history;
                // every other local change is rebased as usual. A failure
                // below restores the original HEAD like `verify_pulled`.
                if work(&["reset", "--quiet", "--hard", &fork.local]).is_err() {
                    let _ = crate::git::restore_after_pull(&ctx.config_dir, previous);
                    return Err("cannot set this machine's local enrollment request aside".into());
                }
                note = Some(format!(
                    "the fleet already holds this machine's enrollment request ({path}); its \
                     local copy was set aside in favor of the fleet's"
                ));
            }
            let mut args: Vec<&str> = Vec::new();
            if !git_bool(&ctx.config_dir, &["config", "user.email"]) {
                args.extend([
                    "-c",
                    "user.name=Muster",
                    "-c",
                    "user.email=muster@localhost",
                ]);
            }
            args.extend(["rebase", "--quiet"]);
            if superseded.is_some() {
                // The commit that held only the request stays, empty: the
                // local history keeps the record of the request.
                args.push("--keep-empty");
            }
            args.push(PULL_REF);
            if work(&args).is_err() {
                let _ = work(&["rebase", "--abort"]);
                // A textual conflict is rebuilt like a signed fork (from the
                // history without the superseded request, if any; the backup
                // keeps the original HEAD).
                match fork {
                    Some(fork) => fallback = Some((fork, anchor)),
                    None => {
                        return Err("local changes conflict with the Cloud history; resolve \
                                    manually and retry muster sync --pull"
                            .into())
                    }
                }
            }
        }
        Plan::Reconcile { fork, anchor } => {
            let report = reconcile(ctx, previous, fork, Some(&anchor), journal)?;
            return Ok((PullOutcome::Reconciled, Some(report)));
        }
    }
    if let Some(note) = note {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Info,
            "own_request_superseded",
            note,
        ));
    }
    if let Some((fork, anchor)) = fallback {
        let report = reconcile(ctx, previous, fork, anchor.as_deref(), journal)?;
        return Ok((PullOutcome::Reconciled, Some(report)));
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
    Ok((outcome, None))
}

/// Rebuilds this machine's forked history on the verified tip: the abandoned
/// commits are kept under a backup ref, the tip is checked out and verified
/// (against `anchor`, else the pin), then the local changes are replayed on
/// top: re-sealed with an `anchor` (this machine's own never-accepted seals),
/// files outside the signed state only without one. A tip still waiting for
/// governance co-signatures is checked out but nothing is replayed on top of
/// an unaccepted proposal. A replay that fails hard leaves the machine on the
/// verified tip with its pin, reported as a collision.
fn reconcile(
    ctx: &mut Context,
    previous: Option<&str>,
    fork: crate::signed_replay::Fork,
    anchor: Option<&crate::fleet_state::TrustAnchor>,
    journal: Option<&PullJournal>,
) -> Result<Reconciled, String> {
    use crate::signed_replay::{Collision, Replay, Scope};
    let dir = ctx.config_dir.clone();
    // The backup keeps the machine's original HEAD, even when the replayed
    // history is a rewrite of it (a superseded enrollment request removed).
    let original = previous.unwrap_or(&fork.local).to_owned();
    let backup = format!("{ABANDONED_REF}/{original}");
    git(&dir, &["update-ref", &backup, &original])
        .map_err(|_| "cannot keep the local history under a backup ref".to_string())?;
    prune_abandoned(&dir, &backup);
    git(&dir, &["reset", "--quiet", "--hard", PULL_REF])
        .map_err(|_| "cannot check out the Cloud history".to_string())?;
    let rolled_back = crate::fleet_state::TrustAnchor::load(&ctx.state_dir)
        .ok()
        .flatten()
        .map(|p| p.payload_hash)
        .unwrap_or_default();
    match anchor {
        Some(anchor) => crate::trust::verify_pulled_from(ctx, previous, anchor, &rolled_back),
        None => crate::trust::verify_pulled(ctx, previous, false, true, false),
    }
    .map_err(|e| e.to_string())?;
    let report = |replay, held| Reconciled {
        backup: backup.clone(),
        local: original.clone(),
        accepted: fork.accepted.clone(),
        replay,
        held,
    };
    if ctx.governance_pending {
        return Ok(report(Replay::default(), true));
    }
    let accepted_pin =
        crate::fleet_state::TrustAnchor::load(&ctx.state_dir).map_err(|e| e.to_string())?;
    let scope = if anchor.is_some() {
        Scope::Signed
    } else {
        Scope::UnsignedOnly
    };
    // The rebuilt HEAD is verified like any other before the sync goes on:
    // a re-proposed governance change (quorum fleets) is held pending, so
    // nothing of it is applied before its co-signatures.
    let rebuilt = crate::signed_replay::replay(ctx, &fork, scope).and_then(|replay| {
        let manifest = crate::manifest::Manifest::load(&dir)?;
        crate::trust::verify(ctx, &manifest, false)?;
        Ok(replay)
    });
    let replay = match rebuilt {
        Ok(replay) => replay,
        Err(e) => {
            // Back to the verified tip and its pin: never half a replay,
            // and nothing the replay created is left behind.
            let _ = git(&dir, &["reset", "--quiet", "--hard", PULL_REF]);
            if let Some(journal) = journal {
                clean_untracked(&dir, &journal.untracked);
            }
            if let Some(pin) = &accepted_pin {
                pin.save(&ctx.state_dir).map_err(|e| e.to_string())?;
            }
            let mut replay = Replay::default();
            replay.collisions.push(Collision {
                subject: "the unpublished local changes".into(),
                hint: format!("they could not be re-applied ({e}); redo them"),
            });
            replay
        }
    };
    Ok(report(replay, false))
}

/// This machine's own enrollment request path (`enroll/<name>.toml`) when
/// its unpublished commits created or changed it and the fetched history
/// already carries it: the fleet's copy of the same request (name,
/// recipient, signing key and nonce all equal) or the machine enrolled with
/// this machine's recipient and signing key. Its local copy is then
/// redundant and would conflict with (or resurrect over) the fleet's.
///
/// Everything is read from git objects of this repository, never the
/// filesystem; both copies must be regular files (no symlink, no symlinked
/// parent). A renamed, revoked or differently-keyed machine never matches,
/// so no other path and no other machine's request is ever affected.
fn superseded_own_request(ctx: &Context, fork: &crate::signed_replay::Fork) -> Option<String> {
    use crate::signed_replay::{changed, manifest_at, request_at};
    let dir = &ctx.config_dir;
    let name = crate::machine::load_name(&ctx.state_dir)?;
    crate::machine::validate_name(&name).ok()?;
    let recipient = crate::identity::Identity::load(&ctx.state_dir)
        .ok()?
        .recipient();
    let signing_key = Some(crate::signing::Signer::load(&ctx.state_dir).ok()??.public());
    let path = crate::fleet::EnrollmentRequest::rel_path(&name);
    if !changed(dir, &fork.base, &fork.local, &path) {
        return None;
    }
    let ours = request_at(dir, &fork.local, &path)?;
    if ours.name != name || ours.recipient != recipient || ours.signing_key != signing_key {
        return None;
    }
    let is_us = |m: &crate::manifest::Manifest| {
        m.machines
            .get(&name)
            .is_some_and(|e| e.recipient == recipient && e.signing_key == signing_key)
    };
    // Still a request locally (not an already enrolled machine).
    if manifest_at(dir, &fork.local).is_none_or(|m| is_us(&m)) {
        return None;
    }
    let tip = manifest_at(dir, &fork.accepted)?;
    if tip.revoked_recipients.contains(&recipient) {
        return None;
    }
    let fleet_copy = request_at(dir, &fork.accepted, &path).is_some_and(|theirs| {
        theirs.name == name
            && theirs.recipient == recipient
            && theirs.signing_key == signing_key
            && theirs.nonce == ours.nonce
    });
    (fleet_copy || is_us(&tip)).then_some(path)
}

/// The rollback rule: the anchor to verify a forked tip against instead of
/// this machine's pin, or `None` (the pin applies, refused as before).
///
/// Holds only when the pin is this machine's OWN seal chain on top of the
/// fork base `P`, that chain was never accepted by Cloud, and the tip's state
/// is newer than `P` (the full chain from `P` is then verified by the caller
/// under the usual rules). The anchor is `P` itself, the state this machine
/// had accepted before sealing.
fn own_rollback_anchor(
    ctx: &Context,
    link: &CloudLink,
    fork: &crate::signed_replay::Fork,
    tip_unchanged: bool,
) -> Option<crate::fleet_state::TrustAnchor> {
    use crate::fleet_state::{governance_digest, STATE_FILE};
    use crate::signed_replay::{commits_touching, manifest_at, state_at};
    let dir = &ctx.config_dir;
    let pin = crate::fleet_state::TrustAnchor::load(&ctx.state_dir).ok()??;
    let own = crate::signing::Signer::load(&ctx.state_dir).ok()??.public();
    // P: the accepted state at the fork base, authentic with its governance.
    let base = state_at(dir, &fork.base)?;
    base.verify_signature().ok()?;
    let base_manifest = manifest_at(dir, &fork.base)?;
    if governance_digest(&base_manifest).ok()? != base.payload.governance {
        return None;
    }
    // The pin is the end of a contiguous chain of this machine's own seals.
    let mut hash = base.payload_hash().ok()?;
    let mut epoch = base.payload.epoch;
    let mut own_chain: Vec<String> = Vec::new();
    for commit in commits_touching(dir, &fork.base, &fork.local, STATE_FILE).ok()? {
        let state = state_at(dir, &commit)?;
        let next = state.payload_hash().ok()?;
        if next == hash && !own_chain.is_empty() {
            continue; // the same payload again (co-signatures only)
        }
        if state.payload.signer != own
            || state.verify_signature().is_err()
            || state.payload.prev.as_deref() != Some(hash.as_str())
            || state.payload.epoch != epoch + 1
        {
            return None;
        }
        hash = next;
        epoch += 1;
        own_chain.push(hash.clone());
    }
    // The pin is the end of that chain, or (quorum fleets) still `P`
    // itself: the own seals above it are governance changes held pending
    // for co-signatures. Either way the pin covers nothing the chain lacks.
    let base_hash = base.payload_hash().ok()?;
    let pinned_end = pin.payload_hash == hash && pin.epoch == epoch;
    let held_pending = pin.payload_hash == base_hash && pin.epoch == base.payload.epoch;
    if own_chain.is_empty()
        || !(pinned_end || held_pending)
        || state_at(dir, &fork.local)?.payload_hash().ok()? != hash
    {
        return None;
    }
    // Never accepted by Cloud: what this link recorded as accepted precedes
    // the fork, no publication of it may be in flight, and the served
    // history does not contain any of the own states.
    let cloud = ctx.state_dir.join("cloud");
    let accepted = load_accepted_for_link(link, &cloud)?;
    if !is_canonical_object_id(&accepted)
        || !git_bool(dir, &["merge-base", "--is-ancestor", &accepted, &fork.base])
        || cloud.join("push-intent.json").exists()
    {
        return None;
    }
    for commit in commits_touching(dir, &fork.base, &fork.accepted, STATE_FILE).ok()? {
        let served = state_at(dir, &commit)?.payload_hash().ok()?;
        if own_chain.contains(&served) {
            return None;
        }
    }
    // The tip descends from P (its chain is verified from P by the caller),
    // or, when only this machine sealed, carries exactly P.
    let tip = state_at(dir, &fork.accepted)?;
    let descends = if tip_unchanged {
        tip.payload_hash().ok()? == base.payload_hash().ok()?
    } else {
        tip.payload.epoch > base.payload.epoch
    };
    if !descends {
        return None;
    }
    crate::fleet_state::TrustAnchor::for_state(&base, &base_manifest, pin.tofu).ok()
}

/// Reports a reconciled pull; collisions make it an honest `conflict`.
fn report_reconciled(ctx: &mut Context, report: Reconciled) -> Result<(), MusterError> {
    use crate::output::{Diagnostic, Severity};
    let short = |c: &str| c.chars().take(12).collect::<String>();
    if report.held {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "signed_fork_pending",
            format!(
                "this machine's unpublished commits ({}) forked from Cloud's accepted head {}, \
                 which holds a governance change still waiting for admin signatures; nothing \
                 was re-applied on top of an unaccepted change. This machine's changes are kept \
                 at {}; redo them once the change is accepted.",
                short(&report.local),
                short(&report.accepted),
                report.backup
            ),
        ));
        return Ok(());
    }
    let replayed = if report.replay.replayed.is_empty() {
        "nothing needed re-applying".to_owned()
    } else {
        format!("re-applied: {}", report.replay.replayed.join(", "))
    };
    ctx.diag(Diagnostic::new(
        Severity::Info,
        "signed_fork_reconciled",
        format!(
            "this machine's unpublished commits ({}) forked from Cloud's accepted head {}; \
             they were rebuilt on top of it and re-sealed where signed ({replayed}). The abandoned \
             history is kept at {}. Next: `muster sync --push`.",
            short(&report.local),
            short(&report.accepted),
            report.backup
        ),
    ));
    if report.replay.collisions.is_empty() {
        return Ok(());
    }
    for collision in &report.replay.collisions {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "signed_change_conflict",
            format!(
                "conflict on {}: it was changed here and differently in the accepted state; \
                 the accepted version was kept ({}). This machine's version stays in {}.",
                collision.subject, collision.hint, report.backup
            ),
        ));
    }
    let subjects: Vec<&str> = report
        .replay
        .collisions
        .iter()
        .map(|c| c.subject.as_str())
        .collect();
    Err(MusterError::Conflict(format!(
        "conflict on {}: this machine and the accepted state changed it differently, so \
         nothing was guessed. This machine is on the accepted state with its other changes \
         re-applied; re-run the command to keep its version, then `muster sync --push`. The \
         abandoned history is kept at {}.",
        subjects.join(", "),
        report.backup
    )))
}

// ---------------------------------------------------------------------------
// Interrupted pulls
// ---------------------------------------------------------------------------

/// Journal of a pull that changes local history (checkout, rebase or
/// reconcile), under `<state_dir>/cloud/`.
const PULL_JOURNAL: &str = "pull-journal.json";

/// Everything needed to undo a history-changing pull: the repository it ran
/// on, the HEAD and pin it started from and the untracked files that existed
/// before it.
#[derive(Serialize, Deserialize)]
struct PullJournal {
    config_dir: std::path::PathBuf,
    previous: Option<String>,
    pin: Option<crate::fleet_state::TrustAnchor>,
    untracked: Vec<String>,
}

fn journal_path(state_dir: &Path) -> std::path::PathBuf {
    state_dir.join("cloud").join(PULL_JOURNAL)
}

fn untracked(dir: &Path) -> Result<Vec<String>, MusterError> {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(["ls-files", "--others", "-z"])
        .output()
        .map_err(|_| failure("cannot list untracked files"))?;
    if !out.status.success() {
        return Err(failure("cannot list untracked files"));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

fn begin_journal(ctx: &Context, previous: Option<&str>) -> Result<PullJournal, MusterError> {
    let journal = PullJournal {
        config_dir: std::fs::canonicalize(&ctx.config_dir)
            .map_err(|_| failure("cannot resolve the config repository"))?,
        previous: previous.map(str::to_owned),
        pin: crate::fleet_state::TrustAnchor::load(&ctx.state_dir)?,
        untracked: untracked(&ctx.config_dir)?,
    };
    let bytes = serde_json::to_vec_pretty(&journal)
        .map_err(|_| failure("cannot record the pull journal"))?;
    crate::vault::write_atomic(&journal_path(&ctx.state_dir), &bytes)?;
    Ok(journal)
}

fn end_journal(state_dir: &Path) {
    let _ = std::fs::remove_file(journal_path(state_dir));
}

/// Removes every untracked file that was not there before the pull.
fn clean_untracked(dir: &Path, before: &[String]) {
    let before: std::collections::BTreeSet<&str> = before.iter().map(String::as_str).collect();
    for rel in untracked(dir).unwrap_or_default() {
        if !before.contains(rel.as_str()) {
            crate::signed_replay::remove_leaf(dir, &rel);
        }
    }
}

/// Puts HEAD, the working tree and the pin back where the journaled pull
/// started (in the repository it ran on), then drops the journal.
fn roll_back(state_dir: &Path, journal: &PullJournal) -> Result<(), MusterError> {
    let config_dir = journal.config_dir.as_path();
    let _ = std::process::Command::new("git")
        .current_dir(config_dir)
        .args(["rebase", "--abort"])
        .output();
    crate::git::restore_after_pull(config_dir, journal.previous.as_deref())?;
    clean_untracked(config_dir, &journal.untracked);
    match &journal.pin {
        Some(pin) => pin.save(state_dir)?,
        None => match std::fs::remove_file(crate::fleet_state::TrustAnchor::path(state_dir)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(failure("cannot restore the trust pin"));
            }
            _ => {}
        },
    }
    end_journal(state_dir);
    Ok(())
}

/// Whether a history-changing pull was interrupted (its journal remains).
pub fn pull_interrupted(state_dir: &Path) -> bool {
    journal_path(state_dir).exists()
}

/// What [`recover_interrupted_pull`] did.
#[derive(Debug, PartialEq, Eq)]
pub enum PullRecovery {
    /// No interrupted pull.
    None,
    /// The pull was rolled back in the repository it ran on (which is not
    /// necessarily the current `--config`).
    RolledBack(std::path::PathBuf),
    /// The journal could not be used (unreadable, or its repository or
    /// commit is gone); it was set aside under the given name.
    SetAside(String),
}

/// Rolls an interrupted pull back to the history and pin it started from,
/// in the repository it ran on, so the next `muster sync --pull` redoes it
/// from scratch. A journal that cannot be used is set aside (renamed, never
/// deleted) instead of blocking every command. Called under the mutation
/// lock before anything verifies.
pub fn recover_interrupted_pull(state_dir: &Path) -> Result<PullRecovery, MusterError> {
    let path = journal_path(state_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(PullRecovery::None),
        Err(_) => return Err(failure("cannot read the pull journal")),
    };
    let usable = serde_json::from_slice::<PullJournal>(&bytes)
        .ok()
        .filter(|j| crate::git::is_repo(&j.config_dir))
        .filter(|j| {
            j.previous.as_deref().is_none_or(|previous| {
                is_canonical_object_id(previous)
                    && git_bool(
                        &j.config_dir,
                        &["cat-file", "-e", &format!("{previous}^{{commit}}")],
                    )
            })
        });
    let Some(journal) = usable else {
        let stamp = time::OffsetDateTime::now_utc().unix_timestamp_nanos();
        let aside = format!("pull-journal.unusable-{stamp}.json");
        std::fs::rename(&path, path.with_file_name(&aside))
            .map_err(|_| failure("cannot set the unusable pull journal aside"))?;
        if let Some(cloud) = path.parent() {
            prune_unusable_journals(cloud, &aside);
        }
        return Ok(PullRecovery::SetAside(aside));
    };
    roll_back(state_dir, &journal)?;
    Ok(PullRecovery::RolledBack(journal.config_dir))
}

/// The conflict details of a `409 revision_conflict` response, recognized by
/// the status and the structured error `code` only.
fn revision_conflict(status: u16, body: &str) -> Option<protocol::ConflictDetails> {
    if status != 409 {
        return None;
    }
    let error: protocol::ApiError = serde_json::from_str(body).ok()?;
    if error.error.code != protocol::error_codes::REVISION_CONFLICT {
        return None;
    }
    serde_json::from_value(error.error.details).ok()
}

/// Outcome of a Cloud publication.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushOutcome {
    /// Cloud's accepted head is now this machine's HEAD (or already was).
    Published,
    /// Cloud kept the publication as an open proposal (`202`, e.g. a
    /// governance change short of its admin quorum); the accepted head did
    /// not move and nothing is left in flight.
    Held {
        proposal_id: String,
        kind: protocol::ProposalKind,
    },
}

impl PushOutcome {
    /// The transport report fields of this outcome.
    pub fn report(&self) -> serde_json::Value {
        match self {
            PushOutcome::Published => serde_json::json!({"pushed": true}),
            PushOutcome::Held { proposal_id, kind } => serde_json::json!({
                "pushed": false,
                "held": {"proposal_id": proposal_id, "kind": kind.as_str()},
            }),
        }
    }
}

/// The publication Cloud last held as a proposal (`<state>/cloud/held.json`).
#[derive(Serialize, Deserialize)]
struct HeldRecord {
    workspace: String,
    device: String,
    base: String,
    revision: String,
    proposal_id: String,
    kind: protocol::ProposalKind,
}

fn load_held(link: &CloudLink, path: &Path) -> Option<HeldRecord> {
    let held: HeldRecord = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    (held.workspace == link.workspace && held.device == link.device).then_some(held)
}

/// What Cloud says about the proposal a held record names.
#[derive(Debug, PartialEq, Eq)]
enum HeldProposal {
    /// Cloud could not say (the request failed).
    Unknown,
    /// Not listed, or already resolved (whoever created it).
    Closed,
    /// Open and created by this machine's device.
    OpenOwn,
    /// Open under that id but not created by this machine's device (or
    /// without a creator at all): it must never be closed from here.
    NotOurs,
}

fn held_proposal(ctx: &Context, link: &CloudLink, id: &str) -> HeldProposal {
    let Ok(all) = crate::cloud_client::list_proposals(&ctx.state_dir, link) else {
        return HeldProposal::Unknown;
    };
    match all.iter().find(|p| p.id == id) {
        Some(p) if p.status == protocol::ProposalStatus::Open => {
            if p.device_id.as_deref() == Some(link.device.as_str()) {
                HeldProposal::OpenOwn
            } else {
                HeldProposal::NotOurs
            }
        }
        _ => HeldProposal::Closed,
    }
}

/// The held record names a proposal this machine did not create: the record
/// is forgotten by the caller (never rejected) and the operator is told.
fn warn_not_ours(ctx: &mut Context, old: &HeldRecord) {
    ctx.diag(crate::output::Diagnostic::new(
        crate::output::Severity::Warning,
        "cloud_proposal_not_ours",
        format!(
            "Cloud lists the earlier held proposal {} as created by another device, so this \
             machine did not close it and forgot its record; an admin can close it with \
             `muster cloud proposals resolve {} --reject` if it is stale",
            old.proposal_id, old.proposal_id
        ),
    ));
}

/// The held proposal this publication replaces is closed, so exactly one
/// stays open. Only a proposal this machine's device created is ever closed;
/// one that belongs to another device is left open and reported (the caller
/// has already replaced or removed its record). Best effort: Cloud decides
/// who may close it, and a refusal is reported instead.
fn supersede_held(ctx: &mut Context, link: &CloudLink, old: &HeldRecord, revision: &str) {
    if old.revision == revision {
        return;
    }
    match held_proposal(ctx, link, &old.proposal_id) {
        HeldProposal::OpenOwn => {}
        HeldProposal::NotOurs => return warn_not_ours(ctx, old),
        HeldProposal::Unknown | HeldProposal::Closed => return,
    }
    if crate::cloud_client::reject_proposal(&ctx.state_dir, link, &old.proposal_id).is_err() {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "cloud_proposal_not_superseded",
            format!(
                "the earlier held proposal {} is replaced by this publication but Cloud did \
                 not let this machine close it; an admin can close it with `muster cloud \
                 proposals resolve {} --reject`",
                old.proposal_id, old.proposal_id
            ),
        ));
    }
}

/// Publish a persistent exact bundle; a retry uses the same key and bytes.
/// The same commit already held as an open proposal is never sent again.
pub fn push(ctx: &mut Context, link: &CloudLink) -> Result<PushOutcome, MusterError> {
    let dir = ctx.state_dir.join("cloud");
    let meta = dir.join("push-intent.json");
    let bundle = dir.join("push-intent.bundle");
    let base_path = dir.join("accepted.json");
    let held_path = dir.join("held.json");
    let mut previous_held = load_held(link, &held_path);
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
                return Ok(PushOutcome::Published);
            }
            if let Some(held) = previous_held
                .as_ref()
                .filter(|h| h.revision == current && h.base == base)
            {
                // Still waiting (or Cloud cannot say right now): report the
                // existing proposal instead of opening a duplicate. A record
                // that points at another device's proposal is not ours to
                // report or close: forget it and publish normally.
                let state = held_proposal(ctx, link, &held.proposal_id);
                if state == HeldProposal::NotOurs {
                    let _ = std::fs::remove_file(&held_path);
                    warn_not_ours(ctx, held);
                    previous_held = None;
                } else if matches!(state, HeldProposal::Unknown | HeldProposal::OpenOwn) {
                    ctx.diag(crate::output::Diagnostic::new(
                        crate::output::Severity::Warning,
                        "cloud_proposal_held",
                        format!(
                            "this commit is already held by Cloud as {} proposal {}; nothing \
                             was sent again. It is accepted once its admin co-signatures \
                             arrive (`muster trust cosign` on another admin machine).",
                            held.kind.as_str(),
                            held.proposal_id
                        ),
                    ));
                    return Ok(PushOutcome::Held {
                        proposal_id: held.proposal_id.clone(),
                        kind: held.kind,
                    });
                }
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
    let signer = crate::signing::Signer::load(&ctx.state_dir)?
        .ok_or_else(|| failure("machine signing key missing"))?;
    let agent: ureq::Agent = ureq::config::Config::builder()
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build()
        .into();
    // The publication carries its idempotency key, so repeating it is safe
    // (the same key and bytes); each attempt is signed afresh. A limit that
    // outlasts the retry leaves the intent in place for the next run.
    let mut response = crate::cloud_client::send_with_rate_limit(true, || {
        let timestamp = time::OffsetDateTime::now_utc().unix_timestamp().to_string();
        let nonce = random_hex()?;
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
        agent
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
            .map_err(|_| failure("Cloud publication failed; retry muster sync --push"))
    })?;
    let status = response.status().as_u16();
    if status == 202 {
        // Held as a proposal: Cloud did not move its head, so the
        // publication is settled (never in flight) and the rollback rule may
        // rebuild it if another head is accepted meanwhile.
        let held: protocol::SyncHeld = response
            .body_mut()
            .read_json()
            .map_err(|_| failure("invalid Cloud publication response"))?;
        let _ = std::fs::remove_file(&meta);
        let _ = std::fs::remove_file(&bundle);
        let record = HeldRecord {
            workspace: link.workspace.clone(),
            device: link.device.clone(),
            base: intent.base.clone(),
            revision: intent.revision.clone(),
            proposal_id: held.proposal_id.clone(),
            kind: held.kind,
        };
        if let Ok(bytes) = serde_json::to_vec(&record) {
            let _ = crate::vault::write_atomic(&held_path, &bytes);
        }
        if let Some(old) = previous_held
            .as_ref()
            .filter(|o| o.proposal_id != held.proposal_id)
        {
            supersede_held(ctx, link, old, &intent.revision);
        }
        let waiting = match held.kind {
            protocol::ProposalKind::PendingQuorum => {
                "waits for the admin co-signatures its governance change needs (`muster trust \
                 cosign` on another admin machine)"
            }
            _ => "waits for an admin decision",
        };
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "cloud_proposal_held",
            format!(
                "Cloud kept this publication as {} proposal {}: the accepted head did not move \
                 and it {waiting}. Your local commits are untouched; `muster sync --pull` \
                 keeps them on top of whatever head is accepted meanwhile.",
                held.kind.as_str(),
                held.proposal_id
            ),
        ));
        return Ok(PushOutcome::Held {
            proposal_id: held.proposal_id,
            kind: held.kind,
        });
    }
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
        // A lost race is told apart by the service's structured error code,
        // never by its wording: the refused bundle is retained there as a
        // conflict proposal the operator can name and close.
        if let Some(conflict) = revision_conflict(status, detail) {
            return Err(MusterError::Conflict(format!(
                "Another machine published first: Cloud's accepted head is {}, so this \
                 publication was kept as conflict proposal {} and your local commits are \
                 untouched. Next: run `muster sync --pull` to rebase them on the accepted \
                 head, then `muster sync --push`; afterwards close the stale proposal with \
                 `muster cloud proposals resolve {} --reject`.",
                conflict.head, conflict.proposal_id, conflict.proposal_id
            )));
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
        // The same key and bytes would only replay this same answer, so
        // keeping the intent would fail every retry forever. Drop it (the
        // commit stays in the local history, nothing is recorded as
        // accepted) and let the next pull reconcile with whatever Cloud
        // really accepted.
        let _ = std::fs::remove_file(&meta);
        let _ = std::fs::remove_file(&bundle);
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Warning,
            "cloud_publication_unconfirmed",
            format!(
                "Cloud accepted the publication but named revision `{}` instead of `{}`; the \
                 pending publication was dropped and nothing was recorded as accepted. Your \
                 local commit is untouched. Run `muster sync --pull` to reconcile with what \
                 Cloud holds, then `muster sync --push` if it is still needed.",
                accepted.revision, intent.revision
            ),
        ));
        return Err(failure(
            "Cloud's answer did not confirm the publication; run muster sync --pull",
        ));
    }
    let state = serde_json::to_vec(&serde_json::json!({"workspace": link.workspace, "device": link.device, "revision": accepted.revision})).map_err(|_| failure("cannot encode Cloud revision"))?;
    crate::vault::write_atomic(&base_path, &state)
        .map_err(|_| failure("cannot save accepted Cloud revision"))?;
    std::fs::remove_file(&meta).map_err(|_| failure("cannot finish Cloud publication"))?;
    std::fs::remove_file(&bundle).map_err(|_| failure("cannot finish Cloud publication"))?;
    if let Some(old) = &previous_held {
        let _ = std::fs::remove_file(&held_path);
        supersede_held(ctx, link, old, &accepted.revision);
    }
    Ok(PushOutcome::Published)
}
