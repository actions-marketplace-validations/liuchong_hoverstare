# Contributing to HoverStare

Thanks for helping improve HoverStare! This guide covers the workflow and quality gates for the Rust workspace (`hoverstare` root crate + `crates/bugbot` alias crate).

For architecture rules and hard constraints, see [`AGENTS.md`](AGENTS.md). For design specs, see [`specs/README.md`](specs/README.md).

## Getting started

1. Clone the repository:
   ```bash
   git clone https://github.com/liuchong/hoverstare.git
   cd hoverstare
   ```
2. Install a recent stable Rust toolchain (the project tracks the latest stable release).
3. The workspace is configured at the repo root; all commands below are run from this directory.

## Quality gate

Every PR must pass the four commands below before being merged. Run them from the workspace root:

```bash
cargo build --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- `cargo build --workspace` ensures the whole workspace compiles.
- `cargo fmt --all -- --check` ensures code is formatted with `rustfmt`.
- `cargo clippy --workspace --all-targets -- -D warnings` runs Clippy and treats any warning as an error.
- `cargo test --workspace` runs all unit tests and contract tests.

If `cargo fmt --all -- --check` fails, run `cargo fmt --all` to apply the formatting.

## Engineering gates

`scripts/verify-all.sh` is the single entry point; CI runs the same script with
`--full --strict` (a missing tool is a failure there, so a gate can never pass by
skipping). Locally:

```bash
scripts/verify-all.sh          # G1 G2 G6 G7 — fast, no extra toolchain
scripts/verify-all.sh --full   # adds G3 dependency audit, G4 coverage, G5 secret scan
scripts/tests/test-gates.sh    # self-test of the gates themselves
```

The gate list is in [`specs/17-verification-gates.md`](specs/17-verification-gates.md):

| Gate | What it protects |
|---|---|
| G1 | Every `uses:` in `action.yml` and the workflows is pinned to a commit SHA (a moved upstream tag changes what runs inside a pinned consumer, and this repository's workflows hold an App key, a PAT and a GPG key) |
| G2 | The six READMEs share one heading structure (translations drift silently otherwise) |
| G3 | Dependencies carry no known advisories and only allow-listed licences |
| G4 | Line coverage does not fall below `scripts/coverage-baseline.txt` |
| G5 | No credential-shaped string reaches the repository |
| G6 | Workflows and gate scripts lint clean |
| G7 | Every spec is indexed and every module declares its spec |

### Upgrading a pinned action

1. Pick the version (a new patch of the current major is routine; a new major is a
   separate decision with its own PR).
2. Resolve the tag to a commit: `gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`.
3. Update the `uses:` line **and** the `# vX.Y.Z` comment in the same edit;
   `scripts/verify-action-pins.sh` fails on a SHA without a source comment.
4. For an action that publishes branch refs only (e.g. `dtolnay/rust-toolchain`),
   keep `with: toolchain: …`: its behaviour comes from the ref, which the pin no
   longer carries.

## Conventional Commits

Use [Conventional Commits](https://www.conventionalcommits.org/) for all commit messages and PR titles. Common prefixes in this repo:

- `feat:` — new feature or behavior
- `fix:` — bug fix
- `docs:` — documentation-only changes
- `refactor:` — code change that neither fixes a bug nor adds a feature
- `test:` — adding or updating tests
- `chore:` — maintenance, tooling, or dependency updates

Examples:

- `feat: add mention command parser`
- `fix: align finding anchor for deleted lines`
- `docs: update quality gate commands in CONTRIBUTING.md`

## PR review process

PR reviews are performed by **HoverStare itself** (the bot). Repo collaborators can trigger or re-trigger a review by posting `@hoverstare review` in a PR. For a list of available commands, post `@hoverstare help`.

If Checks shows a yellow **1 workflow awaiting approval** banner, that is GitHub's maintainer gate for `pull_request` runs. The App identity (`hoverstare[bot]`) covers comments, the PR author, and commits; a later push **from Actions** is still attributed to `github-actions[bot]`. Merging earlier bot PRs does not skip that second actor. Click **Approve workflows to run**, or see the FAQ in [`README.md`](README.md).

For details on bot commands, see the [`@hoverstare` commands](README.md#hoverstare-commands) section in `README.md`.

## Where to learn more

- [`AGENTS.md`](AGENTS.md) — project background, architecture rules, and hard constraints.
- [`specs/README.md`](specs/README.md) — design specs and milestone plan.
- [`README.md`](README.md) — quick start, local dry-run examples, and the `@hoverstare` command table.
- [`docs/web-ide.md`](docs/web-ide.md) — using GitHub in the browser as the develop-mode IDE (usage + dogfood notes).

## Local develop loop

For rapid iteration on prompts, tools, or the review pipeline, use the `develop` subcommand locally. It runs a single task against your working tree without publishing anything to GitHub.

```bash
cargo run -- develop --task "<task-description>"
```

For example:

```bash
cargo run -- develop --task "review src/cli.rs for argument parsing issues"
```

- `--task` describes the goal for the agent; the pipeline runs the same backend and tools used in production.
- No comments, reviews, or status checks are posted to GitHub.
- Combine with `--verbose` for debug-level logging.

This is the fastest way to verify changes before pushing a branch.

## License

By contributing, you agree that your contributions will be licensed under the [1PL — One Public License](https://license.pub/1pl/).
