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
            name: "expect-state",
            about: "Payload SHA-256 (from `muster trust show` on a trusted machine) of the one fleet state this machine may take as its origin when it pins a fleet for the first time",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "expect-signer",
            about: "Full public key (musterpk1...) or full 256-bit fingerprint (SHA256:<64 hex>) the fleet signer must have when this machine pins the fleet for the first time",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
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
    let _lock = if ctx.dry_run {
        None
    } else {
        std::fs::create_dir_all(&ctx.state_dir).map_err(sync_io)?;
        let mut options = std::fs::OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let file = options
            .open(ctx.state_dir.join("sync.lock"))
            .map_err(sync_io)?;
        match file.try_lock() {
            Ok(()) => Some(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                ctx.diag(Diagnostic::new(
                    Severity::Info,
                    "sync_skipped_locked",
                    "another sync is running",
                ));
                return Ok(json!({"skipped": "locked", "applied": false}));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(sync_io(e)),
        }
    };
    let result = sync(ctx, params);
    if !ctx.dry_run {
        let outcome = match &result {
            Ok(_) if !ctx.failed() => "ok",
            Err(e) if e.to_string().contains("conflict with") => "conflict",
            _ => "error",
        };
        let pending_approvals = result
            .as_ref()
            .ok()
            .map(|r| r["approvals"].clone())
            .filter(|value| value.is_array())
            .unwrap_or_else(|| json!([]));
        let approval_needed = !ctx.yes
            && !ctx.is_tty
            && pending_approvals
                .as_array()
                .is_some_and(|items| !items.is_empty());
        let status = json!({
            "ts": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
            "result": outcome,
            "approval_needed": approval_needed,
            "pending_approvals": if approval_needed { pending_approvals } else { json!([]) },
            "applied": result.as_ref().ok().and_then(|r| r["applied"].as_bool()).unwrap_or(false),
        });
        let pending = ctx.state_dir.join("sync-status.json.partial");
        std::fs::write(&pending, status.to_string()).map_err(sync_io)?;
        std::fs::rename(pending, ctx.state_dir.join("sync-status.json")).map_err(sync_io)?;
    }
    result
}

fn sync_io(e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("sync state: {e}"))
}

/// The lock file remains in place: unlinking it would allow a second inode
/// to be locked while a waiting process still owns the first one.
fn sync(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    // A machine with no config yet can join a fleet with `--remote <url> --pull`:
    // it only fetches the repo. Nothing is applied until it is enrolled, since
    // it cannot read the fleet's secrets before that.
    let joining = !ctx.dry_run
        && params.get_string("remote").is_some()
        && params.get_bool("pull")
        && !ctx.config_dir.join(crate::manifest::FILE_NAME).exists();
    if joining {
        git::init_repo(&ctx.config_dir)?;
        let (joined, _) = transport(ctx, params, false)?;
        if !ctx.config_dir.join(crate::manifest::FILE_NAME).exists() {
            return Err(MusterError::Operational(
                "the remote has no Muster config yet; run `muster init` on the first machine and `muster sync --push`".into(),
            ));
        }
        return Ok(json!({"joined": true, "applied": false, "transport": joined}));
    }
    Manifest::load(&ctx.config_dir)?;
    let target = target(params)?;
    let (transport, skills_updated) = if ctx.dry_run {
        (Value::Null, Vec::new())
    } else {
        transport(ctx, params, true)?
    };
    // A governance change is waiting for admin co-signatures: the pull above
    // may bring them, but nothing is applied from the unaccepted state.
    if ctx.governance_pending {
        return Ok(json!({
            "applied": false,
            "governance_pending": true,
            "transport": transport,
            "skills_updated": skills_updated,
        }));
    }
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
    result["skills_updated"] = Value::Array(skills_updated);
    Ok(result)
}

/// Commit local config changes and exchange them with the remote. Skipped
/// when the repo has no remote and no transport flag was given. With
/// `auto_update`, subscribed skills are updated unattended (when the manifest
/// opts in) after pulling and before pushing, so the same run publishes and
/// applies them. Returns the transport report and the updated skill names.
fn transport(
    ctx: &mut Context,
    params: &Params,
    auto_update: bool,
) -> Result<(Value, Vec<Value>), MusterError> {
    let (only_pull, only_push) = (params.get_bool("pull"), params.get_bool("push"));
    let remote = params.get_string("remote");
    if let Some(url) = remote {
        git::set_remote(&ctx.config_dir, url)?;
    }
    if !(git::has_remote(&ctx.config_dir) || only_pull || only_push) {
        let updated = if auto_update {
            crate::commands::skill::auto_update_in_sync(ctx)
        } else {
            Vec::new()
        };
        return Ok((Value::Null, updated));
    }
    let (do_pull, do_push) = match (only_pull, only_push) {
        (false, false) => (true, true),
        flags => flags,
    };
    let committed = git::commit_if_configured(&ctx.config_dir, "chore(muster): sync")?;
    let mut pulled = Value::Null;
    if do_pull {
        ctx.progress("Pulling config repo…");
        let previous = git::head_commit_opt(&ctx.config_dir);
        let pull_result = git::pull(&ctx.config_dir)?;
        // Refuse (and undo) a pull that rolls the fleet back, forges its vault or
        // changes its governance without a valid signature from an authorized signer.
        let joined = matches!(pull_result, Pulled::Joined { .. });
        crate::trust::verify_pulled(
            ctx,
            previous.as_deref(),
            joined,
            params.get_string("remote").is_some(),
        )?;
        pulled = match pull_result {
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
    let skills_updated = if auto_update {
        crate::commands::skill::auto_update_in_sync(ctx)
    } else {
        Vec::new()
    };
    if do_push {
        ctx.progress("Pushing config repo…");
        git::push(&ctx.config_dir)?;
    }
    Ok((
        json!({
            "config_path": ctx.config_dir,
            "committed": committed,
            "pulled": pulled,
            "pushed": do_push,
        }),
        skills_updated,
    ))
}
