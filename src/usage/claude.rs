use super::*;

pub(super) fn scan(
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
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            skipped.push(json!({"path": path, "reason": "unreadable"}));
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            scan(&path, options, counts, malformed, skipped, sessions);
            continue;
        }
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        if !metadata.is_file() {
            continue;
        }
        if metadata
            .modified()
            .ok()
            .is_some_and(|m| OffsetDateTime::from(m) < options.cutoff)
        {
            skipped.push(json!({"path": path, "reason": "older_than_window"}));
            continue;
        }
        if metadata.len() > options.max {
            skipped.push(json!({"path": path, "reason": "too_large"}));
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            skipped.push(json!({"path": path, "reason": "unreadable"}));
            continue;
        };
        for line in BufReader::new(file).lines() {
            let Ok(line) = line else {
                skipped.push(json!({"path": path, "reason": "unreadable"}));
                break;
            };
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                *malformed += 1;
                continue;
            };
            if record["type"] != "assistant" {
                continue;
            }
            if options
                .project
                .is_some_and(|p| record["cwd"].as_str() != Some(p))
            {
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
            let Some(session) = record["sessionId"].as_str() else {
                continue;
            };
            let session = format!("{:x}", Sha256::digest(session.as_bytes()));
            sessions.insert(session.clone());
            if let Some(blocks) = record["message"]["content"].as_array() {
                for block in blocks {
                    if block["type"] != "tool_use" {
                        continue;
                    }
                    let tool = block["name"].as_str().unwrap_or("");
                    let item = if tool == "Skill" {
                        block["input"]["skill"].as_str().map(|n| ("skill", n))
                    } else if tool == "Agent" {
                        block["input"]["subagent_type"]
                            .as_str()
                            .map(|n| ("subagent", n))
                    } else {
                        tool.strip_prefix("mcp__")
                            .and_then(|s| s.split_once("__"))
                            .map(|(n, _)| ("mcp", n))
                    };
                    if let Some((kind, name)) = item {
                        let count = counts
                            .entry((kind.to_owned(), name.to_owned()))
                            .or_default();
                        count.uses += 1;
                        let breakdown = count.agents.entry("claude".into()).or_default();
                        breakdown.0 += 1;
                        breakdown.1.insert(session.clone());
                        count.sessions.insert(session.clone());
                        count.last = Some(count.last.map_or(timestamp, |t| t.max(timestamp)));
                    }
                }
            }
        }
    }
}
