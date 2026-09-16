# 把 GitHub 网页当 IDE（开发模式）

HoverStare 的开发模式把 GitHub 网站当成工作区：issue 是任务文档，评论是结对
会话，PR 是工作区，每一轮 Action 是一次开发，merge 是交付。它**不会**自己盯着
CI 红了就开修（spec 11）。人始终在对话里。

本页同时是 **用法说明** 和 **现场记录**：issue
[#13](https://github.com/liuchong/hoverstare/issues/13) /
PR [#14](https://github.com/liuchong/hoverstare/pull/14) 整条链路都在网页对话里
走完（模型 k3）。操作对照见 [`AGENTS.md`](../AGENTS.md) §7.5–7.6。

English: [`web-ide.md`](web-ide.md).

## 只开浏览器时的路径

1. 开 issue，写上 `@hoverstare`。它调查仓库并给出计划。
2. 在 issue 里回复直到计划可执行，然后 `@hoverstare go`。
3. 在 PR 上用 `@hoverstare …` 下指令。它以 `hoverstare[bot]` 提交并评论汇报。
   预算用尽会自己续轮（每个 PR 最多 10 轮）。
4. Checks 出现黄条 **1 workflow awaiting approval** 时，点 **Approve workflows
   to run**。这是 GitHub 对 `pull_request` 的首次贡献者闸门：Actions 里 push
   之后，触发 actor 经常是 `github-actions[bot]`，不是缺 LLM 密钥。已经 merge
   过的 `hoverstare[bot]` PR 只信任 **开 PR 那一次** 的 App 身份。
5. **check** 红了：打开失败的 job，把编译器 / rustfmt diff 复制到 PR 评论，
   以 `@hoverstare` 开头。Agent **没有**读 Actions 日志的工具；让它「自己去看
   CI」会空转到 10 分钟超时，而且常常 **不在 PR 上留评论**。
6. Checks 全绿且无冲突：`@hoverstare merge`（squash 并删除源分支）。

Finding 线程讨论（issue #13）：在 HoverStare 的行内发现下直接回复，**不必**
`@mention`。Bot 回在同一条 review 线程里。`@hoverstare explain` 仍是显式写法。

## 网页上已经有的入口

| 需求 | 在 github.com 哪里 |
|---|---|
| 批准等待中的 workflow | PR → Checks → **Approve workflows to run** |
| 把 CI 失败送进下一轮 | Checks → 失败 job 日志 → 复制 → PR 评论 |
| 改 workflow 文件 | 文件编辑器 → commit 到 PR 分支（GitHub App **没有** `workflows` 权限就不能推 `.github/workflows/*`） |
| `GH_PAT` / Actions 审批策略 | 仓库 **Settings** |
| develop run 红了但对话里没 bot | **Actions** 页，不是 PR 会话 |

一次只发一条 `@hoverstare`。第二条会取消正在跑的一轮（`cancel-in-progress`）。

`issue_comment` / `pull_request_review_comment` 用的是 **默认分支** 上的
workflow。PR 里改的 job `if` 只有合进默认分支之后，评论触发才会按新规则跑。

## 缺口（#13 dogfood）和能不能改

下面这些挡住了「只评论、不进 Settings/编辑器」的闭环。它们是开发模式体验的
后续工作，**不是**把产品改成自动修 CI。

| 缺口 | 原因 | 改不改 |
|---|---|---|
| 让 bot「自己打开失败的 check」 | 工具只有仓库读写 | **要**：只读、截断的本 PR head 失败 check 摘要。merge 已经在用 `list_check_runs`。人仍然说「CI 红了」。 |
| rustfmt / 测试 | spec 11 不执行构建 | **维持**。贴 Checks 里的 diff。不要让模型猜格式。 |
| bot 改不了 workflow YAML | App 无 `workflows` 则拒推 | 文档写明用网页编辑器；`git push` 前清掉 `http.https://github.com/.extraheader`，让 `GH_PAT` 真正用于 push。给 App 开 `workflows: write` 能保持 bot 作者，权限面更大。 |
| 配了 `GH_PAT` 仍报 GitHub App 拒推 | checkout 的 extraheader 盖过 PAT remote | **要修**（同上）。PR #14 的 dogfood workflow 已加 `persist-credentials: false`；评论触发在合进 **默认分支** 之前仍用旧文件。 |
| 超时 / push 失败不在 PR 留言 | Action step 直接失败 | **要**：timeout、push rejected、三次失败都 `create_issue_comment`。 |
| 每次 continue 都可能黄条 | 触发 actor 是 `github-actions[bot]` | 网页点批准即可。**不要**用 `pull_request_target` 自动批准。想少点：PAT 推送（须先修 extraheader），或 checks 为 `action_required` 时评论提醒。 |
| 评论里的引号弄坏 `xargs` | dogfood workflow 用 xargs 抽 `@hoverstare` | **要**：抽词不要走 xargs 默认引号规则。 |

**不要做：** CI 一红就自动开开发轮；用 `pull_request_target` 自动批准别人的 run。

## 提交身份与签名

开发模式的提交由 Action 产生，所以"提交挂谁的名字"（`commit_identity`、`commit_author`）和
"是否签名"都由你配置。

### 身份

| `commit_identity` | 作者 | 提交信息尾注 |
|---|---|---|
| `author` | 触发本轮的人 | — |
| `bot` | `hoverstare[bot]` | — |
| `coauthor`（默认） | 触发本轮的人 | `Co-authored-by: hoverstare[bot] …` |

- `commit_author = "Name <email>"` 覆盖作者；该覆盖**同样作用于自触发轮与本地
  `hoverstare develop --task`**（这两种情况没有人类触发者）。
- 覆盖作者时，**committer 也用同一身份**（不是 bot）。原因：GitHub 按 **committer** 校验签名，
  而 App 账号无法持有签名密钥——committer 是 bot 时，即使签名正确也显示未验证（`unknown_key`）。
  想要绿色"已验证"徽章，就把 `commit_author` 配成持有签名密钥的账号。
- 推送始终走写令牌（`GH_PAT`，或具备 `contents: write` 的 App token）；上面的设置只决定提交对象
  长什么样。

### 签名（GPG）

不配置下面的 secret 时行为不变（提交不签名）。

1. 用一把具备签名能力的密钥（`S`，仅签名用途即可）。CI 无法交互输入口令：要么用无口令密钥，
   要么同时提供口令（workflow 会把 gpg 包成 loopback 模式）。
2. 把**私钥**放进仓库 secret：
   ```bash
   gpg --armor --export-secret-keys <KEY_ID> \
     | gh secret set HOVERSTARE_GPG_PRIVATE_KEY
   # 仅当密钥带口令时：
   printf '%s' '<口令>' | gh secret set HOVERSTARE_GPG_PASSPHRASE
   ```
3. 把**公钥**加到提交作者的 GitHub 账号上（Settings → SSH and GPG keys → New GPG key）。
   没做这一步时，提交是"已签名但未验证"。
4. workflow 会导入密钥、设置 `user.signingkey` 与 `commit.gpgsign`，然后做一次**空提交自检**：
   如果那次提交没有签上，整个 run 直接失败，而不是静默产出未签名历史。

请自留一份可恢复的私钥副本（口令管理器，或用你自己的密钥加密后导出）：**仓库 secret 读不回来**。
要停用它：删掉 secret、从账号移除公钥、再删本地密钥。

### 如何验证

```bash
git log -1 --format='%G?'   # G/U = 本地能验签，N = 未签名
gh api repos/<owner>/<repo>/commits/<sha> --jq '.commit.verification'
#   {"verified":true, "reason":"valid"}       密钥在账号上
#   {"verified":false,"reason":"unknown_key"} committer 不拥有该密钥
#   {"verified":false,"reason":"unsigned"}    未配置签名密钥
```

## #13 / #14 示范里实际发生的事

- 先改 spec，再写代码：无 `@mention` 的 finding 线程讨论、线程内 `explain`、
  父评论 marker 门闩、serve 路由、dogfood `if`、六语 help。
- CI 红的只是 `cargo fmt --check`，直到人把 Checks 上的 diff 贴进评论。
- 让 bot 去拉 Actions 日志超时三次（各 600s），PR 上没有失败说明。
- 有两轮在 runner 里改好了 `.github/workflows/hoverstare.yml`，App push 被拒。
  协作者提交了该文件（含 `persist-credentials: false`）。
- fmt 和文档之后，`check` 与 dogfood review 全绿。对 **#13 功能** 来说这就是
  做完；表里其余项是开发模式平台债，不挡这期合并。
