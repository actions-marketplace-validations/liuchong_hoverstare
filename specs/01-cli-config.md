# 01 — CLI 与配置

## 目标

定义二进制的命令行界面、配置来源与优先级、退出码契约。

## CLI

```
hoverstare <COMMAND>

Commands:
  review    审查一个 PR（GitHub Actions 中的主入口）
  mention   处理一条 @hoverstare 评论（issue_comment 事件入口，M6）
  develop   处理 issue / PR 开发模式事件（M11-M13）
  serve     启动自托管 webhook 服务（M8）
  version   打印版本
  help      打印帮助信息
```

### `hoverstare review`

从 GitHub Actions 环境读取上下文，正常情况下不需要任何参数。

| flag / env | 说明 |
|---|---|
| `--pr <N>` | 覆盖事件中的 PR 编号（调试用） |
| `--dry-run` | 完整执行分析，但最后不发布，把 review JSON 打到 stdout |
| `--verbose` / `-v` | debug 日志 |
| env `GITHUB_EVENT_PATH` | 事件 payload JSON 路径（Actions 注入） |
| env `GITHUB_REPOSITORY` | `owner/repo` |
| env `GITHUB_TOKEN` | GitHub API token（Actions 注入） |
| env `GH_PAT` | 可选 classic PAT（`repo` scope）。存在时优先于 GITHUB_TOKEN——`resolveReviewThread` 对默认 token 有平台限制（spec 07） |
| env `ANTHROPIC_API_KEY` 或 `OPENAI_API_KEY`(+`OPENAI_BASE_URL`) | LLM 凭据 |
| env `GITHUB_WORKSPACE` | checkout 后的仓库根目录（工具沙箱根） |
| env `HOVERSTARE_MODEL` / `HOVERSTARE_REFORMAT_MODEL` | 覆盖 toml 中的模型名（调试/临时切换用） |
| env `HOVERSTARE_LANGUAGE` | 覆盖 toml `language`（输出语言，en/zh-CN/ru/fr/de/es） |
| env `HOVERSTARE_THINKING` / `HOVERSTARE_REASONING_EFFORT` | 覆盖 toml 的思考模式配置 |
| env `HOVERSTARE_CONTEXT_TOKENS` | 覆盖 toml `context_tokens`（模型上下文窗口） |
| env `HOVERSTARE_COMPACTION` | 覆盖 toml `compaction`（`true`/`false`） |
| env `HOVERSTARE_COMPACTION_THRESHOLD_RATIO` / `HOVERSTARE_COMPACTION_KEEP_RATIO` | 覆盖压缩阈值与保留比例 |
| env `HOVERSTARE_SUMMARY_MAX_CHARS` | 覆盖模型摘要长度上限 |
| env `HOVERSTARE_MAX_ROUNDS` | 覆盖单次 run 的模型调用轮数上限（0 = 由工具预算推导） |
| env `HOVERSTARE_MAX_OUTPUT_TOKENS` | 覆盖单次调用的输出上限（0 = 由窗口推导） |
| env `HOVERSTARE_COMMIT_IDENTITY` | 覆盖 toml `commit_identity`（author/bot/coauthor） |
| env `HOVERSTARE_COMMIT_AUTHOR` | 覆盖 toml `commit_author`（`Name <email>`） |

非 Actions 环境本地调试时，`--pr` + `GITHUB_REPOSITORY` + 两个 token 即可运行。

## 配置文件

仓库内 `.github/hoverstare.toml`，所有字段可选，缺省用默认值：

```toml
# 主审模型。Anthropic 模型名或 OpenAI-compatible 模型名
model = "claude-sonnet-4-5"
# 输出修复（reformat pass）用的廉价快速模型
reformat_model = "claude-haiku-4-5"

# 并行审查路数；1 = 关闭投票（M5）
passes = 3
# 单票 finding 是否过 verifier 复核（M5）
verify = true
# 低于该级别的 finding 只进摘要 Nitpicks，不发行内评论: "low"|"medium"|"high"|"critical"
severity_threshold = "medium"

# 不参与审查的路径（glob）
ignore = ["*.lock", "**/dist/**", "**/*.min.js", "**/generated/**"]

# diff 总大小预算（KB），超出按优先级截断
max_diff_kb = 400
# agentic 循环预算
max_tool_calls = 20
# 单次运行 wall-clock 上限（秒）
timeout_secs = 900

# draft PR 是否审查
review_drafts = false
# 分析失败时是否让 CI 失败（默认 false = fail-open）
fail_closed = false
# 是否写 status checks（M4）
status_checks = false
# 是否给请求设置 temperature。部分端点（如 kimi-for-coding）只接受默认值，
# 置 false 则不传该字段（多 pass 的多样性改由侧重 prompt 承担）
set_temperature = true

# 思考模式（DeepSeek 等支持 reasoning 的 OpenAI 兼容端点）。
# 两个字段都留空 = 一个都不发（老端点如 kimi-for-coding 不认这两个字段，会 400）。
# thinking = "enabled" | "disabled"
# reasoning_effort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
# effort = "none" 或 thinking = "disabled" 等价于关闭思考模式（只发 disabled，不发 effort）。
# DeepSeek 服务端的 effort 映射：minimal/low -> low，medium/high/xhigh -> high，max -> max；
# 思考模式下 temperature 不生效（配 set_temperature = false 即可不发送该字段）。
# 仅 OpenAI 兼容路径生效；Anthropic 原生路径语义不同（thinking 需要 budget_tokens），暂不发送。
thinking = "enabled"
reasoning_effort = "medium"

# 模型上下文窗口（token）。设置后用于推导 diff 预算上限：diff 文本按 4 字节/token
# 估算、最多占窗口的一半，超过时把 max_diff_kb 收窄到该上限并 warn。
# 同时它也是上下文压缩（spec 13）的窗口；未设置时压缩用 131072。
# DeepSeek deepseek-flash 为 1M。留空 = 不做推导（行为同旧版本）。
context_tokens = 1000000

# 上下文压缩（spec 13）。compaction = false 则完全不压缩（溢出即失败）。
compaction = true
# 估算输入达到窗口的这个比例就开始阈值压缩；压缩后保留最近这段比例的窗口逐字不变。
compaction_threshold_ratio = 0.75
compaction_keep_ratio = 0.25
# 模型写的摘要长度上限（确定性摘要另受 durable 上限约束）。
summary_max_chars = 4000
# 单次 run 的模型调用轮数上限；0 = 由 max_tool_calls 推导（+2）。
# 与 max_tool_calls（工具调用预算）独立，长运行时服务可以单独提高轮数。
max_rounds = 0

# 单次模型调用的输出上限（token）。0 = 由窗口推导：window/16，下限 4096、上限 65536。
# 注意：思考模型的推理 token 也计入这个上限，写死一个偏小的值会让推理吃光额度、
# 正文返回空（表现为"模型只产出推理没有答案"）。
max_output_tokens = 0

# 输出语言：PR review 正文/行内评论/help/status check 描述/主要日志/LLM 输出语言。
# 支持 en / zh-CN / ru / fr / de / es（与 README 语言集一致）。
# 优先级：HOVERSTARE_LANGUAGE env > 本字段 > 默认 en；无法识别一律回退 en。
# 机器可读内容（hoverstare-meta、指纹标记、schema、命令名）永不本地化。
language = "en"

# 开发模式 commit 身份（spec 11 §3.3）：author（作者=触发者）/ bot（作者=bot）/
# coauthor（作者=触发者 + Co-authored-by trailer）；默认 coauthor。
commit_identity = "coauthor"
# 人类触发者姓名/邮箱的显式覆盖，形如 "Name <email>"（仅 author/coauthor 生效）。
# commit_author = "Alice <alice@example.com>"

# 自由文本，注入系统提示，写团队特定关注点
instructions = ""
```

## 配置合并优先级

CLI flag > 环境变量 > `.github/hoverstare.toml` > 内置默认值

校验规则（启动时 fail-fast，错误信息指出具体字段）：

- `model` 非空；`passes >= 1`；`max_diff_kb >= 50`；`max_tool_calls >= 1`
- `severity_threshold` 必须是枚举值之一
- `thinking` 必须是 `enabled` / `disabled`；`reasoning_effort` 必须是枚举值之一
- `context_tokens` 设置时必须 `>= 4096`
- `commit_identity` 必须是 `author` / `bot` / `coauthor`；`commit_author` 必须是 `Name <email>`
- 压缩参数必须满足 `0 < compaction_keep_ratio < compaction_threshold_ratio < 1`；`summary_max_chars >= 200`
- `ignore` 的 glob 必须可编译
- `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` 至少一个存在

## 跳过条件（`review` 满足任一即退出，exit 0）

- PR 为 draft 且 `review_drafts = false`
- PR 作者是 bot（`[bot]` 后缀，如 dependabot）
- diff 为空，或过滤后为空
- issue_comment 事件中评论不含 `@hoverstare`（mention 命令）

## 退出码契约

| code | 含义 |
|---|---|
| 0 | 成功；或分析阶段任何失败（fail-open，默认）；或跳过 |
| 1 | 配置错误；发布 review 和降级评论**都**失败；`fail_closed = true` 时的分析失败 |

设计理由：hoverstare 是辅助工具，自身故障（网络、API 限额、模型抽风）绝不阻塞用户 CI；
但配置错误属于用户需要立即修正的问题，应该显眼失败。

## 关键类型

```rust
pub struct Config {
    pub model: String,
    pub reformat_model: String,
    pub passes: u8,
    pub verify: bool,
    pub severity_threshold: Severity,
    pub ignore: globset::GlobSet,
    pub max_diff_kb: usize,
    pub max_tool_calls: u32,
    pub timeout_secs: u64,
    pub review_drafts: bool,
    pub fail_closed: bool,
    pub reasoning: ReasoningOptions, // thinking + reasoning_effort（spec 04）
    pub context_tokens: Option<u64>, // 模型上下文窗口（推导 diff 预算上限）
    pub status_checks: bool,
    pub instructions: String,
    pub commit_identity: CommitIdentity, // author | bot | coauthor（spec 11 §3.3）
    pub commit_author: Option<String>,   // "Name <email>" 覆盖触发者身份
    pub github_token: SecretString,
    pub llm: LlmCredentials, // Anthropic(key) | OpenAICompatible { key, base_url }
    pub workspace: PathBuf,
}

pub enum Severity { Low, Medium, High, Critical } // Ord: Critical > High > Medium > Low
```

## LLM provider 接入示例

`LlmCredentials::OpenAICompatible { api_key, base_url }` 覆盖所有 OpenAI 兼容端点：

| provider | base_url | model 示例 |
|---|---|---|
| Kimi Code（会员订阅） | `https://api.kimi.com/coding/v1` | `kimi-for-coding`（reformat 用 `kimi-for-coding-highspeed`） |
| Kimi 开放平台（按量） | `https://api.moonshot.cn/v1` | `kimi-k2.6` |
| OpenRouter | `https://openrouter.ai/api/v1` | `anthropic/claude-sonnet-4-5` |

注意点：

- 会员订阅端点有**频控**：多 pass 并发（默认 3 路）+ verifier 在高峰可能触发限流，
  撞限流时把 `passes` 降到 1–2；
- UA 合规：部分 provider 要求客户端保持真实 User-Agent，我们的 HTTP client 统一用
  `hoverstare/<version>`，不做伪装；
- Anthropic 兼容端点（如 `https://api.kimi.com/coding/`）也可走 `Anthropic` 凭据变体
  + base_url 覆盖，与 OpenAI 兼容路径二选一即可，默认用 OpenAI 兼容路径（实现更简单）。

## 测试要点

- toml 解析：空文件 / 全字段 / 非法枚举值 / 非法 glob
- 合并优先级：env 覆盖 toml，flag 覆盖 env
- 凭据校验：缺 key 时错误信息可读
