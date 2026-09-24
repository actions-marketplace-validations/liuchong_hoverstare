# HoverStare 开发计划与 Spec 索引

本目录是 HoverStare 的单一事实来源（single source of truth）。所有功能开发必须先有对应 spec，
实现与 spec 不一致时以 spec 为准（或先改 spec）。

## 项目一句话

HoverStare 是一个 Rust 编写的 AI 仓库 agent：根基是 **PR 代码审查**（带仓库上下文的
agentic 审查，把高置信度缺陷以行内评论发到 PR 上并跨 commit 追踪）；在此之上扩展
**Agent 开发模式**（spec 11）——把 issue 和 PR 当成 Web 版 AI 编程 IDE，调查、讨论、
计划、在 PR 分支上开发并推送，自有 agent 体系，非桥接。

## Spec 索引

| Spec | 模块 | 里程碑 |
|---|---|---|
| [00-overview.md](00-overview.md) | 总体架构、数据流、技术选型、非目标 | — |
| [01-cli-config.md](01-cli-config.md) | CLI 子命令、配置文件、环境变量、退出码 | M1 |
| [02-github-client.md](02-github-client.md) | GitHub REST/GraphQL 客户端 | M1 |
| [03-diff-engine.md](03-diff-engine.md) | diff 解析、可评论行映射、过滤与截断 | M1 |
| [04-agent-backend.md](04-agent-backend.md) | AgentBackend 抽象、Rig 实现、只读工具集、输出容错 | M1/M3 |
| [05-review-pipeline.md](05-review-pipeline.md) | 多 pass 编排、投票聚合、verifier | M5 |
| [06-report-publish.md](06-report-publish.md) | findings 校验、行号锚定、评论渲染、发布与降级 | M2 |
| [07-incremental-state.md](07-incremental-state.md) | 指纹、增量审查、跨 commit 追踪、自动 resolve | M4 |
| [08-action-packaging.md](08-action-packaging.md) | action.yml、release 构建、缓存、用户接入 | M7 |
| [09-mention-commands.md](09-mention-commands.md) | `@hoverstare` 评论命令 | M6 |
| [10-serve-mode.md](10-serve-mode.md) | 可选自部署 webhook 服务（零配置 hoverstare[bot]） | M8 ✅ |
| [11-agent-dev.md](11-agent-dev.md) | Agent 开发模式：issue 驱动 + PR 循环开发（写工具、推分支、自触发、合并命令） | M11-M13 |
| [12-permissions.md](12-permissions.md) | 细粒度权限系统：`.github/hoverstare.toml` 声明谁可用哪条命令 | M14 |
| （并入 11）自驱动队列 | 任务队列状态机、claim 守卫、artifact gate、合并队列门、`queue` 命令 | M15 |
| （并入 11/08/01）提交身份与签名、流程级 pin | coauthor/author/bot、GPG 签名、流程起始 revision 固定 | M16 |
| [13-context-compaction.md](13-context-compaction.md) | agentic 循环的上下文压缩：阈值压缩 + 溢出恢复（自持 history） | — |
| [14-review-unit.md](14-review-unit.md) | 审查单元与覆盖契约：确定性选择、零成本预览、覆盖账本 | M17 |
| [15-rule-packs.md](15-rule-packs.md) | 语言规则包：按路径命中的检查要点、注入契约、`rules check` 自检 | M18 |
| [16-output-contract.md](16-output-contract.md) | 输出契约：JSON 与 SARIF 结构化出口 | M19 |
| [17-verification-gates.md](17-verification-gates.md) | 工程门禁：引用 pin、文档结构、依赖审计、覆盖率、密钥扫描 | M20 |
| [validation-2026-07-18.md](validation-2026-07-18.md) | 真实环境端到端验证记录 | — |

## 里程碑计划

> 每个里程碑都是可独立验收的垂直切片，按顺序开发。估时以单人全职为参考。

> M17–M20 的实现设计与施工安排另见 [docs/dev/m17-m20-design.md](../docs/dev/m17-m20-design.md) 与 [docs/dev/m17-m20-plan.md](../docs/dev/m17-m20-plan.md)（spec 定义目标，设计文档定义落法）。

### M1 — 端到端骨架（约 1.5 周）✅ 2026-07-17 完成

**目标**：在 demo PR 上发出第一条合法行内评论。单 pass、无投票、无增量。

- [x] `cargo init`，cli：`hoverstare review` 从 `GITHUB_EVENT_PATH` 读取 PR 事件
- [x] config：env + `.github/hoverstare.toml` 合并，含校验（spec 01）
- [x] github client：GET PR、GET diff、POST reviews（spec 02，先不用 GraphQL）
- [x] diff engine：parser + 可评论行集合（spec 03，含单测 fixture）
- [x] agent：`AgentBackend` trait + RigBackend 单 pass，无工具调用（spec 04）
- [x] **spike**：验证 rig 走自定义 base_url 接 Kimi Code 端点 ✅ 2026-07-17，
  6/6 通过（`spikes/rig-kimi-probe`），结论已固化进 spec 04
- [x] publish：行号校验 → inline review 发布；失败降级为摘要评论（spec 06 的子集）

**验收结果**（达成）：

- 公开 PR 端到端 dry-run（0xPlaygrounds/rig#2162，1 文件）：正确判无缺陷，
  body 渲染与元数据注释正确 ✅
- 埋雷 diff（tests/fixtures/buggy.diff，3 个 bug）经 `examples/local_review`
  完整链路：**3/3 命中，行号全部精确**，行内评论格式合法（含 suggestion 块）✅
- 行号非法时吸附降级链不崩（report 单测覆盖）✅
- 26 单测全绿 + clippy -D warnings + fmt ✅

**M1 经验记录**（供 M2 参考）：

1. reqwest 的 `.header()` 是追加不是覆盖——Accept 双写导致 GitHub 返回 JSON 而非
   diff，排查半天。已加注释，httpmock 合约测试（M2）需覆盖响应类型断言。
2. 实测出现一次模型返回**空输出**（2.5 分钟后）——M2 的重试/reformat 管线
   必须处理空输出这种形态，不仅是散文。

### M2 — 健壮性（约 1 周）✅ 2026-07-17 完成

**目标**：对畸形的模型输出和大 diff 有完整防线。

- [x] findings 归一化（line 字符串→整数、缺省 severity 等）+ jsonschema 结构校验
- [x] 行号吸附降级链：同行 hunk 最近行 → 全局最近行（orphan 标注）→ body 段落
- [x] 同锚点评论合并（防 GitHub 422）
- [x] 大 diff：ignore 过滤、按文件优先级截断（整文件粒度 + TRUNCATED 声明入 prompt）、
  >300 文件走 files API 分页回退
- [x] 输出容错：三级 JSON 解析 → reformat pass（廉价模型）→ 全量重试（最多 3 次）
- [x] fail-open：分析失败 exit 0；仅发布彻底失败 exit 1（GitHub I/O 失败也纳入
  fail-open 区间；fail_closed 可反转）

**验收结果**（达成）：

- 坏 JSON / 结构错误输出 / 散文输出 / 空输出：FakeBackend 管线测试 5 项全绿 ✅
- files API 回退、422 错误透传、429 重试耗尽：httpmock 合约测试 4 项全绿 ✅
- 断网 exit 0、fail_closed exit 1、配置错误 exit 1：手动验证 ✅
- 重构后埋雷 diff 回归：3/3 命中、行号精确 ✅
- 39 测试全绿 + clippy -D warnings + fmt ✅

### M3 — Agentic 循环（约 1.5 周）✅ 2026-07-17 完成

**目标**：模型可以翻仓库做定点验证。

- [x] 只读工具集：read_file / grep / glob / show_base_file（路径沙箱 + 输出截断）
- [x] RigBackend 接入工具注册与多轮循环，预算控制（max_tool_calls / timeout）
- [x] 系统提示：JSON-only 契约、不可信数据声明、定点查证纪律
- [x] 工具调用轨迹记录（结构化日志 + `examples/local_review` 打印，供调试与回放）

**验收结果**（达成）：

- 调用方破坏场景（tests/fixtures/caller_repo，bug 藏在 store.rs 而非 diff 中）：
  模型**主动 grep `make_id` → read_file `src/store.rs`**，正确报告 CRITICAL
  （非确定性 ID 破坏主键/去重语义），附 related location 与合理 suggestion ✅
- 预算耗尽（max_tool_calls=1）：预算闸拦截第 2 次调用，模型基于已有信息
  优雅收尾输出合法 JSON ✅
- 沙箱单测：绝对路径 / `..` 逃逸 / 符号链接逃逸全部拒绝；预算闸与轨迹记录正确 ✅
- 46 测试全绿 + clippy -D warnings + fmt ✅

**M3 经验记录**：

1. rig 0.36 的 `Tool` trait 不要求工具结构体 Serialize/Deserialize，可以放心携带
   `Arc<ToolShared>`；`AgentBuilder` 是三泛型 `<M, P, ToolState>`，注册工具走
   `NoToolConfig → WithBuilderTools` 状态迁移。
2. 轮次上限 = max_tool_calls + 2（收尾轮），给模型留结论空间。

### M4 — 增量审查与跨 commit 追踪（约 1 周）✅ 2026-07-18 完成

**目标**：`synchronize` 事件只审增量，已修复的线程自动 resolve。

- [x] 指纹生成与隐藏标记（spec 07；sha1(path+行内容+标题)[:16]，行号漂移免疫）
- [x] GraphQL：reviewThreads 查询（分页）+ resolveReviewThread
- [x] 增量模式：list_reviews 定位上次 hoverstare review（meta 注释解析），
  compare API 取 delta diff 为主审范围，全量 diff 仅锚定
- [x] 输出 schema 增加 `resolved_finding_ids`；未修复 finding 不重复评论
  （指纹 ∈ 未关闭集合 → carried_over 跳过）
- [x] status checks：`hoverstare` / `hoverstare-findings`（历史线程高危级别从标记 emoji 判定）

**验收结果**（达成）：
- 指纹稳定性/标记解析/meta 解析/线程 resolve 规则（全指纹修复才 resolve）单测全绿 ✅
- GraphQL 分页拼接、resolve mutation、errors 字段判断、list_reviews、
  compare diff、create_status：httpmock 合约测试全绿 ✅
- 埋雷回归：3/3 命中且每条行内评论带指纹标记 ✅
- 50 测试全绿 + clippy -D warnings + fmt ✅

### M5 — 多 pass 投票与 verifier（约 1 周）✅ 2026-07-18 完成

**目标**：显著降低误报率。

- [x] 3 路并行 pass（不同侧重 prompt + 温度错开），futures::join_all 并发
- [x] 聚类合并与投票：≥2 票入选，1 票进 verifier
  （聚类键：同文件 + 行距 ≤3 + 标题 Jaccard ≥0.5；CJK 用单字+二字组 n-gram 分词）
- [x] verifier：逐条带工具复核（驳回需证据、存疑从留），驳回即丢弃
- [x] 小 diff（新增行 < 50）自动降级为 1 pass + verify

**验收结果**（达成）：
- 聚类/投票/合并/verifier/降级/单 pass 失败不连坐/全失败报错：管线测试全绿 ✅
- passes=1 && !verify 直通路径 = M2 容错管线，行为回归 ✅
- 真实回归（小 diff → 1 pass + verify）：verifier 用工具复核，
  确认 2 条真实 bug（除零/越界）、驳回 1 条 ✅
- **实测发现并修复**：kimi-for-coding 端点只接受 temperature=1，
  新增 `set_temperature` 配置（spec 01），置 false 时不传温度字段 ✅
- 63 测试全绿 + clippy -D warnings + fmt ✅

### M6 — `@hoverstare` 评论命令（约 0.5 周）✅ 2026-07-18 完成

**目标**：评论里可以指挥 bot。

- [x] issue_comment / pull_request_review_comment 事件解析，命令：review / explain / help
- [x] 权限：仅 repo collaborator 可触发（其余只回 👀 reaction）
- [x] 接单/完成/失败 reaction（🚀/+1/-1）；代码块内的 @hoverstare 不响应
- [x] review 命令强制全量；explain 轻量调用（线程上下文 + 300 字内中文解释）
- [x] 并发：workflow `concurrency: cancel-in-progress`，最新命令优先（spec 08 示例）

**验收结果**（达成）：
- 命令解析（含代码块排除）/ 权限判定 / 事件解析（PR 评论/纯 issue/线程回复）单测全绿 ✅
- reactions 端点区分（issues vs pulls）/ 线程评论获取：httpmock 合约测试全绿 ✅
- 66 测试全绿 + clippy -D warnings + fmt ✅

### M10 — 仓库指令文件（repo instructions）✅ 2026-07-19 完成

**目标**：支持 AGENTS.md 及兼容规则文件，优先级自由定义，核心规则不可覆盖。

- [x] `instructions` 模块：分层加载（自有 hoverstare.md/.hoverstare.md/.hoverstare/*/
  .github/hoverstare.md → AGENTS.md → copilot-instructions/CLAUDE.md/.cursorrules）
- [x] **base 分支读取**（防 PR 修改 AGENTS.md 注入指令）；单文件 4KB / 总 8KB 截断
- [x] 注入系统提示 [REPOSITORY INSTRUCTIONS] 段落（附"永不覆盖核心规则"声明）
- [x] 不可覆盖内建核心清单写入 spec 04
- [x] 测试：base/head 注入防护、自有文件优先级、目录加载、渲染声明

### M9 — 国际化（i18n）

**目标**：代码注释全英文；用户可见输出按配置多语言。

- [x] `i18n` 模块：Lang（en/zh-CN/ru/fr/de/es，与 README 语言集一致）+ 文本目录 T
- [x] 配置：`language`（toml）/ `HOVERSTARE_LANGUAGE`（env 优先）> 默认 en，无法识别回退 en
- [x] 本地化范围：review 正文、行内评论、help、status check 描述、主要业务日志、
  LLM prompt 输出语言指令、explain 回复、resolve 标记、**预览输出（汇总行 / will review /
  excluded 标签）与覆盖声明行**（spec 14；机器可读载荷如 `hoverstare-meta` 与预览 JSON 永不本地化）
- [x] prompt 全部改英文 + 输出语言指令（finding 描述语言跟随配置）
- [x] 代码注释与硬编码日志全英文化
- [x] 机器可读内容（hoverstare-meta、指纹、schema、命令名）永不本地化

### M8 — serve 模式（可选自部署 webhook 服务）✅ 2026-07-18 完成

**目标**：用户安装 GitHub App 后零配置获得 hoverstare[bot] 审查（无需 workflow、无需 secrets）。

- [x] `hoverstare serve` 子命令（axum，`POST /webhook` + `GET /healthz`）
- [x] HMAC-SHA256 验签（常量时间比较，先验签后解析）
- [x] App JWT（RS256）+ 安装令牌交换与缓存（提前 10 分钟刷新）
- [x] 事件路由：pull_request / issue_comment / pull_request_review_comment
- [x] fork 安全的工作区克隆（按 sha fetch + head 仓库回退）
- [x] 并发上限 semaphore + 同 PR 串行互斥锁
- [x] 复用编排（clone cfg 注入安装令牌 + 任务工作区）
- [x] Dockerfile（多阶段构建）+ docs/deploy.md（Docker/fly.io/自部署）
- [x] spec 10

**验收结果**（真实环境端到端通过）：
- healthz / 坏签名 401 / 无事件 ignored ✅
- 真实 webhook（PR opened）：验签 → 安装令牌 → 克隆 → 多 pass 审查
  （2 路成功 + verifier 确认 2 条 finding）→ **以 hoverstare[bot] 发布** ✅
- 过程中修复：jsonwebtoken 需显式启用 `aws_lc_rs` + `use_pem` feature；
  默认分支克隆无法 checkout 非默认分支 head sha（按 sha fetch 修复）

### M7 — 发布与打磨（约 1 周）✅ 2026-07-18 完成

**目标**：新仓库 5 分钟接入。

- [x] release workflow：tag → musl 静态二进制 → strip → tar.gz + sha256 →
  GitHub Release → 大版本浮动 tag（v1 自动跟进修复版）
- [x] action.yml（composite）：平台识别 → 缓存 → 下载 → sha256 校验 →
  按事件类型运行 review/mention
- [x] 用户文档：根 README（2 分钟接入、配置全表、原理、命令、FAQ、GHE）
- [x] CI workflow：fmt/clippy/test/musl 冒烟；dogfood workflow（源码构建自审）
- [x] 日志分组（::group:: 三阶段）；AGENTS.md（agent 项目约定）
- [ ] ~~prompt cache、成本统计~~（rig agent.prompt 不暴露 usage，记为后续优化项）

**验收**：YAML 全部通过校验；action 下载/校验/缓存逻辑待首个 release tag 后
在 fork 仓库手动验证（spec 08）。

### M11-M13 — Agent 开发模式 ✅ 2026-07-19 完成

**目标**：issue 与 PR 评论区就是 AI 编程 IDE。

- [x] 写工具（`write_file` / `edit_file` / 受控命令白名单）+ 与审查共用的路径沙箱与预算
- [x] issue 主线：讨论轮（`@hoverstare <指令>`）→ 计划 → `@hoverstare go` 开分支与 PR
- [x] PR 主线：评论下达任务 → 在 PR 分支开发 → conventional commit → push → 轮次报告
- [x] 自触发续轮（每 PR 上限 10 轮，人类指令不受限）与失败即停
- [x] `@hoverstare merge`：checks 全绿 + 无冲突 + 队列为空 → squash 合并并删分支
- [x] 令牌职责分离：身份走 App token、写操作走 PAT 类令牌

**验收**：真实仓库（本仓库自身 dogfood）走通 issue → PR → 多轮开发 → merge；
期间修掉的真问题记入 `AGENTS.md` §7（冲突静默掐 CI、输出额度、prefix cache 契约等）。

### M14 — 细粒度权限 ✅ 2026-07-19 完成

- [x] `.github/hoverstare.toml` 的 `[permissions]`：`auto_review` / `review` / `develop` / `merge`
      按 login 或 association 声明；默认 auto_review=anyone、命令类=collaborator、merge=write

**验收**：单测覆盖 authorization 判定；dogfood 上人类指令与自触发分别走不同判定。

### M15 — 自驱动队列 ✅ 2026-09-16 完成

**目标**：把"一连串指令"变成可预期、可中断、可核对的串行工作流。

- [x] `devqueue`：append-only 隐藏标记载体、按来源评论 id 去重、上限与剪枝、状态机
- [x] 出队规则：running 优先 → 人类条目按 id 升序 → bot 条目；一轮只取一条
- [x] claim 守卫（被更新的 run 静默退出）与 artifact gate（记录 sha 必须是 head 祖先）
- [x] 合并队列门：pending 时拒绝合并并原样贴出清单，`force` 放行并说明丢弃条数
- [x] `@hoverstare queue` 查询命令；轮次报告带队列状态与"正在跑"标识
- [x] 接线：人类指令入队、本轮置 Running、结束置 Done/Failed，自触发从队首取任务

**验收**：真实 PR 上连发两条指令 → 严格串行（第二条在其运行的整段时间里保持 pending）、
各自独立提交、队列清空后不再自触发；`enqueue`/`set_state` 在生产路径有调用点（复合
"能力存在 ≠ 可达"的复核纪律）。

### M16 — 提交身份、签名与流程级 pin ✅ 2026-09-16 完成

- [x] `commit_identity = author|bot|coauthor`（默认 coauthor）与 `commit_author` 覆盖；
      覆盖同样作用于自触发轮与本地 `run --task`（此二者没有人类触发者）
- [x] 覆盖时 committer 与 author 同为人：GitHub 按 committer 校验签名、App 不能持有密钥，
      实测 committer 为 bot 时正确签名也只会得到 `unknown_key`
- [x] CI 签名：`HOVERSTARE_GPG_PRIVATE_KEY`（+可选 `HOVERSTARE_GPG_PASSPHRASE`）导入密钥并
      置 `commit.gpgsign`，导入后做一次空提交自检，签不出来直接失败
- [x] 流程级 pin：流程开始时把默认分支 revision 写进 PR body 的隐藏标记，后续轮次固定构建它，
      并按该 commit 单独缓存；标记只接受 master / release tag / 可从默认分支到达的 commit
- [x] 轮次报告显示本轮构建来源；上下文压缩与 prefix cache 可观测

**验收**：dogfood 流程实测——PR body 带 pin 标记、日志 `building hoverstare from pinned ref …`、
`Build (pinned revision)` 成功且事件版本被跳过；bot 产出的提交 `author`/`committer` 均为维护者、
签名通过且 GitHub 侧 `verified=true`。

### M17 — 审查单元与覆盖契约 ✅ 2026-09-24 完成

**目标**：把"答应了审什么 / 实际审了什么"变成机器可核对的账，而不是模型的自觉。

- [x] ReviewUnit 抽象与确定性成组（`group_units`，默认关；组 id 由组键派生）
- [x] 选择收敛为纯函数 + 排除原因枚举（`SelectOptions`；预览与运行共用同一实现）
- [x] `hoverstare review --preview`：零模型调用输出 will_review / excluded（含 `--format json`）
- [x] 覆盖账本：分母冻结、单元状态机、终态 ok/partial/empty；失败注记带覆盖计数
- [x] 摘要与轮次报告呈现覆盖声明（`report_coverage`，默认开；六语言文案 + 元数据字段）
- [x] `select_strict`：`secret-path` / `default-path` / `extension` 三类保留原因

**验收**：见 [spec 14 §10](14-review-unit.md)；逐条证据见 [施工计划](../docs/dev/m17-m20-plan.md#s1--m17-审查单元与覆盖契约)。

### M18 — 语言规则包 ✅ 2026-09-24 完成

**目标**：每份改动只注入与它相关的检查要点。

- [x] 规则包数据形态（id/version/scope/checks/examples）+ schema 校验单测
- [x] 匹配解析：first match wins、专指性排序、歧义嗅探与回退、多包上限
- [x] `[LANGUAGE RULES]` 注入契约（字节上限、不与核心规则/仓库指令冲突）
- [x] `hoverstare rules list` / `rules check <path>` 只读自检
- [x] 初始包：rust / go / ts-js / python / ci-yaml / objc / matlab / default（含正反样例；
      objc/matlab 是 `.m` 歧义批次的两个真实候选）

**验收**：见 [spec 15 §12](15-rule-packs.md)。

### M19 — 输出契约 ✅ 2026-09-24 完成

**目标**：同一份结果既有评论出口，也有可供下游消费的结构化出口。

- [x] `--format human|json|sarif` + `--output`（stdout 纯净，日志走 stderr）
- [x] JSON 契约（schema_version / run / units / findings / resolutions / coverage，稳定排序）
- [x] SARIF 2.1.0 映射（级别、region、partialFingerprints、fixes、invocations）
- [x] 无法锚定的 finding 以文件级结果进入 SARIF（不污染评论）
- [x] 安全：输出不含凭据、原始 prompt、绝对路径
- [x] usage 聚合：多 pass、verifier、reformat 与失败重试都计入（`calls` 字段）

**验收**：见 [spec 16 §10](16-output-contract.md)。

### M20 — 工程门禁 ✅ 2026-09-24 完成

**目标**：把纪律变成机器门禁（本地一条命令、CI 阻塞）。

- [x] `scripts/verify-action-pins.sh`（G1）+ 15 处 action/workflow 引用全部 pin 到 SHA
- [x] `scripts/check-doc-structure.sh`（G2）+ 六份 README 结构对齐
- [x] 依赖与许可审计（G3）：`deny.toml` 按依赖图实际许可写；首次 CI 运行即抓到 2 个真实漏洞（h2 / rustls）并已按补丁版本升级
- [x] 覆盖率基线（G4，实测 80.36% → 阈值 79.8%）与不劣化判定
- [x] 密钥扫描（G5，`.gitleaks.toml`）；actionlint 同时覆盖门禁脚本自身（G6）
- [x] spec 与模块一致性检查（G7）；`scripts/verify-all.sh` 汇总；CI 两个阻塞 job（`gates` 快集合、`gates-heavy` 重集合）

**验收**：见 [spec 17 §7](17-verification-gates.md)。CI 全绿证据：commit `5c3a7cb` 的 run
（`workflow-lint` / `check` / `gates` / `gates-heavy` 四个 job 全部 success）。

**验收**：见 [spec 17 §7](17-verification-gates.md)。


## 测试策略

| 层 | 方法 |
|---|---|
| diff parser / 指纹 / 投票聚合 | 纯单测，fixture 驱动（`tests/fixtures/`） |
| github client | `httpmock` 合约测试（REST + GraphQL） |
| agent backend | 录制工具轨迹回放；RigBackend 用 mock completion model |
| 端到端 | demo repo 手动验收（每里程碑一次） |

## 开发约定

- 稳定版 Rust，edition 2024；`cargo fmt` + `cargo clippy -- -D warnings` 必须全绿
- 错误处理：库代码用 `thiserror`，bin 用 `anyhow`；禁止 `unwrap()`（测试除外）
- 日志用 `tracing`；机密一律 `secrecy::SecretString`，禁止出现在日志
- 每个 spec 对应 `src/` 下同名模块；新增模块先补 spec
- 工程门禁（引用 pin、文档结构、依赖审计、覆盖率、密钥扫描）见 spec 17：本地 `scripts/verify-all.sh`，CI 阻塞
