//! Agent backend abstraction (spec 04)
//!
//! `AgentBackend` is the framework switch point: v1 is implemented by
//! `rig_backend::RigBackend`, and can later be replaced by a self-built
//! NativeBackend. The trait and its request/response types contain no
//! framework types.
//!
//! Since spec 13 the multi-turn loop itself is owned by this crate
//! (`agent_loop`) and a provider is reached through [`ChatClient`]: compaction
//! has to replace conversation history, and history has to be ours for that.

pub mod agent_loop;
pub mod compaction;
pub mod rig_backend;
pub mod tools;

use std::sync::Arc;
use std::time::Duration;

/// Which tool set the model gets (spec 11 §4).
/// Review is ALWAYS ReadOnly; only the develop loop uses ReadWrite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ToolProfile {
    #[default]
    ReadOnly,
    ReadWrite,
}

/// Tool registry: `shared: None` = pure single-turn mode without tools
#[derive(Debug, Clone, Default)]
pub struct ToolRegistry {
    pub shared: Option<Arc<tools::ToolShared>>,
    pub profile: ToolProfile,
}

#[derive(Debug)]
pub struct ReviewRequest {
    /// System prompt: role + JSON contract + safety constraints
    pub system_prompt: String,
    /// User prompt: diff + file list + instructions
    pub user_prompt: String,
    pub tools: ToolRegistry,
    pub budget: Budget,
    pub model: String,
    pub temperature: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// Tool-call budget for the agentic loop (also bounds rig's max turns)
    pub max_tool_calls: u32,
    pub timeout: Duration,
}

#[derive(Debug, Default)]
pub struct ReviewRun {
    /// Final model text (should be JSON, but not guaranteed — see the tolerant parsing in the findings module)
    pub raw_output: String,
    /// Tool-call trace (for debugging/replay tests)
    pub tool_trace: Vec<ToolCallRecord>,
    /// Enabled in M7 (cost accounting)
    #[allow(dead_code)]
    pub usage: Usage,
}

#[derive(Debug, Clone)]
pub struct ToolCallRecord {
    pub name: String,
    pub args_summary: String,
    pub duration: Duration,
    pub result_bytes: usize,
}

#[derive(Debug, Default, Clone, Copy)]
#[allow(dead_code)] // M7 (cost accounting)
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn add(&mut self, other: Usage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

// ---------------------------------------------------------------------------
// Provider-agnostic chat contract (spec 13)
// ---------------------------------------------------------------------------

/// One tool call requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

/// One item of the conversation the loop owns.
#[derive(Debug, Clone)]
pub enum ConversationItem {
    User {
        text: String,
    },
    Assistant {
        text: String,
        tool_calls: Vec<ToolCall>,
    },
    ToolResult {
        call_id: String,
        text: String,
    },
}

/// One provider call: system prompt, conversation so far, tool menu.
#[derive(Debug, Clone)]
pub struct ChatCall {
    pub model: String,
    pub system_prompt: String,
    pub messages: Vec<ConversationItem>,
    pub tools: Vec<tools::ToolSpec>,
    pub temperature: Option<f64>,
    pub max_tokens: u64,
}

/// One provider answer.
#[derive(Debug, Clone, Default)]
pub struct ChatReply {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// Whether the provider returned reasoning for this answer. A thinking
    /// model that answers only in its reasoning channel reaches us as an empty
    /// reply, and this is the only way to tell that apart from a real silence.
    pub had_reasoning: bool,
}

/// The provider seam. Implementations must not leak provider types through it.
#[async_trait::async_trait]
pub trait ChatClient: Send + Sync {
    async fn complete(&self, call: ChatCall) -> Result<ChatReply, AgentError>;
}

/// Thinking-mode switch (spec 01 `thinking`), DeepSeek/OpenAI-compatible semantic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingMode {
    Enabled,
    Disabled,
}

impl ThinkingMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "enabled" => Some(Self::Enabled),
            "disabled" => Some(Self::Disabled),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
        }
    }
}

/// Reasoning effort (spec 01 `reasoning_effort`). Values mirror the DeepSeek
/// scale; the server maps `minimal/low -> low`, `medium/high/xhigh -> high`,
/// `max -> max`. `None` is the API-level switch (config value "none") that
/// turns thinking mode off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ReasoningEffort {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Self::None),
            "minimal" => Some(Self::Minimal),
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            "xhigh" => Some(Self::Xhigh),
            "max" => Some(Self::Max),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// Provider-side generation tuning (spec 01/04). Both fields are `Option` so
/// that an unconfigured deployment sends **no** extra body fields at all
/// (endpoints that predate reasoning — e.g. kimi-for-coding — reject unknown
/// fields with 400).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReasoningOptions {
    /// None = omit `thinking` from the request
    pub thinking: Option<ThinkingMode>,
    /// None = omit `reasoning_effort` from the request
    pub effort: Option<ReasoningEffort>,
}

impl ReasoningOptions {
    /// Nothing configured: callers must not send any reasoning field.
    pub fn is_empty(&self) -> bool {
        self.thinking.is_none() && self.effort.is_none()
    }

    /// Whether thinking mode ends up disabled (`thinking = "disabled"` or
    /// `reasoning_effort = "none"`).
    pub fn thinking_disabled(&self) -> bool {
        self.thinking == Some(ThinkingMode::Disabled) || self.effort == Some(ReasoningEffort::None)
    }

    /// The request-body fragment for OpenAI-compatible endpoints:
    /// `{"thinking":{"type":"enabled"},"reasoning_effort":"medium"}`.
    /// None when nothing is configured.
    pub fn openai_params(&self) -> Option<serde_json::Value> {
        if self.is_empty() {
            return None;
        }
        let disabled = self.thinking_disabled();
        let thinking = if disabled { "disabled" } else { "enabled" };
        let mut params = serde_json::Map::new();
        params.insert(
            "thinking".to_string(),
            serde_json::json!({ "type": thinking }),
        );
        if !disabled && let Some(effort) = self.effort {
            params.insert(
                "reasoning_effort".to_string(),
                serde_json::json!(effort.as_str()),
            );
        }
        Some(serde_json::Value::Object(params))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent call timed out ({0:?})")]
    Timeout(Duration),
    #[error("agent call failed: {0}")]
    Backend(String),
    /// The provider refused the request for exceeding its window (spec 13).
    /// Kept apart from `Backend` so the loop can recover instead of failing.
    #[error("request exceeds the model window: {0}")]
    ContextOverflow(String),
}

#[async_trait::async_trait]
pub trait AgentBackend: Send + Sync {
    async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_options_are_empty_by_default() {
        let opts = ReasoningOptions::default();
        assert!(opts.is_empty());
        assert!(opts.openai_params().is_none());
    }

    #[test]
    fn reasoning_params_enable_thinking_and_effort() {
        let opts = ReasoningOptions {
            thinking: Some(ThinkingMode::Enabled),
            effort: Some(ReasoningEffort::Medium),
        };
        assert_eq!(
            opts.openai_params().unwrap(),
            serde_json::json!({"thinking": {"type": "enabled"}, "reasoning_effort": "medium"})
        );
    }

    #[test]
    fn reasoning_effort_none_disables_thinking_and_drops_effort() {
        let opts = ReasoningOptions {
            thinking: None,
            effort: Some(ReasoningEffort::None),
        };
        assert_eq!(
            opts.openai_params().unwrap(),
            serde_json::json!({"thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn thinking_disabled_wins_over_effort() {
        let opts = ReasoningOptions {
            thinking: Some(ThinkingMode::Disabled),
            effort: Some(ReasoningEffort::High),
        };
        assert_eq!(
            opts.openai_params().unwrap(),
            serde_json::json!({"thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn reasoning_parse_matrix() {
        assert_eq!(
            ThinkingMode::parse(" ENABLED "),
            Some(ThinkingMode::Enabled)
        );
        assert_eq!(ThinkingMode::parse("on"), None);
        assert_eq!(
            ReasoningEffort::parse("xhigh"),
            Some(ReasoningEffort::Xhigh)
        );
        assert_eq!(ReasoningEffort::parse("ultra"), None);
        assert_eq!(ReasoningEffort::None.as_str(), "none");
    }
}
