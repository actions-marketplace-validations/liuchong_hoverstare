//! `@hoverstare` comment commands (spec 09)

use std::sync::Arc;

use crate::agent::tools::ToolShared;
use crate::agent::{AgentBackend, Budget, ReviewRequest, ToolRegistry};
use crate::cli::ReviewArgs;
use crate::config::{Actor, Config, PermissionKey};
use crate::event::MentionEvent;
use crate::github::{GitHubClient, Repo, ReviewCommentDetail, ReviewThreadComment};
use crate::i18n::T;
use crate::orchestrator::{self, Outcome};

/// Sentinel the model answers with to stay silent in a finding thread (spec 09)
const NO_REPLY: &str = "[no-reply]";

/// Thread history window for finding discussions (spec 09 multi-turn memory):
/// last N replies, the whole rendered history capped at THREAD_MAX_BYTES
const THREAD_TAIL: usize = 20;
const THREAD_MAX_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionCommand {
    Review,
    Explain,
    Help,
}

/// Parse an @hoverstare command from a comment (@hoverstare inside code blocks
/// is ignored, spec 09)
pub fn parse_command(body: &str) -> Option<MentionCommand> {
    let stripped = strip_code_blocks(body);
    let at = stripped.find("@hoverstare")?;
    let after = stripped[at + "@hoverstare".len()..].trim_start();
    let mut first: String = after
        .chars()
        .take_while(|c| c.is_alphabetic())
        .collect::<String>()
        .to_lowercase();
    if first.is_empty() {
        // Accept slash aliases such as `@hoverstare /help` (issue #6)
        first = after.split_whitespace().next().unwrap_or("").to_lowercase();
    }
    Some(match first.as_str() {
        "review" => MentionCommand::Review,
        "explain" => MentionCommand::Explain,
        "help" | "/help" => MentionCommand::Help,
        // Unrecognized commands and bare @hoverstare -> help (spec 09)
        _ => MentionCommand::Help,
    })
}

/// Remove ``` fenced code blocks and `inline code`
pub(crate) fn strip_code_blocks(body: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in body.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence {
            out.push_str(line);
            out.push('\n');
        }
    }
    let mut res = String::new();
    let mut in_tick = false;
    for c in out.chars() {
        if c == '`' {
            in_tick = !in_tick;
            continue;
        }
        if !in_tick {
            res.push(c);
        }
    }
    res
}

/// mention command entry point (follows the same fail-open exit-code contract as review)
pub async fn run_mention(cfg: &Config) -> anyhow::Result<Outcome> {
    let Some(ev) = crate::event::resolve_mention()? else {
        return Ok(Outcome::Skipped("not a comment event".into()));
    };
    run_mention_event(cfg, &ev).await
}

/// Handle an already-parsed mention event (reused by serve mode, spec 10)
pub async fn run_mention_event(cfg: &Config, ev: &MentionEvent) -> anyhow::Result<Outcome> {
    // Bot authors never trigger (spec 09), not even with an @hoverstare command
    if ev.is_bot() {
        return Ok(Outcome::Skipped("bot author".into()));
    }
    let repo = Repo::parse(&ev.repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let gh = GitHubClient::new(cfg.github_token.clone())?;

    let Some(cmd) = parse_command(&ev.body) else {
        // No @mention: maybe a finding-thread discussion reply (issue #13)
        return run_thread_discussion(cfg, &gh, &repo, ev).await;
    };

    // Permission: help is always allowed; review/explain use the `review` key (spec 12)
    if cmd != MentionCommand::Help {
        let evaluator = cfg.permissions_evaluator();
        let actor = Actor {
            login: &ev.author,
            author_association: &ev.author_association,
        };
        if !evaluator
            .evaluate(PermissionKey::Review, &gh, &repo, actor)
            .await
        {
            let t = T::new(cfg.language);
            let _ = gh
                .create_issue_comment(&repo, ev.pr_number, t.permission_denied())
                .await;
            let _ = gh.create_reaction(&repo, ev, "eyes").await;
            return Ok(Outcome::Skipped(format!(
                "comment author {} does not have permission for review command",
                ev.author_association
            )));
        }
    }

    // Accepted reaction (spec 09)
    let _ = gh.create_reaction(&repo, ev, "rocket").await;

    let result = match cmd {
        MentionCommand::Review => do_review(cfg, &gh, &repo, ev).await,
        MentionCommand::Explain => do_explain(cfg, &gh, &repo, ev).await,
        MentionCommand::Help => do_help(&gh, &repo, ev, cfg).await,
    };

    let t = T::new(cfg.language);
    match result {
        Ok(msg) => {
            let _ = gh.create_reaction(&repo, ev, "+1").await;
            tracing::info!("✅ {msg}");
            Ok(Outcome::Published { inline_comments: 0 })
        }
        Err(e) => {
            let _ = gh.create_reaction(&repo, ev, "-1").await;
            let _ = gh
                .create_issue_comment(&repo, ev.pr_number, &t.command_failed(&format!("{e:#}")))
                .await;
            Err(e)
        }
    }
}

/// `@hoverstare review`: force a full re-review (spec 09)
async fn do_review(
    cfg: &Config,
    _gh: &GitHubClient,
    repo: &Repo,
    ev: &MentionEvent,
) -> anyhow::Result<String> {
    let args = ReviewArgs {
        pr: Some(ev.pr_number),
        repo: Some(repo.full_name()),
        dry_run: false,
    };
    match orchestrator::run_review(cfg, &args, true).await? {
        Outcome::Published { inline_comments } => Ok(format!(
            "full re-review complete ({inline_comments} inline comments)"
        )),
        Outcome::Skipped(r) => Ok(format!("skipped: {r}")),
        Outcome::AnalysisFailed(r) => Err(anyhow::anyhow!("analysis failed: {r}")),
        Outcome::DryRun => Ok("done".to_string()),
    }
}

/// Finding-thread discussion without `@hoverstare` (issue #13, spec 09): a
/// human collaborator replied inside a HoverStare finding thread. Every gate
/// failure is a silent skip — in particular NO help text is posted.
async fn run_thread_discussion(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &MentionEvent,
) -> anyhow::Result<Outcome> {
    // Must be a review-thread reply (issue_comment events carry no in_reply_to)
    let Some(parent_id) = ev.in_reply_to_id() else {
        return Ok(Outcome::Skipped(
            "comment contains no @hoverstare command".into(),
        ));
    };
    // Cheap gate: the thread's first comment must be a HoverStare finding; a
    // fetch failure is also a skip — no model call either way
    let parent = match gh.get_review_comment(repo, parent_id).await {
        Ok(p) => p,
        Err(e) => {
            return Ok(Outcome::Skipped(format!(
                "parent comment fetch failed: {e}"
            )));
        }
    };
    if !parent.body.contains(crate::state::MARKER_PREFIX) {
        return Ok(Outcome::Skipped(
            "thread does not belong to a hoverstare finding".into(),
        ));
    }
    // Same `review` permission key as the mention commands (spec 12)
    let evaluator = cfg.permissions_evaluator();
    let actor = Actor {
        login: &ev.author,
        author_association: &ev.author_association,
    };
    if !evaluator
        .evaluate(PermissionKey::Review, gh, repo, actor)
        .await
    {
        let _ = gh.create_reaction(repo, ev, "eyes").await;
        return Ok(Outcome::Skipped(format!(
            "comment author {} does not have permission for thread discussion",
            ev.author_association
        )));
    }

    // Accepted reaction (same convention as mention commands)
    let _ = gh.create_reaction(repo, ev, "rocket").await;

    let t = T::new(cfg.language);
    match do_thread_discussion(cfg, gh, repo, ev, parent_id, &parent).await {
        Ok(Some(msg)) => {
            let _ = gh.create_reaction(repo, ev, "+1").await;
            tracing::info!("✅ {msg}");
            Ok(Outcome::Published { inline_comments: 0 })
        }
        // The model judged the reply unrelated to the finding: stay silent
        // (logged as a skip; never masquerade a failure as silence)
        Ok(None) => Ok(Outcome::Skipped("model chose silence ([no-reply])".into())),
        Err(e) => {
            let _ = gh.create_reaction(repo, ev, "-1").await;
            let _ = gh
                .reply_to_review_comment(
                    repo,
                    ev.pr_number,
                    parent_id,
                    &t.command_failed(&format!("{e:#}")),
                )
                .await;
            Err(e)
        }
    }
}

/// The model conversation for a finding-thread reply; `Ok(None)` = stay silent.
async fn do_thread_discussion(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &MentionEvent,
    parent_id: u64,
    parent: &ReviewCommentDetail,
) -> anyhow::Result<Option<String>> {
    // Multi-turn memory (spec 09): recent replies in the thread. A history
    // fetch failure degrades to no history — it never blocks the main flow.
    let history = match gh
        .list_review_thread_comments(repo, ev.pr_number, parent_id)
        .await
    {
        Ok(comments) => render_thread_history(&comments, parent_id, ev.comment_id),
        Err(e) => {
            tracing::warn!("thread history fetch failed, continuing without history: {e}");
            String::new()
        }
    };
    let mut user_prompt = format!("[Review finding]\n{}", thread_context(parent));
    if !history.is_empty() {
        user_prompt.push_str(&format!("\n\n[Thread history]\n{history}"));
    }
    user_prompt.push_str(&format!("\n\n[User reply]\n{}", ev.body));

    let backend = crate::agent::rig_backend::RigBackend::from_config(cfg);
    let shared: Arc<ToolShared> =
        ToolShared::new(cfg.workspace.clone(), "HEAD", cfg.max_tool_calls / 2);
    let req = ReviewRequest {
        system_prompt: format!(
            "You are HoverStare, a code review assistant. A collaborator replied inside the review \
             thread of one of your findings. Discuss in plain, easy-to-understand {lang}: you may \
             acknowledge a false positive, stand by the finding and give evidence, or ask one \
             clarifying question. Do NOT declare the thread resolved unless the user explicitly \
             asks to dismiss it. If the reply is clearly unrelated to the finding (e.g. a short \
             acknowledgement to another collaborator), answer with the exact sentinel {NO_REPLY} \
             and nothing else. Keep the reply under 200 words.",
            lang = cfg.language.display_name()
        ),
        user_prompt,
        tools: ToolRegistry {
            shared: Some(shared),
            ..Default::default()
        },
        budget: Budget {
            max_tool_calls: cfg.max_tool_calls / 2,
            timeout: std::time::Duration::from_secs(180),
        },
        model: cfg.model.clone(),
        temperature: cfg.temp(0.3),
    };
    let run = backend.review(req).await?;
    let text = run.raw_output.trim();
    if is_no_reply(text) {
        return Ok(None);
    }
    if text.is_empty() {
        anyhow::bail!("model returned an empty reply");
    }
    // The reply stays in the original thread (REST replies endpoint, spec 09)
    gh.reply_to_review_comment(repo, ev.pr_number, parent_id, text)
        .await?;
    Ok(Some("thread discussion replied".to_string()))
}

/// Finding context for thread prompts: the first comment body (markers
/// stripped) plus the comment's path/diff_hunk snippet (spec 09)
fn thread_context(detail: &ReviewCommentDetail) -> String {
    let mut ctx = crate::state::strip_markers(&detail.body);
    if !detail.path.is_empty() {
        ctx.push_str(&format!("\n\nFile: `{}`", detail.path));
    }
    if !detail.diff_hunk.is_empty() {
        ctx.push_str(&format!("\n\n```diff\n{}\n```", detail.diff_hunk));
    }
    ctx
}

/// The model stays silent by answering with the exact `[no-reply]` sentinel
fn is_no_reply(text: &str) -> bool {
    text.trim() == NO_REPLY
}

/// Render recent thread replies for the model prompt (spec 09 multi-turn
/// memory): chronological tail window, hidden markers stripped, overall
/// truncation. The root finding (`root_id`, already in [Review finding]) and
/// the triggering comment itself (`exclude_id`, the user message) are not
/// repeated.
fn render_thread_history(
    comments: &[ReviewThreadComment],
    root_id: u64,
    exclude_id: u64,
) -> String {
    let replies: Vec<&ReviewThreadComment> = comments
        .iter()
        .filter(|c| c.id != root_id && c.id != exclude_id)
        .collect();
    let tail = if replies.len() > THREAD_TAIL {
        &replies[replies.len() - THREAD_TAIL..]
    } else {
        &replies
    };
    let mut out = String::new();
    for c in tail {
        out.push_str(&format!(
            "@{}: {}\n\n",
            c.author,
            crate::state::strip_markers(&c.body)
        ));
    }
    let mut out = out.trim_end().to_string();
    if out.len() > THREAD_MAX_BYTES {
        out.truncate(THREAD_MAX_BYTES);
        out.push_str("\n... [history truncated]");
    }
    out
}

/// `@hoverstare explain`: explain a finding (lightweight call, no multi-pass)
async fn do_explain(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &MentionEvent,
) -> anyhow::Result<String> {
    // Context: thread reply -> the comment being replied to (finding body +
    // path/diff_hunk); otherwise the body of the most recent hoverstare review
    let (context, thread_parent) = if let Some(parent_id) = ev.in_reply_to_id() {
        let detail = gh.get_review_comment(repo, parent_id).await?;
        (thread_context(&detail), Some(parent_id))
    } else {
        let reviews = gh.list_reviews(repo, ev.pr_number).await?;
        let body = reviews
            .iter()
            .rev()
            .find(|r| r.body.contains(crate::state::META_MARKER))
            .map(|r| r.body.clone())
            .unwrap_or_else(|| "(no historical review content found)".to_string());
        (body, None)
    };

    let backend = crate::agent::rig_backend::RigBackend::from_config(cfg);
    let text = explain_with_backend(&backend, cfg, &context, &ev.body).await?;
    let body = format!("{}\n\n{text}", T::new(cfg.language).explain_header());
    // With in_reply_to_id the reply stays in the original thread (REST replies
    // endpoint); otherwise fall back to a PR conversation comment (spec 09)
    match thread_parent {
        Some(parent_id) => {
            gh.reply_to_review_comment(repo, ev.pr_number, parent_id, &body)
                .await?;
        }
        None => {
            gh.create_issue_comment(repo, ev.pr_number, &body).await?;
        }
    }
    Ok("explain replied".to_string())
}

/// explain core with an injectable backend (for tests)
async fn explain_with_backend(
    backend: &dyn AgentBackend,
    cfg: &Config,
    context: &str,
    question: &str,
) -> anyhow::Result<String> {
    let shared: Arc<ToolShared> =
        ToolShared::new(cfg.workspace.clone(), "HEAD", cfg.max_tool_calls / 2);
    let req = ReviewRequest {
        system_prompt: format!(
            "You are HoverStare, a code review assistant. The user asks you to explain a review finding. \
             Explain in plain, easy-to-understand {lang}: what the problem is, under what conditions it \
             triggers, what impact it has, and how to fix it. You may quote code snippets. \
             Keep the reply under 300 words.",
            lang = cfg.language.display_name()
        ),
        user_prompt: format!("[Review finding]\n{context}\n\n[User question]\n{question}"),
        tools: ToolRegistry {
            shared: Some(shared),
            ..Default::default()
        },
        budget: Budget {
            max_tool_calls: cfg.max_tool_calls / 2,
            timeout: std::time::Duration::from_secs(180),
        },
        model: cfg.model.clone(),
        temperature: cfg.temp(0.3),
    };
    let run = backend.review(req).await?;
    let text = run.raw_output.trim().to_string();
    if text.is_empty() {
        anyhow::bail!("model returned an empty explanation");
    }
    Ok(text)
}

async fn do_help(
    gh: &GitHubClient,
    repo: &Repo,
    ev: &MentionEvent,
    cfg: &Config,
) -> anyhow::Result<String> {
    gh.create_issue_comment(repo, ev.pr_number, &T::new(cfg.language).help_text())
        .await?;
    Ok("help replied".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        assert_eq!(
            parse_command("@hoverstare review"),
            Some(MentionCommand::Review)
        );
        assert_eq!(
            parse_command("please @hoverstare explain this"),
            Some(MentionCommand::Explain)
        );
        assert_eq!(
            parse_command("@hoverstare help"),
            Some(MentionCommand::Help)
        );
        assert_eq!(
            parse_command("@hoverstare /help"),
            Some(MentionCommand::Help)
        );
        assert_eq!(parse_command("@hoverstare"), Some(MentionCommand::Help));
        assert_eq!(
            parse_command("@hoverstare frobnicate"),
            Some(MentionCommand::Help)
        );
        assert_eq!(parse_command("no command here"), None);
        assert_eq!(parse_command("mentions hoverstare but no @"), None);
    }

    #[test]
    fn ignores_code_blocks() {
        // @hoverstare inside fenced code blocks is ignored (spec 09)
        assert_eq!(parse_command("```\n@hoverstare review\n```"), None);
        // @hoverstare inside inline code is ignored
        assert_eq!(
            parse_command("look at the `@hoverstare review` command"),
            None
        );
        // normal response outside code blocks
        assert_eq!(
            parse_command("```\nsome code\n```\n@hoverstare review"),
            Some(MentionCommand::Review)
        );
    }

    #[test]
    fn no_reply_sentinel() {
        assert!(is_no_reply("[no-reply]"));
        assert!(is_no_reply("  [no-reply]\n"));
        assert!(!is_no_reply("[no-reply] but let me add context"));
        assert!(!is_no_reply("good point, this is a false positive"));
        assert!(!is_no_reply(""));
    }

    #[test]
    fn thread_history_tail_window_markers_and_exclusions() {
        let comments: Vec<ReviewThreadComment> = (1..=30)
            .map(|i| ReviewThreadComment {
                id: i,
                body: format!("msg {i}\n<!-- hoverstare-finding:0123456789abcdef -->"),
                author: format!("u{i}"),
            })
            .collect();
        // root finding (id 1) and the triggering comment (id 30) are excluded
        let out = render_thread_history(&comments, 1, 30);
        assert!(!out.contains("msg 1\n"));
        assert!(!out.contains("msg 30"));
        assert!(!out.contains("hoverstare-finding"));
        // tail window: only the last THREAD_TAIL replies survive (ids 10..=29)
        assert!(!out.contains("msg 9\n"));
        assert!(out.contains("@u10: msg 10"));
        assert!(out.contains("@u29: msg 29"));

        // no history -> empty string (the caller omits the history section)
        assert_eq!(render_thread_history(&[], 1, 30), "");
    }

    #[test]
    fn thread_context_strips_markers_and_adds_anchor() {
        let detail = ReviewCommentDetail {
            body: "🟠 **HIGH**: null deref\n<!-- hoverstare-finding:0123456789abcdef -->".into(),
            path: "src/a.rs".into(),
            diff_hunk: "@@ -1,2 +1,2 @@\n-let x = 1;\n+let x = 2;".into(),
        };
        let ctx = thread_context(&detail);
        assert!(ctx.contains("null deref"));
        assert!(!ctx.contains("hoverstare-finding"));
        assert!(ctx.contains("File: `src/a.rs`"));
        assert!(ctx.contains("```diff"));
        // Missing anchoring info is simply omitted
        let bare = ReviewCommentDetail {
            body: "finding".into(),
            path: String::new(),
            diff_hunk: String::new(),
        };
        assert_eq!(thread_context(&bare), "finding");
    }
}
