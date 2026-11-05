//! Line-based unified diff (LCS). Managed files are small config files, so the
//! quadratic table is fine; very large inputs degrade to "replace everything".

const CONTEXT: usize = 3;
const MAX_CELLS: usize = 4_000_000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Keep,
    Del,
    Add,
}

/// Unified diff of `old` -> `new` with `---`/`+++` headers naming `path`.
/// Empty when the texts are equal. With `mask_removed`, removed lines are
/// replaced by a placeholder (their text may hold secrets nobody can name).
pub fn unified(old: &str, new: &str, path: &str, mask_removed: bool) -> String {
    if old == new {
        return String::new();
    }
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let ops = script(&a, &b);

    // Old/new line number reached before each op.
    let mut pos = Vec::with_capacity(ops.len() + 1);
    let (mut i, mut j) = (0usize, 0usize);
    for (op, _) in &ops {
        pos.push((i, j));
        match op {
            Op::Keep => {
                i += 1;
                j += 1;
            }
            Op::Del => i += 1,
            Op::Add => j += 1,
        }
    }
    pos.push((i, j));

    let changes: Vec<usize> = (0..ops.len()).filter(|&k| ops[k].0 != Op::Keep).collect();
    let mut out = format!("--- a{path}\n+++ b{path}\n");
    if changes.is_empty() {
        // Only line terminators differ.
        out.push_str("@@ line endings changed @@\n");
        return out;
    }
    let mut group_start = 0;
    while group_start < changes.len() {
        let mut group_end = group_start;
        while group_end + 1 < changes.len()
            && changes[group_end + 1] - changes[group_end] <= 2 * CONTEXT + 1
        {
            group_end += 1;
        }
        let start = changes[group_start].saturating_sub(CONTEXT);
        let end = (changes[group_end] + CONTEXT + 1).min(ops.len());
        let (os, ns) = pos[start];
        let (oe, ne) = pos[end];
        let header = |s: usize, len: usize| {
            if len == 0 {
                format!("{s},0")
            } else {
                format!("{},{len}", s + 1)
            }
        };
        out.push_str(&format!(
            "@@ -{} +{} @@\n",
            header(os, oe - os),
            header(ns, ne - ns)
        ));
        for (op, line) in &ops[start..end] {
            match op {
                Op::Keep => out.push_str(&format!(" {line}\n")),
                Op::Add => out.push_str(&format!("+{line}\n")),
                Op::Del if mask_removed => {
                    out.push_str("-<removed line hidden: file contains secrets>\n")
                }
                Op::Del => out.push_str(&format!("-{line}\n")),
            }
        }
        group_start = group_end + 1;
    }
    out
}

fn script<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<(Op, &'a str)> {
    let (n, m) = (a.len(), b.len());
    if (n + 1).saturating_mul(m + 1) > MAX_CELLS {
        let mut ops: Vec<(Op, &str)> = a.iter().map(|l| (Op::Del, *l)).collect();
        ops.extend(b.iter().map(|l| (Op::Add, *l)));
        return ops;
    }
    // lcs[i][j] = LCS length of a[i..] and b[j..].
    let width = m + 1;
    let mut lcs = vec![0u32; (n + 1) * width];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i * width + j] = if a[i] == b[j] {
                lcs[(i + 1) * width + j + 1] + 1
            } else {
                lcs[(i + 1) * width + j].max(lcs[i * width + j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::with_capacity(n + m);
    while i < n && j < m {
        if a[i] == b[j] {
            ops.push((Op::Keep, a[i]));
            i += 1;
            j += 1;
        } else if lcs[(i + 1) * width + j] >= lcs[i * width + j + 1] {
            ops.push((Op::Del, a[i]));
            i += 1;
        } else {
            ops.push((Op::Add, b[j]));
            j += 1;
        }
    }
    ops.extend(a[i..].iter().map(|l| (Op::Del, *l)));
    ops.extend(b[j..].iter().map(|l| (Op::Add, *l)));
    ops
}

/// Diff of a private (secret-bearing) file: names the change, never shows any
/// line, so neither context nor additions can leak a value nobody can name.
pub fn private_placeholder(old: Option<&str>, new: Option<&str>, path: &str) -> String {
    if old == new {
        return String::new();
    }
    let what = match (old, new) {
        (None, _) => "added",
        (_, None) => "removed",
        _ => "modified",
    };
    format!("--- a{path}\n+++ b{path}\n<private file {what}>\n")
}

#[cfg(test)]
mod tests {
    use super::{private_placeholder, unified};

    #[test]
    fn private_files_never_show_lines() {
        let added = private_placeholder(None, Some("token = \"hunter2\"\n"), "/f");
        assert!(added.contains("<private file added>") && !added.contains("hunter2"));
        let modified =
            private_placeholder(Some("a=old-secret\n"), Some("a=new-secret\nb=1\n"), "/f");
        assert!(modified.contains("<private file modified>"));
        assert!(!modified.contains("secret") && !modified.contains("b=1"));
        assert!(private_placeholder(Some("x"), None, "/f").contains("<private file removed>"));
        assert_eq!(private_placeholder(Some("x"), Some("x"), "/f"), "");
    }

    #[test]
    fn equal_texts_have_no_diff() {
        assert_eq!(unified("a\nb\n", "a\nb\n", "/f", false), "");
    }

    #[test]
    fn addition_from_nothing_is_one_hunk() {
        assert_eq!(
            unified("", "x\ny\n", "/f", false),
            "--- a/f\n+++ b/f\n@@ -0,0 +1,2 @@\n+x\n+y\n"
        );
    }

    #[test]
    fn change_in_the_middle_keeps_three_lines_of_context() {
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n";
        let new = "1\n2\n3\n4\nFIVE\n6\n7\n8\n9\n";
        assert_eq!(
            unified(old, new, "/f", false),
            "--- a/f\n+++ b/f\n@@ -2,7 +2,7 @@\n 2\n 3\n 4\n-5\n+FIVE\n 6\n 7\n 8\n"
        );
    }

    #[test]
    fn masked_removals_hide_the_old_text() {
        let diff = unified("token=old\n", "token=new\n", "/f", true);
        assert!(!diff.contains("old"), "{diff}");
        assert!(diff.contains("+token=new"));
    }
}
