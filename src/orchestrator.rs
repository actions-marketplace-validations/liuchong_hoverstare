//! Orchestration: end-to-end flow of the review command (spec 00 core data
//! flow, spec 07 incremental mode)

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::agent::UsageTotal;
use crate::agent::rig_backend::RigBackend;
use crate::agent::tools::ToolShared;
use crate::cli::ReviewArgs;
use crate::config::{Actor, Config, PermissionKey, Severity};
use crate::diff::ParsedDiff;
use crate::event;
use crate::findings::AnalysisResult;
use crate::github::{GitHubClient, NewStatus, Repo, StatusState};
use crate::i18n::T;
use crate::instructions::RepoInstructions;
use crate::output;
use crate::prompt::ReviewMode;
use crate::report::{self, ReviewContext};
use crate::state::{self, OpenFinding};
use crate::units;

#[derive(Debug)]
pub enum Outcome {
    Skipped(String),
    Published {
        inline_comments: usize,
        /// Coverage terminal state (spec 14 §4): ok | partial | empty.
        terminal: units::TerminalState,
    },
    /// `--preview`: the selection was reported and nothing was dispatched
    /// (spec 14 §3). No model call happened, so there is no coverage to report
    /// and no terminal state to derive.
    Previewed {
        units: usize,
        excluded: usize,
        truncated: usize,
    },
    DryRun,
    AnalysisFailed(String),
}

/// fail-open funnel: analysis-zone failure -> AnalysisFailed (exit 0);
/// fail_closed -> Err (exit 1)
fn fail_or_open(cfg: &Config, e: anyhow::Error) -> anyhow::Result<Outcome> {
    if cfg.fail_closed {
        Err(e)
    } else {
        Ok(Outcome::AnalysisFailed(format!("{e:#}")))
    }
}

/// Failure notes state the coverage as well (spec 14 §4): an uncovered unit must
/// be visible, never silent — "no comments posted" and "nothing was reviewed"
/// are different facts. Status descriptions are capped at 140 chars downstream.
fn failure_note(kind: &str, coverage: &units::CoverageLedger) -> String {
    let s = coverage.summary();
    format!(
        "{kind}: coverage {}/{} ({} failed, {} truncated)",
        s.covered, s.total, s.failed, s.truncated
    )
}

/// Every terminal state (including skip/failure paths) writes the `hoverstare`
/// status check (spec 07: otherwise a required check would never arrive and
/// merging would deadlock)
async fn post_terminal_status(
    gh: &GitHubClient,
    repo: &Repo,
    head_sha: &str,
    cfg: &Config,
    analysis_ok: bool,
    note: &str,
) {
    if !cfg.status_checks {
        return;
    }
    let state = if analysis_ok || !cfg.fail_closed {
        StatusState::Success
    } else {
        StatusState::Error
    };
    if let Err(e) = gh
        .create_status(
            repo,
            head_sha,
            &NewStatus {
                context: "hoverstare",
                state,
                description: note.chars().take(140).collect(),
            },
        )
        .await
    {
        tracing::warn!("failed to write status check hoverstare: {e}");
    }
}

/// Unified funnel for skip paths: write terminal checks before returning.
/// Skip = nothing to review -> findings are always empty, so the findings check
/// is also success (spec 07); otherwise a required check would stay pending
/// forever and deadlock merging.
async fn skip_outcome(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    head_sha: &str,
    reason: String,
) -> Outcome {
    post_terminal_status(
        gh,
        repo,
        head_sha,
        cfg,
        true,
        &T::new(cfg.language).status_skipped(&reason),
    )
    .await;
    if cfg.status_checks
        && let Err(e) = gh
            .create_status(
                repo,
                head_sha,
                &NewStatus {
                    context: "hoverstare-findings",
                    state: StatusState::Success,
                    description: T::new(cfg.language)
                        .status_nothing_to_review(&reason)
                        .chars()
                        .take(140)
                        .collect(),
                },
            )
            .await
    {
        tracing::warn!("failed to write status check hoverstare-findings: {e}");
    }
    Outcome::Skipped(reason)
}

/// GitHub Actions log-group markers.
///
/// They go to **stderr**: they are log annotations, and stdout is the product
/// channel (`--format json|sarif`, `--preview`). Writing them to stdout made a
/// structured document unparseable in CI while passing locally — the flag is only
/// set there, which is why the stdout-purity test now forces it on.
fn gha_group(name: &str) {
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        eprintln!("::group::{name}");
    }
}

fn gha_group_end() {
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        eprintln!("::endgroup::");
    }
}

/// Everything the preparation phase resolved: the change set, the selection and
/// the coverage ledger it froze.
///
/// The preview and the real run consume this very value, so "what would be
/// reviewed" and "what is reviewed" cannot drift apart (spec 14 §2).
pub struct ReviewInputs {
    pub repo: Repo,
    pub gh: GitHubClient,
    pub resolve_gh: GitHubClient,
    pub pr_ref: event::PrRef,
    pub pr: crate::github::PullRequest,
    pub head_sha: String,
    pub prior_sha: Option<String>,
    pub incremental: bool,
    /// Full-diff parse used for anchoring (spec 07).
    pub anchor_parsed: ParsedDiff,
    pub analysis_text: String,
    pub truncated_files: Vec<String>,
    pub excluded_files: usize,
    pub analysis_selection: units::Selection,
    pub coverage: units::CoverageLedger,
}

/// Result of preparation: ready to dispatch, or already terminal (skip /
/// fail-open) so nothing further should happen in this run.
pub enum Prep {
    Ready(Box<ReviewInputs>),
    Done(Outcome),
}

/// Resolve the event, the change set, the selection and the coverage ledger.
///
/// Shared by the real run and the preview: this is the structural half of the
/// "one decision, two consumers" rule (spec 14 §2). It touches GitHub but never
/// the model, which is why a preview needs no model credentials.
async fn prepare_inputs(cfg: &Config, args: &ReviewArgs, force_full: bool) -> anyhow::Result<Prep> {
    let pr_ref = event::resolve_pr(args)?;
    let repo = Repo::parse(&pr_ref.repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let gh = GitHubClient::new(cfg.github_token.clone())?;
    // resolveReviewThread with the narrow-duty PAT when present (spec 07);
    // otherwise the identity client (App token already works for resolve).
    let resolve_gh = match &cfg.gh_pat {
        Some(pat) => GitHubClient::new(Some(pat.clone()))?,
        None => gh.clone(),
    };

    tracing::info!("target PR: {} #{}", repo.full_name(), pr_ref.number);
    gha_group("preparation (PR / diff / incremental check)");
    // GitHub I/O failures (network/rate limit/permissions) are in the fail-open zone (spec 01)
    let pr = match gh.get_pull_request(&repo, pr_ref.number).await {
        Ok(p) => p,
        Err(e) => {
            return Ok(Prep::Done(fail_or_open(
                cfg,
                anyhow::anyhow!("failed to fetch PR: {e}"),
            )?));
        }
    };

    // Skip conditions (spec 01)
    let head_sha = pr.head.sha.clone();
    if pr.draft && !cfg.review_drafts {
        return Ok(Prep::Done(
            skip_outcome(cfg, &gh, &repo, &head_sha, "draft PR".into()).await,
        ));
    }
    if pr.user.login.ends_with("[bot]") {
        return Ok(Prep::Done(
            skip_outcome(
                cfg,
                &gh,
                &repo,
                &head_sha,
                format!("bot author: {}", pr.user.login),
            )
            .await,
        ));
    }

    // Auto-review permission gate (spec 12): actor is the PR author.
    // The `review` subcommand (force_full=false) follows the pull_request event path;
    // the mention `review` command (force_full=true) already checked the `review` key.
    if !force_full {
        let evaluator = cfg.permissions_evaluator();
        let actor = Actor {
            login: &pr.user.login,
            author_association: pr.author_association.as_deref().unwrap_or_default(),
        };
        if !evaluator
            .evaluate(PermissionKey::AutoReview, &gh, &repo, actor)
            .await
        {
            return Ok(Prep::Done(
                skip_outcome(
                    cfg,
                    &gh,
                    &repo,
                    &head_sha,
                    "auto-review permission denied".into(),
                )
                .await,
            ));
        }
    }

    // Incremental-mode check (spec 07): find the most recent review containing hoverstare-meta
    let prior_sha = match gh.list_reviews(&repo, pr_ref.number).await {
        Ok(reviews) => reviews
            .iter()
            .rev()
            .find_map(|r| state::parse_meta_head_sha(&r.body)),
        Err(e) => {
            tracing::warn!("failed to fetch historical reviews (treating as full review): {e}");
            None
        }
    };
    let incremental = !force_full && prior_sha.as_deref().is_some_and(|s| s != pr.head.sha);

    // Full diff (for anchoring + the analysis scope of full mode)
    let full_diff = match gh.get_pull_request_diff(&repo, pr_ref.number).await {
        Ok(d) => d,
        Err(e) => {
            return Ok(Prep::Done(fail_or_open(
                cfg,
                anyhow::anyhow!("failed to fetch diff: {e}"),
            )?));
        }
    };
    if full_diff.trim().is_empty() {
        return Ok(Prep::Done(
            skip_outcome(cfg, &gh, &repo, &head_sha, "empty diff".into()).await,
        ));
    }
    // Selection runs once, through the one implementation (spec 14 §2). The
    // anchoring pass needs the same decision with the size budget applied, so it
    // reads the same selection's text instead of deriving its own.
    let full_selection = units::select(
        &full_diff,
        &units::SelectOptions::new(&cfg.ignore, cfg.max_diff_kb)
            .strict(cfg.select_strict)
            .group_units(cfg.group_units),
    );
    let anchor_parsed = ParsedDiff::parse(&full_selection.text);

    // Analysis scope (spec 07: incremental = delta diff of prior..head)
    let analysis_selection = if incremental {
        let prior = prior_sha.as_deref().unwrap_or_default();
        let delta = match gh.get_compare_diff(&repo, prior, &pr.head.sha).await {
            Ok(d) => d,
            Err(e) => {
                return Ok(Prep::Done(fail_or_open(
                    cfg,
                    anyhow::anyhow!("failed to fetch incremental diff: {e}"),
                )?));
            }
        };
        if delta.trim().is_empty() {
            return Ok(Prep::Done(
                skip_outcome(
                    cfg,
                    &gh,
                    &repo,
                    &head_sha,
                    "no new changes since the last review".into(),
                )
                .await,
            ));
        }
        units::select(
            &delta,
            &units::SelectOptions::new(&cfg.ignore, cfg.max_diff_kb)
                .strict(cfg.select_strict)
                .group_units(cfg.group_units),
        )
    } else {
        full_selection.clone()
    };

    let analysis_text = analysis_selection.text.clone();
    // The one exclusion list carries two histories: the path gates (reported to
    // the model as "filtered out by rules") and the size budget (reported as the
    // truncated-file list). Keep both meanings explicit.
    let truncated_files = analysis_selection.oversized_dropped();
    let excluded_files = analysis_selection.path_gate_excluded_count();

    // Coverage ledger (spec 14 §4): freeze the denominator now, before anything
    // is dispatched. From here on the ledger records what happened to that set
    // and nothing else — the denominator is never revised by the outcome.
    let coverage = units::CoverageLedger::freeze(&analysis_selection.units);
    tracing::info!(
        "coverage: {} review unit(s) selected, {} excluded file(s), {} truncated file(s)",
        coverage.denominator().len(),
        excluded_files,
        truncated_files.len()
    );

    Ok(Prep::Ready(Box::new(ReviewInputs {
        repo,
        gh,
        resolve_gh,
        pr_ref,
        pr,
        head_sha,
        prior_sha,
        incremental,
        anchor_parsed,
        analysis_text,
        truncated_files,
        excluded_files,
        analysis_selection,
        coverage,
    })))
}

/// Preview the selection (spec 14 §3): report what a run would review and
/// dispatch nothing.
///
/// It shares `prepare_inputs` with the real run on purpose — the structural half
/// of "one decision, two consumers" (spec 14 §2) — so a preview never needs model
/// credentials and can never disagree with what the run then does.
pub async fn preview(cfg: &Config, args: &ReviewArgs, json: bool) -> anyhow::Result<Outcome> {
    let inputs = match prepare_inputs(cfg, args, false).await? {
        Prep::Done(outcome) => return Ok(outcome),
        Prep::Ready(inputs) => *inputs,
    };
    let selection = &inputs.analysis_selection;
    if json {
        // Machine-readable: never localized (spec 09's rule for machine payloads).
        let doc = output::preview_json(selection, inputs.incremental);
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        let t = T::new(cfg.language);
        let scope = match (inputs.incremental, inputs.prior_sha.as_deref()) {
            (true, Some(prior)) => t.scope_incremental(&prior[..7.min(prior.len())]),
            _ => t.scope_full().to_string(),
        };
        print_preview(selection, &scope, &t);
    }
    Ok(Outcome::Previewed {
        units: selection.units.len(),
        excluded: selection.excluded_count(),
        truncated: selection.oversized_dropped().len(),
    })
}

/// Human-readable preview on stdout (logs stay on stderr, spec 16 §4.3).
///
/// Labels are localized (spec 09: user-visible output follows `language`); unit
/// paths and exclusion reason codes stay as-is because they are identifiers.
fn print_preview(selection: &units::Selection, scope: &str, t: &T) {
    println!(
        "{}",
        t.preview_summary(scope, selection.units.len(), selection.estimated_tokens)
    );
    if selection.units.is_empty() {
        println!("{} {}", t.preview_will_review(), t.preview_nothing());
    } else {
        println!("{}", t.preview_will_review());
        for unit in &selection.units {
            println!("  {}", unit.files.join(", "));
        }
    }
    if !selection.excluded.is_empty() {
        println!("{}", t.preview_excluded());
        for e in &selection.excluded {
            let note = if e.kept_in_text {
                t.preview_kept_for_anchoring()
            } else {
                ""
            };
            println!("  {} ({}{})", e.path, e.reason.as_str(), note);
        }
    }
}

pub async fn run_review(
    cfg: &Config,
    args: &ReviewArgs,
    force_full: bool,
) -> anyhow::Result<Outcome> {
    let inputs = match prepare_inputs(cfg, args, force_full).await? {
        Prep::Done(outcome) => return Ok(outcome),
        Prep::Ready(inputs) => *inputs,
    };
    let ReviewInputs {
        repo,
        gh,
        resolve_gh,
        pr_ref,
        pr,
        head_sha,
        prior_sha,
        incremental,
        anchor_parsed,
        analysis_text,
        truncated_files,
        excluded_files,
        analysis_selection: _,
        mut coverage,
    } = inputs;

    let analysis_parsed = ParsedDiff::parse(&analysis_text);
    if analysis_parsed.files.is_empty() {
        let reason = if excluded_files > 0 {
            format!("all changes filtered out by rules ({excluded_files} files)")
        } else {
            "no reviewable changes in diff".to_string()
        };
        return Ok(skip_outcome(cfg, &gh, &repo, &head_sha, reason).await);
    }
    if analysis_text.len() > cfg.max_diff_kb * 1024 * 2 {
        coverage.truncate_all("diff over budget");
        tracing::warn!(
            "coverage: 0/{} unit(s) covered (terminal={})",
            coverage.denominator().len(),
            coverage.terminal().as_str()
        );
        let outcome = fail_or_open(
            cfg,
            anyhow::anyhow!(
                "diff exceeds 2x budget ({} KB), abandoning analysis",
                analysis_text.len() / 1024
            ),
        )?;
        if matches!(outcome, Outcome::AnalysisFailed(_)) {
            post_terminal_status(
                &gh,
                &repo,
                &head_sha,
                cfg,
                false,
                &failure_note("diff over budget, analysis abandoned", &coverage),
            )
            .await;
        }
        return Ok(outcome);
    }
    tracing::info!(
        "diff: {} files ({} mode, {} filtered, {} truncated), {} KB",
        analysis_parsed.files.len(),
        if incremental { "incremental" } else { "full" },
        excluded_files,
        truncated_files.len(),
        analysis_text.len() / 1024
    );

    // Historical unresolved findings (spec 07)
    let open_findings = fetch_open_findings(&gh, &repo, pr_ref.number).await;
    if !open_findings.is_empty() {
        tracing::info!("{} historical unresolved finding(s)", open_findings.len());
    }
    let open_fps: BTreeSet<String> = open_findings
        .iter()
        .flat_map(|f| f.fingerprints.iter().cloned())
        .collect();

    let prior_short = prior_sha.as_deref().map(|s| &s[..7.min(s.len())]);
    let mode = ReviewMode {
        incremental_from: if incremental { prior_short } else { None },
        open_findings: &open_findings,
    };

    // Analysis (fail-open zone, spec 01 exit-code contract); tool sandbox root =
    // the checked-out workspace
    //
    // Load repo instruction files from the base branch (spec 04 §repo-instructions;
    // never from the PR head — injection protection)
    let instructions = RepoInstructions::load(&cfg.workspace, &pr.base.ref_name).await;

    let t = T::new(cfg.language);
    gha_group_end();
    gha_group("analysis (multi-pass review / voting / verification)");
    let shared = ToolShared::new(cfg.workspace.clone(), &pr.base.ref_name, cfg.max_tool_calls);
    coverage.start_all();
    let started = std::time::Instant::now();
    let (analysis, usage) = match analyze(
        cfg,
        &analysis_parsed,
        &analysis_text,
        &truncated_files,
        &shared,
        &mode,
        &instructions,
    )
    .await
    {
        Ok((a, run_usage)) => {
            coverage.cover_all();
            tracing::info!(
                "coverage: {}/{} unit(s) covered (terminal={})",
                coverage.covered_count(),
                coverage.denominator().len(),
                coverage.terminal().as_str()
            );
            (a, run_usage)
        }
        Err(e) => {
            coverage.fail_all(format!("{e:#}"));
            tracing::warn!(
                "coverage: {}/{} unit(s) covered (terminal={})",
                coverage.covered_count(),
                coverage.denominator().len(),
                coverage.terminal().as_str()
            );
            gha_group_end();
            let outcome = fail_or_open(cfg, e.context("analysis failed"))?;
            if matches!(outcome, Outcome::AnalysisFailed(_)) {
                post_terminal_status(
                    &gh,
                    &repo,
                    &head_sha,
                    cfg,
                    false,
                    &failure_note("analysis failed (fail-open)", &coverage),
                )
                .await;
            }
            return Ok(outcome);
        }
    };
    tracing::info!(
        "{}",
        t.log_findings(analysis.findings.len(), analysis.resolved_finding_ids.len())
    );
    let trace = shared.trace();
    if !trace.is_empty() {
        let names: Vec<&str> = trace.iter().map(|t| t.name.as_str()).collect();
        tracing::info!("{}", t.log_tool_calls(trace.len(), &names.join(", ")));
    }

    // Rendering (anchoring always uses the full diff, spec 07)
    let scope_label = match (incremental, prior_short) {
        (true, Some(p)) => t.scope_incremental(p),
        _ => t.scope_full().to_string(),
    };
    let ctx = ReviewContext {
        repo_full_name: &repo.full_name(),
        head_sha: &pr.head.sha,
        meta_mode: if incremental { "incremental" } else { "full" },
        scope_label: &scope_label,
        files_reviewed: analysis_parsed.files.len(),
        excluded_files,
        summary: &analysis.summary,
        coverage: if cfg.report_coverage {
            Some(coverage.summary())
        } else {
            None
        },
    };
    let built = report::build_review(&analysis.findings, &anchor_parsed, cfg, &ctx, &open_fps);
    let inline_count = built.review.comments.len();

    if args.dry_run {
        let out = serde_json::json!({
            "mode": ctx.meta_mode,
            "terminal": coverage.terminal().as_str(),
            "units_covered": coverage.covered_count(),
            "units_total": coverage.denominator().len(),
            "commit_id": built.review.commit_id,
            "resolved_finding_ids": analysis.resolved_finding_ids,
            "carried_over": built.carried_over,
            "body": built.review.body,
            "comments": built.review.comments.iter().map(|c| serde_json::json!({
                "path": c.path, "line": c.line, "body": c.body,
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(Outcome::DryRun);
    }

    // Publishing (spec 06 (5): on failure fall back to a summary comment;
    // double failure -> exit 1)
    gha_group_end();
    gha_group("publish and wrap up (review / resolve / status checks)");
    let published = match gh.create_review(&repo, pr_ref.number, &built.review).await {
        Ok(_) => true,
        Err(e) => {
            tracing::warn!("review publishing failed ({e}), falling back to a summary comment");
            let fallback =
                report::render_fallback_comment(&built.review.body, &analysis.findings, &t);
            gh.create_issue_comment(&repo, pr_ref.number, &fallback)
                .await
                .map_err(|e2| {
                    anyhow::anyhow!("both review and fallback comment failed: {e} ; {e2}")
                })?;
            false
        }
    };

    // Resolve fixed threads (spec 07; falls back to in-thread marking when
    // GITHUB_TOKEN is restricted)
    let resolved_fps: BTreeSet<String> = analysis.resolved_finding_ids.iter().cloned().collect();
    let to_resolve = state::resolvable_threads(&open_findings, &resolved_fps);
    let mut resolved_count = 0;
    let mut replied_count = 0;
    for tid in &to_resolve {
        let comment_id = open_findings
            .iter()
            .find(|t| &t.thread_id == tid)
            .and_then(|t| t.first_comment_id);
        match resolve_or_reply(&resolve_gh, &repo, pr_ref.number, tid, comment_id).await {
            ResolveOutcome::Resolved => resolved_count += 1,
            ResolveOutcome::Replied => replied_count += 1,
            ResolveOutcome::Failed => {}
        }
    }
    if resolved_count > 0 || replied_count > 0 {
        tracing::info!("{}", t.log_resolved_threads(resolved_count, replied_count));
    }

    // status checks (spec 07)
    if cfg.status_checks {
        post_status_checks(
            &gh,
            &repo,
            &pr.head.sha,
            &analysis,
            &open_findings,
            &to_resolve,
            &t,
        )
        .await;
    }

    // Structured output (spec 16 §1): emitted after publishing so the document
    // describes the run that actually happened. Publishing is unaffected — the
    // document is an additional artifact, not a replacement for the review.
    if args.format.is_structured() {
        let meta = output::RunMeta {
            repository: repo.full_name(),
            change_request: Some(pr_ref.number),
            revision: output::revision_from_env(),
            model: cfg.model.clone(),
            usage,
            duration_ms: started.elapsed().as_millis(),
            terminal: coverage.terminal(),
        };
        let report = output::RunReport {
            meta: &meta,
            findings: &built.findings,
            ledger: &coverage,
            resolutions: &analysis.resolved_finding_ids,
        };
        let document = match args.format {
            output::OutputFormat::Json => output::json(&report),
            output::OutputFormat::Sarif => output::sarif(&report),
            output::OutputFormat::Human => unreachable!("guarded by is_structured"),
        };
        output::emit(
            &output::to_pretty(&document)?,
            args.output.as_deref(),
            &cfg.workspace,
        )?;
    }

    gha_group_end();
    Ok(Outcome::Published {
        inline_comments: if published { inline_count } else { 0 },
        terminal: coverage.terminal(),
    })
}

enum ResolveOutcome {
    Resolved,
    Replied,
    Failed,
}

/// Resolve a thread; when the platform restricts GITHUB_TOKEN (Resource not
/// accessible by integration), fall back to an in-thread "fixed" reply (spec 07)
async fn resolve_or_reply(
    gh: &GitHubClient,
    repo: &Repo,
    pr_number: u64,
    thread_id: &str,
    comment_id: Option<u64>,
) -> ResolveOutcome {
    match gh.resolve_review_thread(thread_id).await {
        Ok(()) => ResolveOutcome::Resolved,
        Err(e) => {
            let Some(cid) = comment_id else {
                tracing::warn!(
                    "failed to resolve thread {thread_id} and no comment id \
                     available for fallback: {e}"
                );
                return ResolveOutcome::Failed;
            };
            tracing::warn!(
                "resolve unavailable ({e}), falling back to in-thread fixed marker: {thread_id}"
            );
            match gh
                .reply_to_review_comment(repo, pr_number, cid, "✅ HoverStare confirmed fixed")
                .await
            {
                Ok(()) => ResolveOutcome::Replied,
                Err(e2) => {
                    tracing::warn!("fallback marking also failed: {e2}");
                    ResolveOutcome::Failed
                }
            }
        }
    }
}

/// Fetch historical unresolved hoverstare findings (GraphQL threads + marker parsing)
async fn fetch_open_findings(gh: &GitHubClient, repo: &Repo, number: u64) -> Vec<OpenFinding> {
    match gh.list_review_threads(repo, number).await {
        Ok(threads) => threads
            .into_iter()
            .filter(|t| !t.is_resolved)
            .filter_map(|t| {
                let fingerprints = state::extract_fingerprints(&t.first_comment_body);
                if fingerprints.is_empty() {
                    return None; // not a hoverstare thread
                }
                let has_high_severity =
                    t.first_comment_body.contains('🔴') || t.first_comment_body.contains('🟠');
                Some(OpenFinding {
                    thread_id: t.id,
                    fingerprints,
                    description: state::strip_markers(&t.first_comment_body),
                    has_high_severity,
                    first_comment_id: t.first_comment_id,
                })
            })
            .collect(),
        Err(e) => {
            tracing::warn!("failed to fetch historical threads (treating as no history): {e}");
            Vec::new()
        }
    }
}

/// Write the two status checks (spec 07); individual failures are only logged
async fn post_status_checks(
    gh: &GitHubClient,
    repo: &Repo,
    head_sha: &str,
    analysis: &AnalysisResult,
    open_findings: &[OpenFinding],
    resolved_threads: &[String],
    t: &T,
) {
    if let Err(e) = gh
        .create_status(
            repo,
            head_sha,
            &NewStatus {
                context: "hoverstare",
                state: StatusState::Success,
                description: t.status_review_done().to_string(),
            },
        )
        .await
    {
        tracing::warn!("failed to write status check hoverstare: {e}");
    }

    let new_high = analysis
        .findings
        .iter()
        .any(|f| f.severity >= Severity::High);
    let open_high = open_findings
        .iter()
        .filter(|t| !resolved_threads.contains(&t.thread_id))
        .any(|t| t.has_high_severity);
    let (state, desc) = if new_high || open_high {
        (
            StatusState::Failure,
            format!(
                "Unresolved high-severity findings (new: {}, open: {})",
                analysis
                    .findings
                    .iter()
                    .filter(|f| f.severity >= Severity::High)
                    .count(),
                open_findings
                    .iter()
                    .filter(|t| !resolved_threads.contains(&t.thread_id) && t.has_high_severity)
                    .count()
            ),
        )
    } else {
        (
            StatusState::Success,
            "No high-severity findings".to_string(),
        )
    };
    if let Err(e) = gh
        .create_status(
            repo,
            head_sha,
            &NewStatus {
                context: "hoverstare-findings",
                state,
                description: desc,
            },
        )
        .await
    {
        tracing::warn!("failed to write status check hoverstare-findings: {e}");
    }
}

/// Analysis entry point (public so examples/integration tests can reuse it):
/// multi-pass pipeline (spec 05)
pub async fn analyze(
    cfg: &Config,
    parsed: &ParsedDiff,
    diff_text: &str,
    truncated_files: &[String],
    shared: &Arc<ToolShared>,
    mode: &ReviewMode<'_>,
    instructions: &RepoInstructions,
) -> anyhow::Result<(AnalysisResult, UsageTotal)> {
    if !instructions.is_empty() {
        tracing::info!(
            "loaded {} repo instruction file(s): {}",
            instructions.sections.len(),
            instructions
                .sections
                .iter()
                .map(|(l, _)| l.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let backend = RigBackend::from_config(cfg)?;
    let outcome = crate::pipeline::run(
        &backend,
        cfg,
        parsed,
        diff_text,
        truncated_files,
        shared,
        mode,
        instructions,
    )
    .await?;
    let st = &outcome.stats;
    if st.passes_run > 1 || st.clusters > 0 {
        tracing::info!(
            "pipeline stats: {} pass(es), {} cluster(s), {} voted in, {} verified \
             in, {} dropped (findings per pass: {:?})",
            st.passes_run,
            st.clusters,
            st.voted_in,
            st.verified_in,
            st.dropped,
            st.pass_findings
        );
    }
    Ok((outcome.analysis, outcome.stats.usage))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ledger_with_one_failed_unit() -> units::CoverageLedger {
        let diff = "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,0 +1,1 @@\n+x\n";
        let selection = units::select(
            diff,
            &units::SelectOptions::new(&globset::GlobSetBuilder::new().build().unwrap(), 400),
        );
        let mut ledger = units::CoverageLedger::freeze(&selection.units);
        ledger.fail_all("provider 500");
        ledger
    }

    #[test]
    fn failure_notes_state_the_coverage() {
        let ledger = ledger_with_one_failed_unit();
        let note = failure_note("analysis failed (fail-open)", &ledger);
        assert!(note.contains("coverage 0/1"), "{note}");
        assert!(note.contains("1 failed"), "{note}");

        let ledger = ledger_with_one_failed_unit();
        let note = failure_note("diff over budget, analysis abandoned", &ledger);
        assert!(note.contains("coverage 0/1"), "{note}");
    }
}
