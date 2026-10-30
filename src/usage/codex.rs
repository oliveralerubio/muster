use super::*;

pub(super) fn scan_codex(
    dir: &Path,
    options: &Options,
    counts: &mut BTreeMap<(String, String), Count>,
    malformed: &mut u64,
    skipped: &mut Vec<Value>,
    sessions: &mut BTreeSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            skipped.push(json!({"path": dir, "reason": "unreadable"}));
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            skipped.push(json!({"path": dir, "reason": "unreadable"}));
            continue;
        };
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            skipped.push(json!({"path": path, "reason": "unreadable"}));
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            scan_codex(&path, options, counts, malformed, skipped, sessions);
            continue;
        }
        if !meta.is_file() || path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
            continue;
        }
        if meta
            .modified()
            .ok()
            .is_some_and(|m| OffsetDateTime::from(m) < options.cutoff)
        {
            skipped.push(json!({"path": path, "reason": "older_than_window"}));
            continue;
        }
        if meta.len() > options.max {
            skipped.push(json!({"path": path, "reason": "too_large"}));
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            skipped.push(json!({"path": path, "reason": "unreadable"}));
            continue;
        };
        let mut session = None;
        let mut cwd = None;
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                skipped.push(json!({"path": path, "reason": "unreadable"}));
                break;
            };
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                *malformed += 1;
                continue;
            };
            if record["type"] == "session_meta" {
                session = record["payload"]["id"]
                    .as_str()
                    .map(|id| format!("{:x}", Sha256::digest(id.as_bytes())));
                cwd = record["payload"]["cwd"].as_str().map(str::to_owned);
                continue;
            }
            if options.project.is_some_and(|p| cwd.as_deref() != Some(p)) {
                continue;
            }
            let Some(timestamp) = record["timestamp"]
                .as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
            else {
                continue;
            };
            if timestamp < options.cutoff {
                continue;
            }
            let Some(session) = session.as_ref() else {
                continue;
            };
            sessions.insert(session.clone());
            let payload = &record["payload"];
            if record["type"] == "event_msg" && payload["type"] == "mcp_tool_call_end" {
                if let Some(server) = payload["invocation"]["server"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                {
                    let count = counts.entry(("mcp".into(), server.into())).or_default();
                    count.uses += 1;
                    let breakdown = count.agents.entry("codex".into()).or_default();
                    breakdown.0 += 1;
                    breakdown.1.insert(session.clone());
                    count.sessions.insert(session.clone());
                    count.last = Some(count.last.map_or(timestamp, |t| t.max(timestamp)));
                }
                continue;
            }
            if record["type"] != "response_item"
                || payload["type"] != "custom_tool_call"
                || payload["name"] != "exec"
            {
                continue;
            }
            let Some(input) = payload["input"].as_str() else {
                continue;
            };
            if !input.contains("tools.exec_command") || !input.contains("cat ") {
                continue;
            }
            for suffix in input.split("/skills/").skip(1) {
                let Some((name, _)) = suffix.split_once("/SKILL.md") else {
                    continue;
                };
                if name.is_empty()
                    || !name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_".contains(c))
                {
                    continue;
                }
                let count = counts.entry(("skill".into(), name.into())).or_default();
                count.uses += 1;
                let breakdown = count.agents.entry("codex".into()).or_default();
                breakdown.0 += 1;
                breakdown.1.insert(session.clone());
                count.sessions.insert(session.clone());
                count.last = Some(count.last.map_or(timestamp, |t| t.max(timestamp)));
            }
        }
    }
}
