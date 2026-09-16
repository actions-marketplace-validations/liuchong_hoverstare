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

use crate::agent::compaction::{self, CompactionConfig, Plan, WorkLedger};
use crate::agent::tools::{self, ToolShared};
use crate::agent::{
    AgentBackend, AgentError, ChatCall, ChatClient, ChatReply, ConversationItem, ReviewRequest,
    ReviewRun, ToolCallRecord, ToolProfile, Usage,
};

/// Floor and ceiling for the per-call output budget.
///
/// Reasoning is billed against `max_tokens` on thinking models, so a cap that
/// is fine for a plain answer can be exhausted by the reasoning alone: the
/// model then returns a finish reason of "length" with empty content, which
/// looks exactly like a model that refused to answer.
const MIN_OUTPUT_TOKENS: u64 = 4096;
const MAX_OUTPUT_TOKENS_CEILING: u64 = 65_536;

/// Output budget for one call: a share of the window, floored and capped.
pub fn output_budget(window: u64) -> u64 {
    (window / 16).clamp(MIN_OUTPUT_TOKENS, MAX_OUTPUT_TOKENS_CEILING)
}

/// How many empty answers in a row are tolerated before the run fails.
///
/// An empty reply is a real provider failure mode, not an exception: a
/// thinking model can answer only in its reasoning channel, and a transport can
/// hand back nothing at all. Each retry nudges the model to answer, and the
/// bound keeps a silent model from burning the run.
const EMPTY_REPLY_ATTEMPTS: u32 = 3;

/// Hard stop on model calls when a caller configured no round limit.
const DEFAULT_MAX_ROUNDS_MARGIN: u32 = 2;

/// Share of the window a summarization request may take (spec 13).
const SUMMARY_INPUT_RATIO: f64 = 0.5;

/// Digest bound when no model summary is available.
const DIGEST_MAX_CHARS: usize = 2_400;

/// A transient provider failure (rate limit, 5xx, dropped connection) is worth
/// retrying inside the loop; the pass-level retry above would otherwise throw
/// away every tool call already made.
const MODEL_MAX_ATTEMPTS: u32 = 3;
const MODEL_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(500);

/// Result markers that make a repeated identical call worth refusing.
const UNHELPFUL_MARKERS: &[&str] = &[
    "error",
    "does not exist",
    "denied",
    "invalid",
    "budget exhausted",
    "failed",
    "unknown tool",
];

/// The summary message that stands in for a dropped prefix.
const SUMMARY_PREFIX: &str = "[Earlier conversation summary]";

pub struct AgentLoop {
    client: Arc<dyn ChatClient>,
    compaction: CompactionConfig,
    /// model window in tokens
    window: u64,
    /// Absolute bound on model calls in one run; 0 derives it from the tool
    /// budget. A long-running review service raises it without raising the
    /// tool budget, so a run can take more turns without doing more work.
    max_rounds: u32,
    /// Per-call output budget, including reasoning tokens (spec 01).
    output_tokens: u64,
}

impl AgentLoop {
    pub fn new(client: Arc<dyn ChatClient>, compaction: CompactionConfig, window: u64) -> Self {
        Self {
            client,
            compaction,
            output_tokens: output_budget(window),
            window,
            max_rounds: 0,
        }
    }

    /// Override the per-call output budget (0 keeps the window-derived one).
    pub fn with_output_tokens(mut self, output_tokens: u64) -> Self {
        if output_tokens > 0 {
            self.output_tokens = output_tokens;
        }
        self
    }

    /// Bound the number of model calls in one run (0 = derive from the tool budget).
    pub fn with_max_rounds(mut self, max_rounds: u32) -> Self {
        self.max_rounds = max_rounds;
        self
    }

    /// The effective round bound for a run with `tool_budget` tool calls.
    fn round_budget(&self, tool_budget: u32) -> u32 {
        if self.max_rounds == 0 {
            tool_budget.saturating_add(DEFAULT_MAX_ROUNDS_MARGIN).max(2)
        } else {
            self.max_rounds.max(1)
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
                max_tokens: self.output_tokens,
            })
            .await
    }

    /// One main-loop call with bounded retries for transient provider failures.
    ///
    /// An over-window refusal is never retried here: it is recovered instead.
    /// The pass-level retry above this loop would rerun the whole analysis and
    /// throw away every tool call already made, so a rate limit or a dropped
    /// connection is worth absorbing in place.
    async fn call_retrying(
        &self,
        model: &str,
        system_prompt: &str,
        items: &[ConversationItem],
        specs: &[tools::ToolSpec],
        temperature: Option<f64>,
    ) -> Result<ChatReply, AgentError> {
        let mut attempt = 0u32;
        loop {
            match self
                .call(model, system_prompt, items, specs, temperature)
                .await
            {
                Ok(reply) => return Ok(reply),
                Err(AgentError::Backend(message))
                    if attempt + 1 < MODEL_MAX_ATTEMPTS && is_transient_backend_error(&message) =>
                {
                    let delay = MODEL_RETRY_BASE * 4u32.pow(attempt);
                    warn!(
                        "transient model error, retrying in {delay:?} ({}/{MODEL_MAX_ATTEMPTS}): {message}",
                        attempt + 1
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(error) => return Err(error),
            }
        }
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
    ///
    /// Everything before `plan.start` stays: that is the pinned run task, and it
    /// survives every compaction by construction.
    fn apply_plan(items: &mut Vec<ConversationItem>, plan: Plan, summary: &str) {
        let tail = items.split_off(plan.cut + 1);
        items.truncate(plan.start);
        items.push(ConversationItem::User {
            text: format!("{SUMMARY_PREFIX}\n{summary}"),
        });
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
            let mut just_recovered = false;
            let mut dump: Option<PathBuf> = None;
            // Deterministic record of the work this run has done. It is what
            // keeps a compacted conversation able to continue: prose may forget
            // a path, the ledger cannot.
            let mut ledger = WorkLedger::from_items(&items);
            let mut rounds = 0u32;
            let mut empty_replies = 0u32;
            let max_rounds = self.round_budget(req.budget.max_tool_calls);
            // Signature -> result of the last execution. A model that repeats a
            // call whose result cannot change would otherwise spend the budget
            // re-reading the same nothing.
            let mut seen_calls: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let specs = if shared.is_some() {
                tools::specs(profile)
            } else {
                Vec::new()
            };

            let outcome = loop {
                // Threshold compaction: shrink before the provider refuses.
                // Not immediately after a recovery: that pass just wrote a
                // summary, and summarizing a summary costs a request and buys
                // nothing back.
                if self.compaction.enabled && !just_recovered {
                    let tokens = self.call_tokens(&req.system_prompt, &items, &specs);
                    if tokens >= self.compaction.threshold_tokens(self.window)
                        && let Some(plan) = compaction::plan(
                            &items,
                            self.compaction.keep_tokens(self.window),
                            pinned_prefix(&items),
                        )
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
                        let summary = compaction::with_ledger(&summary, &ledger);
                        Self::apply_plan(&mut items, plan, &summary);
                    }
                }

                // The last calls must produce an answer, so no tools are
                // offered once the tool budget is spent.
                rounds += 1;
                let menu = if executed >= req.budget.max_tool_calls || rounds >= max_rounds {
                    Vec::new()
                } else {
                    specs.clone()
                };
                match self
                    .call_retrying(
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
                            if reply.text.trim().is_empty() {
                                empty_replies += 1;
                                if empty_replies >= EMPTY_REPLY_ATTEMPTS {
                                    break Err(AgentError::Backend(format!(
                                        "model returned an empty reply {empty_replies} times{}",
                                        if reply.had_reasoning {
                                            " (it produced reasoning but no answer)"
                                        } else {
                                            ""
                                        }
                                    )));
                                }
                                warn!(
                                    "empty model reply ({empty_replies}/{EMPTY_REPLY_ATTEMPTS}, reasoning={}); asking again",
                                    reply.had_reasoning
                                );
                                items.push(ConversationItem::User {
                                    text: "[note] your previous reply was empty. Answer now with \
                                           the required output and nothing else."
                                        .to_string(),
                                });
                                continue;
                            }
                            break Ok((reply.text, trace, usage));
                        }
                        if menu.is_empty() {
                            // The budget is spent, so no tool was offered. A
                            // model that still asks for one is told to answer
                            // instead of being handed an unadvertised call.
                            items.push(ConversationItem::Assistant {
                                text: reply.text.clone(),
                                tool_calls: reply.tool_calls.clone(),
                            });
                            for call in &reply.tool_calls {
                                items.push(ConversationItem::ToolResult {
                                    call_id: call.id.clone(),
                                    text: "no tools are available: the tool or round budget is \
                                           spent. Answer with the information you already have."
                                        .to_string(),
                                });
                            }
                            if rounds >= max_rounds {
                                break Err(AgentError::Backend(format!(
                                    "the run exceeded its {max_rounds} round budget without an answer"
                                )));
                            }
                            continue;
                        }
                        items.push(ConversationItem::Assistant {
                            text: reply.text.clone(),
                            tool_calls: reply.tool_calls.clone(),
                        });
                        for call in &reply.tool_calls {
                            ledger.observe(call);
                            let Some(shared) = shared.clone() else {
                                items.push(ConversationItem::ToolResult {
                                    call_id: call.id.clone(),
                                    text: "no tools are available in this run".to_string(),
                                });
                                continue;
                            };
                            executed += 1;
                            let started = std::time::Instant::now();
                            let signature = format!("{}:{}", call.name, call.arguments);
                            let output = match seen_calls.get(&signature) {
                                // The model repeating a call whose result cannot
                                // change would spend the budget re-reading the
                                // same nothing.
                                Some(previous) if is_unhelpful(previous) => {
                                    "unchanged repeat: this exact call already returned a result \
                                     that will not change. Use it, or try a different path or \
                                     pattern."
                                        .to_string()
                                }
                                _ => {
                                    let output = shared
                                        .run(
                                            call.name.clone(),
                                            format!("{:?}", call.arguments),
                                            async {
                                                tools::dispatch(
                                                    &call.name,
                                                    &call.arguments,
                                                    &shared,
                                                )
                                                .await
                                            },
                                        )
                                        .await;
                                    seen_calls.insert(signature, output.clone());
                                    output
                                }
                            };
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
                        let Some(plan) = compaction::plan(
                            &items,
                            self.compaction.keep_tokens(self.window),
                            pinned_prefix(&items),
                        ) else {
                            break Err(AgentError::ContextOverflow(message));
                        };
                        let previous = previous_summary(&items);
                        let dropped: Vec<ConversationItem> = items[plan.start..=plan.cut].to_vec();
                        let digest =
                            compaction::digest(previous.as_deref(), &dropped, DIGEST_MAX_CHARS);
                        // Crude first: from here the retry cannot be refused for
                        // the same reason, whatever the precise stage does next.
                        let digest = compaction::with_ledger(&digest, &ledger);
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
                        just_recovered = true;
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
                                // The summary the crude stage left sits at the
                                // plan's start; the pinned task before it stays.
                                if let Some(ConversationItem::User { text }) =
                                    items.get_mut(plan.start)
                                {
                                    let precise = compaction::with_ledger(&precise, &ledger);
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

/// Whether a provider failure is the kind that succeeds on a second try.
fn is_transient_backend_error(message: &str) -> bool {
    let text = message.to_ascii_lowercase();
    [
        "429",
        "rate limit",
        "timeout",
        "timed out",
        "connection",
        "500",
        "502",
        "503",
        "504",
        "overloaded",
        "temporarily",
    ]
    .iter()
    .any(|needle| text.contains(needle))
}

/// Whether a tool result is the kind a retry of the same call cannot improve.
fn is_unhelpful(result: &str) -> bool {
    let text = result.to_ascii_lowercase();
    UNHELPFUL_MARKERS.iter().any(|marker| text.contains(marker))
}

/// Items at the front that no compaction may replace: the run's own task.
///
/// After a first compaction the front is the summary itself, and a summary of a
/// summary (with the previous one passed verbatim) is the designed behaviour.
fn pinned_prefix(items: &[ConversationItem]) -> usize {
    if previous_summary(items).is_some() {
        0
    } else {
        1
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
    use std::sync::atomic::{AtomicUsize, Ordering};

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
            // A script shorter than the run repeats its last entry; most tests
            // script the interesting calls and let the tail answer.
            match self.script.get(index).or_else(|| self.script.last()) {
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
            had_reasoning: false,
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

    #[test]
    fn the_output_budget_scales_with_the_window_for_thinking_models() {
        // Reasoning shares max_tokens, so a small fixed cap starves the answer.
        // A 1M window yields ~62.5K, in the same league as the provider default.
        assert_eq!(output_budget(1_000_000), 62_500);
        assert_eq!(output_budget(131_072), 8_192);
        assert_eq!(output_budget(1_000), MIN_OUTPUT_TOKENS);
    }

    #[tokio::test]
    async fn every_call_carries_the_configured_output_budget() {
        let client = ScriptedClient::new(vec![Box::new(|_call, _| Ok(reply("done")))]);
        let loop_backend = AgentLoop::new(client.clone(), CompactionConfig::default(), 1_000_000)
            .with_output_tokens(12_345);
        loop_backend.review(request(None, 0)).await.unwrap();
        assert_eq!(client.calls()[0].max_tokens, 12_345);
        // 0 keeps the window-derived default.
        let client = ScriptedClient::new(vec![Box::new(|_call, _| Ok(reply("done")))]);
        AgentLoop::new(client.clone(), CompactionConfig::default(), 1_000_000)
            .with_output_tokens(0)
            .review(request(None, 0))
            .await
            .unwrap();
        assert_eq!(client.calls()[0].max_tokens, 62_500);
    }

    fn loop_with(client: Arc<ScriptedClient>, window: u64) -> AgentLoop {
        AgentLoop::new(client, CompactionConfig::default(), window)
    }

    /// Whether a call is one of the loop's summarization requests.
    fn is_summarizer(call: &ChatCall) -> bool {
        call.system_prompt
            .contains("Compress a coding conversation")
    }

    fn user_text(call: &ChatCall) -> String {
        call.messages
            .iter()
            .find_map(|item| match item {
                ConversationItem::User { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    /// Extract the dump path an overflow prompt names.
    fn dump_path_in(prompt: &str) -> Option<String> {
        let start = prompt.find(".hoverstare/context-")?;
        let rest = &prompt[start..];
        let end = rest.find(".md")? + 3;
        Some(rest[..end].to_string())
    }

    /// A workspace with two sizeable files: enough conversation to compact.
    fn two_file_workspace() -> (tempfile::TempDir, Arc<ToolShared>) {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.rs", "b.rs"] {
            std::fs::write(
                dir.path().join(name),
                format!("// {name}\n{}", "content ".repeat(800)),
            )
            .unwrap();
        }
        let shared = ToolShared::new(dir.path().to_path_buf(), "HEAD", 8);
        (dir, shared)
    }

    /// One script entry for the whole run: it dispatches by prompt content, so
    /// a summarization call interleaved anywhere cannot shift the answers of
    /// the conversation under test. `answer` sees the index among main calls.
    fn scripted(
        answer: impl Fn(usize, &ChatCall) -> Result<ChatReply, AgentError> + Send + Sync + 'static,
    ) -> Script {
        let counter = Arc::new(AtomicUsize::new(0));
        Box::new(move |call, _index| {
            if is_summarizer(call) {
                return Ok(reply("## Goal\nsummary"));
            }
            let index = counter.fetch_add(1, Ordering::SeqCst);
            answer(index, call)
        })
    }

    #[tokio::test]
    async fn a_plain_answer_needs_one_call() {
        let client = ScriptedClient::new(vec![Box::new(|_call, _| Ok(reply("{\"findings\":[]}")))]);
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
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            _ => Ok(reply("done")),
        })]);
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
            matches!(item, ConversationItem::ToolResult { text, .. } if text.contains("content content"))
        });
        assert!(fed_back, "tool output must be fed back to the model");
    }

    #[tokio::test]
    async fn below_the_threshold_nothing_is_compacted() {
        let (_dir, shared) = two_file_workspace();
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
    async fn the_threshold_summarizes_the_middle_and_pins_the_task() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            1 => Ok(tool_reply(
                "2",
                "read_file",
                serde_json::json!({"path": "b.rs"}),
            )),
            _ => Ok(reply("final")),
        })]);
        let run = loop_with(client.clone(), 400)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        let calls = client.calls();
        let summarizer = calls
            .iter()
            .position(is_summarizer)
            .expect("a summarization request");
        assert!(calls[summarizer].messages.iter().any(|item| matches!(
            item,
            ConversationItem::User { text } if text.contains("<conversation>")
        )));
        let after = calls
            .iter()
            .skip(summarizer + 1)
            .find(|call| !is_summarizer(call))
            .expect("the compacted request");
        assert_eq!(
            after.system_prompt, "SYSTEM PROMPT",
            "the system prompt is untouched"
        );
        let texts: Vec<String> = after
            .messages
            .iter()
            .filter_map(|item| match item {
                ConversationItem::User { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert!(
            texts
                .iter()
                .any(|text| text.starts_with("[Earlier conversation summary]")),
            "the dropped turns became a summary"
        );
        assert!(
            texts.iter().any(|text| text == "review this diff"),
            "the run's own task prompt is pinned and never summarized away"
        );
    }

    #[tokio::test]
    async fn a_failed_summarizer_leaves_the_deterministic_digest() {
        let (_dir, shared) = two_file_workspace();
        let counter = Arc::new(AtomicUsize::new(0));
        let client = ScriptedClient::new(vec![Box::new(move |call: &ChatCall, _index| {
            if is_summarizer(call) {
                return Err(AgentError::Backend("summarizer exploded".to_string()));
            }
            match counter.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(tool_reply(
                    "1",
                    "read_file",
                    serde_json::json!({"path": "a.rs"}),
                )),
                1 => Ok(tool_reply(
                    "2",
                    "read_file",
                    serde_json::json!({"path": "b.rs"}),
                )),
                _ => Ok(reply("final")),
            }
        })]);
        let run = loop_with(client.clone(), 400)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        let after = client
            .calls()
            .iter()
            .skip_while(|call| !is_summarizer(call))
            .find(|call| !is_summarizer(call))
            .cloned()
            .expect("the compacted request");
        assert!(
            after.messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text } if text.starts_with("[Earlier conversation summary]")
            )),
            "the digest stands in for the dropped turns"
        );
    }

    #[tokio::test]
    async fn an_overflow_is_recovered_with_a_dump_and_retried_once() {
        let (dir, shared) = two_file_workspace();
        let counter = Arc::new(AtomicUsize::new(0));
        let dump_reads = Arc::new(AtomicUsize::new(0));
        let client = ScriptedClient::new(vec![Box::new(move |call: &ChatCall, _index| {
            if is_summarizer(call) {
                let prompt = user_text(call);
                if prompt.contains("<crude-digest>") {
                    if dump_reads.fetch_add(1, Ordering::SeqCst) == 0 {
                        let path = dump_path_in(&prompt).expect("the prompt names the dump");
                        return Ok(tool_reply(
                            "99",
                            "read_file",
                            serde_json::json!({ "path": path }),
                        ));
                    }
                    return Ok(reply("## Goal\nprecise summary"));
                }
                return Ok(reply("## Goal\nthreshold summary"));
            }
            match counter.fetch_add(1, Ordering::SeqCst) {
                0 => Ok(tool_reply(
                    "1",
                    "read_file",
                    serde_json::json!({"path": "a.rs"}),
                )),
                1 => Ok(tool_reply(
                    "2",
                    "read_file",
                    serde_json::json!({"path": "b.rs"}),
                )),
                2 => Err(AgentError::ContextOverflow(
                    "This model's maximum context length is 2000 tokens".to_string(),
                )),
                _ => Ok(reply("recovered")),
            }
        })]);
        let run = loop_with(client.clone(), 2_000)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "recovered");
        let calls = client.calls();

        let summarizer = calls
            .iter()
            .position(|call| user_text(call).contains("<crude-digest>"))
            .expect("the overflow summarizer call");
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
        let retried = calls.last().expect("retried call");
        assert!(
            retried.messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text }
                    if text.starts_with("[Earlier conversation summary]")
                        && text.contains("precise summary")
            )),
            "the precise summary replaces the digest before the retry"
        );
        let leftovers: Vec<_> = std::fs::read_dir(dir.path().join(".hoverstare"))
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(leftovers.is_empty(), "the dump is removed after the run");
    }

    #[tokio::test]
    async fn a_compaction_carries_the_deterministic_work_ledger() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            1 => Ok(tool_reply(
                "2",
                "read_file",
                serde_json::json!({"path": "b.rs"}),
            )),
            _ => Ok(reply("final")),
        })]);
        let run = loop_with(client.clone(), 400)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        let after = client
            .calls()
            .iter()
            .skip_while(|call| !is_summarizer(call))
            .find(|call| !is_summarizer(call))
            .cloned()
            .expect("the compacted request");
        let summary = after
            .messages
            .iter()
            .find_map(|item| match item {
                ConversationItem::User { text }
                    if text.starts_with("[Earlier conversation summary]") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .expect("the summary message");
        // Whatever the model's prose said, the paths it actually read are there.
        assert!(summary.contains("[deterministic work ledger]"));
        assert!(summary.contains("files_read: a.rs"));
        assert!(summary.contains("tool_calls:"));
    }

    #[tokio::test]
    async fn rounds_are_bounded_independently_of_the_tool_budget() {
        let (_dir, shared) = two_file_workspace();
        // The model calls tools forever; the round bound withdraws them and the
        // run still answers.
        let client = ScriptedClient::new(vec![scripted(|_index, _call| {
            Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            ))
        })]);
        let result = loop_with(client.clone(), 100_000)
            .with_max_rounds(3)
            .review(request(Some(shared), 50))
            .await;
        // Every model answer is a tool call, so the run ends on the round bound
        // instead of hanging: bounded, and it says why.
        assert!(
            matches!(result, Err(AgentError::Backend(message)) if message.contains("round budget"))
        );
    }

    #[tokio::test]
    async fn an_empty_reply_is_asked_again_before_the_run_fails() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|_index, _call| Ok(reply("")))]);
        let result = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 8))
            .await;
        assert!(
            matches!(result, Err(AgentError::Backend(message)) if message.contains("empty reply"))
        );
        // Bounded: the initial call plus the tolerated retries.
        assert_eq!(client.calls().len() as u32, EMPTY_REPLY_ATTEMPTS);
    }

    #[tokio::test]
    async fn an_empty_reply_then_an_answer_still_completes_the_run() {
        let (_dir, shared) = two_file_workspace();
        let counter = Arc::new(AtomicUsize::new(0));
        let client = ScriptedClient::new(vec![scripted(move |_index, _call| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                return Ok(reply("   "));
            }
            Ok(reply("final"))
        })]);
        let run = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        // The nudge reached the model on the retry.
        let second = &client.calls()[1];
        assert!(
            second.messages.iter().any(|item| matches!(
                item,
                ConversationItem::User { text } if text.contains("previous reply was empty")
            )),
            "the retry asks the model to answer"
        );
    }

    #[tokio::test]
    async fn a_transient_provider_failure_is_retried_inside_the_loop() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            1 => Err(AgentError::Backend("429 rate limit exceeded".to_string())),
            _ => Ok(reply("final")),
        })]);
        let run = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        assert_eq!(run.tool_trace.len(), 1, "the retry kept the tool work");
    }

    #[tokio::test]
    async fn a_non_transient_failure_is_returned_at_once() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![Box::new(|_call, _| {
            Err(AgentError::Backend("invalid api key".to_string()))
        })]);
        let result = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 8))
            .await;
        assert!(matches!(result, Err(AgentError::Backend(_))));
        assert_eq!(client.calls().len(), 1);
    }

    #[tokio::test]
    async fn repeating_an_unhelpful_call_is_refused_not_re_executed() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 | 1 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "does-not-exist.rs"}),
            )),
            _ => Ok(reply("final")),
        })]);
        let run = loop_with(client.clone(), 100_000)
            .review(request(Some(shared), 8))
            .await
            .unwrap();
        assert_eq!(run.raw_output, "final");
        let refusal = client
            .calls()
            .iter()
            .flat_map(|call| call.messages.iter())
            .filter_map(|item| match item {
                ConversationItem::ToolResult { text, .. } => Some(text.clone()),
                _ => None,
            })
            .find(|text| text.contains("unchanged repeat"));
        assert!(refusal.is_some(), "the repeated failing call is refused");
        assert_eq!(
            run.tool_trace.len(),
            2,
            "both calls count against the budget"
        );
    }

    #[tokio::test]
    async fn a_second_overflow_is_not_retried_forever() {
        let (_dir, shared) = two_file_workspace();
        let client = ScriptedClient::new(vec![scripted(|index, _call| match index {
            0 => Ok(tool_reply(
                "1",
                "read_file",
                serde_json::json!({"path": "a.rs"}),
            )),
            1 => Ok(tool_reply(
                "2",
                "read_file",
                serde_json::json!({"path": "b.rs"}),
            )),
            _ => Err(AgentError::ContextOverflow("still too long".to_string())),
        })]);
        let result = loop_with(client.clone(), 2_000)
            .review(request(Some(shared), 8))
            .await;
        assert!(matches!(result, Err(AgentError::ContextOverflow(_))));
        let main_calls = client
            .calls()
            .iter()
            .filter(|call| !is_summarizer(call))
            .count();
        assert_eq!(main_calls, 4, "two turns, one recovery, one retry, no loop");
    }
}
