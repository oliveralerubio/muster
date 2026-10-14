//! Local audit history: `<state>/history.jsonl`, one event per line.
//!
//! Records sensitive actions such as secret reveals (who/what/when, never
//! values). Lives in local state: it describes this machine, not the fleet.

use std::io::Write;
use std::path::Path;

use serde_json::{json, Value};

use crate::errors::MusterError;

const FILE: &str = "history.jsonl";

/// Append an event `{at, action, subject}`. Failing to record is an
/// error: callers must not perform the audited action without its record.
pub fn record(state_dir: &Path, action: &str, subject: &str) -> Result<(), MusterError> {
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let line = json!({ "at": at, "action": action, "subject": subject, "actor": crate::machine::load_name(state_dir) });
    let path = state_dir.join(FILE);
    let io = |e: std::io::Error| MusterError::Operational(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(state_dir).map_err(io)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path).map_err(io)?;
    writeln!(file, "{line}").map_err(io)
}

/// Recorded events, oldest first. A missing file means no events.
pub fn events(state_dir: &Path) -> Result<Vec<Value>, MusterError> {
    let path = state_dir.join(FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(MusterError::Operational(format!("{}: {e}", path.display()))),
    };
    Ok(text
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect())
}
