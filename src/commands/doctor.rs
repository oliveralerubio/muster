//! `muster doctor`: check machine health, agent configurations, permissions,
//! repository and vault state.

use serde_json::Value;

use crate::context::Context;
use crate::doctor;
use crate::errors::MusterError;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};

pub const SPEC: CommandSpec = CommandSpec {
    name: "doctor",
    about: "Diagnose system health, agent configurations, permissions, repository and vault state",
    params: &[ParamSpec {
        name: "fix",
        about:
            "Repair what can be repaired: chmod right away, anything destructive after confirmation",
        kind: ParamKind::Bool,
        required: false,
        positional: false,
    }, ParamSpec {
        name: "check-keys",
        about: "Opt in to checking OPENROUTER_API_KEY validity, expiry and spending limit with OpenRouter",
        kind: ParamKind::Bool,
        required: false,
        positional: false,
    }, ParamSpec {
        name: "offline",
        about: "Skip optional API key network checks",
        kind: ParamKind::Bool,
        required: false,
        positional: false,
    }],
    mutating: true,
    handler: run,
};

fn run(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let fix = params.get_bool("fix");
    doctor::run(
        ctx,
        fix,
        params.get_bool("check-keys"),
        params.get_bool("offline"),
    )
}
