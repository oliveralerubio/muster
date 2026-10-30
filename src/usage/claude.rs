use super::*;

pub(super) fn scan(
    dir: &Path,
    options: &Options,
    counts: &mut BTreeMap<(String, String), Count>,
    malformed: &mut u64,
    skipped: &mut Skipped,
    sessions: &mut BTreeSet<String>,
) {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return,
        Err(_) => {
            skipped.count(SkipReason::Unreadable);
            return;
        }
    };
    for entry in entries {
        let Ok(entry) = entry else {
            skipped.count(SkipReason::Unreadable);
            continue;
        };
        // Directory-entry metadata never follows symlinks.
        let Ok(file_type) = entry.file_type() else {
            skipped.count(SkipReason::Unreadable);
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        if file_type.is_dir() {
            scan(&path, options, counts, malformed, skipped, sessions);
            continue;
        }
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(file) = std::fs::File::open(&path) else {
            skipped.count(SkipReason::Unreadable);
            continue;
        };
        // The open handle's own metadata decides: skip anything that is not a
        // regular file, and bound the read so a file that grew after the size
        // check cannot stream unbounded bytes.
        let Ok(metadata) = file.metadata() else {
            skipped.count(SkipReason::Unreadable);
            continue;
        };
        if !metadata.is_file() {
            skipped.count(SkipReason::Unreadable);
            continue;
        }
        if metadata
            .modified()
            .ok()
            .is_some_and(|m| OffsetDateTime::from(m) < options.cutoff)
        {
            skipped.count(SkipReason::OlderThanWindow);
            continue;
        }
        if metadata.len() > options.max {
            skipped.count(SkipReason::TooLarge);
            continue;
        }
        for line in BufReader::new(file)
            .take(options.max.saturating_add(1))
            .lines()
        {
            let Ok(line) = line else {
                skipped.count(SkipReason::Unreadable);
                break;
            };
            let Ok(record) = serde_json::from_str::<Value>(&line) else {
                *malformed += 1;
                continue;
            };
            if record["type"] != "assistant" {
                continue;
            }
            if let Some(project) = options.project {
                let Some(cwd) = record["cwd"].as_str() else {
                    continue;
                };
                if !cwd_in_project(cwd, project) {
                    continue;
                }
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
                    if let Some((kind, name)) = item.filter(|(_, name)| valid_item_name(name)) {
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
