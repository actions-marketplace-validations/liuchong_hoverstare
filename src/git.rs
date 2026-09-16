//! Local git operations for the develop loop (spec 11 §3.3).
//! Process-based (git CLI, present in every Actions runner).

use std::path::{Path, PathBuf};

use crate::config::CommitIdentity;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(String),
    #[error("rebase/pull conflict: {0}")]
    Conflict(String),
    #[error("git {0}")]
    Other(String),
}

/// Bot identity used for develop-mode commits (spec 11 §3.3).
pub const BOT_NAME: &str = "hoverstare[bot]";
pub const BOT_EMAIL: &str = "hoverstare[bot]@users.noreply.github.com";

/// A git identity in `Name <email>` form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: String,
    pub email: String,
}

impl Identity {
    /// The `--author="Name <email>"` form.
    pub fn spec(&self) -> String {
        format!("{} <{}>", self.name, self.email)
    }
}

/// Author/committer pair (plus an optional message trailer) for one develop
/// commit (spec 11 §3.3). The committer is always the bot: the author says
/// whose instruction produced the change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitAuthor {
    /// `git commit --author`.
    pub author: Identity,
    /// `git -c user.name/user.email` (who actually ran git).
    pub committer: Identity,
    /// Trailing paragraph appended to the message, e.g. a `Co-authored-by:`.
    pub trailer: Option<String>,
}

impl CommitAuthor {
    /// Author = committer = hoverstare[bot], no trailer (the historical behaviour).
    pub fn bot() -> Self {
        let bot = bot_identity();
        Self {
            author: bot.clone(),
            committer: bot,
            trailer: None,
        }
    }
}

fn bot_identity() -> Identity {
    Identity {
        name: BOT_NAME.to_string(),
        email: BOT_EMAIL.to_string(),
    }
}

/// GitHub exposes no email for a login; attribute it to the noreply address
/// (overridable through `commit_author`).
fn trigger_identity(login: &str) -> Identity {
    Identity {
        name: login.to_string(),
        email: format!("{login}@users.noreply.github.com"),
    }
}

/// Parse a `Name <email>` identity (the `commit_author` config form).
pub fn parse_identity(spec: &str) -> Option<Identity> {
    let (name, rest) = spec.trim().split_once('<')?;
    let email = rest.strip_suffix('>')?.trim();
    let name = name.trim();
    if name.is_empty() || email.is_empty() || email.contains(['<', '>']) || !email.contains('@') {
        return None;
    }
    Some(Identity {
        name: name.to_string(),
        email: email.to_string(),
    })
}

/// Resolve the commit contract for a develop round (spec 11 §3.3).
///
/// `trigger` is the login of the instruction's author (the triggering comment,
/// falling back to the issue/PR author); when it is missing — or is the bot
/// itself, as in a self-triggered round — every mode degrades to
/// [`CommitAuthor::bot`], never to an invented `<login>@users.noreply.github.com`
/// author. `override_` is an explicit `Name <email>` from `commit_author` and
/// only applies to the human (`author`/`coauthor`) modes.
pub fn resolve_commit_identity(
    mode: CommitIdentity,
    trigger: Option<&str>,
    override_: Option<&str>,
) -> CommitAuthor {
    // `bot` means "the bot's own identity", so it wins over any override.
    if mode == CommitIdentity::Bot {
        return CommitAuthor::bot();
    }
    // An explicit author override says whose commits these are, not who
    // happened to trigger the round: it applies to local runs (no trigger) and
    // to self-triggered rounds (the bot as trigger) alike. Without this, a
    // repository that configured its own identity would still get bot-authored
    // commits on every automatic round.
    if let Some(author) = override_.and_then(parse_identity) {
        let trailer = (mode == CommitIdentity::Coauthor)
            .then(|| format!("Co-authored-by: {BOT_NAME} <{BOT_EMAIL}>"));
        // Committer = author here, not the bot. GitHub verifies a signature
        // against the *committer*: with the bot as committer even a correctly
        // signed commit is reported as `unknown_key`, because an app account
        // cannot hold signing keys. Verified commits therefore require the
        // human to be the committer (the co-author trailer still credits the
        // bot). Observed, not assumed: a probe commit signed with the
        // maintainer key and committed as the bot came back unverified.
        return CommitAuthor {
            committer: author.clone(),
            author,
            trailer,
        };
    }
    let Some(trigger) = trigger.map(str::trim).filter(|t| !t.is_empty()) else {
        return CommitAuthor::bot();
    };
    if trigger == BOT_NAME {
        return CommitAuthor::bot();
    }
    let author = trigger_identity(trigger);
    let trailer = (mode == CommitIdentity::Coauthor)
        .then(|| format!("Co-authored-by: {BOT_NAME} <{BOT_EMAIL}>"));
    CommitAuthor {
        author,
        committer: bot_identity(),
        trailer,
    }
}

pub struct GitRepo {
    root: PathBuf,
}

impl GitRepo {
    /// Open a repository at `root` (must already be a git work tree).
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, GitError> {
        let root = root.into();
        let ok = std::process::Command::new("git")
            .args(["rev-parse", "--is-inside-work-tree"])
            .current_dir(&root)
            .output()
            .map(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "true")
            .unwrap_or(false);
        if !ok {
            return Err(GitError::NotARepo(root.display().to_string()));
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) async fn run(&self, args: &[&str]) -> Result<String, GitError> {
        let out = tokio::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| GitError::Other(format!("{}: spawn: {e}", args.join(" "))))?;
        if out.status.success() {
            Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
        } else {
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            Err(GitError::Other(format!(
                "{}: {stderr}",
                scrub_tokens(&args.join(" "))
            )))
        }
    }

    /// `git remote add <name> <url>` (idempotent: removes first).
    /// Note: the URL may embed a token; it lives only in .git/config (same
    /// exposure as actions/checkout's persist-credentials), never in logs.
    pub async fn set_remote(&self, name: &str, url: &str) -> Result<(), GitError> {
        let _ = self.run(&["remote", "remove", name]).await;
        self.run(&["remote", "add", name, url]).await.map(|_| ())?;
        Ok(())
    }

    /// Force the local branch to exactly match `from` (a fetched ref):
    /// `git checkout -B <branch> <from>`. Human commits are already on the
    /// remote we fetched from, so nothing is ever overwritten (spec 11 §6).
    pub async fn checkout_reset(&self, branch: &str, from: &str) -> Result<(), GitError> {
        self.run(&["checkout", "-B", branch, from])
            .await
            .map(|_| ())
    }

    pub async fn current_branch(&self) -> Result<String, GitError> {
        self.run(&["rev-parse", "--abbrev-ref", "HEAD"]).await
    }

    /// `git merge-base --is-ancestor <ancestor> <head>`: whether `ancestor` is
    /// reachable from `head`. An unknown object and an unrelated history both
    /// make git exit non-zero, and both mean the same thing to the caller (the
    /// artifact gate): do not trust the recorded commit.
    pub async fn is_ancestor(&self, ancestor: &str, head: &str) -> Result<bool, GitError> {
        match self
            .run(&["merge-base", "--is-ancestor", ancestor, head])
            .await
        {
            Ok(_) => Ok(true),
            Err(GitError::Other(_)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Create and switch to a new branch from `from` (e.g. "origin/master").
    pub async fn checkout_new(&self, name: &str, from: &str) -> Result<(), GitError> {
        self.run(&["checkout", "-b", name, from]).await.map(|_| ())
    }

    pub async fn checkout(&self, branch: &str) -> Result<(), GitError> {
        self.run(&["checkout", branch]).await.map(|_| ())
    }

    /// `git pull --rebase`; conflicts are reported as [`GitError::Conflict`].
    pub async fn pull_rebase(&self) -> Result<(), GitError> {
        let out = tokio::process::Command::new("git")
            .args(["pull", "--rebase"])
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| GitError::Other(format!("pull --rebase: spawn: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Leave the tree untouched for the human when we bail out.
        let _ = tokio::process::Command::new("git")
            .args(["rebase", "--abort"])
            .current_dir(&self.root)
            .output()
            .await;
        Err(GitError::Conflict(stderr))
    }

    /// Merge `reference` into the current branch, leaving a merge commit.
    ///
    /// A development branch drifts behind its base while other work lands, and a
    /// conflicted pull request stops GitHub from running `pull_request` checks
    /// at all — no CI, no failure, no feedback. So a round syncs before it
    /// develops: on conflict the merge is aborted (the tree is left exactly as
    /// the human will find it) and reported as [`GitError::Conflict`].
    ///
    /// The merge commit takes the round's identity contract: its committer is
    /// always hoverstare[bot] (the `author` still credits the trigger for the
    /// round's own commits). A CI runner has no global git identity, and without
    /// one git refuses to create the merge commit at all.
    pub async fn merge_ref(
        &self,
        reference: &str,
        identity: &CommitAuthor,
    ) -> Result<(), GitError> {
        let name = &identity.committer.name;
        let email = &identity.committer.email;
        let identity_name = format!("user.name={name}");
        let identity_email = format!("user.email={email}");
        let out = tokio::process::Command::new("git")
            .args([
                "-c",
                &identity_name,
                "-c",
                &identity_email,
                "merge",
                "--no-edit",
                reference,
            ])
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| GitError::Other(format!("merge: spawn: {e}")))?;
        if out.status.success() {
            return Ok(());
        }
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let _ = tokio::process::Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(&self.root)
            .output()
            .await;
        Err(GitError::Conflict(stderr))
    }

    /// Whether the working tree has unmerged paths (a merge in progress).
    pub async fn has_conflicts(&self) -> Result<bool, GitError> {
        let out = self
            .run(&["diff", "--name-only", "--diff-filter=U"])
            .await?;
        Ok(!out.trim().is_empty())
    }

    pub async fn add_all(&self) -> Result<(), GitError> {
        self.run(&["add", "-A"]).await.map(|_| ())
    }

    /// Working tree has staged or unstaged changes.
    pub async fn has_changes(&self) -> Result<bool, GitError> {
        let out = self.run(&["status", "--porcelain"]).await?;
        Ok(!out.is_empty())
    }

    /// `git status --porcelain` (for dry-run reporting).
    pub async fn status_porcelain(&self) -> Result<String, GitError> {
        self.run(&["status", "--porcelain"]).await
    }

    /// Commit staged changes; returns the new commit sha, or `None` when
    /// there was nothing to commit. `identity` sets the author (the trigger or
    /// the bot), the committer (always the bot) and an optional trailer
    /// (spec 11 §3.3).
    pub async fn commit(
        &self,
        message: &str,
        identity: &CommitAuthor,
    ) -> Result<Option<String>, GitError> {
        let message = match &identity.trailer {
            Some(trailer) => format!("{}\n\n{trailer}", message.trim_end()),
            None => message.to_string(),
        };
        let user_name = format!("user.name={}", identity.committer.name);
        let user_email = format!("user.email={}", identity.committer.email);
        let author = identity.author.spec();
        let out = tokio::process::Command::new("git")
            .args([
                "-c",
                &user_name,
                "-c",
                &user_email,
                "commit",
                "-m",
                &message,
                &format!("--author={author}"),
            ])
            .current_dir(&self.root)
            .output()
            .await
            .map_err(|e| GitError::Other(format!("commit: spawn: {e}")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        if !out.status.success() {
            if stdout.contains("nothing to commit") || stderr.contains("nothing to commit") {
                return Ok(None);
            }
            return Err(GitError::Other(format!("commit: {stderr}")));
        }
        let sha = self.run(&["rev-parse", "HEAD"]).await?;
        Ok(Some(sha))
    }

    /// `git push <remote> HEAD:<branch>`.
    pub async fn push(&self, remote: &str, branch: &str) -> Result<(), GitError> {
        self.run(&["push", remote, &format!("HEAD:{branch}")])
            .await
            .map(|_| ())
    }

    /// `git fetch <remote> <refspec>`.
    pub async fn fetch(&self, remote: &str, refspec: &str) -> Result<(), GitError> {
        self.run(&["fetch", remote, refspec]).await.map(|_| ())
    }
}

/// Never leak tokens embedded in remote URLs into error messages.
fn scrub_tokens(s: &str) -> String {
    match regex::Regex::new(r"x-access-token:[^@\s]+@") {
        Ok(re) => re.replace_all(s, "x-access-token:***@").to_string(),
        Err(_) => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        tokio::process::Command::new("git")
            .args(["init", "-q", "-b", "master"])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qm",
                "init",
            ])
            .current_dir(root)
            .output()
            .await
            .unwrap();
        let repo = GitRepo::open(root).unwrap();
        (dir, repo)
    }

    #[tokio::test]
    async fn branch_commit_cycle() {
        let (_d, repo) = fixture().await;
        assert_eq!(repo.current_branch().await.unwrap(), "master");
        repo.checkout_new("feat-x", "master").await.unwrap();
        assert_eq!(repo.current_branch().await.unwrap(), "feat-x");
        assert!(!repo.has_changes().await.unwrap());
        assert_eq!(
            repo.commit("nothing", &CommitAuthor::bot()).await.unwrap(),
            None
        );

        std::fs::write(repo.root().join("b.txt"), "two\n").unwrap();
        assert!(repo.has_changes().await.unwrap());
        repo.add_all().await.unwrap();
        let sha = repo
            .commit("feat: add b", &CommitAuthor::bot())
            .await
            .unwrap();
        assert!(sha.is_some());
        assert!(!repo.has_changes().await.unwrap());
        let log = repo.run(&["log", "--format=%an %s", "-1"]).await.unwrap();
        assert_eq!(log, "hoverstare[bot] feat: add b");
    }

    #[test]
    fn an_explicit_author_override_outlives_the_trigger() {
        let override_ = Some("刘冲 <mail@liuchong.dev>");
        // No trigger at all (local runs).
        let local = resolve_commit_identity(CommitIdentity::Coauthor, None, override_);
        assert_eq!(local.author.name, "刘冲");
        assert_eq!(local.author.email, "mail@liuchong.dev");
        assert!(local.trailer.is_some(), "coauthor keeps the trailer");
        // A self-triggered round: the bot is the trigger, the override still wins.
        let self_triggered =
            resolve_commit_identity(CommitIdentity::Coauthor, Some(BOT_NAME), override_);
        assert_eq!(self_triggered.author.name, "刘冲");
        assert!(self_triggered.trailer.is_some());
        // `author` mode has no trailer; `bot` mode ignores the override.
        assert_eq!(
            local.committer.email, local.author.email,
            "the committer must carry the override: GitHub verifies signatures\n             against the committer, and the bot cannot hold a signing key"
        );
        let plain = resolve_commit_identity(CommitIdentity::Author, None, override_);
        assert_eq!(plain.author.name, "刘冲");
        assert!(plain.trailer.is_none());
        let bot = resolve_commit_identity(CommitIdentity::Bot, Some("alice"), override_);
        assert_eq!(bot.author.name, BOT_NAME);
        assert!(bot.trailer.is_none());
        // Without an override, the trigger decides as before.
        let triggered = resolve_commit_identity(CommitIdentity::Coauthor, Some("alice"), None);
        assert_eq!(triggered.author.email, "alice@users.noreply.github.com");
    }

    #[tokio::test]
    async fn merging_the_base_is_clean_or_aborts_without_touching_the_tree() {
        let (_d, repo) = fixture().await;
        let identity = CommitAuthor::bot();
        // A clean side branch merges into the development branch.
        repo.run(&["checkout", "-q", "-b", "dev"]).await.unwrap();
        std::fs::write(repo.root().join("dev.txt"), "dev\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("dev work", &identity).await.unwrap();
        repo.run(&["checkout", "-q", "master"]).await.unwrap();
        std::fs::write(repo.root().join("base.txt"), "base\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("base work", &identity).await.unwrap();
        repo.run(&["checkout", "-q", "dev"]).await.unwrap();
        repo.merge_ref("master", &identity)
            .await
            .expect("clean merge");
        assert!(repo.root().join("base.txt").exists());

        // A conflicting branch is aborted, and the tree still holds the round's
        // own work rather than a half-merged mess.
        std::fs::write(repo.root().join("a.txt"), "dev side\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("dev edits a", &identity).await.unwrap();
        repo.run(&["checkout", "-q", "master"]).await.unwrap();
        std::fs::write(repo.root().join("a.txt"), "master side\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("master edits a", &identity).await.unwrap();
        repo.run(&["checkout", "-q", "dev"]).await.unwrap();
        let error = repo
            .merge_ref("master", &identity)
            .await
            .expect_err("conflict");
        assert!(matches!(error, GitError::Conflict(_)), "{error:?}");
        assert!(!repo.has_conflicts().await.unwrap());
        assert_eq!(
            std::fs::read_to_string(repo.root().join("a.txt")).unwrap(),
            "dev side\n"
        );
    }

    #[tokio::test]
    async fn push_and_pull_rebase_conflict() {
        // Bare remote + two clones; advance remote from clone2, then
        // pull_rebase in clone1 with a conflicting change → Conflict.
        let remote = tempfile::tempdir().unwrap();
        tokio::process::Command::new("git")
            .args(["init", "-q", "--bare", "-b", "master"])
            .current_dir(remote.path())
            .output()
            .await
            .unwrap();
        let c1 = tempfile::tempdir().unwrap();
        let c2 = tempfile::tempdir().unwrap();
        for c in [&c1, &c2] {
            tokio::process::Command::new("git")
                .args(["clone", "-q", remote.path().to_str().unwrap(), "."])
                .current_dir(c.path())
                .output()
                .await
                .unwrap();
            std::fs::write(c.path().join("a.txt"), "one\n").unwrap();
            tokio::process::Command::new("git")
                .args(["add", "-A"])
                .current_dir(c.path())
                .output()
                .await
                .unwrap();
            tokio::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "commit",
                    "-qm",
                    "init",
                ])
                .current_dir(c.path())
                .output()
                .await
                .unwrap();
        }
        // c2 pushes an advance
        std::fs::write(c2.path().join("a.txt"), "two\n").unwrap();
        tokio::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-qam",
                "advance",
            ])
            .current_dir(c2.path())
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["push", "-q", "origin", "master"])
            .current_dir(c2.path())
            .output()
            .await
            .unwrap();
        // c1 has a conflicting local change
        let repo1 = GitRepo::open(c1.path()).unwrap();
        std::fs::write(c1.path().join("a.txt"), "three\n").unwrap();
        repo1.add_all().await.unwrap();
        repo1
            .commit("conflicting", &CommitAuthor::bot())
            .await
            .unwrap();
        let err = repo1.pull_rebase().await.unwrap_err();
        assert!(matches!(err, GitError::Conflict(_)), "{err:?}");
    }

    async fn author_of(repo: &GitRepo) -> (String, String) {
        split_identity(
            &repo
                .run(&["log", "-1", "--format=%an%x00%ae"])
                .await
                .unwrap(),
        )
    }

    async fn committer_of(repo: &GitRepo) -> (String, String) {
        split_identity(
            &repo
                .run(&["log", "-1", "--format=%cn%x00%ce"])
                .await
                .unwrap(),
        )
    }

    fn split_identity(raw: &str) -> (String, String) {
        let (name, email) = raw.split_once('\0').unwrap();
        (name.to_string(), email.to_string())
    }

    async fn commit_body(repo: &GitRepo) -> String {
        repo.run(&["log", "-1", "--format=%B"]).await.unwrap()
    }

    #[test]
    fn resolve_identity_rules() {
        use crate::config::CommitIdentity;
        // No trigger → bot, whatever the mode or override.
        assert_eq!(
            resolve_commit_identity(CommitIdentity::Coauthor, None, None),
            CommitAuthor::bot()
        );
        assert_eq!(
            resolve_commit_identity(CommitIdentity::Coauthor, Some("  "), None),
            CommitAuthor::bot()
        );
        // The override wins for the human modes, and is ignored in bot mode.
        let c = resolve_commit_identity(
            CommitIdentity::Coauthor,
            Some("alice"),
            Some("Bob <bob@example.com>"),
        );
        assert_eq!(c.author.name, "Bob");
        assert_eq!(c.author.email, "bob@example.com");
        assert_eq!(
            c.trailer.as_deref(),
            Some("Co-authored-by: hoverstare[bot] <hoverstare[bot]@users.noreply.github.com>")
        );
        assert_eq!(
            resolve_commit_identity(CommitIdentity::Bot, Some("alice"), Some("Bob <bob@x.io>")),
            CommitAuthor::bot()
        );
        // A malformed override falls back to the trigger's noreply address.
        let d = resolve_commit_identity(CommitIdentity::Author, Some("alice"), Some("nope"));
        assert_eq!(d.author.email, "alice@users.noreply.github.com");
        assert!(d.trailer.is_none());
        // A self-triggered round (the bot's own `@hoverstare continue`) has no
        // human trigger: bot identity, and no trailer crediting the bot itself.
        assert_eq!(
            resolve_commit_identity(CommitIdentity::Coauthor, Some("hoverstare[bot]"), None),
            CommitAuthor::bot()
        );
        assert_eq!(parse_identity("A B <a@b.c>").unwrap().name, "A B");
    }

    #[test]
    fn parse_identity_rejects_malformed() {
        // Valid: the `Name <email>` form, whitespace-tolerant.
        let id = parse_identity(" Alice <alice@example.com> ").unwrap();
        assert_eq!(id.name, "Alice");
        assert_eq!(id.email, "alice@example.com");
        // Malformed: no brackets, no email, empty email, empty name, unbalanced.
        for bad in [
            "alice@example.com",
            "Alice alice@example.com",
            "Alice <>",
            "<alice@example.com>",
            "Alice <alice@example.com",
            "Alice alice@example.com>",
        ] {
            assert!(parse_identity(bad).is_none(), "accepted {bad:?}");
        }
    }

    #[tokio::test]
    async fn commit_identity_per_mode() {
        use crate::config::CommitIdentity;
        for (mode, want_author, want_trailer) in [
            (CommitIdentity::Bot, "hoverstare[bot]", false),
            (CommitIdentity::Author, "alice", false),
            (CommitIdentity::Coauthor, "alice", true),
        ] {
            let (_d, repo) = fixture().await;
            std::fs::write(repo.root().join("b.txt"), "two\n").unwrap();
            repo.add_all().await.unwrap();
            let identity = resolve_commit_identity(mode, Some("alice"), None);
            repo.commit("feat: add b", &identity).await.unwrap();

            let (an, ae) = author_of(&repo).await;
            assert_eq!(an, want_author, "{mode:?} author name");
            // Committer is always the bot (who ran git).
            let (cn, ce) = committer_of(&repo).await;
            assert_eq!(cn, "hoverstare[bot]", "{mode:?} committer name");
            assert_eq!(ce, "hoverstare[bot]@users.noreply.github.com");
            if mode == CommitIdentity::Bot {
                assert_eq!(ae, "hoverstare[bot]@users.noreply.github.com");
            } else {
                assert_eq!(ae, "alice@users.noreply.github.com");
            }
            let body = commit_body(&repo).await;
            assert_eq!(
                body.contains(
                    "Co-authored-by: hoverstare[bot] <hoverstare[bot]@users.noreply.github.com>"
                ),
                want_trailer,
                "{mode:?} trailer"
            );
        }
    }
}
