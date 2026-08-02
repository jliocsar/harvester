//! Shared daemon state: the pid table and nothing else (docs/DAEMON.md:
//! harvesterd is stateless — pi's session files are the only source of truth).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use crate::session::{Session, SessionOutput};

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

pub struct DaemonInner {
    pub log_dir: PathBuf,
    pub sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Monotonic counter for injected pi request ids ("hv-<n>").
    pub request_counter: std::sync::atomic::AtomicU64,
}

impl Daemon {
    pub fn new(log_dir: PathBuf) -> Self {
        Daemon {
            inner: Arc::new(DaemonInner {
                log_dir,
                sessions: Mutex::new(HashMap::new()),
                request_counter: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn inner(&self) -> &DaemonInner {
        &self.inner
    }

    pub async fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.inner.sessions.lock().await.get(sid).cloned()
    }

    pub async fn running_sids(&self) -> Vec<String> {
        self.inner.sessions.lock().await.keys().cloned().collect()
    }

    /// Remove the session from the pid table when its pi process exits,
    /// whatever the cause — `stop`, crash, or external kill. Attached clients
    /// already got the `session_exit` event through their forwarders.
    pub fn reap_on_exit(&self, session: Arc<Session>) {
        let daemon = self.clone();
        let mut events = session.output.subscribe();
        let sid = session.sid.clone();

        tokio::spawn(async move {
            while let Ok(output) = events.recv().await {
                if matches!(output, SessionOutput::Exit { .. }) {
                    daemon.inner.sessions.lock().await.remove(&sid);
                    break;
                }
            }
        });
    }
}
