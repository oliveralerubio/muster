//! `muster machine`: fleet inventory (`list`, `rm`, `assign`) and SSH enrollment (`add`).

use serde_json::{json, Value};

use crate::context::Context;
use crate::engine::layer;
use crate::errors::MusterError;
use crate::fleet::EnrollmentRequest;
use crate::identity::Identity;
use crate::machine;
use crate::manifest::Manifest;
use crate::registry::{CommandSpec, ParamKind, ParamSpec, Params};
use crate::team;
use crate::vault::Vault;

pub const ADD: CommandSpec = CommandSpec {
    name: "machine.add",
    about: "Enroll a remote machine into the fleet over SSH (join, enroll, approve, sync)",
    params: &[
        ParamSpec {
            name: "host",
            about: "SSH host alias or user@host of the machine to add",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "name",
            about: "Fleet name of the machine [default: its hostname]",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "role",
            about: "Role to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "profile",
            about: "Configuration profile to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        team::RECOVERY_KEY_PARAM,
        team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: add,
};

/// Program used for `ssh` / `scp`; tests point these at fake scripts.
const SSH_ENV: &str = "MUSTER_SSH";
const SCP_ENV: &str = "MUSTER_SCP";
/// Where a missing `muster` is installed on the remote (relative to `$HOME`).
const REMOTE_BIN_REL: &str = ".local/bin/muster";
const REMOTE_BIN_SHELL: &str = "\"$HOME/.local/bin/muster\"";

/// One remote machine reachable with `ssh`. Every ssh/scp call goes through
/// [`Remote::ssh`] and [`Remote::scp`].
struct Remote<'a> {
    host: &'a str,
}

impl Remote<'_> {
    fn program(var: &str, default: &str) -> std::ffi::OsString {
        std::env::var_os(var)
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.into())
    }

    /// Run `command` (a shell line, already quoted) on the host. Never prompts.
    fn ssh(&self, command: &str) -> Result<std::process::Output, MusterError> {
        std::process::Command::new(Self::program(SSH_ENV, "ssh"))
            .args(["-o", "BatchMode=yes", "--"])
            .arg(self.host)
            .arg(command)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| MusterError::Unavailable(format!("cannot run ssh: {e}")))
    }

    /// Copy `local` to `dest` (relative to the remote home).
    fn scp(&self, local: &std::path::Path, dest: &str) -> Result<(), MusterError> {
        let out = std::process::Command::new(Self::program(SCP_ENV, "scp"))
            .args(["-q", "-o", "BatchMode=yes", "--"])
            .arg(local)
            .arg(format!("{}:{dest}", self.host))
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| MusterError::Unavailable(format!("cannot run scp: {e}")))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(self.failure("copying the muster binary", &out))
        }
    }

    fn failure(&self, what: &str, out: &std::process::Output) -> MusterError {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let tail = stderr.trim().lines().last().unwrap_or("no details");
        let hint = if out.status.code() == Some(255) {
            " (check that `ssh <host>` works without a password prompt)"
        } else {
            ""
        };
        MusterError::Operational(format!("{what} failed on '{}'{hint}: {tail}", self.host))
    }

    /// Run a shell line and require success; returns trimmed stdout.
    fn run(&self, what: &str, command: &str) -> Result<String, MusterError> {
        let out = self.ssh(command)?;
        if !out.status.success() {
            return Err(self.failure(what, &out));
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Run a remote `muster --format json ...` line. Returns the exit code and
    /// the JSON envelope; a missing envelope is an error unless ssh itself failed.
    fn muster(&self, what: &str, command: &str) -> Result<(i32, Value), MusterError> {
        let out = self.ssh(command)?;
        let code = out.status.code().unwrap_or(-1);
        if code == 255 {
            return Err(self.failure(what, &out));
        }
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(envelope) => Ok((code, envelope)),
            Err(_) => Err(self.failure(what, &out)),
        }
    }

    /// Like [`Remote::muster`] but the command must succeed; returns `result`.
    fn muster_ok(&self, what: &str, command: &str, hint: &str) -> Result<Value, MusterError> {
        let (code, envelope) = self.muster(what, command)?;
        if code == 0 && envelope["ok"] == true {
            return Ok(envelope["result"].clone());
        }
        let message = envelope["diagnostics"]
            .as_array()
            .and_then(|d| d.iter().find(|d| d["severity"] == "error"))
            .and_then(|d| d["message"].as_str())
            .unwrap_or("no details");
        Err(MusterError::Operational(format!(
            "{what} failed on '{}': {message}. {hint}",
            self.host
        )))
    }
}

/// Single-quote `s` for a POSIX shell.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.@".contains(c))
}

pub fn validate_host(host: &str) -> Result<String, MusterError> {
    if valid_host(host) {
        Ok(host.to_string())
    } else {
        Err(MusterError::Usage(format!(
            "invalid SSH host '{host}': use an ssh alias or user@host (letters, digits, - _ . @)"
        )))
    }
}

/// `<os>-<arch>` in the form `machine::platform()` uses, from `uname -s` / `-m`.
fn remote_platform(os: &str, arch: &str) -> String {
    let os = match os {
        "Linux" => "linux",
        "Darwin" => "macos",
        other => return format!("{}-{arch}", other.to_ascii_lowercase()),
    };
    let arch = match arch {
        "arm64" | "aarch64" => "aarch64",
        other => other,
    };
    format!("{os}-{arch}")
}

fn check_available(manifest: &Manifest, name: &str) -> Result<(), MusterError> {
    if manifest.machines.contains_key(name) {
        return Err(MusterError::Usage(format!(
            "machine '{name}' is already a member of the fleet"
        )));
    }
    Ok(())
}

fn add(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let host = params
        .get_string("host")
        .ok_or_else(|| MusterError::Usage("SSH host is required".into()))?;
    validate_host(host)?;
    let manifest = Manifest::load(&ctx.config_dir)?;
    let explicit_name = params
        .get_string("name")
        .map(machine::validate_name)
        .transpose()?;
    if let Some(name) = &explicit_name {
        check_available(&manifest, name)?;
    }

    // Preconditions: a git remote to exchange the request/approval through,
    // and permission to approve (checked up front, before touching the host).
    if !crate::git::has_remote(&ctx.config_dir) {
        return Err(MusterError::Usage(
            "the config repo has no git remote, so the new machine cannot get the fleet config; run `muster sync --remote <url>` first".into(),
        ));
    }
    let url = crate::git::remote_url(&ctx.config_dir)?;
    if url.is_empty() || url.starts_with('-') || url.chars().any(char::is_control) {
        return Err(MusterError::Usage(
            "the git remote URL is not usable".into(),
        ));
    }
    if let Some(auth) = url.split("://").nth(1).and_then(|r| r.split('/').next()) {
        if auth.contains('@') && auth.split('@').next().is_some_and(|u| u.contains(':')) {
            return Err(MusterError::Usage(
                "the git remote URL embeds a password or token; it would be sent to the remote machine. Use an ssh URL or a credential helper".into(),
            ));
        }
    }
    // Consume recovery stdin exactly once; the key is never sent over SSH.
    let recovery = team::recovery_identity(ctx, params, &manifest)?;
    if recovery.is_none() {
        let caller = Identity::load(&ctx.state_dir)?;
        if !manifest
            .machines
            .values()
            .any(|m| m.recipient == caller.recipient())
        {
            return Err(MusterError::Security(
                "this machine is not an approved fleet member; run this from a member or pass --recovery-key-stdin".into(),
            ));
        }
        if !manifest.members.is_empty() {
            team::admin(&manifest, &ctx.state_dir)?;
        }
    }

    let name_label = explicit_name
        .clone()
        .unwrap_or_else(|| "<remote hostname>".into());
    let planned = [
        "probe the host (platform, hostname)".to_string(),
        format!("ensure muster is installed on {host} (copy this binary if the platform matches)"),
        "join the config repo on the host (muster sync --remote <origin> --pull)".to_string(),
        format!("enroll '{name_label}' on the host (muster enroll)"),
        "pull the request here and approve it (re-encrypts vault groups)".to_string(),
        "push the approval".to_string(),
        "sync the host and enable automatic sync".to_string(),
    ];
    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "machine": explicit_name,
            "host": host,
            "role": params.get_string("role"),
            "profile": params.get_string("profile"),
            "steps": planned,
            "applied": false,
        }));
    }
    ctx.confirm(&format!(
        "Enroll the machine at '{host}' and grant it access to the fleet's secrets?"
    ))?;
    if crate::git::status(&ctx.config_dir)?.changed != 0 {
        return Err(MusterError::Operational(
            "the config repo has uncommitted changes; run `muster sync` first".into(),
        ));
    }

    let remote = Remote { host };
    let mut steps: Vec<String> = Vec::new();

    // b. platform and default name.
    let probe = remote.run("reading the platform", "uname -s; uname -m; hostname")?;
    let facts: Vec<&str> = probe.lines().map(str::trim).collect();
    if facts.len() < 3 || facts.iter().any(|f| f.is_empty()) {
        return Err(MusterError::Operational(format!(
            "'{host}' did not report its OS, architecture and hostname"
        )));
    }
    let platform = remote_platform(facts[0], facts[1]);
    let name = match explicit_name {
        Some(name) => name,
        None => machine::validate_name(&sanitize_hostname(facts[2]))?,
    };
    check_available(&manifest, &name)?;
    steps.push(format!("probed {host}: {platform}, name '{name}'"));

    // c. a muster binary on the remote.
    let found = remote.ssh(
        "command -v muster || { [ -x \"$HOME/.local/bin/muster\" ] && echo \"$HOME/.local/bin/muster\"; }",
    )?;
    let found_path = String::from_utf8_lossy(&found.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .to_string();
    let bin = if found.status.success() && found_path.starts_with('/') {
        steps.push(format!("muster already installed on {host}"));
        shell_quote(&found_path)
    } else {
        if found.status.code() == Some(255) {
            return Err(remote.failure("looking for muster", &found));
        }
        let local = machine::platform();
        if platform != local {
            return Err(MusterError::Operational(format!(
                "muster is not installed on '{host}' ({platform}) and this machine is {local}, so the binary cannot be copied. Install muster on {host} (see the install instructions) and retry"
            )));
        }
        let exe = std::env::current_exe().map_err(|e| {
            MusterError::Operational(format!("cannot locate the muster binary: {e}"))
        })?;
        remote.run("creating ~/.local/bin", "mkdir -p \"$HOME/.local/bin\"")?;
        remote.scp(&exe, REMOTE_BIN_REL)?;
        remote.run(
            "making muster executable",
            &format!("chmod 755 {REMOTE_BIN_SHELL}"),
        )?;
        steps.push(format!("copied muster to ~/{REMOTE_BIN_REL} on {host}"));
        REMOTE_BIN_SHELL.to_string()
    };

    // d. join the config repo (the host needs its own git credentials).
    let has_config = remote
        .ssh("[ -f \"${XDG_CONFIG_HOME:-$HOME/.config}/muster/muster.toml\" ]")?
        .status
        .success();
    if has_config {
        steps.push(format!("{host} already has a config repo; kept as is"));
    } else {
        // The new host has no pin yet: tell it which signer to trust so it does
        // not take whatever the remote serves first (trust on first use).
        let signer = match crate::fleet_state::FleetState::load(&ctx.config_dir)? {
            Some(state) => state.payload.signer,
            None => crate::signing::Signer::load_or_create(&ctx.state_dir)?.public(),
        };
        remote.muster_ok(
            "joining the config repo",
            &format!(
                "{bin} --format json sync --expect-signer {} --remote {} --pull",
                shell_quote(&signer),
                shell_quote(&url)
            ),
            &format!("{host} needs its own git access to {url} (deploy key, ssh agent or credential helper); Muster does not copy credentials"),
        )?;
        steps.push(format!("{host} joined the config repo"));
    }

    // e. the host generates its identity and pushes the request.
    let enrolled = remote.muster_ok(
        "enrolling",
        &format!("{bin} --format json enroll --name {}", shell_quote(&name)),
        "",
    )?;
    if enrolled["pushed"] != true {
        return Err(MusterError::Operational(format!(
            "'{host}' created the enrollment request but could not push it; it needs write access to {url}, then run `muster sync --push` there and retry"
        )));
    }
    let recipient = enrolled["recipient"]
        .as_str()
        .filter(|r| r.parse::<age::x25519::Recipient>().is_ok())
        .ok_or_else(|| MusterError::Security("the host returned an invalid age recipient".into()))?
        .to_string();
    steps.push(format!(
        "{host} enrolled as '{name}' and pushed its request"
    ));

    // f. fetch the request, check it is the one the host reported, approve, push.
    // Verify what the pull brought in before trusting any of it (the remote
    // can serve a forged request or fleet state).
    let previous = crate::git::head_commit_opt(&ctx.config_dir);
    let pulled = crate::git::pull(&ctx.config_dir)?;
    let joined = matches!(pulled, crate::git::Pulled::Joined { .. });
    crate::trust::verify_pulled(ctx, previous.as_deref(), joined, false)?;
    let request = EnrollmentRequest::load(&ctx.config_dir, &name)?;
    if request.recipient != recipient {
        return Err(MusterError::Security(format!(
            "the enrollment request for '{name}' in the repo does not match the key '{host}' reported; not approving"
        )));
    }
    let mut approval_params = params.clone();
    approval_params.set("name", json!(name));
    approval_params.set("host", json!(host));
    let approval =
        crate::commands::approve::approve_with_identity(ctx, &approval_params, recovery)?;
    if approval["status"] == "pending" {
        steps.push("approval recorded; more admin approvals are required".into());
        return Ok(json!({
            "machine": name, "host": host, "recipient": recipient, "platform": platform,
            "status": "pending", "steps": steps, "applied": false, "approval": approval,
        }));
    }
    crate::git::push(&ctx.config_dir)?;
    steps.push("approved and pushed".into());

    // g. apply on the host and push its applied_profile record back to origin.
    // Without a terminal, sync leaves agent installs pending and refuses new
    // hooks/MCP servers (exit 2); both are reported, never auto-approved.
    let (code, synced) = remote.muster(
        "syncing",
        &format!("MUSTER_FLEET_HOP=1; export MUSTER_FLEET_HOP; {bin} --format json sync"),
    )?;
    let sync_ok = code == 0 && synced["ok"] == true;
    let pending = if sync_ok {
        synced["result"]["approvals"].clone()
    } else {
        Value::Null
    };
    let needs_approval = code == 2 || pending.as_array().is_some_and(|a| !a.is_empty());
    if !sync_ok && code != 2 {
        return Err(MusterError::Operational(format!(
            "'{name}' is approved but its first sync failed; run `muster sync` on {host}"
        )));
    }
    if sync_ok {
        let _ = crate::git::pull(&ctx.config_dir);
    }
    let applied = sync_ok && synced["result"]["applied"] == true;
    steps.push(if needs_approval {
        format!("{host} synced; approvals pending: run `muster sync` there in a terminal")
    } else {
        format!("{host} synced")
    });

    // h. enable periodic sync on the remote machine when a scheduler is available.
    if let Ok((0, sched)) = remote.muster(
        "enabling automatic sync",
        &format!("{bin} --format json schedule enable --yes"),
    ) {
        if sched["result"]["enabled"] == true {
            steps.push(format!("enabled automatic sync on {host}"));
        }
    }

    Ok(json!({
        "machine": name,
        "host": host,
        "recipient": recipient,
        "platform": platform,
        "status": if needs_approval { "enrolled_needs_approval" } else { "enrolled" },
        "steps": steps,
        "applied": applied,
        "pending_approvals": pending,
        "role": params.get_string("role"),
        "profile": params.get_string("profile"),
    }))
}

/// Lowercase `[a-z0-9-]` form of a hostname's first label.
fn sanitize_hostname(hostname: &str) -> String {
    let name: String = hostname
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let name = name.trim_matches('-');
    if name.is_empty() {
        "default".into()
    } else {
        name.into()
    }
}

pub const LIST: CommandSpec = CommandSpec {
    name: "machine.list",
    about: "List all enrolled machines in the fleet",
    params: &[],
    mutating: false,
    handler: list,
};

pub const RM: CommandSpec = CommandSpec {
    name: "machine.rm",
    about: "Remove a machine from the fleet (alias for revoke)",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine to remove",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        crate::team::RECOVERY_KEY_PARAM,
        crate::team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: rm,
};

pub const ASSIGN: CommandSpec = CommandSpec {
    name: "machine.assign",
    about: "Assign a role, profile or SSH host to an enrolled machine",
    params: &[
        ParamSpec {
            name: "name",
            about: "Name of the machine",
            kind: ParamKind::String,
            required: true,
            positional: true,
        },
        ParamSpec {
            name: "role",
            about: "Role to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "profile",
            about: "Configuration profile to assign to the machine",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "member",
            about: "Team member who owns the machine (admins only)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        ParamSpec {
            name: "host",
            about: "SSH host alias or user@host of the machine (empty to clear)",
            kind: ParamKind::String,
            required: false,
            positional: false,
        },
        crate::team::RECOVERY_KEY_PARAM,
        crate::team::RECOVERY_KEY_STDIN_PARAM,
    ],
    mutating: true,
    handler: assign,
};

pub const SYNC: CommandSpec = CommandSpec {
    name: "machine.sync",
    about: "Sync one or all remote fleet machines over SSH",
    params: &[ParamSpec {
        name: "name",
        about: "Fleet name of the remote machine to sync [default: all remote machines]",
        kind: ParamKind::String,
        required: false,
        positional: true,
    }],
    mutating: true,
    handler: sync_machines,
};

fn is_current_machine(
    name: &str,
    recipient: &str,
    local_name: Option<&str>,
    local_recipient: Option<&str>,
) -> bool {
    match (local_name, local_recipient) {
        (Some(ln), _) if ln == name => true,
        (_, Some(lr)) if lr == recipient => true,
        _ => false,
    }
}

fn list(ctx: &mut Context, _params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let local_identity = Identity::load(&ctx.state_dir).ok();
    let local_recipient = local_identity.as_ref().map(|id| id.recipient());
    let local_name = machine::load_name(&ctx.state_dir);
    let local_sync = machine::local_last_sync(&ctx.state_dir);

    let mut machines = Vec::new();
    for (name, m) in &manifest.machines {
        let is_current = is_current_machine(
            name,
            &m.recipient,
            local_name.as_deref(),
            local_recipient.as_deref(),
        );
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
            "status": status,
            "applied_profile": m.applied_profile.clone(),
            "recipient": m.recipient,
            "platform": m.platform,
            "version": m.muster_version,
            "agents": m.agents,
            "last_sync": last_sync,
            "role": m.role,
            "profile": m.profile,
            "is_current": is_current,
        }));
    }

    // Pending enrollment requests waiting for approval (or rejection).
    let approvals_required = manifest.required_approvals() as usize;
    let state_hash = crate::fleet_state::current_hash(&ctx.config_dir)?;
    let pending: Vec<Value> = EnrollmentRequest::pending(&ctx.config_dir)?
        .into_iter()
        .map(|req| {
            let check = req.check_approvals(&manifest, state_hash.as_deref());
            json!({
                "name": req.name,
                "host": req.host,
                "recipient": req.recipient,
                "platform": req.platform,
                "version": req.muster_version,
                "agents": req.agents,
                "created_at": req.created_at,
                "approvals": check.valid.iter().map(|a| a.member.clone()).collect::<Vec<_>>(),
                "approvals_received": check.valid.len(),
                "approvals_required": approvals_required,
                "role": req.role,
                "profile": req.profile,
                "member": req.member,
            })
        })
        .collect();

    let result = json!({
        "config_path": ctx.config_dir,
        "machines": machines,
        "pending": pending,
    });
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format_list_text(&result)));
    }
    Ok(result)
}

fn format_list_text(result: &Value) -> String {
    let mut text = String::new();
    text.push_str(&format!(
        "Config: {}\n",
        result["config_path"].as_str().unwrap_or("unknown")
    ));
    text.push_str("Machines:\n");
    let machines = result["machines"].as_array().cloned().unwrap_or_default();
    for m in &machines {
        let agents = m["agents"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let host = m["host"]
            .as_str()
            .map(|h| format!("  ssh {h}"))
            .unwrap_or_default();
        text.push_str(&format!(
            "  {}{}  {}  platform {}  version {}{}  last sync {}\n",
            m["name"].as_str().unwrap_or("?"),
            if m["is_current"] == Value::Bool(true) {
                " (this machine)"
            } else {
                ""
            },
            m["status"].as_str().unwrap_or("?"),
            m["platform"].as_str().unwrap_or("unknown"),
            m["version"].as_str().unwrap_or("unknown"),
            if agents.is_empty() {
                String::new()
            } else {
                format!("  agents {agents}")
            },
            m["last_sync"].as_str().unwrap_or("never"),
        ));
        if !host.is_empty() {
            text.push_str(&format!("    {host}\n"));
        }
    }
    if machines.is_empty() {
        text.push_str("  none\n");
    }
    let pending = result["pending"].as_array().cloned().unwrap_or_default();
    if !pending.is_empty() {
        text.push_str("Pending enrollment requests:\n");
        for p in &pending {
            text.push_str(&format!(
                "  {}  {}  approvals {}/{}\n",
                p["name"].as_str().unwrap_or("?"),
                p["platform"].as_str().unwrap_or("unknown"),
                p["approvals_received"].as_u64().unwrap_or(0),
                p["approvals_required"].as_u64().unwrap_or(0),
            ));
        }
    }
    text
}

fn rm(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    let manifest = Manifest::load(&ctx.config_dir)?;
    if manifest.machines.contains_key(name) {
        // Machine is enrolled; delegate to revoke so secrets are re-encrypted
        (crate::commands::revoke::SPEC.handler)(ctx, params)
    } else {
        // Machine is not enrolled (may have already been revoked); remove any lingering enrollment
        ctx.confirm(&format!("Remove machine record '{name}'?"))?;
        let pending = EnrollmentRequest::file_path(&ctx.config_dir, name).exists();
        if ctx.dry_run {
            return Ok(json!({ "dry_run": true, "removed": name, "pending_request": pending }));
        }
        let _ = EnrollmentRequest::remove(&ctx.config_dir, name);
        crate::trust::commit(ctx, &format!("chore(fleet): remove machine record {name}"))?;
        Ok(json!({
            "removed": name,
            "pending_request": pending,
        }))
    }
}

fn assign(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let name = params
        .get_string("name")
        .ok_or_else(|| MusterError::Usage("machine name is required".into()))?;
    machine::validate_name(name)?;

    let role = params.get_string("role");
    let profile = params.get_string("profile");
    let member = params.get_string("member");
    let host = params.get_string("host");

    if role.is_none() && profile.is_none() && member.is_none() && host.is_none() {
        return Err(MusterError::Usage(
            "specify at least one of --role, --profile, --member or --host".into(),
        ));
    }
    if let Some(h) = host {
        if !h.is_empty() {
            validate_host(h)?;
        }
    }

    let mut manifest = Manifest::load(&ctx.config_dir)?;
    // Team mode: profiles and ownership are admin changes.
    let actor = if manifest.members.is_empty() && member.is_none() {
        None
    } else {
        Some(team::admin(&manifest, &ctx.state_dir)?)
    };
    let before = manifest.clone();
    if let Some(member) = member {
        if !manifest.machines.contains_key(name) {
            return Err(MusterError::Usage(format!(
                "machine '{name}' is not enrolled"
            )));
        }
        team::materialize(&mut manifest);
        team::forget_machine(&mut manifest, name);
        manifest
            .members
            .get_mut(member)
            .ok_or_else(|| MusterError::Usage(format!("member '{member}' does not exist")))?
            .machines
            .push(name.to_string());
    }
    let entry = manifest
        .machines
        .get_mut(name)
        .ok_or_else(|| MusterError::Usage(format!("machine '{name}' is not enrolled")))?;

    if let Some(r) = role {
        entry.role = Some(r.to_string());
    }
    if let Some(p) = profile {
        entry.profile = Some(p.to_string());
    }
    if let Some(h) = host {
        entry.host = (!h.is_empty()).then(|| h.to_string());
    }
    let (role, profile, host_val) = (
        entry.role.clone(),
        entry.profile.clone(),
        entry.host.clone(),
    );

    let mut vault = Vault::open(&ctx.config_dir)?;
    let changes = team::pending_rekeys(ctx, &vault, &before, &manifest)?;
    let identity = if changes.is_empty() {
        None
    } else {
        let identity = team::unlock(ctx, params, &manifest)?;
        team::check_can_rekey(&vault, &changes, &identity)?;
        Some(identity)
    };
    if let Some(actor) = &actor {
        team::check_self_grant(ctx, &before, &manifest, actor)?;
    }
    let result = json!({
        "machine": name,
        "host": host_val,
        "role": role,
        "profile": profile,
        "member": team::owner_of(&manifest, name),
        "reencrypted_groups": changes.iter().map(|(g, _)| g.clone()).collect::<Vec<_>>(),
    });
    if ctx.dry_run {
        return Ok(result);
    }

    manifest.save(&ctx.config_dir)?;
    if let Some(identity) = identity {
        team::rekey(&mut vault, &changes, &identity)?;
    }
    let summary = format!("chore(fleet): update machine {name}");
    match &actor {
        Some(actor) => team::commit(ctx, actor, &summary)?,
        None => {
            crate::trust::commit(ctx, &summary)?;
        }
    }
    Ok(result)
}

fn remote_sync_shell(yes: bool) -> String {
    let yes_flag = if yes { " --yes" } else { "" };
    format!(
        "MUSTER_FLEET_HOP=1; export MUSTER_FLEET_HOP; \
         bin=$(command -v muster \
           || {{ [ -x \"$HOME/.local/bin/muster\" ] && echo \"$HOME/.local/bin/muster\"; }} \
           || {{ [ -x /usr/local/bin/muster ] && echo /usr/local/bin/muster; }}) \
         && [ -n \"$bin\" ] \
         && \"$bin\" --format json sync{yes_flag}"
    )
}

/// Sync one remote machine over SSH. When `strict` is false (fleet fan-out
/// during `muster sync`), SSH or remote errors become warning diagnostics so
/// an offline machine never breaks the local sync.
fn sync_one_remote(
    ctx: &mut Context,
    name: &str,
    host: &str,
    strict: bool,
) -> Result<(bool, Value), MusterError> {
    ctx.progress(&format!("Syncing {name} ({host})…"));
    let remote = Remote { host };
    let cmd = remote_sync_shell(ctx.yes);
    match remote.muster(&format!("syncing '{name}'"), &cmd) {
        Ok((0, envelope)) if envelope["ok"] == true => {
            let applied = envelope["result"]["applied"].as_bool().unwrap_or(false);
            Ok((
                true,
                json!({
                    "machine": name,
                    "host": host,
                    "ok": true,
                    "applied": applied,
                }),
            ))
        }
        Ok((2, envelope)) => {
            let msg = format!(
                "'{name}' ({host}) has pending approvals; run `muster machine sync {name} --yes` or `muster sync` on {host}"
            );
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Warning,
                "remote_sync_approval_needed",
                &msg,
            ));
            Ok((
                false,
                json!({
                    "machine": name,
                    "host": host,
                    "ok": false,
                    "approval_needed": true,
                    "diagnostics": envelope["diagnostics"],
                }),
            ))
        }
        Ok((_, envelope)) => {
            let detail = envelope["diagnostics"]
                .as_array()
                .and_then(|d| d.iter().find(|d| d["severity"] == "error"))
                .and_then(|d| d["message"].as_str())
                .unwrap_or("remote sync failed");
            let msg = format!("syncing '{name}' on '{host}' failed: {detail}");
            if strict {
                return Err(MusterError::Operational(msg));
            }
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Warning,
                "remote_sync_failed",
                &msg,
            ));
            Ok((
                false,
                json!({
                    "machine": name,
                    "host": host,
                    "ok": false,
                    "error": detail,
                }),
            ))
        }
        Err(err) => {
            if strict {
                return Err(err);
            }
            ctx.diag(crate::output::Diagnostic::new(
                crate::output::Severity::Warning,
                "remote_sync_failed",
                format!("could not sync '{name}' ({err})"),
            ));
            Ok((
                false,
                json!({
                    "machine": name,
                    "host": host,
                    "ok": false,
                    "error": err.to_string(),
                }),
            ))
        }
    }
}

/// Propagate `muster sync` over SSH to every other fleet machine that has an
/// explicit `host` recorded in `muster.toml`. Skipped on dry-run, without a git
/// remote, or inside a remote hop (`MUSTER_FLEET_HOP=1`).
pub fn sync_fleet_after_local_sync(ctx: &mut Context) -> Result<Value, MusterError> {
    if ctx.dry_run
        || std::env::var_os("MUSTER_FLEET_HOP").is_some()
        || !crate::git::has_remote(&ctx.config_dir)
    {
        return Ok(Value::Null);
    }
    let manifest = Manifest::load(&ctx.config_dir)?;
    let local_identity = Identity::load(&ctx.state_dir).ok();
    let local_recipient = local_identity.as_ref().map(|id| id.recipient());
    let local_name = machine::load_name(&ctx.state_dir);

    let targets: Vec<(String, String)> = manifest
        .machines
        .iter()
        .filter(|(name, m)| {
            !is_current_machine(
                name,
                &m.recipient,
                local_name.as_deref(),
                local_recipient.as_deref(),
            )
        })
        .filter_map(|(name, m)| {
            let host = m.host.as_deref()?.trim();
            valid_host(host).then(|| (name.clone(), host.to_string()))
        })
        .collect();

    if targets.is_empty() {
        return Ok(Value::Null);
    }

    let mut results = Vec::with_capacity(targets.len());
    let mut any_ok = false;
    for (name, host) in targets {
        let (ok, report) = sync_one_remote(ctx, &name, &host, false)?;
        any_ok |= ok;
        results.push(report);
    }
    if any_ok {
        let _ = crate::git::pull(&ctx.config_dir);
    }
    Ok(Value::Array(results))
}

fn sync_machines(ctx: &mut Context, params: &Params) -> Result<Value, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let local_identity = Identity::load(&ctx.state_dir).ok();
    let local_recipient = local_identity.as_ref().map(|id| id.recipient());
    let local_name = machine::load_name(&ctx.state_dir);

    let requested = params
        .get_string("name")
        .map(machine::validate_name)
        .transpose()?;
    let targets: Vec<(String, String)> = match &requested {
        Some(name) => {
            let m = manifest
                .machines
                .get(name)
                .ok_or_else(|| MusterError::Usage(format!("machine '{name}' is not enrolled")))?;
            if is_current_machine(
                name,
                &m.recipient,
                local_name.as_deref(),
                local_recipient.as_deref(),
            ) {
                return Err(MusterError::Usage(format!(
                    "'{name}' is this machine; run `muster sync` instead"
                )));
            }
            let host = m.host.as_deref().unwrap_or(name.as_str());
            validate_host(host)?;
            vec![(name.clone(), host.to_string())]
        }
        None => manifest
            .machines
            .iter()
            .filter(|(name, m)| {
                !is_current_machine(
                    name,
                    &m.recipient,
                    local_name.as_deref(),
                    local_recipient.as_deref(),
                )
            })
            .map(|(name, m)| {
                let host = m.host.as_deref().unwrap_or(name.as_str());
                validate_host(host).map(|h| (name.clone(), h))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    if ctx.dry_run {
        return Ok(json!({
            "dry_run": true,
            "machines": targets.iter().map(|(n, h)| json!({"machine": n, "host": h})).collect::<Vec<_>>(),
            "applied": false,
        }));
    }

    // Nothing to do: asking to "sync 0 machines" would only confuse. Checked
    // before the remote requirement: with no remote machines there is nothing
    // to exchange anyway.
    if targets.is_empty() {
        ctx.diag(crate::output::Diagnostic::new(
            crate::output::Severity::Info,
            "machine_sync_empty",
            "no remote fleet machines to sync; assign SSH hosts with `muster machine assign`",
        ));
        return Ok(json!({ "machines": [], "synced": 0 }));
    }

    if !crate::git::has_remote(&ctx.config_dir) {
        return Err(MusterError::Usage(
            "the config repo has no git remote; run `muster sync --remote <url>` first".into(),
        ));
    }

    let prompt = match &requested {
        Some(name) => format!("Sync remote machine '{name}' over SSH now?"),
        None => format!(
            "Sync {} remote fleet machine(s) over SSH now?",
            targets.len()
        ),
    };
    ctx.confirm(&prompt)?;

    let _ = crate::git::commit_if_configured(&ctx.config_dir, "chore(muster): sync");
    let _ = crate::git::push(&ctx.config_dir);

    let strict = requested.is_some();
    let mut results = Vec::with_capacity(targets.len());
    let mut any_ok = false;
    for (name, host) in targets {
        let (ok, report) = sync_one_remote(ctx, &name, &host, strict)?;
        any_ok |= ok;
        results.push(report);
    }
    if any_ok {
        let _ = crate::git::pull(&ctx.config_dir);
    }
    let result = json!({
        "machines": results,
        "synced": results.iter().filter(|r| r["ok"] == true).count(),
    });
    if ctx.format == crate::output::Format::Text {
        return Ok(Value::String(format_sync_text(&result)));
    }
    Ok(result)
}

fn format_sync_text(result: &Value) -> String {
    let machines = result["machines"].as_array().cloned().unwrap_or_default();
    if machines.is_empty() {
        return "No remote fleet machines to sync.".to_owned();
    }
    let mut text = String::new();
    for m in &machines {
        let status = if m["ok"] == Value::Bool(true) {
            match m["approval_needed"].as_bool() {
                Some(true) => "synced; approvals pending on that machine".to_owned(),
                _ => "synced".to_owned(),
            }
        } else {
            format!("failed: {}", m["error"].as_str().unwrap_or("unknown error"))
        };
        text.push_str(&format!(
            "  {} ({}): {status}\n",
            m["machine"].as_str().unwrap_or("?"),
            m["host"].as_str().unwrap_or("?"),
        ));
    }
    text.push_str(&format!(
        "{} of {} machine(s) synced.\n",
        result["synced"],
        machines.len()
    ));
    text
}
