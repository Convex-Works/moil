//! Line diffs between two bundles, for the approval screen.

use std::fmt::Write as _;

use similar::{ChangeTag, TextDiff};

use crate::Bundle;

/// How one bundle file changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    pub name: &'static str,
    /// Changed regions with a few lines of context. Empty if the file is
    /// unchanged.
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub lines: Vec<DiffLine>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line number in the old file, for context and removed lines.
    pub old_line: Option<usize>,
    /// 1-based line number in the new file, for context and added lines.
    pub new_line: Option<usize>,
    /// The line without its line ending.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LineKind {
    Context,
    Added,
    Removed,
}

impl FileDiff {
    pub fn is_unchanged(&self) -> bool {
        self.hunks.is_empty()
    }

    /// Lines added and removed.
    pub fn stats(&self) -> (usize, usize) {
        let lines = self.hunks.iter().flat_map(|h| &h.lines);
        lines.fold((0, 0), |(added, removed), line| match line.kind {
            LineKind::Added => (added + 1, removed),
            LineKind::Removed => (added, removed + 1),
            LineKind::Context => (added, removed),
        })
    }

    /// The diff in unified format, for terminals.
    pub fn unified(&self) -> String {
        let mut out = String::new();
        if self.is_unchanged() {
            return out;
        }
        let _ = writeln!(out, "--- a/{}\n+++ b/{}", self.name, self.name);
        for hunk in &self.hunks {
            let old = hunk.lines.iter().filter(|l| l.kind != LineKind::Added);
            let new = hunk.lines.iter().filter(|l| l.kind != LineKind::Removed);
            let range = |mut lines: Box<dyn Iterator<Item = &DiffLine> + '_>,
                         pick: fn(&DiffLine) -> Option<usize>| {
                let count = lines.by_ref().count();
                (hunk.lines.iter().find_map(pick).unwrap_or(0), count)
            };
            let (old_start, old_len) = range(Box::new(old), |l| l.old_line);
            let (new_start, new_len) = range(Box::new(new), |l| l.new_line);
            let _ = writeln!(out, "@@ -{old_start},{old_len} +{new_start},{new_len} @@");
            for line in &hunk.lines {
                let sign = match line.kind {
                    LineKind::Context => ' ',
                    LineKind::Added => '+',
                    LineKind::Removed => '-',
                };
                let _ = writeln!(out, "{sign}{}", line.text);
            }
        }
        out
    }
}

/// How each file changed from `old` to `new`, in hash order.
pub fn diff(old: &Bundle, new: &Bundle) -> Vec<FileDiff> {
    old.files()
        .zip(new.files())
        .map(|((name, old), (_, new))| FileDiff {
            name,
            hunks: hunks(old, new),
        })
        .collect()
}

fn hunks(old: &str, new: &str) -> Vec<Hunk> {
    if old == new {
        return Vec::new();
    }
    let diff = TextDiff::from_lines(old, new);
    diff.grouped_ops(3)
        .iter()
        .map(|group| Hunk {
            lines: group
                .iter()
                .flat_map(|op| diff.iter_changes(op))
                .map(|change| DiffLine {
                    kind: match change.tag() {
                        ChangeTag::Equal => LineKind::Context,
                        ChangeTag::Insert => LineKind::Added,
                        ChangeTag::Delete => LineKind::Removed,
                    },
                    old_line: change.old_index().map(|i| i + 1),
                    new_line: change.new_index().map(|i| i + 1),
                    text: change.value().trim_end_matches(['\n', '\r']).to_owned(),
                })
                .collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bundle(script: &str, version: &str) -> Bundle {
        let manifest = format!(r#"{{"moil": 1, "name": "echo", "version": "{version}"}}"#);
        Bundle::new(script, "version = 1\n", manifest).unwrap()
    }

    #[test]
    fn reports_changed_lines_with_numbers() {
        let old = bundle("a\nb\nc\nd\ne\nf\ng\nh\ni\n", "1");
        let new = bundle("a\nb\nc\nd\nE\nf\ng\nh\ni\nj\n", "1");
        let diffs = diff(&old, &new);
        assert_eq!(
            diffs.iter().map(|d| d.name).collect::<Vec<_>>(),
            ["job.py", "job.py.lock", "manifest.json"]
        );
        assert!(diffs[1].is_unchanged() && diffs[2].is_unchanged());

        let script = &diffs[0];
        assert_eq!(script.stats(), (2, 1));
        let changed: Vec<_> = script
            .hunks
            .iter()
            .flat_map(|h| &h.lines)
            .filter(|l| l.kind != LineKind::Context)
            .collect();
        assert_eq!(
            changed,
            [
                &DiffLine {
                    kind: LineKind::Removed,
                    old_line: Some(5),
                    new_line: None,
                    text: "e".into()
                },
                &DiffLine {
                    kind: LineKind::Added,
                    old_line: None,
                    new_line: Some(5),
                    text: "E".into()
                },
                &DiffLine {
                    kind: LineKind::Added,
                    old_line: None,
                    new_line: Some(10),
                    text: "j".into()
                },
            ]
        );
    }

    #[test]
    fn unified_output_matches_the_usual_format() {
        let old = bundle("one\ntwo\nthree\n", "1");
        let new = bundle("one\n2\nthree\n", "2");
        let diffs = diff(&old, &new);
        assert_eq!(
            diffs[0].unified(),
            "--- a/job.py\n+++ b/job.py\n@@ -1,3 +1,3 @@\n one\n-two\n+2\n three\n"
        );
        assert!(
            diffs[2]
                .unified()
                .contains("-{\"moil\": 1, \"name\": \"echo\", \"version\": \"1\"}")
        );
    }

    #[test]
    fn identical_bundles_have_no_hunks() {
        let a = bundle("x\n", "1");
        assert!(diff(&a, &a).iter().all(FileDiff::is_unchanged));
    }
}
