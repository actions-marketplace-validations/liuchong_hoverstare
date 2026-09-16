//! The agentic loop and its context compaction (spec 13)
//!
//! The loop owns the conversation. That ownership is the precondition for
//! compaction: a summary replaces history, so history cannot live inside a
//! provider library.
//!
//! Two mechanisms, one contract:
//!
//! - before every model call, if the estimated input is near the window, a
//!   model-written summary replaces the compactable prefix (threshold);
//! - if the provider refuses the request for exceeding its window, the prefix is
//!   replaced by a deterministic digest first, the dropped conversation is
//!   dumped, a read-only summarization run writes a precise summary from that
//!   dump, and the same request is sent once more (overflow recovery).

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{debug, info, warn};

use crate::agent::compaction::{self, CompactionConfig, Plan};
use crate::agent::tools::{self, ToolShared};
use crate::agent::{
    AgentBackend, AgentError, ChatCall, ChatClient, ChatReply, ConversationItem, ReviewRequest,
    ReviewRun, ToolCallRecord, ToolProfile, Usage,
};

/// Output limit per model call.
const MAX_OUTPUT_TOKENS: u64 = 8192;

/// Share of the window a summarization request may take (spec 13).
const SUMMARY_INPUT_RATIO: f64 = 0.5;

/// Digest bound when no model summary is available.
const DIGEST_MAX_CHARS: usize = 2_400;

/// The summary message that stands in for a dropped prefix.
const SUMMARY_PREFIX: &str = "[Earlier conversation summary]";

pub struct AgentLoop {
    client: Arc<dyn ChatClient>,
    compaction: CompactionConfig,
    /// model window in tokens
    window: u64,
}

impl AgentLoop {
    pub fn new(client: Arc<dyn ChatClient>, compaction: CompactionConfig, window: u64) -> Self {
        Self {
            client,
            compaction,
            window,
        }
    }

    pub fn window(&self) -> u64 {
        self.window
    }

    /// Estimated tokens a call carries: system prompt, conversation and the
    /// tool menu, which is sent on every request and cannot be compacted.
    fn call_tokens(
        &self,
        system_prompt: &str,
        items: &[ConversationItem],
        specs: &[tools::ToolSpec],
    ) -> u64 {
        let tool_tokens: u64 = specs
            .iter()
            .map(|spec| {
                compaction::estimate_tokens(spec.name)
                    + compaction::estimate_tokens(spec.description)
                    + compaction::estimate_tokens(&spec.parameters.to_string())
            })
            .sum();
        compaction::estimate_tokens(system_prompt)
            + compaction::conversation_tokens(items)
            + tool_tokens
    }

    async fn call(
        &self,
        model: &str,
        system_prompt: &str,
        items: &[ConversationItem],
        specs: &[tools::ToolSpec],
        temperature: Option<f64>,
    ) -> Result<ChatReply, AgentError> {
        self.client
            .complete(ChatCall {
                model: model.to_string(),
                system_prompt: system_prompt.to_string(),
                messages: items.to_vec(),
                tools: specs.to_vec(),
                temperature,
                max_tokens: MAX_OUTPUT_TOKENS,
            })
            .await
    }

    /// Ask the model to summarize `items` (spec 13, threshold form).
    async fn summarize(
        &self,
        model: &str,
        previous: Option<&str>,
        dropped: &[ConversationItem],
    ) -> Option<String> {
        let input_budget = ((self.window as f64) * SUMMARY_INPUT_RATIO).max(1.0) as u64;
        let (source, source_kind) =
            compaction::summarization_source(previous, dropped, input_budget, DIGEST_MAX_CHARS);
        if source_kind == "digest" {
            debug!("summarization input did not fit the window; summarizing from the digest");
        }
        let messages = vec![ConversationItem::User {
            text: compaction::summary_user_prompt(&source, previous, "threshold"),
        }];
        let reply = self
            .call(
                model,
                &compaction::summary_system_prompt(),
                &messages,
                &[],
                None,
            )
            .await
            .ok()?;
        compaction::validate_summary(&reply.text, self.compaction.summary_max_chars)
    }

    /// Read-only summarization run of the overflow recovery: the model is told
    /// where the dropped conversation was dumped and reads it with tools.
    async fn summarize_with_dump(
        &self,
        model: &str,
        crude: &str,
        dump_relative: &str,
        previous: Option<&str>,
        workspace: &std::path::Path,
        base_ref: &str,
    ) -> Option<String> {
        // A summary run gets its own fresh budget: spending the main run's
        // tool budget on recovery would starve the retry it exists for.
        let shared = ToolShared::new(
            workspace.to_path_buf(),
            base_ref,
            compaction::SUMMARY_MAX_STEPS,
        );
        let specs = tools::readonly_specs();
        let mut items = vec![ConversationItem::User {
            text: compaction::overflow_user_prompt(crude, dump_relative, previous),
        }];
        for _ in 0..compaction::SUMMARY_MAX_STEPS {
            let reply = self
                .call(
                    model,
                    &compaction::summary_system_prompt(),
                    &items,
                    &specs,
                    None,
                )
                .await
                .ok()?;
            if reply.tool_calls.is_empty() {
                return compaction::validate_summary(
                    &reply.text,
                    self.compaction.summary_max_chars,
                );
            }
            items.push(ConversationItem::Assistant {
                text: reply.text.clone(),
                tool_calls: reply.tool_calls.clone(),
            });
            for call in &reply.tool_calls {
                let output = shared
                    .run(call.name.clone(), format!("{:?}", call.arguments), async {
                        tools::dispatch(&call.name, &call.arguments, &shared).await
                    })
                    .await;
                items.push(ConversationItem::ToolResult {
                    call_id: call.id.clone(),
                    text: output,
                });
            }
        }
        None
    }

    /// Replace the planned prefix of `items` with `summary` (spec 13).
    fn apply_plan(items: &mut Vec<ConversationItem>, plan: Plan, summary: &str) {
        let tail = items.split_off(plan.cut + 1);
        *items = vec![ConversationItem::User {
            text: format!("{SUMMARY_PREFIX}\n{summary}"),
        }];
        items.extend(tail);
    }

    fn run_loop<'a>(
        &'a self,
        req: &'a ReviewRequest,
        shared: Option<Arc<ToolShared>>,
        profile: ToolProfile,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<ReviewRun, AgentError>> + Send + 'a>,
    > {
        Box::pin(async move {
            let mut items = vec![ConversationItem::User {
                text: req.user_prompt.clone(),
            }];
            let mut trace: Vec<ToolCallRecord> = Vec::new();
            let mut usage = Usage::default();
            let mut executed = 0u32;
            let mut recovered = false;
            let mut dump: Option<PathBuf> = None;
            let specs = if shared.is_some() {
                tools::specs(profile)
            } else {
                Vec::new()
            };

            let outcome = loop {
                // Threshold compaction: shrink before the provider refuses.
                if self.compaction.enabled {
                    let tokens = self.call_tokens(&req.system_prompt, &items, &specs);
                    if tokens >= self.compaction.threshold_tokens(self.window)
                        && let Some(plan) =
                            compaction::plan(&items, self.compaction.keep_tokens(self.window))
                    {
                        let previous = previous_summary(&items);
                        let dropped: Vec<ConversationItem> = items[plan.start..=plan.cut].to_vec();
                        let digest =
                            compaction::digest(previous.as_deref(), &dropped, DIGEST_MAX_CHARS);
                        let summary = match self
                            .summarize(&req.model, previous.as_deref(), &dropped)
                            .await
                        {
                            Some(summary) => {
                                info!(
                                    "context compaction (threshold): {} item(s) summarized by the model, {} token(s) estimated against a {} token window",
                                    plan.len(),
                                    tokens,
                                    self.window
                                );
                                summary
                            }
                            None => {
                                info!(
                                    "context compaction (threshold): {} item(s) replaced by the deterministic digest",
                                    plan.len()
                                );
                                digest
                            }
                        };
                        Self::apply_plan(&mut items, plan, &summary);
                    }
                }

                // The last calls must produce an answer, so no tools are
                // offered once the tool budget is spent.
                let menu = if executed >= req.budget.max_tool_calls {
                    Vec::new()
                } else {
                    specs.clone()
                };
                match self
                    .call(
                        &req.model,
                        &req.system_prompt,
                        &items,
                        &menu,
                        req.temperature,
                    )
                    .await
                {
                    Ok(reply) => {
                        usage.add(reply.usage);
                        if reply.tool_calls.is_empty() {
                            break Ok((reply.text, trace, usage));
                        }
                        items.push(ConversationItem::Assistant {
                            text: reply.text.clone(),
                            tool_calls: reply.tool_calls.clone(),
                        });
                        for call in &reply.tool_calls {
                            let Some(shared) = shared.clone() else {
                                items.push(ConversationItem::ToolResult {
                                    call_id: call.id.clone(),
                                    text: "no tools are available in this run".to_string(),
                                });
                                continue;
                            };
                            executed += 1;
                            let started = std::time::Instant::now();
                            let output = shared
                                .run(call.name.clone(), format!("{:?}", call.arguments), async {
                                    tools::dispatch(&call.name, &call.arguments, &shared).await
                                })
                                .await;
                            trace.push(ToolCallRecord {
                                name: call.name.clone(),
                                args_summary: format!("{:?}", call.arguments),
                                duration: started.elapsed(),
                                result_bytes: output.len(),
                            });
                            items.push(ConversationItem::ToolResult {
                                call_id: call.id.clone(),
                                text: output,
                            });
                        }
                    }
                    Err(AgentError::ContextOverflow(message))
                        if self.compaction.enabled && !recovered =>
                    {
                        recovered = true;
                        let Some(shared) = shared.clone() else {
                            break Err(AgentError::ContextOverflow(message));
                        };
                        let Some(plan) =
                            compaction::plan(&items, self.compaction.keep_tokens(self.window))
                        else {
                            break Err(AgentError::ContextOverflow(message));
                        };
                        let previous = previous_summary(&items);
                        let dropped: Vec<ConversationItem> = items[plan.start..=plan.cut].to_vec();
                        let digest =
                            compaction::digest(previous.as_deref(), &dropped, DIGEST_MAX_CHARS);
                        // Crude first: from here the retry cannot be refused for
                        // the same reason, whatever the precise stage does next.
                        Self::apply_plan(&mut items, plan, &digest);
                        let relative = format!(
                            ".hoverstare/context-{}.md",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_millis())
                                .unwrap_or(0)
                        );
                        let dump_abs = shared.workspace().join(&relative);
                        match compaction::dump(&dropped, &dump_abs) {
                            Ok(()) => {
                                info!(
                                    "context overflow: {} item(s) compacted, dump at {relative}, retrying once",
                                    dropped.len()
                                );
                                dump = Some(dump_abs);
                            }
                            Err(e) => warn!(
                                "context overflow: could not write the dump ({e}); the crude digest still stands"
                            ),
                        }
                        match self
                            .summarize_with_dump(
                                &req.model,
                                &digest,
                                &relative,
                                previous.as_deref(),
                                shared.workspace(),
                                shared.base_ref(),
                            )
                            .await
                        {
                            Some(precise) => {
                                if let Some(ConversationItem::User { text }) = items.first_mut() {
                                    *text = format!("{SUMMARY_PREFIX}\n{precise}");
                                }
                                info!("context overflow: precise summary written from the dump");
                            }
                            None => warn!(
                                "context overflow: precise summary unavailable, keeping the digest"
                            ),
                        }
                    }
                    Err(error) => break Err(error),
                }
            };

            if let Some(path) = dump {
                match std::fs::remove_file(&path) {
                    Ok(()) => debug!("removed compaction dump {}", path.display()),
                    Err(e) => warn!("could not remove compaction dump {}: {e}", path.display()),
                }
            }
            outcome.map(|(raw_output, tool_trace, usage)| ReviewRun {
                raw_output,
                tool_trace,
                usage,
            })
        })
    }
}

/// The summary already standing in the conversation, if any.
fn previous_summary(items: &[ConversationItem]) -> Option<String> {
    match items.first() {
        Some(ConversationItem::User { text }) if text.starts_with(SUMMARY_PREFIX) => {
            Some(text.clone())
        }
        _ => None,
    }
}

#[async_trait::async_trait]
impl AgentBackend for AgentLoop {
    async fn review(&self, req: ReviewRequest) -> Result<ReviewRun, AgentError> {
        let shared = req.tools.shared.clone();
        let profile = req.tools.profile;
        let fut = self.run_loop(&req, shared, profile);
        match tokio::time::timeout(req.budget.timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(AgentError::Timeout(req.budget.timeout)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{Budget, ToolCall, ToolRegistry};
    use std::sync::Mutex;

    type Script = Box<dyn Fn(&ChatCall, usize) -> Result<ChatReply, AgentError> + Send + Sync>;

    /// A client that answers from a script and records every call it receives.
    struct ScriptedClient {
        script: Vec<Script>,
        calls: Mutex<Vec<ChatCall>>,
    }

    impl ScriptedClient {
        fn new(script: Vec<Script>) -> Arc<Self> {
            Arc::new(Self {
                script,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<ChatCall> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl ChatClient for ScriptedClient {
        async fn complete(&self, call: ChatCall) -> Result<ChatReply, AgentError> {
            let index = {
                let mut calls = self.calls.lock().unwrap();
                let index = calls.len();
                calls.push(call.clone());
                index
            };
            match self.script.get(index) {
                Some(step) => step(&call, index),
                None => Ok(reply("done")),
            }
        }
    }

    fn reply(text: &str) -> ChatReply {
        ChatReply {
            text: text.to_string(),
            ..Default::default()
        }
    }

    fn tool_reply(id: &str, name: &str, arguments: serde_json::Value) -> ChatReply {
        ChatReply {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: id.to_string(),
                name: name.to_string(),
                arguments,
            }],
            usage: Usage::default(),
        }
    }

    fn request(shared: Option<Arc<ToolShared>>, calls: u32) -> ReviewRequest {
        ReviewRequest {
            system_prompt: "SYSTEM PROMPT".to_string(),
            user_prompt: "review this diff".to_string(),
            tools: ToolRegistry {
                shared,
                profile: ToolProfile::ReadOnly,
            },
            budget: Budget {
                max_tool_calls: calls,
                timeout: std::time::Duration::from_secs(30),
            },
            model: "test-model".to_string(),
            temperature: Some(0.0),
        }
    }

    fn loop_with(client: Arc<ScriptedClient>, window: u64) -> AgentLoop {
        AgentLoop::new(client, CompactionConfig::default(), window)
    }

    /// Extract the dump path the overflow prompt names.
    fn dump_path_in(prompt: &str) -> Option<String> {
        let start = prompt.find(".hoverstare/context-")?;
        let rest = &prompt[start..];
        let end = rest.find(".md")? + 3;
        Some(rest[..end].to_string())
    }

    #[tokio::test]
    async fn a_plain_answer_needs_one_call() {
        let client = ScriptedClient::new(vec![Box::new(|_call, _index| {
            Ok(reply("{\"findings\":[]}"))
        })]);
        let run = loop_with(client.clone(), 100_000)
            .review(request(None, 0))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "{\"findings\":[]}");
        assert_eq!(client.calls().len(), 1);
        assert_eq!(client.calls()[0].system_prompt, "SYSTEM PROMPT");
    }

    #[tokio::test]
    async fn tool_calls_are_executed_and_fed_back() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn main() {}\n").unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 5);
        let client = ScriptedClient::new(vec![
            Box::new(|_call, _| {
                Ok(tool_reply(
                    "1",
                    "read_file",
                    serde_json::json!({"path": "lib.rs"}),
                ))
            }),
            Box::new(|_call, _| Ok(reply("done"))),
        ]);
        let run = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 5))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "done");
        assert_eq!(run.tool_trace.len(), 1);
        assert_eq!(run.tool_trace[0].name, "read_file");
        let calls = client.calls();
        assert_eq!(calls.len(), 2);
        let fed_back = calls[1].messages.iter().any(|item| {
            matches!(item, ConversationItem::ToolResult { text, .. } if text.contains("fn main"))
        });
        assert!(fed_back, "tool output must be fed back to the model");
    }

    #[tokio::test]
    async fn below_the_threshold_nothing_is_compacted() {
        let dir = tempfile::tempdir().unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 5);
        let client = ScriptedClient::new(vec![Box::new(|_call, _| Ok(reply("done")))]);
        let mut req = request(Some(shared), 5);
        req.user_prompt = "x".repeat(4_000);
        loop_with(client.clone(), 1_000_000)
            .review(req)
            .await
            .unwrap();
        assert_eq!(client.calls().len(), 1, "no summarization request is sent");
    }

    #[tokio::test]
    async fn the_threshold_replaces_the_prefix_and_keeps_the_system_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 5);
        let client = ScriptedClient::new(vec![
            Box::new(|_call, _| {
                Ok(tool_reply(
                    "1",
                    "glob",
                    serde_json::json!({"pattern": "*.rs"}),
                ))
            }),
            Box::new(|_call, _| {
                Ok(reply(
                    "## Goal\nget the review done\n## Progress\ndiff read",
                ))
            }),
            Box::new(|_call, _| Ok(reply("final"))),
        ]);
        let mut req = request(Some(shared), 5);
        req.user_prompt = "x".repeat(4_000);
        let run = loop_with(client.clone(), 1_000).review(req).await.unwrap();
        assert_eq!(run.raw_output, "final");
        let calls = client.calls();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[1]
                .system_prompt
                .contains("Compress a coding conversation")
        );
        assert!(calls[1].messages.iter().any(|item| matches!(
            item,
            ConversationItem::User { text } if text.contains("<conversation>")
        )));
        assert_eq!(calls[2].system_prompt, "SYSTEM PROMPT");
        assert!(
            calls[2].messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text } if text.contains("get the review done")
            )),
            "the summary replaces the dropped prefix"
        );
    }

    #[tokio::test]
    async fn a_failed_summarizer_leaves_the_deterministic_digest() {
        let dir = tempfile::tempdir().unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 5);
        let client = ScriptedClient::new(vec![
            Box::new(|_call, _| {
                Ok(tool_reply(
                    "1",
                    "glob",
                    serde_json::json!({"pattern": "*.rs"}),
                ))
            }),
            Box::new(|_call, _| Err(AgentError::Backend("summarizer exploded".to_string()))),
            Box::new(|_call, _| Ok(reply("final"))),
        ]);
        let mut req = request(Some(shared), 5);
        req.user_prompt = "x".repeat(4_000);
        let run = loop_with(client.clone(), 1_000).review(req).await.unwrap();
        assert_eq!(run.raw_output, "final");
        let calls = client.calls();
        assert_eq!(calls.len(), 3);
        assert!(
            calls[2].messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text } if text.starts_with("[Earlier conversation summary]")
            )),
            "the digest stands in for the dropped turns"
        );
    }

    #[tokio::test]
    async fn an_overflow_is_recovered_with_a_dump_and_retried_once() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "fn main() {}\n").unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 8);
        let client = ScriptedClient::new(vec![
            Box::new(|_call, _| {
                Ok(tool_reply(
                    "1",
                    "read_file",
                    serde_json::json!({"path": "lib.rs"}),
                ))
            }),
            Box::new(|_call, _| {
                Err(AgentError::ContextOverflow(
                    "This model's maximum context length is 2000 tokens".to_string(),
                ))
            }),
            // The summarization run reads the dump the prompt names.
            Box::new(|call, _| {
                let prompt = call
                    .messages
                    .iter()
                    .find_map(|item| match item {
                        ConversationItem::User { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let path = dump_path_in(&prompt).expect("the prompt names the dump");
                Ok(tool_reply(
                    "2",
                    "read_file",
                    serde_json::json!({ "path": path }),
                ))
            }),
            Box::new(|_call, _| Ok(reply("## Goal\nprecise summary"))),
            Box::new(|_call, _| Ok(reply("recovered"))),
        ]);
        let mut req = request(Some(shared), 8);
        req.user_prompt = "x".repeat(4_000);
        let run = loop_with(client.clone(), 2_000).review(req).await.unwrap();
        assert_eq!(run.raw_output, "recovered");
        let calls = client.calls();

        // The recovery names the dump and hands the summarizer the crude digest.
        let summarizer = calls
            .iter()
            .position(|call| {
                call.messages.iter().any(|item| {
                    matches!(
                        item,
                        ConversationItem::User { text } if text.contains("<crude-digest>")
                    )
                })
            })
            .expect("a summarizer call carrying the digest");
        // Its next call carries what the read tool returned for the dump.
        let after_read = &calls[summarizer + 1];
        assert!(
            after_read.messages.iter().any(|item| matches!(
                item,
                ConversationItem::ToolResult { text, .. }
                    if text.contains("Dropped conversation context")
            )),
            "the summarizer read the dumped conversation through the real read_file tool"
        );
        assert!(
            !after_read.messages.iter().any(|item| matches!(
                item,
                ConversationItem::ToolResult { text, .. }
                    if text.contains("Access denied") || text.contains("file does not exist")
            )),
            "the dump is inside the tool sandbox"
        );
        // The last call is the retried request, and it carries the precise
        // summary rather than the digest.
        let retried = calls.last().expect("retried call");
        assert!(
            retried.messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text }
                    if text.contains("[Earlier conversation summary]")
                        && text.contains("precise summary")
            )),
            "the precise summary replaces the digest before the retry"
        );
        // The dump does not outlive the run.
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".hoverstare"))
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "the dump is removed after the run");
    }

    #[tokio::test]
    async fn a_second_overflow_is_not_retried_forever() {
        let dir = tempfile::tempdir().unwrap();
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 8);
        let client = ScriptedClient::new(vec![
            Box::new(|_call, _| {
                Ok(tool_reply(
                    "1",
                    "glob",
                    serde_json::json!({"pattern": "*.rs"}),
                ))
            }),
            Box::new(|_call, _| Err(AgentError::ContextOverflow("too long".to_string()))),
            Box::new(|_call, _| Ok(reply("## Goal\nsummary"))),
            Box::new(|_call, _| Err(AgentError::ContextOverflow("still too long".to_string()))),
        ]);
        let mut req = request(Some(shared), 8);
        req.user_prompt = "x".repeat(4_000);
        let result = loop_with(client.clone(), 4_000).review(req).await;
        assert!(matches!(result, Err(AgentError::ContextOverflow(_))));
        assert_eq!(client.calls().len(), 4, "one recovery, one retry, no loop");
    }
}
