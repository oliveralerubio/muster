//! YAML mappings shared by the Hermes renderer and ownership tracking.

use yaml_rust2::yaml::Hash;
use yaml_rust2::{Yaml, YamlEmitter, YamlLoader};

use super::unparsable;
use crate::errors::MusterError;

pub(crate) fn mapping(text: &str, path: &str) -> Result<Hash, MusterError> {
    if text.trim().is_empty() {
        return Ok(Hash::new());
    }
    // Parser errors can contain source text, including credentials. Only
    // report the shape of the failure, never the parser's original message.
    let mut docs = YamlLoader::load_from_str(text).map_err(|_| unparsable(path, "invalid YAML"))?;
    if docs.len() != 1 {
        return Err(unparsable(path, "expected one YAML mapping"));
    }
    match docs.remove(0) {
        Yaml::Hash(map) => Ok(map),
        _ => Err(unparsable(path, "expected a YAML mapping")),
    }
}

pub(crate) fn text(map: Hash) -> String {
    let mut out = String::new();
    YamlEmitter::new(&mut out)
        .dump(&Yaml::Hash(map))
        .expect("YAML mapping serializes");
    out.push('\n');
    out
}

pub(crate) fn key(value: &str) -> Yaml {
    Yaml::String(value.into())
}

pub(crate) fn child<'a>(
    map: &'a mut Hash,
    name: &str,
    path: &str,
) -> Result<&'a mut Hash, MusterError> {
    let value = map
        .entry(key(name))
        .or_insert_with(|| Yaml::Hash(Hash::new()));
    match value {
        Yaml::Hash(map) => Ok(map),
        _ => Err(unparsable(path, "managed section must be a YAML mapping")),
    }
}

/// Keep unchanged block-mapping entries verbatim (including comments and YAML
/// 1.1 scalars understood by Hermes). Flow mappings are re-emitted as YAML.
pub(crate) fn merge(original: &str, updated: Hash) -> String {
    use yaml_rust2::parser::{Event, Parser};

    if updated.is_empty() {
        return text(updated);
    }
    let Ok(before) = mapping(original, "YAML") else {
        return text(updated);
    };
    if same_mapping(&before, &updated) {
        return original.to_string();
    }
    let mut depth = 0usize;
    let mut expecting_key = true;
    let mut entries = Vec::new();
    let mut end = original.len();
    let mut line_offsets = vec![0];
    line_offsets.extend(original.match_indices('\n').map(|(i, _)| i + 1));
    let mut parser = Parser::new_from_str(original);
    loop {
        let Ok((event, mark)) = parser.next_token() else {
            return text(updated);
        };
        match event {
            Event::StreamEnd => break,
            Event::MappingStart(..) | Event::SequenceStart(..) => depth += 1,
            Event::MappingEnd | Event::SequenceEnd => {
                depth = depth.saturating_sub(1);
                if depth == 1 {
                    expecting_key = true;
                }
            }
            Event::Scalar(value, ..) if depth == 1 => {
                if expecting_key {
                    // A root key must start on its own line. A flow mapping
                    // or explicit complex key needs normal serialization.
                    let start = line_offsets[mark.line() - 1];
                    if mark.col() != 0 {
                        return text(updated);
                    }
                    entries.push((key(&value), start));
                }
                expecting_key = !expecting_key;
            }
            Event::Alias(..) if depth == 1 => expecting_key = true,
            Event::DocumentEnd => {
                let offset = line_offsets[mark.line() - 1];
                if original[offset..].starts_with("...") {
                    end = offset;
                }
            }
            _ => {}
        }
    }
    let Some((_, first)) = entries.first() else {
        return text(updated);
    };
    let mut out = original[..*first].to_string();
    for (i, (k, start)) in entries.iter().enumerate() {
        let stop = entries.get(i + 1).map_or(end, |(_, start)| *start);
        let fragment = &original[*start..stop];
        let Some(value) = updated.get(k) else {
            continue;
        };
        if before.get(k).is_some_and(|old| same_value(old, value)) {
            out.push_str(fragment);
        } else if let (Some(Yaml::Hash(_)), Yaml::Hash(children)) = (before.get(k), value) {
            // Recurse through ordinary two-space block maps, keeping manual
            // entries in mcp_servers and model byte-for-byte as well.
            if let Some((header, body)) = fragment.split_once('\n') {
                let dedented: Option<String> = body
                    .split_inclusive('\n')
                    .map(|line| {
                        if line.trim().is_empty() {
                            Some(line.to_string())
                        } else {
                            line.strip_prefix("  ").map(str::to_string)
                        }
                    })
                    .collect();
                if header.trim_end().ends_with(':') {
                    if let Some(body) = dedented
                        .as_deref()
                        .filter(|body| mapping(body, "YAML").is_ok())
                    {
                        out.push_str(header);
                        out.push('\n');
                        for line in merge(body, children.clone()).split_inclusive('\n') {
                            out.push_str("  ");
                            out.push_str(line);
                        }
                        continue;
                    }
                }
            }
            let mut one = Hash::new();
            one.insert(k.clone(), value.clone());
            out.push_str(
                text(one)
                    .strip_prefix("---\n")
                    .expect("YAML document marker"),
            );
        } else {
            let mut one = Hash::new();
            one.insert(k.clone(), value.clone());
            out.push_str(
                text(one)
                    .strip_prefix("---\n")
                    .expect("YAML document marker"),
            );
        }
    }
    for (k, value) in &updated {
        if !before.contains_key(k) {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            let mut one = Hash::new();
            one.insert(k.clone(), value.clone());
            out.push_str(
                text(one)
                    .strip_prefix("---\n")
                    .expect("YAML document marker"),
            );
        }
    }
    out.push_str(&original[end..]);
    // Anchors may cross entry boundaries. If a replacement removed an anchor
    // still used elsewhere, serialize the resolved mapping instead.
    if mapping(&out, "YAML").is_ok_and(|parsed| same_mapping(&parsed, &updated)) {
        out
    } else {
        text(updated)
    }
}

// yaml-rust2's insertion-ordered hashes compare order as well as values.
// Native mapping order carries no meaning and must not force a rewrite.
fn same_mapping(left: &Hash, right: &Hash) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .all(|(key, value)| right.get(key).is_some_and(|other| same_value(value, other)))
}

fn same_value(left: &Yaml, right: &Yaml) -> bool {
    match (left, right) {
        (Yaml::Hash(left), Yaml::Hash(right)) => same_mapping(left, right),
        (Yaml::Array(left), Yaml::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_value(left, right))
        }
        _ => left == right,
    }
}
