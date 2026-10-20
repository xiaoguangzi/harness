//! Git worktrees for subagents started with `isolation: "worktree"`: each
//! works on a checkout of its own, on a branch of its own, so subagents
//! changing files at once cannot get in each other's way, nor in the main
//! agent's. The worktrees live in the temp directory (which the sandbox lets
//! commands write) and start from the commit checked out now.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug, Clone)]
pub struct Worktree {
    /// The repository it belongs to, its top level.
    repo: PathBuf,
    /// The checkout's top level.
    pub dir: PathBuf,
    /// Where in it the subagent works: the same subdirectory the harness
    /// works in, in the repository.
    pub workdir: PathBuf,
    pub branch: String,
    /// The commit it started from.
    base: String,
}

/// What a subagent left in its worktree.
#[derive(Debug, PartialEq)]
pub struct Changes {
    /// Files changed and not committed (`git status --porcelain` lines).
    pub files: usize,
    /// Commits on its branch since it started.
    pub commits: usize,
}

fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().context("running git")?;
    if !output.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim_end().to_string())
}

impl Worktree {
    /// A new worktree named `name` of the repository the working directory
    /// `cwd` is in, on a new branch `harness/<name>` from its `HEAD`.
    pub fn create(cwd: &Path, name: &str) -> Result<Self> {
        let repo = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"]).context("isolation: \"worktree\" needs a git repository")?);
        let base = git(&repo, &["rev-parse", "HEAD"]).context("the repository has no commit to start a worktree from")?;
        let inside = cwd.canonicalize().ok().and_then(|c| c.strip_prefix(repo.canonicalize().ok()?).ok().map(Path::to_path_buf));
        // SAFETY: getuid cannot fail.
        let uid = unsafe { libc::getuid() };
        let repo_name = repo.file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
        let dir = std::env::temp_dir().join(format!("harness-{uid}")).join("worktrees").join(format!("{repo_name}-{name}"));
        let branch = format!("harness/{name}");
        std::fs::create_dir_all(dir.parent().unwrap_or(&dir)).context("creating the worktrees directory")?;
        git(&repo, &["worktree", "add", "-b", &branch, &dir.display().to_string(), &base])?;
        let workdir = inside.map_or(dir.clone(), |inside| dir.join(inside));
        Ok(Self { repo, dir, workdir, branch, base })
    }

    /// Makes it again after [`Worktree::remove`], on its branch, which
    /// was kept if it had commits.
    pub fn ensure(&self) -> Result<()> {
        if self.dir.exists() {
            return Ok(());
        }
        let dir = self.dir.display().to_string();
        match git(&self.repo, &["worktree", "add", &dir, &self.branch]) {
            Ok(_) => Ok(()),
            Err(_) => git(&self.repo, &["worktree", "add", "-b", &self.branch, &dir, &self.base]).map(drop),
        }
    }

    pub fn changes(&self) -> Result<Changes> {
        let status = git(&self.dir, &["status", "--porcelain"])?;
        let commits = git(&self.dir, &["rev-list", "--count", &format!("{}..HEAD", self.base)])?;
        Ok(Changes { files: status.lines().count(), commits: commits.trim().parse().unwrap_or(0) })
    }

    /// Removes the worktree and its branch, which have nothing worth
    /// keeping.
    pub fn remove(&self) -> Result<()> {
        git(&self.repo, &["worktree", "remove", "--force", &self.dir.display().to_string()])?;
        git(&self.repo, &["branch", "-D", &self.branch]).map(drop)
    }

    /// Settles the worktree after a run: removed if the subagent left
    /// nothing in it, else kept, with what the parent is told about it.
    pub fn settle(&self) -> String {
        match self.changes() {
            Ok(Changes { files: 0, commits: 0 }) => match self.remove() {
                Ok(()) => "It worked in a git worktree of its own and changed nothing there; the worktree was removed.".into(),
                Err(e) => format!("It worked in a git worktree of its own, {}, and changed nothing there ({e:#}).", self.dir.display()),
            },
            Ok(Changes { files, commits }) => format!(
                "It worked in a git worktree of its own, {dir}, on branch `{branch}` (from {base}), and left {files} \
                 changed {} and {commits} {} there. Its changes are not in your working directory: review them with \
                 `git -C {dir} status` and `git -C {dir} diff {base}`, then bring over what you want (`git merge \
                 {branch}` once they are committed, or `git -C {dir} diff | git apply`), and remove the worktree \
                 with `git worktree remove {dir}` when done.",
                if files == 1 { "file" } else { "files" },
                if commits == 1 { "commit" } else { "commits" },
                dir = self.dir.display(),
                branch = self.branch,
                base = &self.base[..self.base.len().min(12)],
            ),
            Err(e) => format!("It worked in a git worktree of its own, {}; what it left there is unknown ({e:#}).", self.dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(dir.path()).args(args).output().unwrap().status.success(), "{args:?}");
        run(&["init", "-q"]);
        run(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "start"]);
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        dir
    }

    #[test]
    fn creates_settles_and_removes_worktrees() {
        let repo = repo();
        let name = format!("test-{}", std::process::id());
        let tree = Worktree::create(&repo.path().join("sub"), &name).unwrap();
        assert!(tree.dir.join(".git").exists());
        assert!(tree.workdir.ends_with("sub"), "the same subdirectory: {}", tree.workdir.display());
        assert_eq!(tree.changes().unwrap(), Changes { files: 0, commits: 0 });
        assert!(tree.settle().contains("changed nothing"));
        assert!(!tree.dir.exists());
        tree.ensure().unwrap();
        std::fs::write(tree.dir.join("new.txt"), "x").unwrap();
        let note = tree.settle();
        assert!(note.contains("left 1 changed file and 0 commits") && note.contains(&tree.branch), "{note}");
        assert!(tree.dir.exists(), "kept with its changes");
        tree.remove().unwrap();
        assert!(Worktree::create(&std::env::temp_dir(), "x").unwrap_err().to_string().contains("git repository"));
    }
}
