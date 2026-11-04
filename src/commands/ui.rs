//! `muster ui`: the local web admin interface (see `crate::web`).

use serde_json::Value;

use crate::context::Context;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "ui",
    about: "Serve the web admin interface on 127.0.0.1 with a one-time login token (a second call reuses the running panel)",
    params: &[
        ParamSpec {
            name: "port",
            about: "Port to listen on (default 47821; 0 = random free port)",
            kind: ParamKind::Int,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "no-open",
            about: "Do not open a browser; only print the URL",
            kind: ParamKind::Bool,
            required: false,
            positional: false,
        },
    ],
    mutating: false,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let port = params
        .get_int("port")
        .map(|port| {
            u16::try_from(port)
                .map_err(|_| MusterError::Usage(format!("invalid port {port}: expected 0-65535")))
        })
        .transpose()?;
    crate::web::serve(ctx, port, !params.get_bool("no-open"))?;
    Ok(Value::Null)
}
