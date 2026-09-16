//! Read-only toolset (spec 04 §4.1)
//!
//! The "eyes" of the review model. Everything is read-only at the machine level —
//! no write tools exist in the tool registry at all.
//! Framework-agnostic implementation: the metadata (name, description, JSON
//! schema) and the dispatch live here; the provider layer only converts these
//! specs into whatever shape its API wants.

use std::fmt;
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::agent::ToolCallRecord;

const MAX_READ_LINES: usize = 400;
const MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_GREP_MATCHES: usize = 50;
const MAX_GLOB_RESULTS: usize = 100;
/// Hard ceiling a caller may ask for with `limit`.
const MAX_GLOB_LIMIT: u32 = 1000;
const MAX_LIST_ENTRIES: usize = 200;
const MAX_LIST_LIMIT: u32 = 1000;
/// Oversized files skipped by grep
const MAX_GREP_FILE_BYTES: u64 = 1024 * 1024;
/// Directories always skipped during traversal (enforced beyond .gitignore)
const SKIP_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    "dist",
    ".venv",
    "venv",
    "__pycache__",
    ".idea",
];

/// Shared tool state: sandbox root, base ref, budget counter, call trace, read set
pub struct ToolShared {
    workspace: PathBuf, // canonicalized
    base_ref: String,
    max_calls: u32,
    calls: AtomicU32,
    trace: Mutex<Vec<ToolCallRecord>>,
    /// Files this run has actually read. A whole-file write to an existing file
    /// has to have seen the content it replaces; nothing else can tell the
    /// difference between a rewrite and a guess.
    read: Mutex<std::collections::HashSet<PathBuf>>,
}

impl fmt::Debug for ToolShared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolShared")
            .field("workspace", &self.workspace)
            .field("base_ref", &self.base_ref)
            .field("max_calls", &self.max_calls)
            .field("calls", &self.calls.load(Ordering::SeqCst))
            .finish()
    }
}

impl ToolShared {
    pub fn new(workspace: PathBuf, base_ref: impl Into<String>, max_calls: u32) -> Arc<ToolShared> {
        let ws = workspace.canonicalize().unwrap_or(workspace);
        Arc::new(ToolShared {
            workspace: ws,
            base_ref: base_ref.into(),
            max_calls,
            calls: AtomicU32::new(0),
            trace: Mutex::new(Vec::new()),
            read: Mutex::new(std::collections::HashSet::new()),
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn base_ref(&self) -> &str {
        &self.base_ref
    }

    /// Remember that this run has seen the working-copy content of `path`.
    fn record_read(&self, path: &Path) {
        self.read.lock().unwrap().insert(path.to_path_buf());
    }

    /// Whether this run has read the working copy of `path`.
    fn was_read(&self, path: &Path) -> bool {
        self.read.lock().unwrap().contains(path)
    }

    pub fn call_count(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn trace(&self) -> Vec<ToolCallRecord> {
        self.trace.lock().unwrap().clone()
    }

    /// Unified entry point for tool calls: budget gate + trace recording
    pub async fn run(
        &self,
        name: impl Into<String>,
        args_summary: String,
        fut: impl Future<Output = String>,
    ) -> String {
        let name = name.into();
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if n >= self.max_calls {
            return "budget exhausted: tool call budget is exhausted; please conclude with the information you already have".to_string();
        }
        let t = Instant::now();
        let out = fut.await;
        self.trace.lock().unwrap().push(ToolCallRecord {
            name,
            args_summary,
            duration: t.elapsed(),
            result_bytes: out.len(),
        });
        out
    }

    /// Path sandbox (spec 04 safety rules):
    /// rejects absolute paths, `..` escapes, and symlink escapes; returns an
    /// absolute path inside the workspace
    fn resolve_path(&self, rel: &str) -> Result<PathBuf, String> {
        if rel.is_empty() {
            return Err("empty path".to_string());
        }
        if Path::new(rel).is_absolute() {
            return Err(format!("absolute paths are not allowed: {rel}"));
        }
        // Lexical normalization (does not touch the filesystem)
        let mut normalized = PathBuf::new();
        for comp in Path::new(rel).components() {
            match comp {
                Component::CurDir => {}
                Component::Normal(c) => normalized.push(c),
                Component::ParentDir => {
                    if !normalized.pop() {
                        return Err(format!("path escape via ..: {rel}"));
                    }
                }
                _ => return Err(format!("invalid path: {rel}")),
            }
        }
        if normalized.as_os_str().is_empty() {
            return Err("empty path".to_string());
        }
        let candidate = self.workspace.join(&normalized);
        if candidate.exists() {
            let canon = candidate
                .canonicalize()
                .map_err(|e| format!("cannot access {rel}: {e}"))?;
            if !canon.starts_with(&self.workspace) {
                return Err(format!("symlink escape: {rel}"));
            }
            Ok(canon)
        } else {
            Ok(candidate)
        }
    }

    /// Like `resolve_path`, but safe for paths that don't exist yet (write
    /// tools): walks up to the nearest existing ancestor and canonicalizes
    /// it, so a symlinked directory in the middle cannot redirect a write
    /// outside the workspace.
    fn resolve_path_for_write(&self, rel: &str) -> Result<PathBuf, String> {
        let candidate = self.resolve_path(rel)?;
        if candidate.exists() {
            return Ok(candidate); // already canonicalized by resolve_path
        }
        let mut rest: Vec<PathBuf> = vec![
            candidate
                .file_name()
                .ok_or_else(|| format!("invalid path: {rel}"))?
                .into(),
        ];
        let mut anc = candidate
            .parent()
            .ok_or_else(|| format!("invalid path: {rel}"))?
            .to_path_buf();
        while !anc.exists() {
            rest.push(
                anc.file_name()
                    .ok_or_else(|| format!("invalid path: {rel}"))?
                    .into(),
            );
            anc = anc
                .parent()
                .ok_or_else(|| format!("invalid path: {rel}"))?
                .to_path_buf();
        }
        let canon = anc
            .canonicalize()
            .map_err(|e| format!("cannot access {rel}: {e}"))?;
        if !canon.starts_with(&self.workspace) {
            return Err(format!("symlink escape: {rel}"));
        }
        let mut out = canon;
        for c in rest.iter().rev() {
            out.push(c);
        }
        Ok(out)
    }

    /// Walk workspace files (gitignore-aware + forced skip dirs), yielding relative paths
    fn walk_files(&self) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let walker = ignore::WalkBuilder::new(&self.workspace)
            .hidden(true)
            .git_ignore(true)
            .filter_entry(|e| {
                !(e.file_type().is_some_and(|t| t.is_dir())
                    && SKIP_DIRS.contains(&e.file_name().to_string_lossy().as_ref()))
            })
            .build();
        for entry in walker.flatten() {
            if entry.file_type().is_some_and(|t| t.is_file())
                && let Ok(rel) = entry.path().strip_prefix(&self.workspace)
            {
                out.push(rel.to_path_buf());
            }
        }
        out
    }
}

/// Truncate output to MAX_OUTPUT_BYTES, keeping both ends.
///
/// Producers here append their notes last: a shell exit status, a diagnostic
/// line, the count of what was omitted. A head-only cut deletes exactly the
/// lines that explain the result, so the tail is kept and the middle is what
/// gets dropped.
fn cap_output(s: String) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        let tail_len = MAX_OUTPUT_BYTES / 4;
        let head_len = MAX_OUTPUT_BYTES - tail_len;
        let head = floor_char_boundary(&s, head_len);
        let tail_start = ceil_char_boundary(&s, s.len() - tail_len);
        return format!(
            "{}\n... [{} bytes omitted from the middle] ...\n{}",
            &s[..head],
            s.len() - head - (s.len() - tail_start),
            &s[tail_start..]
        );
    }
    s
}

/// Largest char boundary at or below `index`.
fn floor_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Smallest char boundary at or above `index`.
fn ceil_char_boundary(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// read_file: read a workspace file (with line numbers), ≤400 lines and ≤64KB per call
pub async fn read_file(
    shared: &ToolShared,
    path: &str,
    start: Option<u64>,
    end: Option<u64>,
) -> String {
    let p = match shared.resolve_path(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return format!("file does not exist or is not readable: {path} ({e})"),
    };
    if meta.is_dir() {
        return format!("{path} is a directory, not a file");
    }
    if meta.len() > MAX_GREP_FILE_BYTES * 4 {
        return format!("file too large ({} bytes), refusing to read", meta.len());
    }
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) => return format!("failed to read: {e}"),
    };
    let text = String::from_utf8_lossy(&bytes);
    shared.record_read(&p);
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();

    let s = start.unwrap_or(1).max(1) as usize;
    let e = end.map(|v| v as usize).unwrap_or(total).min(total);
    if s > e {
        return format!("invalid line range: {s}-{e} (file has {total} lines)");
    }

    let mut out = String::new();
    for (idx, line) in lines[s - 1..e].iter().enumerate() {
        if idx >= MAX_READ_LINES || out.len() >= MAX_OUTPUT_BYTES {
            out.push_str(&format!(
                "... [truncated: file has {total} lines, showing {idx}]\n"
            ));
            break;
        }
        out.push_str(&format!("{:>5}│{line}\n", s + idx));
    }
    if out.is_empty() {
        return "(empty file or nothing in range)".to_string();
    }
    out
}

/// grep: regex search (whole repo by default), ≤50 matches, supports context lines
pub async fn grep(
    shared: &ToolShared,
    pattern: &str,
    path: Option<&str>,
    glob: Option<&str>,
    ignore_case: bool,
    literal: bool,
    context_lines: Option<u32>,
) -> String {
    let mut source = if literal {
        regex::escape(pattern)
    } else {
        pattern.to_string()
    };
    if ignore_case {
        source = format!("(?i){source}");
    }
    let re = match regex::Regex::new(&source) {
        Ok(r) => r,
        Err(e) => return format!("invalid regex: {e}"),
    };
    let file_filter = match glob {
        Some(pattern) => match globset::Glob::new(pattern) {
            Ok(g) => Some(g.compile_matcher()),
            Err(e) => return format!("invalid glob filter: {e}"),
        },
        None => None,
    };
    let ctx = context_lines.unwrap_or(0) as usize;

    // Determine the search scope: single file / directory / whole repo
    let files: Vec<PathBuf> = match path {
        Some(p) => match shared.resolve_path(p) {
            Ok(abs) if abs.is_file() => vec![
                abs.strip_prefix(shared.workspace())
                    .unwrap_or(&abs)
                    .to_path_buf(),
            ],
            Ok(_) => {
                // Directory: walk and filter by prefix
                let prefix = p.trim_end_matches('/');
                shared
                    .walk_files()
                    .into_iter()
                    .filter(|f| f.to_string_lossy().starts_with(prefix))
                    .collect()
            }
            Err(e) => return e,
        },
        None => shared.walk_files(),
    };

    let mut out = String::new();
    let mut matches = 0usize;
    'files: for rel in &files {
        if let Some(filter) = file_filter.as_ref()
            && !filter.is_match(rel)
        {
            continue;
        }
        let abs = shared.workspace().join(rel);
        let Ok(meta) = std::fs::metadata(&abs) else {
            continue;
        };
        if meta.len() > MAX_GREP_FILE_BYTES {
            continue;
        }
        let Ok(bytes) = std::fs::read(&abs) else {
            continue;
        };
        // Binary probe: skip if the first 8KB contain a NUL
        if bytes[..bytes.len().min(8192)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            if ctx > 0 {
                let lo = i.saturating_sub(ctx);
                let hi = (i + ctx + 1).min(lines.len());
                for (j, l) in lines[lo..hi].iter().enumerate() {
                    let mark = if lo + j == i { '>' } else { ' ' };
                    out.push_str(&format!("{}:{}:{mark} {}\n", rel.display(), lo + j + 1, l));
                }
                out.push_str("--\n");
            } else {
                out.push_str(&format!("{}:{}: {}\n", rel.display(), i + 1, line));
            }
            matches += 1;
            if matches >= MAX_GREP_MATCHES || out.len() >= MAX_OUTPUT_BYTES {
                out.push_str(&format!(
                    "... [truncated: showing first {matches} matches]\n"
                ));
                break 'files;
            }
        }
    }
    if matches == 0 {
        return format!("no matches: {pattern}");
    }
    cap_output(out)
}

/// glob: find files by glob pattern, ≤100 results (or LIMIT), optionally scoped to PATH
pub async fn glob(
    shared: &ToolShared,
    pattern: &str,
    path: Option<&str>,
    limit: Option<u32>,
) -> String {
    let matcher = match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher(),
        Err(e) => return format!("invalid glob pattern: {e}"),
    };
    let scope: Option<String> = match path {
        Some(scoped) => match shared.resolve_path(scoped) {
            Ok(abs) => {
                let rel = abs
                    .strip_prefix(shared.workspace())
                    .unwrap_or(&abs)
                    .to_string_lossy()
                    .replace('\\', "/");
                Some(rel.trim_end_matches('/').to_string())
            }
            Err(e) => return e,
        },
        None => None,
    };
    let limit = limit
        .unwrap_or(MAX_GLOB_RESULTS as u32)
        .clamp(1, MAX_GLOB_LIMIT) as usize;
    let mut hits: Vec<String> = shared
        .walk_files()
        .into_iter()
        .filter(|f| matcher.is_match(f))
        .filter(|f| match scope.as_deref() {
            Some("") | None => true,
            Some(scope) => f
                .to_string_lossy()
                .replace('\\', "/")
                .starts_with(&format!("{scope}/")),
        })
        .map(|f| f.to_string_lossy().replace('\\', "/"))
        .collect();
    hits.sort();
    let total = hits.len();
    hits.truncate(limit);
    if hits.is_empty() {
        return format!("no matches: {pattern}");
    }
    let mut out = hits.join("\n");
    if total > limit {
        out.push_str(&format!("\n... [truncated: {total} matches total]"));
    }
    out.push('\n');
    out
}

/// list_dir: list one directory's entries, `dir/` for directories, bounded
pub async fn list_dir(shared: &ToolShared, path: Option<&str>, limit: Option<u32>) -> String {
    // "." and an omitted path both mean the repository root: the sandbox
    // normalizes a bare "." away, so it is resolved directly.
    let relative = path.map(str::trim).filter(|p| !p.is_empty() && *p != ".");
    let abs = match relative {
        Some(relative) => match shared.resolve_path(relative) {
            Ok(p) => p,
            Err(e) => return e,
        },
        None => shared.workspace().to_path_buf(),
    };
    let relative = relative.unwrap_or(".");
    if !abs.is_dir() {
        return format!("list_dir error: {relative} is not a directory");
    }
    let limit = limit
        .unwrap_or(MAX_LIST_ENTRIES as u32)
        .clamp(1, MAX_LIST_LIMIT) as usize;
    let entries = match std::fs::read_dir(&abs) {
        Ok(entries) => entries,
        Err(e) => return format!("list_dir error: cannot read {relative}: {e}"),
    };
    let mut rows: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        if is_dir {
            rows.push(format!("{name}/"));
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            rows.push(format!("{name} ({size} bytes)"));
        }
    }
    rows.sort();
    let total = rows.len();
    rows.truncate(limit);
    if rows.is_empty() {
        return format!("{relative} is empty");
    }
    let mut out = rows.join("\n");
    if total > limit {
        out.push_str(&format!("\n... [truncated: {total} entries total]"));
    }
    out.push('\n');
    out
}

/// Maximum file size editable via edit_file (same cap as read_file).
const MAX_EDIT_FILE_BYTES: u64 = MAX_GREP_FILE_BYTES * 4;
/// Maximum content size for write_file.
const MAX_WRITE_BYTES: usize = 256 * 1024;

/// edit_file: exact, unique-match replacement (spec 11 §4). Never fuzzy:
/// `old_string` must occur exactly once in the file.
pub async fn edit_file(shared: &ToolShared, path: &str, edits: &[(String, String)]) -> String {
    if edits.is_empty() {
        return "edit_file error: edits must contain at least one replacement".to_string();
    }
    for (index, (old, new)) in edits.iter().enumerate() {
        if old.is_empty() {
            return format!("edit_file error: edits[{index}].old_string must not be empty");
        }
        if old == new {
            return format!("edit_file error: edits[{index}] replaces text with itself");
        }
    }
    let p = match shared.resolve_path_for_write(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(e) => return format!("edit_file error: cannot stat {path}: {e}"),
    };
    if meta.is_dir() {
        return format!("edit_file error: {path} is a directory");
    }
    if meta.len() > MAX_EDIT_FILE_BYTES {
        return format!(
            "edit_file error: {path} is {} bytes, larger than the {} byte edit limit",
            meta.len(),
            MAX_EDIT_FILE_BYTES
        );
    }
    let original = match std::fs::read_to_string(&p) {
        Ok(text) => text,
        Err(e) => {
            return format!(
                "edit_file error: {path} is not readable as UTF-8 text, so exact replacement is impossible: {e}"
            );
        }
    };

    // Every replacement is matched against the ORIGINAL text (never against the
    // result of the previous one), and one occurrence is required: an ambiguous
    // match means the caller cannot say which site it meant.
    let mut spans: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for (index, (old, new)) in edits.iter().enumerate() {
        let mut found = original.match_indices(old.as_str());
        let Some((offset, _)) = found.next() else {
            return format!(
                "edit_file error: edits[{index}].old_string was not found in {path}; read the file and copy the text exactly"
            );
        };
        if found.next().is_some() {
            return format!(
                "edit_file error: edits[{index}].old_string occurs more than once in {path}; include more surrounding context to make it unique"
            );
        }
        spans.push((offset, offset + old.len(), new.as_str()));
    }
    spans.sort_by_key(|(start, _, _)| *start);
    for pair in spans.windows(2) {
        if pair[1].0 < pair[0].1 {
            return format!(
                "edit_file error: two edits overlap in {path}; merge edits that touch the same block"
            );
        }
    }

    let mut updated = String::with_capacity(original.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in &spans {
        updated.push_str(&original[cursor..*start]);
        updated.push_str(replacement);
        cursor = *end;
    }
    updated.push_str(&original[cursor..]);

    match std::fs::write(&p, updated.as_bytes()) {
        Ok(()) => {
            shared.record_read(&p);
            format!(
                "edited {path} ({} replacement(s), {} bytes)",
                edits.len(),
                updated.len()
            )
        }
        Err(e) => format!("edit_file error: failed to write {path}: {e}"),
    }
}

pub async fn write_file(shared: &ToolShared, path: &str, content: &str) -> String {
    if content.len() > MAX_WRITE_BYTES {
        return format!(
            "write_file error: content too large ({} bytes, max {MAX_WRITE_BYTES})",
            content.len()
        );
    }
    let p = match shared.resolve_path_for_write(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if p.is_dir() {
        return format!("write_file error: {path} is a directory");
    }
    if p.exists() && !shared.was_read(&p) {
        return format!(
            "write_file error: {path} already exists and this run has not read it; read it with read_file first, or use edit_file for a targeted change"
        );
    }
    if let Some(parent) = p.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return format!("write_file error: cannot create directories for {path}: {e}");
    }
    match std::fs::write(&p, content.as_bytes()) {
        Ok(()) => format!("wrote {path} ({} bytes)", content.len()),
        Err(e) => format!("write_file error: failed to write {path}: {e}"),
    }
}

/// show_base_file: read the base-branch version (the only allowed process call, fixed argument format)
pub async fn show_base_file(shared: &ToolShared, path: &str) -> String {
    // Sandbox check (prevents path escape; the file is not required to exist in
    // the workspace — it may only exist on base)
    let abs = match shared.resolve_path(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let rel = abs
        .strip_prefix(shared.workspace())
        .unwrap_or(&abs)
        .to_string_lossy()
        .replace('\\', "/");

    // Try origin/<base>, <base>, HEAD in order
    let candidates = [
        format!("origin/{}", shared.base_ref),
        shared.base_ref.clone(),
        "HEAD".to_string(),
    ];
    let mut last_err = String::new();
    for rev in &candidates {
        let spec = format!("{rev}:{rel}");
        let out = tokio::process::Command::new("git")
            .args(["show", &spec])
            .current_dir(shared.workspace())
            .output()
            .await;
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout).to_string();
                if text.is_empty() {
                    return format!("{spec} is empty");
                }
                return cap_output(text);
            }
            Ok(o) => {
                last_err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            }
            Err(e) => {
                last_err = e.to_string();
            }
        }
    }
    format!("cannot read base version of {rel} ({last_err})")
}

// ---------------------------------------------------------------------------
// Tool metadata and dispatch (spec 04 §4.1, spec 13)
// ---------------------------------------------------------------------------

/// Framework-free tool metadata handed to a provider call.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSpec {
    pub name: &'static str,
    pub description: &'static str,
    pub parameters: serde_json::Value,
}

/// The tool menu for a profile. Review is always read-only; only the develop
/// loop gets the two writing tools.
pub fn specs(profile: crate::agent::ToolProfile) -> Vec<ToolSpec> {
    let mut tools = vec![
        ToolSpec {
            name: "read_file",
            description: "Read the contents of a file in the repository (with line numbers). Use it to see context around the diff or symbol definitions.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the repository root"},
                    "start_line": {"type": "integer", "description": "Start line (1-based, inclusive), defaults to the beginning"},
                    "end_line": {"type": "integer", "description": "End line (inclusive), defaults to the end of file"}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "grep",
            description: "Search the repository with a regular expression. Use it to find call sites of a function or type. Returns matching lines with file paths and line numbers.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression"},
                    "path": {"type": "string", "description": "Optional: limit to a file or directory"},
                    "glob": {"type": "string", "description": "Optional: only search files matching this glob (e.g. **/*.rs)"},
                    "ignore_case": {"type": "boolean", "description": "Optional: case-insensitive search"},
                    "literal": {"type": "boolean", "description": "Optional: treat the pattern as literal text instead of a regex"},
                    "context_lines": {"type": "integer", "description": "Optional: context lines around each match"}
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: "glob",
            description: "Find files matching a glob pattern. Use it to locate related files.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern (e.g. src/**/*.rs)"},
                    "path": {"type": "string", "description": "Optional: only search under this directory"},
                    "limit": {"type": "integer", "description": "Optional: maximum results (default 100, max 1000)"}
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: "list_dir",
            description: "List one directory's entries, with a trailing slash for directories. Use it to see what exists before reading.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory relative to the repository root (default the root)"},
                    "limit": {"type": "integer", "description": "Optional: maximum entries (default 200, max 1000)"}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "show_base_file",
            description: "Read the file as it exists on the base branch (the PR target branch). Use it to compare pre-change behavior.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the repository root"}
                },
                "required": ["path"]
            }),
        },
    ];
    if profile == crate::agent::ToolProfile::ReadWrite {
        tools.push(ToolSpec {
            name: "edit_file",
            description: "Edit one file with exact text replacements, several disjoint edits in one call. Every old_string is matched against the original file (not incrementally) and must occur exactly once; overlapping edits are refused.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the repository root"},
                    "edits": {
                        "type": "array",
                        "description": "One or more replacements. Merge edits that touch the same block into one entry.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string", "description": "Exact text to replace (must occur exactly once)"},
                                "new_string": {"type": "string", "description": "Replacement text"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        });
        tools.push(ToolSpec {
            name: "write_file",
            description: "Create a new file, or rewrite an existing file this run has already read. Prefer edit_file for targeted changes.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the repository root"},
                    "content": {"type": "string", "description": "Full file content"}
                },
                "required": ["path", "content"]
            }),
        });
    }
    tools
}

/// Names of the read-only tools: the only menu a summarization run may have.
pub fn readonly_specs() -> Vec<ToolSpec> {
    specs(crate::agent::ToolProfile::ReadOnly)
}

fn arg_str(arguments: &serde_json::Value, key: &str) -> Result<String, String> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing required string argument {key:?}"))
}

fn arg_opt_u64(arguments: &serde_json::Value, key: &str) -> Option<u64> {
    arguments.get(key).and_then(serde_json::Value::as_u64)
}

fn arg_opt_str(arguments: &serde_json::Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn arg_opt_bool(arguments: &serde_json::Value, key: &str) -> bool {
    arguments
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Argument contract per tool: name and JSON type.
///
/// A tool call with an unknown name or a wrong type is a mistake the model can
/// fix only if it hears about it. Silently ignoring `start_line: "10"` would
/// return a whole-file read while the model believes it read a range.
type ArgumentSpec = &'static [(&'static str, &'static str)];

fn argument_spec(tool: &str) -> Option<ArgumentSpec> {
    Some(match tool {
        "read_file" => &[
            ("path", "string"),
            ("start_line", "integer"),
            ("end_line", "integer"),
        ],
        "grep" => &[
            ("pattern", "string"),
            ("path", "string"),
            ("glob", "string"),
            ("ignore_case", "boolean"),
            ("literal", "boolean"),
            ("context_lines", "integer"),
        ],
        "glob" => &[
            ("pattern", "string"),
            ("path", "string"),
            ("limit", "integer"),
        ],
        "list_dir" => &[("path", "string"), ("limit", "integer")],
        "show_base_file" => &[("path", "string")],
        "edit_file" => &[("path", "string"), ("edits", "array")],
        "write_file" => &[("path", "string"), ("content", "string")],
        _ => return None,
    })
}

fn type_matches(value: &serde_json::Value, kind: &str) -> bool {
    match kind {
        "string" => value.is_string(),
        "integer" => value.is_u64() || value.is_i64(),
        "boolean" => value.is_boolean(),
        "array" => value.is_array(),
        _ => true,
    }
}

/// Reject unknown arguments and wrong types before the call runs.
fn validate_arguments(tool: &str, arguments: &serde_json::Value) -> Result<(), String> {
    let spec = argument_spec(tool).unwrap_or(&[]);
    let Some(object) = arguments.as_object() else {
        return Err(format!(
            "invalid arguments for {tool}: expected a JSON object"
        ));
    };
    for key in object.keys() {
        if !spec.iter().any(|(name, _)| name == key) {
            let expected = spec
                .iter()
                .map(|(name, _)| *name)
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "invalid arguments for {tool}: unknown argument {key:?}; expected one of: {expected}"
            ));
        }
    }
    for (name, kind) in spec {
        if let Some(value) = object.get(*name)
            && !value.is_null()
            && !type_matches(value, kind)
        {
            return Err(format!(
                "invalid arguments for {tool}: {name:?} must be a {kind}"
            ));
        }
    }
    Ok(())
}

/// One `edits[]` entry of `edit_file`.
fn parse_edits(arguments: &serde_json::Value) -> Result<Vec<(String, String)>, String> {
    let Some(entries) = arguments.get("edits").and_then(serde_json::Value::as_array) else {
        return Err(
            "edit_file error: \"edits\" must be an array of {old_string, new_string}".to_string(),
        );
    };
    let mut edits = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let old = entry.get("old_string").and_then(serde_json::Value::as_str);
        let new = entry.get("new_string").and_then(serde_json::Value::as_str);
        match (old, new) {
            (Some(old), Some(new)) => edits.push((old.to_string(), new.to_string())),
            _ => {
                return Err(format!(
                    "edit_file error: edits[{index}] needs string old_string and new_string"
                ));
            }
        }
    }
    Ok(edits)
}

/// Execute one tool call by name. Errors are returned as text, never as a
/// failure: a tool problem must not break the agentic loop (spec 04).
pub async fn dispatch(name: &str, arguments: &serde_json::Value, shared: &ToolShared) -> String {
    let empty = serde_json::Value::Object(serde_json::Map::new());
    let arguments = if arguments.is_null() {
        &empty
    } else {
        arguments
    };
    if !argument_spec(name).is_some() {
        return format!("unknown tool: {name}");
    }
    if let Err(message) = validate_arguments(name, arguments) {
        return message;
    }
    match name {
        "read_file" => match arg_str(arguments, "path") {
            Ok(path) => {
                read_file(
                    shared,
                    &path,
                    arg_opt_u64(arguments, "start_line"),
                    arg_opt_u64(arguments, "end_line"),
                )
                .await
            }
            Err(e) => e,
        },
        "grep" => match arg_str(arguments, "pattern") {
            Ok(pattern) => {
                let path = arg_opt_str(arguments, "path");
                let glob = arg_opt_str(arguments, "glob");
                grep(
                    shared,
                    &pattern,
                    path.as_deref(),
                    glob.as_deref(),
                    arg_opt_bool(arguments, "ignore_case"),
                    arg_opt_bool(arguments, "literal"),
                    arg_opt_u64(arguments, "context_lines").map(|v| v as u32),
                )
                .await
            }
            Err(e) => e,
        },
        "glob" => match arg_str(arguments, "pattern") {
            Ok(pattern) => {
                let path = arg_opt_str(arguments, "path");
                glob(
                    shared,
                    &pattern,
                    path.as_deref(),
                    arg_opt_u64(arguments, "limit").map(|v| v as u32),
                )
                .await
            }
            Err(e) => e,
        },
        "list_dir" => {
            let path = arg_opt_str(arguments, "path");
            list_dir(
                shared,
                path.as_deref(),
                arg_opt_u64(arguments, "limit").map(|v| v as u32),
            )
            .await
        }
        "show_base_file" => match arg_str(arguments, "path") {
            Ok(path) => show_base_file(shared, &path).await,
            Err(e) => e,
        },
        "edit_file" => match (arg_str(arguments, "path"), parse_edits(arguments)) {
            (Ok(path), Ok(edits)) => edit_file(shared, &path, &edits).await,
            (Err(e), _) | (_, Err(e)) => e,
        },
        "write_file" => match (arg_str(arguments, "path"), arg_str(arguments, "content")) {
            (Ok(path), Ok(content)) => write_file(shared, &path, &content).await,
            (Err(e), _) | (_, Err(e)) => e,
        },
        other => format!("unknown tool: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Arc<ToolShared>) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/util")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    helper();\n}\n").unwrap();
        std::fs::write(
            root.join("src/util/mod.rs"),
            "pub fn helper() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("README.md"), "# demo\n").unwrap();
        let shared = ToolShared::new(root.to_path_buf(), "main", 3);
        (dir, shared)
    }

    #[tokio::test]
    async fn read_file_with_line_numbers_and_range() {
        let (_d, s) = setup();
        let out = read_file(&s, "src/main.rs", None, None).await;
        assert!(out.contains("1│fn main() {"));
        assert!(out.contains("2│    helper();"));
        let ranged = read_file(&s, "src/main.rs", Some(2), Some(2)).await;
        assert!(ranged.contains("2│    helper();"));
        assert!(!ranged.contains("fn main"));
    }

    #[tokio::test]
    async fn sandbox_rejects_escape() {
        let (_d, s) = setup();
        assert!(
            read_file(&s, "../outside.rs", None, None)
                .await
                .contains("escape")
        );
        assert!(
            read_file(&s, "/etc/passwd", None, None)
                .await
                .contains("absolute paths")
        );
        assert!(
            read_file(&s, "src/../../x.rs", None, None)
                .await
                .contains("escape")
        );
    }

    #[tokio::test]
    async fn sandbox_rejects_symlink_escape() {
        let (_d, s) = setup();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::os::unix::fs::symlink(outside.path(), s.workspace().join("src/link.rs")).unwrap();
        let out = read_file(&s, "src/link.rs", None, None).await;
        assert!(out.contains("symlink escape"), "actual: {out}");
    }

    #[tokio::test]
    async fn grep_finds_callers() {
        let (_d, s) = setup();
        let out = grep(&s, "helper", None, None, false, false, None).await;
        assert!(out.contains("src/main.rs:2:"));
        assert!(out.contains("src/util/mod.rs:1:"));
        // with context
        let ctx = grep(
            &s,
            "helper",
            Some("src/main.rs"),
            None,
            false,
            false,
            Some(1),
        )
        .await;
        assert!(ctx.contains("src/main.rs:1:  fn main() {"));
        assert!(ctx.contains("src/main.rs:2:>     helper();"));
    }

    #[tokio::test]
    async fn glob_matches() {
        let (_d, s) = setup();
        let out = glob(&s, "**/*.rs", None, None).await;
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("src/util/mod.rs"));
        assert!(!out.contains("README.md"));
    }

    #[tokio::test]
    async fn edit_file_applies_several_disjoint_edits_in_one_call() {
        let (_d, s) = setup();
        let out = edit_file(
            &s,
            "src/main.rs",
            &[
                ("helper();".to_string(), "helper_v2();".to_string()),
                ("fn main".to_string(), "fn main_entry".to_string()),
            ],
        )
        .await;
        assert!(out.contains("2 replacement(s)"), "{out}");
        let after = std::fs::read_to_string(s.workspace().join("src/main.rs")).unwrap();
        assert!(after.contains("helper_v2()"));
        assert!(after.contains("fn main_entry"));
    }

    #[tokio::test]
    async fn a_rejected_multi_edit_leaves_the_file_untouched() {
        let (_d, s) = setup();
        let target = s.workspace().join("src/main.rs");
        let before = std::fs::read_to_string(&target).unwrap();
        // The second edit cannot be found: nothing is written at all.
        let out = edit_file(
            &s,
            "src/main.rs",
            &[
                ("helper();".to_string(), "helper_v2();".to_string()),
                ("not in this file".to_string(), "x".to_string()),
            ],
        )
        .await;
        assert!(out.contains("was not found"), "{out}");
        assert_eq!(before, std::fs::read_to_string(&target).unwrap());
        // Overlapping edits are refused the same way.
        let out = edit_file(
            &s,
            "src/main.rs",
            &[
                ("fn main()".to_string(), "fn entry()".to_string()),
                ("main() {".to_string(), "start() {".to_string()),
            ],
        )
        .await;
        assert!(out.contains("overlap"), "{out}");
        assert_eq!(before, std::fs::read_to_string(&target).unwrap());
    }

    #[tokio::test]
    async fn write_file_refuses_to_clobber_an_unread_file() {
        let (_d, s) = setup();
        let target = s.workspace().join("README.md");
        let before = std::fs::read_to_string(&target).unwrap();
        let refused = write_file(&s, "README.md", "// rewritten\n").await;
        assert!(refused.contains("has not read it"), "{refused}");
        assert_eq!(before, std::fs::read_to_string(&target).unwrap());
        // Reading it makes the same write legal, and a new file never needs one.
        let _ = read_file(&s, "README.md", None, None).await;
        let written = write_file(&s, "README.md", "// rewritten\n").await;
        assert!(written.contains("wrote"), "{written}");
        let created = write_file(&s, "src/brand-new.rs", "// new\n").await;
        assert!(created.contains("wrote"), "{created}");
    }

    #[tokio::test]
    async fn grep_filters_by_glob_case_and_literal() {
        let (_d, s) = setup();
        std::fs::write(s.workspace().join("notes.md"), "helper() in markdown\n").unwrap();
        let scoped = grep(&s, "helper", None, Some("**/*.md"), false, false, None).await;
        assert!(scoped.contains("notes.md"), "{scoped}");
        assert!(!scoped.contains("main.rs"), "{scoped}");
        let insensitive = grep(&s, "HELPER", None, None, true, false, None).await;
        assert!(insensitive.contains("main.rs"), "{insensitive}");
        let literal = grep(&s, "helper()", None, None, false, true, None).await;
        assert!(literal.contains("main.rs"), "{literal}");
        // An unclosed paren is an invalid regex; literal mode must still work.
        let literal_paren = grep(&s, "(unclosed", None, None, false, true, None).await;
        assert!(literal_paren.contains("no matches"), "{literal_paren}");
        let invalid_regex = grep(&s, "(unclosed", None, None, false, false, None).await;
        assert!(invalid_regex.contains("invalid regex"), "{invalid_regex}");
    }

    #[tokio::test]
    async fn glob_scopes_to_a_directory_and_honours_a_limit() {
        let (_d, s) = setup();
        let all = glob(&s, "**/*.rs", None, None).await;
        assert!(all.contains("src/main.rs"), "{all}");
        let scoped = glob(&s, "**/*.txt", Some("src"), None).await;
        assert!(scoped.contains("no matches"), "{scoped}");
        let limited = glob(&s, "**/*.rs", None, Some(1)).await;
        assert!(limited.contains("truncated"), "{limited}");
    }

    #[tokio::test]
    async fn list_dir_names_directories_and_bounds_entries() {
        let (_d, s) = setup();
        let listing = list_dir(&s, None, None).await;
        assert!(listing.contains("src/"), "{listing}");
        assert!(listing.contains("README.md"), "{listing}");
        let single = list_dir(&s, Some("src"), Some(1)).await;
        assert!(!single.is_empty());
        let outside = list_dir(&s, Some("../"), None).await;
        assert!(outside.contains("escape"), "{outside}");
    }

    #[tokio::test]
    async fn dispatch_rejects_unknown_and_mistyped_arguments() {
        let (_d, s) = setup();
        let unknown = dispatch(
            "read_file",
            &serde_json::json!({"path": "src/main.rs", "start": 1}),
            &s,
        )
        .await;
        assert!(unknown.contains("unknown argument"), "{unknown}");
        // A string where a line number belongs is reported, not silently ignored.
        let mistyped = dispatch(
            "read_file",
            &serde_json::json!({"path": "src/main.rs", "start_line": "10"}),
            &s,
        )
        .await;
        assert!(mistyped.contains("must be a integer"), "{mistyped}");
        let not_object = dispatch("grep", &serde_json::json!(["helper"]), &s).await;
        assert!(
            not_object.contains("expected a JSON object"),
            "{not_object}"
        );
        let unknown_tool = dispatch("rm_rf", &serde_json::json!({}), &s).await;
        assert!(unknown_tool.contains("unknown tool"), "{unknown_tool}");
        // A well-formed call still runs.
        let ok = dispatch("glob", &serde_json::json!({"pattern": "**/*.rs"}), &s).await;
        assert!(ok.contains("src/main.rs"), "{ok}");
    }

    #[tokio::test]
    async fn dispatch_edits_through_the_array_form() {
        let (_d, s) = setup();
        let out = dispatch(
            "edit_file",
            &serde_json::json!({
                "path": "src/main.rs",
                "edits": [{"old_string": "helper();", "new_string": "helper_three();"}]
            }),
            &s,
        )
        .await;
        assert!(out.contains("edited"), "{out}");
        let content = std::fs::read_to_string(s.workspace().join("src/main.rs")).unwrap();
        assert!(content.contains("helper_three()"));
    }

    #[tokio::test]
    async fn budget_gate() {
        let (_d, s) = setup(); // max_calls = 3
        for _ in 0..3 {
            let out = s
                .run(
                    "read_file",
                    "x".into(),
                    read_file(&s, "src/main.rs", None, None),
                )
                .await;
            assert!(!out.contains("budget exhausted"));
        }
        let out = s
            .run(
                "read_file",
                "x".into(),
                read_file(&s, "src/main.rs", None, None),
            )
            .await;
        assert!(out.contains("budget exhausted"));
        assert_eq!(s.trace().len(), 3); // over-budget calls are not recorded in the trace
        assert_eq!(s.trace()[0].name, "read_file");
    }

    #[tokio::test]
    async fn edit_file_unique_replace() {
        let (_d, s) = setup();
        let out = edit_file(
            &s,
            "src/main.rs",
            &[("helper();".to_string(), "helper_v2();".to_string())],
        )
        .await;
        assert!(out.contains("edited src/main.rs"), "{out}");
        let content = std::fs::read_to_string(s.workspace().join("src/main.rs")).unwrap();
        assert!(content.contains("helper_v2();"));
    }

    #[tokio::test]
    async fn edit_file_error_paths() {
        let (_d, s) = setup();
        // not found
        let out = edit_file(
            &s,
            "src/main.rs",
            &[("nonexistent_call();".to_string(), "x();".to_string())],
        )
        .await;
        assert!(out.contains("not found"), "{out}");
        // not unique
        std::fs::write(s.workspace().join("dup.txt"), "a\na\nb\n").unwrap();
        let out = edit_file(&s, "dup.txt", &[("a".to_string(), "c".to_string())]).await;
        assert!(out.contains("occurs more than once"), "{out}");
        // missing file
        let out = edit_file(&s, "nope.txt", &[("a".to_string(), "b".to_string())]).await;
        assert!(out.contains("cannot stat"), "{out}");
        // empty / identical
        assert!(
            edit_file(&s, "dup.txt", &[("".to_string(), "b".to_string())])
                .await
                .contains("must not be empty")
        );
        assert!(
            edit_file(&s, "dup.txt", &[("a".to_string(), "a".to_string())])
                .await
                .contains("replaces text with itself")
        );
        // non-UTF-8
        std::fs::write(s.workspace().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(
            edit_file(&s, "bin.dat", &[("a".to_string(), "b".to_string())])
                .await
                .contains("not readable as UTF-8")
        );
        // sandbox
        assert!(
            edit_file(&s, "../evil.txt", &[("a".to_string(), "b".to_string())])
                .await
                .contains("escape")
        );
    }

    #[tokio::test]
    async fn write_file_create_overwrite_mkdir() {
        let (_d, s) = setup();
        // create with parent dirs
        let out = write_file(&s, "new/deep/file.txt", "hello\n").await;
        assert!(out.contains("wrote new/deep/file.txt"), "{out}");
        assert_eq!(
            std::fs::read_to_string(s.workspace().join("new/deep/file.txt")).unwrap(),
            "hello\n"
        );
        // overwrite
        // Overwriting needs a prior read of the working copy.
        let refused = write_file(&s, "README.md", "# replaced\n").await;
        assert!(refused.contains("has not read it"), "{refused}");
        let _ = read_file(&s, "README.md", None, None).await;
        let out = write_file(&s, "README.md", "# replaced\n").await;
        assert!(out.contains("wrote README.md"), "{out}");
        assert_eq!(
            std::fs::read_to_string(s.workspace().join("README.md")).unwrap(),
            "# replaced\n"
        );
        // sandbox
        assert!(write_file(&s, "../evil.txt", "x").await.contains("escape"));
        assert!(
            write_file(&s, "/abs/path.txt", "x")
                .await
                .contains("absolute")
        );
    }

    #[tokio::test]
    async fn write_file_blocks_symlinked_parent_escape() {
        let (_d, s) = setup();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), s.workspace().join("linkeddir")).unwrap();
        let out = write_file(&s, "linkeddir/pwned.txt", "x").await;
        assert!(out.contains("symlink escape"), "{out}");
        assert!(!outside.path().join("pwned.txt").exists());
    }

    #[tokio::test]
    async fn show_base_file_fallback_message() {
        let (_d, s) = setup(); // not a git repository
        let out = show_base_file(&s, "src/main.rs").await;
        assert!(out.contains("cannot read base version"));
    }
}
