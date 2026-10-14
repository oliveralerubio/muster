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
    crate::commands::init::SPEC,
    crate::commands::schema::SPEC,
    crate::commands::status::SPEC,
];

pub fn find(name: &str) -> Option<&'static CommandSpec> {
    COMMANDS.iter().find(|c| c.name == name)
}

/// Machine-readable command list (`muster schema`, MCP, web).
pub fn commands_json() -> Value {
    serde_json::to_value(COMMANDS).expect("commands serialize")
}
