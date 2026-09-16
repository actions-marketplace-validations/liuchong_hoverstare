# GitHub as the IDE (develop mode)

HoverStare’s develop mode treats the GitHub website as the workspace: the
issue is the task doc, comments are the pairing session, the PR is the working
tree, each Action run is one development round, merge is delivery. It does
**not** watch CI and start fixing on its own (spec 11). The human stays in the
conversation.

This page is both a **usage guide** and a **field note** from shipping
issue [#13](https://github.com/liuchong/hoverstare/issues/13) /
PR [#14](https://github.com/liuchong/hoverstare/pull/14) entirely through that
loop (with k3). Operator checklist: [`AGENTS.md`](../AGENTS.md) §7.5–7.6.

Chinese: [`web-ide.zh-CN.md`](web-ide.zh-CN.md).

## Happy path (browser only)

1. Open an issue, mention `@hoverstare`. It investigates and posts a plan.
2. Reply on the issue until the plan is right. Then `@hoverstare go`.
3. On the PR, give instructions with `@hoverstare …`. It commits as
   `hoverstare[bot]` and reports in a comment. Budget-exhausted rounds
   self-continue (max 10 per PR).
4. If **Checks** shows a yellow **1 workflow awaiting approval**, click
   **Approve workflows to run**. That is GitHub’s first-time-contributor gate
   for `pull_request` runs whose *triggering actor* is often
   `github-actions[bot]` after an Actions-driven push — not a missing LLM key.
   Merging earlier `hoverstare[bot]` PRs only trusts the App identity (the run
   that *opens* the PR).
5. If **check** is red, open the failing job, copy the compiler / rustfmt
   diff, paste it into a PR comment starting with `@hoverstare`. The agent has
   no tool to read Actions logs; “go look at CI yourself” will spin until the
   10-minute timeout and often **leave no comment on the PR**.
6. When checks are green and there are no conflicts: `@hoverstare merge`
   (squash + delete the branch).

Finding-thread discussion (issue #13): on a HoverStare inline finding, reply
**in that review thread** with no `@mention`. The bot answers in the same
thread. `@hoverstare explain` stays the explicit form.

## What the webpage already provides

| Need | Where on github.com |
|---|---|
| Approve a waiting workflow | PR → Checks → **Approve workflows to run** |
| Feed CI failure into the next round | Checks → failed job log → copy → PR comment |
| Change a workflow file | File editor → commit to the PR branch (a GitHub App **cannot** push `.github/workflows/*` without the `workflows` permission) |
| `GH_PAT` / Actions approval policy | Repo **Settings** |
| See a develop run that failed with no PR comment | **Actions** tab, not the conversation |

Stay on one `@hoverstare` round at a time. A second mention cancels the
in-progress job (`cancel-in-progress`).

`issue_comment` and `pull_request_review_comment` workflows are taken from the
**default branch**. A job `if` change on the PR only applies to comment-driven
runs after it is merged.

## Gaps (issue #13 dogfood) and what to improve

These blocked a *comments-only* loop (no Settings, no file editor). They are
the backlog for develop-mode UX, not a change to “don’t auto-fix CI”.

| Gap | Why | Improve? |
|---|---|---|
| Agent cannot “open the failing check” | Tools are repo read/write only | **Yes**: read-only summary of failed checks on this PR head (truncate). `list_check_runs` already exists for merge. Human still says “CI is red”. |
| rustfmt / tests | Spec 11 does not execute builds | **Keep**. Paste the Checks diff. Do not have the model guess formatting. |
| Bot cannot change workflow YAML | GitHub rejects App pushes without `workflows` | Document the web editor; before `git push`, unset `http.https://github.com/.extraheader` so `GH_PAT` is actually used. Granting the App `workflows: write` keeps bot authorship at a larger permission surface. |
| `GH_PAT` still rejected as a GitHub App | `actions/checkout` persist-credentials extraheader wins over the PAT remote | **Yes** (same as above). `persist-credentials: false` is on the dogfood workflow as of PR #14; comment-triggered runs use the **default branch** copy until that lands. |
| Timeout / push failure leaves no PR comment | The Action step exits 1 | **Yes**: always `create_issue_comment` on timeout, push rejected, or three failed attempts. |
| Yellow banner every continue push | Triggering actor is `github-actions[bot]` | Click Approve on the page. Do **not** auto-approve via `pull_request_target`. Fewer clicks: PAT push (after extraheader fix), or a comment when checks are `action_required`. |
| Quotes in the instruction break `xargs` | Dogfood workflow parses `@hoverstare` with xargs | **Yes**: don’t use xargs default quoting. |

**Do not build:** auto-starting a develop round because CI went red;
`pull_request_target` auto-approvers.

## Commits: identity and signing

Develop-mode commits are made by the Action, so you decide whose name they carry
(`commit_identity`, `commit_author`) and whether they are signed.

### Identity

| `commit_identity` | author | message trailer |
|---|---|---|
| `author` | the person who triggered the round | — |
| `bot` | `hoverstare[bot]` | — |
| `coauthor` (default) | the person who triggered the round | `Co-authored-by: hoverstare[bot] …` |

- `commit_author = "Name <email>"` overrides the author. The override also
  applies to self-triggered rounds and to local `hoverstare develop --task`
  runs, which have no human trigger.
- When the author is overridden, the **committer is that same identity**, not
  the bot. GitHub verifies a signature against the *committer*, and an app
  account cannot hold signing keys: with the bot as committer even a correctly
  signed commit is reported as unverified (`unknown_key`). If you want the
  green "Verified" badge, set `commit_author` to the account that owns the
  signing key.
- Pushing always uses the write token (`GH_PAT`, or an app token with
  `contents: write`); the settings above only decide how the commit object
  looks.

### Signing (GPG)

Without the secrets below nothing changes: commits are unsigned.

1. Use a key whose capabilities include signing (`S`; sign-only is fine).
   CI cannot answer a passphrase prompt, so either use a passphrase-less key or
   provide the passphrase too (the workflow wraps gpg in loopback mode).
2. Store the **private** key as a repository secret:
   ```bash
   gpg --armor --export-secret-keys <KEY_ID> \
     | gh secret set HOVERSTARE_GPG_PRIVATE_KEY
   # only when the key has a passphrase:
   printf '%s' '<passphrase>' | gh secret set HOVERSTARE_GPG_PASSPHRASE
   ```
3. Add the **public** key to the GitHub account of the commit author
   (Settings → SSH and GPG keys → New GPG key). Until you do, commits are
   signed but GitHub shows them as unverified.
4. The workflow imports the key, sets `user.signingkey` and
   `commit.gpgsign`, then makes one empty self-check commit: if that commit
   does not come back signed, the run fails loudly instead of quietly producing
   unsigned history.

Keep a copy of the private key you can restore from (a password manager, or an
export encrypted to your own key): repository secrets cannot be read back. To
stop using it, delete the secret, remove the public key from the account, and
delete the key locally.

### Verifying

```bash
git log -1 --format='%G?'   # G/U = signed here, N = not signed
gh api repos/<owner>/<repo>/commits/<sha> --jq '.commit.verification'
#   {"verified":true, "reason":"valid"}        key is on the account
#   {"verified":false,"reason":"unknown_key"}  committer does not own the key
#   {"verified":false,"reason":"unsigned"}     no signing key configured
```

### One revision per flow

A flow (issue → `go` → pull request) records the default-branch revision it
started from in the pull request body, and every later round of that flow builds
that same revision — master moving on does not change the code under a flow in
flight, and the pinned build reuses its own cache. To move a flow deliberately,
pin it yourself: a comment containing `hoverstare-pin: master` (or a release tag,
or a commit reachable from the default branch) switches the source for the next
round, and a manual `workflow_dispatch` can name any ref through its `version`
input. Marker pins are refused for branches, because anyone who can open a pull
request can write a comment while the workflow holds write permissions. The
round report states which revision it built from.

## Demo notes from #13 / #14

- Specs first, then code: unmentioned finding-thread replies, in-thread
  `explain`, parent-marker gate, serve-mode routing, dogfood `if`, help in
  six languages.
- CI was only `cargo fmt --check` until the human pasted the Checks diff.
- Asking the bot to fetch Actions logs timed out three times (600s each) with
  no PR comment.
- Two rounds edited `.github/workflows/hoverstare.yml` locally; GitHub
  rejected the App push. A collaborator committed that file (including
  `persist-credentials: false`).
- After fmt and docs, `check` and dogfood review were green. That is
  **done** for the feature: remaining items in the table are develop-mode
  platform work, not blockers for #13.
