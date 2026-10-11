//! Waiting belongs to the runtime, not to repeated model requests.

use std::future::Future;
use tokio::sync::mpsc::UnboundedSender;

pub type Event = (String, Vec<String>);

tokio::task_local! {
    static CURRENT: (String, UnboundedSender<Event>);
}

pub async fn scope<F: Future>(id: String, tx: UnboundedSender<Event>, future: F) -> F::Output {
    CURRENT.scope((id, tx), future).await
}

pub struct Waiting(Option<(String, UnboundedSender<Event>)>);

pub fn waiting(tasks: Vec<String>) -> Waiting {
    let context = CURRENT.try_with(Clone::clone).ok();
    if let Some((id, tx)) = &context { let _ = tx.send((id.clone(), tasks)); }
    Waiting(context)
}

impl Drop for Waiting {
    fn drop(&mut self) {
        if let Some((id, tx)) = &self.0 { let _ = tx.send((id.clone(), Vec::new())); }
    }
}
