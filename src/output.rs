//! Structured output contract (spec 16)
//!
//! One shape for the machine-readable result, shared by every form (action,
//! serve, CLI, and whatever comes next) so downstream consumers — CI, security
//! dashboards, editors, other agents — do not have to learn a new one per form.
//!
//! The contract is deliberately small and closed: enumerated values are
//! exhaustive, ordering is fixed here (not per renderer), and nothing that could
//! carry a credential, a raw prompt or an absolute path is ever emitted.

use std::fmt::Write as _;
use std::path::Path;

use crate::agent::UsageTotal;
use crate::config::Severity;
use crate::units::{CoverageLedger, Selection, TerminalState};

/// Schema version of the JSON document (spec 16 §6). Adding optional fields is a
/// minor change; removing fields or changing their meaning is a major one.
pub const SCHEMA_VERSION: &str = "1.0";

/// Output format selector (spec 16 §1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Sarif,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Human => "human",
            OutputFormat::Json => "json",
            OutputFormat::Sarif => "sarif",
        }
    }

    /// Whether the format produces a machine-readable document (and therefore
    /// claims stdout, leaving logs on stderr).
    pub fn is_structured(self) -> bool {
        !matches!(self, OutputFormat::Human)
    }
}

/// How this run reached us. Recorded in the document so a consumer can tell an
/// Actions run from a self-hosted service run without guessing from the shape.
pub fn form() -> &'static str {
    if let Some(explicit) = std::env::var("HOVERSTARE_FORM")
        .ok()
        .filter(|v| !v.trim().is_empty())
    {
        // Leaked: the value is a small fixed identifier, not a secret.
        return match explicit.as_str() {
            "serve" => "serve",
            "cli" => "cli",
            _ => "action",
        };
    }
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        "action"
    } else {
        "cli"
    }
}

/// The revision this binary was built from, as exported by the workflow
/// (`HOVERSTARE_BUILT_FROM`, spec 08's flow pin). Locally nothing exports it, and
/// the package version is more honest than an invented sha.
pub fn revision_from_env() -> String {
    std::env::var("HOVERSTARE_BUILT_FROM")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .map(|v| v.chars().take(12).collect())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string())
}

/// The preview document (spec 14 §3): shape-aligned with the `units` segment
/// above, so a consumer reads the same fields before and after a run.
pub fn preview_json(selection: &Selection, incremental: bool) -> serde_json::Value {
    serde_json::json!({
        "mode": if incremental { "incremental" } else { "full" },
        "estimated_tokens": selection.estimated_tokens,
        "units": selection
            .units
            .iter()
            .map(|u| serde_json::json!({
                "unit_id": u.unit_id,
                "files": u.files,
                "status": "pending",
            }))
            .collect::<Vec<_>>(),
        "excluded": selection
            .excluded
            .iter()
            .map(|e| serde_json::json!({
                "path": normalize_path(&e.path),
                "reason": e.reason.as_str(),
                "kept_in_text": e.kept_in_text,
            }))
            .collect::<Vec<_>>(),
        "truncated": selection.oversized_dropped(),
    })
}

/// Lifecycle of a finding across runs (spec 16 §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingStatus {
    New,
    CarriedOver,
}

impl FindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingStatus::New => "new",
            FindingStatus::CarriedOver => "carried_over",
        }
    }
}

/// A related location (spec 16 §2 `related[]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelatedLocation {
    pub path: String,
    pub line: u64,
}

/// One finding as the contract exposes it.
///
/// `line`/`end_line` are `None` for findings that could not be anchored (spec 06's
/// fallback chain end): those become file-level results in SARIF and are the
/// reason the field is optional rather than defaulted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindingView {
    pub fingerprint: String,
    pub path: String,
    pub line: Option<u64>,
    pub end_line: Option<u64>,
    pub severity: Severity,
    pub title: String,
    pub body: String,
    pub suggestion: Option<String>,
    pub status: FindingStatus,
    pub related: Vec<RelatedLocation>,
}

/// Run-level metadata (spec 16 §2 `run`).
#[derive(Debug, Clone)]
pub struct RunMeta {
    pub repository: String,
    pub change_request: Option<u64>,
    /// The revision this binary was built from (spec 08's pin), not the PR head.
    pub revision: String,
    pub model: String,
    pub usage: UsageTotal,
    pub duration_ms: u128,
    pub terminal: TerminalState,
}

/// Everything one structured document needs.
pub struct RunReport<'a> {
    pub meta: &'a RunMeta,
    pub findings: &'a [FindingView],
    pub ledger: &'a CoverageLedger,
    pub resolutions: &'a [String],
}

/// The JSON document (spec 16 §2).
pub fn json(report: &RunReport<'_>) -> serde_json::Value {
    let summary = report.ledger.summary();
    serde_json::json!({
        "schema_version": SCHEMA_VERSION,
        "run": {
            "form": form(),
            "repository": report.meta.repository,
            "change_request": report.meta.change_request,
            "revision": report.meta.revision,
            "model": report.meta.model,
            "usage": {
                "input_tokens": report.meta.usage.input_tokens,
                "output_tokens": report.meta.usage.output_tokens,
                "cached_input_tokens": report.meta.usage.cached_input_tokens,
                "calls": report.meta.usage.calls,
            },
            "timing": { "duration_ms": report.meta.duration_ms },
            "terminal": report.meta.terminal.as_str(),
        },
        "units": units_json(report.ledger),
        "findings": findings_json(report.findings),
        "resolutions": sorted_fingerprints(report.resolutions),
        "coverage": {
            "total": summary.total,
            "covered": summary.covered,
            "failed": summary.failed,
            "truncated": summary.truncated,
        },
    })
}

/// The `units` segment, shared with the preview document (spec 14 §3).
pub fn units_json(ledger: &CoverageLedger) -> serde_json::Value {
    let summary = ledger.summary();
    let mut units: Vec<serde_json::Value> = ledger
        .denominator()
        .iter()
        .map(|unit_id| {
            let (status, reason) = match ledger.state(unit_id) {
                Some(crate::units::UnitState::Covered) => ("covered", None),
                Some(crate::units::UnitState::Failed(r)) => ("failed", Some(r.clone())),
                Some(crate::units::UnitState::Truncated(r)) => ("truncated", Some(r.clone())),
                // Selected but never dispatched (preview documents).
                _ => ("pending", None),
            };
            let mut entry = serde_json::json!({
                "unit_id": unit_id,
                "files": files_for_unit(unit_id),
                "status": status,
            });
            if let Some(reason) = reason {
                entry["reason"] = serde_json::Value::String(reason);
            }
            entry
        })
        .collect();
    units.sort_by(|a, b| a["unit_id"].as_str().cmp(&b["unit_id"].as_str()));
    let _ = summary;
    serde_json::Value::Array(units)
}

/// Files of a unit, taken from the `unit_id` (`changeset:{path}` or
/// `changeset:group:{primary}`): the contract never invents a file list, and a
/// grouped unit's members are not recoverable from the id alone, so the group
/// reports its primary path. `files` stays a list for forward compatibility.
fn files_for_unit(unit_id: &str) -> Vec<String> {
    match unit_id.split_once(':') {
        Some((_, rest)) => rest
            .strip_prefix("group:")
            .map(|primary| vec![primary.to_string()])
            .unwrap_or_else(|| vec![rest.to_string()]),
        None => vec![unit_id.to_string()],
    }
}

/// The `findings` segment, in the contract's fixed order (spec 16 §2).
pub fn findings_json(findings: &[FindingView]) -> serde_json::Value {
    let mut sorted: Vec<&FindingView> = findings.iter().collect();
    sorted.sort_by(|a, b| {
        (
            a.path.as_str(),
            a.line.unwrap_or(u64::MAX),
            a.fingerprint.as_str(),
        )
            .cmp(&(
                b.path.as_str(),
                b.line.unwrap_or(u64::MAX),
                b.fingerprint.as_str(),
            ))
    });
    serde_json::Value::Array(
        sorted
            .into_iter()
            .map(|f| {
                serde_json::json!({
                    "fingerprint": f.fingerprint,
                    "path": normalize_path(&f.path),
                    "line": f.line,
                    "end_line": f.end_line,
                    "side": "new",
                    "severity": f.severity.as_str(),
                    "title": f.title,
                    "body": f.body,
                    "suggestion": f.suggestion,
                    "status": f.status.as_str(),
                    "related": f.related.iter().map(|r| serde_json::json!({
                        "path": normalize_path(&r.path),
                        "line": r.line,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect(),
    )
}

/// Repository-relative, forward slashes, no leading `./` (spec 16 §2).
pub fn normalize_path(path: &str) -> String {
    let trimmed = path.trim().trim_start_matches("./").replace('\\', "/");
    trimmed.trim_start_matches('/').to_string()
}

pub fn sorted_fingerprints(fingerprints: &[String]) -> Vec<String> {
    let mut sorted = fingerprints.to_vec();
    sorted.sort();
    sorted
}

/// Pretty-print a document (stable key order comes from `serde_json`'s map).
pub fn to_pretty(document: &serde_json::Value) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(document)?)
}

/// Emit a document to stdout or to a file inside the workspace (spec 16 §5).
///
/// Relative paths are sandboxed to the workspace: a structured output path is not
/// a place to gain write access outside the checkout.
pub fn emit(document: &str, output: Option<&str>, workspace: &Path) -> anyhow::Result<()> {
    let Some(target) = output else {
        let mut stdout = String::with_capacity(document.len() + 1);
        let _ = writeln!(stdout, "{document}");
        print!("{stdout}");
        return Ok(());
    };

    let candidate = Path::new(target);
    let resolved = if candidate.is_absolute() {
        bail_out("--output must be a workspace-relative path")?
    } else {
        workspace.join(candidate)
    };
    let normalized = normalize_components(&resolved);
    let base = normalize_components(workspace);
    if !normalized.starts_with(&base) {
        bail_out("--output escapes the workspace")?;
    }
    if let Some(parent) = normalized.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut payload = document.to_string();
    payload.push('\n');
    std::fs::write(&normalized, payload)?;
    Ok(())
}

fn bail_out(message: &str) -> anyhow::Result<std::path::PathBuf> {
    Err(anyhow::anyhow!("{message}"))
}

/// Lexically normalize `..`/`.` without touching the filesystem.
fn normalize_components(path: &Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{ReviewUnit, UnitSource};

    fn unit(path: &str) -> ReviewUnit {
        ReviewUnit {
            unit_id: format!("changeset:{path}"),
            source: UnitSource::Changeset,
            files: vec![path.to_string()],
            unit_fp: "fp".to_string(),
        }
    }

    fn finding(path: &str, line: Option<u64>, fp: &str) -> FindingView {
        FindingView {
            fingerprint: fp.to_string(),
            path: path.to_string(),
            line,
            end_line: line,
            severity: Severity::High,
            title: "t".into(),
            body: "b".into(),
            suggestion: None,
            status: FindingStatus::New,
            related: vec![],
        }
    }

    fn meta() -> RunMeta {
        RunMeta {
            repository: "o/r".into(),
            change_request: Some(7),
            revision: "abc1234".into(),
            model: "m".into(),
            usage: UsageTotal {
                input_tokens: 10,
                output_tokens: 2,
                cached_input_tokens: 5,
                calls: 3,
            },
            duration_ms: 42,
            terminal: TerminalState::Ok,
        }
    }

    #[test]
    fn json_document_is_complete_and_closed() {
        let mut ledger = CoverageLedger::freeze(&[unit("src/a.rs"), unit("src/b.rs")]);
        ledger.cover_all();
        let findings = vec![
            finding("src/b.rs", Some(9), "ff"),
            finding("src/a.rs", None, "aa"),
        ];
        let resolutions = vec!["zz".to_string(), "aa".to_string()];
        let meta = meta();
        let doc = json(&RunReport {
            meta: &meta,
            findings: &findings,
            ledger: &ledger,
            resolutions: &resolutions,
        });

        assert_eq!(doc["schema_version"], SCHEMA_VERSION);
        assert_eq!(doc["run"]["repository"], "o/r");
        assert_eq!(doc["run"]["change_request"], 7);
        assert_eq!(doc["run"]["terminal"], "ok");
        assert_eq!(doc["run"]["usage"]["calls"], 3);
        assert_eq!(doc["run"]["timing"]["duration_ms"], 42);
        // Fixed ordering: path, then line (unanchored last), then fingerprint.
        assert_eq!(doc["findings"][0]["path"], "src/a.rs");
        assert_eq!(doc["findings"][1]["path"], "src/b.rs");
        assert_eq!(doc["findings"][0]["side"], "new");
        assert_eq!(doc["findings"][0]["line"], serde_json::Value::Null);
        assert_eq!(doc["findings"][1]["severity"], "high");
        assert_eq!(doc["resolutions"][0], "aa");
        assert_eq!(doc["coverage"]["covered"], 2);
        assert_eq!(doc["units"][0]["status"], "covered");
    }

    #[test]
    fn unanchored_findings_omit_the_line_instead_of_faking_one() {
        let ledger = CoverageLedger::freeze(&[unit("src/a.rs")]);
        let findings = vec![finding("src/a.rs", None, "aa")];
        let meta = meta();
        let doc = json(&RunReport {
            meta: &meta,
            findings: &findings,
            ledger: &ledger,
            resolutions: &[],
        });
        assert!(doc["findings"][0]["line"].is_null());
        assert!(doc["findings"][0]["end_line"].is_null());
    }

    #[test]
    fn paths_are_normalized_and_secrets_never_appear() {
        assert_eq!(normalize_path("./src/a.rs"), "src/a.rs");
        assert_eq!(normalize_path("/abs/src/a.rs"), "abs/src/a.rs");
        assert_eq!(normalize_path("src\\a.rs"), "src/a.rs");

        let ledger = CoverageLedger::freeze(&[unit("src/a.rs")]);
        let mut f = finding("src/a.rs", Some(1), "aa");
        f.suggestion = Some("let x = 1;".into());
        let findings = [f];
        let meta = meta();
        let doc = json(&RunReport {
            meta: &meta,
            findings: &findings,
            ledger: &ledger,
            resolutions: &[],
        });
        let text = serde_json::to_string(&doc).unwrap();
        assert!(!text.contains("OPENAI_API_KEY"));
        assert!(!text.contains("prompt"));
    }

    #[test]
    fn emit_rejects_absolute_and_escaping_output_paths() {
        let workspace = std::path::Path::new("/tmp/hoverstare-ws");
        assert!(emit("{}", Some("/etc/passwd"), workspace).is_err());
        assert!(emit("{}", Some("../outside.json"), workspace).is_err());
        // `out/../../escape.json` walks back out of the workspace lexically; the
        // check is on the normalized path, so the intermediate `out/` cannot hide it.
        assert!(emit("{}", Some("out/../../escape.json"), workspace).is_err());
    }

    #[test]
    fn emit_writes_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        emit("{\"a\":1}", Some("out/result.json"), dir.path()).unwrap();
        let written = std::fs::read_to_string(dir.path().join("out/result.json")).unwrap();
        assert_eq!(written, "{\"a\":1}\n");
    }

    #[test]
    fn grouped_unit_reports_its_primary_path() {
        assert_eq!(
            files_for_unit("changeset:group:README.md"),
            vec!["README.md".to_string()]
        );
        assert_eq!(
            files_for_unit("changeset:src/a.rs"),
            vec!["src/a.rs".to_string()]
        );
    }

    fn preview_selection() -> Selection {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +1,1 @@\n+x\n";
        crate::units::select(
            diff,
            &crate::units::SelectOptions::new(
                &globset::GlobSetBuilder::new().build().unwrap(),
                400,
            ),
        )
    }

    #[test]
    fn preview_document_is_pending_and_shape_aligned() {
        let selection = preview_selection();
        let doc = preview_json(&selection, false);
        assert_eq!(doc["mode"], "full");
        assert_eq!(doc["units"][0]["unit_id"], "changeset:src/a.rs");
        assert_eq!(doc["units"][0]["status"], "pending");
        assert_eq!(doc["truncated"].as_array().unwrap().len(), 0);
        assert!(doc["estimated_tokens"].as_u64().unwrap() > 0);
    }

    #[test]
    fn preview_document_reports_incremental_mode() {
        let doc = preview_json(&preview_selection(), true);
        assert_eq!(doc["mode"], "incremental");
    }
}
