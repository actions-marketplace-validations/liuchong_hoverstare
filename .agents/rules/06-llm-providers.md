# 06 — LLM Provider 适配

1. **凭据优先级**：`OPENAI_API_KEY`（OpenAI 兼容路径，含 Kimi/DeepSeek/
   OpenRouter 等，配 `OPENAI_BASE_URL`）> `ANTHROPIC_API_KEY`（Anthropic 路径）。
2. **模型名**：`HOVERSTARE_MODEL` env > toml `model` > 默认值。
   OpenAI 兼容端点必须显式配模型名（默认值是 Anthropic 模型，别处不存在）。
3. **temperature 坑**：部分端点（如 kimi-for-coding）只接受默认温度，
   自定义温度直接 400 → 用 `set_temperature = false` 不传该字段；
   此时多 pass 的多样性由侧重 prompt 单独承担。
4. **rig 接自定义端点**：用 `openai::CompletionsClient::builder().base_url()`，
   注意是 Completions API client，不是 rig 默认的 Responses API client。
5. **max_tokens 始终显式设置**（预算控制，与端点是否必填无关）。
6. **空输出是真实存在的模型故障形态**（实测遇到过）：容错管线里空输出
   跳过 reformat pass，直接进全量重试。
7. **频控**：订阅制端点有速率限制，`passes` 默认 3 路并发撞上时降 1–2；
   429 由客户端指数退避重试兜底，最终 fail-open。
8. **思考模式（DeepSeek 等）**：`thinking` / `reasoning_effort` 两个字段只在
   OpenAI 兼容路径上、且只在配置了的时候才发送（`additional_params` 合进请求体）。
   DeepSeek 服务端的 effort 映射：`minimal/low -> low`、`medium/high/xhigh -> high`、
   `max -> max`；`none` = 关闭思考模式。思考模式下 `temperature` 不生效（配
   `set_temperature = false` 最干净），`top_p` 下限被抬到 0.95。
9. **思考模式 + tools 的 reasoning_content 回传**：DeepSeek 文档要求带 tools 的请求
   在后续轮次把历史 assistant 的 `reasoning_content` 回传（否则多轮上下文不连贯）；
   rig 0.36 已把它当非标准字段解析并回传（`AssistantContent::Reasoning`），
   不要自己再拼一遍，也不要把思维链当正文（最终返回值只取 content）。
10. **上下文窗口**：DeepSeek `deepseek-flash` 为 1M（输出上限 384K）。
    `context_tokens` 配了以后会反过来钳制 `max_diff_kb`（估计 4 字节/token、
    diff 最多占半窗），防止 diff 预算定得比窗口还大。
