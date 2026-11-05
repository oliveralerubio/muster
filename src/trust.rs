//! Glue between commands and [`crate::fleet_state`]: verify the repo before a
//! command consumes security state, and re-sign as part of every commit that
//! changes it. See `docs/adr/0005-signed-fleet-state.md`.

use std::io::BufRead;
use std::path::Path;

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet_state::{self, FleetState, TrustAnchor, Verification, VerifyOptions};
use crate::git;
use crate::identity::Identity;
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::signing::{self, Signer};

/// Commands that consume security state (recipients, vault ciphertext,
/// roles, secret policies) and so verify the signed fleet state first.
/// Everything else only gets the one-time legacy migration below.
const SECURITY_CONSUMING: &[&str] = &[
    "sync",
    "status",
    "doctor",
    "approve",
    "reject",
    "revoke",
    "enroll",
    "machine.add",
    "machine.rm",
    "machine.assign",
    "agents.apply",
    "plan",
    "effective",
    "member.add",
    "member.approve",
    "member.reject",
    "member.rm",
    "member.role",
    "member.profile",
    "group.create",
    "group.rekey",
    "group.grant",
    "group.revoke",
    "secret.get",
    "secret.approve",
    "secret.check",
    "secret.deny",
    "secret.revoke",
    "secret.requests",
    "secret.leases",
    "secret.add",
    "secret.rotate",
    "secret.rm",
    "import.bws",
];

/// Pre-flight for a registry command: verify the fleet state when the config
/// repo has one (a pin or a `fleet.state.json`), and migrate a legacy repo
/// when a mutating command runs on an enrolled machine.
pub fn guard(ctx: &mut Context, command: &str, mutating: bool) -> Result<(), MusterError> {
    // A revocation interrupted mid-publication is completed (or discarded)
    // before anything reads the repo: no user action needed.
    if !ctx.dry_run && crate::revocation_tx::recover(&ctx.config_dir, &ctx.state_dir)? {
        ctx.diag(Diagnostic::new(
            Severity::Warning,
            "revocation_recovered",
            "an interrupted revocation was completed automatically",
        ));
    }
    if command == "init" || command == "uninstall" || command.starts_with("trust.") {
        return Ok(());
    }
    // Every mutating command verifies in full, even one not listed as
    // security-consuming: otherwise `seal_if_changed` would sign (launder) an
    // attacker's edit to the working tree. A repo with neither a signed state
    // nor a pin verifies as `Legacy` and is migrated below.
    let consuming = SECURITY_CONSUMING.contains(&command) || mutating;
    if !consuming {
        return Ok(());
    }
    let Ok(manifest) = Manifest::load(&ctx.config_dir) else {
        return Ok(());
    };
    let verification = match verify(ctx, &manifest, !ctx.dry_run) {
        // `doctor` reports a failed verification next to its other findings.
        Err(MusterError::Security(message)) if command == "doctor" => {
            ctx.diag(Diagnostic::new(
                Severity::Error,
                "fleet_state_invalid",
                message,
            ));
            return Ok(());
        }
        other => other?,
    };
    if verification == Verification::Legacy && mutating && !ctx.dry_run {
        migrate_legacy(ctx, manifest)?;
    }
    Ok(())
}

/// Asks the user on a terminal to type `expected` back. Never satisfied by
/// `--yes`; without a terminal the `refusal` is returned as a Security error.
fn confirm_typed(
    ctx: &Context,
    intro: &str,
    expected: &str,
    refusal: &str,
) -> Result<(), MusterError> {
    if !ctx.is_tty {
        return Err(MusterError::Security(refusal.to_string()));
    }
    eprintln!("{intro}");
    eprintln!("  {expected}");
    eprint!("Type the fingerprint above to trust it: ");
    let _ = std::io::Write::flush(&mut std::io::stderr());
    let mut answer = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut answer)
        .map_err(|e| MusterError::Operational(format!("cannot read answer: {e}")))?;
    if answer.trim().eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(MusterError::Security(
            "the fingerprint typed does not match; nothing was pinned".into(),
        ))
    }
}

/// First use on this machine: there is no pin, the repo carries a signed
/// state, and nothing named its signer. The signer must come from
/// `--expect-signer`, a typed confirmation on a terminal, or be this
/// machine's own key.
fn confirm_first_use(ctx: &Context) -> Result<(), MusterError> {
    if ctx.expect_signer.is_some() || TrustAnchor::load(&ctx.state_dir)?.is_some() {
        return Ok(());
    }
    let Some(state) = FleetState::load(&ctx.config_dir)? else {
        return Ok(());
    };
    let signer = &state.payload.signer;
    if Signer::load(&ctx.state_dir)?.is_some_and(|own| &own.public() == signer) {
        return Ok(());
    }
    let fingerprint = signing::fingerprint(signer);
    confirm_typed(
        ctx,
        "This machine has not pinned a fleet yet. The repository's state is signed by:",
        &fingerprint,
        &format!(
            "first use: this machine has not pinned a fleet and the repo is signed by \
             {fingerprint}. Compare that with `muster trust show` on a machine you already \
             trust, then re-run with --expect-signer <fingerprint> (or run interactively to \
             confirm it)"
        ),
    )
}

/// Verifies the repo against the pin, reporting a first-use pin as an info diagnostic.
pub fn verify(
    ctx: &mut Context,
    manifest: &Manifest,
    persist: bool,
) -> Result<Verification, MusterError> {
    // Whatever the entry point, creating a first pin from someone else's
    // signature needs `--expect-signer` or a typed confirmation.
    if persist {
        confirm_first_use(ctx)?;
    }
    let result = fleet_state::verify_with(
        &ctx.config_dir,
        &ctx.state_dir,
        manifest,
        &VerifyOptions {
            expect_signer: ctx.expect_signer.as_deref(),
            persist,
            check_content: true,
        },
    )?;
    if let Verification::Verified {
        epoch,
        signer,
        first_use: true,
    } = &result
    {
        ctx.diag(Diagnostic::new(
            Severity::Info,
            "fleet_signer_pinned",
            format!(
                "pinned fleet signer {} at epoch {epoch}; check this fingerprint out of band \
                 with the admin who approved you (or pass --expect-signer)",
                signing::fingerprint(signer)
            ),
        ));
    }
    Ok(result)
}

/// The machine name of this machine in `manifest`, matched by age recipient.
fn local_machine(ctx: &Context, manifest: &Manifest) -> Option<String> {
    let recipient = Identity::load(&ctx.state_dir).ok()?.recipient();
    let local = crate::machine::load_name(&ctx.state_dir);
    manifest
        .machines
        .iter()
        .filter(|(_, m)| m.recipient == recipient)
        .map(|(name, _)| name.clone())
        .max_by_key(|name| Some(name) == local.as_ref())
}

/// A repo from before signed state: the first mutating command on an
/// enrolled machine signs epoch 1 (no user step), publishing this machine's
/// signing key. In team mode only an admin's machine may be the root.
fn migrate_legacy(ctx: &mut Context, mut manifest: Manifest) -> Result<(), MusterError> {
    let Some(name) = local_machine(ctx, &manifest) else {
        return Ok(());
    };
    if !manifest.members.is_empty() && crate::team::admin(&manifest, &ctx.state_dir).is_err() {
        return Ok(());
    }
    let signer = Signer::load_or_create(&ctx.state_dir)?;
    // Signing epoch 1 makes this machine the root of trust. That is automatic
    // only when it is the whole fleet (it ran `init`); otherwise someone must
    // confirm that this machine should be the root.
    if manifest.machines.len() > 1 {
        let fingerprint = signing::fingerprint(&signer.public());
        confirm_typed(
            ctx,
            "This repo has no signed fleet state. This machine would sign it and become the \
             root of trust, with signing key:",
            &fingerprint,
            &format!(
                "this repo has no signed fleet state and has other machines; refusing to sign \
                 it from this machine ({fingerprint}) without confirmation. Run the command \
                 interactively, or on the machine that ran `muster init`"
            ),
        )?;
    }
    let entry = manifest
        .machines
        .get_mut(&name)
        .expect("machine found just above");
    match entry.signing_key.as_deref() {
        Some(published) if published != signer.public() => {
            return Err(MusterError::Security(format!(
                "machine '{name}' publishes a signing key that is not the one in {}; \
                 refusing to create the signed fleet state",
                ctx.state_dir.display()
            )))
        }
        Some(_) => {}
        None => {
            entry.signing_key = Some(signer.public());
            manifest.save(&ctx.config_dir)?;
        }
    }
    fleet_state::seal(&ctx.config_dir, &ctx.state_dir, &manifest, &signer, true)?;
    ctx.diag(Diagnostic::new(
        Severity::Warning,
        "fleet_state_initialized",
        format!(
            "this repo had no signed fleet state; machine '{name}' signed epoch 1 ({}). Other \
             machines pin it on their next sync; verify the fingerprint out of band",
            signing::fingerprint(&signer.public())
        ),
    ));
    Ok(())
}

/// Commit the working tree, re-signing the fleet state first when the
/// security fields or the vault changed. Replaces `git::commit_if_configured`
/// for every command that can change them.
pub fn commit(ctx: &mut Context, message: &str) -> Result<bool, MusterError> {
    if !ctx.dry_run {
        fleet_state::ensure_not_behind_pin(&ctx.config_dir, &ctx.state_dir, "commit on top of")?;
    }
    seal_if_changed(ctx)?;
    git::commit_if_configured(&ctx.config_dir, message)
}

fn seal_if_changed(ctx: &mut Context) -> Result<(), MusterError> {
    if ctx.dry_run || !git::is_repo(&ctx.config_dir) {
        return Ok(());
    }
    let dir = ctx.config_dir.clone();
    seal_in(ctx, &dir, true)?;
    Ok(())
}

/// Re-signs the repository content under `dir` when it changed. With
/// `persist_pin` false the local pin is left alone (a staged transaction
/// advances it when it publishes).
pub(crate) fn seal_in(
    ctx: &mut Context,
    dir: &Path,
    persist_pin: bool,
) -> Result<Option<FleetState>, MusterError> {
    let Ok(manifest) = Manifest::load(dir) else {
        return Ok(None);
    };
    let Some(state) = FleetState::load(dir)? else {
        return Ok(None);
    };
    if fleet_state::is_current(dir, &manifest, &state)? {
        return Ok(None);
    }
    let pin = TrustAnchor::load(&ctx.state_dir)?.ok_or_else(|| {
        MusterError::Security("no local trust pin; run `muster sync` to pin the fleet first".into())
    })?;
    let vault_only = fleet_state::governance_digest(&manifest)? == pin.governance;
    let allowed = if vault_only {
        &pin.writers
    } else {
        &pin.admins
    };
    let local = Signer::load(&ctx.state_dir)?;
    let signer = match (&local, &ctx.recovery_signer) {
        (Some(s), _) if allowed.contains(&s.public()) => s,
        (_, Some(r)) if allowed.contains(&r.public()) => r,
        _ => {
            return Err(MusterError::Security(format!(
                "this machine cannot sign the fleet state: its signing key is not {} under the \
                 last accepted state. {}",
                if vault_only {
                    "an authorized writer"
                } else {
                    "an admin key"
                },
                if local.is_some() {
                    "Ask an admin to run the change, or pass --recovery-key-stdin."
                } else {
                    "Run `muster enroll` and have an admin approve this machine."
                }
            )))
        }
    };
    if vault_only && !pin.admins.contains(&signer.public()) {
        // A non-admin writer may only re-sign groups it can read.
        let changed = fleet_state::changed_groups(
            &state.payload.vault,
            &state.payload.vault_meta,
            &fleet_state::vault_hashes(dir)?,
            &fleet_state::vault_meta_hashes(dir)?,
        );
        fleet_state::ensure_reads_groups(&manifest, &signer.public(), &changed)?;
    }
    let enrollments = std::mem::take(&mut ctx.enrollments);
    if persist_pin {
        fleet_state::seal_with(dir, &ctx.state_dir, &manifest, signer, false, enrollments)
    } else {
        fleet_state::seal_unpinned(dir, &ctx.state_dir, &manifest, signer, enrollments)
    }
}

/// Runs [`verify`] after a pull. On refusal the pull is undone, so the repo
/// keeps its last good state and nothing is applied.
///
/// `joined` means the remote history is unrelated to the local one (the local
/// repo was replaced). That is accepted only with `rejoin` (the user passed
/// `--remote` on purpose); the old pin then belongs to the discarded fleet and
/// is dropped for a fresh first-use pin. Otherwise an unrelated history is
/// refused, so a rewritten remote cannot reset a pinned machine.
pub fn verify_pulled(
    ctx: &mut Context,
    previous: Option<&str>,
    joined: bool,
    rejoin: bool,
) -> Result<(), MusterError> {
    if joined && previous.is_some() {
        if !rejoin {
            let _ = git::restore_after_pull(&ctx.config_dir, previous);
            return Err(MusterError::Security(
                "the remote history is unrelated to this machine's pinned fleet (rewritten \
                 remote?); the pulled changes were discarded. To join another fleet on purpose \
                 run `muster sync --remote <url> --pull`"
                    .into(),
            ));
        }
        if let Some(old) = TrustAnchor::load(&ctx.state_dir)? {
            let _ = std::fs::remove_file(TrustAnchor::path(&ctx.state_dir));
            ctx.diag(Diagnostic::new(
                Severity::Warning,
                "fleet_rejoined",
                format!(
                    "joined a different fleet; dropped the pin of the previous one (epoch {})",
                    old.epoch
                ),
            ));
        }
    }
    let manifest = Manifest::load(&ctx.config_dir);
    let result = manifest.and_then(|m| verify(ctx, &m, true));
    if let Err(e) = result {
        let restored = git::restore_after_pull(&ctx.config_dir, previous);
        let note = match restored {
            Ok(()) => "the pulled changes were discarded and nothing was applied".to_string(),
            Err(re) => format!("could not discard the pulled changes: {re}"),
        };
        return Err(match e {
            MusterError::Security(m) => MusterError::Security(format!("{m}; {note}")),
            other => other,
        });
    }
    Ok(())
}

/// Whether `state_dir` already holds a pin (used by `doctor`).
pub fn has_pin(state_dir: &Path) -> bool {
    matches!(TrustAnchor::load(state_dir), Ok(Some(_)))
}
