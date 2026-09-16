//! Configuration loading and validation (spec 01)
//!
//! Merge precedence: CLI flag > environment variable > `.github/hoverstare.toml` > built-in defaults.
//! CLI flags currently only override PR/repo targeting (see cli.rs) and do not enter Config.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use dashmap::DashMap;
use globset::{Glob, GlobSet, GlobSetBuilder};
use secrecy::SecretString;
use serde::Deserialize;

use crate::agent::{ReasoningEffort, ReasoningOptions, ThinkingMode};

#[derive(Debug, Clone)]
pub struct Config {
    pub model: String,
    /// M2 (cheap model for the reformat pass)
    #[allow(dead_code)]
    pub reformat_model: String,
    /// Number of multi-pass voting lanes (spec 05)
    pub passes: u8,
    /// Whether single-vote findings go through the verifier (spec 05)
    pub verify: bool,
    pub severity_threshold: Severity,
    pub ignore: GlobSet,
    /// M2 (large diff truncation)
    #[allow(dead_code)]
    pub max_diff_kb: usize,
    pub max_tool_calls: u32,
    pub timeout_secs: u64,
    pub review_drafts: bool,
    pub fail_closed: bool,
    /// M4 (status checks)
    #[allow(dead_code)]
    pub status_checks: bool,
    pub instructions: String,
    /// Whether to set temperature on requests (some endpoints only accept the
    /// default; when false the field is not sent)
    pub set_temperature: bool,
    /// Provider-side thinking/reasoning tuning (spec 01/04)
    pub reasoning: ReasoningOptions,
    /// Model context window in tokens (spec 01). When set, it caps the diff
    /// budget so the prompt still fits the window.
    pub context_tokens: Option<u64>,
    /// Context compaction settings (spec 13)
    pub compaction: crate::agent::compaction::CompactionConfig,
    /// Output language (HOVERSTARE_LANGUAGE env > toml language > default en)
    pub language: crate::i18n::Lang,
    pub github_token: Option<SecretString>,
    /// Classic PAT with a **narrow duty** (spec 07/11): resolveReviewThread
    /// fallback and dev-mode git push. Never used as the API identity —
    /// comments/reviews always go through `github_token` (App token).
    pub gh_pat: Option<SecretString>,
    pub llm: LlmCredentials,
    /// M3 (tool sandbox root)
    pub workspace: PathBuf,
    /// M14 (fine-grained permissions, spec 12)
    pub permissions: Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn parse_loose(s: &str) -> Severity {
        match s.trim().to_ascii_lowercase().as_str() {
            "critical" => Severity::Critical,
            "high" => Severity::High,
            "low" => Severity::Low,
            _ => Severity::Medium,
        }
    }

    pub fn emoji(self) -> &'static str {
        match self {
            Severity::Critical => "🔴",
            Severity::High => "🟠",
            Severity::Medium => "🟡",
            Severity::Low => "🔵",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeverityToml {
    Low,
    Medium,
    High,
    Critical,
}

impl From<SeverityToml> for Severity {
    fn from(v: SeverityToml) -> Self {
        match v {
            SeverityToml::Low => Severity::Low,
            SeverityToml::Medium => Severity::Medium,
            SeverityToml::High => Severity::High,
            SeverityToml::Critical => Severity::Critical,
        }
    }
}

#[derive(Debug, Clone)]
pub enum LlmCredentials {
    Anthropic {
        key: SecretString,
        /// M2 (Anthropic-compatible endpoint override)
        #[allow(dead_code)]
        base_url: Option<String>,
    },
    OpenAICompatible {
        key: SecretString,
        base_url: String,
    },
}

/// Fine-grained command permissions (spec 12).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Permissions {
    #[serde(default = "default_auto_review")]
    pub auto_review: Vec<String>,
    #[serde(default = "default_review")]
    pub review: Vec<String>,
    #[serde(default = "default_develop")]
    pub develop: Vec<String>,
    #[serde(default = "default_merge")]
    pub merge: Vec<String>,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            auto_review: default_auto_review(),
            review: default_review(),
            develop: default_develop(),
            merge: default_merge(),
        }
    }
}

fn default_auto_review() -> Vec<String> {
    vec!["anyone".to_string()]
}
fn default_review() -> Vec<String> {
    vec!["collaborator".to_string()]
}
fn default_develop() -> Vec<String> {
    vec!["collaborator".to_string()]
}
fn default_merge() -> Vec<String> {
    vec!["write".to_string()]
}

const VALID_ASSOCIATIONS: &[&str] = &["anyone", "contributor", "collaborator", "member", "owner"];
const VALID_PERMISSION_LEVELS: &[&str] = &["read", "triage", "write", "maintain", "admin"];

impl Permissions {
    /// Fail-fast validation of every permission entry (spec 12 §6.3).
    pub fn validate(&self) -> anyhow::Result<()> {
        for key in [&self.auto_review, &self.review, &self.develop, &self.merge] {
            for entry in key {
                Self::validate_entry(entry)?;
            }
        }
        Ok(())
    }

    fn validate_entry(entry: &str) -> anyhow::Result<()> {
        let e = entry.trim().to_ascii_lowercase();
        if VALID_ASSOCIATIONS.contains(&e.as_str()) || VALID_PERMISSION_LEVELS.contains(&e.as_str())
        {
            return Ok(());
        }
        if let Some(rest) = e.strip_prefix('@') {
            if rest.is_empty() {
                bail!("invalid permission entry: empty @ name in {entry:?}");
            }
            if !rest.contains('/') {
                return Ok(()); // @user
            }
            let parts: Vec<&str> = rest.split('/').collect();
            if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
                return Ok(()); // @org/team
            }
        }
        bail!("invalid permission entry: {entry:?}")
    }

    pub fn get(&self, key: PermissionKey) -> &[String] {
        match key {
            PermissionKey::AutoReview => &self.auto_review,
            PermissionKey::Review => &self.review,
            PermissionKey::Develop => &self.develop,
            PermissionKey::Merge => &self.merge,
        }
    }
}

/// Command key being evaluated (spec 12 §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionKey {
    AutoReview,
    Review,
    Develop,
    Merge,
}

impl PermissionKey {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionKey::AutoReview => "auto_review",
            PermissionKey::Review => "review",
            PermissionKey::Develop => "develop",
            PermissionKey::Merge => "merge",
        }
    }
}

/// The actor whose permissions are being evaluated.
#[derive(Debug, Clone, Copy)]
pub struct Actor<'a> {
    pub login: &'a str,
    pub author_association: &'a str,
}

/// Evaluates `[permissions]` entries against a concrete actor, caching
/// collaborator-permission API results per user per run (spec 12 §5).
#[derive(Debug, Clone)]
pub struct PermissionsEvaluator {
    permissions: Permissions,
    cache: DashMap<String, crate::github::RepoPermission>,
}

impl PermissionsEvaluator {
    pub fn new(permissions: Permissions) -> Self {
        Self {
            permissions,
            cache: DashMap::new(),
        }
    }

    /// Evaluate a single command key. Any matching entry grants access (OR).
    pub async fn evaluate(
        &self,
        key: PermissionKey,
        gh: &crate::github::GitHubClient,
        repo: &crate::github::Repo,
        actor: Actor<'_>,
    ) -> bool {
        let entries = self.permissions.get(key);
        if entries.is_empty() {
            return false;
        }

        // First pass: free entries (author_association, @user, @org/team).
        for entry in entries {
            if self.evaluate_free_entry(entry, gh, repo, actor).await {
                return true;
            }
        }

        // Second pass: collaborator permission levels only if no free entry hit.
        let requires_level = entries
            .iter()
            .filter_map(|e| crate::github::RepoPermission::parse(e))
            .collect::<Vec<_>>();
        if requires_level.is_empty() {
            return false;
        }
        let Some(user_perm) = self.fetch_permission(gh, repo, actor.login).await else {
            return false;
        };
        requires_level.iter().any(|req| user_perm >= *req)
    }

    async fn evaluate_free_entry(
        &self,
        entry: &str,
        gh: &crate::github::GitHubClient,
        _repo: &crate::github::Repo,
        actor: Actor<'_>,
    ) -> bool {
        let e = entry.trim().to_ascii_lowercase();
        match e.as_str() {
            "anyone" => true,
            "contributor" => actor_association_matches(actor.author_association, "CONTRIBUTOR"),
            "collaborator" => actor_association_matches_one_of(
                actor.author_association,
                &["OWNER", "MEMBER", "COLLABORATOR"],
            ),
            "member" => {
                actor_association_matches_one_of(actor.author_association, &["OWNER", "MEMBER"])
            }
            "owner" => actor_association_matches(actor.author_association, "OWNER"),
            _ if e.starts_with('@') => {
                let rest = &e[1..];
                if !rest.contains('/') {
                    actor.login.eq_ignore_ascii_case(rest)
                } else {
                    let parts: Vec<&str> = rest.split('/').collect();
                    if parts.len() != 2 || parts[0].is_empty() || parts[1].is_empty() {
                        return false;
                    }
                    gh.check_team_membership(parts[0], parts[1], actor.login)
                        .await
                }
            }
            _ => false,
        }
    }

    async fn fetch_permission(
        &self,
        gh: &crate::github::GitHubClient,
        repo: &crate::github::Repo,
        login: &str,
    ) -> Option<crate::github::RepoPermission> {
        if login.is_empty() {
            return None;
        }
        if let Some(p) = self.cache.get(login) {
            return Some(*p);
        }
        match gh.get_collaborator_permission(repo, login).await {
            Ok(p) => {
                self.cache.insert(login.to_string(), p);
                Some(p)
            }
            Err(e) => {
                tracing::warn!("failed to fetch collaborator permission for {login}: {e}");
                None
            }
        }
    }
}

fn actor_association_matches(association: &str, expected: &str) -> bool {
    association.eq_ignore_ascii_case(expected)
}

fn actor_association_matches_one_of(association: &str, expected: &[&str]) -> bool {
    expected.iter().any(|e| association.eq_ignore_ascii_case(e))
}

/// env var (non-empty) > toml value; empty strings count as unset (GH Actions
/// interpolates missing vars as empty).
fn env_or(key: &str, toml: Option<String>) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty()).or(toml)
}

fn parse_thinking(raw: Option<String>) -> anyhow::Result<Option<ThinkingMode>> {
    match raw {
        None => Ok(None),
        Some(v) => ThinkingMode::parse(&v)
            .map(Some)
            .with_context(|| format!("invalid thinking mode: {v:?} (expected enabled|disabled)")),
    }
}

fn parse_effort(raw: Option<String>) -> anyhow::Result<Option<ReasoningEffort>> {
    match raw {
        None => Ok(None),
        Some(v) => ReasoningEffort::parse(&v).map(Some).with_context(|| {
            format!(
                "invalid reasoning_effort: {v:?} \
                 (expected none|minimal|low|medium|high|xhigh|max)"
            )
        }),
    }
}

fn parse_bool(raw: Option<String>) -> Option<anyhow::Result<bool>> {
    raw.map(|v| match v.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        other => bail!("invalid boolean: {other:?} (expected true|false)"),
    })
}

fn parse_ratio(raw: Option<String>) -> Option<anyhow::Result<f64>> {
    raw.map(|v| {
        v.trim()
            .parse::<f64>()
            .with_context(|| format!("invalid ratio: {v:?}"))
    })
}

fn parse_usize(raw: Option<String>) -> Option<anyhow::Result<usize>> {
    raw.map(|v| {
        v.trim()
            .parse::<usize>()
            .with_context(|| format!("invalid integer: {v:?}"))
    })
}

/// File structure of `.github/hoverstare.toml` (all optional)
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct TomlConfig {
    model: Option<String>,
    reformat_model: Option<String>,
    passes: Option<u8>,
    verify: Option<bool>,
    severity_threshold: Option<SeverityToml>,
    ignore: Option<Vec<String>>,
    max_diff_kb: Option<usize>,
    max_tool_calls: Option<u32>,
    timeout_secs: Option<u64>,
    review_drafts: Option<bool>,
    fail_closed: Option<bool>,
    status_checks: Option<bool>,
    instructions: Option<String>,
    set_temperature: Option<bool>,
    thinking: Option<String>,
    reasoning_effort: Option<String>,
    context_tokens: Option<u64>,
    compaction: Option<bool>,
    compaction_threshold_ratio: Option<f64>,
    compaction_keep_ratio: Option<f64>,
    summary_max_chars: Option<usize>,
    language: Option<String>,
    permissions: Option<Permissions>,
}

/// Rough bytes-per-token estimate for diff text (code is ASCII-heavy).
const BYTES_PER_TOKEN: usize = 4;
/// Divisor for the share of the context window the diff may occupy.
const DIFF_CONTEXT_DIVISOR: usize = 2;
/// Smallest accepted `context_tokens` value (spec 01).
const MIN_CONTEXT_TOKENS: u64 = 4096;

/// Built-in filter rules (spec 03): lockfiles / minified artifacts / CI directories
const BUILTIN_IGNORE: &[&str] = &[
    "**/Cargo.lock",
    "**/package-lock.json",
    "**/pnpm-lock.yaml",
    "**/yarn.lock",
    "**/poetry.lock",
    "**/go.sum",
    "**/composer.lock",
    "**/Gemfile.lock",
    "**/*.min.js",
    "**/*.min.css",
    "**/*.map",
    ".github/**",
];

impl Config {
    /// Convert the temperature argument according to set_temperature (endpoints
    /// that do not support a custom temperature get None, and rig omits the
    /// field, using the provider default)
    pub fn temp(&self, t: f64) -> Option<f64> {
        self.set_temperature.then_some(t)
    }

    /// Permission evaluator for the current run, seeded with the loaded config.
    pub fn permissions_evaluator(&self) -> PermissionsEvaluator {
        PermissionsEvaluator::new(self.permissions.clone())
    }

    pub fn load() -> anyhow::Result<Config> {
        let workspace = std::env::var("GITHUB_WORKSPACE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        let toml = Self::load_toml(&workspace)?;
        Self::merge(toml, workspace)
    }

    fn load_toml(workspace: &Path) -> anyhow::Result<TomlConfig> {
        let path = workspace.join(".github/hoverstare.toml");
        if !path.exists() {
            return Ok(TomlConfig::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("failed to parse {}", path.display()))
    }

    fn merge(t: TomlConfig, workspace: PathBuf) -> anyhow::Result<Config> {
        // model: HOVERSTARE_MODEL env var > toml > default
        let model = std::env::var("HOVERSTARE_MODEL")
            .ok()
            .filter(|v| !v.is_empty())
            .or(t.model)
            .unwrap_or_else(|| "claude-sonnet-4-6".to_string());
        let reformat_model = std::env::var("HOVERSTARE_REFORMAT_MODEL")
            .ok()
            .filter(|v| !v.is_empty())
            .or(t.reformat_model)
            .unwrap_or_else(|| "claude-haiku-4-5".to_string());
        let passes = t.passes.unwrap_or(3);
        let max_diff_kb = t.max_diff_kb.unwrap_or(400);
        let max_tool_calls = t.max_tool_calls.unwrap_or(20);
        let timeout_secs = t.timeout_secs.unwrap_or(900);

        // Reasoning tuning (spec 01/04): both fields are optional and are only
        // sent when configured (see ReasoningOptions).
        let reasoning = ReasoningOptions {
            thinking: parse_thinking(env_or("HOVERSTARE_THINKING", t.thinking))?,
            effort: parse_effort(env_or("HOVERSTARE_REASONING_EFFORT", t.reasoning_effort))?,
        };
        let context_tokens = match env_or(
            "HOVERSTARE_CONTEXT_TOKENS",
            t.context_tokens.map(|v| v.to_string()),
        ) {
            Some(raw) => Some(
                raw.trim()
                    .parse::<u64>()
                    .with_context(|| format!("invalid HOVERSTARE_CONTEXT_TOKENS: {raw:?}"))?,
            ),
            None => None,
        };

        // Context compaction (spec 13): env > toml > defaults.
        let compaction = crate::agent::compaction::CompactionConfig {
            enabled: parse_bool(env_or("HOVERSTARE_COMPACTION", None))
                .transpose()?
                .or(t.compaction)
                .unwrap_or(true),
            threshold_ratio: parse_ratio(env_or("HOVERSTARE_COMPACTION_THRESHOLD_RATIO", None))
                .transpose()?
                .or(t.compaction_threshold_ratio)
                .unwrap_or(0.75),
            keep_ratio: parse_ratio(env_or("HOVERSTARE_COMPACTION_KEEP_RATIO", None))
                .transpose()?
                .or(t.compaction_keep_ratio)
                .unwrap_or(0.25),
            summary_max_chars: parse_usize(env_or("HOVERSTARE_SUMMARY_MAX_CHARS", None))
                .transpose()?
                .or(t.summary_max_chars)
                .unwrap_or(4_000),
        };

        // Validation (spec 01)
        if model.trim().is_empty() {
            bail!("model must not be empty");
        }
        if passes == 0 {
            bail!("passes must be >= 1");
        }
        if max_diff_kb < 50 {
            bail!("max_diff_kb must be >= 50 (got {max_diff_kb})");
        }
        if let Some(tokens) = context_tokens
            && tokens < MIN_CONTEXT_TOKENS
        {
            bail!("context_tokens must be >= {MIN_CONTEXT_TOKENS} (got {tokens})");
        }
        // Diff budget derived from the model context window (spec 01): the diff
        // is estimated at BYTES_PER_TOKEN bytes/token and may take at most
        // 1/DIFF_CONTEXT_DIVISOR of the window. A larger max_diff_kb is clamped
        // (the prompt must still fit), a smaller one is untouched.
        let max_diff_kb = match context_tokens {
            Some(tokens) => {
                let cap_kb = (tokens as usize * BYTES_PER_TOKEN / DIFF_CONTEXT_DIVISOR) / 1024;
                if max_diff_kb > cap_kb {
                    tracing::warn!(
                        "max_diff_kb={max_diff_kb} exceeds the {cap_kb}KB that fits the \
                         configured context_tokens={tokens}; clamping"
                    );
                }
                max_diff_kb.min(cap_kb.max(1))
            }
            None => max_diff_kb,
        };
        if max_tool_calls == 0 {
            bail!("max_tool_calls must be >= 1");
        }
        if !(compaction.keep_ratio > 0.0
            && compaction.keep_ratio < compaction.threshold_ratio
            && compaction.threshold_ratio < 1.0)
        {
            bail!(
                "compaction ratios must satisfy 0 < compaction_keep_ratio < \
                 compaction_threshold_ratio < 1 (got {} and {})",
                compaction.keep_ratio,
                compaction.threshold_ratio
            );
        }
        if compaction.summary_max_chars < 200 {
            bail!(
                "summary_max_chars must be >= 200 (got {})",
                compaction.summary_max_chars
            );
        }

        // ignore: built-in + user-configured
        let mut builder = GlobSetBuilder::new();
        for pat in BUILTIN_IGNORE {
            builder.add(Glob::new(pat)?);
        }
        for pat in t.ignore.unwrap_or_default() {
            builder.add(Glob::new(&pat).with_context(|| format!("invalid ignore glob: {pat:?}"))?);
        }
        let ignore = builder.build()?;

        // LLM credentials: OPENAI_API_KEY takes precedence (OpenAI-compatible path, incl. Kimi Code endpoints)
        let llm = match (
            std::env::var("OPENAI_API_KEY"),
            std::env::var("ANTHROPIC_API_KEY"),
        ) {
            (Ok(key), _) if !key.is_empty() => LlmCredentials::OpenAICompatible {
                key: SecretString::from(key),
                // empty string (e.g. unset Actions var interpolation) counts as unset
                base_url: std::env::var("OPENAI_BASE_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| "https://api.openai.com/v1".to_string()),
            },
            (_, Ok(key)) if !key.is_empty() => LlmCredentials::Anthropic {
                key: SecretString::from(key),
                base_url: std::env::var("ANTHROPIC_BASE_URL").ok(),
            },
            _ => bail!(
                "missing LLM credentials: set OPENAI_API_KEY (optionally with OPENAI_BASE_URL) or ANTHROPIC_API_KEY"
            ),
        };

        // Identity token (comments/reviews/API): GITHUB_TOKEN only (App token in Actions).
        let github_token = std::env::var("GITHUB_TOKEN")
            .ok()
            .filter(|v| !v.is_empty())
            .map(SecretString::from);
        // GH_PAT no longer hijacks the identity (spec 07/11): it only serves
        // resolveReviewThread fallback and dev-mode pushes.
        let gh_pat = std::env::var("GH_PAT")
            .ok()
            .filter(|v| !v.is_empty())
            .map(SecretString::from);

        // Fine-grained permissions (spec 12): toml > defaults
        let mut permissions = t.permissions.unwrap_or_default();
        // Empty lists fall back to defaults (spec 12 §6.3)
        if permissions.auto_review.is_empty() {
            permissions.auto_review = default_auto_review();
        }
        if permissions.review.is_empty() {
            permissions.review = default_review();
        }
        if permissions.develop.is_empty() {
            permissions.develop = default_develop();
        }
        if permissions.merge.is_empty() {
            permissions.merge = default_merge();
        }
        permissions.validate()?;

        Ok(Config {
            model,
            reformat_model,
            passes,
            verify: t.verify.unwrap_or(true),
            severity_threshold: t.severity_threshold.unwrap_or(SeverityToml::Medium).into(),
            ignore,
            max_diff_kb,
            max_tool_calls,
            timeout_secs,
            review_drafts: t.review_drafts.unwrap_or(false),
            fail_closed: t.fail_closed.unwrap_or(false),
            status_checks: t.status_checks.unwrap_or(false),
            instructions: t.instructions.unwrap_or_default(),
            set_temperature: t.set_temperature.unwrap_or(true),
            reasoning,
            context_tokens,
            compaction,
            language: crate::i18n::Lang::resolve(
                std::env::var("HOVERSTARE_LANGUAGE").ok().as_deref(),
                t.language.as_deref(),
            ),
            github_token,
            gh_pat,
            llm,
            workspace,
            permissions,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret as _;

    #[test]
    fn gh_pat_does_not_hijack_identity() {
        // spec 07/11: GH_PAT is narrow-duty (resolve/push); identity stays GITHUB_TOKEN.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "k");
            std::env::set_var("GITHUB_TOKEN", "identity-tok");
            std::env::set_var("GH_PAT", "pat-tok");
        }
        let cfg = merge_str("").unwrap();
        assert_eq!(cfg.github_token.unwrap().expose_secret(), "identity-tok");
        assert_eq!(cfg.gh_pat.unwrap().expose_secret(), "pat-tok");
        unsafe {
            std::env::remove_var("GH_PAT");
            std::env::remove_var("GITHUB_TOKEN");
        }
    }

    fn merge_str(toml: &str) -> anyhow::Result<Config> {
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
        let t: TomlConfig = toml::from_str(toml)?;
        Config::merge(t, PathBuf::from("/tmp/x"))
    }

    #[test]
    fn defaults_apply_on_empty_toml() {
        let c = merge_str("").unwrap();
        assert_eq!(c.model, "claude-sonnet-4-6");
        assert_eq!(c.passes, 3);
        assert!(c.verify);
        assert_eq!(c.severity_threshold, Severity::Medium);
        assert!(!c.fail_closed);
        assert!(c.ignore.is_match("Cargo.lock"));
        assert!(c.ignore.is_match("web/app.min.js"));
        assert!(c.ignore.is_match(".github/workflows/ci.yml"));
    }

    #[test]
    fn toml_overrides_defaults() {
        let c = merge_str(
            r#"model = "kimi-for-coding"
               passes = 1
               severity_threshold = "high"
               ignore = ["vendor/**"]"#,
        )
        .unwrap();
        assert_eq!(c.model, "kimi-for-coding");
        assert_eq!(c.passes, 1);
        assert_eq!(c.severity_threshold, Severity::High);
        assert!(c.ignore.is_match("vendor/a/b.rs"));
    }

    #[test]
    fn invalid_values_rejected() {
        assert!(merge_str("passes = 0").is_err());
        assert!(merge_str("max_diff_kb = 10").is_err());
        assert!(merge_str("max_tool_calls = 0").is_err());
        assert!(merge_str(r#"severity_threshold = "urgent""#).is_err());
        assert!(merge_str(r#"ignore = ["[bad""#).is_err());
    }

    #[test]
    fn unknown_fields_rejected() {
        assert!(merge_str("unknown_key = 1").is_err());
    }

    #[test]
    fn reasoning_defaults_to_unset() {
        // spec 01/04: nothing configured => no extra request body fields at all
        let c = merge_str("").unwrap();
        assert!(c.reasoning.is_empty());
        assert!(c.reasoning.openai_params().is_none());
        assert_eq!(c.context_tokens, None);
        assert_eq!(c.max_diff_kb, 400);
    }

    #[test]
    fn reasoning_toml_overrides() {
        let c = merge_str(
            r#"thinking = "enabled"
               reasoning_effort = "medium"
               context_tokens = 1000000"#,
        )
        .unwrap();
        assert_eq!(
            c.reasoning.openai_params().unwrap(),
            serde_json::json!({"thinking": {"type": "enabled"}, "reasoning_effort": "medium"})
        );
        assert_eq!(c.context_tokens, Some(1_000_000));
        // 1M tokens is far larger than the default diff budget: not clamped
        assert_eq!(c.max_diff_kb, 400);
    }

    #[test]
    fn reasoning_invalid_values_rejected() {
        assert!(merge_str(r#"thinking = "on""#).is_err());
        assert!(merge_str(r#"reasoning_effort = "ultra""#).is_err());
        assert!(merge_str("context_tokens = 100").is_err());
    }

    #[test]
    fn compaction_defaults_and_overrides() {
        // spec 13 defaults
        let c = merge_str("").unwrap();
        assert!(c.compaction.enabled);
        assert_eq!(c.compaction.threshold_ratio, 0.75);
        assert_eq!(c.compaction.keep_ratio, 0.25);
        assert_eq!(c.compaction.summary_max_chars, 4_000);
        let c = merge_str(
            r#"compaction = false
               compaction_threshold_ratio = 0.5
               compaction_keep_ratio = 0.1
               summary_max_chars = 1_000"#,
        )
        .unwrap();
        assert!(!c.compaction.enabled);
        assert_eq!(c.compaction.threshold_ratio, 0.5);
        assert_eq!(c.compaction.keep_ratio, 0.1);
        assert_eq!(c.compaction.summary_max_chars, 1_000);
    }

    #[test]
    fn compaction_invalid_values_rejected() {
        // the keep share must leave room for the summary that replaces the rest
        assert!(
            merge_str("compaction_keep_ratio = 0.9\ncompaction_threshold_ratio = 0.5").is_err()
        );
        assert!(merge_str("compaction_keep_ratio = 0.0").is_err());
        assert!(merge_str("compaction_threshold_ratio = 1.0").is_err());
        assert!(merge_str("summary_max_chars = 10").is_err());
        assert!(merge_str("compaction = \"maybe\"").is_err());
    }

    #[test]
    fn context_tokens_clamps_diff_budget() {
        // 32K tokens -> 32K*4/2 bytes = 64KB of diff at most
        let c = merge_str("context_tokens = 32768").unwrap();
        assert_eq!(c.max_diff_kb, 64);
        // 256K tokens -> 256K*4/2 bytes = 512KB of diff at most
        let c = merge_str("context_tokens = 262144\nmax_diff_kb = 5000").unwrap();
        assert_eq!(c.max_diff_kb, 512);
        // a budget below the cap is left alone
        let c = merge_str("context_tokens = 262144\nmax_diff_kb = 100").unwrap();
        assert_eq!(c.max_diff_kb, 100);
    }

    #[test]
    fn permissions_unknown_fields_rejected() {
        assert!(merge_str("[permissions]\nunknown_key = 1").is_err());
    }

    #[test]
    fn permissions_defaults_match_current_behavior() {
        let c = merge_str("").unwrap();
        assert_eq!(c.permissions.auto_review, vec!["anyone"]);
        assert_eq!(c.permissions.review, vec!["collaborator"]);
        assert_eq!(c.permissions.develop, vec!["collaborator"]);
        assert_eq!(c.permissions.merge, vec!["write"]);
    }

    #[test]
    fn permissions_toml_overrides() {
        let c = merge_str(
            r#"[permissions]
auto_review = ["owner"]
review = ["member", "@alice"]
develop = ["@org/team"]
merge = ["admin", "maintain"]"#,
        )
        .unwrap();
        assert_eq!(c.permissions.auto_review, vec!["owner"]);
        assert_eq!(c.permissions.review, vec!["member", "@alice"]);
        assert_eq!(c.permissions.develop, vec!["@org/team"]);
        assert_eq!(c.permissions.merge, vec!["admin", "maintain"]);
    }

    #[test]
    fn permissions_empty_list_falls_back_to_default() {
        let c = merge_str("[permissions]\nreview = []").unwrap();
        assert_eq!(c.permissions.review, vec!["collaborator"]);
    }

    #[test]
    fn permissions_invalid_entries_rejected() {
        assert!(
            merge_str(
                r#"[permissions]
review = ["everyone"]"#,
            )
            .is_err()
        );
        assert!(
            merge_str(
                r#"[permissions]
review = ["@foo/bar/baz"]"#,
            )
            .is_err()
        );
        assert!(
            merge_str(
                r#"[permissions]
review = ["@"]"#,
            )
            .is_err()
        );
        assert!(
            merge_str(
                r#"[permissions]
review = ["@/team"]"#,
            )
            .is_err()
        );
    }

    #[test]
    fn permissions_association_matrix() {
        use crate::github::Repo;
        let perms = Permissions {
            auto_review: vec!["anyone".into()],
            review: vec!["collaborator".into()],
            develop: vec!["member".into()],
            merge: vec!["owner".into()],
        };
        let evaluator = PermissionsEvaluator::new(perms);
        let gh = crate::github::GitHubClient::new(None).unwrap(); // no real calls needed
        let repo = Repo::parse("o/r").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let check = |login, assoc, key| {
            rt.block_on(evaluator.evaluate(
                key,
                &gh,
                &repo,
                Actor {
                    login,
                    author_association: assoc,
                },
            ))
        };

        assert!(check("x", "NONE", PermissionKey::AutoReview));
        assert!(check("x", "OWNER", PermissionKey::Review));
        assert!(check("x", "MEMBER", PermissionKey::Review));
        assert!(check("x", "COLLABORATOR", PermissionKey::Review));
        assert!(!check("x", "CONTRIBUTOR", PermissionKey::Review));
        assert!(check("x", "MEMBER", PermissionKey::Develop));
        assert!(check("x", "OWNER", PermissionKey::Develop));
        assert!(!check("x", "COLLABORATOR", PermissionKey::Develop));
        assert!(check("x", "OWNER", PermissionKey::Merge));
        assert!(!check("x", "MEMBER", PermissionKey::Merge));
    }

    #[test]
    fn permissions_user_entry_is_case_insensitive() {
        use crate::github::Repo;
        let perms = Permissions {
            auto_review: vec!["anyone".into()],
            review: vec!["@Alice".into()],
            develop: vec!["collaborator".into()],
            merge: vec!["write".into()],
        };
        let evaluator = PermissionsEvaluator::new(perms);
        let gh = crate::github::GitHubClient::new(None).unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let hit = rt.block_on(evaluator.evaluate(
            PermissionKey::Review,
            &gh,
            &repo,
            Actor {
                login: "alice",
                author_association: "NONE",
            },
        ));
        assert!(hit);
        let miss = rt.block_on(evaluator.evaluate(
            PermissionKey::Review,
            &gh,
            &repo,
            Actor {
                login: "bob",
                author_association: "OWNER",
            },
        ));
        assert!(!miss);
    }
}
