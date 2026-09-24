//! Job execution: workspace preparation + orchestration reuse (spec 10)

use std::sync::Arc;

use secrecy::ExposeSecret;

use crate::cli::ReviewArgs;
use crate::config::Config;
use crate::orchestrator;
use crate::serve::AppState;
use crate::serve::webhook::{MentionHookEvent, ReviewEvent};

/// review job: clone workspace -> inject installation token -> reuse orchestration
pub async fn run_review_job(state: Arc<AppState>, ev: ReviewEvent) {
    let _permit = state.job_semaphore.acquire().await;
    let pr_mutex = state.pr_lock(ev.repo.clone(), ev.pr_number);
    let _pr_guard = pr_mutex.lock().await;

    let result = run_review_inner(&state, &ev).await;
    if let Err(e) = &result {
        tracing::warn!("review job failed {}#{}: {e:#}", ev.repo, ev.pr_number);
    }
}

async fn run_review_inner(state: &AppState, ev: &ReviewEvent) -> anyhow::Result<()> {
    let token = state.auth.installation_token(ev.installation_id).await?;

    let dir = tempfile::tempdir()?;
    clone_workspace(
        dir.path(),
        &ev.repo,
        &ev.head_repo,
        &token,
        &ev.head_sha,
        &ev.base_ref,
    )
    .await?;

    let mut cfg: Config = state.cfg.clone();
    cfg.github_token = Some(token);
    cfg.workspace = dir.path().to_path_buf();

    let args = ReviewArgs {
        pr: Some(ev.pr_number),
        repo: Some(ev.repo.clone()),
        ..Default::default()
    };
    let outcome = orchestrator::run_review(&cfg, &args, false).await?;
    tracing::info!("review completed {}#{}: {outcome:?}", ev.repo, ev.pr_number);
    Ok(())
}

/// mention job: inject installation token -> reuse mention orchestration
pub async fn run_mention_job(state: Arc<AppState>, ev: MentionHookEvent) {
    let _permit = state.job_semaphore.acquire().await;
    let pr_mutex = state.pr_lock(ev.mention.repo.clone(), ev.mention.pr_number);
    let _pr_guard = pr_mutex.lock().await;

    let result = async {
        let token = state.auth.installation_token(ev.installation_id).await?;
        let mut cfg: Config = state.cfg.clone();
        cfg.github_token = Some(token);
        crate::mention::run_mention_event(&cfg, &ev.mention).await
    }
    .await;
    if let Err(e) = &result {
        tracing::warn!(
            "mention job failed {}#{}: {e:#}",
            ev.mention.repo,
            ev.mention.pr_number
        );
    }
}

/// Clone the workspace (spec 10): target repo + fetch head sha (falls back to
/// the head repo for forks)
async fn clone_workspace(
    dir: &std::path::Path,
    repo: &str,
    head_repo: &str,
    token: &secrecy::SecretString,
    head_sha: &str,
    base_ref: &str,
) -> anyhow::Result<()> {
    let url = |r: &str| {
        format!(
            "https://x-access-token:{}@github.com/{r}.git",
            token.expose_secret()
        )
    };
    run_git(
        dir,
        &["clone", "--depth", "100", "--quiet", &url(repo), "."],
    )
    .await?;
    // GitHub supports fetching reachable commits by sha; a fork's head sha is
    // not in the target repo -> fall back to the head repo
    if run_git(
        dir,
        &["fetch", "--depth", "100", "--quiet", "origin", head_sha],
    )
    .await
    .is_err()
        && head_repo != repo
    {
        run_git(
            dir,
            &[
                "fetch",
                "--depth",
                "100",
                "--quiet",
                &url(head_repo),
                &format!("{head_sha}:refs/remotes/fork-head/{head_sha}"),
            ],
        )
        .await?;
    }
    run_git(
        dir,
        &[
            "fetch",
            "--depth",
            "100",
            "--quiet",
            "origin",
            &format!("{base_ref}:refs/remotes/origin/{base_ref}"),
        ],
    )
    .await?;
    run_git(dir, &["checkout", "--quiet", head_sha]).await?;
    Ok(())
}

async fn run_git(dir: &std::path::Path, args: &[&str]) -> anyhow::Result<()> {
    let out = tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        // Sanitize: errors may contain the URL (with the token), always truncate
        // everything from the domain part onward
        let sanitized = stderr.replace("x-access-token:", "x-access-token:***");
        let sanitized = sanitized
            .split('@')
            .next()
            .unwrap_or("git error")
            .to_string();
        anyhow::bail!("git {} failed: {sanitized}", args.first().unwrap_or(&""));
    }
    Ok(())
}
