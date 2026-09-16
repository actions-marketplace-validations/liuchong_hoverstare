//! Local git operations for the develop loop (spec 11 §3.3).
//! Process-based (git CLI, present in every Actions runner).

use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepo(String),
    #[error("rebase/pull conflict: {0}")]
    Conflict(String),
    #[error("git {0}")]
    Other(String),
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
    pub async fn merge_ref(&self, reference: &str) -> Result<(), GitError> {
        let out = tokio::process::Command::new("git")
            .args(["merge", "--no-edit", reference])
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
    /// there was nothing to commit.
    pub async fn commit(
        &self,
        message: &str,
        author_name: &str,
        author_email: &str,
    ) -> Result<Option<String>, GitError> {
        let out = tokio::process::Command::new("git")
            .args([
                "-c",
                &format!("user.name={author_name}"),
                "-c",
                &format!("user.email={author_email}"),
                "commit",
                "-m",
                message,
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
        assert_eq!(repo.commit("nothing", "t", "t@t").await.unwrap(), None);

        std::fs::write(repo.root().join("b.txt"), "two\n").unwrap();
        assert!(repo.has_changes().await.unwrap());
        repo.add_all().await.unwrap();
        let sha = repo
            .commit("feat: add b", "hoverstare[bot]", "bot@example.com")
            .await
            .unwrap();
        assert!(sha.is_some());
        assert!(!repo.has_changes().await.unwrap());
        let log = repo.run(&["log", "--format=%an %s", "-1"]).await.unwrap();
        assert_eq!(log, "hoverstare[bot] feat: add b");
    }

    #[tokio::test]
    async fn merging_the_base_is_clean_or_aborts_without_touching_the_tree() {
        let (_d, repo) = fixture().await;
        // A clean side branch merges into the development branch.
        repo.run(&["checkout", "-q", "-b", "dev"]).await.unwrap();
        std::fs::write(repo.root().join("dev.txt"), "dev\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("dev work", "t", "t@t").await.unwrap();
        repo.run(&["checkout", "-q", "master"]).await.unwrap();
        std::fs::write(repo.root().join("base.txt"), "base\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("base work", "t", "t@t").await.unwrap();
        repo.run(&["checkout", "-q", "dev"]).await.unwrap();
        repo.merge_ref("master").await.expect("clean merge");
        assert!(repo.root().join("base.txt").exists());

        // A conflicting branch is aborted, and the tree still holds the round's
        // own work rather than a half-merged mess.
        std::fs::write(repo.root().join("a.txt"), "dev side\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("dev edits a", "t", "t@t").await.unwrap();
        repo.run(&["checkout", "-q", "master"]).await.unwrap();
        std::fs::write(repo.root().join("a.txt"), "master side\n").unwrap();
        repo.add_all().await.unwrap();
        repo.commit("master edits a", "t", "t@t").await.unwrap();
        repo.run(&["checkout", "-q", "dev"]).await.unwrap();
        let error = repo.merge_ref("master").await.expect_err("conflict");
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
        repo1.commit("conflicting", "t", "t@t").await.unwrap();
        let err = repo1.pull_rebase().await.unwrap_err();
        assert!(matches!(err, GitError::Conflict(_)), "{err:?}");
    }
}
