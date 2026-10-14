//! `muster status`: display fleet machines, their platform, version and sync status.

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "status",
    about: "Show fleet status and this machine's membership",
    params: &[],
    mutating: false,
    handler: run,
};

fn run(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;

    let local_identity = Identity::load(&ctx.state_dir).ok();
    let local_recipient = local_identity.as_ref().map(|id| id.recipient());
    let local_name = machine::load_name(&ctx.state_dir);

    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let is_current = match (&local_name, &local_recipient) {
            (Some(ln), _) if ln == name => true,
            (_, Some(lr)) if lr == &m.recipient => true,
            _ => false,
        };
        let status = if is_current {
            "current"
        } else if m.last_sync.is_some() {
            "active"
        } else {
            "enrolled"
        };
        machines.push(json!({
            "name": name,
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "last_sync": m.last_sync,
            "status": status,
            "is_current": is_current,
        }));
    }

    let team = crate::team::drift(&ctx.config_dir, &manifest);
    Ok(json!({
        "config_path": ctx.config_dir,
        "machines": machines,
        "pending": team["pending"],
        "drift": team["drift"],
    }))
}
