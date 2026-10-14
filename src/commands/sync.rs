//! `muster sync`: exchange the config repo with its git remote.
//!
//! Git transport only; the apply engine (plan, generations, rollback) builds on
//! top of this later.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::git::{self, Pulled};
use crate::manifest::Manifest;
use crate::output::{Diagnostic, Severity};
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "sync",
    about: "Pull and push the config repo with its git remote",
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
            about: "Only pull from the remote",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "push",
            about: "Only push to the remote",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    Manifest::load(&ctx.config_dir)?;
    let (only_pull, only_push) = (params.get_bool("pull"), params.get_bool("push"));
    let (do_pull, do_push) = match (only_pull, only_push) {
        (false, false) => (true, true),
        flags => flags,
    };
    let remote = params.get_string("remote");
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "remote": remote,
            "pull": do_pull,
            "push": do_push,
        }));
    }
    if let Some(url) = remote {
        git::set_remote(&ctx.config_dir, url)?;
    }
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
