//! Read a chezmoi source tree into plain files Muster can manage.
//!
//! Supported attributes: `dot_` (leading `.`), `private_` (0600, inherited by
//! a directory's files), `executable_`, `readonly_`, `empty_`, `exact_`,
//! `literal_` and the `.literal` / `.tmpl` suffixes. A `.tmpl` file without
//! template syntax is a plain file; one with `{{ ... }}` cannot be translated
//! and is reported as a template. Scripts, `modify_`/`create_`/`remove_`,
//! symlinks, encrypted files and externals are skipped with a reason.
//! `.chezmoiroot` is honoured; `.chezmoiignore` lines without template syntax
//! are applied as globs on target paths.

use std::path::{Path, PathBuf};

use crate::errors::MusterError;

/// A regular file to manage under `$HOME`.
pub struct File {
    /// Path inside the chezmoi source dir (for messages).
    pub source: String,
    /// Path relative to `$HOME`.
    pub target: String,
    pub contents: String,
    pub private: bool,
    pub executable: bool,
}

/// A `.tmpl` file that uses template syntax.
pub struct Template {
    pub source: String,
    pub target: String,
    pub contents: String,
}

pub struct Skipped {
    pub source: String,
    pub reason: String,
}

#[derive(Default)]
pub struct Scan {
    pub files: Vec<File>,
    pub templates: Vec<Template>,
    pub skipped: Vec<Skipped>,
    /// `.chezmoiignore` lines that could not be applied (templated, negated).
    pub ignored_rules: Vec<String>,
}

/// Attributes parsed from one source-state name.
#[derive(Debug, PartialEq, Eq)]
struct Name {
    target: String,
    private: bool,
    executable: bool,
    empty: bool,
    template: bool,
}

const UNSUPPORTED: &[(&str, &str)] = &[
    ("run_", "chezmoi scripts are not imported"),
    ("modify_", "modify scripts are not imported"),
    ("create_", "create-only files are not imported"),
    ("remove_", "remove entries are not imported"),
    ("symlink_", "symlinks are not imported"),
    (
        "encrypted_",
        "encrypted files are not imported; move the secret to the vault",
    ),
    ("external_", "externals are not imported"),
];

/// Parse a source-state name. `Err` = unsupported entry, with the reason.
fn parse_name(name: &str, is_dir: bool) -> Result<Name, &'static str> {
    let mut rest = name;
    let mut parsed = Name {
        target: String::new(),
        private: false,
        executable: false,
        empty: false,
        template: false,
    };
    let mut literal = false;
    loop {
        if let Some((_, reason)) = UNSUPPORTED.iter().find(|(p, _)| rest.starts_with(p)) {
            return Err(reason);
        }
        if let Some(r) = rest.strip_prefix("literal_") {
            rest = r;
            literal = true;
            break;
        } else if let Some(r) = rest.strip_prefix("private_") {
            parsed.private = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("readonly_") {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("exact_").filter(|_| is_dir) {
            rest = r;
        } else if let Some(r) = rest.strip_prefix("empty_").filter(|_| !is_dir) {
            parsed.empty = true;
            rest = r;
        } else if let Some(r) = rest.strip_prefix("executable_").filter(|_| !is_dir) {
            parsed.executable = true;
            rest = r;
        } else {
            break;
        }
    }
    let mut target = match rest.strip_prefix("dot_").filter(|_| !literal) {
        Some(r) => format!(".{r}"),
        None => rest.to_string(),
    };
    if !is_dir {
        if let Some(t) = target.strip_suffix(".literal") {
            target = t.to_string();
        } else if let Some(t) = target.strip_suffix(".tmpl") {
            target = t.to_string();
            parsed.template = true;
        }
    }
    if target.is_empty() {
        return Err("empty target name");
    }
    parsed.target = target;
    Ok(parsed)
}

/// Scan a chezmoi source directory. Never writes.
pub fn scan(dir: &Path) -> Result<Scan, MusterError> {
    if !dir.is_dir() {
        return Err(MusterError::Usage(format!(
            "chezmoi source directory {} does not exist",
            dir.display()
        )));
    }
    let mut root = dir.to_path_buf();
    if let Some(sub) = read(&dir.join(".chezmoiroot"))? {
        root = dir.join(sub.trim());
    }
    if !root.is_dir() {
        return Err(MusterError::Usage(format!(
            ".chezmoiroot points to {}, which does not exist",
            root.display()
        )));
    }
    let mut scan = Scan::default();
    let rules = match read(&root.join(".chezmoiignore"))? {
        Some(text) => ignore_rules(&text, &mut scan.ignored_rules),
        None => Vec::new(),
    };
    walk(&root, &root, "", false, &rules, &mut scan)?;
    Ok(scan)
}

fn walk(
    root: &Path,
    dir: &Path,
    target_dir: &str,
    private_dir: bool,
    rules: &[String],
    scan: &mut Scan,
) -> Result<(), MusterError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| io(dir, e))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .collect();
    entries.sort();
    for path in entries {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        // chezmoi ignores every source entry starting with `.`.
        if name.starts_with('.') {
            continue;
        }
        let source = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .display()
            .to_string();
        let is_dir = path.is_dir();
        let parsed = match parse_name(&name, is_dir) {
            Ok(parsed) => parsed,
            Err(reason) => {
                scan.skipped.push(Skipped {
                    source,
                    reason: reason.into(),
                });
                continue;
            }
        };
        let target = if target_dir.is_empty() {
            parsed.target.clone()
        } else {
            format!("{target_dir}/{}", parsed.target)
        };
        if rules.iter().any(|r| ignored(r, &target)) {
            continue;
        }
        let private = private_dir || parsed.private;
        if is_dir {
            walk(root, &path, &target, private, rules, scan)?;
            continue;
        }
        let bytes = std::fs::read(&path).map_err(|e| io(&path, e))?;
        let Ok(contents) = String::from_utf8(bytes) else {
            scan.skipped.push(Skipped {
                source,
                reason: "binary (non UTF-8) files are not imported".into(),
            });
            continue;
        };
        if contents.is_empty() && !parsed.empty {
            scan.skipped.push(Skipped {
                source,
                reason: "empty file without `empty_` (chezmoi removes its target)".into(),
            });
            continue;
        }
        if parsed.template && contents.contains("{{") {
            scan.templates.push(Template {
                source,
                target,
                contents,
            });
            continue;
        }
        scan.files.push(File {
            source,
            target,
            contents,
            private,
            executable: parsed.executable,
        });
    }
    Ok(())
}

/// Usable `.chezmoiignore` globs; the rest is reported.
fn ignore_rules(text: &str, unsupported: &mut Vec<String>) -> Vec<String> {
    let mut rules = Vec::new();
    for line in text.lines() {
        let line = line.split(" #").next().unwrap_or("").trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.contains("{{") || line.starts_with('!') {
            unsupported.push(line.to_string());
            continue;
        }
        rules.push(line.trim_matches('/').to_string());
    }
    rules
}

/// `pattern` matches `target` or one of its parent directories.
fn ignored(pattern: &str, target: &str) -> bool {
    let pattern: Vec<&str> = pattern.split('/').collect();
    let parts: Vec<&str> = target.split('/').collect();
    (1..=parts.len()).any(|n| glob(&pattern, &parts[..n]))
}

/// Segment glob: `**` spans segments, `*` and `?` stay inside one.
fn glob(pattern: &[&str], parts: &[&str]) -> bool {
    match pattern.split_first() {
        None => parts.is_empty(),
        Some((&"**", rest)) => (0..=parts.len()).any(|i| glob(rest, &parts[i..])),
        Some((first, rest)) => match parts.split_first() {
            Some((part, tail)) => segment(first.as_bytes(), part.as_bytes()) && glob(rest, tail),
            None => false,
        },
    }
}

fn segment(p: &[u8], s: &[u8]) -> bool {
    match (p.split_first(), s.split_first()) {
        (None, _) => s.is_empty(),
        (Some((b'*', rest)), _) => (0..=s.len()).any(|i| segment(rest, &s[i..])),
        (Some((b'?', rest)), Some((_, tail))) => segment(rest, tail),
        (Some((c, rest)), Some((d, tail))) => c == d && segment(rest, tail),
        (Some(_), None) => false,
    }
}

fn read(path: &Path) -> Result<Option<String>, MusterError> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path, e)),
    }
}

fn io(path: &Path, e: std::io::Error) -> MusterError {
    MusterError::Operational(format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(target: &str) -> Name {
        Name {
            target: target.into(),
            private: false,
            executable: false,
            empty: false,
            template: false,
        }
    }

    #[test]
    fn attribute_prefixes_and_suffixes_map_to_target_names() {
        assert_eq!(parse_name("dot_gitconfig", false), Ok(name(".gitconfig")));
        assert_eq!(
            parse_name("private_dot_netrc", false),
            Ok(Name {
                private: true,
                ..name(".netrc")
            })
        );
        assert_eq!(
            parse_name("executable_dot_local.tmpl", false),
            Ok(Name {
                executable: true,
                template: true,
                ..name(".local")
            })
        );
        assert_eq!(parse_name("literal_dot_x", false), Ok(name("dot_x")));
        assert_eq!(parse_name("dot_x.tmpl.literal", false), Ok(name(".x.tmpl")));
        assert_eq!(parse_name("exact_dot_config", true), Ok(name(".config")));
        assert!(parse_name("run_once_setup.sh", false).is_err());
        assert!(parse_name("encrypted_private_dot_key", false).is_err());
        assert!(parse_name("symlink_dot_vimrc", false).is_err());
    }

    #[test]
    fn ignore_globs_match_targets_and_their_parents() {
        assert!(ignored("README.md", "README.md"));
        assert!(ignored(".config/foo", ".config/foo/bar.toml"));
        assert!(ignored("*.md", "notes.md"));
        assert!(!ignored("*.md", "docs/notes.md"));
        assert!(ignored("**/*.md", "docs/notes.md"));
        assert!(!ignored(".ssh/id_*", ".ssh/config"));
    }
}
