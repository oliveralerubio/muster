//! The central command registry.
//!
//! Shared by the CLI, MCP server, web interface, and `muster schema`.
//! Subcommands use dotted names (e.g. `secret.add`).

use std::collections::HashMap;

use serde::Serialize;
use serde_json::Value;

use crate::context::Context;
use crate::errors::MusterError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ParamKind {
    String,
    Bool,
    Int,
    List,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ParamSpec {
    pub name: &'static str,
    pub about: &'static str,
    pub kind: ParamKind,
    pub required: bool,
    /// Taken by position (`muster approve bob`) instead of `--name`.
    pub positional: bool,
}

/// One operation. `handler` never prints, exits or reads stdin: it returns the
/// envelope `result` and reports through `ctx` (`ctx.diag`, `ctx.confirm`).
#[derive(Serialize)]
pub struct CommandSpec {
    pub name: &'static str,
    pub about: &'static str,
    pub params: &'static [ParamSpec],
    pub mutating: bool,
    #[serde(skip)]
    pub handler: fn(&mut Context, &Params) -> Result<Value, MusterError>,
}

/// Typed accessor over parsed command parameters.
#[derive(Debug, Default, Clone)]
pub struct Params {
    values: HashMap<String, Value>,
}

impl Params {
    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        self.values.insert(name.into(), value);
    }

    pub fn get_string(&self, name: &str) -> Option<&str> {
        self.values.get(name).and_then(|v| v.as_str())
    }

    pub fn get_bool(&self, name: &str) -> bool {
        self.values
            .get(name)
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    }

    pub fn get_int(&self, name: &str) -> Option<i64> {
        self.values.get(name).and_then(|v| v.as_i64())
    }

    pub fn get_list(&self, name: &str) -> Vec<String> {
        self.values
            .get(name)
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| item.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Every command Muster knows. Register new commands here.
pub static COMMANDS: &[CommandSpec] = &[
    crate::commands::provider::LIST,
    crate::commands::provider::ADD,
    crate::commands::provider::RM,
    crate::commands::provider::LOGIN,
    crate::commands::provider::USE,
    crate::commands::agents::SPEC,
    crate::commands::agents::APPLY_SPEC,
    crate::commands::agents::INSTALL_SPEC,
    crate::commands::init::SPEC,
    crate::commands::schema::SPEC,
    crate::commands::status::SPEC,
    crate::commands::why::SPEC,
    crate::commands::sync::SPEC,
    crate::commands::plan::SPEC,
    crate::commands::effective::SPEC,
    crate::commands::plan::ROLLBACK_SPEC,
    crate::commands::enroll::SPEC,
    crate::commands::approve::SPEC,
    crate::commands::reject::SPEC,
    crate::commands::revoke::SPEC,
    crate::commands::machine::LIST,
    crate::commands::machine::ASSIGN,
    crate::commands::machine::RM,
    crate::commands::secret::ADD,
    crate::commands::secret::LIST,
    crate::commands::secret::ROTATE,
    crate::commands::secret::RM,
    crate::commands::secret::GET,
    crate::commands::secret::CHECK,
    crate::commands::secret::REQUESTS,
    crate::commands::secret::APPROVE,
    crate::commands::secret::DENY,
    crate::commands::secret::LEASES,
    crate::commands::secret::REVOKE,
    crate::commands::skill::LIST,
    crate::commands::skill::ADD,
    crate::commands::skill::CHECK,
    crate::commands::skill::PIN,
    crate::commands::skill::UPDATE,
    crate::commands::skill::RM,
    crate::commands::member::ADD,
    crate::commands::member::APPROVE,
    crate::commands::member::REJECT,
    crate::commands::member::LIST,
    crate::commands::member::ROLE_SPEC,
    crate::commands::member::PROFILE,
    crate::commands::member::RM,
    crate::commands::group::CREATE,
    crate::commands::group::LIST,
    crate::commands::group::GRANT,
    crate::commands::group::REVOKE,
    crate::commands::group::REKEY,
    crate::commands::profile::LIST,
    crate::commands::profile::SHOW,
    crate::commands::profile::SET,
    crate::commands::profile::RM,
    crate::commands::mcp_server::LIST,
    crate::commands::mcp_server::ADD,
    crate::commands::mcp_server::RM,
    crate::commands::schedule::ENABLE,
    crate::commands::schedule::DISABLE,
    crate::commands::schedule::STATUS,
    crate::commands::doctor::SPEC,
    crate::commands::import::BWS,
    crate::commands::import::CHEZMOI,
    crate::commands::import::CUARTEL,
    crate::commands::ui::SPEC,
    crate::commands::uninstall::SPEC,
    crate::commands::proposal::CREATE,
    crate::commands::proposal::ACCEPT,
    crate::commands::proposal::LIST,
    crate::commands::toggle::ENABLE,
    crate::commands::toggle::DISABLE,
    crate::mcp::SPEC,
    crate::commands::usage::SPEC,
];

pub fn find(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Machine-readable command list (`muster schema`, MCP, web).
pub fn commands_json() -> Value {
    serde_json::to_value(COMMANDS).expect("commands serialize")
}
