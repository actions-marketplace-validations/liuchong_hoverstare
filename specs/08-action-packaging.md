# 08 — Action 打包与发布

## 目标

用户侧接入成本：一个 workflow 文件 + 一个 secret。维护者侧发布成本：打一个 tag。

## 用户接入（example workflow）

`.github/workflows/hoverstare.yml`：

```yaml
name: HoverStare
on:
  pull_request:
    types: [opened, reopened, synchronize]
  issue_comment:
    types: [created]
  pull_request_review_comment:   # @hoverstare explain 的线程回复场景（spec 09）
    types: [created]

permissions:
  contents: read
  pull-requests: write
  statuses: write

concurrency:
  # 不含 @hoverstare 的评论事件给独立组名，避免无意义的 run 取消正在跑的审查；
  # 无 mention 的 finding 线程回复（spec 09）也落在这个 noop 组：job 会运行，
  # 但绝不取消正在进行的 review——这是有意为之的隔离，不要改
  group: >-
    hoverstare-${{
      (github.event_name == 'issue_comment' || github.event_name == 'pull_request_review_comment')
      && !contains(github.event.comment.body, '@hoverstare')
      && format('noop-{0}', github.event.comment.id)
      || (github.event.pull_request.number || github.event.issue.number)
    }}
  cancel-in-progress: true

jobs:
  hoverstare:
    runs-on: ubuntu-latest
    # 最后一条：放行 finding 线程内的无 mention 人类回复（spec 09 线程讨论），
    # 父评论是否为 finding 由二进制内的 marker 检查把关（廉价、无模型调用）
    if: >-
      github.event_name == 'pull_request' ||
      contains(github.event.comment.body, '@hoverstare') ||
      (github.event_name == 'pull_request_review_comment' &&
        github.event.comment.in_reply_to_id &&
        github.event.comment.user.type != 'Bot')
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0        # show_base_file 需要 base 分支历史
      - uses: liuchong/hoverstare@v0.0.5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          ANTHROPIC_API_KEY: ${{ secrets.ANTHROPIC_API_KEY }}
```

## action.yml（composite）

inputs：

- `version`（默认 `v1`）；
- `app_id` / `app_private_key`（可选）：提供时先用 `actions/create-github-app-token`
  换 installation token 再运行——评论以 `hoverstare[bot]` 品牌身份发布
  （默认 `GITHUB_TOKEN` 只能以 `github-actions[bot]` 发布），且 App token 不受
  `resolveReviewThread` 平台限制（spec 07 的完整 resolve 路径，无需 GH_PAT）。
  注意：create-github-app-token 会替换后续步骤的 `github.token` 上下文，
  因此 cache 步骤显式固定 `GITHUB_TOKEN: ${{ github.token }}`（App token 无
  cache 写权限，否则 post-cache 报 warning——实测踩坑）。
- `gh_pat`（可选）：dev-mode 写令牌（classic PAT，`repo` scope）。`develop` 循环
  的 git push / merge 优先使用 `GH_PAT`：PAT 推送可触发 CI，而 `GITHUB_TOKEN`
  推送不触发。API 身份（评论、状态检查等）仍使用 App / `GITHUB_TOKEN`，
  不因 `gh_pat` 改变 bot 身份。

步骤：

1. 解析 runner OS/ARCH（v1 仅支持 linux-x64）；
2. 命中 `actions/cache`（key = `hoverstare-{version}-{os}-{arch}`）则跳过下载；
3. 否则从 GitHub Releases 下载 `hoverstare-{version}-{target}.tar.gz` 与 `.sha256`，
   校验后解压到 `$RUNNER_TEMP/hoverstare/bin`；
4. 事件分派：
   - `pull_request` → `hoverstare review`；
   - `issue_comment` / `pull_request_review_comment` / `issues` 事件 → 先提取
     `@hoverstare` 后的命令关键字；若匹配 `review|explain|help|empty` 则
     `hoverstare mention`，否则 `hoverstare develop`；
   - 无 mention 的 `pull_request_review_comment` 线程回复（命令关键字为空）同样
     路由到 `mention`——finding 线程讨论（spec 09），永不进入 `develop`；
   - 路由实现需容忍 CJK 文本下 `grep` 未匹配的情况，避免崩溃或错路由；
5. 透传 `GITHUB_TOKEN`、`ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`OPENAI_BASE_URL`；
   若提供 `gh_pat`，作为 `GH_PAT` 透传给 `develop` 子命令。

## 发布流水线（`.github/workflows/release.yml`，本仓库自身）

`push: tags: ["v*"]` 触发：

1. **安装 musl 交叉工具链**：`sudo apt-get install -y musl-tools`
   （rustls 依赖的 aws-lc-sys 需要 `x86_64-linux-musl-gcc`，ubuntu-latest 不自带——
   首个 release 实测踩坑，2026-07-18 修正）；
2. `cargo build --release --target x86_64-unknown-linux-musl`，`strip`；
3. 产物打包 + `sha256sum`（`.sha256` 只含哈希值，action 侧拼 `hash  filename` 校验）；
4. `softprops/action-gh-release` 创建 Release 并上传产物。

> 版本引用纪律（2026-07 修正）：**不使用浮动大版本 tag**（如 `v0`）——可变 tag
> 破坏可复现性且有供应链风险；用户一律 pin 精确版本（如 `@v0.0.5`），
> action.yml 的默认 version 输入与最新 release 同步更新。

## 版本与兼容性

- 语义化版本；配置字段只增不删，破坏性变更升大版本；
- CHANGELOG.md 按 Keep a Changelog 维护。

## 本仓库 CI（`.github/workflows/ci.yml`）

- `cargo fmt --check`、`cargo clippy -- -D warnings`、`cargo test`；
- 构建 musl 产物冒烟（不发布）；
- **自举（dogfood）**：本仓库的 PR/issue 用 hoverstare 自己处理，跑的是**事件所在版本
  的源码构建**（`cargo build --release`，不是预编译二进制），因此 dogfood 永远测最新代码。
- **版本自救（pin）**：dogfood 默认用事件所在版本构建；需要时可以固定构建某个 ref：
  - 手动触发（`workflow_dispatch`）：`version` 填分支/tag/commit，`pr` 填要处理的 PR，
    维护者可用任意 ref（这是 master 坏掉时的自救入口）；
  - 自动触发：写 `hoverstare-pin: <ref>` 即生效。读取顺序 = 触发评论 → **该 PR/issue 的最近 30 条评论（新→旧）**
    → PR/issue body。评论是可编辑的，所以不只读触发的那一条：改过的、较早写下的标记同样生效。
    **这里只接受 `master`、release tag（`vX.Y.Z`），或可从默认分支到达的 commit**——任何能开 PR
    的人都能写这个标记，而 workflow 持有写权限，不能允许它指着一份别人控制的代码去构建运行。
    **分支名一律拒绝**：先过 `^[0-9a-f]{7,40}$`，再过 `merge-base --is-ancestor` 校验
    （必须从默认分支可达）——分支本身正是别人可控的指向。
  - **流程级 pin（issue → go → PR）**：流程开始时，harness 会把当时的默认分支 revision 以隐藏标记
    `<!-- hoverstare-pin: <sha> -->` 写进 PR body；该流程后续每一轮都构建这个 revision（同一版本
    还让 pin 构建命中按 sha 的缓存 `pin-<commit>`，不再逐轮冷编译）。人工覆盖仍可用
    `hoverstare-pin: master` 或 `workflow_dispatch` 的 `version`。
  - **提交身份**：`commit_identity = "coauthor"`（默认）时作者 = 触发者，提交信息带
    `Co-authored-by: hoverstare[bot]` 尾注；`commit_author = "Name <email>"` 可覆盖作者，且
    **覆盖优先于"谁触发的"**，因此自触发轮与本地 `develop --task` 也按覆盖署名
    （本仓库配置为 `刘冲 <mail@liuchong.dev>`）。`commit_identity = "bot"` 仍表示纯 bot
    身份，覆盖不生效。
  - pin 只改变**二进制的来源**：工作区仍是被处理的那份代码，`show_base_file`、工具沙箱、
    push 目标都不受影响；pin 构建在独立 worktree 里进行，并按 **pin 的 commit** 单独缓存，
    因此一个流程只有第一轮付冷编译（约 3 分钟），后续轮命中缓存。
- **流程级 pin**：流程（issue → `go` → PR）开始时，harness 把当时默认分支的 revision 以隐藏标记
  `hoverstare-pin: <sha>` 写进 PR body；该流程后续每一轮都构建这个 revision，master 前进不会
  让流程中途换代码，也不会因此每轮重建。标记接受的 ref：`master`、release tag（`vX.Y.Z`）、
  或**可从默认分支到达的 commit**；分支名一律拒绝（任何能开 PR 的人都能写标记，而 workflow 有写权限）。
  手工覆盖仍可用 `hoverstare-pin: master` 或手动触发的 `version` 输入。
- **提交签名**：开发轮的提交必须签名。CI 上导入 `HOVERSTARE_GPG_PRIVATE_KEY`（armored 私钥）
  与可选 `HOVERSTARE_GPG_PASSPHRASE` 后设置 `user.signingkey` + `commit.gpgsign=true`，并在导入后
  做一次空提交自检（签不出来直接失败，避免静默产出未签名提交）。未配置 secret 时该步骤跳过，
  行为与之前一致（提交为未签名）。本地 `hoverstare develop --task` 走本机 git 配置，同样受
  `commit.gpgsign` 约束。

## 测试要点

- action 的下载/校验/缓存逻辑在 fork 仓库手动验证；
- release dry-run：`cargo build --target ...musl` 在 CI 通过；
- sha256 不匹配时 action 明确报错退出。
