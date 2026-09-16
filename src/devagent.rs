//! Develop-mode orchestration (spec 11): issue mainline (discuss/plan → go →
//! PR) and PR mainline (dev rounds on the PR branch + merge command).
//!
//! Stateless rounds: context comes from the event, the comment thread, the
//! hidden `hoverstare-dev` marker in bot comments, and the workspace.

use std::time::Duration;

use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};

use crate::agent::rig_backend::RigBackend;
use crate::agent::tools::ToolShared;
use crate::agent::{AgentBackend, Budget, ReviewRequest, ToolRegistry};
use crate::config::{Actor, Config, PermissionKey};
use crate::develop::{self};
use crate::event::{DevEvent, DevKind};
use crate::git::GitRepo;
use crate::github::{GitHubClient, IssueComment, PullRequest, Repo};
use crate::i18n::T;
use crate::mention::strip_code_blocks;

/// Hidden state marker embedded in bot comments: `<!-- hoverstare-dev:{json} -->`.
pub const MARKER_PREFIX: &str = "<!-- hoverstare-dev:";
/// Self-trigger fuse: max **automatic** dev rounds per PR (spec 11 §6).
///
/// The fuse bounds a chain that drives itself. It must not bound a human: past
/// the cap a person can still ask for rounds one at a time, otherwise a
/// runaway chain would lock the maintainers out of their own pull request.
pub const MAX_PR_ROUNDS: u32 = 10;

/// Whether round `round` may run: human requests always may; the chain may not
/// pass the fuse.
pub fn round_allowed(round: u32, human: bool) -> bool {
    human || round <= MAX_PR_ROUNDS
}
/// Thread context window: first post + last N comments.
const THREAD_TAIL: usize = 30;
const THREAD_MAX_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DevMarker {
    /// "plan" (issue discussion) | "impl" (PR development)
    pub m: String,
    /// rounds completed
    pub r: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<u64>,
}

pub fn marker_text(marker: &DevMarker) -> String {
    format!(
        "{MARKER_PREFIX}{} -->",
        serde_json::to_string(marker).unwrap()
    )
}

pub fn parse_marker(body: &str) -> Option<DevMarker> {
    let start = body.find(MARKER_PREFIX)? + MARKER_PREFIX.len();
    let end = body[start..].find("-->")? + start;
    serde_json::from_str(body[start..end].trim()).ok()
}

fn latest_marker(comments: &[IssueComment]) -> Option<DevMarker> {
    comments
        .iter()
        .rev()
        .filter_map(|c| c.body.as_deref())
        .find_map(parse_marker)
}

/// Develop commands parsed from `@hoverstare ...` (spec 11 §5/§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DevCommand {
    /// `@hoverstare go` (issue: implement the plan)
    Go,
    /// `@hoverstare merge` (PR only)
    Merge,
    /// `@hoverstare help` or `@hoverstare /help`: print unified help
    Help,
    /// Everything else: discussion (issue) or dev instruction (PR)
    Task(String),
}

pub fn parse_dev_command(body: &str) -> Option<DevCommand> {
    let stripped = strip_code_blocks(body);
    let at = stripped.find("@hoverstare")?;
    let after = stripped[at + "@hoverstare".len()..].trim();
    let first = after.split_whitespace().next().unwrap_or("").to_lowercase();
    Some(match first.as_str() {
        "go" => DevCommand::Go,
        "merge" => DevCommand::Merge,
        "help" | "/help" => DevCommand::Help,
        _ => DevCommand::Task(after.to_string()),
    })
}

/// Branch name slug from an issue title (spec 11 §8.3).
pub fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut dash = true; // no leading dash
    for c in title.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !dash {
            out.push('-');
            dash = true;
        }
    }
    let out = out.trim_end_matches('-');
    let out: String = out.chars().take(30).collect();
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "task".to_string()
    } else {
        out.to_string()
    }
}

/// Entry point for develop-mode events (spec 11 §3).
pub async fn run_event(cfg: &Config, ev: &DevEvent) -> anyhow::Result<String> {
    let repo = Repo::parse(&ev.repo).map_err(|e| anyhow::anyhow!("{e}"))?;
    let gh = GitHubClient::new(cfg.github_token.clone())?;
    let Some(cmd) = parse_dev_command(&ev.body) else {
        return Ok("ignored: no @hoverstare command".to_string());
    };
    if cmd == DevCommand::Help {
        // Unified help works everywhere (spec 09): issues and PRs alike.
        gh.create_issue_comment(
            &repo,
            ev.number,
            &crate::i18n::T::new(cfg.language).help_text(),
        )
        .await?;
        return Ok("help replied".to_string());
    }

    // Permission gate (spec 12): help is always allowed; self-trigger is always
    // allowed (spec 11 §6); everything else is checked against the configured key.
    if !ev.is_self_trigger() {
        let key = match cmd {
            DevCommand::Merge => PermissionKey::Merge,
            _ => PermissionKey::Develop,
        };
        let evaluator = cfg.permissions_evaluator();
        let actor = Actor {
            login: &ev.author,
            author_association: &ev.author_association,
        };
        if !evaluator.evaluate(key, &gh, &repo, actor).await {
            let t = T::new(cfg.language);
            let _ = gh
                .create_issue_comment(&repo, ev.number, t.permission_denied())
                .await;
            if let Some(cid) = ev.comment_id {
                let _ = gh
                    .create_reaction_for_comment(&repo, cid, ev.in_reply_to, "eyes")
                    .await;
            }
            return Ok(format!(
                "permission denied: author {} does not meet {:?} requirements",
                ev.author, key
            ));
        }
    }

    match (ev.kind, ev.is_pr) {
        (DevKind::IssueOpened, _) | (DevKind::IssueComment, false) => {
            issue_flow(cfg, &gh, &repo, ev, cmd).await
        }
        _ => pr_flow(cfg, &gh, &repo, ev, cmd).await,
    }
}

// ---------------------------------------------------------------------------
// Issue mainline (spec 11 §5)
// ---------------------------------------------------------------------------

async fn issue_flow(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    cmd: DevCommand,
) -> anyhow::Result<String> {
    let comments = gh.list_issue_comments(repo, ev.number).await?;
    let marker = latest_marker(&comments);
    match cmd {
        DevCommand::Merge => Ok("ignored: merge is only valid on PRs".to_string()),
        DevCommand::Help => unreachable!("handled in run_event"),
        DevCommand::Go => implement_issue(cfg, gh, repo, ev, &comments, marker).await,
        DevCommand::Task(text) => {
            if let Some(m) = &marker
                && m.m == "impl"
            {
                let pr = m.pr.unwrap_or(0);
                gh.create_issue_comment(
                    repo,
                    ev.number,
                    &format!("已在 PR #{pr} 中开发；后续任务请移步 PR 评论区。"),
                )
                .await?;
                return Ok(format!("redirected to PR #{pr}"));
            }
            discuss_round(cfg, gh, repo, ev, &comments, marker, &text).await
        }
    }
}

/// Discussion/plan round: read-only investigation, reply with analysis+plan.
async fn discuss_round(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    comments: &[IssueComment],
    marker: Option<DevMarker>,
    text: &str,
) -> anyhow::Result<String> {
    let round = marker.as_ref().map(|m| m.r).unwrap_or(0) + 1;
    let issue = gh.get_issue(repo, ev.number).await?;
    let thread = render_thread(&issue.title, issue.body.as_deref().unwrap_or(""), comments);
    let meta = gh.get_repo_meta(repo).await?;
    let backend = RigBackend::from_config(cfg);
    let budget = Budget {
        max_tool_calls: cfg.max_tool_calls.max(20),
        timeout: Duration::from_secs(300),
    };
    let shared = ToolShared::new(
        cfg.workspace.clone(),
        &meta.default_branch,
        budget.max_tool_calls,
    );
    let req = ReviewRequest {
        system_prompt: format!(
            "You are HoverStare, an AI developer. The user filed an issue and wants to discuss it \
             before any implementation. Investigate the repository with your read-only tools, then \
             reply in {}: a focused analysis and a concrete plan (which files to change, how, and \
             how to verify). Markdown, under 600 words. End with: 确认后回复 @hoverstare go 开始实现。",
            cfg.language.display_name()
        ),
        user_prompt: format!(
            "[Issue #{} {}]\n{}\n\n[Discussion so far]\n{}\n\n[Latest message]\n{}",
            ev.number,
            issue.title,
            issue.body.unwrap_or_default(),
            thread,
            text
        ),
        tools: ToolRegistry {
            shared: Some(shared),
            ..Default::default()
        },
        budget,
        model: cfg.model.clone(),
        temperature: cfg.temp(0.0),
    };
    let run = backend.review(req).await?;
    let reply = run.raw_output.trim();
    if reply.is_empty() {
        anyhow::bail!("model returned an empty reply");
    }
    let marker = DevMarker {
        m: "plan".into(),
        r: round,
        pr: None,
    };
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!(
            "{}\n\n{}",
            crate::sanitize::model_text(reply),
            marker_text(&marker)
        ),
    )
    .await?;
    Ok(format!("discuss round {round} replied"))
}

/// `@hoverstare go`: implement the agreed plan on a new branch and open a PR.
async fn implement_issue(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    comments: &[IssueComment],
    marker: Option<DevMarker>,
) -> anyhow::Result<String> {
    if let Some(m) = &marker
        && m.m == "impl"
    {
        return Ok(format!("already implemented in PR #{}", m.pr.unwrap_or(0)));
    }
    let issue = gh.get_issue(repo, ev.number).await?;
    let meta = gh.get_repo_meta(repo).await?;
    let title = issue.title.clone();
    let thread = render_thread(&title, issue.body.as_deref().unwrap_or(""), comments);

    let git = GitRepo::open(&cfg.workspace)?;
    let token = dev_token(cfg);
    git.set_remote(
        "devpush",
        &token_remote(token.expose_secret(), &repo.full_name()),
    )
    .await?;
    git.fetch(
        "devpush",
        &format!(
            "{}:refs/remotes/devpush/{}",
            meta.default_branch, meta.default_branch
        ),
    )
    .await?;
    let branch = format!("hoverstare/issue-{}-{}", ev.number, slug(&title));
    // checkout -B is idempotent: a retried `go` after a no-change round still works
    git.checkout_reset(
        &branch,
        &format!("refs/remotes/devpush/{}", meta.default_branch),
    )
    .await?;

    let task = format!(
        "Implement the agreed plan for GitHub issue #{}.\n\n[Issue: {}]\n{}\n\n[Discussion and plan]\n{}\n\n\
         Implement the plan now, staying minimal and focused.",
        ev.number,
        title,
        issue.body.unwrap_or_default(),
        thread
    );
    let backend = RigBackend::from_config(cfg);
    let outcome = develop::run(develop::DevelopRequest {
        workspace: &cfg.workspace,
        task: &task,
        commit_hint: &title,
        dry_run: false,
        backend: &backend,
        model: &cfg.model,
        temperature: cfg.temp(0.0),
        budget_calls: cfg.max_tool_calls.max(develop::DEFAULT_BUDGET_CALLS),
    })
    .await?;
    if outcome.commit.is_none() {
        // Nothing changed: do not open an empty PR.
        tracing::warn!(
            "implement_issue: no changes (budget_exhausted={}); agent summary: {}",
            outcome.budget_exhausted,
            outcome.summary.chars().take(400).collect::<String>()
        );
        gh.create_issue_comment(
            repo,
            ev.number,
            "实现轮没有产生任何改动，未创建 PR。请补充更明确的任务描述。",
        )
        .await?;
        return Ok("no changes; PR not created".into());
    }
    git.push("devpush", &branch).await?;
    // Untrusted model text: markup that failed as a tool call must not become a
    // pull request body, where humans read it and later rounds inherit it.
    let summary = crate::sanitize::model_text(&outcome.summary);
    let pr_body = format!(
        "{}\n\nCloses #{}\n\n---\n由 HoverStare 实现。后续调整请在 PR 评论区 `@hoverstare` 下达。",
        summary, ev.number
    );
    let pr = gh
        .create_pull_request(
            repo,
            &format!("[hoverstare] {title}"),
            &branch,
            &meta.default_branch,
            &pr_body,
        )
        .await?;
    let marker = DevMarker {
        m: "impl".into(),
        r: 0,
        pr: Some(pr.number),
    };
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!("✅ 已创建 PR：{}\n\n{}", pr.html_url, marker_text(&marker)),
    )
    .await?;
    Ok(format!("opened PR #{}", pr.number))
}

// ---------------------------------------------------------------------------
// PR mainline (spec 11 §6)
// ---------------------------------------------------------------------------

async fn pr_flow(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    cmd: DevCommand,
) -> anyhow::Result<String> {
    let pr = gh.get_pull_request(repo, ev.number).await?;
    // Same-repo branches only (spec 11 §2: no fork handling).
    let head_repo = pr
        .head
        .repo
        .as_ref()
        .map(|r| r.full_name.clone())
        .unwrap_or_default();
    if head_repo != repo.full_name() {
        gh.create_issue_comment(
            repo,
            ev.number,
            "仅支持本仓库分支上的开发（PR 来源分支不在本仓库）。",
        )
        .await?;
        return Ok("rejected: PR head branch is not in this repo".into());
    }
    match cmd {
        DevCommand::Merge => merge_flow(cfg, gh, repo, ev, &pr).await,
        DevCommand::Help => unreachable!("handled in run_event"),
        DevCommand::Go => {
            pr_dev_round(
                cfg,
                gh,
                repo,
                ev,
                &pr,
                "continue the current task",
                !ev.is_self_trigger(),
            )
            .await
        }
        DevCommand::Task(text) => pr_dev_round(cfg, gh, repo, ev, &pr, &text, true).await,
    }
}

/// One dev round on the PR branch: sync to remote head, develop, push, report.
async fn pr_dev_round(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    pr: &PullRequest,
    instruction: &str,
    human: bool,
) -> anyhow::Result<String> {
    let comments = gh.list_issue_comments(repo, ev.number).await?;
    let marker = latest_marker(&comments);
    let round = marker.as_ref().map(|m| m.r).unwrap_or(0) + 1;
    if !round_allowed(round, human) {
        gh.create_issue_comment(
            repo,
            ev.number,
            &format!("已达最大开发轮次（{MAX_PR_ROUNDS}），请人类接管。"),
        )
        .await?;
        return Ok("round cap reached".into());
    }

    let git = GitRepo::open(&cfg.workspace)?;
    let token = dev_token(cfg);
    git.set_remote(
        "devpush",
        &token_remote(token.expose_secret(), &repo.full_name()),
    )
    .await?;
    let branch = &pr.head.ref_name;
    git.fetch(
        "devpush",
        &format!("{branch}:refs/remotes/devpush/{branch}"),
    )
    .await?;
    // Sync local branch exactly to the just-fetched remote head — human commits
    // are on the remote, so nothing is ever overwritten (spec 11 §6).
    git.checkout_reset(branch, &format!("refs/remotes/devpush/{branch}"))
        .await?;

    let task = format!(
        "You are developing on the branch `{branch}` of PR #{}.\n\n[Instruction from the PR discussion]\n{}\n\n\
         Implement the instruction now, staying minimal and focused.",
        ev.number, instruction
    );
    let backend = RigBackend::from_config(cfg);
    let outcome = develop::run(develop::DevelopRequest {
        workspace: &cfg.workspace,
        task: &task,
        commit_hint: instruction,
        dry_run: false,
        backend: &backend,
        model: &cfg.model,
        temperature: cfg.temp(0.0),
        budget_calls: cfg.max_tool_calls.max(develop::DEFAULT_BUDGET_CALLS),
    })
    .await?;
    let mut pushed = false;
    if outcome.commit.is_some() {
        git.push("devpush", branch).await?;
        pushed = true;
    }
    let marker = DevMarker {
        m: "impl".into(),
        r: round,
        pr: Some(ev.number),
    };
    let head = if pushed {
        "本轮改动已提交并推送："
    } else {
        "本轮无代码改动。"
    };
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!(
            "{head}\n\n{}\n\n{}",
            crate::sanitize::model_text(&outcome.summary),
            marker_text(&marker)
        ),
    )
    .await?;

    // Self-trigger the next round when the budget cut the loop short (spec 11 §6).
    if outcome.budget_exhausted && round < MAX_PR_ROUNDS {
        gh.create_issue_comment(repo, ev.number, "@hoverstare continue")
            .await?;
        return Ok(format!(
            "round {round} done; budget exhausted → self-triggered round {}",
            round + 1
        ));
    }
    // Say so when the fuse, not the work, is what stopped the chain: otherwise
    // the thread just goes quiet and a reader cannot tell why.
    if outcome.budget_exhausted && round >= MAX_PR_ROUNDS {
        gh.create_issue_comment(
            repo,
            ev.number,
            &format!(
                "已达自动轮次上限（{MAX_PR_ROUNDS}），自动链在此停止；需要继续请人工下达指令。"
            ),
        )
        .await?;
    }
    Ok(format!("round {round} done"))
}

/// `@hoverstare merge`: gate on open + mergeable + checks green, then squash.
async fn merge_flow(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    pr: &PullRequest,
) -> anyhow::Result<String> {
    let _ = cfg;
    if pr.state.as_deref() != Some("open") {
        gh.create_issue_comment(repo, ev.number, "PR 未处于打开状态，无法合并。")
            .await?;
        return Ok("PR is not open".into());
    }
    // `mergeable` is computed lazily by GitHub; refetch once if unknown.
    let mut mergeable = pr.mergeable;
    if mergeable.is_none() {
        tokio::time::sleep(Duration::from_secs(3)).await;
        mergeable = gh.get_pull_request(repo, ev.number).await?.mergeable;
    }
    if mergeable != Some(true) {
        gh.create_issue_comment(repo, ev.number, "PR 存在冲突或暂不可合并，请先处理。")
            .await?;
        return Ok("not mergeable".into());
    }
    let checks = gh.list_check_runs(repo, &pr.head.sha).await?;
    let not_green: Vec<&str> = checks
        .iter()
        .filter(|c| {
            c.status != "completed"
                || !matches!(
                    c.conclusion.as_deref(),
                    Some("success") | Some("neutral") | Some("skipped")
                )
        })
        .map(|c| c.name.as_str())
        .collect();
    if !not_green.is_empty() {
        gh.create_issue_comment(
            repo,
            ev.number,
            &format!("checks 未全部通过（{}），暂不合并。", not_green.join(", ")),
        )
        .await?;
        return Ok(format!("checks not green: {}", not_green.join(", ")));
    }
    // Merge + branch deletion are WRITE operations: use the PAT-class token
    // (merge requires contents: write; the App token has read until upgraded).
    // Comments still go through the identity client (App token).
    let write_gh = GitHubClient::new(Some(dev_token(cfg)))?;
    let sha = write_gh.merge_pull_request(repo, ev.number).await?;
    // Delete the merged source branch (spec 11 §6); failure only warns.
    let branch_note = match write_gh.delete_branch(repo, &pr.head.ref_name).await {
        Ok(()) => format!("，源分支 `{}` 已删除", pr.head.ref_name),
        Err(e) => format!("（警告：源分支删除失败：{e}）"),
    };
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!("✅ 已合并（squash）：`{sha}`{branch_note}"),
    )
    .await?;
    Ok(format!(
        "merged: {sha}; branch deleted: {}",
        pr.head.ref_name
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// First post + last THREAD_TAIL comments, capped (spec 11 §3.1).
fn render_thread(title: &str, body: &str, comments: &[IssueComment]) -> String {
    let mut out = format!("### {title}\n{body}\n");
    let tail = if comments.len() > THREAD_TAIL {
        &comments[comments.len() - THREAD_TAIL..]
    } else {
        comments
    };
    for c in tail {
        let body = c.body.as_deref().unwrap_or("");
        // Skip the hidden markers to keep the context clean.
        let body = body
            .lines()
            .filter(|l| !l.trim_start().starts_with(MARKER_PREFIX))
            .collect::<Vec<_>>()
            .join("\n");
        out.push_str(&format!("\n**@{}:** {}\n", c.user.login, body));
    }
    if out.len() > THREAD_MAX_BYTES {
        out.truncate(THREAD_MAX_BYTES);
        out.push_str("\n... [thread truncated]");
    }
    out
}

/// Token for git push (spec 11 §3.3): HOVERSTARE_DEV_TOKEN > gh_pat >
/// the identity token. PAT-class tokens trigger CI on push; the App token
/// needs `contents: write` to work at all.
fn dev_token(cfg: &Config) -> secrecy::SecretString {
    if let Ok(t) = std::env::var("HOVERSTARE_DEV_TOKEN") {
        return t.into();
    }
    if let Some(pat) = &cfg.gh_pat {
        return pat.clone();
    }
    cfg.github_token.clone().unwrap_or_default()
}

fn token_remote(token: &str, full_name: &str) -> String {
    format!("https://x-access-token:{token}@github.com/{full_name}.git")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::PrUser;

    fn comment(id: u64, body: &str) -> IssueComment {
        IssueComment {
            id,
            body: Some(body.to_string()),
            user: PrUser {
                login: "alice".into(),
            },
        }
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_dev_command("@hoverstare go"), Some(DevCommand::Go));
        assert_eq!(
            parse_dev_command("@hoverstare merge"),
            Some(DevCommand::Merge)
        );
        assert_eq!(
            parse_dev_command("@hoverstare add tests for calc.py"),
            Some(DevCommand::Task("add tests for calc.py".into()))
        );
        assert_eq!(
            parse_dev_command("please @hoverstare fix the flaky test"),
            Some(DevCommand::Task("fix the flaky test".into()))
        );
        assert_eq!(parse_dev_command("no mention"), None);
        assert_eq!(parse_dev_command("```\n@hoverstare go\n```"), None);
        assert_eq!(
            parse_dev_command("@hoverstare help"),
            Some(DevCommand::Help)
        );
        assert_eq!(
            parse_dev_command("@hoverstare /help"),
            Some(DevCommand::Help)
        );
    }

    #[test]
    fn marker_roundtrip_and_latest() {
        let m = DevMarker {
            m: "plan".into(),
            r: 2,
            pr: None,
        };
        let text = marker_text(&m);
        assert_eq!(parse_marker(&format!("reply body\n\n{text}")), Some(m));
        let comments = vec![
            comment(
                1,
                &marker_text(&DevMarker {
                    m: "plan".into(),
                    r: 1,
                    pr: None,
                }),
            ),
            comment(2, "plain reply"),
            comment(
                3,
                &marker_text(&DevMarker {
                    m: "impl".into(),
                    r: 0,
                    pr: Some(7),
                }),
            ),
        ];
        let latest = latest_marker(&comments).unwrap();
        assert_eq!(latest.m, "impl");
        assert_eq!(latest.pr, Some(7));
    }

    #[test]
    fn slug_rules() {
        assert_eq!(slug("Add fibonacci function!"), "add-fibonacci-function");
        assert_eq!(slug("修复 缓存 Bug（紧急）"), "bug");
        assert_eq!(slug(""), "task");
        assert_eq!(slug("a".repeat(100).as_str()), "a".repeat(30));
        assert_eq!(slug("--weird--title--"), "weird-title");
    }

    #[test]
    fn the_fuse_bounds_the_chain_but_never_a_person() {
        // Rounds 1..=10 may run automatically; past that only a human request
        // may start one, so a runaway chain cannot lock maintainers out.
        assert!(round_allowed(1, false));
        assert!(round_allowed(MAX_PR_ROUNDS, false));
        assert!(!round_allowed(MAX_PR_ROUNDS + 1, false));
        assert!(round_allowed(MAX_PR_ROUNDS + 1, true));
        assert!(round_allowed(MAX_PR_ROUNDS + 50, true));
    }

    #[test]
    fn thread_render_strips_markers_and_caps() {
        let mut comments: Vec<IssueComment> =
            (0..40).map(|i| comment(i, &format!("msg {i}"))).collect();
        comments.push(comment(
            99,
            &format!(
                "final\n{}",
                marker_text(&DevMarker {
                    m: "plan".into(),
                    r: 9,
                    pr: None
                })
            ),
        ));
        let out = render_thread("T", "body", &comments);
        assert!(out.contains("### T"));
        // only the last 30 comments are rendered
        assert!(!out.contains("msg 0"));
        assert!(out.contains("msg 39"));
        assert!(!out.contains("hoverstare-dev:"), "markers stripped");
    }
}
