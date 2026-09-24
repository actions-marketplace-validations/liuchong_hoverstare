//! Local diff review tool: runs the full analysis + rendering on a local diff
//! file without depending on GitHub.
//!
//! Usage:
//!   cargo run --example local_review -- tests/fixtures/buggy.diff [base_ref]
//!
//! base_ref defaults to main (the reference branch of the show_base_file tool).
//! Use GITHUB_WORKSPACE to set the tool sandbox root (defaults to current dir).
//! Used for debugging and acceptance (booby-trapped diffs -> verify inline
//! anchoring; caller scenarios -> verify grep-based checking).

use std::path::PathBuf;

use hoverstare::agent::tools::ToolShared;
use hoverstare::{config, diff, orchestrator, prompt, report};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hoverstare=info".into()),
        )
        .with_target(false)
        .without_time()
        .init();

    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .expect("usage: local_review <diff file> [base_ref]");
    let base_ref = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "main".to_string());

    let cfg = config::Config::load()?;
    let diff_text = std::fs::read_to_string(&path)?;
    let (filtered, excluded) = diff::filter_text(&diff_text, &cfg.ignore);
    let truncated = diff::truncate_text(&filtered, cfg.max_diff_kb);
    let parsed = diff::ParsedDiff::parse(&truncated.text);
    anyhow::ensure!(!parsed.files.is_empty(), "no reviewable changes in diff");
    tracing::info!(
        "diff: {} files ({} filtered, {} truncated)",
        parsed.files.len(),
        excluded,
        truncated.truncated_files.len()
    );

    let shared = ToolShared::new(cfg.workspace.clone(), &base_ref, cfg.max_tool_calls);
    let mode = prompt::ReviewMode::default();
    let instructions =
        hoverstare::instructions::RepoInstructions::load(&cfg.workspace, &base_ref).await;
    let (analysis, usage) = orchestrator::analyze(
        &cfg,
        &parsed,
        &truncated.text,
        &truncated.truncated_files,
        &shared,
        &mode,
        &instructions,
    )
    .await?;
    tracing::info!("model reported {} findings", analysis.findings.len());
    tracing::info!(
        "usage: {} call(s), {} input / {} output tokens ({} cached)",
        usage.calls,
        usage.input_tokens,
        usage.output_tokens,
        usage.cached_input_tokens
    );

    // Tool trace (spec 04: basis for debugging and replay tests)
    let trace = shared.trace();
    for (i, t) in trace.iter().enumerate() {
        println!(
            "[tool #{:02}] {:<14} {:<60} {:?} {}B",
            i + 1,
            t.name,
            t.args_summary,
            t.duration,
            t.result_bytes
        );
    }
    if trace.is_empty() {
        println!("[tool] no tools were called in this analysis");
    }

    let ctx = report::ReviewContext {
        repo_full_name: "local/demo",
        head_sha: "0000000",
        meta_mode: "full",
        scope_label: "Full review",
        files_reviewed: parsed.files.len(),
        excluded_files: excluded,
        summary: &analysis.summary,
        coverage: None,
    };
    let review =
        report::build_review(&analysis.findings, &parsed, &cfg, &ctx, &Default::default()).review;

    let out = serde_json::json!({
        "body": review.body,
        "comments": review.comments.iter().map(|c| serde_json::json!({
            "path": c.path, "line": c.line, "body": c.body,
        })).collect::<Vec<_>>(),
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
