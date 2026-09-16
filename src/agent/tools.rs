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

/// Shared tool state: sandbox root, base ref, budget counter, call trace
pub struct ToolShared {
    workspace: PathBuf, // canonicalized
    base_ref: String,
    max_calls: u32,
    calls: AtomicU32,
    trace: Mutex<Vec<ToolCallRecord>>,
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
        })
    }

    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn base_ref(&self) -> &str {
        &self.base_ref
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

/// Truncate output to MAX_OUTPUT_BYTES
fn cap_output(mut s: String) -> String {
    if s.len() > MAX_OUTPUT_BYTES {
        s.truncate(MAX_OUTPUT_BYTES);
        s.push_str("\n... [output truncated]\n");
    }
    s
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
    context_lines: Option<u32>,
) -> String {
    let re = match regex::Regex::new(pattern) {
        Ok(r) => r,
        Err(e) => return format!("invalid regex: {e}"),
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

/// glob: find files by glob pattern, ≤100 results
pub async fn glob(shared: &ToolShared, pattern: &str) -> String {
    let matcher = match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher(),
        Err(e) => return format!("invalid glob pattern: {e}"),
    };
    let mut hits: Vec<String> = shared
        .walk_files()
        .into_iter()
        .filter(|f| matcher.is_match(f))
        .map(|f| f.to_string_lossy().replace('\\', "/"))
        .collect();
    hits.sort();
    let total = hits.len();
    hits.truncate(MAX_GLOB_RESULTS);
    if hits.is_empty() {
        return format!("no matches: {pattern}");
    }
    let mut out = hits.join("\n");
    if total > MAX_GLOB_RESULTS {
        out.push_str(&format!("\n... [truncated: {total} matches total]"));
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
pub async fn edit_file(
    shared: &ToolShared,
    path: &str,
    old_string: &str,
    new_string: &str,
) -> String {
    if old_string.is_empty() {
        return "edit_file error: old_string must not be empty".to_string();
    }
    if old_string == new_string {
        return "edit_file error: old_string and new_string are identical".to_string();
    }
    let p = match shared.resolve_path_for_write(path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let meta = match std::fs::metadata(&p) {
        Ok(m) => m,
        Err(_) => {
            return format!(
                "edit_file error: file does not exist: {path} (use write_file to create it)"
            );
        }
    };
    if meta.is_dir() {
        return format!("edit_file error: {path} is a directory");
    }
    if meta.len() > MAX_EDIT_FILE_BYTES {
        return format!(
            "edit_file error: file too large ({} bytes), refusing to edit",
            meta.len()
        );
    }
    let bytes = match std::fs::read(&p) {
        Ok(b) => b,
        Err(e) => return format!("edit_file error: failed to read {path}: {e}"),
    };
    let text = match String::from_utf8(bytes) {
        Ok(t) => t,
        Err(_) => return format!("edit_file error: {path} is not a UTF-8 text file"),
    };
    let count = text.matches(old_string).count();
    if count == 0 {
        return format!("edit_file error: old_string not found in {path}");
    }
    if count > 1 {
        return format!(
            "edit_file error: old_string matches {count} locations in {path}; it must be unique — include more surrounding context"
        );
    }
    let updated = text.replacen(old_string, new_string, 1);
    match std::fs::write(&p, updated.as_bytes()) {
        Ok(()) => format!("edited {path} ({} -> {} bytes)", meta.len(), updated.len()),
        Err(e) => format!("edit_file error: failed to write {path}: {e}"),
    }
}

/// write_file: create or overwrite a whole file (spec 11 §4).
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
            description: "Search the repository with a regular expression. Use it to find call sites of a function or type.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression"},
                    "path": {"type": "string", "description": "Optional: limit to a file or directory"},
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
                    "pattern": {"type": "string", "description": "Glob pattern (e.g. src/**/*.rs)"}
                },
                "required": ["pattern"]
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
            description: "Replace exact text in an existing file. old_string must match exactly once — include enough surrounding context to make it unique.",
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path relative to the repository root"},
                    "old_string": {"type": "string", "description": "Exact text to replace (must occur exactly once in the file)"},
                    "new_string": {"type": "string", "description": "Replacement text"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        });
        tools.push(ToolSpec {
            name: "write_file",
            description: "Create a new file or overwrite an existing file with the given content. Prefer edit_file for targeted changes to existing files.",
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

/// Execute one tool call by name. Errors are returned as text, never as a
/// failure: a tool problem must not break the agentic loop (spec 04).
pub async fn dispatch(name: &str, arguments: &serde_json::Value, shared: &ToolShared) -> String {
    let arguments = if arguments.is_null() {
        &serde_json::Value::Object(serde_json::Map::new())
    } else {
        arguments
    };
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
                grep(
                    shared,
                    &pattern,
                    path.as_deref(),
                    arg_opt_u64(arguments, "context_lines").map(|v| v as u32),
                )
                .await
            }
            Err(e) => e,
        },
        "glob" => match arg_str(arguments, "pattern") {
            Ok(pattern) => glob(shared, &pattern).await,
            Err(e) => e,
        },
        "show_base_file" => match arg_str(arguments, "path") {
            Ok(path) => show_base_file(shared, &path).await,
            Err(e) => e,
        },
        "edit_file" => match (
            arg_str(arguments, "path"),
            arg_str(arguments, "old_string"),
            arg_str(arguments, "new_string"),
        ) {
            (Ok(path), Ok(old), Ok(new)) => edit_file(shared, &path, &old, &new).await,
            (Err(e), ..) | (_, Err(e), _) | (_, _, Err(e)) => e,
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
        let out = grep(&s, "helper", None, None).await;
        assert!(out.contains("src/main.rs:2:"));
        assert!(out.contains("src/util/mod.rs:1:"));
        // with context
        let ctx = grep(&s, "helper", Some("src/main.rs"), Some(1)).await;
        assert!(ctx.contains("src/main.rs:1:  fn main() {"));
        assert!(ctx.contains("src/main.rs:2:>     helper();"));
    }

    #[tokio::test]
    async fn glob_matches() {
        let (_d, s) = setup();
        let out = glob(&s, "**/*.rs").await;
        assert!(out.contains("src/main.rs"));
        assert!(out.contains("src/util/mod.rs"));
        assert!(!out.contains("README.md"));
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
        let out = edit_file(&s, "src/main.rs", "helper();", "helper_v2();").await;
        assert!(out.contains("edited src/main.rs"), "{out}");
        let content = std::fs::read_to_string(s.workspace().join("src/main.rs")).unwrap();
        assert!(content.contains("helper_v2();"));
    }

    #[tokio::test]
    async fn edit_file_error_paths() {
        let (_d, s) = setup();
        // not found
        let out = edit_file(&s, "src/main.rs", "nonexistent_call();", "x();").await;
        assert!(out.contains("not found"), "{out}");
        // not unique
        std::fs::write(s.workspace().join("dup.txt"), "a\na\nb\n").unwrap();
        let out = edit_file(&s, "dup.txt", "a", "c").await;
        assert!(out.contains("matches 2 locations"), "{out}");
        // missing file
        let out = edit_file(&s, "nope.txt", "a", "b").await;
        assert!(out.contains("does not exist"), "{out}");
        // empty / identical
        assert!(
            edit_file(&s, "dup.txt", "", "b")
                .await
                .contains("must not be empty")
        );
        assert!(
            edit_file(&s, "dup.txt", "a", "a")
                .await
                .contains("identical")
        );
        // non-UTF-8
        std::fs::write(s.workspace().join("bin.dat"), [0xff, 0xfe, 0x00]).unwrap();
        assert!(
            edit_file(&s, "bin.dat", "a", "b")
                .await
                .contains("not a UTF-8")
        );
        // sandbox
        assert!(
            edit_file(&s, "../evil.txt", "a", "b")
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
