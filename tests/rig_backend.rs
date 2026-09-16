//! RigBackend 合约测试：思考模式参数是否真的进入请求体（spec 01/04，httpmock）
//!
//! 这里只验证"请求长什么样"，不访问真实 LLM（测试规则 6）。

use std::time::Duration;

use hoverstare::agent::rig_backend::RigBackend;
use hoverstare::agent::{
    AgentBackend, Budget, ReasoningEffort, ReasoningOptions, ReviewRequest, ThinkingMode,
    ToolRegistry,
};
use hoverstare::config::LlmCredentials;
use httpmock::prelude::*;
use secrecy::SecretString;

fn test_request() -> ReviewRequest {
    ReviewRequest {
        system_prompt: "You are HoverStare.".to_string(),
        user_prompt: "review this diff".to_string(),
        tools: ToolRegistry::default(),
        budget: Budget {
            max_tool_calls: 0,
            timeout: Duration::from_secs(10),
        },
        model: "deepseek-flash".to_string(),
        temperature: None,
    }
}

fn backend(server: &MockServer, reasoning: ReasoningOptions) -> RigBackend {
    RigBackend::new(
        LlmCredentials::OpenAICompatible {
            key: SecretString::from("test-key".to_string()),
            base_url: server.base_url(),
        },
        reasoning,
    )
}

fn completion_body() -> String {
    r#"{
      "id": "chatcmpl-test",
      "object": "chat.completion",
      "created": 0,
      "model": "deepseek-flash",
      "choices": [{
        "index": 0,
        "message": {
          "role": "assistant",
          "reasoning_content": "thinking hard",
          "content": "{\"findings\":[]}"
        },
        "finish_reason": "stop"
      }],
      "usage": {"prompt_tokens": 3, "completion_tokens": 5, "total_tokens": 8}
    }"#
    .to_string()
}

/// 配了 thinking/reasoning_effort -> 请求体里必须带上这两个字段
#[tokio::test]
async fn thinking_params_reach_the_openai_request_body() {
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .json_body_includes(
                    r#"{"thinking": {"type": "enabled"}, "reasoning_effort": "medium"}"#,
                );
            then.status(200)
                .header("content-type", "application/json")
                .body(completion_body());
        })
        .await;

    let run = backend(
        &server,
        ReasoningOptions {
            thinking: Some(ThinkingMode::Enabled),
            effort: Some(ReasoningEffort::Medium),
        },
    )
    .review(test_request())
    .await
    .unwrap();

    assert_eq!(run.raw_output, "{\"findings\":[]}");
    assert_eq!(mock.calls_async().await, 1);
}

/// 未配置 -> 一个字段都不发（老端点不认这些字段，发了会 400）
#[tokio::test]
async fn unset_reasoning_sends_no_extra_fields() {
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .json_body_excludes(r#"{"thinking": {"type": "enabled"}}"#);
            then.status(200)
                .header("content-type", "application/json")
                .body(completion_body());
        })
        .await;

    let run = backend(&server, ReasoningOptions::default())
        .review(test_request())
        .await
        .unwrap();

    assert!(!run.raw_output.is_empty());
    assert_eq!(mock.calls_async().await, 1);
}

/// reasoning_effort = none -> 只发关闭思考模式，不发 effort
#[tokio::test]
async fn effort_none_sends_disabled_thinking_only() {
    let server = MockServer::start_async().await;
    let mock = server
        .mock_async(|when, then| {
            when.method(POST)
                .path("/chat/completions")
                .json_body_includes(r#"{"thinking": {"type": "disabled"}}"#)
                .json_body_excludes(r#"{"reasoning_effort": "none"}"#);
            then.status(200)
                .header("content-type", "application/json")
                .body(completion_body());
        })
        .await;

    let _ = backend(
        &server,
        ReasoningOptions {
            thinking: None,
            effort: Some(ReasoningEffort::None),
        },
    )
    .review(test_request())
    .await
    .unwrap();

    assert_eq!(mock.calls_async().await, 1);
}
