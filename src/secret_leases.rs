//! Local, reference-only human approvals. The lock covers policy checks,
//! mutations and audited reveals so a completed revoke blocks subsequent use.
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::audit;
use crate::context::Context;
use crate::errors::MusterError;
use crate::manifest::{Manifest, SecretApproval};
use crate::output::Diagnostic;

pub const FILE: &str = "secret-leases.json";

/// Positive whole seconds, minutes or hours, capped at one day.
pub fn parse_ttl(raw: &str) -> Result<i64, MusterError> {
    let seconds = raw
        .strip_suffix('s')
        .map(|s| (s, 1))
        .or_else(|| raw.strip_suffix('m').map(|s| (s, 60)))
        .or_else(|| raw.strip_suffix('h').map(|s| (s, 3600)))
        .and_then(|(s, unit)| s.parse::<i64>().ok()?.checked_mul(unit))
        .filter(|s| (1..=86400).contains(s));
    seconds.ok_or_else(|| {
        MusterError::Usage("TTL must be a positive duration such as 15m or 1h, at most 24h".into())
    })
}

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn id() -> String {
    format!(
        "{:x}-{:x}",
        time::OffsetDateTime::now_utc().unix_timestamp_nanos(),
        std::process::id()
    )
}
fn io(e: impl std::fmt::Display) -> MusterError {
    MusterError::Operational(format!("secret leases: {e}"))
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub reference: String,
    pub agent: String,
    pub requested_at: i64,
    pub source: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Lease {
    pub id: String,
    pub reference: String,
    pub agent: String,
    pub granted_at: i64,
    pub expires_at: i64,
    pub granted_by: String,
}
#[derive(Default, Serialize, Deserialize)]
pub struct Data {
    pub requests: Vec<Request>,
    pub leases: Vec<Lease>,
}
pub struct Store {
    path: PathBuf,
    _lock: File,
    pub data: Data,
}
impl Store {
    pub fn open(state: &Path) -> Result<Self, MusterError> {
        std::fs::create_dir_all(state).map_err(io)?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let lock = options.open(state.join("secret-leases.lock")).map_err(io)?;
        lock.lock().map_err(io)?;
        let path = state.join(FILE);
        let data = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(io)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Data::default(),
            Err(e) => return Err(io(e)),
        };
        let mut store = Self {
            path,
            _lock: lock,
            data,
        };
        let count = store.data.leases.len();
        store.data.leases.retain(|lease| lease.expires_at > now());
        if count != store.data.leases.len() {
            store.save()?;
        }
        Ok(store)
    }
    pub fn save(&self) -> Result<(), MusterError> {
        let tmp = self.path.with_extension(format!("{}.tmp", id()));
        let result = (|| {
            let mut options = OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
            let mut file = options.open(&tmp).map_err(io)?;
            file.write_all(&serde_json::to_vec_pretty(&self.data).map_err(io)?)
                .map_err(io)?;
            file.sync_all().map_err(io)?;
            std::fs::rename(&tmp, &self.path).map_err(io)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
    pub fn approve(
        &mut self,
        state: &Path,
        request_id: &str,
        ttl: Option<&str>,
        actor: &str,
        config: &Path,
    ) -> Result<Lease, MusterError> {
        let index = self
            .data
            .requests
            .iter()
            .position(|r| r.id == request_id)
            .ok_or_else(|| MusterError::Usage("unknown secret request".into()))?;
        let request = &self.data.requests[index];
        let manifest = Manifest::load(config)?;
        let policy = manifest
            .secrets
            .get(&request.reference)
            .filter(|p| p.allows(&request.agent, None))
            .ok_or_else(|| {
                MusterError::Security("secret no longer allowed for requesting agent".into())
            })?;
        let policy_ttl = parse_ttl(policy.approval_ttl.as_deref().unwrap_or("15m"))?;
        let ttl = ttl.map(parse_ttl).transpose()?.unwrap_or(policy_ttl);
        let lease = Lease {
            id: id(),
            reference: request.reference.clone(),
            agent: request.agent.clone(),
            granted_at: now(),
            expires_at: now() + ttl,
            granted_by: actor.into(),
        };
        audit::record_with_actor(state, "secret.approve", &lease.reference, actor)?;
        self.data.requests.remove(index);
        self.data
            .leases
            .retain(|l| l.reference != lease.reference || l.agent != lease.agent);
        self.data.leases.push(lease.clone());
        self.save()?;
        Ok(lease)
    }
}

/// Returns the held lock for lease-protected reveals, preventing a revoke from
/// completing between the lease check and the audited delivery of the value.
pub fn authorize(
    ctx: &mut Context,
    reference: &str,
    agent: &str,
) -> Result<Option<Store>, MusterError> {
    let manifest = Manifest::load(&ctx.config_dir)?;
    let Some(policy) = manifest
        .secrets
        .get(reference)
        .filter(|p| p.allows(agent, None))
    else {
        ctx.diag(Diagnostic::error(
            "secret_not_allowed_for_agent",
            format!("{reference} is not allowed for {agent}"),
        ));
        return Err(MusterError::Security("secret not allowed for agent".into()));
    };
    if policy.approval != SecretApproval::Each {
        return Ok(None);
    }
    let mut store = Store::open(&ctx.state_dir)?;
    if store
        .data
        .leases
        .iter()
        .any(|l| l.reference == reference && l.agent == agent && l.expires_at > now())
    {
        return Ok(Some(store));
    }
    let pending = store
        .data
        .requests
        .iter()
        .find(|r| r.reference == reference && r.agent == agent);
    let request_id = if let Some(request) = pending {
        request.id.clone()
    } else {
        let request_id = id();
        if !ctx.dry_run {
            audit::record_with_actor(&ctx.state_dir, "secret.request", reference, agent)?;
            store.data.requests.push(Request {
                id: request_id.clone(),
                reference: reference.into(),
                agent: agent.into(),
                requested_at: now(),
                source: if ctx.audit_actor.is_some() {
                    "mcp"
                } else {
                    "cli"
                }
                .into(),
            });
            store.save()?;
        }
        request_id
    };
    ctx.diag(
        Diagnostic::error(
            "secret_approval_required",
            format!("human approval required for {agent} to use {reference}"),
        )
        .with_suggestion(format!("muster secret approve {request_id}")),
    );
    Err(MusterError::Security("secret_approval_required".into()))
}
