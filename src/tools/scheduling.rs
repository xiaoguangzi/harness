//! Resource guards shared by conversations and their subagents. Unknown
//! mutations own the workspace; named resources and file edits can coexist.

use std::collections::HashMap;
use std::path::Path;
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
    workspace: Arc<RwLock<()>>,
    resources: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
    /// The background commands that hold a guard.
    background: Arc<Mutex<Vec<Holder>>>,
    held: std::sync::atomic::AtomicU64,
}

/// A background command holding a guard.
struct Holder {
    id: u64,
    /// The resource it holds; `None` for the workspace.
    resource: Option<String>,
    /// How its conversation knows it: `shell_1` and its command.
    label: String,
}

/// Kept until execution ends, including after a command moves into the background.
pub struct Guard {
    _workspace: Option<OwnedRwLockWriteGuard<()>>,
    _shared: Option<OwnedRwLockReadGuard<()>>,
    _resource: Option<OwnedMutexGuard<()>>,
    /// The resource it holds for itself, or the workspace (`Some(None)`).
    holds: Option<Option<String>>,
    /// Where it is listed once a background command holds it.
    background: Option<(Arc<Mutex<Vec<Holder>>>, u64)>,
    scheduler: (Arc<Mutex<Vec<Holder>>>, u64),
}

impl Guard {
    /// Notes that the background command `label` holds this from now on, so
    /// calls that wait for it can say what they wait for.
    pub fn held_in_background(&mut self, label: String) {
        let (Some(resource), None) = (self.holds.clone(), &self.background) else { return };
        let (list, id) = self.scheduler.clone();
        list.lock().unwrap().push(Holder { id, resource, label });
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

impl Scheduler {
    pub async fn acquire(&self, access: Access) -> Guard {
        let (mut workspace, mut shared, mut resource) = (None, None, None);
        let file = matches!(&access, Access::File(_));
        let holds = match &access {
            Access::Workspace => Some(None),
            Access::Resource(key) => Some(Some(key.clone())),
            Access::Concurrent | Access::File(_) => None,
        };
        match access {
            Access::Concurrent => {}
            Access::Workspace => workspace = Some(self.workspace.clone().write_owned().await),
            Access::Resource(key) | Access::File(key) => {
                if file { shared = Some(self.workspace.clone().read_owned().await); }
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
        let wanted = match &access {
            Access::Concurrent => return Ok(self.acquire(access).await),
            Access::Resource(key) => Some(key.clone()),
            Access::Workspace | Access::File(_) => None,
        };
        let mut acquiring = std::pin::pin!(self.acquire(access));
        loop {
            if let Ok(guard) = tokio::time::timeout(patience, &mut acquiring).await {
                return Ok(guard);
            }
            let holders = self.background.lock().unwrap();
            let held: Vec<&str> = holders.iter().filter(|h| h.resource == wanted).map(|h| h.label.as_str()).collect();
            if !held.is_empty() {
                let what = wanted.map_or("the workspace".to_string(), |key| format!("resource `{key}`"));
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

pub fn file_access(input: &serde_json::Value) -> Access {
    let Some(path) = input["path"].as_str() else { return Access::Workspace };
    let path = Path::new(path);
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
