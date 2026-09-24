//! CLI definition and entry-point logic (spec 01)

use clap::{Args, Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::{config, devagent, develop, event, i18n, mention, orchestrator};

#[derive(Parser)]
#[command(
    name = "hoverstare",
    version,
    about = "Repo-aware AI code review bot",
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,

    /// debug logging
    #[arg(short, long, global = true)]
    pub verbose: bool,
}

#[derive(Subcommand)]
pub enum Command {
    /// Review a PR (main GitHub Actions entry point)
    Review(ReviewArgs),
    /// Handle an @hoverstare comment (M6)
    Mention,
    /// Develop on a task with the agent (spec 11; local mode)
    Develop(DevelopArgs),
    /// Run as a webhook service (optional self-hosted, spec 10)
    Serve(ServeArgs),
    /// Show help and exit (works without LLM credentials, issue #6)
    Help,
}

#[derive(Args)]
pub struct DevelopArgs {
    /// Local mode: the development task in natural language (no GitHub events)
    #[arg(long)]
    pub task: Option<String>,

    /// Local mode: do not commit; print what would change instead
    #[arg(long)]
    pub dry_run: bool,

    /// Local mode: target repo (owner/name) for issue/PR flows
    #[arg(long)]
    pub repo: Option<String>,

    /// Local mode: run the issue flow for this issue number
    #[arg(long)]
    pub issue: Option<u64>,

    /// Local mode: run the PR flow for this PR number
    #[arg(long)]
    pub pr: Option<u64>,

    /// Local mode: instruction text (issue discuss / PR dev round)
    #[arg(long)]
    pub instruction: Option<String>,

    /// Local mode: implement the agreed plan (issue flow)
    #[arg(long)]
    pub go: bool,

    /// Local mode: merge the PR (PR flow)
    #[arg(long)]
    pub merge: bool,
}

#[derive(Args)]
pub struct ServeArgs {
    /// Listen port (can also be overridden by the PORT env var)
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Args)]
pub struct ReviewArgs {
    /// Override the PR number from the event (for local debugging)
    #[arg(long)]
    pub pr: Option<u64>,

    /// Override the repository (owner/repo, for local debugging)
    #[arg(long)]
    pub repo: Option<String>,

    /// Run the full analysis without publishing; print the review JSON to stdout
    #[arg(long)]
    pub dry_run: bool,

    /// Report what would be reviewed and exit without calling the model
    /// (spec 14 §3)
    #[arg(long)]
    pub preview: bool,

    /// Output format: `human` (default) or `json`. Structured output for the run
    /// itself lands with spec 16 (M19); today `json` applies to `--preview`.
    #[arg(long, value_enum, default_value = "human")]
    pub format: OutputFormat,
}

impl Default for ReviewArgs {
    /// The no-flag form used by internal callers (mention commands, serve mode).
    fn default() -> Self {
        ReviewArgs {
            pr: None,
            repo: None,
            dry_run: false,
            preview: false,
            format: OutputFormat::Human,
        }
    }
}

/// Output format selector (spec 16 §1 defines the full set; `sarif` arrives with
/// M19, so it is accepted here and rejected with an explicit message until then).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
    Sarif,
}

impl OutputFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            OutputFormat::Human => "human",
            OutputFormat::Json => "json",
            OutputFormat::Sarif => "sarif",
        }
    }
}

/// CLI main entry point (shared by the hoverstare and bugbot alias binaries)
pub async fn run() {
    let args = Cli::parse();

    let filter = if args.verbose {
        "hoverstare=debug"
    } else {
        "hoverstare=info"
    };
    // Logs go to stderr so that stdout stays a clean, pipeable channel: the
    // preview prints its selection there, and spec 16's structured output will
    // too (a log line mixed into a JSON document is not a document).
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| filter.into()))
        .with_target(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    let code = match args.command {
        Command::Review(review) => run_review(review).await,
        Command::Mention => run_mention().await,
        Command::Develop(develop_args) => run_develop(develop_args).await,
        Command::Serve(serve_args) => run_serve(serve_args).await,
        Command::Help => run_help(),
    };
    std::process::exit(code);
}

/// One log line per terminal outcome (shared by the run and the preview).
fn log_review_outcome(outcome: &orchestrator::Outcome) {
    use orchestrator::Outcome::*;
    match outcome {
        Skipped(reason) => tracing::info!("skipped: {reason}"),
        Published {
            inline_comments,
            terminal,
        } => tracing::info!(
            "✅ review published ({inline_comments} inline comments, coverage {})",
            terminal.as_str()
        ),
        Previewed {
            units,
            excluded,
            truncated,
        } => tracing::info!(
            "✅ preview: {units} unit(s) would be reviewed ({excluded} excluded, {truncated} truncated), no model calls"
        ),
        DryRun => tracing::info!("✅ dry-run complete (not published)"),
        AnalysisFailed(reason) => {
            // fail-open: analysis failure does not block CI (spec 01)
            tracing::warn!("analysis failed (fail-open, exit 0): {reason}");
        }
    }
}

/// Config errors are problems the user must fix immediately -> exit 1 (spec 01)
fn load_config() -> Result<config::Config, i32> {
    config::Config::load().map_err(|e| {
        tracing::error!("config error: {e:#}");
        1
    })
}

/// Print localized help to stdout without loading config (issue #6)
fn run_help() -> i32 {
    let lang = i18n::Lang::resolve(std::env::var("HOVERSTARE_LANGUAGE").ok().as_deref(), None);
    println!("{}", i18n::T::new(lang).help_text());
    0
}

async fn run_review(args: ReviewArgs) -> i32 {
    // A preview never calls the model, so it loads config without requiring
    // model credentials (spec 14 §3): "what would be reviewed" must be askable
    // in an environment that has no key yet.
    let cfg = match if args.preview {
        config::Config::load_read_only()
    } else {
        config::Config::load()
    } {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("config error: {e:#}");
            return 1;
        }
    };
    if args.preview {
        return match orchestrator::preview(&cfg, &args, args.format == OutputFormat::Json).await {
            Ok(outcome) => {
                log_review_outcome(&outcome);
                0
            }
            Err(e) => {
                tracing::error!("{e:#}");
                1
            }
        };
    }
    if args.format != OutputFormat::Human {
        // Refuse rather than silently ignore: the run's structured output is
        // spec 16's job (M19), and a quiet no-op would look like it worked.
        tracing::error!(
            "--format {} is not supported for a run yet (spec 16 / M19); use --preview --format json",
            args.format.as_str()
        );
        return 1;
    }
    match orchestrator::run_review(&cfg, &args, false).await {
        Ok(outcome) => {
            log_review_outcome(&outcome);
            0
        }
        Err(e) => {
            tracing::error!("{e:#}");
            1
        }
    }
}

async fn run_develop(args: DevelopArgs) -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    // M11 local mode: run a task in the current workspace, no GitHub events.
    if let Some(task) = args.task {
        let backend = match crate::agent::rig_backend::RigBackend::from_config(&cfg) {
            Ok(backend) => backend,
            Err(e) => {
                // Unreachable through `load_config` (it already requires
                // credentials), but a configuration question never panics.
                tracing::error!("{e:#}");
                return 1;
            }
        };
        let budget = cfg.max_tool_calls.max(develop::DEFAULT_BUDGET_CALLS);
        return match develop::run(develop::DevelopRequest {
            workspace: &cfg.workspace,
            task: &task,
            commit_hint: &task,
            dry_run: args.dry_run,
            backend: &backend,
            model: &cfg.model,
            temperature: cfg.temp(0.0),
            budget_calls: budget,
            // Local `--task` mode has no trigger: fall back to the bot identity.
            commit_identity: crate::git::resolve_commit_identity(
                cfg.commit_identity,
                None,
                cfg.commit_author.as_deref(),
            ),
        })
        .await
        {
            Ok(outcome) => {
                println!("{}", outcome.summary);
                if let Some(sha) = outcome.commit {
                    tracing::info!("✅ committed: {}", &sha[..sha.len().min(10)]);
                }
                0
            }
            Err(e) => {
                tracing::error!("develop failed: {e:#}");
                1
            }
        };
    }
    // Event/local-flag mode: issue & PR flows (spec 11)
    let ev = match resolve_dev_event(&args) {
        Ok(Some(ev)) => ev,
        Ok(None) => {
            tracing::info!(
                "develop: no trigger (not a dev event; pass --task/--issue/--pr for local mode)"
            );
            return 0;
        }
        Err(e) => {
            tracing::error!("develop: bad event: {e:#}");
            return 1;
        }
    };
    match devagent::run_event(&cfg, &ev).await {
        Ok(msg) => {
            tracing::info!("develop: {msg}");
            0
        }
        Err(e) => {
            tracing::error!("develop failed: {e:#}");
            1
        }
    }
}

/// Build the dev trigger from CLI flags, or fall back to GITHUB_EVENT_PATH.
fn resolve_dev_event(args: &DevelopArgs) -> anyhow::Result<Option<event::DevEvent>> {
    let owner_flag = || "OWNER".to_string();
    if let Some(n) = args.issue {
        let repo = args
            .repo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--issue requires --repo owner/name"))?;
        let body = if args.go {
            "@hoverstare go".to_string()
        } else {
            format!(
                "@hoverstare {}",
                args.instruction.clone().unwrap_or_default()
            )
        };
        return Ok(Some(event::DevEvent {
            repo,
            number: n,
            is_pr: false,
            kind: event::DevKind::IssueComment,
            title: None,
            body,
            comment_id: None,
            author_association: owner_flag(),
            in_reply_to: None,
            author: "local".into(),
        }));
    }
    if let Some(n) = args.pr {
        let repo = args
            .repo
            .clone()
            .ok_or_else(|| anyhow::anyhow!("--pr requires --repo owner/name"))?;
        let body = if args.merge {
            "@hoverstare merge".to_string()
        } else {
            format!(
                "@hoverstare {}",
                args.instruction
                    .clone()
                    .unwrap_or_else(|| "continue".to_string())
            )
        };
        return Ok(Some(event::DevEvent {
            repo,
            number: n,
            is_pr: true,
            kind: event::DevKind::IssueComment,
            title: None,
            body,
            comment_id: None,
            author_association: owner_flag(),
            in_reply_to: None,
            author: "local".into(),
        }));
    }
    event::resolve_dev_event()
}

async fn run_serve(args: ServeArgs) -> i32 {
    let port = std::env::var("PORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .or(args.port)
        .unwrap_or(8080);
    match crate::serve::run(port).await {
        Ok(()) => 0,
        Err(e) => {
            tracing::error!("serve failed to start: {e:#}");
            1
        }
    }
}

async fn run_mention() -> i32 {
    let cfg = match load_config() {
        Ok(c) => c,
        Err(code) => return code,
    };
    match mention::run_mention(&cfg).await {
        Ok(outcome) => {
            use orchestrator::Outcome::*;
            match outcome {
                Skipped(reason) => tracing::info!("skipped: {reason}"),
                _ => tracing::info!("✅ mention handled"),
            }
            0
        }
        Err(e) => {
            // mention command failures are also fail-open (spec 09 follows the same contract)
            tracing::warn!("mention handling failed (fail-open, exit 0): {e:#}");
            0
        }
    }
}
