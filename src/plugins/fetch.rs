//! Git checkouts of marketplaces and plugins, under `~/.harness/plugins`.

use std::path::{Path, PathBuf};
use std::process::Stdio;

/// A repository cloned for a marketplace or a plugin.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    /// `marketplace <name>` or `plugin <id>`, for messages.
    pub label: String,
    pub dir: PathBuf,
    pub url: String,
    /// A branch or tag; the default branch without one.
    pub git_ref: Option<String>,
    /// A commit to stay on.
    pub sha: Option<String>,
}

impl Checkout {
    /// What it was cloned from, kept in its `.git` to notice a source that
    /// changed in the settings or the marketplace.
    fn spec(&self) -> String {
        let field = |v: &Option<String>| v.clone().unwrap_or_default();
        format!("{}\n{}\n{}\n", self.url, field(&self.git_ref), field(&self.sha))
    }

    fn spec_file(dir: &Path) -> PathBuf {
        dir.join(".git/harness-source")
    }

    /// Clones it, unless it is there already from the same source.
    pub fn ensure(&self) -> Result<(), String> {
        if std::fs::read_to_string(Self::spec_file(&self.dir)).is_ok_and(|s| s == self.spec()) {
            return Ok(());
        }
        eprintln!("\x1b[2mfetching {} from {}…\x1b[0m", self.label, self.url);
        let parent = self.dir.parent().ok_or("no parent directory")?;
        std::fs::create_dir_all(parent).map_err(|e| format!("creating {}: {e}", parent.display()))?;
        let name = self.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        // Cloned beside it first, so a failure leaves what was there.
        let fresh = parent.join(format!(".{name}.{}.tmp", std::process::id()));
        let _ = std::fs::remove_dir_all(&fresh);
        let cloned = self.clone_into(&fresh).and_then(|()| {
            std::fs::write(Self::spec_file(&fresh), self.spec()).map_err(|e| e.to_string())?;
            match std::fs::remove_dir_all(&self.dir) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.to_string()),
                _ => {}
            }
            std::fs::rename(&fresh, &self.dir).map_err(|e| e.to_string())
        });
        if cloned.is_err() {
            let _ = std::fs::remove_dir_all(&fresh);
        }
        cloned.map_err(|e| format!("fetching {}: {e}", self.url))
    }

    fn clone_into(&self, dir: &Path) -> Result<(), String> {
        let dir_arg = dir.to_string_lossy();
        let mut args = vec!["clone", "--quiet", "--depth", "1"];
        if let Some(git_ref) = &self.git_ref {
            args.extend(["--branch", git_ref]);
        }
        args.extend([self.url.as_str(), &dir_arg]);
        git(None, &args)?;
        if let Some(sha) = &self.sha {
            git(Some(dir), &["fetch", "--quiet", "--depth", "1", "origin", sha])?;
            git(Some(dir), &["checkout", "--quiet", "--detach", sha])?;
        }
        Ok(())
    }

    /// Moves it to the latest commit of its branch; one pinned to a commit
    /// stays. Says what changed.
    pub fn update(&self) -> Result<String, String> {
        if let Some(sha) = &self.sha {
            return Ok(format!("{}: pinned to {}", self.label, &sha[..sha.len().min(7)]));
        }
        let head = || git(Some(&self.dir), &["rev-parse", "--short", "HEAD"]);
        let before = head()?;
        let branch = self.git_ref.as_deref().unwrap_or("HEAD");
        git(Some(&self.dir), &["fetch", "--quiet", "--depth", "1", "origin", branch])?;
        git(Some(&self.dir), &["reset", "--quiet", "--hard", "FETCH_HEAD"])?;
        let after = head()?;
        Ok(if before == after {
            format!("{}: up to date ({after})", self.label)
        } else {
            format!("{}: {before} → {after}", self.label)
        })
    }
}

/// Runs git, in `dir` if given, without asking for credentials; returns
/// its output.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    let output = cmd
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("failed");
        return Err(format!("git {}: {detail}", args[0]));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A git repository in `dir` holding `files`, committed.
    pub(in crate::plugins) fn repo(dir: &Path, files: &[(&str, &str)]) {
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let run = |args: &[&str]| git(Some(dir), args).unwrap();
        if !dir.join(".git").exists() {
            run(&["init", "--quiet", "--initial-branch", "main"]);
        }
        run(&["add", "-A"]);
        run(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "c"]);
    }

    #[test]
    fn clones_once_and_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        repo(&origin, &[("a.txt", "one")]);
        let checkout = Checkout {
            label: "plugin p@m".into(),
            dir: tmp.path().join("cache/m/p"),
            url: format!("file://{}", origin.display()),
            git_ref: None,
            sha: None,
        };
        checkout.ensure().unwrap();
        assert_eq!(std::fs::read_to_string(checkout.dir.join("a.txt")).unwrap(), "one");

        repo(&origin, &[("a.txt", "two")]);
        checkout.ensure().unwrap();
        assert_eq!(std::fs::read_to_string(checkout.dir.join("a.txt")).unwrap(), "one", "only cloned once");
        let report = checkout.update().unwrap();
        assert!(report.contains(" → "), "{report}");
        assert_eq!(std::fs::read_to_string(checkout.dir.join("a.txt")).unwrap(), "two");
        assert!(checkout.update().unwrap().contains("up to date"));

        // A source that changed is cloned again; one that fails leaves the
        // old checkout.
        let moved = Checkout { git_ref: Some("nope".into()), ..checkout.clone() };
        assert!(moved.ensure().is_err());
        assert!(checkout.dir.join("a.txt").exists());
        let entries: Vec<_> = std::fs::read_dir(checkout.dir.parent().unwrap()).unwrap().collect();
        assert_eq!(entries.len(), 1, "no temporary clone left");
    }
}
