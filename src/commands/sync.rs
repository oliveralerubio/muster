//! `muster sync`: git transport of the config repo (when it has a remote),
//! then plan + apply through the engine. `--dry-run` = plan only.

use serde_json::{json, Value};

use crate::commands::plan::{target, PROJECT_PARAM};
use crate::context::Context;
use crate::engine::{self, Options};
use crate::errors::MusterError;
use crate::git::{self, Pulled};
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "sync",
    about: "Pull/push the config repo, then apply the desired state (backup, generation)",
    params: &[
        ParamSpec {
            name: "remote",
            about: "Set the git remote URL of the config repo (joins an existing fleet)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "pull",
            about: "Only pull from the remote (no push)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "push",
            about: "Only push to the remote (no pull)",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        PROJECT_PARAM,
    ],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    Manifest::load(&ctx.config_dir)?;
    let target = target(params)?;
    let transport = if ctx.dry_run {
        Value::Null
    } else {
        transport(ctx, params)?
    };
    let global = target == engine::Target::Global;
    let plan = engine::plan(
        ctx,
        &Options {
            target,
            agent: None,
        },
    )?;
    let mut result = engine::apply(ctx, plan)?;
    if global {
        crate::team::record_applied(ctx, &result)?;
    }
    result["transport"] = transport;
    Ok(result)
}

/// Commit local config changes and exchange them with the remote. Skipped
/// when the repo has no remote and no transport flag was given.
fn transport(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let (only_pull, only_push) = (params.get_bool("pull"), params.get_bool("push"));
    let remote = params.get_string("remote");
    if let Some(url) = remote {
        git::set_remote(&ctx.config_dir, url)?;
    }
    if !(git::has_remote(&ctx.config_dir) || only_pull || only_push) {
        return Ok(Value::Null);
    }
    let (do_pull, do_push) = match (only_pull, only_push) {
        (false, false) => (true, true),
        flags => flags,
    };
    let committed = git::commit_if_configured(&ctx.config_dir, "chore(muster): sync")?;
    let mut pulled = Value::Null;
    if do_pull {
        ctx.progress("Pulling config repo…");
        pulled = match git::pull(&ctx.config_dir)? {
            Pulled::Empty => json!("remote_empty"),
            Pulled::Updated => json!("updated"),
            Pulled::Joined { backup } => {
                ctx.diag(Diagnostic::new(
                    Severity::Warning,
                    "joined_fleet",
                    format!(
                        "local config history was unrelated to the remote and was replaced by it (backup: {backup}); run `muster enroll` to join the fleet"
                    ),
                ));
                json!("joined")
            }
        };
    }
    if do_push {
        ctx.progress("Pushing config repo…");
        git::push(&ctx.config_dir)?;
    }
    Ok(json!({
        "config_path": ctx.config_dir,
        "committed": committed,
        "pulled": pulled,
        "pushed": do_push,
    }))
}
