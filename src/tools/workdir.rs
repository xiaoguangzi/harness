//! A working directory of its own for the tools a subagent runs in a git
//! worktree: relative paths resolve against it, and commands start in it.
//! Elsewhere it is the process's.

use std::future::Future;
use std::path::{Path, PathBuf};

tokio::task_local! {
    static DIR: PathBuf;
}

/// Runs `future` with the tools working in `dir`, if it is given.
pub async fn scope<F: Future>(dir: Option<PathBuf>, future: F) -> F::Output {
    match dir {
        Some(dir) => DIR.scope(dir, future).await,
        None => future.await,
    }
}

/// Runs `f` with the tools working in `dir`, if it is given.
pub fn sync<R>(dir: Option<PathBuf>, f: impl FnOnce() -> R) -> R {
    match dir {
        Some(dir) => DIR.sync_scope(dir, f),
        None => f(),
    }
}

/// The working directory the running tools were given, if any.
pub fn current() -> Option<PathBuf> {
    DIR.try_with(Clone::clone).ok()
}

/// The working directory the tools work in.
pub fn get() -> PathBuf {
    current().or_else(|| std::env::current_dir().ok()).unwrap_or_default()
}

/// `path`, made absolute against the given working directory when it is
/// relative and there is one; otherwise as it is.
pub fn resolve(path: &str) -> String {
    match current() {
        Some(dir) if Path::new(path).is_relative() => dir.join(path).display().to_string(),
        _ => path.to_string(),
    }
}

/// `command` for `bash -c`, made to start in the given working directory.
pub fn command(command: &str) -> String {
    match current() {
        Some(dir) => format!("cd {} || exit 1\n{command}", shell_quote(&dir.display().to_string())),
        None => command.to_string(),
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_against_the_given_directory_only() {
        assert_eq!(resolve("src/a.rs"), "src/a.rs");
        assert_eq!(command("ls"), "ls");
        scope(Some(PathBuf::from("/tmp/it's")), async {
            assert_eq!(resolve("src/a.rs"), "/tmp/it's/src/a.rs");
            assert_eq!(resolve("/etc/hosts"), "/etc/hosts");
            assert_eq!(command("ls"), "cd '/tmp/it'\\''s' || exit 1\nls");
            assert_eq!(get(), PathBuf::from("/tmp/it's"));
        })
        .await;
    }
}
