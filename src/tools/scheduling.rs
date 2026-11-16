//! Resource guards shared by conversations and their subagents. Unknown
//! mutations own the workspace; named resources and file edits can coexist.
//! Each working directory is a workspace of its own: a subagent in a git
//! worktree changes its checkout beside the main agent and other worktrees.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

pub enum Access {
    Concurrent,
    Workspace,
    Resource(String),
    File(String),
}

/// How long a call waits for something a background command holds before
/// it gives up and says so. A foreground call that holds it ends by itself
/// (it has a timeout), so those are waited out; a background command may
/// never end, such as a dev server started without a resource of its own.
const BACKGROUND_PATIENCE: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct Scheduler {
    /// By working directory.
    workspaces: Mutex<HashMap<PathBuf, Weak<RwLock<()>>>>,
    resources: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    /// The background commands that hold a guard.
    background: Arc<Mutex<Vec<Holder>>>,
    held: std::sync::atomic::AtomicU64,
}

/// What a guard holds for itself, and a background command may keep.
#[derive(Clone, PartialEq)]
enum Held {
    /// The workspace of this working directory.
    Workspace(PathBuf),
    Resource(String),
}

/// A background command holding a guard.
struct Holder {
    id: u64,
    held: Held,
    /// How its conversation knows it: `shell_1` and its command.
    label: String,
}

/// Kept until execution ends, including after a command moves into the background.
pub struct Guard {
    _workspace: Option<OwnedRwLockWriteGuard<()>>,
    _shared: Vec<OwnedRwLockReadGuard<()>>,
    _resource: Option<OwnedMutexGuard<()>>,
    holds: Option<Held>,
    /// Where it is listed once a background command holds it.
    background: Option<(Arc<Mutex<Vec<Holder>>>, u64)>,
    scheduler: (Arc<Mutex<Vec<Holder>>>, u64),
}

impl Guard {
    /// Notes that the background command `label` holds this from now on, so
    /// calls that wait for it can say what they wait for.
    pub fn held_in_background(&mut self, label: String) {
        let (Some(held), None) = (self.holds.clone(), &self.background) else { return };
        let (list, id) = self.scheduler.clone();
        list.lock().unwrap().push(Holder { id, held, label });
        self.background = Some((list, id));
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some((list, id)) = self.background.take() {
            list.lock().unwrap().retain(|holder| holder.id != id);
        }
    }
}

/// The workspaces a change of the file `key` (`file:<path>`) keeps others
/// out of, in the order to take them: the call's own, and the one of the
/// directory the harness started in if the file is in it, for a worktree
/// subagent changing a file of the main checkout.
fn file_workspaces(key: &str, here: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![here.to_path_buf()];
    let file = Path::new(key.strip_prefix("file:").unwrap_or(key));
    if let Ok(start) = std::env::current_dir()
        && start != here
        && (file.starts_with(&start) || start.canonicalize().is_ok_and(|s| file.starts_with(s)))
    {
        dirs.push(start);
    }
    // One order for everyone, so two changes never wait for each other.
    dirs.sort();
    dirs
}

impl Scheduler {
    /// The lock of the workspace of working directory `dir`.
    fn workspace(&self, dir: &Path) -> Arc<RwLock<()>> {
        let mut workspaces = self.workspaces.lock().unwrap();
        workspaces.retain(|_, lock| lock.strong_count() > 0);
        let lock = workspaces.get(dir).and_then(Weak::upgrade).unwrap_or_default();
        workspaces.insert(dir.to_path_buf(), Arc::downgrade(&lock));
        lock
    }

    /// Waits for `access` in the working directory the calling task works in.
    pub async fn acquire(&self, access: Access) -> Guard {
        let here = super::workdir::get();
        let (mut workspace, mut shared, mut resource) = (None, Vec::new(), None);
        let holds = match &access {
            Access::Workspace => Some(Held::Workspace(here.clone())),
            Access::Resource(key) => Some(Held::Resource(key.clone())),
            Access::Concurrent | Access::File(_) => None,
        };
        match access {
            Access::Concurrent => {}
            Access::Workspace => workspace = Some(self.workspace(&here).write_owned().await),
            Access::Resource(key) | Access::File(key) => {
                if key.starts_with("file:") {
                    for dir in file_workspaces(&key, &here) {
                        shared.push(self.workspace(&dir).read_owned().await);
                    }
                }
                let lock = {
                    let mut resources = self.resources.lock().unwrap();
                    resources.retain(|_, lock| lock.strong_count() > 0);
                    let lock = resources.get(&key).and_then(Weak::upgrade).unwrap_or_default();
                    resources.insert(key, Arc::downgrade(&lock));
                    lock
                };
                resource = Some(lock.lock_owned().await);
            }
        }
        let id = self.held.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Guard {
            _workspace: workspace,
            _shared: shared,
            _resource: resource,
            holds,
            background: None,
            scheduler: (self.background.clone(), id),
        }
    }

    /// [`Scheduler::acquire`], but a call that has waited `patience` for
    /// something a background command holds is refused, with what holds it
    /// and what to do: such a command may run for as long as it likes.
    pub async fn acquire_within(&self, access: Access, patience: Duration) -> Result<Guard, String> {
        // A file change waits for the workspace, as a workspace command does.
        let here = super::workdir::get();
        let wanted: Vec<Held> = match &access {
            Access::Concurrent => return Ok(self.acquire(access).await),
            Access::Resource(key) => vec![Held::Resource(key.clone())],
            Access::Workspace => vec![Held::Workspace(here)],
            Access::File(key) => file_workspaces(key, &here).into_iter().map(Held::Workspace).collect(),
        };
        let mut acquiring = std::pin::pin!(self.acquire(access));
        loop {
            if let Ok(guard) = tokio::time::timeout(patience, &mut acquiring).await {
                return Ok(guard);
            }
            let holders = self.background.lock().unwrap();
            let holding: Vec<&Holder> = holders.iter().filter(|h| wanted.contains(&h.held)).collect();
            if let Some(first) = holding.first() {
                let held: Vec<&str> = holding.iter().map(|h| h.label.as_str()).collect();
                let what = match &first.held {
                    Held::Workspace(_) => "the workspace".to_string(),
                    Held::Resource(key) => format!("resource `{key}`"),
                };
                return Err(format!(
                    "Not run: {what} is held by background command {}, which has not finished after {} s of \
                     waiting. If it is yours and you need its result, wait for it with wait_tasks; if it should \
                     not be running, stop it with kill_shell. A command that keeps running (a dev server, a \
                     watcher) must be started with background_kind service and a resource of its own, or it \
                     holds the workspace for as long as it runs. If it is not yours, do what does not need \
                     {what}, or say that you are blocked.",
                    held.join(", "),
                    patience.as_secs(),
                ));
            }
        }
    }
}

fn scheduler() -> &'static Scheduler {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::default)
}

/// Waits for `access` for as long as calls that will end hold it; `Err` is
/// what the model is told when a background command holds it instead (see
/// [`Scheduler::acquire_within`]).
pub async fn acquire(access: Access) -> Result<Guard, String> {
    scheduler().acquire_within(access, BACKGROUND_PATIENCE).await
}

/// A change of the file `input` names, the path resolved against the
/// working directory the calling task works in.
pub fn file_access(input: &serde_json::Value) -> Access {
    let Some(path) = input["path"].as_str() else { return Access::Workspace };
    let resolved = super::workdir::resolve(path);
    let path = Path::new(&resolved);
    let canonical = path.canonicalize().or_else(|_| {
        let absolute = std::path::absolute(path)?;
        let parent = absolute.parent().unwrap_or(Path::new("/"));
        Ok::<_, std::io::Error>(parent.canonicalize()?.join(absolute.file_name().unwrap_or_default()))
    }).unwrap_or_else(|_| path.to_path_buf());
    Access::File(format!("file:{}", canonical.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn conflicts_wait_and_independent_resources_continue() {
        let scheduler = Scheduler::default();
        let workspace = scheduler.acquire(Access::Workspace).await;
        let independent = scheduler.acquire(Access::Resource("host:other".into())).await;
        let blocked = tokio::time::timeout(Duration::from_millis(20), scheduler.acquire(Access::File("file:a".into()))).await;
        assert!(blocked.is_err());
        assert!(tokio::time::timeout(Duration::from_millis(20), scheduler.acquire(Access::Resource("host:other".into()))).await.is_err());
        drop(workspace);
        let a = scheduler.acquire(Access::File("file:a".into())).await;
        let b = scheduler.acquire(Access::File("file:b".into())).await;
        assert!(tokio::time::timeout(Duration::from_millis(20), scheduler.acquire(Access::Workspace)).await.is_err());
        drop((a, b, independent));
        scheduler.acquire(Access::Workspace).await;
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn each_working_directory_is_a_workspace_of_its_own() {
        use crate::tools::workdir::scope;
        // The directory the harness started in is read here.
        let _guard = crate::util::lock_cwd();
        let scheduler = Scheduler::default();
        let (one, two) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (one, two) = (one.path().to_path_buf(), two.path().to_path_buf());
        let short = Duration::from_millis(20);
        let main = scheduler.acquire(Access::Workspace).await;
        let a = scope(Some(one.clone()), scheduler.acquire(Access::Workspace)).await;
        let b = scope(Some(two.clone()), scheduler.acquire(Access::Workspace)).await;
        let again = scope(Some(one.clone()), tokio::time::timeout(short, scheduler.acquire(Access::Workspace)));
        assert!(again.await.is_err(), "the same worktree still waits");
        drop((a, b));
        // A worktree subagent changing a file of the main checkout waits for the main agent's command.
        let main_file = format!("file:{}", std::env::current_dir().unwrap().join("src/x.rs").display());
        let file = scope(Some(one.clone()), tokio::time::timeout(short, scheduler.acquire(Access::File(main_file.clone()))));
        assert!(file.await.is_err());
        let own_file = format!("file:{}", one.join("x.rs").display());
        scope(Some(one.clone()), scheduler.acquire(Access::File(own_file))).await;
        drop(main);
        scope(Some(one), scheduler.acquire(Access::File(main_file))).await;
    }

    #[tokio::test]
    async fn a_call_is_told_what_background_command_holds_it_up() {
        let scheduler = Scheduler::default();
        let patience = Duration::from_millis(20);
        // A foreground call holds the workspace: waited out, however long.
        let foreground = scheduler.acquire(Access::Workspace).await;
        let waiting = scheduler.acquire_within(Access::File("file:a".into()), patience);
        let (waited, _) = tokio::join!(waiting, async {
            tokio::time::sleep(patience * 3).await;
            drop(foreground);
        });
        drop(waited.unwrap());

        let mut server = scheduler.acquire(Access::Workspace).await;
        server.held_in_background("shell_1 (`npm run dev`)".into());
        let mut deploy = scheduler.acquire(Access::Resource("host:prod".into())).await;
        deploy.held_in_background("shell_2 (`./deploy`)".into());
        for access in [Access::Workspace, Access::File("file:a".into())] {
            let refused = scheduler.acquire_within(access, patience).await.err().unwrap();
            assert!(refused.contains("the workspace is held by background command shell_1 (`npm run dev`)"), "{refused}");
            assert!(refused.contains("kill_shell") && !refused.contains("shell_2"), "{refused}");
        }
        let refused = scheduler.acquire_within(Access::Resource("host:prod".into()), patience).await.err().unwrap();
        assert!(refused.contains("resource `host:prod` is held by background command shell_2"), "{refused}");
        scheduler.acquire_within(Access::Resource("host:other".into()), patience).await.unwrap();
        scheduler.acquire_within(Access::Concurrent, patience).await.unwrap();

        // Once the command ends, nothing is in the way, nor listed.
        drop(server);
        scheduler.acquire_within(Access::Workspace, patience).await.unwrap();
        assert_eq!(scheduler.background.lock().unwrap().len(), 1);
    }
}
