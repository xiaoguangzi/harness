//! The agent's inbox: what arrives for the model from outside its turn.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::Notify;

/// Messages for the model that arrive from elsewhere while it works or
/// waits: a background subagent's report for the main agent, a follow-up
/// for a subagent, a message the user typed while a turn ran. The loop
/// hands over what has arrived with its next request; a frontend starts a
/// turn for them when the agent is idle.
#[derive(Default)]
pub struct Inbox {
    queue: Mutex<Queue>,
    arrived: Notify,
    /// Called after each push that wants a turn.
    on_wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct Queue {
    entries: Vec<Entry>,
    next_id: u64,
}

struct Entry {
    id: u64,
    blocks: Vec<Value>,
    /// Whether it should start a turn of its own, rather than wait for the
    /// next one.
    wake: bool,
    completion: Option<String>,
}

impl Inbox {
    /// Queues `text` for the model. With `wake`, an idle agent should run a
    /// turn for it; otherwise it goes along with the next one.
    pub fn push(&self, text: String, wake: bool) {
        self.push_content(vec![json!({"type": "text", "text": text})], wake);
    }

    /// Queues content blocks for the model, like [`Inbox::push`]; returns
    /// an id to follow them by.
    pub fn push_content(&self, blocks: Vec<Value>, wake: bool) -> u64 {
        self.enqueue(blocks, wake, None)
    }

    pub fn push_completion(&self, key: String, text: String) {
        self.enqueue(vec![json!({"type": "text", "text": text})], true, Some(key));
    }

    /// Explicitly collected results must not also wake a later turn.
    pub fn acknowledge(&self, key: &str) {
        self.queue.lock().unwrap().entries.retain(|e| e.completion.as_deref() != Some(key));
    }

    pub async fn input_wanted(&self) {
        loop {
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            if self.queue.lock().unwrap().entries.iter().any(|e| e.wake && e.completion.is_none()) { return; }
            arrived.await;
        }
    }

    fn enqueue(&self, blocks: Vec<Value>, wake: bool, completion: Option<String>) -> u64 {
        let deferred = completion.is_some();
        let id = {
            let mut queue = self.queue.lock().unwrap();
            let id = queue.next_id;
            queue.next_id += 1;
            queue.entries.push(Entry { id, blocks, wake, completion });
            id
        };
        if wake {
            self.arrived.notify_waiters();
            let hook = self.on_wake.lock().unwrap().clone();
            if let Some(hook) = hook {
                // Completion is published under the task state lock. Invoke
                // frontend hooks outside that lock to avoid lock inversion.
                if deferred { tokio::spawn(async move { hook(); }); } else { hook(); }
            }
        }
        id
    }

    /// Whether something waiting wants a turn.
    pub fn wants_turn(&self) -> bool {
        self.queue.lock().unwrap().entries.iter().any(|e| e.wake)
    }

    /// Whether what was pushed as `id` still waits, not yet taken.
    pub fn waiting(&self, id: u64) -> bool {
        self.queue.lock().unwrap().entries.iter().any(|e| e.id == id)
    }

    /// Takes back what was pushed as `id`, if it still waits.
    pub fn withdraw(&self, id: u64) -> bool {
        let mut queue = self.queue.lock().unwrap();
        let before = queue.entries.len();
        queue.entries.retain(|e| e.id != id);
        queue.entries.len() < before
    }

    pub fn clear(&self) {
        self.take();
    }

    /// Takes everything waiting.
    pub fn take(&self) -> Vec<Value> {
        let entries = std::mem::take(&mut self.queue.lock().unwrap().entries);
        entries.into_iter().flat_map(|e| e.blocks).collect()
    }

    /// Sets what to call when something arrives that wants a turn, such
    /// as starting one in a frontend.
    pub fn on_wake(&self, hook: impl Fn() + Send + Sync + 'static) {
        *self.on_wake.lock().unwrap() = Some(Arc::new(hook));
    }

    /// Resolves once something waiting wants a turn.
    pub async fn wanted(&self) {
        loop {
            let arrived = self.arrived.notified();
            tokio::pin!(arrived);
            arrived.as_mut().enable();
            if self.wants_turn() {
                return;
            }
            arrived.await;
        }
    }
}
