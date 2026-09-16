//! RigBackend: AgentBackend implementation based on rig-core (spec 04)
//!
//! This file is the only module allowed to `use rig::*`; rig types must not leak out.
//! Custom endpoints such as Kimi Code go through the OpenAI-compatible path
//! (`CompletionsClient` + `base_url`, verified in a spike, see spikes/rig-kimi-probe).
//!
//! Toolset: the framework-agnostic implementation lives in agent/tools.rs; this file
//! only provides thin wrappers around the rig Tool trait.

use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;

use async_trait::async_trait;
use rig::client::CompletionClient;
use rig::completion::{Prompt, PromptError, ToolDefinition};
use rig::providers::{anthropic, openai};
use rig::tool::Tool;
use secrecy::ExposeSecret;
use serde::Deserialize;
use serde_json::json;

use crate::agent::tools::{self, ToolShared};
use crate::agent::{
    AgentBackend, AgentError, ReasoningOptions, ReviewRequest, ReviewRun, ToolProfile, Usage,
};
use crate::config::LlmCredentials;

/// Output limit per call (the findings JSON is never large)
const MAX_OUTPUT_TOKENS: u64 = 8192;
/// Headroom rig's max turns leaves on top of the tool budget (final turn)
const TURN_MARGIN: u32 = 2;

/// Uniform prompt future type across providers
type PromptFuture = Pin<Box<dyn Future<Output = Result<String, PromptError>> + Send>>;

pub struct RigBackend {
    creds: LlmCredentials,
    /// Thinking/reasoning tuning (spec 01/04); empty = send nothing extra
    reasoning: ReasoningOptions,
}

impl RigBackend {
    pub fn new(creds: LlmCredentials, reasoning: ReasoningOptions) -> RigBackend {
        RigBackend { creds, reasoning }
    }

    /// Convenience constructor from the loaded config.
    pub fn from_config(cfg: &crate::config::Config) -> RigBackend {
        RigBackend::new(cfg.llm.clone(), cfg.reasoning)
    }
}

#[async_trait]
impl AgentBackend for RigBackend {
    async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError> {
        let shared = req.tools.shared.clone();
        let fut = self.build_prompt_future(&req)?;
        let raw = match tokio::time::timeout(req.budget.timeout, fut).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(AgentError::Backend(e.to_string())),
            Err(_) => return Err(AgentError::Timeout(req.budget.timeout)),
        };
        let tool_trace = shared.map(|s| s.trace()).unwrap_or_default();
        // M7: token usage accounting still to be added
        Ok(ReviewRun {
            raw_output: raw,
            tool_trace,
            usage: Usage::default(),
        })
    }
}

impl RigBackend {
    /// Build the prompt future per provider (the two branches have different types, so box them)
    fn build_prompt_future(&self, req: &ReviewRequest) -> Result<PromptFuture, AgentError> {
        let model = req.model.clone();
        let sys = req.system_prompt.clone();
        let user = req.user_prompt.clone();
        let temp = req.temperature;
        let shared = req.tools.shared.clone();
        let max_turns = (req.budget.max_tool_calls + TURN_MARGIN) as usize;

        match &self.creds {
            LlmCredentials::Anthropic { key, .. } => {
                let client = anthropic::Client::builder()
                    .api_key(key.expose_secret())
                    .build()
                    .map_err(|e| {
                        AgentError::Backend(format!("failed to build anthropic client: {e}"))
                    })?;
                let mut builder = client
                    .agent(&model)
                    .preamble(&sys)
                    .max_tokens(MAX_OUTPUT_TOKENS);
                if let Some(t) = temp {
                    builder = builder.temperature(t);
                }
                // Anthropic's native thinking config has different semantics
                // (budget_tokens), so provider tuning is not forwarded here.
                if !self.reasoning.is_empty() {
                    tracing::debug!(
                        "thinking/reasoning_effort configured but not sent on the Anthropic path"
                    );
                }
                Ok(match shared {
                    Some(shared) => {
                        let agent =
                            with_tools(builder, shared, req.tools.profile, max_turns).build();
                        Box::pin(agent.prompt(user).into_future())
                    }
                    None => {
                        let agent = builder.build();
                        Box::pin(agent.prompt(user).into_future())
                    }
                })
            }
            LlmCredentials::OpenAICompatible { key, base_url } => {
                let client = openai::CompletionsClient::builder()
                    .api_key(key.expose_secret())
                    .base_url(base_url)
                    .build()
                    .map_err(|e| {
                        AgentError::Backend(format!(
                            "failed to build openai-compatible client: {e}"
                        ))
                    })?;
                let mut builder = client
                    .agent(&model)
                    .preamble(&sys)
                    .max_tokens(MAX_OUTPUT_TOKENS);
                if let Some(t) = temp {
                    builder = builder.temperature(t);
                }
                // DeepSeek-style thinking mode: merged into the request body as
                // {"thinking":{"type":"enabled"},"reasoning_effort":"medium"}.
                // Nothing is sent when the config leaves both fields unset.
                if let Some(params) = self.reasoning.openai_params() {
                    builder = builder.additional_params(params);
                }
                Ok(match shared {
                    Some(shared) => {
                        let agent =
                            with_tools(builder, shared, req.tools.profile, max_turns).build();
                        Box::pin(agent.prompt(user).into_future())
                    }
                    None => {
                        let agent = builder.build();
                        Box::pin(agent.prompt(user).into_future())
                    }
                })
            }
        }
    }
}

/// Register the toolset for the given profile + turn limit
/// (generic over both providers' AgentBuilder).
/// ReadOnly: 4 read tools. ReadWrite (develop loop): + edit_file/write_file.
fn with_tools<M, P>(
    builder: rig::agent::AgentBuilder<M, P, rig::agent::NoToolConfig>,
    shared: Arc<ToolShared>,
    profile: ToolProfile,
    max_turns: usize,
) -> rig::agent::AgentBuilder<M, P, rig::agent::WithBuilderTools>
where
    M: rig::completion::CompletionModel,
    P: rig::agent::PromptHook<M>,
{
    let b = builder
        .tool(ReadFileTool {
            shared: shared.clone(),
        })
        .tool(GrepTool {
            shared: shared.clone(),
        })
        .tool(GlobTool {
            shared: shared.clone(),
        })
        .tool(ShowBaseFileTool {
            shared: shared.clone(),
        });
    let b = if profile == ToolProfile::ReadWrite {
        b.tool(EditFileTool {
            shared: shared.clone(),
        })
        .tool(WriteFileTool { shared })
    } else {
        b
    };
    b.default_max_turns(max_turns)
}

// ---------------------------------------------------------------------------
// rig Tool thin wrappers (pass-through to the framework-agnostic implementation in agent/tools.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ToolErr(String);

macro_rules! impl_tool {
    ($ty:ident, $name:literal, $desc:literal, $args:ident, $params:expr, |$s:ident, $a:ident| $body:expr) => {
        struct $ty {
            shared: Arc<ToolShared>,
        }

        impl Tool for $ty {
            const NAME: &'static str = $name;
            type Error = ToolErr;
            type Args = $args;
            type Output = String;

            async fn definition(&self, _prompt: String) -> ToolDefinition {
                ToolDefinition {
                    name: $name.to_string(),
                    description: $desc.to_string(),
                    parameters: $params,
                }
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                let summary = format!("{:?}", args);
                let summary: String = summary.chars().take(200).collect();
                let $s = &self.shared;
                let $a = &args;
                Ok(self.shared.run($name, summary, $body).await)
            }
        }
    };
}

#[derive(Deserialize, Debug)]
struct ReadFileArgs {
    /// File path relative to the repository root
    path: String,
    /// Start line (1-based, inclusive)
    start_line: Option<u64>,
    /// End line (inclusive)
    end_line: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct GrepArgs {
    /// Regular expression
    pattern: String,
    /// Optional: limit to a file or directory (relative to the repository root)
    path: Option<String>,
    /// Optional: number of context lines shown per match
    context_lines: Option<u32>,
}

#[derive(Deserialize, Debug)]
struct GlobArgs {
    /// glob pattern (e.g. src/**/*.rs)
    pattern: String,
}

#[derive(Deserialize, Debug)]
struct ShowBaseFileArgs {
    /// File path relative to the repository root
    path: String,
}

impl_tool!(
    ReadFileTool,
    "read_file",
    "Read the contents of a file in the repository (with line numbers). Use it to see context around the diff or symbol definitions.",
    ReadFileArgs,
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path relative to the repository root"},
            "start_line": {"type": "integer", "description": "Start line (1-based, inclusive), defaults to the beginning"},
            "end_line": {"type": "integer", "description": "End line (inclusive), defaults to the end of file"}
        },
        "required": ["path"]
    }),
    |s, a| tools::read_file(s, &a.path, a.start_line, a.end_line)
);

impl_tool!(
    GrepTool,
    "grep",
    "Search the repository with a regular expression. Use it to find call sites of a function or type.",
    GrepArgs,
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regular expression"},
            "path": {"type": "string", "description": "Optional: limit to a file or directory"},
            "context_lines": {"type": "integer", "description": "Optional: context lines around each match"}
        },
        "required": ["pattern"]
    }),
    |s, a| tools::grep(s, &a.pattern, a.path.as_deref(), a.context_lines)
);

impl_tool!(
    GlobTool,
    "glob",
    "Find files matching a glob pattern. Use it to locate related files.",
    GlobArgs,
    json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Glob pattern (e.g. src/**/*.rs)"}
        },
        "required": ["pattern"]
    }),
    |s, a| tools::glob(s, &a.pattern)
);

impl_tool!(
    ShowBaseFileTool,
    "show_base_file",
    "Read the file as it exists on the base branch (the PR target branch). Use it to compare pre-change behavior.",
    ShowBaseFileArgs,
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path relative to the repository root"}
        },
        "required": ["path"]
    }),
    |s, a| tools::show_base_file(s, &a.path)
);

#[derive(Deserialize, Debug)]
struct EditFileArgs {
    /// File path relative to the repository root (must already exist)
    path: String,
    /// Exact text to replace; must occur exactly once in the file
    old_string: String,
    /// Replacement text
    new_string: String,
}

#[derive(Deserialize, Debug)]
struct WriteFileArgs {
    /// File path relative to the repository root (created or overwritten)
    path: String,
    /// Full file content
    content: String,
}

impl_tool!(
    EditFileTool,
    "edit_file",
    "Replace exact text in an existing file. old_string must match exactly once — include enough surrounding context to make it unique.",
    EditFileArgs,
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path relative to the repository root"},
            "old_string": {"type": "string", "description": "Exact text to replace (must occur exactly once in the file)"},
            "new_string": {"type": "string", "description": "Replacement text"}
        },
        "required": ["path", "old_string", "new_string"]
    }),
    |s, a| tools::edit_file(s, &a.path, &a.old_string, &a.new_string)
);

impl_tool!(
    WriteFileTool,
    "write_file",
    "Create a new file or overwrite an existing file with the given content. Prefer edit_file for targeted changes to existing files.",
    WriteFileArgs,
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string", "description": "File path relative to the repository root"},
            "content": {"type": "string", "description": "Full file content"}
        },
        "required": ["path", "content"]
    }),
    |s, a| tools::write_file(s, &a.path, &a.content)
);
