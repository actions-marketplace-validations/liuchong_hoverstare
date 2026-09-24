//! Review pipeline (spec 05): multi-pass voting + verifier false-positive reduction
//!
//! Path selection:
//! - `passes = 1` and `verify = false` -> straight through (M2 fault-tolerance pipeline)
//! - small diff (added lines < 50) -> 1 pass + verify
//! - otherwise -> N parallel lanes (different focus + staggered temperature) ->
//!   clustering -> >=2 votes accepted -> single-vote verifier

use std::collections::BTreeSet;
use std::sync::Arc;

use crate::agent::tools::ToolShared;
use crate::agent::{AgentBackend, Budget, ReviewRequest, ToolRegistry, Usage, UsageTotal};
use crate::config::Config;
use crate::diff::ParsedDiff;
use crate::findings::{self, AnalysisResult, Finding};
use crate::instructions::RepoInstructions;
use crate::prompt::{self, ReviewMode};

/// Full-analysis retry count (spec 04 output fault-tolerance pipeline, level 5)
const MAX_ANALYSIS_ATTEMPTS: u32 = 3;
#[cfg(not(test))]
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
#[cfg(test)]
const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(1);
/// reformat pass timeout (single-turn conversion without tools, much shorter
/// than the main review)
const REFORMAT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
/// Small-diff threshold (added lines); below it, force 1 pass + verify (spec 05
/// degradation rule)
const SMALL_DIFF_ADDED_LINES: u64 = 50;
/// Verifier budget is half of the main review budget
const VERIFIER_TOOL_BUDGET_DIVISOR: u32 = 2;

/// Pass focus (spec 05 table): same skeleton, only the focus paragraph and
/// temperature differ
struct Lens {
    focus: &'static str,
    temp: f64,
}

const LENSES: &[Lens] = &[
    Lens {
        focus: "correctness: logic errors, null dereferences, off-by-one errors, missing error handling",
        temp: 0.2,
    },
    Lens {
        focus: "concurrency & resources: races, deadlocks, leaks, lifetimes",
        temp: 0.4,
    },
    Lens {
        focus: "security & boundaries: injection, privilege escalation, input validation, integer overflow",
        temp: 0.6,
    },
];

pub struct PipelineOutcome {
    pub analysis: AnalysisResult,
    pub stats: PipelineStats,
}

#[derive(Debug, Default)]
pub struct PipelineStats {
    pub passes_run: usize,
    pub pass_findings: Vec<usize>,
    pub clusters: usize,
    pub voted_in: usize,
    pub verified_in: usize,
    pub dropped: usize,
    /// Token accounting for the whole run, including verifier and reformat calls
    /// (spec 16 §2: a total that omits them under-reports the cost).
    pub usage: UsageTotal,
}

#[allow(clippy::too_many_arguments)]
pub async fn run(
    backend: &dyn AgentBackend,
    cfg: &Config,
    parsed: &ParsedDiff,
    diff_text: &str,
    truncated_files: &[String],
    shared: &Arc<ToolShared>,
    mode: &ReviewMode<'_>,
    instructions: &RepoInstructions,
) -> anyhow::Result<PipelineOutcome> {
    let small_diff = parsed.added_line_count() < SMALL_DIFF_ADDED_LINES;
    let passes = if small_diff {
        1usize
    } else {
        (cfg.passes as usize).clamp(1, LENSES.len())
    };
    let verify = if small_diff { true } else { cfg.verify };

    // Language rule packs (spec 15 §4). Computed once per run, not per pass: the
    // block describes the change set, so every pass must see the same one.
    let rules_block = build_rules_block(cfg, parsed, shared).await;
    let rules_block = rules_block.as_deref();

    // Straight-through path (spec 05: passes=1 and no verify is equivalent to
    // having no pipeline)
    if passes == 1 && !verify {
        let (analysis, usage) = analyze_with_backend(
            backend,
            cfg,
            parsed,
            diff_text,
            truncated_files,
            shared,
            mode,
            instructions,
            rules_block,
        )
        .await?;
        return Ok(PipelineOutcome {
            analysis,
            stats: PipelineStats {
                passes_run: 1,
                usage,
                ..Default::default()
            },
        });
    }

    // ---- N parallel review lanes (join_all only borrows, no 'static needed) ----
    let extras: Vec<String> = (0..passes)
        .map(|i| format!("\n\n[FOCUS OF THIS PASS]\n{}", LENSES[i].focus))
        .collect();
    let futs: Vec<_> = (0..passes)
        .map(|i| {
            single_shot(
                backend,
                cfg,
                parsed,
                diff_text,
                truncated_files,
                shared,
                mode,
                instructions,
                rules_block,
                &extras[i],
                cfg.temp(LENSES[i].temp),
            )
        })
        .collect();
    let results = futures::future::join_all(futs).await;

    let mut per_pass: Vec<Vec<Finding>> = Vec::new();
    let mut summaries: Vec<(usize, String)> = Vec::new();
    let mut resolved_union: BTreeSet<String> = BTreeSet::new();
    let mut failures = 0usize;
    // Token accounting for the whole run (spec 16 §2), including failed passes:
    // a pass that burned tokens and failed was still paid for.
    let mut usage = UsageTotal::default();
    for (i, r) in results.into_iter().enumerate() {
        match r {
            Ok((a, pass_usage)) => {
                usage.add(&pass_usage);
                resolved_union.extend(a.resolved_finding_ids);
                summaries.push((a.findings.len(), a.summary));
                per_pass.push(a.findings);
            }
            Err(e) => {
                failures += 1;
                tracing::warn!("pass {} failed ({})", i + 1, e);
            }
        }
    }
    if per_pass.is_empty() {
        return Err(anyhow::anyhow!("all {passes} review passes failed"));
    }
    let pass_findings: Vec<usize> = per_pass.iter().map(|v| v.len()).collect();

    // ---- Clustering ----
    let clusters = cluster_findings(&per_pass);
    let cluster_count = clusters.len();

    // ---- Voting ----
    let mut accepted: Vec<Finding> = Vec::new();
    let mut singles: Vec<Finding> = Vec::new();
    for c in &clusters {
        let merged = merge_cluster(c);
        if c.pass_votes.len() >= 2 {
            accepted.push(merged);
        } else {
            singles.push(merged);
        }
    }
    let voted_in = accepted.len();

    // ---- verifier (re-checks single-vote findings; dropped directly when
    // verify=false) ----
    let mut verified_in = 0usize;
    let mut dropped = 0usize;
    for f in singles {
        if !verify {
            dropped += 1;
            continue;
        }
        let (accepted_by_verifier, verify_usage) =
            verify_finding(backend, cfg, shared, diff_text, &f).await;
        if let Some(u) = verify_usage {
            usage.add(&u);
        }
        if accepted_by_verifier {
            accepted.push(f);
            verified_in += 1;
        } else {
            dropped += 1;
        }
    }

    // summary comes from the pass with the most findings (spec 05)
    let summary = summaries
        .into_iter()
        .max_by_key(|(n, _)| *n)
        .map(|(_, s)| s)
        .unwrap_or_default();

    Ok(PipelineOutcome {
        analysis: AnalysisResult {
            findings: accepted,
            summary,
            resolved_finding_ids: resolved_union.into_iter().collect(),
        },
        stats: PipelineStats {
            passes_run: passes - failures,
            pass_findings,
            clusters: cluster_count,
            voted_in,
            verified_in,
            dropped,
            usage,
        },
    })
}

// ---------------------------------------------------------------------------
// Clustering and merging (spec 05)
// ---------------------------------------------------------------------------

struct Cluster {
    pass_votes: BTreeSet<usize>,
    members: Vec<Finding>,
}

const LINE_WINDOW: u64 = 3;
const TITLE_SIM_THRESHOLD: f64 = 0.5;
const STOPWORDS: &[&str] = &[
    "的", "了", "在", "是", "和", "与", "或", "会", "将", "被", "把", "对", "为", "于", "a", "an",
    "the", "of", "to", "in", "on", "for", "and", "or", "is", "be",
];

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{4e00}'..='\u{9fff}' | '\u{3400}'..='\u{4dbf}' | '\u{f900}'..='\u{faff}')
}

/// Title tokenization: ASCII by word, CJK by single character + bigram
/// (Chinese has no whitespace tokenization; n-grams make overlap measurable)
fn title_tokens(t: &str) -> BTreeSet<String> {
    fn flush_cjk(run: &mut Vec<char>, tokens: &mut BTreeSet<String>) {
        for c in run.iter() {
            let s = c.to_string();
            if !STOPWORDS.contains(&s.as_str()) {
                tokens.insert(s);
            }
        }
        for w in run.windows(2) {
            tokens.insert(w.iter().collect());
        }
        run.clear();
    }

    let mut tokens = BTreeSet::new();
    let mut word = String::new();
    let mut cjk_run: Vec<char> = Vec::new();
    let flush_word = |word: &mut String, tokens: &mut BTreeSet<String>| {
        if !word.is_empty() {
            if !STOPWORDS.contains(&word.as_str()) {
                tokens.insert(word.clone());
            }
            word.clear();
        }
    };

    for c in t.to_lowercase().chars() {
        if is_cjk(c) {
            flush_word(&mut word, &mut tokens);
            cjk_run.push(c);
        } else if c.is_alphanumeric() {
            if !cjk_run.is_empty() {
                flush_cjk(&mut cjk_run, &mut tokens);
            }
            word.push(c);
        } else {
            flush_word(&mut word, &mut tokens);
            if !cjk_run.is_empty() {
                flush_cjk(&mut cjk_run, &mut tokens);
            }
        }
    }
    flush_word(&mut word, &mut tokens);
    if !cjk_run.is_empty() {
        flush_cjk(&mut cjk_run, &mut tokens);
    }
    tokens
}

fn title_similarity(a: &str, b: &str) -> f64 {
    let (ta, tb) = (title_tokens(a), title_tokens(b));
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    inter / union
}

/// Greedy clustering: same file + line distance <=3 + title Jaccard >=0.5
/// counts as the same problem
fn cluster_findings(per_pass: &[Vec<Finding>]) -> Vec<Cluster> {
    let mut clusters: Vec<Cluster> = Vec::new();
    for (pass_idx, findings) in per_pass.iter().enumerate() {
        for f in findings {
            let hit = clusters.iter_mut().find(|c| {
                c.members.iter().any(|m| {
                    m.file == f.file
                        && m.line.abs_diff(f.line) <= LINE_WINDOW
                        && title_similarity(&m.title, &f.title) >= TITLE_SIM_THRESHOLD
                })
            });
            match hit {
                Some(c) => {
                    c.pass_votes.insert(pass_idx);
                    c.members.push(f.clone());
                }
                None => clusters.push(Cluster {
                    pass_votes: [pass_idx].into_iter().collect(),
                    members: vec![f.clone()],
                }),
            }
        }
    }
    clusters
}

/// Cluster merge: highest severity, longest description, deduplicated union of
/// additional_locations
fn merge_cluster(c: &Cluster) -> Finding {
    let mut best = c
        .members
        .iter()
        .max_by_key(|m| m.severity)
        .cloned()
        .expect("cluster is non-empty");
    if let Some(d) = c
        .members
        .iter()
        .map(|m| &m.description)
        .max_by_key(|d| d.len())
    {
        best.description = d.clone();
    }
    let mut locs: Vec<crate::findings::Location> = Vec::new();
    for m in &c.members {
        for l in &m.additional_locations {
            if !locs.contains(l) {
                locs.push(l.clone());
            }
        }
    }
    best.additional_locations = locs;
    best
}

// ---------------------------------------------------------------------------
// verifier (spec 05: independent re-check of single-vote findings)
// ---------------------------------------------------------------------------

const VERIFIER_SYSTEM: &str = r#"You are a strict code-review verifier. Your job is to judge whether a reported finding is a REAL, TRIGGERABLE defect.
You may use read-only tools to verify (read_file / grep / glob / show_base_file) — targeted verification only.
Criteria:
- confirmed: the problem genuinely exists in the code (even if the trigger is narrow), or it cannot be falsified and the mechanism is sound;
- rejected: you can DETERMINE it is not a real problem (impossible scenario, misread semantics, already guarded).
Rejection requires evidence; when in doubt, keep the finding.
Your final reply MUST be exactly one JSON object: {"verdict": "confirmed|rejected", "confidence": 0.0-1.0, "reason": "one-line reason"}.
No prose, no markdown fences. Begin with `{` and end with `}`."#;

async fn verify_finding(
    backend: &dyn AgentBackend,
    cfg: &Config,
    shared: &Arc<ToolShared>,
    diff_text: &str,
    f: &Finding,
) -> (bool, Option<Usage>) {
    let section = crate::diff::section_for_file(diff_text, &f.file).unwrap_or("");
    let finding_json = serde_json::json!({
        "file": f.file,
        "line": f.line,
        "severity": f.severity.as_str(),
        "title": f.title,
        "description": f.description,
    });
    let user = format!(
        "Verify the following review finding. Use tools to check before concluding.\n\n[Finding under review]\n{}\n\n[Relevant diff section]\n{}\n\n[Repository available for verification]",
        serde_json::to_string_pretty(&finding_json).unwrap_or_default(),
        section
    );
    let req = ReviewRequest {
        system_prompt: VERIFIER_SYSTEM.to_string(),
        user_prompt: user,
        tools: ToolRegistry {
            shared: Some(shared.clone()),
            ..Default::default()
        },
        budget: Budget {
            max_tool_calls: (cfg.max_tool_calls / VERIFIER_TOOL_BUDGET_DIVISOR).max(1),
            timeout: REFORMAT_TIMEOUT,
        },
        model: cfg.model.clone(),
        temperature: cfg.temp(0.0),
    };
    let run = match backend.review(req).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("verifier call failed (treating as rejected): {e}");
            return (false, None);
        }
    };
    let accepted = match findings::extract_json_value(run.raw_output.trim()) {
        None => {
            tracing::warn!("verifier output is not JSON (treating as rejected)");
            false
        }
        Some(v) => {
            let verdict = v["verdict"].as_str().unwrap_or("rejected");
            let confidence = v["confidence"].as_f64().unwrap_or(0.0);
            verdict == "confirmed" && confidence >= 0.6
        }
    };
    (accepted, Some(run.usage))
}

// ---------------------------------------------------------------------------
// Single call and fault-tolerance pipeline (spec 04)
// ---------------------------------------------------------------------------

/// Build the `[LANGUAGE RULES]` block for the change set (spec 15 §4).
///
/// Only ambiguous extensions need their content, so only those heads are read —
/// through the same sandbox as every other file read. A read that fails leaves
/// the declared pack in place: an unreadable file never silently changes which
/// checkpoints apply, it just means no sniff happened.
async fn build_rules_block(
    cfg: &Config,
    parsed: &ParsedDiff,
    shared: &Arc<ToolShared>,
) -> Option<String> {
    if !cfg.rule_packs {
        tracing::info!("rule_resolved: disabled by configuration (rule_packs = false)");
        return None;
    }

    // Per-file resolution, logged per hit (spec 15 §9): "which checkpoints applied
    // to which file" must be answerable from the logs, including whether a sniff
    // (not the declared order) decided it.
    let mut resolutions: Vec<crate::rules::Resolution> = Vec::new();
    for file in &parsed.files {
        let head = if crate::rules::needs_sniff(&file.path) {
            // Bounded read through the same sandbox as every other file read; a
            // failure just means "no sniff", never a different answer.
            Some(crate::agent::tools::read_file(shared, &file.path, Some(1), Some(40)).await)
        } else {
            None
        };
        for hit in crate::rules::resolve(&file.path, head.as_deref(), cfg.max_rule_packs) {
            tracing::info!(
                "rule_resolved path={} pack={} matched={} sniffed={}",
                file.path,
                hit.pack_id,
                hit.matched_pattern,
                hit.sniffed
            );
            if !resolutions.iter().any(|r| r.pack_id == hit.pack_id) {
                resolutions.push(hit);
            }
        }
    }

    let block = crate::rules::render(&resolutions);
    match &block {
        Some(rendered) => tracing::info!(
            "rule_injected packs={} bytes={} budget={}",
            resolutions.len(),
            rendered.len(),
            crate::rules::MAX_INJECTED_BYTES
        ),
        None => tracing::info!("rule_injected packs=0 (nothing to inject)"),
    }
    block
}

/// Single analysis call (no retries, for multi-pass use; the focus paragraph is
/// appended to the system prompt)
#[allow(clippy::too_many_arguments)]
async fn single_shot(
    backend: &dyn AgentBackend,
    cfg: &Config,
    parsed: &ParsedDiff,
    diff_text: &str,
    truncated_files: &[String],
    shared: &Arc<ToolShared>,
    mode: &ReviewMode<'_>,
    instructions: &RepoInstructions,
    rules_block: Option<&str>,
    extra_system: &str,
    temperature: Option<f64>,
) -> anyhow::Result<(AnalysisResult, Usage)> {
    let req = ReviewRequest {
        system_prompt: prompt::system_prompt(cfg, instructions, rules_block) + extra_system,
        user_prompt: prompt::user_prompt(diff_text, parsed, cfg, truncated_files, mode),
        tools: ToolRegistry {
            shared: Some(shared.clone()),
            ..Default::default()
        },
        budget: Budget {
            max_tool_calls: cfg.max_tool_calls,
            timeout: std::time::Duration::from_secs(cfg.timeout_secs),
        },
        model: cfg.model.clone(),
        temperature,
    };
    let run = backend.review(req).await?;
    tracing::debug!(
        "raw model output ({} chars): {:.2000}",
        run.raw_output.len(),
        run.raw_output
    );
    let analysis = findings::parse_analysis(&run.raw_output).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((analysis, run.usage))
}

/// Fault-tolerance pipeline (spec 04): analyze -> parse failure -> reformat
/// pass -> full retry (up to 3 times)
#[allow(clippy::too_many_arguments)]
pub async fn analyze_with_backend(
    backend: &dyn AgentBackend,
    cfg: &Config,
    parsed: &ParsedDiff,
    diff_text: &str,
    truncated_files: &[String],
    shared: &Arc<ToolShared>,
    mode: &ReviewMode<'_>,
    instructions: &RepoInstructions,
    rules_block: Option<&str>,
) -> anyhow::Result<(AnalysisResult, UsageTotal)> {
    let mut last_err: Option<anyhow::Error> = None;
    // Every provider call counts, including failed attempts and the reformat
    // pass: the number reported to the user must be the number that was paid for.
    let mut usage = UsageTotal::default();

    for attempt in 1..=MAX_ANALYSIS_ATTEMPTS {
        if attempt > 1 {
            tracing::warn!(
                "retrying analysis {attempt}/{MAX_ANALYSIS_ATTEMPTS} (in {RETRY_DELAY:?})"
            );
            tokio::time::sleep(RETRY_DELAY).await;
        }

        let run = match single_attempt(
            backend,
            cfg,
            parsed,
            diff_text,
            truncated_files,
            shared,
            mode,
            instructions,
            rules_block,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("agent call failed: {e}");
                last_err = Some(e);
                continue;
            }
        };

        usage.add(&run.usage);
        match findings::parse_analysis(&run.raw_output) {
            Ok(a) => return Ok((a, usage)),
            Err(e) => {
                tracing::warn!("output parsing failed: {e}");
                if !run.raw_output.trim().is_empty() {
                    match reformat_with_backend(backend, cfg, &run.raw_output).await {
                        Ok((a, reformat_usage)) => {
                            usage.add(&reformat_usage);
                            tracing::info!("reformat pass recovered successfully");
                            return Ok((a, usage));
                        }
                        Err(e2) => tracing::warn!("reformat pass failed: {e2}"),
                    }
                } else {
                    tracing::warn!(
                        "model returned empty output, skipping reformat and retrying directly"
                    );
                }
                last_err = Some(anyhow::anyhow!(e));
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("analysis failed")))
}

/// Single call that keeps the raw output (the fault-tolerance pipeline needs
/// the raw text for reformat)
#[allow(clippy::too_many_arguments)]
async fn single_attempt(
    backend: &dyn AgentBackend,
    cfg: &Config,
    parsed: &ParsedDiff,
    diff_text: &str,
    truncated_files: &[String],
    shared: &Arc<ToolShared>,
    mode: &ReviewMode<'_>,
    instructions: &RepoInstructions,
    rules_block: Option<&str>,
) -> anyhow::Result<crate::agent::ReviewRun> {
    let req = ReviewRequest {
        system_prompt: prompt::system_prompt(cfg, instructions, rules_block),
        user_prompt: prompt::user_prompt(diff_text, parsed, cfg, truncated_files, mode),
        tools: ToolRegistry {
            shared: Some(shared.clone()),
            ..Default::default()
        },
        budget: Budget {
            max_tool_calls: cfg.max_tool_calls,
            timeout: std::time::Duration::from_secs(cfg.timeout_secs),
        },
        model: cfg.model.clone(),
        temperature: None,
    };
    backend.review(req).await.map_err(|e| anyhow::anyhow!(e))
}

/// reformat pass (spec 04 level 4): a cheap model rewrites prose output into
/// schema JSON
async fn reformat_with_backend(
    backend: &dyn AgentBackend,
    cfg: &Config,
    raw_output: &str,
) -> anyhow::Result<(AnalysisResult, Usage)> {
    let req = ReviewRequest {
        system_prompt: prompt::REFORMAT_SYSTEM_PROMPT.to_string(),
        user_prompt: prompt::reformat_user_prompt(raw_output),
        tools: ToolRegistry::default(),
        budget: Budget {
            max_tool_calls: 0,
            timeout: REFORMAT_TIMEOUT,
        },
        model: cfg.reformat_model.clone(),
        temperature: cfg.temp(0.0),
    };
    let run = backend.review(req).await?;
    let analysis = findings::parse_analysis(&run.raw_output).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok((analysis, run.usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentError, ReviewRun};
    use crate::config::Severity;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Deterministic per-call usage so aggregation can be asserted (not "some
    /// numbers were reported" but "exactly these numbers").
    fn fake_usage() -> Usage {
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            cached_input_tokens: 50,
        }
    }

    /// Distinguishes passes by whether the system_prompt contains the "focus",
    /// and returns scripted outputs in order
    struct FakeBackend {
        outputs: Mutex<Vec<Result<&'static str, &'static str>>>,
        calls: AtomicUsize,
        reformat_calls: AtomicUsize,
        last_verify_output: Mutex<Option<&'static str>>,
        /// Last system prompt seen: the rule-pack block is only observable from
        /// inside the request, so a test has to capture it.
        last_system_prompt: Mutex<Option<String>>,
    }

    impl FakeBackend {
        fn new(outputs: Vec<Result<&'static str, &'static str>>) -> FakeBackend {
            FakeBackend {
                outputs: Mutex::new(outputs),
                calls: AtomicUsize::new(0),
                reformat_calls: AtomicUsize::new(0),
                last_verify_output: Mutex::new(Some(
                    r#"{"verdict":"confirmed","confidence":0.9,"reason":"ok"}"#,
                )),
                last_system_prompt: Mutex::new(None),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentBackend for FakeBackend {
        async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError> {
            *self.last_system_prompt.lock().unwrap() = Some(req.system_prompt.clone());
            // verifier call (system contains "code-review verifier")
            if req.system_prompt.contains("code-review verifier") {
                let out = self.last_verify_output.lock().unwrap().take();
                return Ok(ReviewRun {
                    raw_output: out
                        .unwrap_or(r#"{"verdict":"rejected","confidence":0.2,"reason":"no"}"#)
                        .to_string(),
                    usage: fake_usage(),
                    ..Default::default()
                });
            }
            // reformat call (no tools + reformat model name)
            if req.model == "reformat-model" {
                self.reformat_calls.fetch_add(1, Ordering::SeqCst);
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let out = self.outputs.lock().unwrap().remove(0);
            match out {
                Ok(text) => Ok(ReviewRun {
                    raw_output: text.to_string(),
                    usage: fake_usage(),
                    ..Default::default()
                }),
                Err(msg) => Err(AgentError::Backend(msg.to_string())),
            }
        }
    }

    fn ins() -> crate::instructions::RepoInstructions {
        crate::instructions::RepoInstructions::empty()
    }

    fn shared() -> Arc<ToolShared> {
        ToolShared::new(std::env::temp_dir(), "HEAD", 10)
    }

    fn mode() -> ReviewMode<'static> {
        ReviewMode::default()
    }

    fn cfg() -> Config {
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "test");
        }
        let mut c = Config::load().unwrap();
        c.model = "main-model".into();
        c.reformat_model = "reformat-model".into();
        c.timeout_secs = 5;
        c
    }

    fn diff_input(added: usize) -> (ParsedDiff, String) {
        let adds: String = (0..added).map(|i| format!("+l{i}\n")).collect();
        let text = format!(
            "diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,{} @@\n ctx\n{adds}",
            added + 1
        );
        (ParsedDiff::parse(&text), text)
    }

    fn finding_json(file: &str, line: u64, title: &str) -> String {
        format!(
            r#"{{"file":"{file}","line":{line},"severity":"high","title":"{title}","description":"d-{title}"}}"#
        )
    }

    // ---------- Clustering and voting ----------

    #[test]
    fn clustering_merges_near_duplicates() {
        let f = |line: u64, title: &str| Finding {
            file: "a.rs".into(),
            line,
            severity: Severity::High,
            title: title.into(),
            description: "d".into(),
            suggestion: None,
            additional_locations: vec![],
        };
        let per_pass = vec![
            vec![f(10, "空指针解引用"), f(50, "整数溢出")],
            vec![f(11, "空指针 解引用风险"), f(51, "整数溢出问题")],
            vec![f(10, "可能存在空指针解引用")],
        ];
        let clusters = cluster_findings(&per_pass);
        assert_eq!(clusters.len(), 2);
        let npe = clusters
            .iter()
            .find(|c| c.members[0].line.abs_diff(10) <= 3)
            .unwrap();
        assert_eq!(npe.pass_votes.len(), 3); // reported by all three lanes -> 3 votes
        let overflow = clusters
            .iter()
            .find(|c| c.members[0].line.abs_diff(50) <= 3)
            .unwrap();
        assert_eq!(overflow.pass_votes.len(), 2);
    }

    #[test]
    fn title_similarity_basic() {
        assert!(title_similarity("空指针解引用", "空指针解引用风险") > 0.5);
        assert!(title_similarity("空指针解引用", "整数溢出") < 0.5);
    }

    #[test]
    fn merge_takes_highest_severity_and_longest_description() {
        let mut a = Finding {
            file: "a.rs".into(),
            line: 1,
            severity: Severity::Medium,
            title: "t".into(),
            description: "短".into(),
            suggestion: None,
            additional_locations: vec![crate::findings::Location {
                file: "b.rs".into(),
                line: 2,
                note: None,
            }],
        };
        let b = Finding {
            severity: Severity::Critical,
            description: "非常非常长的详细描述".into(),
            ..a.clone()
        };
        a.severity = Severity::Medium;
        let c = Cluster {
            pass_votes: [0, 1].into_iter().collect(),
            members: vec![a, b],
        };
        let merged = merge_cluster(&c);
        assert_eq!(merged.severity, Severity::Critical);
        assert_eq!(merged.description, "非常非常长的详细描述");
        assert_eq!(merged.additional_locations.len(), 1); // deduplicated
    }

    // ---------- Pipeline behavior ----------

    #[tokio::test]
    async fn straight_through_when_one_pass_no_verify() {
        // passes=1 and verify=false -> fault-tolerance pipeline (M2 behavior)
        let mut c = cfg();
        c.passes = 1;
        c.verify = false;
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![Ok(r#"{"findings":[],"summary":"clean"}"#)]);
        let out = run(
            &backend,
            &c,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert_eq!(out.stats.passes_run, 1);
        assert_eq!(out.analysis.findings.len(), 0);
    }

    #[tokio::test]
    async fn multi_pass_voting() {
        // Large diff, 3 lanes: two lanes report the same problem (voted in),
        // one lane-unique finding (rejected by the verifier)
        let mut c = cfg();
        c.passes = 3;
        c.verify = true;
        let (parsed, text) = diff_input(100);
        let common = finding_json("a.rs", 10, "空指针解引用");
        let unique = finding_json("a.rs", 60, "一个可疑但只有一路看到的问题");
        let backend = FakeBackend {
            outputs: Mutex::new(vec![
                Ok(Box::leak(
                    format!(r#"{{"findings":[{common}, {unique}],"summary":"s1"}}"#)
                        .into_boxed_str(),
                )),
                Ok(Box::leak(
                    format!(
                        r#"{{"findings":[{}],"summary":"s2"}}"#,
                        finding_json("a.rs", 11, "空指针解引用风险")
                    )
                    .into_boxed_str(),
                )),
                Ok(Box::leak(
                    format!(
                        r#"{{"findings":[{}],"summary":"s3"}}"#,
                        finding_json("a.rs", 10, "可能存在空指针解引用")
                    )
                    .into_boxed_str(),
                )),
            ]),
            last_system_prompt: Mutex::new(None),
            calls: AtomicUsize::new(0),
            reformat_calls: AtomicUsize::new(0),
            last_verify_output: Mutex::new(Some(
                r#"{"verdict":"rejected","confidence":0.9,"reason":"误报"}"#,
            )),
        };
        let out = run(
            &backend,
            &c,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert_eq!(out.stats.passes_run, 3);
        assert_eq!(out.stats.voted_in, 1); // common problem voted in with 3 votes
        assert_eq!(out.stats.dropped, 1); // lane-unique problem rejected by the verifier
        assert_eq!(out.analysis.findings.len(), 1);
        assert!(out.analysis.findings[0].title.contains("空指针"));
    }

    #[tokio::test]
    async fn single_finding_rescued_by_verifier() {
        let mut c = cfg();
        c.passes = 2;
        c.verify = true;
        let (parsed, text) = diff_input(100);
        let unique = finding_json("a.rs", 60, "一路独见但真实的问题");
        let backend = FakeBackend::new(vec![
            Ok(Box::leak(
                format!(r#"{{"findings":[{unique}],"summary":"s1"}}"#).into_boxed_str(),
            )),
            Ok(r#"{"findings":[],"summary":"s2"}"#),
        ]);
        // default verifier output: confirmed 0.9
        let out = run(
            &backend,
            &c,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert_eq!(out.stats.verified_in, 1);
        assert_eq!(out.analysis.findings.len(), 1);
    }

    #[tokio::test]
    async fn small_diff_forces_single_pass_with_verify() {
        // Small diff (<50 added lines): degraded to 1 pass + verify even with passes=3
        let mut c = cfg();
        c.passes = 3;
        c.verify = false; // small diff forces verify on
        let (parsed, text) = diff_input(10);
        let unique = finding_json("a.rs", 5, "小问题");
        let backend = FakeBackend::new(vec![Ok(Box::leak(
            format!(r#"{{"findings":[{unique}],"summary":"s"}}"#).into_boxed_str(),
        ))]);
        let out = run(
            &backend,
            &c,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert_eq!(out.stats.passes_run, 1);
        assert_eq!(out.stats.verified_in, 1);
    }

    #[tokio::test]
    async fn one_pass_failure_does_not_sink_others() {
        let mut c = cfg();
        c.passes = 2;
        c.verify = false;
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![Err("boom"), Ok(r#"{"findings":[],"summary":"s2"}"#)]);
        let out = run(
            &backend,
            &c,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert_eq!(out.stats.passes_run, 1); // only one lane succeeded
        assert_eq!(out.analysis.summary, "s2");
    }

    #[tokio::test]
    async fn all_passes_fail_is_error() {
        let mut c = cfg();
        c.passes = 2;
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![Err("boom1"), Err("boom2")]);
        assert!(
            run(
                &backend,
                &c,
                &parsed,
                &text,
                &[],
                &shared(),
                &mode(),
                &ins()
            )
            .await
            .is_err()
        );
    }

    // ---------- Fault-tolerance pipeline (M2 behavior regression) ----------

    #[tokio::test]
    async fn prose_recovered_by_reformat() {
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![
            Ok("发现一个 bug：a.rs 第 2 行有 NPE 风险"),
            Ok(
                r#"{"findings":[{"file":"a.rs","line":2,"severity":"high","title":"t","description":"d"}],"summary":"s"}"#,
            ),
        ]);
        let (r, usage) = analyze_with_backend(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.findings.len(), 1);
        assert_eq!(backend.reformat_calls.load(Ordering::SeqCst), 1);
        // The initial call plus the reformat pass: both were paid for.
        assert_eq!(usage.calls, 2);
        assert_eq!(usage.input_tokens, 200);
    }

    #[tokio::test]
    async fn empty_output_retries_without_reformat() {
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![Ok(""), Ok(r#"{"findings":[],"summary":"s"}"#)]);
        let (r, usage) = analyze_with_backend(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.findings.len(), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        assert_eq!(backend.reformat_calls.load(Ordering::SeqCst), 0);
        // An empty output is retried, and the retry counted: failed calls are
        // still paid for (spec 16 §2).
        assert_eq!(usage.calls, 2);
    }

    #[tokio::test]
    async fn persistent_garbage_fails_after_max_attempts() {
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![
            Ok("g1"),
            Ok("g2"),
            Ok("g3"),
            Ok("g4"),
            Ok("g5"),
            Ok("g6"),
        ]);
        let r = analyze_with_backend(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
            None,
        )
        .await;
        assert!(r.is_err());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 6); // 3 main reviews + 3 reformats (FakeBackend counts them together)
    }

    #[tokio::test]
    async fn backend_error_then_success() {
        let (parsed, text) = diff_input(100);
        let backend = FakeBackend::new(vec![Err("boom"), Ok(r#"{"findings":[],"summary":"s"}"#)]);
        let (r, usage) = analyze_with_backend(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.findings.len(), 0);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        // The call that failed at the backend level burned no reported tokens;
        // only the successful one is counted.
        assert_eq!(usage.calls, 1);
    }

    #[tokio::test]
    async fn rule_packs_reach_the_review_prompt_and_can_be_switched_off() {
        // The block is built by `run` (from the change set), so the test has to
        // go through `run` rather than through a single backend call.
        let (parsed, text) = diff_input(100);
        let one = r#"{"findings":[{"file":"src/a.rs","line":2,"severity":"high","title":"t","description":"d"}],"summary":"s"}"#;

        let backend = FakeBackend::new(vec![Ok(one), Ok(one), Ok(one)]);
        run(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        let prompt = backend
            .last_system_prompt
            .lock()
            .unwrap()
            .clone()
            .expect("the fake backend sees every request");
        assert!(
            prompt.contains(crate::rules::RULES_HEADER),
            "the rule block must be injected: {prompt}"
        );
        assert!(prompt.contains(crate::rules::RULES_PRECEDENCE_NOTE));
        assert!(
            prompt.contains("Rust"),
            "the .rs checkpoints must be present"
        );

        let mut off = cfg();
        off.rule_packs = false;
        let backend = FakeBackend::new(vec![Ok(one), Ok(one), Ok(one)]);
        run(
            &backend,
            &off,
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        assert!(
            backend
                .last_system_prompt
                .lock()
                .unwrap()
                .as_deref()
                .is_some_and(|p| !p.contains(crate::rules::RULES_HEADER)),
            "rule_packs = false must remove the block entirely"
        );
    }

    #[tokio::test]
    async fn multi_pass_usage_includes_every_pass_and_the_verifier() {
        let (parsed, text) = diff_input(200);
        // Three passes, one reporting a finding: a single vote must go through the
        // verifier, and the verifier call is charged like any other (spec 16 §2).
        let one = r#"{"findings":[{"file":"src/a.rs","line":2,"severity":"high","title":"t","description":"d"}],"summary":"s"}"#;
        let none = r#"{"findings":[],"summary":"s"}"#;
        let backend = FakeBackend::new(vec![Ok(one), Ok(none), Ok(none)]);
        let outcome = run(
            &backend,
            &cfg(),
            &parsed,
            &text,
            &[],
            &shared(),
            &mode(),
            &ins(),
        )
        .await
        .unwrap();
        let usage = outcome.stats.usage;
        assert!(
            usage.calls >= 4,
            "3 passes + verifier expected to be counted, got {}",
            usage.calls
        );
        assert_eq!(usage.input_tokens, 100 * usage.calls as u64);
        assert_eq!(usage.cached_input_tokens, 50 * usage.calls as u64);
    }
}
