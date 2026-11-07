//! `muster history`: restorable generations and the local audit events.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "history",
    about: "List generations (see `muster rollback`), local events, and config repo commits",
    params: &[],
    mutating: false,
    handler: run,
};

fn run(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    Ok(json!({
        "generations": crate::engine::generations(&ctx.state_dir)?,
        "events": crate::audit::events(&ctx.state_dir)?,
        "commits": crate::git::recent_commits(&ctx.config_dir, 50),
    }))
}
