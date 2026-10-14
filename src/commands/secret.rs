//! `muster secret`: secret metadata (values never leave the vault here).

use serde_json::{json, Value};

use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, Params};
use crate::vault::Vault;

pub const LIST: CommandSpec = CommandSpec {
    name: "secret.list",
    about: "List secret names, groups and readers (never values)",
    params: &[],
    mutating: false,
    handler: list,
};

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let vault = Vault::open(&ctx.config_dir)?;
    let mut secrets = Vec::new();
    for group in vault.groups()? {
        for meta in vault.list(&group)? {
            secrets.push(json!({
                "group": group,
                "name": meta.name,
                "reference": format!("native://{group}/{}", meta.name),
                "created_at": meta.created_at,
                "updated_at": meta.updated_at,
                "readers": meta.recipients,
            }));
        }
    }
    Ok(Value::Array(secrets))
}
