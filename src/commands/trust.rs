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

    let status = match fleet_state::verify(&ctx.config_dir, &ctx.state_dir, &manifest, None) {
        Ok(Verification::Verified { .. }) => "verified",
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

    // Governance diff against what this machine last accepted.
    let governance_diff = pin
        .as_ref()
        .and_then(|p| p.snapshot.as_ref())
        .map(|old| {
            let render = |v: &Value| serde_json::to_string_pretty(v).unwrap_or_default();
            crate::engine::diff::unified(
                &render(old),
                &render(&governance_content(&manifest)),
                "governance",
                false,
            )
        })
        .unwrap_or_default();
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
    if !ctx.yes {
        if governance_diff.is_empty() {
            eprintln!("Governance fields are unchanged; only vault hashes will be re-signed.");
        } else {
            eprintln!("Governance changes to be signed:\n{governance_diff}");
        }
    }
    ctx.confirm("Re-sign fleet state with the current muster.toml security fields?")?;

    let state = fleet_state::seal(&ctx.config_dir, &ctx.state_dir, &manifest, &signer, true)?
        .ok_or_else(|| MusterError::Operational("sealing failed".into()))?;

    git::commit_if_configured(&ctx.config_dir, "feat(security): re-sign fleet state")?;
    if git::has_remote(&ctx.config_dir) {
        let _ = git::push(&ctx.config_dir);
    }

    Ok(json!({
        "resigned": true,
        "epoch": state.payload.epoch,
        "signer": state.payload.signer,
        "fingerprint": signing::fingerprint(&state.payload.signer),
        "governance": state.payload.governance,
        "governance_content": governance_content(&manifest),
        "governance_diff": governance_diff,
    }))
}
