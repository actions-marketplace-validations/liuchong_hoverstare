# AGENTS.md — HoverStare 项目 Agent 指南

> 给后续维护本项目的代码 agent：本文是项目背景、架构决策、硬性约定与运维经验的
> 单一事实来源。模块级设计细节见 [`specs/`](specs/README.md)；本文负责"为什么这么设计"。

## 1. 项目是什么

**HoverStare（代号 bugbot）**：Rust 编写的 AI 仓库 agent，以单一静态二进制
（musl）通过 GitHub Action 分发。两大功能：**(1) PR 审查**（根基功能）——对 PR 做
仓库感知的 agentic 审查，审查模型带只读工具集翻阅仓库做定点验证，多路并行
审查 + 投票 + 逐条复核压制误报，行内评论精确锚定，跨 commit 指纹追踪；
**(2) Agent 开发模式**（spec 11，M11-M13）——把 issue 和 PR 当成 Web 版 AI 编程
IDE：issue 里调查/讨论/计划，`@hoverstare go` 后拉分支实现并开 PR；PR 评论区
下任务，在 PR 分支上开发、commit 并推回本仓库分支，支持自触发熔断和
`@hoverstare merge`。自有 agent 体系，非桥接。

- 仓库：<https://github.com/liuchong/hoverstare>
- 双 crates 发布：`hoverstare`（主） + `bugbot`（同代码别名包，行为一致）
- 协议：1PL（One Public License，<https://license.pub/1pl/>）
- 命名彩蛋：HoverStare 源自周星驰电影《百变星君》"凌空瞪"——悬浮眼球直勾勾瞪人；
  logo 即"悬浮眼球瞪一只冒汗的小虫"。README 六种语言都带"凌空瞪"出处。

## 2. 项目沿革（怎么走到今天的）

1. **立项**：目标做一个 Cursor Bugbot 类的 AI PR 审查工具，要求 GitHub Action 形态、
   Rust 开发、能结合仓库上下文（不是只看 diff）。
2. **关键选型**：agentic 层用 **rig-core** 框架快速上线，但用 `AgentBackend` trait
   隔离框架，预留切换自研 NativeBackend 的可能（评估过自研循环、包 CLI agent、
   其他 Rust agent 框架后的折中）。
3. **spike 验证**：先写探针验证 rig 接 Kimi Code 端点（自定义 base_url +
   tool_use + 并发），6/6 通过后才开工（`spikes/rig-kimi-probe`）。
4. **M1–M7 里程碑**（全部完成，详见 `specs/README.md` 验收记录）：
   骨架 → 健壮性 → agentic 循环 → 增量追踪 → 多 pass 投票 → @命令 → 发布打包。
5. **真实环境验证**（`specs/validation-2026-07-18.md`）：演示 PR 全流程通过——
   精确行内评论、增量审查、resolve 降级、@命令、action 分发路径。
6. **发布与改名**：先以 bugbot 发布 crates v0.0.1；Marketplace 因名字冲突
   （GitHub 已有同名用户）改名 **HoverStare**，全项目彻底改名；
   crates 双发布（hoverstare 主包 + bugbot 别名包）。
7. **开发模式（M11-M13，2026-07-19）**：从审查 bot 扩展为开发 agent（spec 11）。
   关键事实：App 推送可触发 CI、GITHUB_TOKEN 推送不触发；自触发评论是唯一豁免
   collaborator 校验的 bot 发言；bot 自己发的 review 会产生 pull_request_review
   事件，并发组必须给它 noop 组名（run 级并发先于 job if 生效），否则取消
   正在跑的审查。

## 3. 架构地图

```
src/
├── main.rs            # 薄入口：cli::run()（与 bugbot 别名二进制共用）
├── lib.rs             # 模块声明
├── cli.rs             # clap 子命令 review/mention/develop + 入口逻辑
├── config.rs          # env + .github/hoverstare.toml 合并校验（spec 01）
├── event.rs           # pull_request / issue_comment / develop 事件解析
├── github.rs          # REST + GraphQL 客户端（spec 02）
├── diff.rs            # 容错 diff 解析、过滤、优先级截断（spec 03）
├── agent/
│   ├── mod.rs         # AgentBackend/ChatClient trait + ToolProfile（审查永远只读）
│   ├── agent_loop.rs  # 自持 history 的 agentic 循环 + 阈值压缩 + 溢出恢复（spec 13）
│   ├── compaction.rs  # 压缩计划/粗摘要/dump/摘要契约/溢出识别（框架无关，spec 13）
│   ├── rig_backend.rs # 唯一允许 use rig::* 的文件：一次 provider 调用，不跑循环
│   └── tools.rs       # 工具元数据+分发+只读/写工具集 + 路径沙箱 + 预算 + 轨迹
├── develop.rs         # develop 核心循环：agent 开发 → conventional commit（spec 11）
├── devagent.rs        # issue/PR 主线编排：讨论/计划/go/开发轮/merge（spec 11）
├── git.rs             # git 操作（分支/commit/push/fetch/checkout -B，错误脱敏）
├── pipeline.rs        # 多 pass 投票 + verifier + 容错管线（spec 05/04）
├── prompt.rs          # 系统提示契约（JSON-only、不可信数据、定点查证）
├── instructions.rs    # 仓库指令文件加载（base 分支读取，spec 04 §repo-instructions）
├── findings.rs        # 三级 JSON 提取 + jsonschema + 归一化
├── report.rs          # 锚定降级链、同锚点合并、渲染（spec 06）
├── state.rs           # 指纹、标记解析、线程 resolve 规则（spec 07）
├── mention.rs         # @hoverstare 命令路由（spec 09）
└── orchestrator.rs    # review 流程编排（fail-open 区间划分）

crates/bugbot/         # 别名 crate：re-export + 同入口二进制（同步发布用）
```

## 4. 硬性约定（不可违反）

1. **spec-first**：`specs/` 是单一事实来源。开发先写/改 spec，实现必须遵守；
   发现 spec 不合理**先改 spec 再写代码**，禁止代码直接偏离。
2. **rig 隔离**：rig 类型只允许出现在 `src/agent/rig_backend.rs`。
3. **工具机器层只读**：新工具必须过路径沙箱评审；禁止执行 checkout 下来的代码
   （`git show` 固定格式是唯一进程调用例外）。
4. **fail-open 契约**（spec 01）：分析区失败 exit 0；配置错误、发布双失败 exit 1。
   新增失败路径先想清楚落在哪个区间。
5. **模型输出不可信**：先 schema 校验再归一化；行号必须过锚定降级链。
6. **diff/代码是 prompt injection 面**：系统提示里的不可信声明不可删改。
7. **机密**：一律 `SecretString`，禁止进日志；**任何密钥/令牌绝不提交进仓库
   或出现在 PR/issue/CI 日志中**（有 `.env~` 备份文件泄漏被拦截的前科）。
8. **发布禁令**：**没有用户的主动要求，严禁任何形式的发布**——包括但不限于：
   打/删/移动 tag、创建 GitHub Release、`cargo publish`、Marketplace 上架、
   向任意 registry 推包。实现完成 ≠ 发布授权；发布前必须停下来等用户明确指令。
9. **压缩契约**（spec 13）：system prompt 永不压缩；摘要只能替换对话前缀；**本轮任务提示
   必须钉住**（压缩不许丢掉正在处理的 diff/issue）；切点必须工具配对安全；摘要失败一律
   回落确定性 digest（压缩本身不允许失败）；溢出恢复先落粗摘要再 dump 再精确摘要，
   同一请求只重试一次；摘要 run 用独立预算；**确定性工作台账**（实际读过/改过的文件、
   搜索式、调用次数）随每次压缩累积并追加在摘要后，模型散文不得代替它。
10. **重试预算隔离**：agent 循环的任何重试都必须用全新的预算/状态对象
   （共享计数器会饿死后续重试——issue #9 两轮零改动的根因）。模型空输出、
   畸形响应是常态不是异常，循环必须容忍并重试（当前为 3 次尝试）。
11. **workflow 表达式卫生**：`${{ }}` 表达式内禁止 `#` 注释（会成为表达式
    的一部分导致解析失败）；并发组在 **run 级先于 job if 生效**——bot 自己
    产生的评论/review 事件必须给 noop 组名或明确豁免，否则会取消正在跑的
    run（三类变体都踩过：普通评论、bot 评论、bot review）。
12. **令牌职责分离**：身份（评论/API）永远走 App token；写操作（push/merge/
    删分支）走 PAT 类令牌或升了 contents:write 的 App token。`GITHUB_TOKEN`
    的 push 不触发 CI；squash merge 需要 contents:write。
13. **bot 能力边界**：bot 不能执行代码，fmt/clippy/编译错误只能靠 CI 暴露——
    给 bot 反馈 CI 失败时必须附上具体错误文本（rustc/fmt diff），否则它会
    盲改；bot 偏离 spec（如自加 env 覆盖）时用指令纠正，不替它重写实现。

## 5. 构建 / 测试 / 发布

```bash
cargo build --workspace
cargo test --workspace                          # 217 项（单元 + httpmock 合约）
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings
```

发布（四渠道）：

| 渠道 | 方式 |
|---|---|
| GitHub Release | 打 tag `v*` → release.yml 自动构建 musl 产物 + sha256 + 大版本浮动 tag |
| crates.io（主） | `cargo publish -p hoverstare` |
| crates.io（别名） | `cargo publish -p bugbot`（在主包索引可见后再发，版本跟随） |
| Marketplace | Release 编辑页手动勾选（无 API），元数据在根目录 action.yml |
| GitHub App | HoverStare App（App ID 4331106，Public、无 webhook），action 传 app_id/app_private_key 后评论以 hoverstare[bot] 发布，且不受 resolveReviewThread 平台限制 |
| serve 模式 | `hoverstare serve`（spec 10）：可选自部署 webhook 服务，用户装 App 零配置即得 hoverstare[bot] 审查；Dockerfile + docs/deploy.md |

## 6. 关键决策记录（为什么这么做）

| 决策 | 理由 |
|---|---|
| Rust + 静态 musl 二进制 | Action 分发零依赖、冷启动快 |
| rig-core 而非自研循环 | 快速上线；`AgentBackend` trait 保留切换自研的可能 |
| 只读工具集 + 定点查证纪律 | 纯 diff 审查可见度低是误报/漏报主因；工具机器层强制只读防注入 |
| 多 pass 投票 + verifier | 单 pass 误报高；≥2 票入选、单票复核（"驳回需证据，存疑从留"） |
| 指纹=路径+行内容+标题哈希 | 行号漂移免疫，跨 commit 认出同一个问题 |
| fail-open | 辅助工具不该弄红别人的 CI |
| 状态全存 GitHub 侧（评论标记+meta 注释） | bot 无持久化、天然无状态、水平扩展零成本 |
| 双 crates（hoverstare 主 + bugbot 别名） | 改名后老包不废弃、自然导流，两包永远行为一致 |

## 7. 运维经验（踩过的坑，别再踩）

1. **reqwest `.header()` 是追加不是覆盖**：Accept 双写会让 GitHub 返回 JSON 而非
   diff。自定义 Accept 走 `request_with_accept`。
2. **kimi-for-coding 只接受 temperature=1**：自定义温度直接 400。用
   `set_temperature = false` 配置项不传温度字段。
3. **musl 交叉编译需要 `musl-tools`**（aws-lc-sys 依赖 x86_64-linux-musl-gcc），
   ubuntu runner 不自带，release/CI workflow 里必须 apt 安装。
4. **默认 GITHUB_TOKEN 调不了 `resolveReviewThread`**（GitHub 平台限制，
   "Resource not accessible by integration"）→ 自动降级为线程内回复标记修复；
   完整 resolve 可用 App token 或 classic PAT（`GH_PAT`）。**GH_PAT 只干两件事**：
   resolve fallback 和开发模式 push——历史上它曾全局优先导致 bot 用人类身份
   发言，现已职责分离（见硬性约定 #11）。
5. **GraphQL 错误是 HTTP 200 + errors 字段**，别只看状态码。
6. **模型会空输出**（实测 2.5 分钟返回空）：空输出跳过 reformat 直接全量重试。
7. **中文标题聚类**：CJK 无空格分词，用单字+二字组 n-gram 算 Jaccard。
8. **httpmock 的 `mock_async(...)` 要 `.await` 才是 Mock**；429/5xx 重试用
   `with_retry_backoff(1ms)` 加速测试。
9. **create-github-app-token 会替换后续步骤的 `github.token` 上下文**：
   App token 无 cache 写权限，action 的 cache 步骤必须显式固定
   `GITHUB_TOKEN: ${{ github.token }}`。
10. **concurrency cancel-in-progress 在 run 级别先于 job `if` 生效**：不含命令的
    评论事件要进独立 noop 组名，否则机器人评论会取消正在跑的审查 run。
    同类的三个变体都踩过：bot 自己的含命令评论、bot 发的 review
    （pull_request_review 事件）、dev 轮与审查 run 同组互杀——分组设计必须
    把"bot 自己产生的事件"和"dev/审查两类工作"都考虑进去。
11. **重试必须换全新预算**：agent 循环重试共享 ToolShared 计数器会饿死后续
    重试（issue #9 两轮零改动的根因）——每次 attempt 新建预算对象。
12. **模型空输出和畸形响应是常态**（Kimi 偶发空文本、ApiResponse 反序列化
    失败）：develop 循环必须多次尝试（3 次）且后续 attempt 加催促提示，
    一次失败绝不直接判死刑。
13. **bash 路由别被 pipefail 秒杀**：提取关键词的 grep 无匹配会 exit 1，
    `set -euo pipefail` 下整个 step 静默死亡；且 `@hoverstare 中文指令`
    不含 [A-Za-z] 词，路由逻辑要按"空/review|explain|help → mention，
    其余 → develop"判断而不是匹配英文词。
14. **`${{ }}` 表达式里不能写 `#` 注释**（会并进表达式串，workflow 解析失败）。
15. **squash merge 需要 contents:write**（不是 pull-requests:write）：App 只读
    时 `@hoverstare merge` 403——写操作全部走 PAT 类令牌，身份仍归 App。
16. **`.git/` 不给模型读**：分支/提交/推送状态是 harness 的事，模型既不需要也不允许翻 `.git/`
    （沙箱层拒绝，不靠提示词）。dogfood 实测：一旦任务描述里提到 git 状态，模型会拿整个
    工具预算去读 `.git/refs/...` 而一个文件都不改。
17. **模型文本发布前必须过 `sanitize::model_text`**：把模型文本写进 PR body / 评论前要剥掉
    工具标记（`<read_file>…`、`<tool_call>…` 等），否则失败的工具调用会成为人类读的内容，
    并作为下一轮的上下文被继承。循环层的拒绝（下一节）是主防线，发布层的清洗是兜底。
18. **模型写工具调用有多种方言**："<｜｜DSML｜｜> 这类提供方私有标记用的是**全角竖线**，
    纯 ASCII 的识别与清洗会整段落空（dogfood 实测：报告与 PR 正文连续泄漏）。识别/清洗必须
    按真实字节来：先剥方言标记（`sanitize::model_text` / `tools::looks_like_tool_markup`），
    再处理普通 XML 形式；测试里要用 `\u{ff5c}` 这类转义构造，别靠手写。
19. **"用文字写工具调用"不是答案**：预算用尽后不再给工具，模型可能把工具调用写成
    正文标记（`<read_file>…`）——循环必须识别（`tools::looks_like_tool_markup`）并
    要求用散文作答，连续两次则明确失败；否则会把"什么都没做"的一轮当成成功上报。
20. **dogfood 不好用就先修 dogfood**：用 dogfood 推进任务时，凡遇到静默失败（没有 CI/没有
    评论却没有产出）、空转、指令被吃掉、输出污染（工具标记/实验痕迹进正文或提交）、预算导致
    的必然失败，或"每轮都要人重做同一件机械动作"，**立刻停下推进，先按 spec-first 修产品**，
    带确定性测试与真实证据（run/评论链接、原始日志），记录到 §7 后**再回来推进**。判据是
    "人类的真实使用会不会变差"，不是"是否违反某条既有实现"。详见 `.agents/rules/07-dogfood-loop.md`。
21. **"存在"不等于"可达"**：宣布局部特性完成前，必须确认每个新能力都有**生产调用点**
    （`rg` 查调用，排除模块自身与测试）。dogfood 实测：队列状态机+渲染+gate 全都实现且有单测，
    但 `enqueue`/`set_state` 在生产路径零调用点，队列永远是空的——能力不可达等于没做。
22. **前缀缓存是成本底线**：每次调用重发整段对话，只有公共前缀能命中 provider 缓存。
    循环里后续调用必须是前一次的**追加**（不得重建/重排历史；压缩是唯一允许的前缀替换）。
    `Usage.cached_input_tokens` 会打日志（`run used ... (cached, X%)`）；长 run 命中率长期为 0
    就是前缀被改写了。改循环时不要破坏这条，测试里有 `each_call_extends_the_previous_one_*` 钉着。
23. **思考模型的输出额度要够**：`max_output_tokens`（默认 window/16，下限 4096 上限 65536）
    是**每次调用**的上限，**推理 token 也计入**。额度被推理吃光时 provider 返回空正文，
    日志形如 `empty model reply (n/3, reasoning=true)`，看起来像模型拒答、实际是额度太小；
    循环会重试并最终报错，见到这个错误先调大额度而不是怀疑模型。写死小值（如 8192）
    会在长任务上稳定复现该故障。
24. **轮次开始先与 base 同步**：分支落后于 base 会让 PR 冲突，而**冲突态下 GitHub 不跑任何
    `pull_request` check**（没有红也没有绿），round 会在没有 CI 的世界里盲开发。开发轮现在
    先 merge base、成功即推送；冲突则 abort 并回帖请人类解决（bot 不做 rebase）。这是
    `.agents/rules/07-dogfood-loop.md` 的第一个实战案例。
25. **改 workflow 必须过 actionlint**：`secrets` 之类上下文在步骤 `if` 里不可用，写错会让
    GitHub 判定该工作流**无效**——之后每次 push 只得到一个以文件名命名的失败 run，而 dogfood
    静默失效（踩过一次）。CI 里已有 `workflow-lint` 作业；本地改完先跑 `actionlint`。
26. **签名验证看 committer**：GitHub 按 **committer** 校验提交签名，而 **GitHub App 不能持有密钥**：
    实测一笔"用维护者密钥正确签名、但 committer 是 `hoverstare[bot]`"的提交，GitHub 判定
    `verified=false reason=unknown_key`。所以署名覆盖时 **committer 必须与 author 同为人类**，
    想拿到"已验证"徽章还要求签名密钥挂在该人账号上（CI 用 `HOVERSTARE_GPG_PRIVATE_KEY`）。
27. **提交必须签名**：开发轮的提交要签名（`git log --format=%G?` 不能是 `N`）。CI 侧靠
    `HOVERSTARE_GPG_PRIVATE_KEY`（+可选 `HOVERSTARE_GPG_PASSPHRASE`）导入密钥并置
    `commit.gpgsign=true`，导入后有空提交自检兜底；本地 `develop --task` 跟随本机 git 配置。
    GitHub 的 squash 合并提交由平台自己签（密钥 `B5690EEEBB952194`），不需要我们处理。
28. **dogfood 可以钉版本自救**：master 上的**代码**坏掉时，dogfood 会连自己也跑不起来
    （构建的就是坏代码）。自救入口有两个：`workflow_dispatch`（填 `version` + `pr`，维护者
    可指定任意 ref）或在 PR body / 评论里写 `hoverstare-pin: <ref>`（只接受 `master` 与
    release tag，防止外部 PR 指向自己控制的代码）。pin 只换二进制来源，工作区仍是被处理的
    代码；pin 构建在独立 worktree，冷编译约 3 分钟。注意 workflow 文件本身由 App 推不动，
    harness 改动只能由人提交。
29. **bot 写的代码不过 fmt**：bot 不能执行代码，每轮都可能引入 rustfmt 偏差，
    不要让它逐条手改 18 处格式——人跑 `cargo fmt` 提一个 style commit 才是
    设计内的协作方式（人类可通过 commit 调整分支）。
30. **自驱动队列的接线与验收**：人类 `@hoverstare <指令>` 是一条任务入队，经历
    Running → Done/Failed 收尾。一轮结束时，只有"落地成功 + 队列还有活 + 未到轮次
    上限"才自触发下一条（以 `@hoverstare continue` 评论启动），且**自触发只从队列取
    任务、不带自由指令**；队列空（或已排空）**绝不**自触发，链在无人处静默终止。
    `@hoverstare queue` 打印计数 + 清单，running 条目带上**来源评论 id 与首行摘要**，
    每轮报告也带 `▶︎ 本轮执行 #<id> <摘要>` 一行（不再只有计数）。**PR 上肉眼验收**：
    把两条指令**分别**发成两条评论，观察一次只跑一条、每轮报告点名本轮执行的那一条、
    下一条以 `@hoverstare continue` 自触发带出——若一次跑两条或漏跑，先查并发组
    （§7 #10）与 claim/gate（`devqueue::precheck` / `plan_round`）。
31. **队列运维：入队去重、失败即停、自触发取 pending**：人类在 PR 上的每条
    `@hoverstare <指令>` 都**入队**并按**来源评论 id 去重**（同一条评论重放不
    重复入队）；本轮执行的条目开局置 `running`、结束按结果置 `done` / `failed`，
    **失败即停、不自动重试**。**自触发轮的任务取队列的下一个 pending 项**（不是
    自由指令），队列为空则不自触发；自动链单 PR 上限 10 轮，**人类指令不受此限**。
    **PR 上肉眼验收**：连发两条指令，看轮次报告里的队列摘要与下一条被
    `@hoverstare continue` 自触发带出。两个老坑会伪装成"队列没工作"：分支与 base
    冲突会**静默掐掉 CI**（开发轮已先合并 base，见 #24），以及 `.git/` 不给模型读
    （见 #16）——遇到"什么都没发生"先对照这两条。
32. **流程级 pin 与提交身份约定**：两条 dogfood 欠账，代码已上线，文档见
    `specs/08-action-packaging.md` 的 dogfood/pin 小节。用途：**一条流程（issue → go → PR）
    从头到尾钉在同一个 revision 上**（`go` 时把当时的默认分支 revision 写成
    `<!-- hoverstare-pin: <sha> -->` 存进 PR body，后续每轮都构建它，同一版本还命中按 sha
    的缓存 `pin-<commit>`），以及**提交归到人头上、bot 只做 co-author**
    （`commit_identity = "coauthor"` 是默认，`commit_author` 覆盖**优先于"谁触发的"**，
    所以自触发轮与本地 `--task` 也按覆盖署名；`bot` 仍是纯 bot 身份，覆盖不生效）。
    **PR 上肉眼验收**：`gh pr view <n> --json body` 看 PR body 里的 pin 标记，run 日志看
    `building hoverstare from pinned ref …`（同段还有 `pin scan: …`，实现见
    `.github/workflows/hoverstare.yml`）；**pin 是否生效不用翻 Actions 日志，直接看轮次报告
    评论里的来源行**（`本轮构建自 <短 sha>（来源：流程 pin）`），短 sha 与 PR body 标记一致
    即本轮确实构建于该版本；`git log --format='%an <%ae>%n%b'` 看作者与
    `Co-authored-by: hoverstare[bot]` 尾注。

## 7.5 Dogfood 验证手册（开发模式端到端怎么测）

测开发模式不要改完就跑 Actions 猜结果——分层验证：

1. **本地闭环（最快）**：`hoverstare develop --task "..." [--dry-run]` 在临时
   仓库里验证写工具+提交；`--repo X --issue N [--go] / --pr N [--merge]
   [--instruction "..."]` 本地驱动真实 issue/PR（用 `GH_PAT=$(gh auth token)`
   当写令牌，评论会显示为你的账号——仅测试期）。
2. **Actions 全链路**：issue 里 `@hoverstare`（讨论）→ 评论 `go`（开 PR）→
   PR 评论指令（开发轮）→ 等 CI → `@hoverstare merge`。观测点：
   `gh run list --workflow hoverstare.yml`、issue/PR 评论里的 hoverstare-dev
   隐藏标记（m=plan/impl, r=轮次）、分支 commit 作者应为 hoverstare[bot]。
3. **常见卡点对照**：
   - 黄条 **1 workflow awaiting approval** / run `conclusion=action_required`：
     GitHub 的 `pull_request` maintainer 闸门。**merge 过 `hoverstare[bot]` 的
     PR 并不能消掉它**：那只信任 App 身份。开 PR 那一次 CI 的 triggering actor
     是 `hoverstare[bot]`（已 merge 过则不拦）；workflow 里再 push 的后续
     commit 触发 actor 常为另一个账号 `github-actions[bot]`（从未作为作者被
     merge），默认「首次贡献者须批准」会 **每 push 都停一次**。
     对照：只有一颗 commit 的 bot PR 不会碰到第二次；有 `GH_PAT` 时 push 是
     协作者，continue 轮也不拦（PR 作者甚至会显示成协作者）。
     检测：`gh run list --json conclusion --jq '.[]|select(.conclusion=="action_required")'`。
     解开：PR 上 **Approve workflows to run**，或
     `gh api -X POST repos/<owner>/<repo>/actions/runs/<id>/approve`。
     预防：写操作走协作者 PAT（`GH_PAT`），评论/开 PR 仍用 App。不要指望「都用
     App」来统一 CI actor——没配 `GH_PAT` 时 push 已经是 App token，continue
     轮仍记成 `github-actions[bot]`。fork-PR「只拦 GitHub 新账号」对这种同仓库
     Actions push **实测无效**。不要用 `pull_request_target` 自动批准。
   - push/merge 403 → 写令牌缺 contents:write；
   - 推 `.github/workflows/*` 被拒（`GitHub App` + `workflows` permission）→
     App 不能改 workflow 文件。人在网页编辑器提交到 PR 分支，或给 App 开
     `workflows: write`。即便配了 `GH_PAT`，`actions/checkout` 默认
     `persist-credentials` 会把 `http.extraheader` 设成 Actions 的 App
     token，git 仍按 App 推，PAT 用不上；push 前清 extraheader，或
     `persist-credentials: false`（须已合入 **默认分支**，因为
     `issue_comment` 跑的是 master 上的 workflow）。
   - develop 红了但 PR 上没有 bot 评论 → 多半是 3×600s timeout 或 push
     失败，错误只在 Actions 日志。网页上要自己打开失败的 HoverStare
     (dogfood) run。
   - "no changes" → 先看 warn 日志里的 agent 摘要和 budget_exhausted；
   - run 显示 cancelled → 查并发组是否又被 bot 自己的事件顶掉（手册 #10）。
     连发两条 `@hoverstare` 会取消上一轮。
4. **测试期令牌纪律**：临时 `GH_PAT` secret 用完即删；验证 App 权限用
   JWT→installation token 现场铸（私钥不入库），绝不把令牌值写进任何日志。

## 7.6 网页闭环复盘（issue #13 / PR #14 dogfood）

用户文档与示范（用法 + 现场缺口）：[`docs/web-ide.md`](docs/web-ide.md) /
[`docs/web-ide.zh-CN.md`](docs/web-ide.zh-CN.md)。下面是操作对照摘要。

产品把 GitHub 网页当 IDE（spec 11）：issue/PR 评论 = 对话，Checks = 验证，
bot 不主动去扫 CI。这次把「只坐在 github.com、不靠本机 CLI」能走多远测清楚了。

**网页上本来就做得到、不必 CLI 的：**

- 黄条 **Approve workflows to run**（PR Checks）。
- CI 红了：打开失败 job → 复制 rustfmt/编译器 diff → 贴回 PR 评论给
  `@hoverstare`（这就是设计内的人机分工；bot **没有** 读 Actions 日志的
  工具，让它「自己去看 Checks」会空转到 timeout）。
- 改 workflow 文件：GitHub 文件编辑器 commit 到 PR 分支（App 推不动 yml）。
- 配 `GH_PAT`、改 Actions 审批策略：仓库 Settings。
- 看 dogfood 红叉：Actions 页，不是 PR 对话。

**网页上做得差、容易以为「停了」的：**

- 每轮 Actions 推上来的 `pull_request` 都可能黄条（actor 是
  `github-actions[bot]`，不是 `hoverstare[bot]`）。点批准能过，但不知道的人
  会停在 Checks。fork-PR「只拦 GitHub 新账号」**挡不住**这种同仓库 push。
- develop 超时/push 失败 **不在 PR 上留言**，对话链断了。
- 连发指令会 cancel 正在跑的一轮。
- `issue_comment` 用的是 **默认分支** 上的 workflow。PR 里改的 job `if`
  要合进 master 之后，网页上的无 mention 线程回复才会按新规则跑。

**当前单纯网页（只评论、不进 Settings/编辑器）做不到的：**

| 缺口 | 原因 | 改进（值不值得） |
|---|---|---|
| 让 bot「自己打开失败的 check」 | 工具集只有仓库读写，没有 check run / job log | **值得**：只读工具，拉本 PR head 的失败 check 摘要（截断）。人仍可以说「CI 红了」，不必预读日志。不是自动修 CI，只是把 Checks 页读进对话。`list_check_runs` 已给 merge 用过。 |
| fmt 红了只能手改或贴 diff | spec 11 不执行代码，没有 `cargo fmt` | **维持设计**。人贴 Checks 里的 rustfmt diff 即可。不要让模型猜 18 处格式。可选：独立 CI job 在 bot 之后自动 fmt（产品上要另开讨论，不是开发模式该偷做）。 |
| bot 改不了 `.github/workflows/*` | GitHub：App 无 `workflows` 权限则拒推 | **两条都要**：文档写明「改 workflow 用网页编辑器」；代码上 push 前清 `http.https://github.com/.extraheader`，让已配的 `GH_PAT` 真正用于 push。给 App 开 `workflows: write` 能让身份仍是 bot，权限面更大。 |
| PAT 配了仍报 GitHub App 拒推 workflow | checkout 的 extraheader 盖掉 remote URL 里的 PAT | **值得修**（上一条）。`persist-credentials: false` 已写在 PR #14 的 dogfood workflow，**合入 master 前对评论触发无效**。 |
| 超时三次后对话里没有 bot | `develop failed` 直接让 step 失败 | **值得**：失败也 `create_issue_comment`（timeout / push rejected / 3 attempts），网页才能接着下指令。 |
| 黄条没有人点就停 | 平台闸门 + Actions 推送 actor | **不要**用 `pull_request_target` 自动批准。网页点批准就够；要少点：`GH_PAT` 推送（须 extraheader 修复）或接受每轮点一次。推送成功后若 checks 为 `action_required`，**评论里提醒去点 Approve**（只读 checks API，App 能读）。 |
| 评论里的引号弄坏 `xargs` | dogfood workflow 用 xargs 抽 `@hoverstare` 行 | **值得**：抽词不要走 xargs 默认引号规则。 |

**结论：** 审查+开发的主路径（讨论 / go / 贴 CI / 再修 / merge）可以纯网页完成。这次多出来的 CLI 动作，大部分是「没在 PR 上留言、没清 git extraheader、没在网页编辑器改 yml」。真正要补进产品的是：**失败回评、可读失败 check 摘要、push 用 PAT 时不被 checkout 凭据覆盖**。不改「CI 红了不自动开修」这条产品边界。

## 8. 配置与秘钥管理

- LLM 凭据只走 env：`OPENAI_API_KEY`(+`OPENAI_BASE_URL`) 或 `ANTHROPIC_API_KEY`；
  模型名 `HOVERSTARE_MODEL` / toml `model`（OpenAI 兼容端点必配）。
- 思考模式（仅 OpenAI 兼容端点）：`HOVERSTARE_THINKING` / `HOVERSTARE_REASONING_EFFORT`
  （或 toml `thinking` / `reasoning_effort`），未配置则一个字段都不发；
  `HOVERSTARE_CONTEXT_TOKENS` 记录模型窗口并用来钳制 `max_diff_kb`。
  本仓库 dogfood 现接 DeepSeek：`https://api.deepseek.com` + `deepseek-flash`
  + thinking medium + 1M 上下文（见 `.github/hoverstare.toml` 与 Actions vars）。
- CI 里用户的 LLM key 放 GitHub Secrets（如 `HOVERSTARE_LLM_KEY`），
  **绝不写进 toml/workflow/日志**。
- 本地开发 key 放 `spikes/rig-kimi-probe/.env`（已 gitignore，模式 `.env*` 全部忽略）。
- GHE：`GITHUB_API_URL` 覆盖 API 地址。

## 9. 规则文件

更细的专项规则在 `.agents/rules/`：

- `01-spec-first.md` — spec 优先的开发纪律
- `02-security.md` — 秘钥/不可信数据/只读强制/prompt injection
- `03-architecture.md` — 模块边界、rig 隔离、AgentBackend 契约
- `04-testing.md` — 测试约定与常用模式
- `05-release.md` — 四渠道发布流程与双 crates 同步
- `06-llm-providers.md` — 各 provider 的脾气与适配
- `07-dogfood-loop.md` — dogfood 驱动开发，以及"工具不好用就先修工具"的工作方法（必读）
