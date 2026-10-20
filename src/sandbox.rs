//! OS sandbox for the commands the model runs.
//!
//! When enabled (`--sandbox`, `HARNESS_SANDBOX=1` or `"enabled": true` in
//! the settings file's `sandbox` section), `bash` commands and `grep` searches run in
//! Seatbelt (`sandbox-exec`) on macOS or bubblewrap (`bwrap`) on Linux:
//!
//! - everything is readable except the `deny_read` paths;
//! - writes are allowed under the working directory, the temp directories
//!   and the `allow_write` paths, but never to the config in the working
//!   directory that runs code outside the sandbox later ([`PROTECTED`]);
//! - the network is limited to loopback unless `network` is true.
//!
//! The `read` and `write` tools check the same rules themselves. The config
//! comes only from the user's settings file (`settings.rs`), so a repository
//! cannot loosen it.

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tokio::process::Command;

use crate::util::expand_env;

/// Paths in the working directory that stay read-only, and whether each is
/// a directory: the harness's and Claude Code's config, and git's hooks and
/// config (which can name commands for git to run).
const PROTECTED: [(&str, bool); 5] =
    [(".harness", true), (".claude", true), (".mcp.json", false), (".git/hooks", true), (".git/config", false)];

/// Devices that commands write to as a matter of course.
const SEATBELT_DEVICES: &str =
    r#"(allow file-write* (literal "/dev/null") (literal "/dev/zero") (literal "/dev/dtracehelper") (regex #"^/dev/fd/"))"#;

const SEATBELT_LOOPBACK: &str = r#"(deny network*)
(allow network-bind network-inbound (local ip "localhost:*"))
(allow network-outbound (remote ip "localhost:*"))
"#;

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub enabled: Option<bool>,
    #[serde(default)]
    network: bool,
    #[serde(default)]
    allow_write: Vec<String>,
    #[serde(default)]
    deny_read: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Backend {
    Seatbelt,
    Bubblewrap,
}

impl Backend {
    fn native() -> Result<Self> {
        match std::env::consts::OS {
            "macos" => Ok(Self::Seatbelt),
            "linux" => Ok(Self::Bubblewrap),
            os => bail!("the sandbox supports macOS and Linux, not {os}"),
        }
    }

    fn program(self) -> &'static str {
        match self {
            Self::Seatbelt => "sandbox-exec",
            Self::Bubblewrap => "bwrap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Access {
    Write,
    /// Readable only. `dir` says what to stand in for it on Linux while it
    /// does not exist.
    ReadOnly { dir: bool },
    Hidden,
    /// A directory that cannot be renamed or removed, so the read-only
    /// paths under it stay where they are; its contents follow the rules
    /// above it.
    Pinned,
}

/// Each rule covers its path and everything under it; the deepest rule
/// that covers a path decides.
#[derive(Debug, Clone, PartialEq)]
struct Rule {
    path: PathBuf,
    access: Access,
}

#[derive(Debug)]
pub struct Sandbox {
    backend: Backend,
    cwd: PathBuf,
    network: bool,
    /// Sorted from shallow to deep, so later rules take precedence, as in
    /// a Seatbelt profile and the order of bwrap's mounts.
    rules: Vec<Rule>,
    /// How long starting `true` in it took at startup: what the sandbox
    /// adds to each command.
    startup: Duration,
}

static ACTIVE: OnceLock<Sandbox> = OnceLock::new();

/// The sandbox, if it is on.
pub fn active() -> Option<&'static Sandbox> {
    ACTIVE.get()
}

/// Takes `--sandbox` and `--no-sandbox` out of the command line.
pub fn parse_flag(args: Vec<String>) -> (Option<bool>, Vec<String>) {
    let mut flag = None;
    let rest = args
        .into_iter()
        .filter(|a| match a.as_str() {
            "--sandbox" => {
                flag = Some(true);
                false
            }
            "--no-sandbox" => {
                flag = Some(false);
                false
            }
            _ => true,
        })
        .collect();
    (flag, rest)
}

/// Turns the sandbox on if the flag, else `HARNESS_SANDBOX`, else the
/// config says so, and checks that it works here.
pub async fn init(flag: Option<bool>, config: &Config) -> Result<Option<&'static Sandbox>> {
    let env = std::env::var("HARNESS_SANDBOX").ok().filter(|v| !v.is_empty());
    let from_env = match env.as_deref() {
        None => None,
        Some("1" | "true" | "on" | "yes") => Some(true),
        Some("0" | "false" | "off" | "no") => Some(false),
        Some(other) => bail!("HARNESS_SANDBOX must be 1 or 0, not `{other}`"),
    };
    if flag.or(from_env) == Some(false) {
        return Ok(None);
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if !flag.or(from_env).or(config.enabled).unwrap_or(false) {
        return Ok(None);
    }
    let cwd = std::env::current_dir().context("finding the working directory")?;
    let mut sandbox = Sandbox::new(Backend::native()?, &cwd, home.as_deref(), config);
    sandbox.startup = sandbox.probe().await?;
    Ok(Some(ACTIVE.get_or_init(|| sandbox)))
}

impl Sandbox {
    fn new(backend: Backend, cwd: &Path, home: Option<&Path>, config: &Config) -> Self {
        let cwd = resolve(cwd);
        let mut rules = Vec::new();
        let mut add = |path: &Path, access| rules.push(Rule { path: resolve(path), access });

        add(&cwd, Access::Write);
        for dir in temp_dirs() {
            add(&dir, Access::Write);
        }
        for path in &config.allow_write {
            add(&expand_path(path, home, &cwd), Access::Write);
        }
        for (name, dir) in PROTECTED {
            let path = cwd.join(name);
            // `.git/...` only in a repository, so `git init` still works
            // elsewhere.
            if !path.parent().is_some_and(Path::is_dir) {
                continue;
            }
            for between in path.ancestors().skip(1).take_while(|a| *a != cwd) {
                add(between, Access::Pinned);
            }
            add(&path, Access::ReadOnly { dir });
        }
        for path in &config.deny_read {
            add(&expand_path(path, home, &cwd), Access::Hidden);
        }

        let mut unique: Vec<Rule> = Vec::new();
        for rule in rules {
            if !unique.contains(&rule) {
                unique.push(rule);
            }
        }
        unique.sort_by_key(|r| r.path.components().count());
        Self { backend, cwd, network: config.network, rules: unique, startup: Duration::ZERO }
    }

    /// Runs `true` in the sandbox, to fail at startup rather than on every
    /// command; returns how long it took.
    async fn probe(&self) -> Result<Duration> {
        let program = self.backend.program();
        let started = Instant::now();
        let output = match self.command("true", [""; 0]) {
            Ok((mut command, _guard)) => command.output().await,
            Err(e) => Err(e),
        };
        let output = match output {
            Ok(o) => o,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && self.backend == Backend::Bubblewrap => {
                bail!("the sandbox needs bubblewrap: install it (`apt install bubblewrap`, `dnf install bubblewrap`)")
            }
            Err(e) => return Err(e).with_context(|| format!("running {program}")),
        };
        if output.status.success() {
            return Ok(started.elapsed());
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let hint = match self.backend {
            Backend::Seatbelt => "",
            Backend::Bubblewrap => {
                " (bwrap needs unprivileged user namespaces; containers often lack them, and on \
                 Ubuntu 24.04+ AppArmor must allow bwrap)"
            }
        };
        bail!("{program} cannot start a sandbox here{hint}: {}", stderr.trim())
    }

    /// Builds `program args` to run in the sandbox. It starts a new session,
    /// so it has no terminal to read from or type into, and leads its own
    /// process group. Keep the [`Guard`] until the process has exited.
    pub fn command<S: AsRef<OsStr>>(
        &self,
        program: &str,
        args: impl IntoIterator<Item = S>,
    ) -> std::io::Result<(Command, Guard)> {
        // The sandbox program would report a missing command as the
        // command's own failure.
        let program = find_program(program).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, format!("{program} not found on PATH"))
        })?;
        let (sandbox_args, guard) = match self.backend {
            Backend::Seatbelt => (self.seatbelt_args(), Guard(false)),
            Backend::Bubblewrap => {
                let mut placeholders = PLACEHOLDERS.lock().unwrap_or_else(|e| e.into_inner());
                placeholders.running += 1;
                (self.bwrap_args(&mut placeholders.paths), Guard(true))
            }
        };
        let mut command = Command::new(self.backend.program());
        command.args(sandbox_args).arg("--").arg(program).args(args);
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Ok((command, guard))
    }

    fn seatbelt_args(&self) -> Vec<OsString> {
        let mut profile = format!("(version 1)\n(allow default)\n(deny file-write*)\n{SEATBELT_DEVICES}\n");
        // Paths go in as parameters, so they need no quoting.
        let mut params: Vec<OsString> = Vec::new();
        let mut param = |path: &Path| {
            let key = format!("P{}", params.len());
            let mut define = OsString::from(format!("{key}="));
            define.push(path);
            params.push(define);
            format!("(param \"{key}\")")
        };
        for rule in &self.rules {
            let p = param(&rule.path);
            profile.push_str(&match rule.access {
                Access::Write => format!("(allow file-read* file-write* (subpath {p}))\n"),
                Access::ReadOnly { .. } => format!("(allow file-read* (subpath {p}))\n(deny file-write* (subpath {p}))\n"),
                Access::Hidden => format!("(deny file-read* file-write* (subpath {p}))\n"),
                Access::Pinned => format!("(deny file-write* (literal {p}))\n"),
            });
        }
        if !self.network {
            profile.push_str(SEATBELT_LOOPBACK);
        }
        let mut args = vec![OsString::from("-p"), profile.into()];
        for define in params {
            args.push("-D".into());
            args.push(define);
        }
        args
    }

    /// bwrap's options. A read-only path that does not exist yet gets an
    /// empty stand-in, which bwrap creates on the host as a mount point;
    /// it is added to `placeholders` to be removed afterwards.
    fn bwrap_args(&self, placeholders: &mut Vec<PathBuf>) -> Vec<OsString> {
        let mut args: Vec<OsString> =
            ["--die-with-parent", "--unshare-pid", "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"]
                .map(OsString::from)
                .to_vec();
        if !self.network {
            args.push("--unshare-net".into());
        }
        let mut mount = |op: &str, from: &Path, to: &Path| {
            args.push(op.into());
            if !from.as_os_str().is_empty() {
                args.push(from.into());
            }
            args.push(to.into());
        };
        let none = Path::new("");
        for Rule { path, access } in &self.rules {
            let exists = path.symlink_metadata().ok();
            match (access, exists) {
                // A mount point cannot be renamed or removed.
                (Access::Write | Access::Pinned, Some(_)) => mount("--bind", path, path),
                (Access::ReadOnly { .. }, Some(_)) => mount("--ro-bind", path, path),
                (Access::ReadOnly { dir }, None) if path.parent().is_some_and(Path::is_dir) => {
                    if *dir {
                        mount("--tmpfs", none, path);
                        mount("--remount-ro", none, path);
                    } else {
                        mount("--ro-bind", Path::new("/dev/null"), path);
                    }
                    if !placeholders.contains(path) {
                        placeholders.push(path.clone());
                    }
                }
                (Access::Hidden, Some(meta)) if meta.is_dir() => {
                    mount("--tmpfs", none, path);
                    mount("--remount-ro", none, path);
                }
                (Access::Hidden, Some(_)) => mount("--ro-bind", Path::new("/dev/null"), path),
                _ => {}
            }
        }
        args.push("--chdir".into());
        args.push(self.cwd.clone().into());
        args
    }

    /// The deepest rule covering `path`, with symlinks resolved.
    fn access(&self, path: &Path) -> Option<Access> {
        let path = fold_case(&resolve(path));
        self.rules
            .iter()
            .rev()
            .filter(|r| r.access != Access::Pinned)
            .find(|r| path.starts_with(fold_case(&r.path)))
            .map(|r| r.access)
    }

    pub fn check_read(&self, path: &Path) -> Result<(), String> {
        match self.access(path) {
            Some(Access::Hidden) => Err(format!("{} is hidden by the sandbox (deny_read)", path.display())),
            _ => Ok(()),
        }
    }

    pub fn check_write(&self, path: &Path) -> Result<(), String> {
        match self.access(path) {
            Some(Access::Write) => Ok(()),
            Some(Access::ReadOnly { .. }) => Err(format!("{} is read-only in the sandbox", path.display())),
            _ => Err(format!(
                "{} is outside the directories the sandbox allows writing to: {}",
                path.display(),
                self.paths(Access::Write).join(", ")
            )),
        }
    }

    fn paths(&self, access: Access) -> Vec<String> {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        self.rules
            .iter()
            .filter(|r| std::mem::discriminant(&r.access) == std::mem::discriminant(&access))
            .map(|r| match home.as_deref().and_then(|h| r.path.strip_prefix(h).ok()) {
                Some(rest) => format!("~/{}", rest.display()),
                None => r.path.display().to_string(),
            })
            .collect()
    }

    /// What the sandbox allows, as label and value pairs.
    pub fn describe(&self) -> Vec<(&'static str, String)> {
        let backend = match self.backend {
            Backend::Seatbelt => "Seatbelt (sandbox-exec)",
            Backend::Bubblewrap => "bubblewrap (bwrap)",
        };
        let mut items = vec![
            ("backend", backend.to_string()),
            ("writable", self.paths(Access::Write).join(", ")),
            ("read-only", self.paths(Access::ReadOnly { dir: false }).join(", ")),
        ];
        let hidden = self.paths(Access::Hidden);
        if !hidden.is_empty() {
            items.push(("hidden", hidden.join(", ")));
        }
        items.push(("network", if self.network { "allowed" } else { "loopback only" }.into()));
        items
    }

    /// What the sandbox adds to each command, as measured at startup. Not
    /// in [`Sandbox::describe`], which the system prompt repeats.
    pub fn startup(&self) -> Duration {
        self.startup
    }

    /// A note for the system prompt.
    pub fn prompt_note(&self) -> String {
        let items: Vec<String> = self.describe().into_iter().skip(1).map(|(k, v)| format!("{k}: {v}")).collect();
        format!(
            "`bash` and `grep` run in an OS sandbox, and `read` and `write` follow the same rules ({}). \
             When something fails because of it (\"Operation not permitted\", \"Read-only file system\", \
             a network error), tell the user instead of working around it; they can loosen it in \
             the `sandbox` section of ~/.harness/settings.json.",
            items.join("; ")
        )
    }
}

/// Stand-ins bwrap created for missing read-only paths, and how many
/// sandboxed commands are running: a stand-in is only removed once none is,
/// since it protects every running command.
struct Placeholders {
    running: usize,
    paths: Vec<PathBuf>,
}

static PLACEHOLDERS: Mutex<Placeholders> = Mutex::new(Placeholders { running: 0, paths: Vec::new() });

/// Held while a sandboxed command runs.
pub struct Guard(bool);

impl Drop for Guard {
    fn drop(&mut self) {
        if !self.0 {
            return;
        }
        let mut placeholders = PLACEHOLDERS.lock().unwrap_or_else(|e| e.into_inner());
        placeholders.running -= 1;
        if placeholders.running == 0 {
            for path in std::mem::take(&mut placeholders.paths) {
                remove_if_empty(&path);
            }
        }
    }
}

/// Removes an empty file or directory: a stand-in nothing replaced.
fn remove_if_empty(path: &Path) {
    let Ok(meta) = path.symlink_metadata() else { return };
    if meta.is_dir() {
        let _ = std::fs::remove_dir(path);
    } else if meta.is_file() && meta.len() == 0 {
        let _ = std::fs::remove_file(path);
    }
}

/// Where temp files go: `/tmp`, `/var/tmp`, `$TMPDIR` and, on macOS, the
/// per-user temp and cache directories.
fn temp_dirs() -> Vec<PathBuf> {
    let mut dirs = vec![PathBuf::from("/tmp"), PathBuf::from("/var/tmp")];
    dirs.extend(std::env::var_os("TMPDIR").map(PathBuf::from));
    #[cfg(target_os = "macos")]
    dirs.extend([libc::_CS_DARWIN_USER_TEMP_DIR, libc::_CS_DARWIN_USER_CACHE_DIR].into_iter().filter_map(confstr));
    dirs.retain(|d| d.is_dir());
    dirs
}

#[cfg(target_os = "macos")]
fn confstr(name: libc::c_int) -> Option<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: confstr writes at most `buf.len()` bytes, NUL included.
    let len = unsafe { libc::confstr(name, buf.as_mut_ptr().cast(), buf.len()) };
    (len > 1 && len <= buf.len()).then(|| PathBuf::from(OsStr::from_bytes(&buf[..len - 1])))
}

/// A config path: `${VAR}` and a leading `~/` expanded, relative to `cwd`.
fn expand_path(path: &str, home: Option<&Path>, cwd: &Path) -> PathBuf {
    let path = expand_env(path);
    let path = match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => {
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    };
    cwd.join(path)
}

/// `path` made absolute with symlinks resolved as far as it exists: where
/// a write to it would land. `..` after a missing directory is resolved by
/// name, which is what the kernel will do once it exists.
pub fn resolve(path: &Path) -> PathBuf {
    resolve_within(path, 0)
}

fn resolve_within(path: &Path, depth: usize) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => out.push(component),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => {
                out.push(name);
                if let Ok(real) = out.canonicalize() {
                    out = real;
                } else if depth < 40
                    && let Ok(target) = std::fs::read_link(&out)
                {
                    // A dangling symlink: a write would create its target.
                    out.pop();
                    out = resolve_within(&out.join(target), depth + 1);
                }
            }
        }
    }
    out
}

/// macOS file systems ignore case by default, so a path that does not
/// exist yet is compared without it there.
fn fold_case(path: &Path) -> PathBuf {
    if cfg!(target_os = "macos") { PathBuf::from(path.to_string_lossy().to_lowercase()) } else { path.to_path_buf() }
}

/// `name` looked up on `PATH`, unless it is a path already.
fn find_program(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if name.contains('/') {
        return Some(PathBuf::from(name));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(json: &str) -> Config {
        serde_json::from_str(json).unwrap()
    }

    fn sandbox(backend: Backend, cwd: &Path, json: &str) -> Sandbox {
        Sandbox::new(backend, cwd, Some(Path::new("/no-such-home/u")), &config(json))
    }

    /// A working directory that is a git repository with hooks.
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".git/hooks")).unwrap();
        std::fs::write(dir.path().join(".git/config"), "[core]\n").unwrap();
        dir
    }

    #[test]
    fn parses_flags() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_flag(args(&["--sandbox", "hi"])), (Some(true), args(&["hi"])));
        assert_eq!(parse_flag(args(&["-c", "--no-sandbox"])), (Some(false), args(&["-c"])));
        assert_eq!(parse_flag(args(&["hi"])), (None, args(&["hi"])));
    }

    #[test]
    fn rejects_unknown_config_fields() {
        assert!(serde_json::from_str::<Config>(r#"{"enabled": true, "net": true}"#).is_err());
    }

    #[test]
    fn expands_config_paths() {
        let (home, cwd) = (Some(Path::new("/home/u")), Path::new("/w"));
        assert_eq!(expand_path("~/.cargo", home, cwd), Path::new("/home/u/.cargo"));
        assert_eq!(expand_path("~", home, cwd), Path::new("/home/u"));
        assert_eq!(expand_path("~other", home, cwd), Path::new("/w/~other"));
        assert_eq!(expand_path("build", home, cwd), Path::new("/w/build"));
        assert_eq!(expand_path("/opt/x", home, cwd), Path::new("/opt/x"));
    }

    #[test]
    fn resolves_symlinks_and_missing_parts() {
        let dir = tempfile::tempdir().unwrap();
        let real = resolve(dir.path());
        std::os::unix::fs::symlink(real.join("target"), real.join("link")).unwrap();
        assert_eq!(resolve(&real.join("a/../b/./c")), real.join("b/c"));
        // A dangling link leads to where a write would go.
        assert_eq!(resolve(&real.join("link")), real.join("target"));
        std::fs::create_dir(real.join("target")).unwrap();
        assert_eq!(resolve(&real.join("link/x")), real.join("target/x"));
    }

    #[test]
    fn protects_config_and_git_hooks() {
        let dir = repo();
        let cwd = resolve(dir.path());
        let s = sandbox(Backend::Seatbelt, &cwd, "{}");
        assert_eq!(s.check_write(&cwd.join("src/main.rs")), Ok(()));
        assert_eq!(s.check_write(&cwd.join(".git/objects/ab")), Ok(()));
        for path in [".harness/settings.json", ".mcp.json", ".claude/settings.json", ".git/hooks/pre-commit", ".git/config"] {
            let err = s.check_write(&cwd.join(path)).unwrap_err();
            assert!(err.contains("read-only"), "{path}: {err}");
        }
        assert!(s.check_write(Path::new("/etc/hosts")).unwrap_err().contains("outside"));
        assert!(s.check_write(&cwd.join("missing/../.mcp.json")).unwrap_err().contains("read-only"));
        assert!(s.check_write(Path::new("/tmp/../etc/escape")).is_err());
        assert!(s.rules.contains(&Rule { path: cwd.join(".git"), access: Access::Pinned }));
    }

    #[test]
    fn git_paths_are_only_protected_in_a_repository() {
        let dir = tempfile::tempdir().unwrap();
        let s = sandbox(Backend::Seatbelt, dir.path(), "{}");
        assert_eq!(s.check_write(&dir.path().join(".git/config")), Ok(()));
        assert!(s.check_write(&dir.path().join(".mcp.json")).is_err());
    }

    #[test]
    fn deepest_rule_wins() {
        let dir = repo();
        let cwd = resolve(dir.path());
        let json = format!(r#"{{"deny_read": ["{0}", "{0}/work/.env"], "allow_write": ["{0}/work"]}}"#, cwd.display());
        std::fs::create_dir(cwd.join("work")).unwrap();
        let s = sandbox(Backend::Seatbelt, &cwd.join("work"), &json);
        assert!(s.check_read(&cwd.join("secret")).is_err());
        assert_eq!(s.check_read(&cwd.join("work/main.rs")), Ok(()));
        assert!(s.check_read(&cwd.join("work/.env")).is_err());
        assert!(s.check_write(&cwd.join("secret")).is_err());
        let depths: Vec<usize> = s.rules.iter().map(|r| r.path.components().count()).collect();
        assert!(depths.is_sorted(), "{:?}", s.rules);
    }

    #[test]
    fn seatbelt_profile_passes_paths_as_parameters() {
        let dir = repo();
        let cwd = resolve(dir.path());
        let s = sandbox(Backend::Seatbelt, &cwd, r#"{"deny_read": ["~/.ssh"]}"#);
        let args = s.seatbelt_args();
        let profile = args[1].to_str().unwrap();
        assert!(profile.contains("(deny file-write*)\n"), "{profile}");
        assert!(profile.contains("(deny network*)"), "{profile}");
        assert!(!profile.contains(cwd.to_str().unwrap()), "paths only in -D: {profile}");
        assert!(args[2..].iter().step_by(2).all(|a| a == "-D"), "{args:?}");
        let defines: Vec<String> = args[3..].iter().step_by(2).map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(defines.contains(&format!("P0={}", cwd.display())) || defines.iter().any(|d| d.ends_with(&format!("={}", cwd.display()))), "{defines:?}");
        assert!(defines.iter().any(|d| d.ends_with("=/no-such-home/u/.ssh")), "{defines:?}");

        let open = sandbox(Backend::Seatbelt, &cwd, r#"{"network": true}"#);
        assert!(!open.seatbelt_args()[1].to_str().unwrap().contains("network"));
    }

    #[test]
    fn bwrap_mounts_in_order_with_placeholders() {
        let dir = repo();
        let cwd = resolve(dir.path());
        std::fs::write(cwd.join(".env"), "KEY=1").unwrap();
        let s = sandbox(Backend::Bubblewrap, &cwd, r#"{"deny_read": [".env", "missing"]}"#);
        let mut placeholders = Vec::new();
        let args: Vec<String> = s.bwrap_args(&mut placeholders).iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let args = args.join(" ");
        let c = cwd.display();
        assert!(args.starts_with("--die-with-parent --unshare-pid --ro-bind / / --dev /dev --proc /proc --unshare-net"));
        let order = [
            format!("--bind {c} {c}"),
            format!("--bind {c}/.git {c}/.git"),
            format!("--tmpfs {c}/.harness --remount-ro {c}/.harness"),
            format!("--ro-bind /dev/null {c}/.mcp.json"),
            format!("--ro-bind /dev/null {c}/.env"),
            format!("--ro-bind {c}/.git/hooks {c}/.git/hooks"),
            format!("--ro-bind {c}/.git/config {c}/.git/config"),
        ];
        let at: Vec<usize> = order.iter().map(|m| args.find(m.as_str()).unwrap_or_else(|| panic!("{m} in {args}"))).collect();
        assert!(at[0] < at[1] && at[1] < at[5], "{args}");
        assert!(!args.contains("missing"), "{args}");
        assert!(args.ends_with(&format!("--chdir {c}")), "{args}");
        assert_eq!(placeholders, [cwd.join(".harness"), cwd.join(".claude"), cwd.join(".mcp.json")]);
    }

    #[test]
    fn removes_only_empty_placeholders() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir(p.join("empty")).unwrap();
        std::fs::create_dir_all(p.join("full/x")).unwrap();
        std::fs::write(p.join("blank.json"), "").unwrap();
        std::fs::write(p.join("real.json"), "{}").unwrap();
        for name in ["empty", "full", "blank.json", "real.json", "absent"] {
            remove_if_empty(&p.join(name));
        }
        assert!(!p.join("empty").exists() && !p.join("blank.json").exists());
        assert!(p.join("full/x").exists() && p.join("real.json").exists());
    }

    /// Runs `script` with bash in a Seatbelt sandbox rooted at `cwd`.
    #[cfg(target_os = "macos")]
    async fn run_seatbelt(s: &Sandbox, script: &str) -> std::process::Output {
        let (mut command, _guard) = s.command("bash", ["-c", script]).unwrap();
        command.current_dir(&s.cwd).output().await.unwrap()
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn seatbelt_confines_commands() {
        let dir = repo();
        let cwd = resolve(dir.path());
        let s = sandbox(Backend::Seatbelt, &cwd, r#"{"deny_read": ["secret"]}"#);
        std::fs::write(cwd.join("secret"), "hush").unwrap();
        s.probe().await.unwrap();

        let out = run_seatbelt(&s, "echo ok > inside && cat inside && echo tmp > \"$TMPDIR/harness-sb\" && echo fine > /dev/null").await;
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, b"ok\n");
        for script in [
            "echo x > .mcp.json",
            "mkdir -p .harness",
            "echo x > .git/hooks/pre-commit",
            "echo x >> .git/config",
            "mv .git .g",
            "ln .git/config linked",
            "cat secret",
            "echo x > \"$HOME/.harness-sandbox-test\"",
            "exec 3<>/dev/tcp/1.1.1.1/80",
        ] {
            let out = run_seatbelt(&s, script).await;
            assert!(!out.status.success(), "`{script}` was allowed");
        }
        assert!(!cwd.join(".mcp.json").exists());
        assert!(!Path::new(&std::env::var("HOME").unwrap()).join(".harness-sandbox-test").exists());
        // It leads its own process group, which a timeout kills.
        let out = run_seatbelt(&s, "exec perl -e 'print \"$$ \", getpgrp()'").await;
        let text = String::from_utf8_lossy(&out.stdout);
        let ids: Vec<&str> = text.split_whitespace().collect();
        assert_eq!(ids.len(), 2, "{text}");
        assert_eq!(ids[0], ids[1], "{text}");
    }

    #[cfg(target_os = "linux")]
    async fn run_bwrap(s: &Sandbox, script: &str) -> std::process::Output {
        let (mut command, _guard) = s.command("bash", ["-c", script]).unwrap();
        command.output().await.unwrap()
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn bwrap_confines_commands() {
        if find_program("bwrap").is_none() {
            eprintln!("bwrap not installed; skipping");
            return;
        }
        let dir = repo();
        let cwd = resolve(dir.path());
        let s = sandbox(Backend::Bubblewrap, &cwd, "{}");
        if let Err(e) = s.probe().await {
            eprintln!("{e:#}; skipping");
            return;
        }
        let out = run_bwrap(&s, "echo ok > inside && cat inside").await;
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        for script in ["echo x > .mcp.json", "echo x > .harness/x", "echo x > .git/hooks/pre-commit", "mv .git .g", "exec 3<>/dev/tcp/1.1.1.1/80"] {
            assert!(!run_bwrap(&s, script).await.status.success(), "`{script}` was allowed");
        }
        // The stand-ins are gone once nothing runs.
        assert!(!cwd.join(".mcp.json").exists() && !cwd.join(".harness").exists());
    }
}
