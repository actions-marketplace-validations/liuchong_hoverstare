# 04 — 测试约定

1. 提交前四者全绿：`cargo test --workspace`、`cargo clippy --workspace
   --all-targets -- -D warnings`、`cargo fmt --check`、`cargo build --workspace`。
2. 纯逻辑（diff/指纹/投票/命令解析）：模块内单测 + `tests/fixtures/` fixture。
3. GitHub API：`tests/github_client.rs` httpmock 合约测试。
   注意 `mock_async(...).await` 才是 Mock；验证调用用 `assert_async()`。
   429/5xx 重试场景用 `with_retry_backoff(Duration::from_millis(1))` 加速。
4. agent 行为：实现 `AgentBackend` 的 FakeBackend 注入
   （参考 pipeline 测试），按 model/system_prompt 区分调用来源。
   agentic 循环与压缩（spec 13）用「脚本化 ChatClient」测：实现 `ChatClient`，
   按调用序号（或按收到的 prompt 内容）返回预设的文本/tool call/错误，
   这样压缩、溢出恢复、工具回灌都能在无网络下走真实代码路径。
5. 真实 LLM 冒烟：`cargo run --example local_review -- <diff> [base_ref]`
   （需要 LLM 凭据 env，只本地跑，不进 CI）。
6. 测试里禁止访问真实网络/真实凭据。
