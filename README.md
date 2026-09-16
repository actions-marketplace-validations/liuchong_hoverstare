<p align="center">
  <img src=".github/assets/logo.svg" width="128" alt="hoverstare logo" />
  <h1 align="center">HoverStare</h1>
  <p align="center">
    <b>AI code review that actually reads your repo.</b>
  </p>
  <p align="center">
    <i>The name comes from the Stephen Chow movie gag "凌空瞪" — a disembodied eyeball floating in mid-air, staring you down.</i>
  </p>
  <p align="center">
    <a href="https://github.com/liuchong/hoverstare/actions/workflows/ci.yml"><img src="https://github.com/liuchong/hoverstare/actions/workflows/ci.yml/badge.svg" alt="CI" /></a>
    <a href="https://github.com/liuchong/hoverstare/releases"><img src="https://img.shields.io/github/v/release/liuchong/hoverstare" alt="release" /></a>
    <a href="https://crates.io/crates/hoverstare"><img src="https://img.shields.io/crates/v/hoverstare" alt="crates.io" /></a>
    <a href="https://license.pub/1pl/"><img src="https://img.shields.io/badge/license-1PL-green" alt="license 1PL" /></a>
  </p>
  <p align="center">
    <b>English</b> ·
    <a href="docs/readme/README.zh-CN.md">简体中文</a> ·
    <a href="docs/readme/README.ru.md">Русский</a> ·
    <a href="docs/readme/README.fr.md">Français</a> ·
    <a href="docs/readme/README.de.md">Deutsch</a> ·
    <a href="docs/readme/README.es.md">Español</a>
  </p>
</p>

<br/>

HoverStare is an AI code review bot for GitHub pull requests, written in Rust and
shipped as a single static binary that runs as a GitHub Action. Instead of
tossing a diff at a model in one shot, its reviewer **reads your repository
like a human reviewer would** — opening context files, grepping call sites,
comparing against the base branch — before it says anything. A multi-pass
vote plus an independent verifier keeps false positives down, and every
finding it reports is tracked across commits until it's fixed.

## Why HoverStare?

- 🔍 **Repo-aware, not diff-only.** The reviewing model gets a read-only tool
  set (`read_file` / `grep` / `glob` / `show_base_file`) and uses it to verify
  suspicions before reporting. It catches bugs that hide *outside* the diff —
  like a changed function whose callers break two files away.
- 🗳️ **Multi-pass voting + verifier.** Three independent review passes
  (correctness / concurrency / security lenses) vote on findings; lone-vote
  findings must survive an independent verifier pass with tool access.
  High signal, low noise.
- 📌 **Precise inline comments.** Line numbers are validated against the real
  diff and snapped to the nearest valid anchor, so comments land exactly where
  the bug is — never on the wrong line.
- 🔁 **Incremental reviews.** Push a fix and HoverStare reviews only the delta,
  marks fixed findings as resolved (or leaves a "✅ confirmed fixed" note),
  and never repeats itself.
- 🛡️ **Fail-open by design.** Network trouble, rate limits, or a flaky model
  will never block your CI.
- 🔑 **BYOK.** Bring your own key: Anthropic, or any OpenAI-compatible endpoint
  (Kimi, DeepSeek, OpenRouter, …). Code goes straight to your provider.

## How it works

```mermaid
flowchart LR
    A[PR opened / synchronized] --> B{skip?}
    B -->|draft / bot / empty diff| Z((exit 0))
    B --> C[fetch diff]
    C --> D{prior review?}
    D -->|yes| E[delta diff]
    D -->|no| F[full diff]
    E --> G
    F --> G["N parallel review passes<br/>(read-only repo tools)"]
    G --> H[cluster & vote]
    H --> I[verifier pass]
    I --> J[validate & anchor lines]
    J --> K["post review<br/>+ resolve fixed threads<br/>+ status checks"]
```

Every inline comment carries a hidden fingerprint (`path + code line + title`
hash). On the next push, HoverStare diffs against its previous review, asks the
model which open findings are fixed, and resolves those threads — immune to
line-number drift.

## Quick start (2 minutes)

**1. Add the workflow** — `.github/workflows/hoverstare.yml`:

```yaml
name: HoverStare
on:
  pull_request:
    types: [opened, reopened, synchronize]
  issue_comment:
    types: [created]
  pull_request_review_comment:
    types: [created]

permissions:
  contents: read
  pull-requests: write
  statuses: write

concurrency:
  # 不含 @hoverstare 的评论事件给独立组名，避免无意义的 run 取消正在跑的审查
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
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: liuchong/hoverstare@v0.0.5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          OPENAI_API_KEY: ${{ secrets.HOVERSTARE_LLM_KEY }}
          OPENAI_BASE_URL: ${{ vars.HOVERSTARE_LLM_BASE_URL }}
          HOVERSTARE_MODEL: ${{ vars.HOVERSTARE_MODEL }}   # e.g. kimi-for-coding
```

**2. Configure LLM credentials** (pick one):

| Provider | Settings |
|---|---|
| **Anthropic** | secret `ANTHROPIC_API_KEY` (default model `claude-sonnet-4-6`) |
| **OpenAI-compatible** (Kimi, DeepSeek, OpenRouter…) | secret `OPENAI_API_KEY`, var `OPENAI_BASE_URL` (e.g. `https://api.kimi.com/coding/v1`), and a model name via var `HOVERSTARE_MODEL` or `model` in `.github/hoverstare.toml` |

> ⚠️ With an OpenAI-compatible endpoint you **must** set the model name —
> the default `claude-sonnet-4-6` won't exist there.

**3. (Optional) Repo config** — `.github/hoverstare.toml`, every field optional:

```toml
model = "kimi-for-coding"             # main review model
reformat_model = "kimi-for-coding-highspeed"  # cheap model for output repair
passes = 3                            # parallel review passes; 1 disables voting
verify = true                         # verifier pass for single-vote findings
severity_threshold = "medium"         # below this → Nitpicks section only
ignore = ["*.lock", "**/dist/**", "**/*.min.js"]
max_diff_kb = 400                     # diff budget (truncated by priority)
max_tool_calls = 20                   # agentic loop tool budget
timeout_secs = 900
review_drafts = false
fail_closed = false                   # true → analysis failures fail CI
status_checks = false                 # write hoverstare / hoverstare-findings checks
language = "en"                       # output language: en/zh-CN/ru/fr/de/es
set_temperature = true                # false for endpoints that only accept default temperature
thinking = "enabled"
reasoning_effort = "medium"
context_tokens = 1000000
instructions = ""                     # team-specific review focus, injected into the system prompt
```

## Repository instructions

HoverStare reads repo-level rule files and applies them to reviews (they
supplement but never override the built-in core rules). Precedence:

1. `hoverstare.md` / `.hoverstare.md` / `.hoverstare/*.md` / `.github/hoverstare.md`
2. `AGENTS.md`
3. `.github/copilot-instructions.md`, `CLAUDE.md`, `.cursorrules`

Files are read **from the base branch** (a PR editing AGENTS.md cannot inject
instructions). Core safety rules (read-only tools, targeted verification,
defect-only scope, JSON contract) can never be overridden.

## Optional: brand identity (posts as your own bot)

By default, reviews post as `github-actions[bot]` — that's a `GITHUB_TOKEN`
limitation, and **it's the recommended mode for most users** (zero extra setup).

Want a branded bot identity? Register **your own** GitHub App (5 minutes, no
server needed — token exchange happens inside GitHub Actions) and pass its
credentials to the action:

1. Create a GitHub App at *Settings → Developer settings → GitHub Apps*
   (webhook **off**; permissions: contents read, pull-requests write,
   issues write, commit statuses write), install it on your repo
2. Add its App ID and private key as secrets `APP_ID` / `APP_PRIVATE_KEY`
3. Pass them:

```yaml
      - uses: liuchong/hoverstare@v0.0.5
        with:
          app_id: ${{ secrets.APP_ID }}
          app_private_key: ${{ secrets.APP_PRIVATE_KEY }}
```

Reviews then post as **your-app-name[bot]**, and `resolveReviewThread` works
without the `GITHUB_TOKEN` limitation (no `GH_PAT` needed).

> **Zero-config `hoverstare[bot]` identity** for everyone is on the roadmap as
> an optional self-hostable `hoverstare serve` webhook service.

## `@hoverstare` commands

Post in a PR (repo collaborators only):

| Command | What it does |
|---|---|
| `@hoverstare review` | Force a full re-review |
| `@hoverstare explain` | Reply in the thread with a plain-language explanation of the finding |
| Reply in a finding thread | Discuss the finding with HoverStare in-thread — no `@mention` needed |
| `@hoverstare help` | Command list |

## Develop mode: issues & PRs as your AI IDE

HoverStare can also *develop* — issues and PRs become a conversation-driven
development environment (spec 11/12). Not automation glue that reacts to
events: a place where you actually build the feature, end to end, in the open.

### How a feature flows

**Issue mainline** — file an issue mentioning `@hoverstare`:

1. It investigates the repo and replies with an analysis + plan (in comments).
2. Discuss by simply replying; each round is answered in the thread.
3. `@hoverstare go` — it creates a branch, implements, pushes, and opens a PR
   (with `Closes #N`).

**PR mainline** — on any same-repo PR:

- `@hoverstare <instruction>` — it checks out the PR branch, develops, commits
  (Conventional Commits, authored as `hoverstare[bot]`), pushes back to the
  branch, and reports in a comment. Rounds that exhaust their budget
  self-continue (max 10 rounds per PR).
- Humans can adjust by committing to the branch directly — the next round
  always syncs to the remote head first and never overwrites your commits.
- `@hoverstare merge` — once checks are green and there are no conflicts, it
  squash-merges **and deletes the source branch**.

### Setup

1. **Workflow**: add the `issues` and `pull_request_review` triggers and grant
   `contents: write` + `issues: write`. `.github/workflows/hoverstare.yml` in
   this repo is a complete working example.
2. **Tokens** (two duties, two tokens):
   - *Identity* (comments, reviews, opening PRs): the default `GITHUB_TOKEN`,
     or a GitHub App token for the `hoverstare[bot]` identity.
   - *Write operations* (git push, merge, branch deletion): a PAT via the
     `gh_pat` input, or an App with `contents: write`. Pushes made with the
     default `GITHUB_TOKEN` do **not** trigger CI, so required checks would
     never run on bot commits.
3. **Permissions** (optional): declare who can use what in
   `.github/hoverstare.toml` — see spec 12. Defaults: auto-review for anyone,
   `@review`/`develop` for collaborators, `merge` for users with write access.

### Notes

- Only repo collaborators can issue commands; fork PRs are out of scope.
- PRs opened by the bot can show **1 workflow awaiting approval** on
  `pull_request` checks. That is GitHub's maintainer gate (below), not a missing
  LLM key. Approve the run, or prevent it as described in the FAQ.
- Large tasks are sliced into budgeted rounds; the bot self-continues
  (max 10 rounds per PR). It cannot run builds or tests. When Checks fail,
  paste the job log into an `@hoverstare` comment — the agent does not fetch
  Actions logs. Browser-only loop, approval banner, and dogfood notes:
  [`docs/web-ide.md`](docs/web-ide.md).

## FAQ

**Reviews/comments fail with permission errors?**
Check workflow `permissions` (`pull-requests: write` required) and repo
*Settings → Actions → General → Workflow permissions* is "Read and write".

**"model not found"?**
You configured an OpenAI-compatible endpoint but no model name. Set
`HOVERSTARE_MODEL` (or `model` in `hoverstare.toml`).

**400 / invalid temperature?**
Your endpoint only accepts the default temperature. Set
`set_temperature = false` in `hoverstare.toml`.

**Fixed findings aren't getting resolved?**
A GitHub platform limitation: the default `GITHUB_TOKEN` cannot call
`resolveReviewThread`. HoverStare falls back to a "✅ confirmed fixed" reply in
the thread. For full resolution, store a classic PAT (`repo` scope) as secret
`GH_PAT` and pass it in the workflow env.

**GitHub Enterprise?**
Set `GITHUB_API_URL=https://<your-ghe-host>/api/v3`.

**`@hoverstare merge` fails with 403?**
The merge endpoint needs `contents: write`. Pass a PAT via `gh_pat`, or grant
the GitHub App Contents: Read and write (and accept the upgrade on the
installation).

**Yellow banner "1 workflow awaiting approval" / checks stuck at *action_required*?**
This is GitHub's maintainer-approval gate for `pull_request` workflows, not a
HoverStare bug and not a missing secret. How to drive develop mode from the
browser, including this banner: [`docs/web-ide.md`](docs/web-ide.md).

GitHub checks **both** the pull-request author and the **actor of the event
that started the run**. Those are different GitHub users:

- Opening the PR as `hoverstare[bot]` uses the App identity. Once a
  `hoverstare[bot]` PR has been merged, later PRs opened by the same bot run
  `pull_request` CI without this banner.
- Using the GitHub App does **not** make those later CI runs look like the
  App. Comments, the PR author, and `git log` stay `hoverstare[bot]`. A commit
  pushed **from Actions** (even with the App installation token) is still
  recorded as `github-actions[bot]` for the `pull_request` workflow. That
  account has never had a PR merged, so the default policy asks a maintainer
  to approve **on every such push**. Approving one SHA does not trust the next.

That is why a repo can already have merged bot PRs and still stop on
`continue` rounds. Single-commit bot PRs only hit the first case (already
trusted). The App is the right unique identity for comments and commits; it
is not the actor GitHub uses for this gate.

Detect:

```bash
gh run list --repo OWNER/REPO --json databaseId,name,conclusion,event,headBranch \
  --jq '.[] | select(.conclusion=="action_required")'
```

Unblock this PR: click **Approve workflows to run**, or

```bash
gh api -X POST repos/OWNER/REPO/actions/runs/RUN_ID/approve
```

Prevent:

1. **What actually skips the gate:** pass a collaborator PAT as `gh_pat` /
   secret `GH_PAT` for `git push`. Keep the App token for comments and opening
   the PR (`hoverstare[bot]` stays the unique product identity). GitHub then
   treats the `pull_request` actor as the collaborator. This is also how
   develop mode is specified: App = identity, PAT = write.
2. **What does not:** using only the App for push (the current fallback when
   `GH_PAT` is unset). That already happens in dogfood, and continue-round CI
   still shows `github-actions[bot]`. Relaxing *Settings → Actions → General →
   Approval for running fork pull request workflows* to **Require approval for
   first-time contributors who are new to GitHub** is fine for real fork PRs;
   it has **not** been observed to skip this same-repo Actions-push gate.
3. Do **not** add a `pull_request_target` workflow whose only job is to approve
   other runs. That event runs with base-branch privileges and is a common
   injection path.

**A `go`/dev round says "no changes, no PR created"?**
Usually the task was too vague for one budgeted round. Reply with a sharper,
smaller instruction (files to touch, acceptance criteria) and run `go` again —
see the bot's comment for what it actually did.

## Local development

```bash
# Dry-run a full review of a public PR (no publishing)
export OPENAI_API_KEY=... OPENAI_BASE_URL=... HOVERSTARE_MODEL=...
cargo run -- review --repo owner/repo --pr 123 --dry-run

# Review a local diff file (prints tool-call trace)
cargo run --example local_review -- path/to.diff [base_ref]

cargo test                                   # unit + httpmock contract tests
cargo clippy --all-targets -- -D warnings
cargo fmt
```

Specs and the milestone plan live in [`specs/`](specs/README.md) — the single
source of truth for design decisions.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md) for the quality gate, commit-message convention, and PR review process.

## Star history & contributors

Auto-updated daily by [RepoScope](https://github.com/liuchong/reposcope) —
committed to the orphan `reposcope` branch, never to `master`.

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history-dark.svg">
  <img alt="Star History" src="https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/star-history.svg">
</picture>

![Contributors](https://raw.githubusercontent.com/liuchong/hoverstare/reposcope/assets/reposcope/contributors.svg)

## License

[1PL — One Public License](https://license.pub/1pl/)
