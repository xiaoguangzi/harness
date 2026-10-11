//! Resource guards shared by conversations and their subagents. Unknown
//! mutations own the workspace; named resources and file edits can coexist.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use tokio::sync::{OwnedMutexGuard, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

pub enum Access {
    Concurrent,
    Workspace,
    Resource(String),
    File(String),
}

#[derive(Default)]
pub struct Scheduler {
    workspace: Arc<RwLock<()>>,
    resources: Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>,
}

/// Kept until execution ends, including after a command moves into the background.
pub struct Guard {
    _workspace: Option<OwnedRwLockWriteGuard<()>>,
    _shared: Option<OwnedRwLockReadGuard<()>>,
    _resource: Option<OwnedMutexGuard<()>>,
}

impl Scheduler {
    pub async fn acquire(&self, access: Access) -> Guard {
        let (mut workspace, mut shared, mut resource) = (None, None, None);
        let file = matches!(&access, Access::File(_));
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
        Guard { _workspace: workspace, _shared: shared, _resource: resource }
    }
}

pub async fn acquire(access: Access) -> Guard {
    static SCHEDULER: OnceLock<Scheduler> = OnceLock::new();
    SCHEDULER.get_or_init(Scheduler::default).acquire(access).await
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
}
