//! `muster mcp`: Model Context Protocol server over stdio.
//!
//! Hand-rolled JSON-RPC 2.0, one message per line. Every tool is a thin
//! adapter over a registry command: its `inputSchema` is derived from the
//! command's `ParamSpec`s and its result is the exact JSON envelope the CLI
//! prints with `--format json`. Only read-only commands are exposed, plus
//! `propose_change` (records a proposal and returns its plan, never applies)
//! and `secret_reveal` (refused unless the reference is listed in the
//! manifest's `[mcp_server] reveal` AND its `[secrets."<ref>"] allow` grants
//! the calling agent, identified from `initialize` `clientInfo.name`).
//! stdout carries protocol only.

use std::cell::RefCell;
use std::io::{BufRead, Write};

use serde_json::{json, Map, Value};

use crate::cli;
use crate::context::{Context, Globals};
use crate::errors::MusterError;
use crate::manifest::Manifest;
use crate::output::{self, Envelope};
use crate::registry::{self, CommandSpec, ParamKind, ParamSpec, Params};
use crate::secrets::{SecretAccess, SecretRef};

/// Newest protocol revision first; a client asking for another one gets this.
const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

pub const SPEC: CommandSpec = CommandSpec {
    name: "mcp",
    about: "Run the MCP server over stdio (tools: status, doctor, plan, effective, why, usage, provider_list, secret_list_meta, propose_change, secret_reveal)",
    params: &[ParamSpec {
        name: "agent",
        about: "Pin client agent identity (e.g. claude, codex)",
        kind: ParamKind::String,
        required: false,
        positional: false,
    }],
    mutating: false,
    handler: not_a_handler,
};

/// `cli::main` runs [`serve`] instead; other adapters must not start a server.
fn not_a_handler(_: &mut Context, _: &Params) -> Result<Value, MusterError> {
    Err(MusterError::Usage(
        "`muster mcp` only runs as a stdio server from the command line".into(),
    ))
}

/// One MCP tool = one registry command with some parameters hidden or pinned.
struct Tool {
    name: &'static str,
    command: &'static str,
    /// Parameters never offered to agents (e.g. `doctor --fix` writes).
    hidden: &'static [&'static str],
    /// Boolean parameters always set for this tool.
    forced: &'static [&'static str],
}

const TOOLS: &[Tool] = &[
    Tool {
        name: "provider_list",
        command: "provider.list",
        hidden: &[],
        forced: &[],
    },
    Tool {
        name: "status",
        command: "status",
        hidden: &[],
        forced: &[],
    },
    Tool {
        name: "doctor",
        command: "doctor",
        hidden: &["fix"],
        forced: &[],
    },
    Tool {
        name: "plan",
        command: "plan",
        hidden: &[],
        forced: &[],
    },
    Tool {
        name: "effective",
        command: "effective",
        hidden: &["project"],
        forced: &[],
    },
    Tool {
        name: "why",
        command: "why",
        hidden: &["project"],
        forced: &[],
    },
    Tool {
        name: "usage",
        command: "usage",
        hidden: &["project", "max-file-bytes"],
        forced: &[],
    },
    Tool {
        name: "secret_list_meta",
        command: "secret.list",
        hidden: &[],
        forced: &[],
    },
    Tool {
        name: "propose_change",
        command: "proposal.create",
        hidden: &[],
        forced: &[],
    },
    Tool {
        name: "secret_reveal",
        command: "secret.get",
        hidden: &["reveal", "agent"],
        forced: &["reveal"],
    },
];

impl Tool {
    fn spec(&self) -> &'static CommandSpec {
        registry::find(self.command).expect("MCP tools map to registered commands")
    }

    fn params(&self) -> impl Iterator<Item = &'static ParamSpec> + '_ {
        self.spec()
            .params
            .iter()
            .filter(|p| !self.hidden.contains(&p.name))
    }

    fn describe(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();
        for p in self.params() {
            let mut schema = json!({ "type": json_type(p.kind), "description": p.about });
            if p.kind == ParamKind::List {
                schema["items"] = json!({ "type": "string" });
            }
            properties.insert(p.name.into(), schema);
            if p.required {
                required.push(p.name);
            }
        }
        let about = self.spec().about;
        let description = if self.name == "secret_reveal" {
            format!(
                "{about}. Refused unless the reference is listed in muster.toml [mcp_server] reveal \
                 and its [secrets.\"<reference>\"] allow grants the calling agent"
            )
        } else if self.name == "propose_change" {
            format!("{about}. To enable or disable an item, propose muster.toml with the item moved to the corresponding [agents.toggles] enable or disable list; a human accepts the proposal")
        } else {
            about.to_string()
        };
        json!({
            "name": self.name,
            "description": description,
            "inputSchema": {
                "type": "object",
                "properties": properties,
                "required": required,
                "additionalProperties": false,
            },
        })
    }

    /// Validate `arguments` against the visible parameters.
    fn arguments(&self, arguments: &Value) -> Result<Params, String> {
        let empty = Map::new();
        let args = match arguments {
            Value::Null => &empty,
            Value::Object(map) => map,
            _ => return Err("arguments must be an object".into()),
        };
        if let Some(unknown) = args.keys().find(|k| !self.params().any(|p| p.name == *k)) {
            return Err(format!(
                "unknown argument `{unknown}` for tool `{}`",
                self.name
            ));
        }
        let mut params = Params::default();
        for p in self.params() {
            match args.get(p.name) {
                None if p.required => {
                    return Err(format!("missing required argument `{}`", p.name))
                }
                None => {}
                Some(value) if type_matches(p.kind, value) => params.set(p.name, value.clone()),
                Some(_) => {
                    return Err(format!(
                        "argument `{}` must be of type {}",
                        p.name,
                        json_type(p.kind)
                    ))
                }
            }
        }
        for name in self.forced {
            params.set(*name, Value::Bool(true));
        }
        Ok(params)
    }
}

fn json_type(kind: ParamKind) -> &'static str {
    match kind {
        ParamKind::String => "string",
        ParamKind::Bool => "boolean",
        ParamKind::Int => "integer",
        ParamKind::List => "array",
    }
}

fn type_matches(kind: ParamKind, value: &Value) -> bool {
    match kind {
        ParamKind::String => value.is_string(),
        ParamKind::Bool => value.is_boolean(),
        ParamKind::Int => value.is_i64(),
        ParamKind::List => value
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string)),
    }
}

/// Serve until stdin reaches EOF (exit 0). SIGINT is handled in `main` (130).
pub fn serve(globals: Globals, pinned: Option<String>) -> i32 {
    let pinned_id = if let Some(ref a) = pinned {
        match client_agent(a).or_else(|| crate::adapters::find(a).map(|ad| ad.id)) {
            Some(id) => Some(id),
            None => {
                let err = MusterError::Usage(format!("unknown agent `{a}` for --agent"));
                eprintln!("muster mcp: error[{}]: {err}", err.code());
                return err.exit_code();
            }
        }
    } else {
        None
    };
    let server = Server {
        globals,
        pinned_agent: pinned_id,
        first_client_name: RefCell::new(None),
        agent: RefCell::new(pinned_id),
    };
    let mut stdout = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(e) => {
                eprintln!("muster mcp: cannot read stdin: {e}");
                return 1;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle(&line) {
            if writeln!(stdout, "{response}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                // The client went away: nothing left to serve.
                return 0;
            }
        }
    }
    0
}

struct Server {
    globals: Globals,
    pinned_agent: Option<&'static str>,
    first_client_name: RefCell<Option<String>>,
    /// Adapter id of the connected client (from `initialize` `clientInfo`);
    /// `None` = unknown client, which no secret allow list grants.
    agent: RefCell<Option<&'static str>>,
}

/// Map an MCP client name to the adapter id its secrets are granted to.
pub fn client_agent(name: &str) -> Option<&'static str> {
    match name.to_ascii_lowercase().as_str() {
        "claude" | "claude-code" => Some("claude"),
        "codex" => Some("codex"),
        "opencode" => Some("opencode"),
        "antigravity" | "agy" => Some("antigravity"),
        _ => None,
    }
}

impl Server {
    /// The response line for one incoming message (`None` for notifications).
    fn handle(&self, line: &str) -> Option<Value> {
        let message: Value = match serde_json::from_str(line) {
            Ok(message) => message,
            Err(e) => {
                return Some(error(
                    Value::Null,
                    PARSE_ERROR,
                    &format!("parse error: {e}"),
                ))
            }
        };
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Some(error(
                id.unwrap_or(Value::Null),
                INVALID_REQUEST,
                "invalid request: expected a JSON-RPC 2.0 object with a method",
            ));
        };
        // Notifications (no id) never get a response, known or not.
        let id = id?;
        let params = message.get("params").unwrap_or(&Value::Null);
        let outcome = match method {
            "initialize" => {
                let client = params
                    .pointer("/clientInfo/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut first = self.first_client_name.borrow_mut();
                if let Some(existing) = first.as_deref() {
                    if existing != client {
                        Err((
                            INVALID_REQUEST,
                            format!("cannot re-initialize with a different client identity ('{client}' != '{existing}')"),
                        ))
                    } else {
                        Ok(initialize(params))
                    }
                } else {
                    *first = Some(client.to_owned());
                    if self.pinned_agent.is_none() {
                        *self.agent.borrow_mut() = client_agent(client);
                    }
                    Ok(initialize(params))
                }
            }
            "ping" => Ok(json!({})),
            "tools/list" => {
                Ok(json!({ "tools": TOOLS.iter().map(Tool::describe).collect::<Vec<_>>() }))
            }
            "tools/call" => self.call(params),
            _ => Err((METHOD_NOT_FOUND, format!("method not found: {method}"))),
        };
        Some(match outcome {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn call(&self, params: &Value) -> Result<Value, (i64, String)> {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let tool = TOOLS
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| (INVALID_PARAMS, format!("unknown tool: `{name}`")))?;
        let arguments = tool
            .arguments(params.get("arguments").unwrap_or(&Value::Null))
            .map_err(|message| (INVALID_PARAMS, message))?;
        let envelope = self.run(tool, &arguments);
        Ok(json!({
            "content": [{ "type": "text", "text": output::json_text(&envelope) }],
            "isError": !envelope.ok,
        }))
    }

    fn run(&self, tool: &Tool, arguments: &Params) -> Envelope {
        let spec = tool.spec();
        let mut globals = Globals {
            format: Some(output::Format::Json),
            yes: false,
            dry_run: false,
            ..self.globals.clone()
        };
        if tool.name == "secret_reveal" {
            if let Err(err) = self.reveal_allowed(arguments) {
                return Envelope::new(spec.name, false, Value::Null, vec![err.to_diagnostic()]);
            }
            // The manifest allow list is the human's prior, explicit approval.
            globals.yes = true;
            globals.audit_actor = self.agent.borrow().map(str::to_owned);
        }
        cli::execute(spec, globals, arguments, false).0
    }

    /// `Ok` only when the reference is listed in `[mcp_server] reveal` and
    /// its `[secrets."<ref>"] allow` grants the calling agent.
    fn reveal_allowed(&self, arguments: &Params) -> Result<(), MusterError> {
        let reference = SecretRef::parse(arguments.get_string("reference").unwrap_or(""))?;
        let ctx = Context::new(self.globals.clone())?;
        let manifest = Manifest::load(&ctx.config_dir)?;
        let listed = manifest.mcp_server.as_ref().is_some_and(|server| {
            server
                .reveal
                .iter()
                .any(|r| SecretRef::parse(r).is_ok_and(|r| r.as_str() == reference.as_str()))
        });
        if !listed {
            return Err(MusterError::Security(format!(
                "{reference} may not be revealed over MCP; list it in muster.toml [mcp_server] reveal to allow it"
            )));
        }
        let agent = *self.agent.borrow();
        let granted = agent.is_some_and(|agent| {
            SecretAccess::new(manifest.secrets.clone(), None).allows(agent, reference.as_str())
        });
        if granted {
            Ok(())
        } else {
            Err(MusterError::Security(format!(
                "{reference} may not be revealed to {} over MCP; add the agent to muster.toml \
                 [secrets.\"{reference}\"] allow",
                agent.map_or("an unidentified client".to_string(), |a| format!(
                    "agent `{a}`"
                ))
            )))
        }
    }
}

fn initialize(params: &Value) -> Value {
    let requested = params.get("protocolVersion").and_then(Value::as_str);
    let version = requested
        .filter(|v| SUPPORTED_VERSIONS.contains(v))
        .unwrap_or(SUPPORTED_VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "muster", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Read-only tools return the same JSON envelope as `muster --format json <command>`. \
    Changes go through propose_change: it records a proposal and returns its plan; a human accepts it \
    (`muster proposal accept <id>`) and applies it with `muster sync`.",
    })
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod client_identity_security_tests {
    use super::client_agent;

    #[test]
    fn client_names_must_match_exactly() {
        assert_eq!(client_agent("claude-code"), Some("claude"));
        assert_eq!(client_agent("not-claude-code"), None);
        assert_eq!(client_agent("codex-impersonator"), None);
    }
}
