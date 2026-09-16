# Spec 11 — Agent 开发模式（issue 驱动 + PR 循环开发）

状态：Implemented（M11-M13，2026-07-19 全链路 Actions 实测通过：issue #4 → 讨论 → go → PR #5 → 开发轮 → merge）
目标版本：v0.1.0（从 0.0.8 起跳，独立于审查功能的 0.0.x 线）

## 0. 产品精神

**GitHub 仓库即开发环境。** 不是"出事了自动去处理"的自动化杂务工
（CI 红了读日志、来 review 自动回），而是把 issue 和 PR 当成一个 Web 版
AI 编程 IDE：issue = 任务文档，评论 = 结对编程的对话，PR = 编辑器与工作区，
每一轮 = 一次开发会话，merge = 交付。对话链是产品本体，所有机制为它服务。

**自有 agent 体系，非桥接。** HoverStare 自己就是完整的编程工具系统：
自己的 agent 循环（rig）、自己的工具集、自己的上下文管理、自己的审查引擎、
自己的 git 操作。不包装别家 CLI，不做 "bring your own agent"。

**根基不可丢。** PR 审查与缺陷发现是 HoverStare 的初心和根基功能：
review/mention 既有路径零改动，开发模式全部走新子命令与新事件分支，
审查用户不受任何影响。

## 1. 目标

让 HoverStare 从"审查 bot"扩展为"开发 agent"，两条主线：

1. **Issue 主线**：用户在 issue 里提需求/bug → AI 调查仓库、在评论里讨论、
   产出计划 → 用户批准 → AI 开发并开出 PR。
2. **PR 主线**：在 PR 上，用户通过评论/review 评论给 AI 下任务 → AI 在
   **PR 分支上**开发，完成后 commit 并**推送到本仓库该分支**，并评论汇报。
   支持 AI 自触发下一轮；支持 `@hoverstare merge` 合并。

## 2. 非目标（明确不做）

- **不做 fork PR 的任何处理**：PR 来源分支不在本仓库时，命令一律回复
  一行"仅支持本仓库分支的开发"并停止。不做兜底分支、不做补丁评论。
- 不做 label 状态机、不做看板、不做多任务编排。
- 不执行代码（不跑测试/构建）：开发与验证分离，CI 负责验证。
- 不改变现有审查行为：review/mention 既有路径零改动（产品精神 §0），
  新功能全部走新子命令与新事件分支。审查模式**永远不挂写工具**。

## 3. 总体模型

### 3.1 无状态轮次

每次 Action run 是一轮。轮与轮之间不保留内存状态；上下文来源：

- 触发事件（issue/comment/PR 元数据）
- 该 issue/PR 的评论串（首帖 + 最近 N=30 条评论，超长截断）
- 仓库工作区（按需 read/grep/glob）
- bot 自己评论里的隐藏标记 `<!-- hoverstare-dev:{json} -->`：记录模式
  （plan/implement）、已进行轮次、关联 issue 号

### 3.2 触发与信任

- 事件：`issues.opened`、`issue_comment.created`（issue 与 PR 通用）、
  `pull_request_review_comment.created`、`pull_request_review.submitted`。
- 命令一律以 `@hoverstare` 开头；**仅响应 collaborator 及以上**（复用
  mention.rs 的校验），其余评论忽略。
- issue/PR 文本（标题、正文、评论）一律视为不可信输入，只作为任务
  上下文，不得改变权限边界。

### 3.3 身份与推送

**两类令牌职责分离**：身份操作（评论、开 PR、查 checks）用 App installation
token（显示 hoverstare[bot]）；写操作（git push、merge、删分支，需要
contents: write）优先用 PAT 类令牌（`HOVERSTARE_DEV_TOKEN` > `GH_PAT`），
PAT 推送可触发 CI。App 升 contents:write 后写操作也可回落到 App token。
GITHUB_TOKEN 的 push 不触发 CI，会导致 checks 不跑、无法合并。
- commit 身份（`commit_identity` 配置）：**committer 始终是 `hoverstare[bot]`**
  （git 由 bot 执行）；**author 由触发者决定**——`bot` 模式作者即 bot，`author`
  模式作者为下达指令的人类，`coauthor` 模式在 author 之上追加
  `Co-authored-by: hoverstare[bot]` trailer。无触发者、或触发者即 bot（自触发）
  时一律退化为 bot 身份，不臆造 `<login>@users.noreply.github.com` 作者。
  人类作者默认取 `<login>@users.noreply.github.com`，可用 `commit_author` 的
  `Name <email>` 显式覆盖。相应环境变量名 `HOVERSTARE_COMMIT_IDENTITY` /
  `HOVERSTARE_COMMIT_AUTHOR`。优先级：env > toml > 默认 `coauthor`。
- commit message：Conventional Commits，如
  `feat: <task summary> (hoverstare-dev #123)`。

## 4. 写工具（agent/tools.rs 扩展）

在现有只读工具集上新增两个，走同一路径沙箱（拒绝绝对路径、`..`、
符号链接逃逸；仅允许工作区内相对路径）：

| 工具 | 参数 | 语义 |
|---|---|---|
| `edit_file` | `path`, `old_string`, `new_string` | 精确替换；`old_string` 在文件中必须恰好出现一次，否则报错（不猜、不模糊匹配） |
| `write_file` | `path`, `content` | 整文件写入（新建或覆盖），自动创建父目录 |

- 写入后返回简短确认（路径 + 字节数），不回显全文（省 token）。
- Budget 复用：`max_tool_calls` 对读+写统一计数；默认 implement 轮
  budget=40 次调用、timeout=10min。
- **轮次开始先与 base 同步**：开发前把 base 分支 merge 进 PR 分支，成功且产生合并提交就立刻推送。
  原因：分支落后于 base 会让 PR 变成冲突态，而 GitHub 在冲突态**不运行任何 `pull_request` check**——
  既没有红也没有绿，round 会在"没有 CI 的世界"里盲开发。冲突时中止合并（`git merge --abort`，
  工作区留给人）并在 PR 上明确报告，等人类解决；bot 不做 rebase。合并提交以
  `hoverstare[bot]` 作为 committer（作者随之取 bot），与开发轮「author=触发者」
  的契约不冲突（见 §3.3），身份仍只有一处权威。
- **超时不重试**：一轮把整个预算用满仍没结束（`AgentError::Timeout`）时不换预算重跑——
  同样的分钟数会得到同样的结果，还会一直占着并发组。直接失败并说明"拆分任务或提高预算"。
  空输出/畸形响应仍然重试（3 次尝试，spec 04）。

## 5. Issue 主线

命令（issue 评论区）：

| 命令 | 行为 |
|---|---|
| `@hoverstare`（任意文本，或 issue 首帖 @） | **讨论/计划轮**：带仓库上下文调查，以评论输出分析+计划（修改哪些文件、怎么做、验收方式）。之后的普通评论（无需 @）视为继续讨论，bot 逐轮回复 |
| `@hoverstare go` | **实现轮**：以最近计划为准，从默认分支拉 `hoverstare/issue-<N>-<slug>` 分支 → 开发 → commit/push → 开 PR（body 含 `Closes #N`）→ 在 issue 评论 PR 链接 |

- 状态记在隐藏标记里：`planning`（讨论中）→ `implementing`（已开 PR，
  后续开发转到 PR 评论区进行）。
- 已在 `implementing` 的 issue 上再讨论，bot 回复引导去 PR。

## 6. PR 主线

命令（PR 评论区 / review 评论 / review body）：

| 命令 | 行为 |
|---|---|
| `@hoverstare <任意指令>` | **开发轮**：checkout PR head 分支 → 按指令开发（读+写工具）→ commit（conventional）→ push 到该分支 → 评论汇报（改了什么、为什么） |
| `@hoverstare merge` | 检查：collaborator + checks 全绿 + 无冲突 → squash 合并 → **删除 PR 源分支**（必是同仓分支，§2 已保证）→ 评论确认；不满足则回复原因不合并。删分支失败不翻转合并结果，仅在评论中警告 |

- 开发轮上下文包含：触发评论所在的 review 线程（若是行内评论）、
  PR diff 摘要、最近评论。
- **自触发**：一轮 budget 耗尽但任务未完时，bot 通过 App token 自己发
  `@hoverstare continue` 评论启动下一轮；隐藏标记累计轮次，
  **自动链单 PR 上限 10 轮**（到顶明确说明"自动链停止，继续请人工下令"），
  但**人类指令不受该上限约束**——熔断是防失控的链，不是锁人的门。自触发评论是唯一豁免
  collaborator 校验的 bot 发言（判据：作者为 hoverstare[bot] 且正文恰好
  是该命令）；workflow 的 `if` 同步豁免它，其余 bot 评论一律不触发，
  防止自己的计划/汇报评论递归触发并打断在跑的 run。
- 人类在分支上的 commit 不被覆盖：每轮开始先 `git pull --rebase`，
  冲突则停止并评论说明。

### 队列（自驱动）

队列把 PR 上的多条指令排成串行、可恢复的工作流。它是纯逻辑（`devqueue`），
所有状态都在 GitHub 侧，轮与轮之间无内存状态（承 §3.1）。轮次报告除队列状态外
还携带构建来源行（`本轮构建自 <短 sha>（来源：流程 pin）`），来源词为人话
（流程 pin / 指令标记 / 事件版本 / 手动触发），无该环境变量时不显示。逐条契约如下
（每条附一个现存测试名作锚点）：

1. **载体**：队列状态以 append-only 隐藏标记 `<!-- hoverstare-queue:{json} -->`
   附在轮次报告评论里，latest-wins（读最近一条标记）；只存来源评论 id 与状态，
   **不复制指令正文**——正文按 id 回读评论串。
   锚点：`devqueue::tests::queue_roundtrips_and_latest_wins`。
2. **上限**：未完成条目上限 20 条、单条指令文本 2000 字符、历史保留 10 条后
   剪枝；超限**明确拒绝**（在 PR 上贴出拒绝原因），而不是静默丢弃。
   锚点：`devqueue::tests::enqueue_refuses_beyond_caps_and_is_idempotent`
   （剪枝：`history_is_pruned_so_the_live_cap_stays_free`）。
3. **出队规则**：`running` 优先 → 人类条目按来源 id 升序 → bot 条目；一轮只取
   一条；人类指令不因机器人自触发而被丢。
   锚点：`devqueue::tests::dequeue_is_running_then_human_then_fifo`。
4. **执行与迁移**：本轮执行的条目在开局置 `running`，结束按结果置 `done` /
   `failed`；失败不再自动重试（failure-stop）。
   锚点：`devqueue::tests::self_trigger_only_after_landed_round_with_work_left`。
5. **claim 守卫**：自触发评论携带"刚完成的轮次"；若最新标记轮次 ≥ 它声称的
   轮次，则本轮**静默退出**（被更新的 run 取代，不写任何东西）。
   锚点：`devqueue::tests::stale_claim_and_cap_are_prechecked`。
6. **artifact gate**：上一轮记录的 sha 必须是当前分支 head 的**祖先**，否则停止
   （分支被改写时不叠加工作）；仅约束自驱动轮，人类指令照常执行。
   锚点：`devqueue::tests::artifact_gate_requires_ok_and_ancestor`。
7. **合并门**：队列仍有 open 条目时 `@hoverstare merge` **拒绝**并原样贴出
   `@hoverstare queue` 的清单；`@hoverstare merge force` 放行并说明丢弃条数。
   锚点：`devqueue::tests::merge_gate_refuses_nonempty_queue_unless_forced`。
8. **`@hoverstare queue` 命令**：在 PR 上显示 pending / running / done / failed /
   dropped 的清单与计数（队列为空也明确说明）；在 issue 上无效。
   锚点：`devqueue::tests::checklist_shows_running_and_pending`。
9. **与既有机制的关系**：§6 自动链 10 轮上限只约束自触发（人类指令不受限）；
   自触发评论身份为 `hoverstare[bot]`，故身份退化为 bot（`commit_identity` 的
   author/coauthor 只在人类触发轮生效，见 §3.3）。

## 7. CLI 与事件扩展

- 新子命令 `hoverstare develop`：从 `GITHUB_EVENT_PATH` 解析
  issue/comment/review 事件，按 §5/§6 执行；`--dry-run` 本地演练
  （不 push、不开 PR，打印计划）。
- `hoverstare review` / `mention` 现有行为不变。
- action.yml：`issues` 等事件接入；默认不影响仅使用审查的用户
  （无 @hoverstare 开发命令时develop 立即成功退出）。

## 8. 开放问题（spec 评审时定）

1. ~~commit 作者邮箱用什么~~ **已实现**（关闭）：bot 用
   `hoverstare[bot]@users.noreply.github.com`，人类触发者用
   `<login>@users.noreply.github.com`，可用 `commit_author` 覆盖（§3.3）。
2. issue 首帖是否需要显式 @hoverstare 才启动，还是首帖即任务（倾向：
   首帖含 @ 才启动，避免误触发）。
3. ~~分支命名 slug 规则~~ **已实现**（关闭）：`src/devagent.rs` 的 `slug`
   取 issue 标题前 30 字符 slug 化，注释即 §8.3。

## 9. 里程碑

| M | 内容 | 验收 |
|---|---|---|
| M11 | 写工具 + git 模块 + `develop --dry-run` 本地闭环 | 本地对测试仓库把一条任务变成 commit；82+ 既有测试全绿 |
| M12 | Issue 主线（讨论/计划/go→开 PR） | hoverstare 仓库测试 issue：讨论两轮 → go → 自动开 PR |
| M13 | PR 主线（开发轮/自触发熔断/merge） | 测试 PR 上：review 评论下任务 → bot 推 commit；超 budget 自触发；@merge 合并 |

## 10. 风险与边界

- **提示注入**：issue/PR 文本可诱导 bot 写恶意代码并推送。缓解：仅
  collaborator 触发 + 不执行代码 + 人类 review 后才合并。同仓协作者
  本身有写权限，bot 不扩大攻击面。
- **推送冲突**：人类同时推了 commit → rebase 失败即停并评论，不强推。
- **成本**：每轮 budget 硬顶；单 PR 10 轮熔断；长评论串截断。
