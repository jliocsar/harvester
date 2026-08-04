//! Shared daemon state: the pid table and nothing else (docs/DAEMON.md:
//! harvesterd is stateless — pi's session files are the only source of truth).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::session::{Session, SpawnArgs};

/// Namespace for request ids the daemon injects into pi commands. The pump
/// routes a response back to its waiter by this prefix, so the mint
/// ([`Daemon::next_passthrough_id`]) and the match (`session.rs`) must agree —
/// hence one const rather than a literal at each site.
pub const PASSTHROUGH_ID_PREFIX: &str = "hv-";

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

pub struct DaemonInner {
    pub log_dir: PathBuf,
    /// The pid table. std::sync on purpose: no guard is ever held across an
    /// await, and lookups happen on every passthrough.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Monotonic counter behind [`Daemon::next_passthrough_id`]; private so
    /// the id format cannot be spelled a second way.
    request_counter: std::sync::atomic::AtomicU64,
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

    /// The next `hv-<n>` id for a command the daemon injects into pi. Paired
    /// with the pump's prefix match — see [`PASSTHROUGH_ID_PREFIX`].
    pub fn next_passthrough_id(&self) -> String {
        let request_number = self
            .inner
            .request_counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        format!("{PASSTHROUGH_ID_PREFIX}{request_number}")
    }

    pub fn session(&self, sid: &str) -> Option<Arc<Session>> {
        self.lock_sessions().get(sid).cloned()
    }

    pub fn running_sids(&self) -> Vec<String> {
        self.lock_sessions().keys().cloned().collect()
    }

    /// Check-and-spawn under one lock — the table is the per-sid single-
    /// instance guard, so a second `create_session` for a live sid loses here.
    pub fn spawn_session(&self, args: SpawnArgs) -> std::io::Result<Arc<Session>> {
        let mut sessions = self.lock_sessions();

        if sessions.contains_key(&args.sid) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                format!("session '{}' already running", args.sid),
            ));
        }

        let session = Session::spawn(args, self)?;
        sessions.insert(session.sid.clone(), session.clone());

        Ok(session)
    }

    /// Vacate `session`'s table entry — only if it is still the stored
    /// generation (`Arc::ptr_eq`); a resumed successor is never evicted.
    /// Called by the stdout pump *before* it broadcasts Exit.
    pub fn remove_session_if_same(&self, session: &Arc<Session>) {
        let mut sessions = self.lock_sessions();

        if sessions
            .get(&session.sid)
            .is_some_and(|stored| Arc::ptr_eq(stored, session))
        {
            sessions.remove(&session.sid);
        }
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<String, Arc<Session>>> {
        self.inner.sessions.lock().expect("sessions mutex")
    }
}
