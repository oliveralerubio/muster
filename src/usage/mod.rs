use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

#[derive(Default)]
struct Count {
    uses: u64,
    sessions: BTreeSet<String>,
    last: Option<OffsetDateTime>,
    agents: BTreeMap<String, (u64, BTreeSet<String>)>,
}

struct Options<'a> {
    cutoff: OffsetDateTime,
    max: u64,
    project: Option<&'a str>,
}

mod claude;
mod codex;

pub fn report(
    days: i64,
    config: &Path,
    agent: Option<&str>,
    max: u64,
    project: Option<&str>,
) -> Value {
    let options = Options {
        cutoff: OffsetDateTime::now_utc() - Duration::days(days),
        max,
        project,
    };
    let mut counts = BTreeMap::new();
    let mut malformed = 0;
    let mut skipped = Vec::new();
    let mut sessions = BTreeSet::new();
    if let Some(home) =
        std::env::var_os("HOME").filter(|_| agent.is_none() || agent == Some("claude"))
    {
        claude::scan(
            &Path::new(&home).join(".claude/projects"),
            &options,
            &mut counts,
            &mut malformed,
            &mut skipped,
            &mut sessions,
        );
    }
    let mut codex_malformed = 0;
    let mut codex_skipped = Vec::new();
    let mut codex_sessions = BTreeSet::new();
    if let Some(home) =
        std::env::var_os("HOME").filter(|_| agent.is_none() || agent == Some("codex"))
    {
        codex::scan_codex(
            &Path::new(&home).join(".codex/sessions"),
            &options,
            &mut counts,
            &mut codex_malformed,
            &mut codex_skipped,
            &mut codex_sessions,
        );
    }
    let mut managed = BTreeSet::new();
    if let Ok(manifest) = crate::manifest::Manifest::load(config) {
        for name in manifest.mcp.keys() {
            managed.insert(("mcp".to_owned(), name.clone()));
        }
        if let Some(subagents) = manifest.agents.as_ref().and_then(|a| a.subagents.as_ref()) {
            if let Ok(entries) = std::fs::read_dir(config.join(&subagents.source)) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|s| s.to_str()) == Some("md") {
                        if let Some(name) = path.file_stem().and_then(|s| s.to_str()) {
                            managed.insert(("subagent".to_owned(), name.to_owned()));
                        }
                    }
                }
            }
        }
        let source = manifest
            .agents
            .as_ref()
            .and_then(|a| a.skills.as_ref())
            .map_or("skills", |s| &s.source);
        if let Ok(entries) = std::fs::read_dir(config.join(source)) {
            for entry in entries.flatten() {
                if entry.path().join("SKILL.md").is_file() {
                    if let Some(name) = entry.file_name().to_str() {
                        managed.insert(("skill".to_owned(), name.to_owned()));
                    }
                }
            }
        }
    }
    let unused: Vec<Value> = managed.iter().filter(|key| !counts.contains_key(*key)).map(|(kind, name)| json!({"kind": kind, "name": name, "suggestion": format!("muster disable {kind}:{name}")})).collect();
    for key in managed {
        counts.entry(key).or_default();
    }
    let items: Vec<Value> = counts.into_iter().map(|((kind, name), c)| json!({"kind": kind, "name": name, "uses": c.uses, "sessions": c.sessions.len(), "last_used": c.last.and_then(|t| t.format(&Rfc3339).ok()), "agents": c.agents.into_iter().map(|(a, (uses, sessions))| (a, json!({"uses": uses, "sessions": sessions.len()}))).collect::<BTreeMap<_,_>>()})).collect();
    json!({"items": items, "unused": unused, "sessions_scanned": {"claude": sessions.len(), "codex": codex_sessions.len()}, "sources": [
        {"agent": "claude", "status": "supported", "directory": "~/.claude/projects", "malformed_lines": malformed, "skipped": skipped},
        {"agent": "codex", "status": "partial", "directory": "~/.codex/sessions", "malformed_lines": codex_malformed, "skipped": codex_skipped},
        {"agent": "opencode", "status": "unsupported", "directory": "~/.local/share/opencode"},
        {"agent": "antigravity", "status": "unsupported", "directory": "~/.gemini"}
    ]})
}
