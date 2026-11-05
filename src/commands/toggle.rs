use serde_json::{json, Value};
use toml_edit::{DocumentMut, Item, Table};

use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

const PARAMS: &[ParamSpec] = &[
    ParamSpec {
        name: "item",
        about: "Item as kind:name[@adapter]",
        kind: ParamKind::String,
        required: true,
        positional: true,
    },
    ParamSpec {
        name: "project",
        about: "Project directory",
        kind: ParamKind::String,
        required: false,
        positional: false,
    },
    ParamSpec {
        name: "profile",
        about: "Profile name",
        kind: ParamKind::String,
        required: false,
        positional: false,
    },
];
pub const ENABLE: CommandSpec = CommandSpec {
    name: "enable",
    about: "Enable an item",
    params: PARAMS,
    mutating: true,
    handler: enable,
};
pub const DISABLE: CommandSpec = CommandSpec {
    name: "disable",
    about: "Disable an item",
    params: PARAMS,
    mutating: true,
    handler: disable,
};
fn enable(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    edit(ctx, params, "enable")
}
fn disable(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    edit(ctx, params, "disable")
}
fn table<'a>(parent: &'a mut Table, key: &str) -> Result<&'a mut Table, MusterError> {
    if !parent.contains_key(key) {
        let mut t = Table::new();
        t.set_implicit(true);
        parent.insert(key, Item::Table(t));
    }
    parent[key]
        .as_table_mut()
        .ok_or_else(|| MusterError::Usage(format!("{key} is not a table")))
}
fn edit(ctx: &mut Context, params: &Params, action: &str) -> Result<Value, MusterError> {
    let item = params
        .get_string("item")
        .ok_or_else(|| MusterError::Usage("item required".into()))?;
    crate::adapters::config::parse_toggle(item).map_err(MusterError::Usage)?;
    let project = params.get_string("project");
    let profile = params.get_string("profile");
    if project.is_some() && profile.is_some() {
        return Err(MusterError::Usage(
            "use either --project or --profile".into(),
        ));
    }
    let manifest = Manifest::load(&ctx.config_dir)?;
    let actor = if project.is_none() && !manifest.members.is_empty() {
        Some(crate::team::admin(&manifest, &ctx.state_dir)?)
    } else {
        None
    };
    if let Some(name) = profile {
        if !manifest.profiles.contains_key(name) {
            return Err(MusterError::Usage(format!(
                "profile '{name}' is not declared"
            )));
        }
    }
    let path = project
        .map(|p| std::path::Path::new(p).join(".muster/muster.toml"))
        .unwrap_or_else(|| ctx.config_dir.join("muster.toml"));
    let text = std::fs::read_to_string(&path)
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
    Manifest::parse(&text, &path.display().to_string())?;
    let mut doc: DocumentMut = text
        .parse()
        .map_err(|e| MusterError::Usage(format!("invalid TOML: {e}")))?;
    let root = doc.as_table_mut();
    let root = if let Some(name) = profile {
        table(table(root, "profiles")?, name)?
    } else {
        root
    };
    let toggles = table(table(root, "agents")?, "toggles")?;
    for key in ["disable", "enable"] {
        let mut values = toggles
            .get(key)
            .and_then(Item::as_array)
            .cloned()
            .unwrap_or_default();
        if key == action && values.iter().any(|value| value.as_str() == Some(item)) {
            continue;
        }
        let mut i = 0;
        while i < values.len() {
            if values.get(i).and_then(|v| v.as_str()) == Some(item) {
                values.remove(i);
            } else {
                i += 1;
            }
        }
        if key == action {
            values.push(item);
        }
        if !values.is_empty() || toggles.contains_key(key) {
            toggles.insert(key, Item::Value(toml_edit::Value::Array(values)));
        }
    }
    let output = doc.to_string();
    Manifest::parse(&output, &path.display().to_string())?;
    let result = json!({"item":item, "action":action, "path":path, "changed":output != text});
    if ctx.dry_run || output == text {
        return Ok(result);
    }
    ctx.confirm(&format!("{action} {item}?"))?;
    crate::manifest::write_atomic(&path, output.as_bytes())
        .map_err(|e| MusterError::Operational(format!("{}: {e}", path.display())))?;
    if project.is_none() {
        if let Some(actor) = actor {
            crate::team::commit(ctx, &actor, &format!("feat: {action} {item}"))?;
        } else {
            crate::trust::commit(ctx, &format!("feat: {action} {item}"))?;
        }
    }
    Ok(result)
}
