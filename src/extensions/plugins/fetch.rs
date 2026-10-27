//! The marketplaces and plugins fetched into `~/.harness/plugins`: git
//! repositories and npm packages. Each is kept by version, as
//! `<dir>/<version>/` (a commit, or the package's version) with
//! `<dir>/current` naming the one in use, so updating never changes files a
//! running hook or MCP server reads: the new version goes beside the old
//! one and applies from the next start. An old version is deleted once no
//! harness process that used it is running.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{Value, json};

/// A marketplace or a plugin fetched from elsewhere.
#[derive(Debug, Clone, PartialEq)]
pub struct Checkout {
    /// `marketplace <name>` or `plugin <id>`, for messages.
    pub label: String,
    /// Where its versions are kept.
    pub dir: PathBuf,
    pub source: Source,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Git {
        url: String,
        /// A branch or tag; the default branch without one.
        git_ref: Option<String>,
        /// A commit to stay on.
        sha: Option<String>,
    },
    Npm {
        package: String,
        /// A version or range; the latest without one.
        version: Option<String>,
        registry: Option<String>,
    },
}

/// What `<dir>/current` holds.
struct Current {
    version: String,
    /// The source it was fetched from, to notice one that changed in the
    /// settings or the marketplace.
    source: String,
}

/// Where the processes using each version leave a mark,
/// `<version>.<pid>`.
const IN_USE: &str = ".in-use";
const TMP_PREFIX: &str = ".tmp-";

impl Checkout {
    fn spec(&self) -> String {
        let field = |v: &Option<String>| v.clone().unwrap_or_default();
        match &self.source {
            Source::Git { url, git_ref, sha } => format!("git\n{url}\n{}\n{}", field(git_ref), field(sha)),
            Source::Npm { package, version, registry } => format!("npm\n{package}\n{}\n{}", field(version), field(registry)),
        }
    }

    /// Where it comes from, for messages.
    fn origin(&self) -> String {
        match &self.source {
            Source::Git { url, .. } => url.clone(),
            Source::Npm { package, .. } => format!("npm package {package}"),
        }
    }

    /// The directory of the version in use, fetched if it is not there yet
    /// (or its source changed). This process counts as using it from now on.
    pub fn ensure(&self) -> Result<PathBuf, String> {
        self.drop_old_layout();
        let version = match self.current() {
            Some(current) if current.source == self.spec() && self.dir.join(&current.version).is_dir() => current.version,
            _ => {
                eprintln!("\x1b[2mfetching {} from {}…\x1b[0m", self.label, self.origin());
                let version = self.fetch().map_err(|e| format!("fetching {}: {e}", self.origin()))?;
                self.set_current(&version)?;
                version
            }
        };
        self.mark_in_use(&version);
        self.prune();
        Ok(self.dir.join(version))
    }

    /// Fetches its latest version (one pinned to a commit stays) and makes
    /// it current, from the next start on. Says what changed.
    pub fn update(&self) -> Result<String, String> {
        if let Source::Git { sha: Some(sha), .. } = &self.source {
            return Ok(format!("{}: pinned to {}", self.label, short(sha)));
        }
        self.drop_old_layout();
        let before = self.current().filter(|c| c.source == self.spec()).map(|c| c.version);
        let after = self.fetch()?;
        let report = match &before {
            Some(before) if *before == after => format!("{}: up to date ({})", self.label, short(&after)),
            Some(before) => format!("{}: {} → {}", self.label, short(before), short(&after)),
            None => format!("{}: fetched {}", self.label, short(&after)),
        };
        self.set_current(&after)?;
        self.prune();
        Ok(report)
    }

    fn current(&self) -> Option<Current> {
        let text = std::fs::read_to_string(self.dir.join("current")).ok()?;
        let value: Value = serde_json::from_str(&text).ok()?;
        Some(Current { version: value["version"].as_str()?.to_string(), source: value["source"].as_str()?.to_string() })
    }

    /// Makes `version` the one in use, replacing the file at once.
    fn set_current(&self, version: &str) -> Result<(), String> {
        let text = json!({"version": version, "source": self.spec()}).to_string();
        let tmp = self.dir.join(format!("{TMP_PREFIX}{}-current", std::process::id()));
        std::fs::write(&tmp, text)
            .and_then(|()| std::fs::rename(&tmp, self.dir.join("current")))
            .map_err(|e| format!("writing {}: {e}", self.dir.join("current").display()))
    }

    /// Fetches the source into a new version directory, unless that version
    /// is there already; returns its name.
    fn fetch(&self) -> Result<String, String> {
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("creating {}: {e}", self.dir.display()))?;
        let tmp = self.dir.join(format!("{TMP_PREFIX}{}-{}", std::process::id(), unique()));
        let _ = std::fs::remove_dir_all(&tmp);
        let fetched = match &self.source {
            Source::Git { url, git_ref, sha } => clone(&tmp, url, git_ref.as_deref(), sha.as_deref()),
            Source::Npm { package, version, registry } => pack(&tmp, package, version.as_deref(), registry.as_deref()),
        };
        let result = fetched.and_then(|(root, version)| {
            let target = self.dir.join(&version);
            if target.is_dir() {
                return Ok(version);
            }
            std::fs::rename(&root, &target).map_err(|e| format!("moving it into {}: {e}", target.display()))?;
            Ok(version)
        });
        let _ = std::fs::remove_dir_all(&tmp);
        result
    }

    /// Earlier versions kept one clone in `dir` itself.
    fn drop_old_layout(&self) {
        if self.dir.join(".git").exists() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn mark_in_use(&self, version: &str) {
        let marks = self.dir.join(IN_USE);
        let _ = std::fs::create_dir_all(&marks);
        let _ = std::fs::write(marks.join(format!("{version}.{}", std::process::id())), "");
    }

    /// Deletes the versions no longer current that no running process
    /// uses, and what fetches that never finished left behind.
    fn prune(&self) {
        let Some(current) = self.current() else { return };
        let marks = self.dir.join(IN_USE);
        let mut users: Vec<(String, u32)> = Vec::new();
        for entry in std::fs::read_dir(&marks).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((version, pid)) = name.rsplit_once('.').and_then(|(v, p)| Some((v.to_string(), p.parse().ok()?)))
            else {
                continue;
            };
            if alive(pid) {
                users.push((version, pid));
            } else {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        for entry in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            if let Some(rest) = name.strip_prefix(TMP_PREFIX) {
                let pid = rest.split('-').next().and_then(|p| p.parse().ok());
                if pid.is_some_and(|pid| !alive(pid)) {
                    let _ = if path.is_dir() { std::fs::remove_dir_all(&path) } else { std::fs::remove_file(&path) };
                }
                continue;
            }
            if name.starts_with('.') || name == "current" || name == current.version || !path.is_dir() {
                continue;
            }
            if !users.iter().any(|(version, _)| *version == name) {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
    }
}

/// The first characters of a commit; a version as it is.
fn short(version: &str) -> &str {
    if version.len() == 12 && version.chars().all(|c| c.is_ascii_hexdigit()) { &version[..7] } else { version }
}

/// A number no other call in this process got.
fn unique() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether process `pid` is running.
fn alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else { return false };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

/// Clones `url` into `dir`; returns the files' directory and the commit.
fn clone(dir: &Path, url: &str, git_ref: Option<&str>, sha: Option<&str>) -> Result<(PathBuf, String), String> {
    let dir_arg = dir.to_string_lossy();
    let mut args = vec!["clone", "--quiet", "--depth", "1"];
    if let Some(git_ref) = git_ref {
        args.extend(["--branch", git_ref]);
    }
    args.extend([url, &dir_arg]);
    git(None, &args)?;
    if let Some(sha) = sha {
        git(Some(dir), &["fetch", "--quiet", "--depth", "1", "origin", sha])?;
        git(Some(dir), &["checkout", "--quiet", "--detach", sha])?;
    }
    let head = git(Some(dir), &["rev-parse", "HEAD"])?;
    // Nothing reads the history; the version's name keeps the commit.
    std::fs::remove_dir_all(dir.join(".git")).map_err(|e| e.to_string())?;
    Ok((dir.to_path_buf(), head.chars().take(12).collect()))
}

/// Fetches npm package `package` with `npm pack` and unpacks it in `dir`;
/// returns the package's directory and its version. Its scripts never run.
fn pack(dir: &Path, package: &str, version: Option<&str>, registry: Option<&str>) -> Result<(PathBuf, String), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let spec = match version {
        Some(version) => format!("{package}@{version}"),
        None => package.to_string(),
    };
    let mut cmd = std::process::Command::new("npm");
    cmd.args(["pack", "--json", "--ignore-scripts", "--pack-destination"]).arg(dir).arg(&spec);
    if let Some(registry) = registry {
        cmd.args(["--registry", registry]);
    }
    let output = run(cmd.current_dir(dir), "npm pack")?;
    let packed: Value = serde_json::from_str(&output).map_err(|e| format!("npm pack printed no JSON: {e}"))?;
    let (Some(file), Some(version)) = (packed[0]["filename"].as_str(), packed[0]["version"].as_str()) else {
        return Err("npm pack named no package".into());
    };
    if version.is_empty() || version.contains(['/', '\\']) || version.starts_with('.') {
        return Err(format!("invalid package version `{version}`"));
    }
    let unpacked = dir.join("unpacked");
    std::fs::create_dir_all(&unpacked).map_err(|e| e.to_string())?;
    let mut tar = std::process::Command::new("tar");
    tar.arg("-xzf").arg(dir.join(file.rsplit('/').next().unwrap_or(file))).arg("-C").arg(&unpacked);
    run(&mut tar, "tar")?;
    // The files are in one directory, usually `package/`.
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&unpacked).map_err(|e| e.to_string())?.flatten().map(|e| e.path()).collect();
    let root = match entries.len() {
        1 if entries[0].is_dir() => entries.remove(0),
        _ => unpacked,
    };
    Ok((root, version.to_string()))
}

/// Runs `cmd` without input; returns what it printed, or the last line of
/// its errors.
fn run(cmd: &mut std::process::Command, name: &str) -> Result<String, String> {
    let output = cmd.stdin(Stdio::null()).output().map_err(|e| format!("cannot run {name}: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.lines().map(str::trim).rfind(|l| !l.is_empty()).unwrap_or("failed");
        return Err(format!("{name}: {detail}"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Runs git, in `dir` if given, without asking for credentials; returns
/// its output.
fn git(dir: Option<&Path>, args: &[&str]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("git");
    if let Some(dir) = dir {
        cmd.arg("-C").arg(dir);
    }
    run(cmd.args(args).env("GIT_TERMINAL_PROMPT", "0"), &format!("git {}", args[0]))
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// A git repository in `dir` holding `files`, committed.
    pub(in crate::extensions::plugins) fn repo(dir: &Path, files: &[(&str, &str)]) {
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

    fn git_checkout(dir: PathBuf, origin: &Path) -> Checkout {
        let url = format!("file://{}", origin.display());
        Checkout { label: "plugin p@m".into(), dir, source: Source::Git { url, git_ref: None, sha: None } }
    }

    /// A process id no process has.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        pid
    }

    #[test]
    fn keeps_versions_and_updates() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        repo(&origin, &[("a.txt", "one")]);
        let checkout = git_checkout(tmp.path().join("cache/m/p"), &origin);
        let first = checkout.ensure().unwrap();
        assert_eq!(std::fs::read_to_string(first.join("a.txt")).unwrap(), "one");
        assert_eq!(first.parent(), Some(checkout.dir.as_path()));
        assert!(!first.join(".git").exists());

        repo(&origin, &[("a.txt", "two")]);
        assert_eq!(checkout.ensure().unwrap(), first, "only fetched once");
        let report = checkout.update().unwrap();
        assert!(report.contains(" → "), "{report}");
        let second = checkout.ensure().unwrap();
        assert_ne!(second, first);
        assert_eq!(std::fs::read_to_string(second.join("a.txt")).unwrap(), "two");
        assert_eq!(std::fs::read_to_string(first.join("a.txt")).unwrap(), "one", "a version in use stays");
        assert!(checkout.update().unwrap().contains("up to date"));

        // Once nobody running uses it, the old version goes.
        let marks = checkout.dir.join(IN_USE);
        std::fs::remove_file(marks.join(format!("{}.{}", first.file_name().unwrap().to_string_lossy(), std::process::id())))
            .unwrap();
        std::fs::write(marks.join(format!("{}.{}", first.file_name().unwrap().to_string_lossy(), dead_pid())), "").unwrap();
        std::fs::create_dir(checkout.dir.join(format!("{TMP_PREFIX}{}-0", dead_pid()))).unwrap();
        checkout.ensure().unwrap();
        assert!(!first.exists());
        let mut left: Vec<String> =
            std::fs::read_dir(&checkout.dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        let mut expected = [IN_USE.to_string(), "current".into(), second.file_name().unwrap().to_string_lossy().into_owned()];
        left.sort();
        expected.sort();
        assert_eq!(left, expected);

        // A source that changed is fetched again; one that fails leaves the
        // version in use.
        let moved = Checkout { source: Source::Git { url: format!("file://{}", origin.display()), git_ref: Some("nope".into()), sha: None }, ..checkout.clone() };
        assert!(moved.ensure().is_err());
        assert_eq!(checkout.ensure().unwrap(), second);
        assert!(std::fs::read_dir(&checkout.dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(TMP_PREFIX)));
    }

    #[test]
    fn replaces_the_old_single_clone() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        repo(&origin, &[("a.txt", "one")]);
        let dir = tmp.path().join("cache/m/p");
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let path = git_checkout(dir.clone(), &origin).ensure().unwrap();
        assert!(path.join("a.txt").is_file() && !dir.join(".git").exists());
    }

    #[test]
    fn fetches_npm_packages() {
        if std::process::Command::new("npm").arg("--version").output().is_err() {
            eprintln!("npm not found; skipped");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let package = tmp.path().join("pkg");
        std::fs::create_dir_all(package.join(".claude-plugin")).unwrap();
        std::fs::write(package.join("package.json"), r#"{"name": "kit", "version": "1.2.0", "scripts": {"prepack": "exit 1"}}"#)
            .unwrap();
        std::fs::write(package.join(".claude-plugin/plugin.json"), r#"{"name": "kit"}"#).unwrap();
        let checkout = Checkout {
            label: "plugin kit@m".into(),
            dir: tmp.path().join("cache/m/kit"),
            source: Source::Npm { package: package.display().to_string(), version: None, registry: None },
        };
        let path = checkout.ensure().unwrap();
        assert_eq!(path, checkout.dir.join("1.2.0"));
        assert!(path.join(".claude-plugin/plugin.json").is_file());
        assert!(checkout.update().unwrap().contains("up to date (1.2.0)"));
    }
}
