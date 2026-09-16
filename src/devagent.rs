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
use crate::devqueue::{
    Idle, ItemKind, ItemState, MergeGate, Outcome, QUEUE_PREFIX, QueueState, RoundRecord,
    checklist, instruction, merge_gate, precheck, round_note, self_trigger, summary_line,
};
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
    /// Commit pushed by this round (artifact gate of the next round).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha: Option<String>,
    /// How this round ended (`ok`/`nochange`/`failed`; spec 11 §6 failure-stop).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub st: Option<String>,
    /// Queue item this round worked on (spec 11 §6 artifact gate).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<u64>,
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
    /// `@hoverstare merge` (PR only); `force` discards the unfinished queue.
    Merge { force: bool },
    /// `@hoverstare queue` (PR only): print the visible queue checklist
    Queue,
    /// `@hoverstare help` or `@hoverstare /help`: print unified help
    Help,
    /// Everything else: discussion (issue) or dev instruction (PR)
    Task(String),
}

pub fn parse_dev_command(body: &str) -> Option<DevCommand> {
    // Humans naturally quote earlier commands in the same comment, so the
    // *newest* mention wins (issue #15). If that trailing mention carries no
    // command text (a bare `@hoverstare`), fall back to the previous one.
    let stripped = strip_code_blocks(body);
    let after = last_mention_tail(&stripped)?.trim();
    let first = after.split_whitespace().next().unwrap_or("").to_lowercase();
    Some(match first.as_str() {
        "go" => DevCommand::Go,
        "merge" => DevCommand::Merge {
            force: after
                .split_whitespace()
                .skip(1)
                .any(|w| w.eq_ignore_ascii_case("force")),
        },
        "queue" => DevCommand::Queue,
        "help" | "/help" => DevCommand::Help,
        _ => DevCommand::Task(after.to_string()),
    })
}

/// Text that follows the last `@hoverstare` mention in `stripped` (up to the
/// next mention). A trailing mention with no command text falls back to the
/// most recent mention that has one; `None` when there is no mention at all.
fn last_mention_tail(stripped: &str) -> Option<&str> {
    const MARKER: &str = "@hoverstare";
    let mut last_any: Option<&str> = None;
    let mut last_with_command: Option<&str> = None;
    let mut rest = stripped;
    while let Some(idx) = rest.find(MARKER) {
        let after = &rest[idx + MARKER.len()..];
        let tail = match after.find(MARKER) {
            Some(next) => &after[..next],
            None => after,
        };
        last_any = Some(tail);
        if !tail.trim().is_empty() {
            last_with_command = Some(tail);
        }
        rest = after;
    }
    last_with_command.or(last_any)
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
            DevCommand::Merge { .. } => PermissionKey::Merge,
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
        DevCommand::Merge { .. } => Ok("ignored: merge is only valid on PRs".to_string()),
        DevCommand::Queue => Ok("invalid: queue is only valid on PRs".to_string()),
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
        sha: None,
        st: None,
        task: None,
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
    // Revision this flow starts from. Recorded in the pull request body so every
    // later round of the same flow builds the same source instead of whatever
    // master happens to be, which also lets the pinned build reuse its cache.
    let flow_revision = git.run(&["rev-parse", "HEAD"]).await?;
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
        commit_identity: commit_identity_for(cfg, &ev.author),
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
        "{}\n\nCloses #{}\n\n---\n由 HoverStare 实现。后续调整请在 PR 评论区 `@hoverstare` 下达。\n\n\
         <!-- hoverstare-pin: {} -->",
        summary,
        ev.number,
        flow_revision.trim()
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
        sha: None,
        st: None,
        task: None,
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
        DevCommand::Merge { force } => merge_flow(cfg, gh, repo, ev, &pr, force).await,
        DevCommand::Queue => queue_flow(gh, repo, ev).await,
        DevCommand::Help => unreachable!("handled in run_event"),
        // A continuation pulls the queue's next item (spec 11 §6); the bot's
        // `@hoverstare continue` self-trigger parses as a task, so route it here
        // rather than enqueueing it as a human instruction.
        DevCommand::Task(_) if ev.is_self_trigger() => {
            pr_dev_round(cfg, gh, repo, ev, &pr, RoundTrigger::Continue).await
        }
        DevCommand::Go => pr_dev_round(cfg, gh, repo, ev, &pr, RoundTrigger::Continue).await,
        DevCommand::Task(text) => {
            pr_dev_round(cfg, gh, repo, ev, &pr, RoundTrigger::Instruction(&text)).await
        }
    }
}

/// What starts a dev round (spec 11 §6 queue contract).
enum RoundTrigger<'a> {
    /// A human instruction comment: enqueue it (idempotent by comment id), then
    /// run the queue's next item.
    Instruction(&'a str),
    /// A continuation — the bot's `@hoverstare continue` self-trigger or a human
    /// `@hoverstare go`: run the queue's next item.
    Continue,
}

/// The queue's next item whose source comment still carries an instruction
/// (spec 11 §6 dequeue order).
fn dequeue(queue: &QueueState, comments: &[IssueComment]) -> Option<(u64, String)> {
    queue
        .ordered()
        .into_iter()
        .find_map(|item| instruction(comments, item.src).map(|text| (item.src, text)))
}

/// One dev round on the PR branch: pick the queued task, sync to remote head,
/// develop, push, report (spec 11 §6).
async fn pr_dev_round(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    pr: &PullRequest,
    trigger: RoundTrigger<'_>,
) -> anyhow::Result<String> {
    let comments = gh.list_issue_comments(repo, ev.number).await?;
    let latest = latest_marker(&comments);
    let round = latest.as_ref().map(|m| m.r).unwrap_or(0) + 1;
    // Queue guard (spec 11 §6): the marker as the queue sees it. A self-trigger
    // comment carries the round it just finished, so it claims the next one; a
    // human instruction claims nothing and always gets to run.
    let record = latest.map(|m| RoundRecord {
        r: m.r,
        st: m.st.as_deref().and_then(Outcome::parse),
        sha: m.sha,
        task: m.task,
    });
    match precheck(round, MAX_PR_ROUNDS, ev.claimed_round(), record.as_ref()) {
        // A newer run already completed this round: no comment, no commit, no
        // self-trigger — the run leaves the PR exactly as it found it.
        Some(Idle::StaleClaim) => return Ok("stale claim: nothing written".into()),
        // The round cap is the fuse for the automatic chain; a maintainer's
        // request may still start a round (spec 11 §6).
        Some(_) if !round_allowed(round, !ev.is_self_trigger()) => {
            gh.create_issue_comment(
                repo,
                ev.number,
                &format!("已达最大开发轮次（{MAX_PR_ROUNDS}），请人类接管。"),
            )
            .await?;
            return Ok("round cap reached".into());
        }
        Some(_) | None => {}
    }

    // Queue (spec 11 §6): a human instruction joins the queue (idempotent by
    // comment id); a continuation pulls the queue's next open item. A review
    // body has no comment id, so it runs directly, outside the queue.
    let mut queue = QueueState::latest(&comments).unwrap_or_default();
    let mut direct: Option<String> = None;
    if let RoundTrigger::Instruction(text) = trigger {
        match ev.comment_id {
            Some(id) => {
                if let Err(err) = queue.enqueue(id, ItemKind::Human, text) {
                    // A refused instruction is reported loudly, never dropped.
                    gh.create_issue_comment(repo, ev.number, &err.message())
                        .await?;
                    return Ok("instruction not queued".into());
                }
            }
            None => direct = Some(text.to_string()),
        }
    }
    let (src, instruction_text) = match direct.as_deref().filter(|t| !t.trim().is_empty()) {
        Some(text) => (0u64, text.to_string()),
        None => match dequeue(&queue, &comments) {
            Some((src, text)) => (src, text),
            None => {
                let msg = if queue.open_count() == 0 {
                    Idle::EmptyQueue.message()
                } else {
                    "队列中的任务源评论已不可见，无法执行；请重新下达指令。"
                };
                gh.create_issue_comment(repo, ev.number, msg).await?;
                return Ok("nothing queued to run".into());
            }
        },
    };

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

    // Artifact gate: when the previous round recorded the commit it pushed, that
    // commit must still be on the branch. A rewritten (force-pushed) branch would
    // make this round stack work on a history that no longer exists, so stop here.
    // Checked before the base merge: a rewritten history is not something to
    // build on, and the merge would move HEAD and hide the problem.
    if let Some(sha) = record.as_ref().and_then(|r| r.sha.as_deref())
        && !git.is_ancestor(sha, "HEAD").await?
    {
        gh.create_issue_comment(repo, ev.number, Idle::PreviousNotOnBranch.message())
            .await?;
        return Ok(format!("previous commit {sha} is not on {branch}"));
    }
    // Failure-stop (spec 11 §6): a self-driving continuation only starts when the
    // previous queued task actually landed. Human instructions always run.
    if ev.claimed_round().is_some()
        && let Some(rec) = &record
        && rec.task.is_some()
        && rec.st != Some(Outcome::Ok)
    {
        gh.create_issue_comment(repo, ev.number, Idle::PreviousFailed.message())
            .await?;
        return Ok("previous round did not land".into());
    }
    // Mark the item in flight (spec 11 §6) so a concurrent `@hoverstare queue`
    // shows what is running; the final state lands with the report below.
    if src != 0 {
        queue.set_state(src, ItemState::Running);
    }

    // Merge the base branch before developing. A branch that drifted behind its
    // base turns the pull request conflicted, and GitHub then runs no
    // `pull_request` checks at all: the round would develop with no CI and no
    // failure to read. A clean merge is pushed immediately, so the pull request
    // stays mergeable even when this round changes nothing else.
    let base = &pr.base.ref_name;
    git.fetch("devpush", &format!("{base}:refs/remotes/devpush/{base}"))
        .await?;
    let head_before = git.run(&["rev-parse", "HEAD"]).await?;
    match git
        .merge_ref(
            &format!("refs/remotes/devpush/{base}"),
            &commit_identity_for(cfg, &ev.author),
        )
        .await
    {
        Ok(()) => {
            if git.run(&["rev-parse", "HEAD"]).await? != head_before {
                git.push("devpush", branch).await?;
                tracing::info!("dev round: merged {base} into {branch} before developing");
            }
        }
        Err(crate::git::GitError::Conflict(detail)) => {
            gh.create_issue_comment(
                repo,
                ev.number,
                &format!(
                    "⚠️ 分支 `{branch}` 与 `{base}` 冲突，本轮未开发。\n\n\
                     冲突需要人工解决（bot 不做 rebase）：请把 `{base}` 合进分支或改掉冲突文件，\
                     然后重新下达指令。\n\n<details><summary>git 输出</summary>\n\n```\n{}\n```\n</details>",
                    detail.chars().take(1500).collect::<String>()
                ),
            )
            .await?;
            return Ok("conflict with base; round skipped".into());
        }
        Err(error) => return Err(error.into()),
    }

    let task = format!(
        "You are developing on the branch `{branch}` of PR #{}.\n\n[Instruction from the PR discussion]\n{}\n\n\
         Implement the instruction now, staying minimal and focused.",
        ev.number, instruction_text
    );
    let backend = RigBackend::from_config(cfg);
    let outcome = develop::run(develop::DevelopRequest {
        workspace: &cfg.workspace,
        task: &task,
        commit_hint: &instruction_text,
        dry_run: false,
        backend: &backend,
        model: &cfg.model,
        temperature: cfg.temp(0.0),
        budget_calls: cfg.max_tool_calls.max(develop::DEFAULT_BUDGET_CALLS),
        commit_identity: commit_identity_for(cfg, &ev.author),
    })
    .await?;
    let ok = outcome.commit.is_some();
    if ok {
        git.push("devpush", branch).await?;
    }
    let outcome_st = if ok { Outcome::Ok } else { Outcome::Nochange };
    // The round's result lands on the queue (spec 11 §6): a landed item is done,
    // a round that produced nothing is failed — and a failure never releases the
    // next round. The final queue marker rides with the report (append-only).
    if src != 0 {
        queue.set_state(
            src,
            if ok {
                ItemState::Done
            } else {
                ItemState::Failed
            },
        );
    }
    let marker = DevMarker {
        m: "impl".into(),
        r: round,
        pr: Some(ev.number),
        sha: outcome.commit.clone(),
        st: Some(outcome_st.as_str().to_string()),
        task: (src != 0).then_some(src),
    };
    let head = if ok {
        "本轮改动已提交并推送："
    } else {
        "本轮无代码改动。"
    };
    // Queue status rides with the report so a human sees what is still pending
    // or in flight without opening the queue command (spec 11 §6).
    let queue_note = if src != 0 {
        // The item left `Running` above, so the report names what this round
        // executed instead of only counting what is left.
        round_note(&queue, &comments, src)
    } else if queue.open_count() == 0 {
        "队列已空".to_string()
    } else {
        summary_line(&queue, &comments)
    };
    // Build provenance (spec 11 §6): the workflow says which revision the
    // running binary came from, and the report repeats it aloud so a human can
    // verify the flow-level pin without opening the run log. Absent locally.
    let source_line = build_source_from_env()
        .map(|line| format!("\n\n{line}"))
        .unwrap_or_default();
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!(
            "{head}\n\n{}\n\n{queue_note}{source_line}\n\n{}\n\n{}",
            crate::sanitize::model_text(&outcome.summary),
            marker_text(&marker),
            queue.render()
        ),
    )
    .await?;

    // Self-trigger the next queued round (spec 11 §6): only progress that landed
    // pulls the next item, only while the fuse allows and only while work remains.
    // An empty queue never self-triggers, so the chain ends when the queue drains.
    // The comment rides with this round's marker (first line stays the command),
    // so the next run knows which round it is claiming.
    if self_trigger(round, MAX_PR_ROUNDS, outcome_st, &queue) {
        gh.create_issue_comment(
            repo,
            ev.number,
            &format!("@hoverstare continue\n\n{}", marker_text(&marker)),
        )
        .await?;
        return Ok(format!(
            "round {round} done; self-triggered round {}",
            round + 1
        ));
    }
    // Say so when the fuse, not the queue, is what stopped the chain: otherwise
    // the thread just goes quiet and a reader cannot tell why.
    if ok && round >= MAX_PR_ROUNDS && queue.open_count() > 0 {
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

/// `@hoverstare queue`: paste the visible queue status (counts + checklist) and
/// carry the state forward as a new marker so the append-only chain stays
/// consistent.
async fn queue_flow(gh: &GitHubClient, repo: &Repo, ev: &DevEvent) -> anyhow::Result<String> {
    let comments = gh.list_issue_comments(repo, ev.number).await?;
    let queue = QueueState::latest(&comments).unwrap_or_default();
    let body = if queue.items.is_empty() {
        "队列已空".to_string()
    } else {
        format!(
            "{}\n\n{}",
            summary_line(&queue, &comments),
            checklist(&queue, &comments)
        )
    };
    gh.create_issue_comment(repo, ev.number, &format!("{body}\n\n{}", queue.render()))
        .await?;
    Ok(format!("queue reported ({} item(s))", queue.items.len()))
}

/// `@hoverstare merge`: gate on open + mergeable + checks green, then squash.
async fn merge_flow(
    cfg: &Config,
    gh: &GitHubClient,
    repo: &Repo,
    ev: &DevEvent,
    pr: &PullRequest,
    force: bool,
) -> anyhow::Result<String> {
    if pr.state.as_deref() != Some("open") {
        gh.create_issue_comment(repo, ev.number, "PR 未处于打开状态，无法合并。")
            .await?;
        return Ok("PR is not open".into());
    }
    // Queue gate (spec 11 §6): never merge while queued work is unfinished —
    // those instructions would be lost. Paste the outstanding items verbatim
    // (`checklist`) so the refusal says what is left, not just that something is.
    // A human `force` overrides the refusal, but the discard is reported aloud.
    let comments = gh.list_issue_comments(repo, ev.number).await?;
    let mut queue = QueueState::latest(&comments).unwrap_or_default();
    let dropped = match merge_gate(&queue, force) {
        MergeGate::Clear => 0,
        MergeGate::Blocked => {
            gh.create_issue_comment(
                repo,
                ev.number,
                &format!(
                    "队列仍有未完成项，拒绝合并；请先执行或清理队列\
                     （确认丢弃可回复 `@hoverstare merge force`）：\n\n{}",
                    checklist(&queue, &comments)
                ),
            )
            .await?;
            return Ok("refused: queue has unfinished items".into());
        }
        MergeGate::Forced { dropped } => dropped,
    };
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
    // The squash commit carries the same identity contract (spec 11 §3.3): in
    // coauthor mode credit hoverstare[bot] with a trailer; bot/author leave the
    // default message untouched. (GitHub sets the squash author from the token,
    // which is the push/merge identity, not the commit author.)
    let identity = commit_identity_for(cfg, &ev.author);
    let sha = write_gh
        .merge_pull_request(repo, ev.number, identity.trailer.as_deref())
        .await?;
    // Delete the merged source branch (spec 11 §6); failure only warns.
    let branch_note = match write_gh.delete_branch(repo, &pr.head.ref_name).await {
        Ok(()) => format!("，源分支 `{}` 已删除", pr.head.ref_name),
        Err(e) => format!("（警告：源分支删除失败：{e}）"),
    };
    // A forced merge throws the guarded instructions away: record them as dropped
    // (append-only marker) so a later read no longer sees them as pending, and
    // state the count in the confirmation so the discard is never silent.
    let drop_note = if dropped > 0 {
        let srcs: Vec<u64> = queue.outstanding().iter().map(|i| i.src).collect();
        for src in srcs {
            queue.set_state(src, ItemState::Dropped);
        }
        format!("\n\n已丢弃 {dropped} 条未完成项。\n\n{}", queue.render())
    } else {
        String::new()
    };
    gh.create_issue_comment(
        repo,
        ev.number,
        &format!("✅ 已合并（squash）：`{sha}`{branch_note}{drop_note}"),
    )
    .await?;
    Ok(format!(
        "merged: {sha}; dropped {dropped} queued item(s); branch deleted: {}",
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
        // Skip the hidden dev/queue markers to keep the context clean.
        let body = body
            .lines()
            .filter(|l| {
                let l = l.trim_start();
                !l.starts_with(MARKER_PREFIX) && !l.starts_with(QUEUE_PREFIX)
            })
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

/// Commit identity for one develop round (spec 11 §3.3): the trigger's login
/// with the configured mode. A missing trigger degrades to the bot identity.
fn commit_identity_for(cfg: &Config, trigger: &str) -> crate::git::CommitAuthor {
    crate::git::resolve_commit_identity(
        cfg.commit_identity,
        Some(trigger),
        cfg.commit_author.as_deref(),
    )
}

/// Human wording for a build source the workflow can report (spec 11 §6).
fn build_source_word(mode: &str) -> Option<&'static str> {
    match mode {
        "flow-pin" => Some("流程 pin"),
        "marker" => Some("指令标记"),
        "event" => Some("事件版本"),
        "dispatch" => Some("手动触发"),
        _ => None,
    }
}

/// One line for the round report naming which revision the running binary was
/// built from (spec 11 §6). An unknown mode or a missing sha yields `None`, so
/// nothing untrusted reaches the comment; the line is absent entirely when the
/// workflow exported no provenance (local `develop --task`, older workflows).
fn build_source_line(mode: Option<&str>, sha: Option<&str>) -> Option<String> {
    let word = build_source_word(mode?)?;
    let short: String = sha?.trim().chars().take(8).collect();
    if short.is_empty() {
        return None;
    }
    Some(format!("本轮构建自 {short}（来源：{word}）"))
}

/// Build provenance as exported by the workflow (spec 11 §6).
fn build_source_from_env() -> Option<String> {
    build_source_from(|key| std::env::var(key).ok())
}

/// Same as [`build_source_from_env`], reading through a getter so tests can
/// drive it without touching the process environment.
fn build_source_from(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    build_source_line(
        get("HOVERSTARE_BUILD_MODE").as_deref(),
        get("HOVERSTARE_BUILT_FROM").as_deref(),
    )
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
    fn build_source_line_names_mode_and_short_sha() {
        let sha = "4d407d3712345678901234567890abcdefabcdef";
        let line = build_source_line(Some("flow-pin"), Some(sha)).expect("rendered");
        assert!(line.starts_with("本轮构建自 4d407d37"), "{line}");
        assert!(line.contains("流程 pin"), "{line}");
        // Only the short sha appears, never the whole one.
        assert!(!line.contains(sha), "{line}");
        // Every source word the workflow can emit renders in plain language.
        for (mode, word) in [
            ("flow-pin", "流程 pin"),
            ("marker", "指令标记"),
            ("event", "事件版本"),
            ("dispatch", "手动触发"),
        ] {
            let rendered = build_source_line(Some(mode), Some(sha)).unwrap();
            assert!(rendered.contains(word), "{mode} -> {rendered}");
        }
        // Unknown mode or missing provenance renders nothing, so an old workflow
        // and a local run leave the report unchanged.
        assert_eq!(build_source_line(Some("nope"), Some(sha)), None);
        assert_eq!(build_source_line(None, Some(sha)), None);
        assert_eq!(build_source_line(Some("flow-pin"), None), None);
        assert_eq!(build_source_line(Some("event"), Some("   ")), None);
        // The env-reading path agrees.
        let get = |key: &str| match key {
            "HOVERSTARE_BUILD_MODE" => Some("marker".to_string()),
            "HOVERSTARE_BUILT_FROM" => Some(sha.to_string()),
            _ => None,
        };
        let line = build_source_from(get).expect("rendered");
        assert!(line.contains("指令标记"), "{line}");
        assert_eq!(build_source_from(|_| None), None);
    }

    #[test]
    fn parses_commands() {
        assert_eq!(parse_dev_command("@hoverstare go"), Some(DevCommand::Go));
        assert_eq!(
            parse_dev_command("@hoverstare merge"),
            Some(DevCommand::Merge { force: false })
        );
        assert_eq!(
            parse_dev_command("@hoverstare merge force"),
            Some(DevCommand::Merge { force: true })
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
    fn parses_queue_command_case_and_spacing() {
        // Command words are case-insensitive and tolerate extra whitespace.
        assert_eq!(
            parse_dev_command("@hoverstare queue"),
            Some(DevCommand::Queue)
        );
        assert_eq!(
            parse_dev_command("@hoverstare QUEUE"),
            Some(DevCommand::Queue)
        );
        assert_eq!(
            parse_dev_command("@hoverstare   Queue  "),
            Some(DevCommand::Queue)
        );
        assert_eq!(
            parse_dev_command("@hoverstare MERGE   FORCE"),
            Some(DevCommand::Merge { force: true })
        );
        assert_eq!(
            parse_dev_command("@hoverstare  merge"),
            Some(DevCommand::Merge { force: false })
        );
    }

    #[test]
    fn uses_last_mention_with_fallback() {
        // A command quoted in inline code followed by the real command: the
        // trailing (last) mention is the one that is parsed.
        assert_eq!(
            parse_dev_command("之前我用了 `@hoverstare go`，现在 @hoverstare merge"),
            Some(DevCommand::Merge { force: false })
        );
        // Two free-standing mentions -> the newest instruction wins.
        assert_eq!(
            parse_dev_command("@hoverstare go ... actually @hoverstare add tests"),
            Some(DevCommand::Task("add tests".into()))
        );
        // A bare trailing mention carries no command -> fall back to the
        // previous mention that does.
        assert_eq!(
            parse_dev_command("@hoverstare merge\n\n@hoverstare"),
            Some(DevCommand::Merge { force: false })
        );
    }

    #[test]
    fn unpaired_backtick_does_not_hide_the_command() {
        // Odd number of backticks: the stray one must not swallow the rest.
        let body = "说明里有一个落单的反引号 ` 然后 @hoverstare go";
        assert_eq!(parse_dev_command(body), Some(DevCommand::Go));
        // The text after the stray backtick is preserved, not discarded.
        assert!(strip_code_blocks(body).contains("然后 @hoverstare go"));

        // No command anywhere -> None.
        assert_eq!(parse_dev_command("just a normal comment"), None);
        // A mention inside a fenced block is not a command.
        assert_eq!(
            parse_dev_command("示例：\n```\n@hoverstare go\n```\n没有命令"),
            None
        );
    }

    #[test]
    fn marker_roundtrip_and_latest() {
        let m = DevMarker {
            m: "plan".into(),
            r: 2,
            pr: None,
            sha: Some("c0ffee".into()),
            st: None,
            task: None,
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
                    sha: None,
                    st: None,
                    task: None,
                }),
            ),
            comment(2, "plain reply"),
            comment(
                3,
                &marker_text(&DevMarker {
                    m: "impl".into(),
                    r: 0,
                    pr: Some(7),
                    sha: None,
                    st: None,
                    task: None,
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
                    pr: None,
                    sha: None,
                    st: None,
                    task: None,
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

    #[test]
    fn thread_render_strips_queue_markers() {
        let comments = vec![comment(
            7,
            &format!("please do it\n\n{}", QueueState::new().render()),
        )];
        let out = render_thread("T", "body", &comments);
        assert!(out.contains("please do it"));
        assert!(!out.contains(QUEUE_PREFIX), "queue marker stripped");
    }
}
