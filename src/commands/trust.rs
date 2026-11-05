//! `muster trust`: view the local trust anchor and re-sign after intentional
//! manual changes to `muster.toml`.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet_state::{
    self, governance_content, governance_digest, FleetState, TrustAnchor, Verification,
};
use crate::git;
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::signing::{self, Signer};
use crate::team;

pub const SHOW: CommandSpec = CommandSpec {
    name: "trust.show",
    about: "Print the current fleet state, local trust pin, and signer fingerprints",
    params: &[],
    mutating: false,
    handler: run_show,
};

pub const COSIGN: CommandSpec = CommandSpec {
    name: "trust.cosign",
    about: "Add an admin co-signature to a pending governance change",
    params: &[
        team::RECOVERY_KEY_PARAM,
        team::RECOVERY_KEY_STDIN_PARAM,
        ParamSpec {
            name: "yes",
            about: "Confirm the co-signing without an interactive prompt",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "reject",
            about: "Reject and discard the pending governance proposal, reverting to the last accepted pin",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: run_cosign,
};

pub const RESIGN: CommandSpec = CommandSpec {
    name: "trust.resign",
    about: "Re-sign fleet state after deliberate manual changes to muster.toml (admins only)",
    params: &[
        team::RECOVERY_KEY_PARAM,
        team::RECOVERY_KEY_STDIN_PARAM,
        ParamSpec {
            name: "yes",
            about: "Confirm the re-signing without an interactive prompt",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: run_resign,
};

pub const CHECKPOINT: CommandSpec = CommandSpec {
    name: "trust.checkpoint",
    about: "Mark the current accepted state as a bootstrap origin (recovery key or k admins)",
    params: &[
        team::RECOVERY_KEY_PARAM,
        team::RECOVERY_KEY_STDIN_PARAM,
        ParamSpec {
            name: "yes",
            about: "Confirm the checkpoint without an interactive prompt",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: run_checkpoint,
};

fn run_show(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let state = FleetState::load(&ctx.config_dir)?;
    let pin = TrustAnchor::load(&ctx.state_dir)?;
    let current_gov = governance_digest(&manifest)?;

    let state_json = match &state {
        Some(s) => {
            let fp = signing::fingerprint(&s.payload.signer);
            let valid = s.verify_signature().is_ok();
            json!({
                "epoch": s.payload.epoch,
                "prev": s.payload.prev,
                "signer": s.payload.signer,
                "signer_fingerprint": fp,
                "governance": s.payload.governance,
                "governance_match": s.payload.governance == current_gov,
                "vault_groups": s.payload.vault.keys().collect::<Vec<_>>(),
                "signature_valid": valid,
            })
        }
        None => Value::Null,
    };

    let pin_json = match &pin {
        Some(p) => json!({
            "epoch": p.epoch,
            "payload_hash": p.payload_hash,
            "governance": p.governance,
            "admins": p.admins.iter().map(|k| json!({
                "key": k,
                "fingerprint": signing::fingerprint(k),
            })).collect::<Vec<_>>(),
            "writers": p.writers.iter().map(|k| json!({
                "key": k,
                "fingerprint": signing::fingerprint(k),
            })).collect::<Vec<_>>(),
        }),
        None => Value::Null,
    };

    // Inspection only: `trust show` never creates or advances a pin.
    let inspected = fleet_state::verify_with(
        &ctx.config_dir,
        &ctx.state_dir,
        &manifest,
        &fleet_state::VerifyOptions {
            expect_signer: None,
            persist: false,
            check_content: true,
        },
    );
    let status = match inspected {
        Ok(Verification::Verified { .. }) => "verified",
        Ok(Verification::Pending { .. }) => "pending_signatures",
        Ok(Verification::Legacy) => "legacy_uninitialized",
        Err(e) => {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "verification_failed",
                e.to_string(),
            ));
            "unverified"
        }
    };

    Ok(json!({
        "status": status,
        "fleet_state": state_json,
        "trust_pin": pin_json,
        "current_governance_digest": current_gov,
    }))
}

/// Pushes the committed change. A failure is surfaced, never silent: peers do
/// not see the change until it is pushed. False when nothing reached a remote.
fn publish(ctx: &mut Context, what: &str) -> bool {
    if !git::has_remote(&ctx.config_dir) {
        return false;
    }
    match git::push(&ctx.config_dir) {
        Ok(()) => true,
        Err(e) => {
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "push_failed",
                format!("{what} committed but not pushed: {e}; run `muster sync --push`"),
            ));
            false
        }
    }
}

fn run_resign(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    fleet_state::ensure_not_behind_pin(&ctx.config_dir, &ctx.state_dir, "re-sign")?;
    let recovery = team::recovery_identity(ctx, params, &manifest)?;
    let signer = match recovery {
        Some(rec_id) => Signer::from_recovery(&rec_id)?,
        None => {
            let s = Signer::load_or_create(&ctx.state_dir)?;
            // Must be an admin if team mode
            if !manifest.members.is_empty() {
                let _ = team::admin(&manifest, &ctx.state_dir)?;
            }
            s
        }
    };

    // The signer must be an admin under the last state this machine accepted
    // (the pin decides who may change governance); with no pin, under the
    // manifest. Checked before anything is sealed.
    let public = signer.public();
    let pin = TrustAnchor::load(&ctx.state_dir)?;
    let is_admin = match &pin {
        Some(p) => p.admins.contains(&public),
        None => fleet_state::signer_sets(&manifest).0.contains(&public),
    };
    if !is_admin {
        return Err(MusterError::Security(format!(
            "cannot re-sign: signer {} is not an admin of this fleet",
            signing::fingerprint(&public)
        )));
    }

    // With no pin there is nothing to hold the manifest against, so the
    // manifest being signed could be exactly what an attacker wrote. Only a
    // machine that is the whole fleet (a fresh `init`) may re-sign then.
    if pin.is_none() && !only_enrolled_machine(ctx, &manifest) {
        return Err(MusterError::Security(
            "cannot re-sign: this machine has no trust pin and is not the only machine of \
             the fleet, so there is no accepted state to compare muster.toml with. Pin the \
             fleet with `muster sync --pull --expect-signer <fingerprint>` or restore the \
             repo from a trusted copy"
                .into(),
        ));
    }

    // Governance diff against what this machine last accepted; with no pin,
    // against the last signed state committed to the repo (everything counts
    // as added when there is none).
    let base = match pin.as_ref().and_then(|p| p.snapshot.clone()) {
        Some(snapshot) => Some(snapshot),
        None => last_signed_governance(&ctx.config_dir),
    };
    let render = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
    let governance_diff = crate::engine::diff::unified(
        &base.as_ref().map(render).unwrap_or_default(),
        &render(&governance_content(&manifest)),
        "governance",
        false,
    );
    let existing_state = FleetState::load(&ctx.config_dir)?;
    if let Some(existing) = &existing_state {
        if fleet_state::is_current(&ctx.config_dir, &manifest, existing)? {
            return Ok(json!({
                "resigned": false,
                "message": "fleet state is already up to date with the current muster.toml and vault",
                "epoch": existing.payload.epoch,
            }));
        }
    }

    if params.get_bool("yes") {
        ctx.yes = true;
    }
    // Always shown, `--yes` or not: the operator and the logs see what is signed.
    if governance_diff.is_empty() {
        eprintln!("Governance fields are unchanged; only vault hashes will be re-signed.");
    } else {
        eprintln!("Governance changes to be signed:\n{governance_diff}");
    }
    ctx.confirm("Re-sign fleet state with the current muster.toml security fields?")?;

    let state = fleet_state::seal(&ctx.config_dir, &ctx.state_dir, &manifest, &signer, true)?
        .ok_or_else(|| MusterError::Operational("sealing failed".into()))?;

    git::commit_if_configured(&ctx.config_dir, "feat(security): re-sign fleet state")?;
    let pushed = publish(ctx, "re-signed state");

    Ok(json!({
        "resigned": true,
        "pushed": pushed,
        "epoch": state.payload.epoch,
        "signer": state.payload.signer,
        "fingerprint": signing::fingerprint(&state.payload.signer),
        "governance": state.payload.governance,
        "governance_content": governance_content(&manifest),
        "governance_diff": governance_diff,
    }))
}

/// Whether `manifest` lists exactly one machine and it is this one.
fn only_enrolled_machine(ctx: &Context, manifest: &Manifest) -> bool {
    let Ok(identity) = crate::identity::Identity::load(&ctx.state_dir) else {
        return false;
    };
    manifest.machines.len() == 1
        && manifest
            .machines
            .values()
            .all(|m| m.recipient == identity.recipient())
}

/// Governance content of the newest committed `muster.toml` that matches the
/// digest of the `fleet.state.json` committed with it: what was last signed.
fn last_signed_governance(config_dir: &std::path::Path) -> Option<Value> {
    for commit in git::commits_touching(config_dir, fleet_state::STATE_FILE, 20) {
        let state: FleetState =
            serde_json::from_str(&git::file_at(config_dir, &commit, fleet_state::STATE_FILE)?)
                .ok()?;
        let text = git::file_at(config_dir, &commit, crate::manifest::FILE_NAME)?;
        let past = Manifest::parse(&text, crate::manifest::FILE_NAME).ok()?;
        if governance_digest(&past).ok()? == state.payload.governance {
            return Some(governance_content(&past));
        }
    }
    None
}

fn run_cosign(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let mut state = FleetState::load(&ctx.config_dir)?
        .ok_or_else(|| MusterError::Security("the repo has no fleet state to co-sign".into()))?;
    state.verify_signature()?;
    // Take every quorate epoch before the proposal first (best effort: the
    // checks below refuse anything that does not verify).
    let _ = fleet_state::verify_with(
        &ctx.config_dir,
        &ctx.state_dir,
        &manifest,
        &fleet_state::VerifyOptions {
            expect_signer: None,
            persist: true,
            check_content: true,
        },
    );
    let pin = TrustAnchor::load(&ctx.state_dir)?.ok_or_else(|| {
        MusterError::Security(
            "no local trust pin; cannot determine if this change is authorized".into(),
        )
    })?;

    if params.get_bool("reject") {
        return run_reject(ctx, params, &manifest, state, &pin);
    }
    if state.payload.epoch <= pin.epoch {
        return Ok(json!({
            "cosigned": false,
            "message": "the current fleet state is already accepted by this machine",
            "epoch": state.payload.epoch,
        }));
    }
    if state.payload.epoch != pin.epoch + 1 {
        return Err(MusterError::Security(format!(
            "cannot co-sign an epoch jump (state epoch {} > pinned {} + 1)",
            state.payload.epoch, pin.epoch
        )));
    }
    let recovery = team::recovery_identity(ctx, params, &manifest)?;
    let signer = match recovery {
        Some(rec_id) => Signer::from_recovery(&rec_id)?,
        None => {
            let s = Signer::load_or_create(&ctx.state_dir)?;
            if !manifest.members.is_empty() {
                let _ = team::admin(&manifest, &ctx.state_dir)?;
            }
            s
        }
    };
    let public = signer.public();
    if !pin.admins.contains(&public) {
        return Err(MusterError::Security(format!(
            "cannot co-sign: signer {} was not an admin of the last accepted state",
            signing::fingerprint(&public)
        )));
    }
    if state.payload.signer == public {
        return Err(MusterError::Security(
            "this machine signed the proposal; a different admin must co-sign it".into(),
        ));
    }
    if state.valid_cosigners().contains(&public) {
        return Ok(json!({
            "cosigned": false,
            "message": "this machine has already co-signed the proposal",
            "epoch": state.payload.epoch,
        }));
    }
    // The signature covers the payload's digests, so the working tree must be
    // exactly what they describe before anything is shown or signed.
    fleet_state::verify_with(
        &ctx.config_dir,
        &ctx.state_dir,
        &manifest,
        &fleet_state::VerifyOptions {
            expect_signer: None,
            persist: false,
            check_content: true,
        },
    )
    .map_err(|e| match e {
        MusterError::Security(m) => MusterError::Security(format!(
            "refusing to co-sign: the working tree does not match the proposed fleet state \
             ({m})"
        )),
        other => other,
    })?;
    // Show the operator the exact governance diff vs their pinned baseline,
    // and which vault groups changed (names only, never values).
    let diff = fleet_state::governance_diff(pin.snapshot.as_ref(), &governance_content(&manifest));
    let groups = fleet_state::changed_groups(
        &pin.vault,
        &pin.vault_meta,
        &state.payload.vault,
        &state.payload.vault_meta,
    );
    eprintln!("Governance changes to co-sign:\n{diff}");
    if groups.is_empty() {
        eprintln!("Vault groups changed: none");
    } else {
        eprintln!("Vault groups changed: {}", groups.join(", "));
    }
    if params.get_bool("yes") {
        ctx.yes = true;
    }
    ctx.confirm("Add your admin co-signature to this governance change?")?;

    state.add_cosignature(&signer)?;
    state.save(&ctx.config_dir)?;

    // If the new signature completed the quorum, advance the pin.
    let verified = fleet_state::verify_with(
        &ctx.config_dir,
        &ctx.state_dir,
        &manifest,
        &fleet_state::VerifyOptions {
            expect_signer: None,
            persist: true,
            check_content: true,
        },
    )?;
    let accepted = matches!(verified, Verification::Verified { .. });

    git::commit_if_configured(&ctx.config_dir, "feat(security): co-sign governance change")?;
    let pushed = publish(ctx, "co-signature");
    Ok(json!({
        "cosigned": true,
        "pushed": pushed,
        "epoch": state.payload.epoch,
        "accepted": accepted,
        "signer": public,
        "fingerprint": signing::fingerprint(&public),
        "cosigners": state.valid_cosigners(),
        "vault_groups_changed": groups,
    }))
}

/// `trust cosign --reject`: discards the pending proposal at the head of the
/// repo and restores the newest ACCEPTED state, recomputed from the history
/// (the local pin may lag behind quorate epochs). It takes as many distinct
/// admins of the accepted state as the team requires, collected as signed
/// rejections in `fleet.state.json` the same way co-signatures are, or the
/// recovery key alone. A lone admin can therefore never veto a proposal, least
/// of all its own removal.
fn run_reject(
    ctx: &mut Context,
    params: &Params,
    manifest: &Manifest,
    mut state: FleetState,
    pin: &TrustAnchor,
) -> Result<Value, MusterError> {
    let proposal = fleet_state::pending_proposal(&ctx.config_dir, &ctx.state_dir, manifest)?;
    if proposal.accepted_epoch < pin.epoch {
        return Err(MusterError::Security(
            "refusing to reject: that would drop an epoch this machine already accepted".into(),
        ));
    }
    let signer = match team::recovery_identity(ctx, params, manifest)? {
        Some(rec_id) => Signer::from_recovery(&rec_id)?,
        None => Signer::load_or_create(&ctx.state_dir)?,
    };
    let public = signer.public();
    if !proposal.is_admin(&public) {
        return Err(MusterError::Security(
            "cannot reject proposal: must be an admin of the last accepted state (or provide \
             the recovery key)"
                .into(),
        ));
    }
    if params.get_bool("yes") {
        ctx.yes = true;
    }
    ctx.confirm("Reject the unaccepted pending proposal (and revert to the last accepted state once enough admins did)?")?;

    state.add_rejection(&signer)?;
    let (received, required) = proposal.reject_tally(&state);
    if received < required {
        // Recorded and shared; the revert waits for the other admins.
        state.save(&ctx.config_dir)?;
        git::commit_if_configured(&ctx.config_dir, "feat(security): record proposal rejection")?;
        let pushed = publish(ctx, "rejection");
        return Ok(json!({
            "rejected": false,
            "rejections_received": received,
            "rejections_required": required,
            "pushed": pushed,
            "message": format!(
                "rejection recorded ({received} of {required}); another admin of the accepted \
                 state must run `muster trust cosign --reject` too"
            ),
        }));
    }

    let accepted =
        git::commit_with_state(&ctx.config_dir, &proposal.accepted_hash).ok_or_else(|| {
            MusterError::Security(
                "the accepted state is not in the repository history; restore the repo from a \
                 trusted copy"
                    .into(),
            )
        })?;
    let head = git::head_commit(&ctx.config_dir)?;
    // The signed files only: state, governance and the whole vault.
    let signed_paths = [fleet_state::STATE_FILE, crate::manifest::FILE_NAME, "vault"];
    git::restore_paths(&ctx.config_dir, &accepted, &signed_paths)?;
    let restored = Manifest::load(&ctx.config_dir).and_then(|m| {
        fleet_state::verify_with(
            &ctx.config_dir,
            &ctx.state_dir,
            &m,
            &fleet_state::VerifyOptions {
                expect_signer: None,
                persist: true,
                check_content: true,
            },
        )
    });
    if !matches!(restored, Ok(Verification::Verified { .. })) {
        git::restore_paths(&ctx.config_dir, &head, &signed_paths)?;
        return Err(MusterError::Security(
            "the accepted state could not be restored cleanly; nothing was changed".into(),
        ));
    }
    git::commit_if_configured(
        &ctx.config_dir,
        "revert: discard unaccepted pending proposal",
    )?;
    let pushed = publish(ctx, "revert");
    Ok(json!({
        "rejected": true,
        "reverted_to_epoch": proposal.accepted_epoch,
        "rejections_received": received,
        "rejections_required": required,
        "pushed": pushed,
    }))
}

fn run_checkpoint(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    fleet_state::ensure_not_behind_pin(&ctx.config_dir, &ctx.state_dir, "checkpoint")?;
    let pin = TrustAnchor::load(&ctx.state_dir)?
        .ok_or_else(|| MusterError::Security("no local trust pin; cannot checkpoint".into()))?;
    let recovery = team::recovery_identity(ctx, params, &manifest)?;
    let signer = match recovery {
        Some(rec_id) => Signer::from_recovery(&rec_id)?,
        None => {
            let s = Signer::load_or_create(&ctx.state_dir)?;
            if !manifest.members.is_empty() {
                let _ = team::admin(&manifest, &ctx.state_dir)?;
            }
            s
        }
    };
    let public = signer.public();
    if !pin.admins.contains(&public) {
        return Err(MusterError::Security(format!(
            "cannot checkpoint: signer {} is not an admin of this fleet",
            signing::fingerprint(&public)
        )));
    }
    if params.get_bool("yes") {
        ctx.yes = true;
    }
    ctx.confirm("Mark the current accepted fleet state as a bootstrap checkpoint?")?;

    let state = fleet_state::seal_checkpoint(&ctx.config_dir, &ctx.state_dir, &manifest, &signer)?;
    git::commit_if_configured(
        &ctx.config_dir,
        "feat(security): mark fleet state checkpoint",
    )?;
    let pushed = publish(ctx, "checkpoint");

    let pending = fleet_state::pending_quorum(&ctx.state_dir, &state, &manifest)?;
    Ok(json!({
        "checkpoint": true,
        "accepted": pending.is_none(),
        "approvals_received": pending.map(|(received, _)| received),
        "approvals_required": pending.map(|(_, required)| required),
        "pushed": pushed,
        "epoch": state.payload.epoch,
        "signer": state.payload.signer,
        "fingerprint": signing::fingerprint(&state.payload.signer),
    }))
}
