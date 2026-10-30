use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use serde_json::Value;

pub const SPEC: CommandSpec = CommandSpec {
    name: "usage",
    about: "Report local agent usage",
    params: &[
        ParamSpec {
            name: "since",
            about: "Lookback in days (e.g. 14d)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "agent",
            about: "Filter by agent",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "max-file-bytes",
            about: "Maximum transcript size",
            kind: ParamKind::Int,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "project",
            about: "Filter by project directory",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
    ],
    mutating: false,
    handler: usage,
};

fn usage(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let days = params
        .get_string("since")
        .unwrap_or("14d")
        .strip_suffix('d')
        .and_then(|s| s.parse::<i64>().ok())
        .filter(|d| (1..=36500).contains(d))
        .ok_or_else(|| {
            MusterError::Usage("--since must be a positive number of days (e.g. 14d)".into())
        })?;
    let agent = params.get_string("agent");
    if agent.is_some_and(|a| !["claude", "codex", "opencode", "antigravity"].contains(&a)) {
        return Err(MusterError::Usage("unknown agent".into()));
    }
    let max = params.get_int("max-file-bytes").unwrap_or(50_000_000);
    if max <= 0 {
        return Err(MusterError::Usage(
            "--max-file-bytes must be positive".into(),
        ));
    }
    Ok(crate::usage::report(
        days,
        &ctx.config_dir,
        agent,
        max as u64,
        params.get_string("project"),
    ))
}
