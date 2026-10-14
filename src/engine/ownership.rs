//! Ownership records: which keys of a native file Muster manages.
//!
//! Owned keys are derived from the adapter's own output (render against an
//! empty file), so no adapter-specific knowledge lives here: a top-level object
//! owns its children (`mcpServers.github`, `hooks.PreToolUse`), anything else
//! owns itself (`model`). Markdown files own their managed block. Pruning a key
//! that is no longer desired leaves every other byte of user content alone.

use serde_json::Value;

use crate::adapters::{strip_markdown_block, FileFormat};

const BLOCK: &str = "block";

/// Keys an adapter wrote into `rendered` (its output for an empty file).
pub fn owned_keys(format: FileFormat, rendered: &str) -> Vec<Vec<String>> {
    match format {
        FileFormat::Markdown => vec![vec![BLOCK.to_string()]],
        FileFormat::Json => match serde_json::from_str::<Value>(rendered) {
            Ok(Value::Object(map)) => map
                .iter()
                .flat_map(|(k, v)| match v {
                    Value::Object(children) => children
                        .keys()
                        .map(|c| vec![k.clone(), c.clone()])
                        .collect::<Vec<_>>(),
                    _ => vec![vec![k.clone()]],
                })
                .collect(),
            _ => Vec::new(),
        },
        FileFormat::Toml => match rendered.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc
                .iter()
                .flat_map(|(k, item)| match item.as_table_like() {
                    Some(t) => t
                        .iter()
                        .map(|(c, _)| vec![k.to_string(), c.to_string()])
                        .collect::<Vec<_>>(),
                    None => vec![vec![k.to_string()]],
                })
                .collect(),
            Err(_) => Vec::new(),
        },
    }
}

/// `text` without `keys` (empty parents removed too). Unparsable text is
/// returned unchanged: the adapter's render reports it and leaves it alone.
pub fn prune(format: FileFormat, text: &str, keys: &[Vec<String>]) -> String {
    if keys.is_empty() {
        return text.to_string();
    }
    match format {
        FileFormat::Markdown => strip_markdown_block(text),
        FileFormat::Json => {
            let Ok(Value::Object(mut map)) = serde_json::from_str::<Value>(text) else {
                return text.to_string();
            };
            let mut changed = false;
            for key in keys {
                changed |= remove_json(&mut map, key);
            }
            if !changed {
                return text.to_string();
            }
            let mut out = serde_json::to_string_pretty(&Value::Object(map)).expect("JSON");
            out.push('\n');
            out
        }
        FileFormat::Toml => {
            let Ok(mut doc) = text.parse::<toml_edit::DocumentMut>() else {
                return text.to_string();
            };
            for key in keys {
                match key.as_slice() {
                    [k] => {
                        doc.remove(k);
                    }
                    [k, child] => {
                        if let Some(t) = doc.get_mut(k).and_then(|i| i.as_table_like_mut()) {
                            t.remove(child);
                            if t.is_empty() {
                                doc.remove(k);
                            }
                        }
                    }
                    _ => {}
                }
            }
            doc.to_string()
        }
    }
}

fn remove_json(map: &mut serde_json::Map<String, Value>, key: &[String]) -> bool {
    match key {
        [k] => map.remove(k).is_some(),
        [k, child] => {
            let Some(Value::Object(children)) = map.get_mut(k) else {
                return false;
            };
            let removed = children.remove(child).is_some();
            if children.is_empty() {
                map.remove(k);
            }
            removed
        }
        _ => false,
    }
}

/// True when nothing but empty structure is left (a file Muster created and
/// no longer manages can then be deleted).
pub fn is_empty(format: FileFormat, text: &str) -> bool {
    match format {
        FileFormat::Markdown => text.trim().is_empty(),
        FileFormat::Json => match serde_json::from_str::<Value>(text) {
            Ok(v) => json_empty(&v),
            Err(_) => text.trim().is_empty(),
        },
        FileFormat::Toml => match text.parse::<toml_edit::DocumentMut>() {
            Ok(doc) => doc
                .iter()
                .all(|(_, item)| item.as_table_like().is_some_and(|t| t.is_empty())),
            Err(_) => text.trim().is_empty(),
        },
    }
}

fn json_empty(v: &Value) -> bool {
    match v {
        Value::Object(map) => map.values().all(json_empty),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(list: &[&[&str]]) -> Vec<Vec<String>> {
        list.iter()
            .map(|k| k.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    #[test]
    fn json_objects_own_their_children_and_scalars_themselves() {
        let rendered = r#"{"model": "m", "mcpServers": {"github": {}}, "hooks": {}}"#;
        assert_eq!(
            owned_keys(FileFormat::Json, rendered),
            keys(&[&["model"], &["mcpServers", "github"]])
        );
    }

    #[test]
    fn pruning_json_keeps_user_keys() {
        let text = r#"{"mcpServers": {"old": {}, "mine": {}}, "user": 1}"#;
        let pruned = prune(FileFormat::Json, text, &keys(&[&["mcpServers", "old"]]));
        assert!(
            !pruned.contains("\"old\"") && pruned.contains("\"mine\""),
            "{pruned}"
        );
        assert!(pruned.contains("\"user\""));
        let all = prune(FileFormat::Json, &pruned, &keys(&[&["mcpServers", "mine"]]));
        assert!(!all.contains("mcpServers"), "empty parent removed: {all}");
    }

    #[test]
    fn pruning_toml_keeps_comments_and_user_tables() {
        let text =
            "model = \"x\"\n\n[mcp_servers.old]\ncommand = \"a\"\n\n# user config\n[user]\nk = 1\n";
        let pruned = prune(
            FileFormat::Toml,
            text,
            &keys(&[&["model"], &["mcp_servers", "old"]]),
        );
        assert_eq!(pruned.trim_start(), "# user config\n[user]\nk = 1\n");
    }

    #[test]
    fn emptiness_ignores_empty_structure() {
        assert!(is_empty(FileFormat::Json, "{\"mcpServers\": {}}\n"));
        assert!(!is_empty(FileFormat::Json, "{\"user\": 1}"));
        assert!(is_empty(FileFormat::Toml, "[mcp_servers]\n"));
        assert!(is_empty(FileFormat::Markdown, "\n"));
    }
}
