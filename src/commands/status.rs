//! `muster status`: display fleet machines, their platform, version and sync status.

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::layer;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
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
    // The joiner's own question — "is my machine's enrollment still
    // pending?" — answered from local evidence before the fleet-wide view.
    let enrollment = enrollment_status(
        ctx,
        &manifest,
        local_name.as_deref(),
        local_recipient.as_deref(),
    )?;

    let local_sync = machine::local_last_sync(&ctx.state_dir);
    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let is_current = match (&local_name, &local_recipient) {
            (Some(ln), _) if ln == name => true,
            (_, Some(lr)) if lr == &m.recipient => true,
            _ => false,
        };
        let last_sync =
            machine::effective_last_sync(m.last_sync.as_deref(), local_sync.as_deref(), is_current);
        let status = if is_current {
            "current"
        } else if m.applied_profile.is_none() {
            "enrolled"
        } else {
            let target_digest = layer::digest(&ctx.config_dir, &manifest, name);
            if m.applied_profile.as_deref() == Some(target_digest.as_str()) {
                "active"
            } else {
                "drifted"
            }
        };
        machines.push(json!({
            "name": name,
            "host": m.host,
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "experimental_agents": m.agents.iter().filter(|id| {
                crate::adapters::find(id).is_some_and(|a| a.experimental)
            }).collect::<Vec<_>>(),
            "last_sync": last_sync,
            "status": status,
            "is_current": is_current,
        }));
    }

    let team = crate::team::drift(&ctx.config_dir, &manifest);
    let sync = match std::fs::read(ctx.state_dir.join("sync-status.json")) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .map_err(|e| MusterError::Operational(format!("invalid sync status: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(e) => {
            return Err(MusterError::Operational(format!(
                "cannot read sync status: {e}"
            )))
        }
    };
    let convergence = convergence(ctx)?;
    // Local, per-use approval requests an agent created on this machine: the
    // human approving them is here, so status is where they surface.
    let pending_secret_approvals: Vec<Value> = crate::secret_leases::Store::open(&ctx.state_dir)?
        .data
        .requests
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "reference": r.reference,
                "agent": r.agent,
                "requested_at": r.requested_at,
            })
        })
        .collect();
    let result = json!({
        "convergence": convergence,
        "sync": sync,
        "config_path": ctx.config_dir,
        "machines": machines,
        "enrollment": enrollment,
        "pending": team["pending"],
        "drift": team["drift"],
        "pending_secret_approvals": pending_secret_approvals,
    });
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format_text(&result)));
    }
    Ok(result)
}

/// The joining machine's own enrollment situation, in `status --format text`:
/// whether its request is still pending, whether it was published at all, and
/// the next action — never a hint of access before approval.
fn format_enrollment_text(enrollment: &serde_json::Map<String, Value>) -> String {
    let machine = enrollment.get("machine").and_then(Value::as_str);
    let mut text = String::new();
    match enrollment.get("state").and_then(Value::as_str) {
        Some("pending") => {
            let name = machine.unwrap_or("?");
            text.push_str(&format!(
                "Enrollment: this machine ('{name}') is not a fleet member yet — it has no \
                 access to fleet secrets until an admin approves the request.\n"
            ));
            text.push_str(&format!(
                "  Request '{name}' created {}; {} of {} required approvals recorded in this \
                 machine's copy of the request.\n",
                enrollment
                    .get("created_at")
                    .and_then(Value::as_str)
                    .unwrap_or("?"),
                enrollment
                    .get("approvals_received")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
                enrollment
                    .get("approvals_required")
                    .and_then(Value::as_i64)
                    .unwrap_or(0),
            ));
            let note = enrollment
                .get("publication_note")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match enrollment.get("published") {
                Some(Value::Bool(true)) => {
                    text.push_str("  Published to the fleet remote: yes (as of the last sync).\n")
                }
                Some(Value::Bool(false)) => {
                    text.push_str(&format!("  Published to the fleet remote: no — {note}.\n"))
                }
                _ => text.push_str(&format!(
                    "  Published to the fleet remote: cannot be determined — {note}.\n"
                )),
            }
        }
        Some("none") => {
            text.push_str(
                "Enrollment: this machine is not a fleet member and has no enrollment request \
                 in the config repo (it was never created, or it was rejected).\n",
            );
        }
        _ => {
            text.push_str("Enrollment: cannot be determined from this machine alone.\n");
        }
    }
    if let Some(notes) = enrollment.get("notes").and_then(Value::as_array) {
        for note in notes.iter().filter_map(Value::as_str) {
            text.push_str(&format!("  {note}\n"));
        }
    }
    if let Some(next) = enrollment.get("next").and_then(Value::as_array) {
        for step in next.iter().filter_map(Value::as_str) {
            text.push_str(&format!("  Next: {step}\n"));
        }
    }
    text
}

/// What `muster status` can honestly tell about this machine's own
/// enrollment into the fleet, from local evidence only (no network):
///
/// - `pending`: this machine (by identity) has an enrollment request in the
///   config repo and is not a fleet member yet.
/// - `none`: not a member and no request from this machine exists.
/// - `unknown`: an enrollment situation is evident (a matching request, or a
///   member-looking name) but cannot be tied to this machine; the evidence
///   is reported instead of a guess.
///
/// `Null` for fleet members and machines with nothing enrollment-shaped.
fn enrollment_status(
    ctx: &Context,
    manifest: &Manifest,
    local_name: Option<&str>,
    local_recipient: Option<&str>,
) -> Result<Value, MusterError> {
    if local_recipient.is_none() && local_name.is_none() {
        return Ok(Value::Null);
    }
    // Membership is proven by identity: the local recipient appears among the
    // fleet's machines. A name match alone proves nothing — a rebuilt machine
    // reusing a name has a different key.
    if local_recipient.is_some_and(|r| manifest.machines.values().any(|m| m.recipient == r)) {
        return Ok(Value::Null);
    }
    let requests = EnrollmentRequest::pending(&ctx.config_dir)?;
    // Without a local identity neither membership nor request ownership can
    // be proven; report the evidence instead of guessing.
    let Some(recipient) = local_recipient else {
        if let Some(name) = local_name.filter(|n| manifest.machines.contains_key(*n)) {
            return Ok(json!({
                "state": "unknown",
                "machine": name,
                "notes": [format!(
                    "a fleet machine named '{name}' exists, but this machine has no local \
                     identity to confirm it is this one; if it is, its state directory is gone"
                )],
            }));
        }
        if let Some(req) = local_name.and_then(|n| requests.iter().find(|req| req.name == n)) {
            return Ok(json!({
                "state": "unknown",
                "machine": req.name,
                "notes": [format!(
                    "an enrollment request named '{}' exists, but this machine has no local \
                     identity to prove it is its own; evidence: {}",
                    req.name,
                    EnrollmentRequest::file_path(&ctx.config_dir, &req.name).display()
                )],
            }));
        }
        return Ok(Value::Null);
    };

    let revoked = manifest.revoked_recipients.iter().any(|r| r == recipient);
    let mut mine: Vec<_> = requests
        .iter()
        .filter(|req| req.recipient == recipient)
        .collect();
    if mine.is_empty() {
        // Not a member and no request of its own: it never enrolled, or its
        // request was rejected (reject removes the file).
        let mut notes = Vec::new();
        if revoked {
            notes.push(
                "this machine's identity was revoked by the fleet; `muster enroll` will be \
                 refused while this key is revoked"
                    .to_owned(),
            );
        }
        return Ok(json!({
            "state": "none",
            "member": false,
            "notes": notes,
            "next": ["run `muster enroll` to request joining this machine into the fleet"],
        }));
    }
    // Several requests can carry one identity (enrolling again under a new
    // name leaves the earlier one behind); the newest is the live one.
    mine.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    let req = mine[0];
    let mut notes: Vec<String> = mine[1..]
        .iter()
        .map(|other| {
            format!(
                "an earlier request from this machine, '{}', is still in the repo; ask an admin \
                 to either approve that one instead or reject it (`muster reject {}`)",
                other.name, other.name
            )
        })
        .collect();
    // Approvals are verified against this machine's copy of the fleet state;
    // anything signed against a newer state cannot be counted here.
    let state_hash = crate::fleet_state::current_hash(&ctx.config_dir)?;
    let check = req.check_approvals(manifest, state_hash.as_deref());
    if check.invalid > 0 {
        notes.push(format!(
            "{} recorded approval(s) do not verify against this machine's copy of the fleet \
             state; they were signed against a different state",
            check.invalid
        ));
    }
    if revoked {
        notes.push(
            "this machine's identity was revoked by the fleet; an approval of this request \
             will be refused"
                .to_owned(),
        );
    }
    notes.push(
        "this machine's copy of the request is only as fresh as its last sync; a newer \
         approval or rejection is invisible until then"
            .to_owned(),
    );

    let (published, publication_note) =
        request_publication(ctx, &EnrollmentRequest::rel_path(&req.name));
    let mut next = Vec::new();
    match published {
        Some(false) => next.push(
            "run `muster sync --push` to publish the request (until then no other machine can \
             approve it)"
                .to_owned(),
        ),
        None => {
            next.push("run `muster sync --push` to make sure the request is published".to_owned())
        }
        Some(true) => {}
    }
    next.push(format!(
        "an admin approves it on a fleet machine with `muster approve {}`",
        req.name
    ));
    next.push(
        "then run `muster sync --pull` here to pick up the decision (this machine has no access \
         until the approved state is pulled)"
            .to_owned(),
    );

    Ok(json!({
        "state": "pending",
        "member": false,
        "machine": req.name,
        "recipient": req.recipient,
        "created_at": req.created_at,
        "approvals_received": check.valid.len(),
        "approvals_required": manifest.required_approvals(),
        "invalid_approvals": check.invalid,
        "published": published,
        "publication_note": if publication_note.is_empty() { Value::Null } else { json!(publication_note) },
        "notes": notes,
        "next": next,
    }))
}

/// Whether this machine's enrollment request has been published to the fleet
/// remote, from local git evidence only (no network): a request that never
/// left this machine cannot be approved by anyone else, and "pending"
/// without that distinction would mislead. Returns the `published` value
/// (`true`/`false`/`None` = undeterminable) and the evidence for the
/// non-published and undeterminable cases.
fn request_publication(ctx: &Context, rel_path: &str) -> (Option<bool>, String) {
    if !crate::git::is_repo(&ctx.config_dir) {
        return (
            None,
            "the config directory is not a git repository".to_owned(),
        );
    }
    match crate::git::path_dirty(&ctx.config_dir, rel_path) {
        Ok(true) => {
            return (
                Some(false),
                format!("the request ({rel_path}) has uncommitted local changes"),
            )
        }
        Err(e) => return (None, format!("cannot inspect the local git repo: {e}")),
        Ok(false) => {}
    }
    if !crate::git::has_remote(&ctx.config_dir) {
        return (
            Some(false),
            "the config repo has no remote, so only this machine can see the request".to_owned(),
        );
    }
    match crate::git::path_on_upstream(&ctx.config_dir, rel_path) {
        Ok(Some(true)) => (Some(true), String::new()),
        Ok(Some(false)) => (
            Some(false),
            "the request is committed but has not been pushed as of the last sync".to_owned(),
        ),
        Ok(None) => (
            None,
            "no upstream branch is known locally, so whether the request was pushed cannot be \
             determined without syncing"
                .to_owned(),
        ),
        Err(e) => (None, format!("cannot inspect the upstream branch: {e}")),
    }
}

/// `muster status --format text`: a short human summary of the same JSON.
fn format_text(result: &Value) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "Config: {}\n",
        result["config_path"].as_str().unwrap_or("unknown")
    ));
    if let Some(sync) = result["sync"].as_object() {
        let ts = sync.get("ts").and_then(Value::as_str).unwrap_or("unknown");
        let outcome = sync.get("result").and_then(Value::as_str).unwrap_or("?");
        let applied = sync.get("applied").and_then(Value::as_bool);
        text.push_str(&format!(
            "Last sync: {ts} ({outcome}{})\n",
            applied
                .map(|a| if a { ", applied" } else { ", not applied" })
                .unwrap_or_default()
        ));
    } else {
        text.push_str("Last sync: never\n");
    }
    if let Some(enrollment) = result["enrollment"].as_object() {
        text.push_str(&format_enrollment_text(enrollment));
    }
    text.push_str("Machines:\n");
    if let Some(machines) = result["machines"].as_array() {
        for m in machines {
            let agents = m["agents"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_default();
            text.push_str(&format!(
                "  {}{}  {}  {}  platform {}  version {}  last sync {}\n",
                m["name"].as_str().unwrap_or("?"),
                if m["is_current"] == Value::Bool(true) {
                    " (this machine)"
                } else {
                    ""
                },
                m["status"].as_str().unwrap_or("?"),
                if agents.is_empty() {
                    "no agents".to_owned()
                } else {
                    format!("agents {agents}")
                },
                m["platform"].as_str().unwrap_or("unknown"),
                m["version"].as_str().unwrap_or("unknown"),
                m["last_sync"].as_str().unwrap_or("never"),
            ));
        }
        if machines.is_empty() {
            text.push_str("  none\n");
        }
    }
    let pending = result["pending_secret_approvals"].as_array();
    if let Some(requests) = pending.filter(|r| !r.is_empty()) {
        text.push_str("Pending secret approvals (this machine):\n");
        for r in requests {
            text.push_str(&format!(
                "  {} for {}  (muster secret approve {})\n",
                r["reference"].as_str().unwrap_or("?"),
                r["agent"].as_str().unwrap_or("?"),
                r["id"].as_str().unwrap_or("?"),
            ));
        }
    }
    let drift = result["drift"].as_object();
    let drift_count = drift
        .map(|d| {
            d.get("members")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0)
                + d.get("machines")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    .unwrap_or(0)
        })
        .unwrap_or(0);
    if drift_count > 0 {
        text.push_str(&format!("Drift: {drift_count} item(s) need attention\n"));
    }
    text
}

/// One convergence report shared by the CLI, web status and MCP status.
fn convergence(ctx: &mut Context) -> Result<Value, MusterError> {
    let plan = crate::engine::plan(
        ctx,
        &crate::engine::Options {
            target: crate::engine::Target::Global,
            agent: None,
        },
    )?;
    let planned = plan.to_json(true, false, plan.base_generation());
    let lock = crate::installer::Lock::load_or_default(&ctx.config_dir)?;
    let bin_dir = crate::installer::bin_dir()?;
    let platform = machine::platform();
    let mut agents = Vec::new();
    for (id, pin) in lock.agents {
        let artifact = pin.resolve_artifact(&platform);
        let installed_version = crate::installer::installed_version(&id, &pin, &bin_dir)?;
        let newer = installed_version
            .as_deref()
            .is_some_and(|v| crate::installer::is_newer(v, &pin.version));
        let note = if artifact.is_none() {
            Some(format!("no artifact for platform {platform}"))
        } else if newer {
            Some("newer installed version kept; bump the pin in muster.lock".to_string())
        } else {
            None
        };
        agents.push(json!({
            "id": id, "installed_version": installed_version,
            "pinned_version": pin.version, "artifact": artifact, "note": note, "kept_newer": newer,
        }));
    }
    let converged = planned["approvals"].as_array().is_some_and(Vec::is_empty)
        && planned["skipped_skills"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && agents.iter().all(|agent| {
            !agent["artifact"].is_null()
                && (agent["installed_version"] == agent["pinned_version"]
                    || agent["kept_newer"] == true)
        });
    Ok(json!({
        "converged": converged,
        "agents": agents,
        "skipped_skills": planned["skipped_skills"],
        "pending_approvals": planned["approvals"],
    }))
}
