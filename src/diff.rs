//! Diff engine (spec 03)
//!
//! Parses unified diffs into structured data, answering two questions:
//! 1. Review scope: which files and which lines were changed;
//! 2. Commentable-line mapping: for each file, which line numbers on the RIGHT
//!    side can host inline comments.
//!
//! Parsing is a fault-tolerant state machine: it never panics and never returns
//! Err (input is treated as untrusted data).

use std::collections::BTreeSet;

#[derive(Debug, Default)]
pub struct ParsedDiff {
    pub files: Vec<FileDiff>,
}

#[derive(Debug)]
pub struct FileDiff {
    /// New path (`+++ b/` side)
    pub path: String,
    /// Old path in case of a rename
    pub old_path: Option<String>,
    pub kind: FileKind,
    pub hunks: Vec<Hunk>,
    /// All commentable line numbers on the RIGHT side (context + added)
    commentable: BTreeSet<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Added,
    Modified,
    Deleted,
    Renamed,
}

#[derive(Debug)]
pub struct Hunk {
    pub new_start: u64,
    pub lines: Vec<DiffLine>,
}

#[derive(Debug)]
pub enum DiffLine {
    Context(String),
    Added(String),
    Deleted,
}

impl FileDiff {
    fn hunk_of_new_line(&self, line: u64) -> Option<&Hunk> {
        self.hunks.iter().find(|h| {
            let span = h
                .lines
                .iter()
                .filter(|l| !matches!(l, DiffLine::Deleted))
                .count() as u64;
            let end = h.new_start + span.saturating_sub(1);
            (h.new_start..=end).contains(&line)
        })
    }
}

impl ParsedDiff {
    /// Fault-tolerant parsing: unrecognized lines are skipped; never fails
    pub fn parse(input: &str) -> ParsedDiff {
        let mut out = ParsedDiff::default();
        let mut current: Option<FileDiff> = None;
        let mut new_line: u64 = 0;
        let mut in_hunk = false;

        for line in input.lines() {
            // New file starts -> back to file-header state
            if line.starts_with("diff --git ") {
                if let Some(f) = current.take() {
                    out.files.push(f);
                }
                current = Some(FileDiff {
                    path: String::new(),
                    old_path: None,
                    kind: FileKind::Modified,
                    hunks: Vec::new(),
                    commentable: BTreeSet::new(),
                });
                in_hunk = false;
                continue;
            }

            let Some(file) = current.as_mut() else {
                continue; // ignore anything before the first diff --git
            };

            // hunk header: enter the hunk body
            if let Some(start) = parse_hunk_header(line) {
                new_line = start;
                in_hunk = true;
                file.hunks.push(Hunk {
                    new_start: start,
                    lines: Vec::new(),
                });
                continue;
            }

            if !in_hunk {
                // File-header state: only here are +++ / --- / rename markers
                // recognized; content lines inside a hunk body that start with
                // these strings are not misread
                if let Some(path) = line.strip_prefix("+++ b/") {
                    file.path = path.to_string();
                } else if line.starts_with("+++ /dev/null") {
                    file.kind = FileKind::Deleted;
                } else if let Some(old) = line.strip_prefix("--- a/") {
                    // Deleted files have no +++ side, so path comes from the old
                    // side; on rename it is overwritten by +++
                    if file.path.is_empty() {
                        file.path = old.to_string();
                    }
                } else if line.starts_with("--- /dev/null") {
                    file.kind = FileKind::Added;
                } else if let Some(old) = line.strip_prefix("rename from ") {
                    file.old_path = Some(old.to_string());
                    file.kind = FileKind::Renamed;
                }
                continue;
            }

            // hunk body
            let content = &line[1.min(line.len())..];
            match line.as_bytes().first() {
                Some(b' ') => {
                    file.commentable.insert(new_line);
                    push_line(file, DiffLine::Context(content.to_string()));
                    new_line += 1;
                }
                Some(b'+') => {
                    file.commentable.insert(new_line);
                    push_line(file, DiffLine::Added(content.to_string()));
                    new_line += 1;
                }
                Some(b'-') => {
                    push_line(file, DiffLine::Deleted);
                }
                _ => {} // "\ No newline at end of file" and others
            }
        }

        if let Some(f) = current.take() {
            out.files.push(f);
        }
        // Drop file sections without a path (abnormal input)
        out.files.retain(|f| !f.path.is_empty());
        out
    }

    pub fn commentable_lines(&self, path: &str) -> Option<&BTreeSet<u64>> {
        self.files
            .iter()
            .find(|f| f.path == path)
            .map(|f| &f.commentable)
    }

    pub fn file(&self, path: &str) -> Option<&FileDiff> {
        self.files.iter().find(|f| f.path == path)
    }

    /// Find the nearest valid anchor for `line` in `path`: nearest line in the
    /// same hunk -> globally nearest line (spec 03)
    pub fn nearest_anchor(&self, path: &str, line: u64) -> Option<u64> {
        let file = self.file(path)?;
        if file.commentable.contains(&line) {
            return Some(line);
        }
        if let Some(hunk) = file.hunk_of_new_line(line) {
            // Find the nearest commentable line within the same hunk
            let mut best: Option<u64> = None;
            let mut cur = hunk.new_start;
            for l in &hunk.lines {
                let is_commentable = !matches!(l, DiffLine::Deleted);
                if is_commentable {
                    if best.is_none_or(|b| cur.abs_diff(line) < b.abs_diff(line)) {
                        best = Some(cur);
                    }
                    cur += 1;
                }
            }
            if best.is_some() {
                return best;
            }
        }
        // Globally nearest
        file.commentable
            .iter()
            .min_by_key(|l| l.abs_diff(line))
            .copied()
    }

    /// Code text of the line (added/context) (used for fingerprints, spec 07)
    pub fn line_content(&self, path: &str, line: u64) -> Option<&str> {
        let file = self.file(path)?;
        for hunk in &file.hunks {
            let mut cur = hunk.new_start;
            for l in &hunk.lines {
                match l {
                    DiffLine::Deleted => {}
                    DiffLine::Context(s) | DiffLine::Added(s) => {
                        if cur == line {
                            return Some(s.as_str());
                        }
                        cur += 1;
                    }
                }
            }
        }
        None
    }

    /// File list fed to the model
    pub fn file_list(&self) -> Vec<String> {
        self.files.iter().map(|f| f.path.clone()).collect()
    }

    /// Total number of added lines (used by the spec 05 small-diff check)
    pub fn added_line_count(&self) -> u64 {
        self.files
            .iter()
            .flat_map(|f| &f.hunks)
            .flat_map(|h| &h.lines)
            .filter(|l| matches!(l, DiffLine::Added(_)))
            .count() as u64
    }
}

/// Extract the section of a file from the diff text (for verifier display)
pub fn section_for_file<'a>(input: &'a str, path: &str) -> Option<&'a str> {
    split_sections(input)
        .into_iter()
        .find(|s| section_path(s) == Some(path))
}

/// Text-level filtering (spec 03): runs before parsing, ensuring the diff fed
/// to the model matches the parsed input.
/// Returns (filtered diff text, number of excluded files).
///
/// The decision itself lives in [`crate::units::select`] (spec 14 §2): this is a
/// thin wrapper kept for the anchoring path, which needs the filtered text
/// without a size budget. Preview and the analysis both go through the selector,
/// so no caller derives its own answer.
pub fn filter_text(input: &str, ignore: &globset::GlobSet) -> (String, usize) {
    let selection = crate::units::select(input, &crate::units::SelectOptions::unbounded(ignore));
    let excluded = selection.excluded_count();
    (selection.text, excluded)
}

/// Split on `diff --git ` boundaries (boundary line kept at the start of each section)
pub(crate) fn split_sections(input: &str) -> Vec<&str> {
    let mut boundaries: Vec<usize> = Vec::new();
    let mut pos = 0;
    for line in input.split_inclusive('\n') {
        if line.starts_with("diff --git ") {
            boundaries.push(pos);
        }
        pos += line.len();
    }
    if boundaries.is_empty() || boundaries[0] != 0 {
        boundaries.insert(0, 0);
    }
    boundaries.push(input.len());
    boundaries
        .windows(2)
        .map(|w| &input[w[0]..w[1]])
        .filter(|s| !s.trim().is_empty())
        .collect()
}

/// Extract the file path from a section: prefer `+++ b/`, then `--- a/`, finally the diff --git header
pub(crate) fn section_path(section: &str) -> Option<&str> {
    let mut old: Option<&str> = None;
    for line in section.lines() {
        if line.starts_with("@@ ") {
            break;
        }
        if let Some(p) = line.strip_prefix("+++ b/") {
            return Some(p);
        }
        if let Some(p) = line.strip_prefix("--- a/") {
            old = Some(p);
        }
    }
    old.or_else(|| {
        section
            .lines()
            .next()?
            .strip_prefix("diff --git a/")?
            .split(" b/")
            .next()
    })
}

pub(crate) fn looks_generated(section: &str) -> bool {
    section
        .lines()
        .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
        .take(5)
        .any(|l| l.contains("Code generated") && l.contains("DO NOT EDIT"))
}

/// Result of large-diff truncation
pub struct Truncation {
    pub text: String,
    /// Files dropped entirely (the model must be told about them)
    pub truncated_files: Vec<String>,
}

/// File priority: source code > tests > docs > config > other (spec 03)
const CODE_EXTS: &[&str] = &[
    "rs", "go", "py", "js", "jsx", "ts", "tsx", "java", "kt", "kts", "c", "h", "cc", "cpp", "cxx",
    "hpp", "cs", "rb", "php", "swift", "scala", "sh", "bash", "zsh", "sql", "vue", "svelte", "lua",
    "pl", "r", "dart", "ex", "exs", "erl", "hrl", "clj", "hs", "ml", "fs", "fsx", "vb", "groovy",
];
const DOC_EXTS: &[&str] = &["md", "markdown", "rst", "adoc", "txt"];
const CONFIG_EXTS: &[&str] = &[
    "toml",
    "yaml",
    "yml",
    "json",
    "ini",
    "cfg",
    "xml",
    "properties",
    "lock",
    "gradle",
];

fn path_priority(path: Option<&str>) -> u8 {
    let Some(path) = path else { return 5 };
    let lower = path.to_ascii_lowercase();
    let ext = lower.rsplit('.').next().unwrap_or("");
    let looks_test = [
        "/tests/",
        "/test/",
        "test_",
        "_test.",
        ".test.",
        ".spec.",
        "/spec/",
        "/fixtures/",
    ]
    .iter()
    .any(|m| lower.contains(m));
    if CODE_EXTS.contains(&ext) {
        if looks_test { 1 } else { 0 }
    } else if looks_test {
        1
    } else if DOC_EXTS.contains(&ext) {
        2
    } else if CONFIG_EXTS.contains(&ext) {
        3
    } else {
        4
    }
}

/// Whether a changed path is in the reviewable-kind allowlist (spec 14 §2,
/// `extension`).
///
/// Reuses the priority table so there is exactly one list of "kinds we know how
/// to review", plus a small set of extensionless files that are worth reading
/// (`Dockerfile`, `Makefile`, ...).
pub(crate) fn is_reviewable_path(path: &str) -> bool {
    if path_priority(Some(path)) < 4 {
        return true;
    }
    let base = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    if KNOWN_BASENAMES.contains(&base.as_str()) {
        return true;
    }
    // Build definitions carry variants as suffixes (`Dockerfile.prod`,
    // `Makefile.am`), and those are exactly the files worth reviewing.
    if KNOWN_BASENAME_PREFIXES
        .iter()
        .any(|prefix| base.starts_with(prefix))
    {
        return true;
    }
    let ext = base.rsplit('.').next().unwrap_or("");
    EXTRA_REVIEWABLE_EXTS.contains(&ext)
}

/// Kinds that are reviewable but absent from the spec 03 priority table:
/// dependency manifests (`go.mod`), schema definitions (protobuf, GraphQL) and
/// infrastructure/template sources. Found by running the strict gate over a real
/// pull request, where `go.mod` was the one file mis-classified as unknown.
const EXTRA_REVIEWABLE_EXTS: &[&str] = &[
    "mod", "work", "proto", "graphql", "gql", "tf", "tfvars", "hcl", "s", "asm", "sv", "v", "j2",
    "tpl", "hbs", "mustache", "zig", "nix", "sol",
];

/// Extensionless files whose content is reviewable code or build definition.
const KNOWN_BASENAMES: &[&str] = &[
    "procfile",
    "vagrantfile",
    "justfile",
    "build",
    "workspace",
    ".gitignore",
    ".gitattributes",
    ".gitmodules",
    ".dockerignore",
    ".editorconfig",
];

/// Build-definition files that carry their variant as a suffix.
const KNOWN_BASENAME_PREFIXES: &[&str] = &["dockerfile", "makefile", "gnumakefile", "jenkinsfile"];

/// Large-diff truncation (spec 03):
/// - truncates at whole-file granularity (never cuts a file in half);
/// - keeps files by priority; the first file is always kept, even over budget (floor guarantee);
/// - the list of truncated files is returned to the caller for prompt injection.
pub fn truncate_text(input: &str, max_kb: usize) -> Truncation {
    let budget = max_kb * 1024;
    if input.len() <= budget {
        return Truncation {
            text: input.to_string(),
            truncated_files: Vec::new(),
        };
    }

    let sections = split_sections(input);
    // (original index, priority) sorted to decide the keep order
    let mut order: Vec<(usize, u8)> = sections
        .iter()
        .enumerate()
        .map(|(i, s)| (i, path_priority(section_path(s))))
        .collect();
    order.sort_by_key(|(i, prio)| (*prio, *i));

    let mut kept = vec![false; sections.len()];
    let mut used = 0usize;
    let mut truncated = Vec::new();
    for (i, _) in &order {
        let len = sections[*i].len();
        if used == 0 || used + len <= budget {
            kept[*i] = true;
            used += len;
        } else if let Some(p) = section_path(sections[*i]) {
            truncated.push(p.to_string());
        }
    }

    // Output in the original file order
    let mut text = String::with_capacity(used);
    for (i, s) in sections.iter().enumerate() {
        if kept[i] {
            text.push_str(s);
            if !s.ends_with('\n') {
                text.push('\n');
            }
        }
    }
    Truncation {
        text,
        truncated_files: truncated,
    }
}

fn push_line(file: &mut FileDiff, l: DiffLine) {
    if let Some(h) = file.hunks.last_mut() {
        h.lines.push(l);
    }
}

/// Parse `@@ -a[,b] +c[,d] @@`, returning the new-side start line c
fn parse_hunk_header(line: &str) -> Option<u64> {
    let rest = line.strip_prefix("@@ -")?;
    let plus = rest.find(" +")?;
    let after_plus = &rest[plus + 2..];
    let num_end = after_plus.find([',', ' ']).unwrap_or(after_plus.len());
    after_plus[..num_end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -10,6 +10,7 @@ fn main() {
 context
-old
+new
+new2
 context2
";

    #[test]
    fn parses_commentable_lines() {
        let d = ParsedDiff::parse(SIMPLE);
        let lines = d.commentable_lines("src/main.rs").unwrap();
        // context=10, new=11, new2=12, context2=13
        assert!(lines.contains(&10));
        assert!(lines.contains(&11));
        assert!(lines.contains(&12));
        assert!(lines.contains(&13));
        assert!(!lines.contains(&99));
    }

    #[test]
    fn hunk_header_variants() {
        assert_eq!(parse_hunk_header("@@ -1,2 +5,9 @@"), Some(5));
        assert_eq!(parse_hunk_header("@@ -10 +12 @@ fn x() {"), Some(12));
        assert_eq!(parse_hunk_header("not a hunk"), None);
    }

    #[test]
    fn literal_diff_markers_inside_content_not_misread() {
        // Someone pasted a diff into a doc: content lines starting with "+++ b/"
        // or "diff --git" must appear with a '+' prefix inside the hunk body and
        // must not be treated as file headers
        let input = "\
diff --git a/docs/guide.md b/docs/guide.md
--- a/docs/guide.md
+++ b/docs/guide.md
@@ -1,2 +1,4 @@
 intro
+example: +++ b/fake.rs
+example: @@ -1,1 +1,1 @@
 tail
";
        let d = ParsedDiff::parse(input);
        assert_eq!(d.files.len(), 1);
        assert_eq!(d.files[0].path, "docs/guide.md");
        let lines = d.commentable_lines("docs/guide.md").unwrap();
        assert!(lines.contains(&2));
        assert!(lines.contains(&3));
    }

    #[test]
    fn new_and_deleted_files() {
        let input = "\
diff --git a/new.txt b/new.txt
new file mode 100644
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,2 @@
+a
+b
diff --git a/old.txt b/old.txt
deleted file mode 100644
--- a/old.txt
+++ /dev/null
@@ -1,2 +0,0 @@
-a
-b
";
        let d = ParsedDiff::parse(input);
        assert_eq!(d.file("new.txt").unwrap().kind, FileKind::Added);
        assert_eq!(d.file("old.txt").unwrap().kind, FileKind::Deleted);
        assert_eq!(d.commentable_lines("new.txt").unwrap().len(), 2);
        // Deleted files exist but have no commentable lines on the RIGHT side
        assert!(d.commentable_lines("old.txt").unwrap().is_empty());
    }

    #[test]
    fn rename_tracked() {
        let input = "\
diff --git a/old.rs b/new.rs
similarity index 90%
rename from old.rs
rename to new.rs
--- a/old.rs
+++ b/new.rs
@@ -1,1 +1,1 @@
-x
+y
";
        let d = ParsedDiff::parse(input);
        let f = d.file("new.rs").unwrap();
        assert_eq!(f.kind, FileKind::Renamed);
        assert_eq!(f.old_path.as_deref(), Some("old.rs"));
    }

    #[test]
    fn no_newline_marker_and_empty() {
        let input = "\
diff --git a/a.txt b/a.txt
--- a/a.txt
+++ b/a.txt
@@ -1 +1 @@
-x
\\ No newline at end of file
+y
\\ No newline at end of file
";
        let d = ParsedDiff::parse(input);
        assert_eq!(d.commentable_lines("a.txt").unwrap().len(), 1);
        assert!(ParsedDiff::parse("").files.is_empty());
        assert!(ParsedDiff::parse("garbage\n\n").files.is_empty());
    }

    #[test]
    fn nearest_anchor_chain() {
        let d = ParsedDiff::parse(SIMPLE);
        // Valid lines are returned as-is
        assert_eq!(d.nearest_anchor("src/main.rs", 11), Some(11));
        // Invalid line in the same hunk (old was deleted; new line 11 is the
        // corresponding new-side position) -> snap to the nearest commentable line
        assert_eq!(d.nearest_anchor("src/main.rs", 100), Some(13));
        // File does not exist -> None
        assert_eq!(d.nearest_anchor("nope.rs", 1), None);
    }

    #[test]
    fn filter_generated_and_ignored() {
        let input = "\
diff --git a/gen.rs b/gen.rs
--- /dev/null
+++ b/gen.rs
@@ -0,0 +1 @@
+// Code generated by protoc. DO NOT EDIT.
diff --git a/real.rs b/real.rs
--- /dev/null
+++ b/real.rs
@@ -0,0 +1 @@
+fn f() {}
diff --git a/Cargo.lock b/Cargo.lock
--- a/Cargo.lock
+++ b/Cargo.lock
@@ -1 +1 @@
-a
+b
";
        let set = globset::GlobSetBuilder::new()
            .add(globset::Glob::new("**/Cargo.lock").unwrap())
            .build()
            .unwrap();
        let (text, excluded) = filter_text(input, &set);
        assert_eq!(excluded, 2); // gen.rs + Cargo.lock
        let d = ParsedDiff::parse(&text);
        assert_eq!(d.file_list(), vec!["real.rs".to_string()]);
    }

    #[test]
    fn line_content_lookup() {
        let d = ParsedDiff::parse(SIMPLE);
        assert_eq!(d.line_content("src/main.rs", 11), Some("new"));
        assert_eq!(d.line_content("src/main.rs", 10), Some("context"));
        assert_eq!(d.line_content("src/main.rs", 99), None);
    }

    #[test]
    fn truncate_keeps_source_over_docs() {
        let code = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let big_doc = format!(
            "diff --git a/docs/big.md b/docs/big.md\n--- a/docs/big.md\n+++ b/docs/big.md\n@@ -0,0 +1 @@\n+{}\n",
            "d".repeat(3000)
        );
        let input = format!("{code}{big_doc}");
        // Budget 1KB: the code section must be kept, the doc section truncated
        let t = truncate_text(&input, 1);
        assert!(t.text.contains("src/a.rs"));
        assert!(!t.text.contains("docs/big.md"));
        assert_eq!(t.truncated_files, vec!["docs/big.md".to_string()]);
    }

    #[test]
    fn truncate_keeps_first_file_even_if_over_budget() {
        let input = format!(
            "diff --git a/src/huge.rs b/src/huge.rs\n--- a/src/huge.rs\n+++ b/src/huge.rs\n@@ -0,0 +1 @@\n+{}\n",
            "x".repeat(9000)
        );
        let t = truncate_text(&input, 1);
        assert!(t.text.contains("src/huge.rs"));
        assert!(t.truncated_files.is_empty());
    }

    #[test]
    fn truncate_noop_under_budget() {
        let input = "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-x\n+y\n";
        let t = truncate_text(input, 400);
        assert_eq!(t.text, input);
        assert!(t.truncated_files.is_empty());
    }
}
