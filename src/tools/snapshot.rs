//! The environment of the user's login shell, captured once, for the
//! commands the model runs.
//!
//! `bash -c` reads no startup files, so what `~/.bashrc`, `~/.zshrc` or
//! `~/.profile` set up (`PATH` additions from nvm, pyenv, conda, Homebrew,
//! `JAVA_HOME`, ...) would be missing, unless the harness was started from
//! a shell that has it. At startup, `$SHELL -l -i -c 'env -0'` runs once in
//! the background, in the home directory (so the project cannot change what
//! it finds) and in a session of its own without a terminal (so the startup
//! files cannot touch the REPL's); the variables it ends up with that
//! differ from the harness's own are set for every command. Commands wait
//! for it, at most [`TIMEOUT`]; if it fails, they run with the harness's
//! environment, and the first one says why.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::sync::OnceCell;

const TIMEOUT: Duration = Duration::from_secs(5);
/// Printed before the environment, past whatever the startup files print.
const MARKER: &[u8] = b"\0__HARNESS_ENV__\0";
const SCRIPT: &str = r"printf '\0__HARNESS_ENV__\0'; env -0";

/// Variables that describe the shell rather than the environment.
const SKIPPED: [&str; 14] = [
    "_", "PWD", "OLDPWD", "SHLVL", "PS1", "PS2", "PS3", "PS4", "PROMPT_COMMAND", "TERM", "COLUMNS", "LINES",
    "HISTFILE", "SHELL_SESSION_ID",
];

struct Snapshot {
    /// What to set in the commands' environment.
    vars: Vec<(OsString, OsString)>,
    /// Why there is none.
    failure: Option<String>,
}

static SHELL: OnceLock<Option<PathBuf>> = OnceLock::new();
static SNAPSHOT: OnceCell<Snapshot> = OnceCell::const_new();
static FAILURE_SHOWN: AtomicBool = AtomicBool::new(false);

/// Starts capturing the login shell's environment in the background, if
/// `enabled`. Call once, at startup; without it commands get none.
pub fn start(enabled: bool) {
    let shell = enabled.then(|| std::env::var_os("SHELL").filter(|s| !s.is_empty()).map_or("bash".into(), PathBuf::from));
    if SHELL.set(shell).is_ok() && SHELL.get().is_some_and(Option::is_some) {
        tokio::spawn(wait());
    }
}

/// Waits for the capture [`start`] began, if it did.
pub async fn wait() {
    if let Some(Some(shell)) = SHELL.get() {
        SNAPSHOT.get_or_init(|| capture(shell.clone())).await;
    }
}

/// The variables to set in the commands' environment; none until [`wait`]
/// returned.
pub fn vars() -> &'static [(OsString, OsString)] {
    SNAPSHOT.get().map_or(&[], |s| &s.vars)
}

/// Why the capture failed, the first time this is asked after it did.
pub fn take_failure() -> Option<&'static str> {
    let failure = SNAPSHOT.get()?.failure.as_deref()?;
    (!FAILURE_SHOWN.swap(true, Ordering::Relaxed)).then_some(failure)
}

async fn capture(shell: PathBuf) -> Snapshot {
    match run(&shell).await {
        Ok(found) => Snapshot { vars: changes(&found, |name| std::env::var_os(name)), failure: None },
        Err(e) => Snapshot {
            vars: Vec::new(),
            failure: Some(format!(
                "[the environment of your login shell ({}) could not be loaded: {e}; commands run with the harness's]",
                shell.display()
            )),
        },
    }
}

async fn run(shell: &std::path::Path) -> Result<Vec<(OsString, OsString)>, String> {
    let mut cmd = Command::new(shell);
    cmd.args(["-l", "-i", "-c", SCRIPT]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    cmd.kill_on_drop(true);
    if let Some(home) = std::env::var_os("HOME") {
        cmd.current_dir(home);
    }
    // SAFETY: setsid is async-signal-safe and touches no memory.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("starting it failed: {e}"))?;
    // On a timeout, the shell and what its startup files started go.
    let mut group = super::KillGroup(child.id());
    let mut stdout = child.stdout.take().expect("piped");
    let mut out = Vec::new();
    let status = {
        let mut reading = pin!(stdout.read_to_end(&mut out));
        let mut read_done = false;
        let waited = tokio::time::timeout(TIMEOUT, async {
            loop {
                tokio::select! {
                    _ = &mut reading, if !read_done => read_done = true,
                    status = child.wait() => break status,
                }
            }
        })
        .await;
        let Ok(status) = waited else {
            return Err(format!("its startup files took longer than {}s", TIMEOUT.as_secs()));
        };
        // What the startup files left running (an ssh-agent, say) stays,
        // as in a terminal, and may hold the pipe open.
        group.0 = None;
        if !read_done {
            let _ = tokio::time::timeout(super::bash::PIPE_GRACE, reading).await;
        }
        status.map_err(|e| e.to_string())?
    };
    parse(&out).ok_or_else(|| match status.code() {
        Some(0) | None => "it printed no environment (do its startup files `exec` another program?)".into(),
        Some(code) => format!("it exited with code {code}"),
    })
}

/// The `NAME=value` entries after [`MARKER`].
fn parse(stdout: &[u8]) -> Option<Vec<(OsString, OsString)>> {
    let at = stdout.windows(MARKER.len()).position(|w| w == MARKER)?;
    let entries = stdout[at + MARKER.len()..].split(|&b| b == 0).filter(|e| !e.is_empty());
    let pairs = entries.filter_map(|entry| {
        let eq = entry.iter().position(|&b| b == b'=')?;
        let name = std::ffi::OsStr::from_bytes(&entry[..eq]).to_os_string();
        Some((name, std::ffi::OsStr::from_bytes(&entry[eq + 1..]).to_os_string()))
    });
    Some(pairs.collect())
}

/// The variables in `found` whose value differs from `current`'s.
fn changes(found: &[(OsString, OsString)], current: impl Fn(&OsString) -> Option<OsString>) -> Vec<(OsString, OsString)> {
    let found = found.iter().filter(|(name, _)| !SKIPPED.iter().any(|s| name == s));
    found.filter(|(name, value)| current(name).as_ref() != Some(value)).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_past_what_startup_files_print() {
        let mut out = b"Welcome!\n\0junk".to_vec();
        out.extend_from_slice(MARKER);
        out.extend_from_slice(b"PATH=/a:/b\0MULTI=x\ny\0EMPTY=\0=bad\0");
        let vars = parse(&out).unwrap();
        let vars: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.to_str().unwrap(), v.to_str().unwrap())).collect();
        assert_eq!(vars, [("PATH", "/a:/b"), ("MULTI", "x\ny"), ("EMPTY", ""), ("", "bad")]);
        assert!(parse(b"PATH=/a\0").is_none(), "no marker, no environment");
    }

    #[test]
    fn keeps_what_differs_but_the_shells_own() {
        let found: Vec<(OsString, OsString)> =
            [("PATH", "/nvm/bin:/usr/bin"), ("HOME", "/h"), ("PWD", "/h"), ("NVM_DIR", "/nvm")].map(|(k, v)| (k.into(), v.into())).to_vec();
        let current = |name: &OsString| match name.to_str() {
            Some("PATH") => Some("/usr/bin".into()),
            Some("HOME") => Some("/h".into()),
            _ => None,
        };
        let kept = changes(&found, current);
        let names: Vec<&str> = kept.iter().map(|(k, _)| k.to_str().unwrap()).collect();
        assert_eq!(names, ["PATH", "NVM_DIR"]);
    }

    #[tokio::test]
    async fn captures_a_login_shell() {
        let dir = tempfile::tempdir().unwrap();
        // A "shell" whose startup sets a variable and prints a banner.
        let shell = dir.path().join("sh");
        std::fs::write(&shell, "#!/bin/bash\necho banner\nexport FROM_RC=1\nexec bash -c \"$4\"\n").unwrap();
        std::fs::set_permissions(&shell, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let snapshot = capture(shell.clone()).await;
        assert!(snapshot.failure.is_none(), "{:?}", snapshot.failure);
        assert!(snapshot.vars.iter().any(|(k, v)| k == "FROM_RC" && v == "1"));

        std::fs::write(&shell, "#!/bin/bash\nexit 3\n").unwrap();
        let failed = capture(shell).await;
        assert!(failed.vars.is_empty() && failed.failure.unwrap().contains("exited with code 3"));
    }
}
