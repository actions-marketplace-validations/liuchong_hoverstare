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
- commit 作者：`hoverstare[bot] <bot@hoverstare>`（待定，见 §8）。
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

## 7. CLI 与事件扩展

- 新子命令 `hoverstare develop`：从 `GITHUB_EVENT_PATH` 解析
  issue/comment/review 事件，按 §5/§6 执行；`--dry-run` 本地演练
  （不 push、不开 PR，打印计划）。
- `hoverstare review` / `mention` 现有行为不变。
- action.yml：`issues` 等事件接入；默认不影响仅使用审查的用户
  （无 @hoverstare 开发命令时develop 立即成功退出）。

## 8. 开放问题（spec 评审时定）

1. commit 作者邮箱用什么（App 没有公开邮箱；可用
   `hoverstare[bot]@users.noreply.github.com` 形式）。
2. issue 首帖是否需要显式 @hoverstare 才启动，还是首帖即任务（倾向：
   首帖含 @ 才启动，避免误触发）。
3. 分支命名 slug 规则（issue 标题前 30 字符 slug 化）。

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
