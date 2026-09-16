//! GitHub REST client (spec 02)
//!
//! M1 scope: PR info, diff fetching, review publishing, fallback comments.
//! GraphQL (threads/resolve) and status checks are added in M4.
//! The files API fallback (>300 files) is added in M2.

use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;

const DEFAULT_API: &str = "https://api.github.com";
const MAX_RETRIES: u32 = 3;

#[derive(Debug, thiserror::Error)]
pub enum GitHubError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("api error {status}: {body}")]
    Api { status: u16, body: String },
    #[error("graphql errors: {0}")]
    Graphql(String),
    #[error("rate limited after retries")]
    RateLimited,
}

#[derive(Debug, Clone)]
pub struct Repo {
    pub owner: String,
    pub name: String,
}

impl Repo {
    pub fn parse(full: &str) -> Result<Repo, GitHubError> {
        let (owner, name) = full.split_once('/').ok_or(GitHubError::Api {
            status: 0,
            body: format!("repo must be in owner/repo form, got: {full:?}"),
        })?;
        Ok(Repo {
            owner: owner.to_string(),
            name: name.to_string(),
        })
    }

    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Deserialize)]
pub struct PullRequest {
    #[allow(dead_code)]
    pub number: u64,
    pub head: PrRef,
    /// base branch (reference for show_base_file)
    pub base: PrRef,
    #[serde(default)]
    pub draft: bool,
    pub user: PrUser,
    /// PR author's association with the repo (used by the permissions system, spec 12)
    #[serde(default)]
    pub author_association: Option<String>,
    /// "open" | "closed" (develop mode, spec 11)
    #[serde(default)]
    pub state: Option<String>,
    /// Mergeability flag; GitHub computes it lazily (may be null)
    #[serde(default)]
    pub mergeable: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct PrRef {
    pub sha: String,
    #[serde(rename = "ref")]
    pub ref_name: String,
    /// Present on PR head/base objects; used for the same-repo check (spec 11 §2)
    #[serde(default)]
    pub repo: Option<PrRepo>,
}

#[derive(Debug, Deserialize)]
pub struct PrRepo {
    pub full_name: String,
}

#[derive(Debug, Deserialize)]
pub struct PrUser {
    pub login: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RepoPermission {
    Read = 1,
    Triage = 2,
    Write = 3,
    Maintain = 4,
    Admin = 5,
}

impl RepoPermission {
    pub fn parse(s: &str) -> Option<RepoPermission> {
        match s.to_ascii_lowercase().as_str() {
            "read" => Some(RepoPermission::Read),
            "triage" => Some(RepoPermission::Triage),
            "write" => Some(RepoPermission::Write),
            "maintain" => Some(RepoPermission::Maintain),
            "admin" => Some(RepoPermission::Admin),
            _ => None,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct PrFile {
    pub filename: String,
    #[allow(dead_code)]
    pub status: String,
    /// None for binary files or files too large
    pub patch: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ReviewSummary {
    #[allow(dead_code)]
    pub id: u64,
    #[serde(default)]
    pub body: String,
    #[allow(dead_code)]
    pub commit_id: String,
}

#[derive(Debug)]
pub struct ReviewThread {
    pub id: String,
    pub is_resolved: bool,
    /// databaseId of the first comment (for the REST reply endpoint; FORBIDDEN fallback path)
    pub first_comment_id: Option<u64>,
    pub first_comment_body: String,
}

/// One review-thread comment with its anchoring info (context for the explain
/// command and finding-thread discussions, spec 09)
#[derive(Debug)]
pub struct ReviewCommentDetail {
    pub body: String,
    pub path: String,
    pub diff_hunk: String,
}

/// One reply inside a review thread (multi-turn history for finding-thread
/// discussions, spec 09)
#[derive(Debug)]
pub struct ReviewThreadComment {
    pub id: u64,
    pub body: String,
    pub author: String,
}

pub struct NewStatus {
    pub context: &'static str,
    pub state: StatusState,
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusState {
    Success,
    Failure,
    Error,
}

impl StatusState {
    pub fn as_str(self) -> &'static str {
        match self {
            StatusState::Success => "success",
            StatusState::Failure => "failure",
            StatusState::Error => "error",
        }
    }
}

pub struct NewInlineComment {
    pub path: String,
    pub line: u64,
    pub body: String,
}

pub struct NewReview {
    pub commit_id: String,
    pub body: String,
    pub comments: Vec<NewInlineComment>,
}

#[derive(Clone)]
pub struct GitHubClient {
    http: reqwest::Client,
    token: Option<SecretString>,
    api: String,
    retry_backoff_base: std::time::Duration,
}

impl GitHubClient {
    pub fn new(token: Option<SecretString>) -> Result<GitHubClient, GitHubError> {
        let api = std::env::var("GITHUB_API_URL").unwrap_or_else(|_| DEFAULT_API.to_string());
        Self::with_api_url(token, &api)
    }

    /// Explicitly set the API base URL (for tests and GHE)
    pub fn with_api_url(
        token: Option<SecretString>,
        api: &str,
    ) -> Result<GitHubClient, GitHubError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent(concat!("hoverstare/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(GitHubClient {
            http,
            token,
            api: api.to_string(),
            retry_backoff_base: std::time::Duration::from_millis(500),
        })
    }

    /// Override the retry backoff base (for tests)
    pub fn with_retry_backoff(mut self, base: std::time::Duration) -> GitHubClient {
        self.retry_backoff_base = base;
        self
    }

    fn request(&self, method: reqwest::Method, url: &str) -> reqwest::RequestBuilder {
        self.request_with_accept(method, url, "application/vnd.github+json")
    }

    fn request_with_accept(
        &self,
        method: reqwest::Method,
        url: &str,
        accept: &str,
    ) -> reqwest::RequestBuilder {
        let mut req = self.http.request(method, url);
        if let Some(token) = &self.token {
            req = req.bearer_auth(token.expose_secret());
        }
        // Note: reqwest's .header() appends rather than overwrites, so Accept
        // can only be set once
        req.header("Accept", accept)
            .header("X-GitHub-Api-Version", "2022-11-28")
    }

    /// Exponential backoff retries (0.5s/2s/8s) for 429/5xx; other statuses are
    /// returned directly; exhausted 429 retries -> RateLimited (spec 02)
    async fn send(
        &self,
        build: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, GitHubError> {
        let mut attempt = 0u32;
        loop {
            let resp = build().send().await?;
            let status = resp.status().as_u16();
            let retryable = status == 429 || (500..600).contains(&status);
            if !retryable {
                return Ok(resp);
            }
            if attempt >= MAX_RETRIES {
                if status == 429 {
                    return Err(GitHubError::RateLimited);
                }
                return Ok(resp);
            }
            let backoff = self.retry_backoff_base * 4u32.pow(attempt);
            tracing::warn!(
                "GitHub API {status}, retrying in {backoff:?} ({}/{MAX_RETRIES})",
                attempt + 1
            );
            tokio::time::sleep(backoff).await;
            attempt += 1;
        }
    }

    async fn error_for_status(resp: reqwest::Response) -> Result<reqwest::Response, GitHubError> {
        let status = resp.status().as_u16();
        if (200..300).contains(&status) {
            Ok(resp)
        } else {
            let body = resp.text().await.unwrap_or_default();
            Err(GitHubError::Api { status, body })
        }
    }

    pub async fn get_pull_request(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<PullRequest, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        Ok(resp.json().await?)
    }

    /// Fetch the PR's unified diff text
    pub async fn get_pull_request_diff(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<String, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| {
                self.request_with_accept(
                    reqwest::Method::GET,
                    &url,
                    "application/vnd.github.v3.diff",
                )
            })
            .await?;
        match Self::error_for_status(resp).await {
            Ok(resp) => Ok(resp.text().await?),
            // PRs with >300 files are rejected by the diff endpoint (406) ->
            // fall back to reassembling via the files API (spec 02)
            Err(GitHubError::Api { status: 406, .. }) => {
                tracing::warn!(
                    "diff endpoint returned 406 (too many files), falling back to files API pagination"
                );
                self.fetch_diff_via_files_api(repo, number).await
            }
            Err(e) => Err(e),
        }
    }

    /// Reassemble a unified diff via files API pagination (spec 02 fallback path)
    async fn fetch_diff_via_files_api(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<String, GitHubError> {
        let files = self.list_pull_request_files(repo, number).await?;
        let mut out = String::new();
        for f in &files {
            let Some(patch) = &f.patch else {
                tracing::warn!(
                    "file {} has no patch (binary or too large), skipped",
                    f.filename
                );
                continue;
            };
            out.push_str(&format!("diff --git a/{0} b/{0}\n", f.filename));
            out.push_str(&format!("--- a/{0}\n+++ b/{0}\n", f.filename));
            out.push_str(patch);
            if !patch.ends_with('\n') {
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Fetch all PR files with pagination (per_page=100)
    pub async fn list_pull_request_files(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Vec<PrFile>, GitHubError> {
        let mut all = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{}/repos/{}/{}/pulls/{number}/files?per_page=100&page={page}",
                self.api, repo.owner, repo.name
            );
            let resp = self
                .send(|| self.request(reqwest::Method::GET, &url))
                .await?;
            let resp = Self::error_for_status(resp).await?;
            let batch: Vec<PrFile> = resp.json().await?;
            let done = batch.len() < 100;
            all.extend(batch);
            if done {
                break;
            }
            page += 1;
        }
        Ok(all)
    }

    /// Publish a PR review (body + optional inline comments, one request)
    pub async fn create_review(
        &self,
        repo: &Repo,
        number: u64,
        review: &NewReview,
    ) -> Result<u64, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/reviews",
            self.api, repo.owner, repo.name
        );
        let comments: Vec<serde_json::Value> = review
            .comments
            .iter()
            .map(|c| {
                serde_json::json!({
                    "path": c.path,
                    "line": c.line,
                    "side": "RIGHT",
                    "body": c.body,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "commit_id": review.commit_id,
            "body": review.body,
            "event": "COMMENT",
            "comments": comments,
        });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(body["id"].as_u64().unwrap_or(0))
    }

    /// Fallback: publish a plain PR comment
    pub async fn create_issue_comment(
        &self,
        repo: &Repo,
        number: u64,
        body: &str,
    ) -> Result<u64, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/issues/{number}/comments",
            self.api, repo.owner, repo.name
        );
        let payload = serde_json::json!({ "body": body });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(body["id"].as_u64().unwrap_or(0))
    }

    /// List the PR's reviews (used to find historical hoverstare reviews in incremental mode)
    pub async fn list_reviews(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Vec<ReviewSummary>, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/reviews?per_page=100",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        Ok(resp.json().await?)
    }

    /// compare API: diff between two commits (delta diff for incremental review)
    pub async fn get_compare_diff(
        &self,
        repo: &Repo,
        base: &str,
        head: &str,
    ) -> Result<String, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/compare/{base}...{head}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| {
                self.request_with_accept(
                    reqwest::Method::GET,
                    &url,
                    "application/vnd.github.v3.diff",
                )
            })
            .await?;
        let resp = Self::error_for_status(resp).await?;
        Ok(resp.text().await?)
    }

    /// Write a commit status (branch protection can consume it, spec 07)
    pub async fn create_status(
        &self,
        repo: &Repo,
        sha: &str,
        status: &NewStatus,
    ) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/statuses/{sha}",
            self.api, repo.owner, repo.name
        );
        let payload = serde_json::json!({
            "state": status.state.as_str(),
            "context": status.context,
            "description": status.description,
        });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        Self::error_for_status(resp).await?;
        Ok(())
    }

    /// Add a reaction to a comment by raw ids (issues/comments or pulls/comments).
    pub async fn create_reaction_for_comment(
        &self,
        repo: &Repo,
        comment_id: u64,
        in_reply_to: Option<u64>,
        content: &str,
    ) -> Result<(), GitHubError> {
        // issue comments and review thread comments use different endpoints
        let base = if in_reply_to.is_some() {
            format!(
                "{}/repos/{}/{}/pulls/comments/{}/reactions",
                self.api, repo.owner, repo.name, comment_id
            )
        } else {
            format!(
                "{}/repos/{}/{}/issues/comments/{}/reactions",
                self.api, repo.owner, repo.name, comment_id
            )
        };
        let payload = serde_json::json!({ "content": content });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &base).json(&payload))
            .await?;
        Self::error_for_status(resp).await?;
        Ok(())
    }

    /// Add a reaction to a mention event comment (spec 09).
    pub async fn create_reaction(
        &self,
        repo: &Repo,
        ev: &crate::event::MentionEvent,
        content: &str,
    ) -> Result<(), GitHubError> {
        self.create_reaction_for_comment(repo, ev.comment_id, ev.in_reply_to, content)
            .await
    }

    /// Fetch a review thread comment with its anchoring info (path + diff hunk)
    pub async fn get_review_comment(
        &self,
        repo: &Repo,
        comment_id: u64,
    ) -> Result<ReviewCommentDetail, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/comments/{comment_id}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        let body: serde_json::Value = resp.json().await?;
        Ok(ReviewCommentDetail {
            body: body["body"].as_str().unwrap_or_default().to_string(),
            path: body["path"].as_str().unwrap_or_default().to_string(),
            diff_hunk: body["diff_hunk"].as_str().unwrap_or_default().to_string(),
        })
    }

    /// Fetch the body of a review thread comment (context for the explain command)
    pub async fn get_review_comment_body(
        &self,
        repo: &Repo,
        comment_id: u64,
    ) -> Result<String, GitHubError> {
        Ok(self.get_review_comment(repo, comment_id).await?.body)
    }

    /// Fetch all comments of one review thread, chronological (multi-turn
    /// history for finding-thread discussions, spec 09). GitHub review
    /// threads are flat: the root has `id == root_id` and every reply has
    /// `in_reply_to_id == root_id`.
    pub async fn list_review_thread_comments(
        &self,
        repo: &Repo,
        number: u64,
        root_id: u64,
    ) -> Result<Vec<ReviewThreadComment>, GitHubError> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{}/repos/{}/{}/pulls/{number}/comments?per_page=100&page={page}",
                self.api, repo.owner, repo.name
            );
            let resp = self
                .send(|| self.request(reqwest::Method::GET, &url))
                .await?;
            let resp = Self::error_for_status(resp).await?;
            let batch: Vec<serde_json::Value> = resp.json().await?;
            let done = batch.len() < 100;
            for c in batch {
                let id = c["id"].as_u64().unwrap_or_default();
                if id != root_id && c["in_reply_to_id"].as_u64() != Some(root_id) {
                    continue;
                }
                out.push(ReviewThreadComment {
                    id,
                    body: c["body"].as_str().unwrap_or_default().to_string(),
                    author: c["user"]["login"].as_str().unwrap_or_default().to_string(),
                });
            }
            if done {
                break;
            }
            page += 1;
        }
        out.sort_by_key(|c| c.id);
        Ok(out)
    }

    /// Fetch a user's collaborator permission level for this repo (spec 12).
    pub async fn get_collaborator_permission(
        &self,
        repo: &Repo,
        login: &str,
    ) -> Result<RepoPermission, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/collaborators/{login}/permission",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        #[derive(serde::Deserialize)]
        struct PermResp {
            permission: String,
        }
        let body: PermResp = resp.json().await?;
        RepoPermission::parse(&body.permission).ok_or_else(|| GitHubError::Api {
            status: 200,
            body: format!("unknown permission: {}", body.permission),
        })
    }

    /// Check whether a user is an active member of an org team (spec 12).
    /// Treats any API error (including non-org repos) as "not a member".
    pub async fn check_team_membership(&self, org: &str, team: &str, login: &str) -> bool {
        let url = format!("{}/orgs/{org}/teams/{team}/memberships/{login}", self.api);
        let Ok(resp) = self.send(|| self.request(reqwest::Method::GET, &url)).await else {
            return false;
        };
        if resp.status().as_u16() == 404 {
            return false;
        }
        let Ok(resp) = Self::error_for_status(resp).await else {
            return false;
        };
        #[derive(serde::Deserialize)]
        struct MembershipResp {
            state: String,
        }
        let Ok(body) = resp.json::<MembershipResp>().await else {
            return false;
        };
        body.state.eq_ignore_ascii_case("active")
    }

    // ------------------------------------------------------------------
    // GraphQL
    // ------------------------------------------------------------------

    async fn graphql(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<serde_json::Value, GitHubError> {
        let url = format!("{}/graphql", self.api);
        let payload = serde_json::json!({ "query": query, "variables": variables });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        let resp = Self::error_for_status(resp).await?;
        let body: serde_json::Value = resp.json().await?;
        // GraphQL errors come back as HTTP 200 + an errors field (spec 02)
        if let Some(errors) = body.get("errors")
            && errors.as_array().is_some_and(|e| !e.is_empty())
        {
            return Err(GitHubError::Graphql(errors.to_string()));
        }
        Ok(body)
    }

    /// Fetch all review threads of a PR with pagination (first comment of each thread)
    pub async fn list_review_threads(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Vec<ReviewThread>, GitHubError> {
        const QUERY: &str = r#"query($owner: String!, $repo: String!, $pr: Int!, $cursor: String) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $pr) {
      reviewThreads(first: 100, after: $cursor) {
        nodes {
          id
          isResolved
          comments(first: 1) { nodes { databaseId body } }
        }
        pageInfo { hasNextPage endCursor }
      }
    }
  }
}"#;
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let data = self
                .graphql(
                    QUERY,
                    serde_json::json!({
                        "owner": repo.owner,
                        "repo": repo.name,
                        "pr": number,
                        "cursor": cursor,
                    }),
                )
                .await?;
            let threads = &data["data"]["repository"]["pullRequest"]["reviewThreads"];
            for node in threads["nodes"].as_array().cloned().unwrap_or_default() {
                out.push(ReviewThread {
                    id: node["id"].as_str().unwrap_or_default().to_string(),
                    is_resolved: node["isResolved"].as_bool().unwrap_or(false),
                    first_comment_id: node["comments"]["nodes"][0]["databaseId"].as_u64(),
                    first_comment_body: node["comments"]["nodes"][0]["body"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                });
            }
            let page_info = &threads["pageInfo"];
            if page_info["hasNextPage"].as_bool() == Some(true) {
                cursor = page_info["endCursor"].as_str().map(String::from);
            } else {
                break;
            }
        }
        Ok(out)
    }

    /// Reply inside a review thread (FORBIDDEN fallback path for resolve, spec 07)
    pub async fn reply_to_review_comment(
        &self,
        repo: &Repo,
        number: u64,
        comment_id: u64,
        body: &str,
    ) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/comments/{comment_id}/replies",
            self.api, repo.owner, repo.name
        );
        let payload = serde_json::json!({ "body": body });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        Self::error_for_status(resp).await?;
        Ok(())
    }

    /// Resolve a review thread (individual failures are recorded by the caller, not retried)
    pub async fn resolve_review_thread(&self, thread_id: &str) -> Result<(), GitHubError> {
        const MUTATION: &str = r#"mutation($threadId: ID!) {
  resolveReviewThread(input: { threadId: $threadId }) {
    thread { isResolved }
  }
}"#;
        self.graphql(MUTATION, serde_json::json!({ "threadId": thread_id }))
            .await?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Develop mode API (spec 11)
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct IssueComment {
    pub id: u64,
    #[serde(default)]
    pub body: Option<String>,
    pub user: PrUser,
}

#[derive(Debug, serde::Deserialize)]
pub struct CreatedPr {
    pub number: u64,
    pub html_url: String,
}

#[derive(Debug, serde::Deserialize)]
pub struct CheckRun {
    pub name: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
pub struct RepoMeta {
    pub default_branch: String,
}

#[derive(Debug, serde::Deserialize)]
struct CheckRunsPage {
    check_runs: Vec<CheckRun>,
}

impl GitHubClient {
    pub async fn get_issue(&self, repo: &Repo, number: u64) -> Result<Issue, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/issues/{number}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        Ok(Self::error_for_status(resp).await?.json().await?)
    }

    /// All issue comments (paginated, chronological).
    pub async fn list_issue_comments(
        &self,
        repo: &Repo,
        number: u64,
    ) -> Result<Vec<IssueComment>, GitHubError> {
        let mut out = Vec::new();
        let mut page = 1u32;
        loop {
            let url = format!(
                "{}/repos/{}/{}/issues/{number}/comments?per_page=100&page={page}",
                self.api, repo.owner, repo.name
            );
            let resp = self
                .send(|| self.request(reqwest::Method::GET, &url))
                .await?;
            let batch: Vec<IssueComment> = Self::error_for_status(resp).await?.json().await?;
            let done = batch.len() < 100;
            out.extend(batch);
            if done {
                return Ok(out);
            }
            page += 1;
        }
    }

    pub async fn create_pull_request(
        &self,
        repo: &Repo,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
    ) -> Result<CreatedPr, GitHubError> {
        let url = format!("{}/repos/{}/{}/pulls", self.api, repo.owner, repo.name);
        let payload = serde_json::json!({
            "title": title, "head": head, "base": base, "body": body,
        });
        let resp = self
            .send(|| self.request(reqwest::Method::POST, &url).json(&payload))
            .await?;
        Ok(Self::error_for_status(resp).await?.json().await?)
    }

    /// Squash-merge a PR; returns the merge commit sha. `commit_message` (when
    /// set) becomes the squash commit body, e.g. a co-author trailer
    /// (spec 11 §3.3).
    pub async fn merge_pull_request(
        &self,
        repo: &Repo,
        number: u64,
        commit_message: Option<&str>,
    ) -> Result<String, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/pulls/{number}/merge",
            self.api, repo.owner, repo.name
        );
        let mut payload = serde_json::json!({ "merge_method": "squash" });
        if let Some(message) = commit_message {
            payload["commit_message"] = serde_json::Value::String(message.to_string());
        }
        let resp = self
            .send(|| self.request(reqwest::Method::PUT, &url).json(&payload))
            .await?;
        #[derive(serde::Deserialize)]
        struct MergeResult {
            sha: String,
        }
        let r: MergeResult = Self::error_for_status(resp).await?.json().await?;
        Ok(r.sha)
    }

    pub async fn list_check_runs(
        &self,
        repo: &Repo,
        sha: &str,
    ) -> Result<Vec<CheckRun>, GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/commits/{sha}/check-runs?per_page=100",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        let page: CheckRunsPage = Self::error_for_status(resp).await?.json().await?;
        Ok(page.check_runs)
    }

    /// Delete a branch (best-effort; spec 11 §6: after a successful merge).
    pub async fn delete_branch(&self, repo: &Repo, branch: &str) -> Result<(), GitHubError> {
        let url = format!(
            "{}/repos/{}/{}/git/refs/heads/{branch}",
            self.api, repo.owner, repo.name
        );
        let resp = self
            .send(|| self.request(reqwest::Method::DELETE, &url))
            .await?;
        Self::error_for_status(resp).await?;
        Ok(())
    }

    pub async fn get_repo_meta(&self, repo: &Repo) -> Result<RepoMeta, GitHubError> {
        let url = format!("{}/repos/{}/{}", self.api, repo.owner, repo.name);
        let resp = self
            .send(|| self.request(reqwest::Method::GET, &url))
            .await?;
        Ok(Self::error_for_status(resp).await?.json().await?)
    }
}
