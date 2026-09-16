//! GitHub Actions event parsing (spec 01/03)

use anyhow::{Context as _, bail};
use serde::Deserialize;

use crate::cli::ReviewArgs;

/// PR targeting info
#[derive(Debug, Clone)]
pub struct PrRef {
    pub repo: String, // "owner/repo"
    pub number: u64,
}

/// `@hoverstare` comment event (spec 09): unifies issue_comment and
/// pull_request_review_comment
#[derive(Debug, Clone)]
pub struct MentionEvent {
    pub repo: String,
    pub pr_number: u64,
    pub comment_id: u64,
    pub body: String,
    pub author_association: String,
    /// Login of the comment author (used by the permissions system, spec 12)
    pub author: String,
    /// Comment author type ("User" | "Bot"); bot authors never trigger (spec 09)
    pub user_type: String,
    /// In pull_request_review_comment events, the id of the comment being
    /// replied to (None for issue_comment)
    pub in_reply_to: Option<u64>,
}

impl MentionEvent {
    /// Permission: only repo collaborators may trigger (spec 09)
    pub fn is_collaborator(&self) -> bool {
        matches!(
            self.author_association.as_str(),
            "OWNER" | "MEMBER" | "COLLABORATOR"
        )
    }

    /// Bot authors never trigger (spec 09), including comments that contain
    /// @hoverstare (self-excitation guard)
    pub fn is_bot(&self) -> bool {
        self.user_type.eq_ignore_ascii_case("bot")
    }

    pub fn in_reply_to_id(&self) -> Option<u64> {
        self.in_reply_to
    }
}

#[derive(Debug, Deserialize)]
struct MentionPayload {
    issue: Option<MentionIssue>,
    pull_request: Option<EventPr>,
    comment: Option<MentionComment>,
}

#[derive(Debug, Deserialize)]
struct MentionIssue {
    number: u64,
    /// If present, this is a comment on a PR (pure issues are not handled in v1, spec 09)
    pull_request: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct MentionComment {
    id: u64,
    body: String,
    author_association: String,
    in_reply_to_id: Option<u64>,
    /// Comment author login (develop-mode self-trigger gate, spec 11 §6)
    #[serde(default)]
    user: Option<EventUser>,
}

#[derive(Debug, Deserialize)]
struct EventUser {
    login: String,
    /// "User" | "Bot" (mention-mode bot-author gate, spec 09)
    #[serde(rename = "type", default)]
    user_type: Option<String>,
}

/// Parse a mention event from the GitHub Actions environment (issue_comment / pull_request_review_comment)
pub fn resolve_mention() -> anyhow::Result<Option<MentionEvent>> {
    let Ok(path) = std::env::var("GITHUB_EVENT_PATH") else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read GITHUB_EVENT_PATH ({path})"))?;
    let payload: MentionPayload = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse event payload ({path})"))?;
    let Some(comment) = payload.comment else {
        return Ok(None);
    };
    let repo = std::env::var("GITHUB_REPOSITORY").context("missing GITHUB_REPOSITORY")?;

    // issue_comment (PR conversation) or pull_request_review_comment (thread)
    if let Some(issue) = payload.issue {
        if issue.pull_request.is_none() {
            return Ok(None); // pure issue, not handled in v1
        }
        return Ok(Some(MentionEvent {
            repo,
            pr_number: issue.number,
            comment_id: comment.id,
            body: comment.body,
            author_association: comment.author_association,
            author: comment
                .user
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_default(),
            user_type: comment
                .user
                .as_ref()
                .and_then(|u| u.user_type.clone())
                .unwrap_or_else(|| "User".to_string()),
            in_reply_to: None,
        }));
    }
    if let Some(pr) = payload.pull_request {
        return Ok(Some(MentionEvent {
            repo,
            pr_number: pr.number,
            comment_id: comment.id,
            body: comment.body,
            author_association: comment.author_association,
            author: comment
                .user
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_default(),
            user_type: comment
                .user
                .as_ref()
                .and_then(|u| u.user_type.clone())
                .unwrap_or_else(|| "User".to_string()),
            in_reply_to: comment.in_reply_to_id,
        }));
    }
    Ok(None)
}

#[derive(Debug, Deserialize)]
struct EventPayload {
    pull_request: Option<EventPr>,
}

#[derive(Debug, Deserialize)]
struct EventPr {
    number: u64,
}

/// Read PR targeting from the GitHub Actions environment
fn from_event() -> anyhow::Result<Option<PrRef>> {
    let Ok(path) = std::env::var("GITHUB_EVENT_PATH") else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read GITHUB_EVENT_PATH ({path})"))?;
    let payload: EventPayload = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse event payload ({path})"))?;
    let Some(pr) = payload.pull_request else {
        return Ok(None);
    };
    let repo = std::env::var("GITHUB_REPOSITORY")
        .context("event contains pull_request but GITHUB_REPOSITORY is missing")?;
    Ok(Some(PrRef {
        repo,
        number: pr.number,
    }))
}

/// Resolve the target PR for this run: CLI flags first, then the event
pub fn resolve_pr(args: &ReviewArgs) -> anyhow::Result<PrRef> {
    match (&args.repo, args.pr) {
        (Some(repo), Some(number)) => Ok(PrRef {
            repo: repo.clone(),
            number,
        }),
        (Some(_), None) | (None, Some(_)) => {
            bail!("--repo and --pr must be provided together")
        }
        (None, None) => from_event()?
            .context("cannot determine the target PR: --repo/--pr not provided and not running in a pull_request event environment"),
    }
}

// ---------------------------------------------------------------------------
// Develop mode events (spec 11 §3.2)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DevKind {
    IssueOpened,
    IssueComment,
    ReviewComment,
    Review,
}

/// A develop-mode trigger: issue opened, or a comment anywhere (issue / PR
/// conversation / review thread / review body).
#[derive(Debug, Clone)]
pub struct DevEvent {
    pub repo: String,
    pub number: u64,
    pub is_pr: bool,
    pub kind: DevKind,
    /// Issue title (IssueOpened only).
    pub title: Option<String>,
    pub body: String,
    pub comment_id: Option<u64>,
    pub author_association: String,
    pub in_reply_to: Option<u64>,
    /// Author login of the triggering comment/issue/review.
    pub author: String,
}

impl DevEvent {
    pub fn is_collaborator(&self) -> bool {
        matches!(
            self.author_association.as_str(),
            "OWNER" | "MEMBER" | "COLLABORATOR"
        )
    }

    /// Self-trigger gate (spec 11 §6): the bot's own `@hoverstare continue`
    /// is allowed even though the bot is not a collaborator; everything else
    /// from non-collaborators is ignored.
    ///
    /// Compared on the **first line** so the self-driving queue can ride along
    /// in the hidden markers below it (the workflow's `startsWith` gate stays
    /// satisfied that way).
    pub fn is_self_trigger(&self) -> bool {
        self.author == "hoverstare[bot]"
            && self.body.lines().next().map(str::trim) == Some("@hoverstare continue")
    }

    /// Rounds claimed by this comment (self-trigger only): the marker records
    /// the round the bot just finished, so it claims the next one.
    pub fn claimed_round(&self) -> Option<u32> {
        if !self.is_self_trigger() {
            return None;
        }
        crate::devagent::parse_marker(&self.body).map(|m| m.r + 1)
    }
}

#[derive(Debug, Deserialize)]
struct DevPayload {
    action: Option<String>,
    issue: Option<DevIssue>,
    pull_request: Option<EventPr>,
    comment: Option<MentionComment>,
    review: Option<DevReview>,
}

#[derive(Debug, Deserialize)]
struct DevIssue {
    number: u64,
    title: Option<String>,
    body: Option<String>,
    author_association: Option<String>,
    pull_request: Option<serde_json::Value>,
    #[serde(default)]
    user: Option<EventUser>,
}

#[derive(Debug, Deserialize)]
struct DevReview {
    body: Option<String>,
    author_association: String,
    #[serde(default)]
    user: Option<EventUser>,
}

/// Parse a develop-mode trigger from the GitHub Actions environment.
pub fn resolve_dev_event() -> anyhow::Result<Option<DevEvent>> {
    let Ok(path) = std::env::var("GITHUB_EVENT_PATH") else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read GITHUB_EVENT_PATH ({path})"))?;
    let payload: DevPayload = serde_json::from_str(&text)
        .with_context(|| format!("failed to parse event payload ({path})"))?;
    let repo = std::env::var("GITHUB_REPOSITORY").context("missing GITHUB_REPOSITORY")?;

    // issue_comment.created (works for pure issues and PRs)
    if let (Some(issue), Some(comment)) = (&payload.issue, &payload.comment) {
        return Ok(Some(DevEvent {
            repo,
            number: issue.number,
            is_pr: issue.pull_request.is_some(),
            kind: DevKind::IssueComment,
            title: issue.title.clone(),
            body: comment.body.clone(),
            comment_id: Some(comment.id),
            author_association: comment.author_association.clone(),
            in_reply_to: None,
            author: comment
                .user
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_default(),
        }));
    }
    // issues.opened
    if let (Some(issue), None) = (&payload.issue, &payload.comment) {
        if payload.action.as_deref() != Some("opened") {
            return Ok(None);
        }
        return Ok(Some(DevEvent {
            repo,
            number: issue.number,
            is_pr: false,
            kind: DevKind::IssueOpened,
            title: issue.title.clone(),
            body: issue.body.clone().unwrap_or_default(),
            comment_id: None,
            author_association: issue
                .author_association
                .clone()
                .unwrap_or_else(|| "NONE".to_string()),
            in_reply_to: None,
            author: issue
                .user
                .as_ref()
                .map(|u| u.login.clone())
                .unwrap_or_default(),
        }));
    }
    // pull_request_review_comment.created / pull_request_review.submitted
    if let Some(pr) = &payload.pull_request {
        if let Some(comment) = &payload.comment {
            return Ok(Some(DevEvent {
                repo,
                number: pr.number,
                is_pr: true,
                kind: DevKind::ReviewComment,
                title: None,
                body: comment.body.clone(),
                comment_id: Some(comment.id),
                author_association: comment.author_association.clone(),
                in_reply_to: comment.in_reply_to_id,
                author: comment
                    .user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .unwrap_or_default(),
            }));
        }
        if let Some(review) = &payload.review {
            return Ok(Some(DevEvent {
                repo,
                number: pr.number,
                is_pr: true,
                kind: DevKind::Review,
                title: None,
                body: review.body.clone().unwrap_or_default(),
                comment_id: None,
                author_association: review.author_association.clone(),
                in_reply_to: None,
                author: review
                    .user
                    .as_ref()
                    .map(|u| u.login.clone())
                    .unwrap_or_default(),
            }));
        }
    }
    Ok(None)
}
