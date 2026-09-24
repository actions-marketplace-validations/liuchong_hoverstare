# HoverStare 设计方案

> 详细模块规格与开发计划见 [specs/](../specs/README.md)，本文档是高层概览。
> 两者冲突时以 specs 为准。

## 定位

HoverStare 是一个 Rust 编写的仓库 agent，以 GitHub Action 形态分发（单一静态
二进制）。它有两条主线：

1. **PR 审查**（根基功能）——对 PR 做**带仓库上下文的 agentic 审查**：审查模型可以像
   人类 reviewer 一样翻阅仓库（读上下文、查调用点、对比 base 版本）做定点验证，再给出
   结论；并用**多路独立审查 + 投票 + 逐条复核**压制误报。结果以精确到行的行内评论发布，
   跨 commit 跟踪每条发现，修复后自动关闭对应线程。
2. **Agent 开发模式**——把 issue 和 PR 当成 Web 版 AI 编程 IDE：issue 里调查、讨论、
   计划，`@hoverstare go` 拉分支实现并开 PR；PR 评论区下达任务即在分支上继续开发、
   commit、推回；支持任务队列（严格串行）、自触发续轮、提交身份与签名、`@hoverstare merge`。

两条主线共用同一套 agent 循环与工具沙箱，唯一区别是**工具 profile**：审查永远只读，
开发模式额外拿到写工具（spec 04 / spec 11）。

GitHub Action 是**最典型的接入形态**，不是唯一的形态：审查单位（spec 14）、语言规则包
（spec 15）、输出契约（spec 16）都不依赖平台事件，同一套核心可以被本地 CLI、serve、
以及后续接入的其它平台/形态复用。因此新能力一律按"形态无关的核心 + 形态适配层"来设计，
避免把能力写死在某个平台的 API 形状里。

## 核心能力

| 能力 | 说明 | spec |
|---|---|---|
| agentic 审查 | 只读工具集（read_file / grep / glob / list_dir / show_base_file），机器层强制只读 | [04](../specs/04-agent-backend.md) |
| 多 pass 投票 | 3 路并行独立审查 → 聚类 → ≥2 票入选 → 单票 verifier 复核 | [05](../specs/05-review-pipeline.md) |
| 精确锚定 | diff 解析出可评论行集合，非法行号按降级链吸附，同锚点合并 | [03](../specs/03-diff-engine.md) / [06](../specs/06-report-publish.md) |
| 增量审查 | synchronize 只审 delta，全量 diff 仅用于锚定 | [07](../specs/07-incremental-state.md) |
| 跨 commit 追踪 | 指纹 + 隐藏标记，修复后 GraphQL 自动 resolve 线程 | [07](../specs/07-incremental-state.md) |
| status checks | `hoverstare` / `hoverstare-findings`，可接 branch protection | [07](../specs/07-incremental-state.md) |
| 评论命令 | `@hoverstare review / explain / help` | [09](../specs/09-mention-commands.md) |
| 开发模式 | issue 讨论 → `go` 开 PR → PR 评论继续开发 → 推分支 | [11](../specs/11-agent-dev.md) |
| 自驱动队列 | 指令按评论 id 入队、严格串行、自触发轮取队首、失败即停、合并门 | [11](../specs/11-agent-dev.md) |
| 提交身份 | `commit_identity`（author/bot/coauthor）+ `commit_author` 覆盖 + GPG 签名 | [11](../specs/11-agent-dev.md) / [08](../specs/08-action-packaging.md) |
| 版本 pin | 流程开始时固定该流程构建的 revision，支持指令标记与手动 dispatch | [08](../specs/08-action-packaging.md) |
| 细粒度权限 | `.github/hoverstare.toml` 声明谁能用哪条命令（login / association） | [12](../specs/12-permissions.md) |
| 上下文压缩 | 阈值压缩 + 溢出恢复，前缀缓存友好（只追加、只压中段） | [13](../specs/13-context-compaction.md) |
| fail-open | 分析失败不阻塞 CI；仅配置错误与发布彻底失败才 exit 1 | [01](../specs/01-cli-config.md) |
| 覆盖契约 | 审查单元 + 确定性选择 + 零成本预览 + 覆盖账本（承诺集合 vs 实际集合） | [14](../specs/14-review-unit.md) |
| 语言规则包 | 按路径/扩展名命中的检查要点，注入与自检（`rules check`） | [15](../specs/15-rule-packs.md) |
| 结构化输出 | JSON 与 SARIF 出口，供 CI/安全面板/编辑器/其它 agent 消费 | [16](../specs/16-output-contract.md) |
| 工程门禁 | 引用 pin、文档结构、依赖审计、覆盖率、密钥扫描 | [17](../specs/17-verification-gates.md) |

## 架构

```
┌──────────────── GitHub Actions workflow / serve 模式 ────────────────┐
│ on: pull_request / issue_comment / issues / review / workflow_dispatch│
│ steps: 解析来源（事件版本或 pin）→ 构建/下载二进制 → 配置签名 → hoverstare │
└──────────────────────────────┬───────────────────────────────────────┘
                               ▼
                 hoverstare (single static binary, musl)
 ┌─────────────────────────────────────────────────────────────────────┐
 │ cli (clap):  review | mention | develop | serve | help              │
 ├─────────────────────────────────────────────────────────────────────┤
 │ orchestrator（审查编排）/ devagent（issue·PR 主线 + 轮次报告）        │
 │ devqueue（任务队列状态机）/ develop（开发轮 → conventional commit）    │
 ├──────────┬──────────┬──────────────┬────────────────┬──────────────┤
 │ github   │ diff     │ agent        │ report / state │ git          │
 │ REST/GQL │ 解析/锚定 │ 循环/工具/压缩│ 渲染/指纹/resolve│ 分支/commit/push│
 ├──────────┴──────────┴──────────────┴────────────────┴──────────────┤
 │ AgentBackend trait（框架类型不外泄）                                  │
 │  ├─ RigBackend（v1，rig-core；唯一 `use rig::*` 的模块）              │
 │  └─ NativeBackend（后续自研，可替换）                                 │
 └─────────────────────────────────────────────────────────────────────┘
```

## 关键技术决策

1. **GitHub Action 优先**：单静态二进制 + composite action，用户零运维接入；
   架构不绑死 Action（模块无副作用），`serve` 子命令另提供 webhook 服务形态。
2. **Agent 层用 Rig 框架、trait 隔离**：需求面（单 agent + 自定义只读工具 +
   多轮循环 + 结构化输出）正是 rig-core 的成熟路径，v1 用它快速上线；
   `AgentBackend` trait 把框架锁在一个模块里，保留切换自研 NativeBackend 的可能。
   详见 [specs/00](../specs/00-overview.md#agent-层选型决策记录)。
3. **安全默认**：工具注册表机器层只读（不靠 prompt 约束）；路径沙箱；不执行
   checkout 代码；系统提示声明 diff/代码为不可信数据，防 prompt injection。
   写工具只在开发模式注册，且同样过沙箱与预算。
4. **高 precision 优先**：多 pass 投票 + verifier + 明确排除清单（风格/文档/
   测试覆盖率不报），宁可少报不可乱报。
5. **状态存在 GitHub 侧**：指纹、轮次、队列都藏在评论/正文标记里，bot 本身无
   持久化、天然无状态、水平扩展零成本。
6. **spec-first**：`specs/` 是单一事实来源，先改 spec 再改代码；文档与实现漂移
   视为缺陷（本轮 dogfood 就修过若干处漂移）。
7. **队列而不是并发**：同一 PR 的指令严格串行执行（一轮一条、其余 pending），
   评论/review 类事件在 workflow 层排队而非取消；自触发只从队列取任务，队列空
   则不再续轮。
8. **提交身份与签名契约**：作者默认是触发者（`coauthor` 模式加
   `Co-authored-by: hoverstare[bot]` 尾注），`commit_author` 可覆盖；**覆盖时
   committer 也用同一人**——GitHub 按 committer 校验签名，而 App 账号不能持有
   密钥，committer 是 bot 时即使签名正确也只会显示未验证。
9. **流程级 pin**：一个流程（issue → go → PR）开始时记录当时的默认分支 revision，
   后续每一轮都构建该 revision（并按该 commit 缓存），避免 master 前进导致流程中途
   换代码与每轮重建；指令标记只接受 master / release tag / 可从默认分支到达的 commit。
10. **审查单元是形态无关的核心单位（spec 14）**：并发、上下文隔离、覆盖分母、报告聚合
    都以审查单元计；选择逻辑收敛成一个纯函数，预览与真实运行同源（两处各算一遍必然漂移）。
    "答应了审什么"必须由机器记账，不能靠模型自觉。
11. **规则按文件命中，而不是全量塞提示（spec 15）**：语言规则包只把与当前文件相关的
    检查要点注入进去；它与多 pass 视角**正交**（不是第四路 pass），权威低于核心规则与
    仓库指令，且随二进制内置（不新增外部输入面）。
12. **对下游要有稳定出口（spec 16）**：评论是给人看的出口，JSON/SARIF 是给机器看的出口；
    两者共享同一份 findings 数据模型，路径/排序/枚举封闭，凭据与原始 prompt 绝不出现在输出里。
13. **纪律要机器化（spec 17）**：引用 pin、文档结构对齐、依赖审计、覆盖率不劣化、密钥扫描
    全部做成门禁（本地一条命令、CI 阻塞）；门禁失败属工程问题，**不进入 fail-open 区间**。
14. **威胁模型与设计同级（docs/threat-model.md）**：新增形态、凭据种类、写能力或外部输入
    来源时，必须同步更新威胁模型；涉及凭据、写操作、路径沙箱、提示注入面的改动要在 PR 里
    显式引用对应小节。

## 配置一览（`.github/hoverstare.toml`，全部可选）

```toml
model = "claude-sonnet-4-6"
reformat_model = "claude-haiku-4-5"  # 输出修复模型（reformat pass），默认 claude-haiku-4-5
passes = 3                    # 并行审查路数，1 = 关闭投票
verify = true                 # 单票 finding 过复核
severity_threshold = "medium" # 低于此级别只进摘要 Nitpicks
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400
max_tool_calls = 20           # 单轮工具调用预算
timeout_secs = 900
fail_closed = false           # true 时分析失败会让 CI 失败
status_checks = false
review_drafts = false
language = "en"               # 输出语言 en/zh-CN/ru/fr/de/es
set_temperature = true        # 只接受默认温度的端点设 false
thinking = "enabled"          # OpenAI 兼容端点的思考模式
reasoning_effort = "medium"
context_tokens = 200000       # 模型上下文窗口（推导预算用）
compaction = true             # 上下文压缩（spec 13）
compaction_threshold_ratio = 0.75
compaction_keep_ratio = 0.25
summary_max_chars = 4000
max_rounds = 0                # 单次 run 模型调用轮数上限，0 = 由工具预算推导
max_output_tokens = 0         # 单次调用输出上限，0 = 由窗口推导（推理 token 计入）
commit_identity = "coauthor"  # author | bot | coauthor
commit_author = ""            # 可选 "Name <email>" 覆盖作者（同时作用于 committer）
instructions = ""             # 团队特定关注点，注入系统提示

[permissions]                 # 谁能用哪条命令（spec 12）
auto_review = ["anyone"]
review = ["collaborator"]
develop = ["collaborator"]
merge = ["write"]
```

## 开发计划

按可独立验收的里程碑推进（任务分解与验收标准见
[specs/README.md](../specs/README.md#里程碑计划)）：

> M17–M20 的实现设计（模块/类型/接线）见 [dev/m17-m20-design.md](dev/m17-m20-design.md)，
> 施工顺序、批次与验收见 [dev/m17-m20-plan.md](dev/m17-m20-plan.md)。

| 里程碑 | 内容 | 状态 |
|---|---|---|
| M1 | 端到端骨架：demo PR 上发出第一条合法行内评论 | ✅ |
| M2 | 健壮性：行号降级链、大 diff、输出容错、fail-open | ✅ |
| M3 | agentic 循环：只读工具 + 定点验证 + 预算控制 | ✅ |
| M4 | 增量审查 + 指纹追踪 + 自动 resolve + status checks | ✅ |
| M5 | 多 pass 投票 + verifier | ✅ |
| M6 | `@hoverstare` 评论命令 | ✅ |
| M7 | release 流水线 + action 打包 + 文档打磨 | ✅ |
| M8 | serve 模式（可选自部署 webhook 服务） | ✅ |
| M9 | 国际化（六语言输出与 README） | ✅ |
| M10 | 仓库指令文件 | ✅ |
| M11-M13 | agent 开发模式：写工具、issue 主线、PR 轮次、自触发、合并命令 | ✅ |
| M14 | 细粒度权限（`.github/hoverstare.toml` `[permissions]`） | ✅ |
| M15 | 自驱动队列：严格串行、失败即停、合并门与 `queue` 命令 | ✅ |
| M16 | 提交身份与签名、流程级 pin、上下文压缩与缓存可观测 | ✅ |
| M17 | 审查单元与覆盖契约：确定性选择、零成本预览、覆盖账本、覆盖声明、严格模式、成组（spec 14） | ✅ |
| M18 | 语言规则包：按路径命中、注入契约、`rules check`（spec 15） | ⏳ 计划中 |
| M19 | 输出契约：JSON 与 SARIF 出口、usage 聚合（spec 16） | ✅ |
| M20 | 工程门禁：引用 pin、文档结构、依赖审计、覆盖率、密钥扫描（spec 17） | ⏳ 计划中 |

**当前状态**：M1-M16 全部完成；**M17 已完成**（spec 14 验收逐条通过，见施工计划）；**M19 已完成**；M18、M20 的 spec 已定稿、实现待做。`cargo test --workspace` 214 项（单元 + httpmock 合约），
`cargo clippy --all-targets -D warnings` 与 `cargo fmt --check` 干净，CI 另跑
actionlint（workflow 文件本身的有效性）。
