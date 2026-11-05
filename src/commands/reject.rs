//! `muster reject`: discard a pending enrollment request, with any approvals
//! recorded on it, without enrolling or revoking the machine.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::git;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team;

pub const SPEC: CommandSpec = CommandSpec {
    name: "reject",
    about: "Reject a machine's pending enrollment request and discard its recorded approvals",
    params: &[ParamSpec {
        name: "name",
        about: "Name of the machine whose request to reject",
        kind: ParamKind::String,
        required: true,
        positional: true,
    }],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    ctx.confirm(&format!(
        "Reject the enrollment request of machine '{name}'?"
    ))?;

    let manifest = Manifest::load(&ctx.config_dir)?;
    if manifest.machines.contains_key(name) {
        return Err(MusterError::Usage(format!(
            "machine '{name}' is already a member of the fleet"
        )));
    }
    let request = EnrollmentRequest::load(&ctx.config_dir, name)?;

    // Authorization mirrors `approve`: admins only in team mode, fleet
    // members only in single-person mode.
    let actor = if manifest.members.is_empty() {
        let caller = Identity::load(&ctx.state_dir)?;
        if !manifest
            .machines
            .values()
            .any(|m| m.recipient == caller.recipient())
        {
            return Err(MusterError::Security(
                "this machine is not an approved fleet member; reject from an approved machine"
                    .into(),
            ));
        }
        None
    } else {
        Some(team::admin(&manifest, &ctx.state_dir)?)
    };

    let result = |dry_run: bool| {
        let mut value = json!({
            "rejected": name,
            "kind": "machine",
            "approvals_discarded": request.approval_members(),
        });
        if dry_run {
            value["dry_run"] = Value::Bool(true);
        }
        value
    };
    if ctx.dry_run {
        return Ok(result(true));
    }

    // Rejecting only drops the request together with any approvals recorded
    // on it: nothing is revoked and the vault is not re-encrypted.
    EnrollmentRequest::remove(&ctx.config_dir, name)?;

    let summary = format!("feat(fleet): reject machine {name}");
    match &actor {
        Some(actor) => team::commit(ctx, actor, &summary)?,
        None => {
            crate::trust::commit(ctx, &summary)?;
            if git::has_remote(&ctx.config_dir) {
                if let Err(e) = git::push(&ctx.config_dir) {
                    ctx.diag(Diagnostic::new(
                        Severity::Warning,
                        "push_failed",
                        format!(
                            "rejection committed but not pushed: {e}; run `muster sync --push`"
                        ),
                    ));
                }
            }
        }
    }
    Ok(result(false))
}
