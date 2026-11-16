//! Git worktrees for subagents started with `isolation: "worktree"`: each
//! works on a checkout of its own, on a branch of its own, so subagents
//! changing files at once cannot get in each other's way, nor in the main
//! agent's. The worktrees live in the temp directory (which the sandbox lets
//! commands write) and start from the commit checked out now. One that holds
//! nothing is removed when it is let go of: after a run that left nothing in
//! it, or with its subagent (`/clear`, the conversation's end, a subagent
//! forgotten to make room), or when the subagent could not start. One a
//! process left behind when it was killed is removed by the next one that
//! makes a worktree, if it holds nothing.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

#[derive(Debug)]
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

/// Where the worktrees are made. Beside them, `.owners/<dir name>` says
/// which process made each, of which repository, on which branch and from
/// which commit, so a later process can tell one left behind.
fn root() -> PathBuf {
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    std::env::temp_dir().join(format!("harness-{uid}")).join("worktrees")
}

fn owner_file(dir: &Path) -> Option<PathBuf> {
    Some(dir.parent()?.join(".owners").join(dir.file_name()?))
}

/// Whether the process `pid` is running (or belongs to another user).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    pid > 0 && (unsafe { libc::kill(pid, 0) } == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM))
}

/// Removes what processes that are gone left in `root`: a worktree with
/// nothing in it, with its branch; a worktree's registration and branch
/// when the directory itself went (a temp cleaner). One with changes stays.
fn sweep(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root.join(".owners")) else { return };
    for entry in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(entry.path()) else { continue };
        let lines: Vec<&str> = text.lines().collect();
        let [pid, repo, branch, base] = lines[..] else { continue };
        if pid.parse().is_ok_and(alive) {
            continue;
        }
        let dir = root.join(entry.file_name());
        let tree = Worktree { repo: repo.into(), workdir: dir.clone(), dir, branch: branch.into(), base: base.into() };
        if tree.dir.exists() {
            // Removed when let go of, with its owner file, if it holds nothing.
            continue;
        }
        let _ = git(&tree.repo, &["worktree", "prune"]);
        let unmoved = git(&tree.repo, &["rev-list", "--count", &format!("{}..{}", tree.base, tree.branch)]).is_ok_and(|n| n == "0");
        if unmoved {
            let _ = git(&tree.repo, &["branch", "-D", &tree.branch]);
        }
        let _ = std::fs::remove_file(entry.path());
    }
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
        let root = root();
        sweep(&root);
        let repo_name = repo.file_name().map_or("repo".into(), |n| n.to_string_lossy().into_owned());
        let dir = root.join(format!("{repo_name}-{name}"));
        let branch = format!("harness/{name}");
        std::fs::create_dir_all(root.join(".owners")).context("creating the worktrees directory")?;
        git(&repo, &["worktree", "add", "-b", &branch, &dir.display().to_string(), &base])?;
        let workdir = inside.map_or(dir.clone(), |inside| dir.join(inside));
        let tree = Self { repo, dir, workdir, branch, base };
        tree.mark();
        Ok(tree)
    }

    /// Records this process as the worktree's owner.
    fn mark(&self) {
        if let Some(file) = owner_file(&self.dir) {
            let text = format!("{}\n{}\n{}\n{}\n", std::process::id(), self.repo.display(), self.branch, self.base);
            let _ = std::fs::write(file, text);
        }
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
        }?;
        self.mark();
        Ok(())
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
        if let Some(file) = owner_file(&self.dir) {
            let _ = std::fs::remove_file(file);
        }
        git(&self.repo, &["branch", "-D", &self.branch]).map(drop)
    }

    /// Removes the worktree and its branch if they hold nothing: no changed
    /// file and no commit. True if it is gone.
    pub fn discard_if_unchanged(&self) -> bool {
        if !self.dir.exists() {
            return true;
        }
        matches!(self.changes(), Ok(Changes { files: 0, commits: 0 })) && self.remove().is_ok()
    }

    /// Where the worktree is, for a parent whose subagent stopped in it.
    pub fn whereabouts(&self) -> String {
        format!(
            "Its git worktree, {}, on branch `{}`, stays as it is; message_agent continues it there.",
            self.dir.display(),
            self.branch
        )
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

impl Drop for Worktree {
    fn drop(&mut self) {
        self.discard_if_unchanged();
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
        let dir = tree.dir.clone();
        drop(tree);
        assert!(dir.exists(), "kept when let go of, with its changes");
        git(repo.path(), &["worktree", "remove", "--force", &dir.display().to_string()]).unwrap();
        assert!(Worktree::create(&std::env::temp_dir(), "x").unwrap_err().to_string().contains("git repository"));
    }

    #[test]
    fn worktrees_left_by_a_process_that_is_gone_are_swept() {
        let repo = repo();
        let dead = {
            let mut child = Command::new("true").spawn().unwrap();
            let pid = child.id();
            child.wait().unwrap();
            pid
        };
        let orphan = |name: &str| {
            let tree = Worktree::create(repo.path(), &format!("{name}-{}", std::process::id())).unwrap();
            let file = owner_file(&tree.dir).unwrap();
            let text = std::fs::read_to_string(&file).unwrap();
            std::fs::write(&file, text.replacen(&std::process::id().to_string(), &dead.to_string(), 1)).unwrap();
            let kept = (tree.dir.clone(), tree.branch.clone(), file);
            std::mem::forget(tree);
            kept
        };
        let (empty, empty_branch, empty_owner) = orphan("test-sweep-empty");
        let (changed, _, _) = orphan("test-sweep-changed");
        std::fs::write(changed.join("work.txt"), "x").unwrap();
        let (gone, gone_branch, gone_owner) = orphan("test-sweep-gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let live = Worktree::create(repo.path(), &format!("test-sweep-live-{}", std::process::id())).unwrap();

        sweep(&root());
        assert!(!empty.exists() && !empty_owner.exists(), "nothing in it");
        assert!(changed.exists(), "its changes are kept");
        assert!(live.dir.exists(), "its process is running");
        assert!(!gone_owner.exists());
        let branches = git(repo.path(), &["branch", "--list", "harness/*"]).unwrap();
        assert!(!branches.contains(&empty_branch) && !branches.contains(&gone_branch), "{branches}");
        assert!(!git(repo.path(), &["worktree", "list"]).unwrap().contains(&gone.display().to_string()), "pruned");
        git(repo.path(), &["worktree", "remove", "--force", &changed.display().to_string()]).unwrap();
        let _ = std::fs::remove_file(owner_file(&changed).unwrap());
    }

    #[test]
    fn one_let_go_of_with_nothing_in_it_is_removed() {
        let repo = repo();
        let tree = Worktree::create(repo.path(), &format!("test-drop-{}", std::process::id())).unwrap();
        let (dir, branch) = (tree.dir.clone(), tree.branch.clone());
        drop(tree);
        assert!(!dir.exists());
        assert!(git(repo.path(), &["branch", "--list", &branch]).unwrap().is_empty(), "its branch went too");
    }
}
