//! Report rendering (spec 06, M1 subset)
//!
//! Validate -> anchor (fallback chain) -> merge same anchors -> render.
//! Fingerprint tracking and incremental metadata are added in M4.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::Config;
use crate::diff::ParsedDiff;
use crate::findings::Finding;
use crate::github::{NewInlineComment, NewReview};
use crate::i18n::T;
use crate::state;

pub struct ReviewContext<'a> {
    pub repo_full_name: &'a str,
    pub head_sha: &'a str,
    /// Mode recorded in the metadata: full | incremental (spec 07)
    pub meta_mode: &'a str,
    /// Scope description text in the body (e.g. "Full review" / "Incremental review (since abc1234)")
    pub scope_label: &'a str,
    pub files_reviewed: usize,
    pub excluded_files: usize,
    pub summary: &'a str,
    /// Coverage of this run (spec 14 §4). `None` when the review body should not
    /// state it (`report_coverage = false`).
    pub coverage: Option<crate::units::CoverageSummary>,
}

/// Anchoring fallback chain (spec 06 (2)):
/// valid line -> snap to nearest line -> body section
enum Anchor {
    Exact(u64),
    Snapped(u64),
    BodySection,
}

fn anchor_for(f: &Finding, diff: &ParsedDiff) -> Anchor {
    let Some(lines) = diff.commentable_lines(&f.file) else {
        return Anchor::BodySection; // file is not in the diff
    };
    if lines.is_empty() {
        return Anchor::BodySection; // e.g. deleted files, no commentable lines
    }
    if lines.contains(&f.line) {
        return Anchor::Exact(f.line);
    }
    match diff.nearest_anchor(&f.file, f.line) {
        Some(l) => Anchor::Snapped(l),
        None => Anchor::BodySection,
    }
}

/// Result of build_review: the review itself + incremental statistics
pub struct BuiltReview {
    pub review: NewReview,
    /// Number of findings already in the historical unresolved set, skipped
    /// this time to avoid duplicate comments (spec 07)
    pub carried_over: usize,
}

pub fn build_review(
    findings: &[Finding],
    diff: &ParsedDiff,
    cfg: &Config,
    ctx: &ReviewContext,
    open_fps: &BTreeSet<String>,
) -> BuiltReview {
    let t = T::new(cfg.language);
    let mut buckets: BTreeMap<(String, u64), Vec<String>> = BTreeMap::new();
    let mut cross_cutting: Vec<&Finding> = Vec::new();
    let mut nitpicks: Vec<&Finding> = Vec::new();
    let mut carried_over = 0usize;
    // Finding list for the metadata (fingerprint + location + severity)
    let mut meta_findings: Vec<(String, String, u64, crate::config::Severity)> = Vec::new();

    for f in findings {
        if f.severity < cfg.severity_threshold {
            nitpicks.push(f);
            continue;
        }
        match anchor_for(f, diff) {
            Anchor::Exact(line) | Anchor::Snapped(line) => {
                let snapped = if matches!(anchor_for(f, diff), Anchor::Snapped(_)) {
                    Some(f.line)
                } else {
                    None
                };
                let fp = state::fingerprint(&f.file, diff.line_content(&f.file, line), &f.title);
                if open_fps.contains(&fp) {
                    carried_over += 1;
                    continue; // historical thread still open, do not comment again (spec 07)
                }
                meta_findings.push((fp.clone(), f.file.clone(), line, f.severity));
                buckets
                    .entry((f.file.clone(), line))
                    .or_default()
                    .push(render_inline(f, snapped, &fp, &t));
            }
            Anchor::BodySection => cross_cutting.push(f),
        }
    }

    // Merge findings sharing an anchor (prevents GitHub 422)
    let comments: Vec<NewInlineComment> = buckets
        .into_iter()
        .map(|((path, line), bodies)| NewInlineComment {
            path,
            line,
            body: bodies.join("\n\n---\n\n"),
        })
        .collect();

    let body = render_body(
        ctx,
        &cross_cutting,
        &nitpicks,
        comments.len(),
        cfg,
        &meta_findings,
        &t,
    );

    BuiltReview {
        review: NewReview {
            commit_id: ctx.head_sha.to_string(),
            body,
            comments,
        },
        carried_over,
    }
}

fn render_inline(f: &Finding, snapped_from: Option<u64>, fp: &str, t: &T) -> String {
    let mut s = format!(
        "{} **{}**: {}\n\n{}",
        f.severity.emoji(),
        f.severity.as_str().to_uppercase(),
        f.title,
        f.description
    );

    if !f.additional_locations.is_empty() {
        s.push_str(&format!("\n\n{}", t.related_locations()));
        for loc in &f.additional_locations {
            let note = loc
                .note
                .as_deref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default();
            s.push_str(&format!("\n- `{}:{}`{note}", loc.file, loc.line));
        }
    }

    if let Some(code) = &f.suggestion {
        s.push_str(&format!("\n\n```suggestion\n{code}\n```"));
    }

    if let Some(orig) = snapped_from {
        s.push_str(&format!("\n\n{}", t.snap_note(orig)));
    }
    // Hidden marker: cross-commit tracking (spec 07), always on the last line
    s.push_str(&format!("\n\n{}", state::marker(fp)));
    s
}

fn render_body(
    ctx: &ReviewContext,
    cross_cutting: &[&Finding],
    nitpicks: &[&Finding],
    inline_count: usize,
    cfg: &Config,
    meta_findings: &[(String, String, u64, crate::config::Severity)],
    t: &T,
) -> String {
    let mut b = String::from("## 👁 HoverStare Review\n\n");

    b.push_str(&format!(
        "**{}** — {}; {}",
        t.scope_heading(),
        ctx.scope_label,
        t.files_count(ctx.files_reviewed)
    ));
    if ctx.excluded_files > 0 {
        b.push_str(&t.excluded_note(ctx.excluded_files));
    }
    b.push_str("\n\n");

    // Coverage statement (spec 14 §4): the run says what it promised to review
    // and what it covered, so "nothing was reported" cannot be confused with
    // "nothing was looked at".
    if let Some(coverage) = ctx.coverage {
        b.push_str(&t.coverage_line(
            coverage.covered,
            coverage.total,
            coverage.failed,
            coverage.truncated,
        ));
        b.push_str("\n\n");
    }

    if !ctx.summary.is_empty() {
        b.push_str(ctx.summary);
        b.push_str("\n\n");
    }

    // cross-cutting: findings that cannot be anchored to a line (files outside
    // the diff, files without commentable lines)
    for f in cross_cutting {
        b.push_str(&format!(
            "### {} {}\n\n{}\n\n",
            f.severity.emoji(),
            f.title,
            f.description
        ));
        let url = format!(
            "https://github.com/{}/blob/{}/{}#L{}",
            ctx.repo_full_name, ctx.head_sha, f.file, f.line
        );
        b.push_str(&format!("> 📍 [`{}:{}`]({url})\n\n", f.file, f.line));
    }

    if !nitpicks.is_empty() {
        b.push_str("### ℹ️ Nitpicks\n\n");
        for f in nitpicks {
            b.push_str(&format!(
                "- {} **{}** `{}:{}` — {}\n",
                f.severity.emoji(),
                f.severity.as_str().to_uppercase(),
                f.file,
                f.line,
                f.title
            ));
        }
        b.push('\n');
    }

    b.push_str("---\n\n");
    if inline_count == 0 && cross_cutting.is_empty() {
        b.push_str(t.clean_verdict());
        b.push_str("\n\n");
    } else {
        b.push_str(&t.stats_line(
            inline_count,
            cross_cutting.len(),
            cfg.severity_threshold.as_str(),
        ));
        b.push_str("\n\n");
    }

    // Machine-readable metadata (incremental review depends on it, spec 07)
    let (units_total, units_covered, terminal) = match ctx.coverage {
        Some(c) => (c.total, c.covered, c.terminal.as_str()),
        None => (0, 0, "unknown"),
    };
    b.push_str(&format!(
        "<!-- hoverstare-meta\nmode: {}\nhead_sha: {}\nfiles_reviewed: {}\nexcluded_files: {}\n\
         units_total: {units_total}\nunits_covered: {units_covered}\nterminal: {terminal}\n",
        ctx.meta_mode, ctx.head_sha, ctx.files_reviewed, ctx.excluded_files
    ));
    for (fp, file, line, sev) in meta_findings {
        b.push_str(&format!("finding: {fp} {} {line} {}\n", file, sev.as_str()));
    }
    b.push_str("-->");
    b
}

/// Fallback comment used when review publishing fails (no anchoring, all
/// findings listed flat)
pub fn render_fallback_comment(body: &str, findings: &[Finding], t: &T) -> String {
    let mut s = String::from(body);
    if !findings.is_empty() {
        s.push_str(&format!("\n\n{}\n\n", t.fallback_header()));
        for f in findings {
            s.push_str(&format!(
                "- {} **{}** `{}:{}` — {}\n",
                f.severity.emoji(),
                f.severity.as_str().to_uppercase(),
                f.file,
                f.line,
                f.title
            ));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::findings::Location;

    fn cfg() -> Config {
        unsafe { std::env::set_var("OPENAI_API_KEY", "test") };
        Config::load().unwrap()
    }

    fn diff() -> ParsedDiff {
        ParsedDiff::parse(
            "diff --git a/src/a.rs b/src/a.rs\n--- a/src/a.rs\n+++ b/src/a.rs\n@@ -1,2 +1,3 @@\n ctx\n-old\n+new1\n+new2\n",
        )
    }

    fn finding(file: &str, line: u64, sev: crate::config::Severity) -> Finding {
        Finding {
            file: file.to_string(),
            line,
            severity: sev,
            title: "t".into(),
            description: "d".into(),
            suggestion: None,
            additional_locations: vec![],
        }
    }

    fn ctx<'a>(summary: &'a str) -> ReviewContext<'a> {
        ctx_with_coverage(summary, None)
    }

    fn ctx_with_coverage<'a>(
        summary: &'a str,
        coverage: Option<crate::units::CoverageSummary>,
    ) -> ReviewContext<'a> {
        ReviewContext {
            repo_full_name: "o/r",
            head_sha: "abc123",
            meta_mode: "full",
            scope_label: "Full review",
            files_reviewed: 1,
            excluded_files: 0,
            summary,
            coverage,
        }
    }

    fn build(fs: &[Finding], d: &ParsedDiff) -> crate::github::NewReview {
        build_review(fs, d, &cfg(), &ctx("s"), &Default::default()).review
    }

    #[test]
    fn exact_line_anchors_inline() {
        let d = diff();
        let fs = vec![finding("src/a.rs", 2, crate::config::Severity::High)];
        let r = build(&fs, &d);
        assert_eq!(r.comments.len(), 1);
        assert_eq!(r.comments[0].line, 2);
        assert!(r.body.contains("1 inline comment(s)"));
    }

    #[test]
    fn invalid_line_snaps_with_note() {
        let d = diff();
        let fs = vec![finding("src/a.rs", 999, crate::config::Severity::High)];
        let r = build(&fs, &d);
        assert_eq!(r.comments.len(), 1);
        assert_ne!(r.comments[0].line, 999);
        assert!(
            r.comments[0]
                .body
                .contains("anchored to the nearest changed line")
        );
    }

    #[test]
    fn file_outside_diff_goes_to_body() {
        let d = diff();
        let fs = vec![finding(
            "src/other.rs",
            5,
            crate::config::Severity::Critical,
        )];
        let r = build(&fs, &d);
        assert!(r.comments.is_empty());
        assert!(r.body.contains("### 🔴 t"));
        assert!(
            r.body
                .contains("https://github.com/o/r/blob/abc123/src/other.rs#L5")
        );
    }

    #[test]
    fn below_threshold_goes_to_nitpicks() {
        let d = diff();
        let fs = vec![finding("src/a.rs", 2, crate::config::Severity::Low)];
        let r = build(&fs, &d);
        assert!(r.comments.is_empty());
        assert!(r.body.contains("### ℹ️ Nitpicks"));
    }

    #[test]
    fn same_anchor_merges_into_one_comment() {
        let d = diff();
        let fs = vec![
            finding("src/a.rs", 2, crate::config::Severity::High),
            finding("src/a.rs", 2, crate::config::Severity::Critical),
        ];
        let r = build(&fs, &d);
        assert_eq!(r.comments.len(), 1);
        assert!(r.comments[0].body.contains("\n\n---\n\n"));
    }

    #[test]
    fn empty_findings_renders_clean() {
        let d = diff();
        let r = build(&[], &d);
        assert!(r.comments.is_empty());
        assert!(r.body.contains("✅ No defects found."));
        assert!(r.body.contains("<!-- hoverstare-meta"));
    }

    #[test]
    fn chinese_language_review_body() {
        let mut c = cfg();
        c.language = crate::i18n::Lang::ZhCn;
        let d = diff();
        let fs = vec![finding("src/a.rs", 2, crate::config::Severity::High)];
        let r = build_review(&fs, &d, &c, &ctx("s"), &Default::default()).review;
        assert!(!r.body.contains("✅ 未发现缺陷"));
        assert!(r.body.contains("共 1 条行内评论"));
        assert!(!r.body.contains("相关位置"));
    }

    #[test]
    fn suggestion_block_rendered() {
        let d = diff();
        let mut f = finding("src/a.rs", 2, crate::config::Severity::High);
        f.suggestion = Some("let x = 1;".into());
        f.additional_locations = vec![Location {
            file: "src/b.rs".into(),
            line: 9,
            note: Some("same kind".into()),
        }];
        let r = build(&[f], &d);
        assert!(
            r.comments[0]
                .body
                .contains("```suggestion\nlet x = 1;\n```")
        );
        assert!(r.comments[0].body.contains("`src/b.rs:9` — same kind"));
    }

    #[test]
    fn coverage_line_is_stated_and_absent_when_disabled() {
        let cfg = cfg();
        let t = T::new(crate::i18n::Lang::En);

        let summary = crate::units::CoverageSummary {
            total: 3,
            covered: 3,
            ..crate::units::CoverageSummary::default()
        };
        let built = build_review(
            &[],
            &diff(),
            &cfg,
            &ctx_with_coverage("s", Some(summary)),
            &BTreeSet::new(),
        );
        assert!(
            built.review.body.contains("Coverage: 3/3 review unit(s)"),
            "body must state the coverage: {}",
            built.review.body
        );
        // Machine-readable mirror (spec 16 will consume the same numbers).
        assert!(built.review.body.contains("units_total: 3"));
        assert!(built.review.body.contains("units_covered: 3"));

        let built = build_review(&[], &diff(), &cfg, &ctx("s"), &BTreeSet::new());
        assert!(
            !built.review.body.contains("Coverage:"),
            "report_coverage = false must omit the line"
        );
        assert!(built.review.body.contains("terminal: unknown"));
        let _ = t;
    }

    #[test]
    fn partial_coverage_is_visible_in_the_body() {
        let cfg = cfg();
        let summary = crate::units::CoverageSummary {
            total: 4,
            covered: 2,
            failed: 1,
            truncated: 1,
            terminal: crate::units::TerminalState::Partial,
        };
        let built = build_review(
            &[],
            &diff(),
            &cfg,
            &ctx_with_coverage("s", Some(summary)),
            &BTreeSet::new(),
        );
        assert!(
            built.review.body.contains("2/4") && built.review.body.contains("failed"),
            "partial coverage must be stated, not silent: {}",
            built.review.body
        );
        assert!(built.review.body.contains("terminal: partial"));
    }
}
