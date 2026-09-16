//! Rig-backed provider client (spec 04, spec 13)
//!
//! This file is the only module allowed to `use rig::*`; rig types must not leak
//! out. It no longer runs the multi-turn loop: since spec 13 the loop belongs to
//! `agent_loop`, because compaction has to replace conversation history and
//! history has to be ours for that. What is left here is one provider call —
//! system prompt, conversation, tool menu in; text, tool calls, usage out.
//!
//! Custom endpoints such as Kimi Code go through the OpenAI-compatible path
//! (`CompletionsClient` + `base_url`, verified in a spike, see
//! spikes/rig-kimi-probe).

use std::sync::Arc;

use async_trait::async_trait;
use rig::OneOrMany;
use rig::client::CompletionClient;
use rig::completion::message::{AssistantContent, Message};
use rig::completion::{CompletionError, CompletionModel, CompletionRequest, ToolDefinition};
use rig::providers::{anthropic, openai};
use secrecy::ExposeSecret;

use crate::agent::agent_loop::AgentLoop;
use crate::agent::compaction::{self, CompactionConfig};
use crate::agent::{
    AgentBackend, AgentError, ChatCall, ChatClient, ChatReply, ConversationItem, ReasoningOptions,
    ReviewRequest, ReviewRun, Usage,
};
use crate::config::LlmCredentials;

pub struct RigChatClient {
    creds: LlmCredentials,
    /// Thinking/reasoning tuning (spec 01/04); empty = send nothing extra
    reasoning: ReasoningOptions,
}

impl RigChatClient {
    pub fn new(creds: LlmCredentials, reasoning: ReasoningOptions) -> Self {
        Self { creds, reasoning }
    }

    /// Translate the framework-free call into a rig completion request.
    fn build_request(
        &self,
        call: &ChatCall,
        additional_params: Option<serde_json::Value>,
    ) -> CompletionRequest {
        let mut history: Vec<Message> = Vec::with_capacity(call.messages.len());
        for item in &call.messages {
            match item {
                ConversationItem::User { text } => history.push(Message::user(text.clone())),
                ConversationItem::Assistant { text, tool_calls } => {
                    let mut contents: Vec<AssistantContent> = Vec::new();
                    if !text.is_empty() {
                        contents.push(AssistantContent::text(text.clone()));
                    }
                    for tool_call in tool_calls {
                        contents.push(AssistantContent::tool_call(
                            tool_call.id.clone(),
                            tool_call.name.clone(),
                            tool_call.arguments.clone(),
                        ));
                    }
                    if contents.is_empty() {
                        contents.push(AssistantContent::text(String::new()));
                    }
                    history.push(Message::Assistant {
                        id: None,
                        content: OneOrMany::many(contents).unwrap_or_else(|_| {
                            OneOrMany::one(AssistantContent::text(String::new()))
                        }),
                    });
                }
                ConversationItem::ToolResult { call_id, text } => {
                    history.push(Message::tool_result(call_id.clone(), text.clone()))
                }
            }
        }
        let chat_history = OneOrMany::many(history)
            .unwrap_or_else(|_| OneOrMany::one(Message::user(String::new())));
        CompletionRequest {
            model: None,
            preamble: Some(call.system_prompt.clone()),
            chat_history,
            documents: Vec::new(),
            tools: call
                .tools
                .iter()
                .map(|spec| ToolDefinition {
                    name: spec.name.to_string(),
                    description: spec.description.to_string(),
                    parameters: spec.parameters.clone(),
                })
                .collect(),
            temperature: call.temperature,
            max_tokens: Some(call.max_tokens),
            tool_choice: None,
            additional_params,
            output_schema: None,
        }
    }
}

/// Map a provider error, keeping the over-window case recognizable (spec 13).
fn map_completion_error(error: CompletionError) -> AgentError {
    let text = error.to_string();
    if compaction::looks_like_context_overflow(&text) {
        AgentError::ContextOverflow(text)
    } else {
        AgentError::Backend(text)
    }
}

/// Convert a provider answer into the framework-free reply.
fn convert_reply<T>(response: rig::completion::CompletionResponse<T>) -> ChatReply {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    let mut had_reasoning = false;
    for content in response.choice.iter() {
        if matches!(content, AssistantContent::Reasoning(_)) {
            had_reasoning = true;
        }
        match content {
            AssistantContent::Text(value) => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&value.text);
            }
            AssistantContent::ToolCall(call) => tool_calls.push(crate::agent::ToolCall {
                id: call.id.clone(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            }),
            _ => {}
        }
    }
    ChatReply {
        text,
        tool_calls,
        had_reasoning,
        usage: Usage {
            input_tokens: response.usage.input_tokens,
            output_tokens: response.usage.output_tokens,
            cached_input_tokens: response.usage.cached_input_tokens,
        },
    }
}

#[async_trait]
impl ChatClient for RigChatClient {
    async fn complete(&self, call: ChatCall) -> Result<ChatReply, AgentError> {
        match &self.creds {
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
                let model = client.completion_model(call.model.clone());
                let request = self.build_request(&call, self.reasoning.openai_params());
                let response = model
                    .completion(request)
                    .await
                    .map_err(map_completion_error)?;
                Ok(convert_reply(response))
            }
            LlmCredentials::Anthropic { key, .. } => {
                let client = anthropic::Client::builder()
                    .api_key(key.expose_secret())
                    .build()
                    .map_err(|e| {
                        AgentError::Backend(format!("failed to build anthropic client: {e}"))
                    })?;
                // Anthropic's native thinking config has different semantics
                // (budget_tokens), so provider tuning is not forwarded here.
                if !self.reasoning.is_empty() {
                    tracing::debug!(
                        "thinking/reasoning_effort configured but not sent on the Anthropic path"
                    );
                }
                let model = client.completion_model(call.model.clone());
                let request = self.build_request(&call, None);
                let response = model
                    .completion(request)
                    .await
                    .map_err(map_completion_error)?;
                Ok(convert_reply(response))
            }
        }
    }
}

/// The `AgentBackend` the rest of the crate talks to: provider client + the
/// agentic loop that owns history and compaction.
pub struct RigBackend {
    inner: AgentLoop,
}

impl RigBackend {
    pub fn new(creds: LlmCredentials, reasoning: ReasoningOptions) -> RigBackend {
        Self::with_options(
            creds,
            reasoning,
            CompactionConfig::default(),
            compaction::DEFAULT_CONTEXT_TOKENS,
        )
    }

    pub fn with_options(
        creds: LlmCredentials,
        reasoning: ReasoningOptions,
        compaction: CompactionConfig,
        window: u64,
    ) -> RigBackend {
        let client: Arc<dyn ChatClient> = Arc::new(RigChatClient::new(creds, reasoning));
        RigBackend {
            inner: AgentLoop::new(client, compaction, window),
        }
    }

    /// Convenience constructor from the loaded config (spec 01/13).
    pub fn from_config(cfg: &crate::config::Config) -> RigBackend {
        RigBackend::with_options(
            cfg.llm.clone(),
            cfg.reasoning,
            cfg.compaction,
            cfg.context_tokens
                .unwrap_or(compaction::DEFAULT_CONTEXT_TOKENS),
        )
    }
}

#[async_trait]
impl AgentBackend for RigBackend {
    async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError> {
        self.inner.review(req).await
    }
}
