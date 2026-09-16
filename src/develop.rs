//! Develop loop (spec 11): run the agent with read+write tools on a task,
//! then commit the resulting workspace changes (Conventional Commits).
//!
//! Review keeps the read-only toolset; only this loop uses `ReadWrite`.

use std::path::Path;
use std::time::Duration;

use crate::agent::{
    AgentBackend, Budget, ReviewRequest, ToolProfile, ToolRegistry, tools::ToolShared,
};
use crate::git::GitRepo;

/// Commit identity for bot-authored commits (spec 11 §3.3).
pub const AUTHOR_NAME: &str = "hoverstare[bot]";
pub const AUTHOR_EMAIL: &str = "hoverstare[bot]@users.noreply.github.com";
/// Default implement-round budget (spec 11 §4).
pub const DEFAULT_BUDGET_CALLS: u32 = 40;
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(600);

pub struct DevelopOutcome {
    pub summary: String,
    /// Commit sha when a commit was created.
    pub commit: Option<String>,
    pub dry_run: bool,
    /// The tool-call budget ran out mid-loop (self-trigger signal, spec 11 §6).
    pub budget_exhausted: bool,
}

/// System prompt for develop mode (spec 11). Kept separate from the review
/// prompt contract — this loop implements, it does not review.
pub fn dev_system_prompt() -> String {
    "You are HoverStare, an AI developer working inside a repository checkout.\n\
     Your job: implement the user's task by editing files with the provided tools.\n\n\
     Rules:\n\
     - Investigate before editing: read the files you will change and their callers.\n\
     - Use edit_file for targeted changes (old_string must match exactly and uniquely);\n\
     use write_file to create new files or rewrite whole files.\n\
     - Stay minimal and focused: implement the task, nothing more. Follow the repo's\n\
     existing style and conventions.\n\
     - Do not touch files unrelated to the task. Never edit anything under .git/.\n\
     - You cannot run builds or tests; write code that is correct by careful reading.\n\
     - When finished, reply with a concise summary: what changed, where, and why."
        .to_string()
}

/// Parameters for one develop round.
pub struct DevelopRequest<'a> {
    pub workspace: &'a Path,
    pub task: &'a str,
    /// Human-readable subject for the commit message (usually the raw task
    /// or instruction, not the wrapped prompt).
    pub commit_hint: &'a str,
    pub dry_run: bool,
    pub backend: &'a dyn AgentBackend,
    pub model: &'a str,
    pub temperature: Option<f64>,
    pub budget_calls: u32,
}

/// Run one develop round locally: agent works the task in the workspace,
/// then changes are committed (unless `dry_run`).
pub async fn run(req: DevelopRequest<'_>) -> anyhow::Result<DevelopOutcome> {
    let DevelopRequest {
        workspace,
        task,
        commit_hint,
        dry_run,
        backend,
        model,
        temperature,
        budget_calls,
    } = req;
    let git = GitRepo::open(workspace)?;
    let base_ref = git
        .current_branch()
        .await
        .unwrap_or_else(|_| "HEAD".to_string());
    let budget = Budget {
        max_tool_calls: budget_calls,
        timeout: DEFAULT_TIMEOUT,
    };
    // Resilience (same spirit as the review pipeline): transient backend
    // errors and empty outputs get up to 3 attempts, later attempts with a
    // nudge to start editing immediately. Each attempt gets a FRESH tool
    // budget (a shared counter would starve retries).
    const MAX_ATTEMPTS: u32 = 3;
    let mut run_opt = None;
    let mut last_err = None;
    let mut last_counter = None;
    for attempt in 1..=MAX_ATTEMPTS {
        let shared = ToolShared::new(workspace.to_path_buf(), base_ref.clone(), budget_calls);
        let req = |user_prompt: String| ReviewRequest {
            system_prompt: dev_system_prompt(),
            user_prompt,
            tools: ToolRegistry {
                shared: Some(shared.clone()),
                profile: ToolProfile::ReadWrite,
            },
            budget,
            model: model.to_string(),
            temperature,
        };
        let prompt = if attempt == 1 {
            format!(
                "# Task\n{task}\n\nImplement it now. When done, reply with a concise summary of what changed and why."
            )
        } else {
            format!(
                "# Task\n{task}\n\nIMPORTANT: start editing files right away with the tools. Work incrementally: small, correct edits. When done, reply with a concise summary of what changed and why."
            )
        };
        last_counter = Some(shared.clone());
        match backend.review(req(prompt)).await {
            Ok(run) if !run.raw_output.trim().is_empty() => {
                run_opt = Some(run);
                break;
            }
            Ok(run) => {
                tracing::warn!("develop: empty agent output (attempt {attempt}/{MAX_ATTEMPTS})");
                // Keep as fallback; a later attempt may produce content.
                run_opt = Some(run);
            }
            Err(crate::agent::AgentError::Timeout(budget)) => {
                // A timeout means the model had its whole budget and did not
                // finish. Retrying spends the same minutes on the same result
                // while holding the concurrency group, so this stops here and
                // says what the task needed.
                tracing::warn!("develop: agent timed out after {budget:?}; not retrying");
                return Err(anyhow::anyhow!(
                    "agent timed out after {budget:?} without finishing the task; \
                     split the task or raise the budget"
                ));
            }
            Err(e) => {
                tracing::warn!(
                    "develop: agent call failed (attempt {attempt}/{MAX_ATTEMPTS}): {e}"
                );
                last_err = Some(e);
            }
        }
    }
    let run = match (run_opt, last_err) {
        (Some(run), _) => run,
        (None, Some(e)) => {
            return Err(anyhow::anyhow!(
                "agent failed after {MAX_ATTEMPTS} attempts: {e}"
            ));
        }
        (None, None) => unreachable!("loop always sets one of them"),
    };
    let summary = run.raw_output.trim().to_string();

    let exhausted = last_counter
        .as_ref()
        .map(|c| c.call_count() >= budget_calls)
        .unwrap_or(false);
    if !git.has_changes().await? {
        return Ok(DevelopOutcome {
            summary: format!("{summary}\n\n(no workspace changes)"),
            commit: None,
            dry_run,
            budget_exhausted: exhausted,
        });
    }
    if dry_run {
        let status = git.status_porcelain().await?;
        return Ok(DevelopOutcome {
            summary: format!("{summary}\n\n[dry-run] would commit:\n{status}"),
            commit: None,
            dry_run: true,
            budget_exhausted: exhausted,
        });
    }
    git.add_all().await?;
    let sha = git
        .commit(&commit_message(commit_hint), AUTHOR_NAME, AUTHOR_EMAIL)
        .await?;
    Ok(DevelopOutcome {
        summary,
        commit: sha,
        dry_run,
        budget_exhausted: exhausted,
    })
}

/// Conventional commit message from the task's first line (spec 11 §3.3).
fn commit_message(task: &str) -> String {
    let first = task.lines().next().unwrap_or("task").trim();
    let short: String = first.chars().take(60).collect();
    format!("feat(hoverstare-dev): {short}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentError, ReviewRun};

    /// Fake backend that performs a write through the registry, like a real
    /// agent calling write_file mid-loop.
    struct WriteBackend;
    #[async_trait::async_trait]
    impl AgentBackend for WriteBackend {
        async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError> {
            assert_eq!(req.tools.profile, ToolProfile::ReadWrite);
            let shared = req.tools.shared.unwrap();
            let out =
                crate::agent::tools::write_file(&shared, "hello.txt", "hello from agent\n").await;
            assert!(out.contains("wrote hello.txt"), "{out}");
            Ok(ReviewRun {
                raw_output: "created hello.txt".into(),
                ..Default::default()
            })
        }
    }

    async fn fixture_repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::process::Command::new("git")
            .args(["init", "-q", "-b", "master"])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        let repo = GitRepo::open(root).unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn develop_commits_agent_changes() {
        let (_d, repo) = fixture_repo().await;
        let out = run(DevelopRequest {
            workspace: repo.root(),
            task: "create hello.txt with a greeting",
            commit_hint: "create hello.txt with a greeting",
            dry_run: false,
            backend: &WriteBackend,
            model: "test-model",
            temperature: None,
            budget_calls: 10,
        })
        .await
        .unwrap();
        assert!(out.commit.is_some());
        assert_eq!(out.summary, "created hello.txt");
        let log = repo.run(&["log", "--format=%an %s", "-1"]).await.unwrap();
        assert_eq!(
            log,
            "hoverstare[bot] feat(hoverstare-dev): create hello.txt with a greeting"
        );
        assert!(!repo.has_changes().await.unwrap());
    }

    #[tokio::test]
    async fn dry_run_does_not_commit() {
        let (_d, repo) = fixture_repo().await;
        let out = run(DevelopRequest {
            workspace: repo.root(),
            task: "create hello.txt",
            commit_hint: "create hello.txt",
            dry_run: true,
            backend: &WriteBackend,
            model: "m",
            temperature: None,
            budget_calls: 10,
        })
        .await
        .unwrap();
        assert!(out.commit.is_none());
        assert!(out.summary.contains("[dry-run] would commit:"));
        assert!(out.summary.contains("hello.txt"));
        // working tree still dirty, no new commit
        assert!(repo.has_changes().await.unwrap());
        let log = repo.run(&["log", "--format=%s", "-1"]).await.unwrap();
        assert_eq!(log, "init");
    }
}
