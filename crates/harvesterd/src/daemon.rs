//! Shared daemon state: the pid table and nothing else (docs/DAEMON.md:
//! harvesterd is stateless — pi's session files are the only source of truth).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::session::{Session, SpawnArgs};
use crate::sid::Sid;

/// Namespace for request ids the daemon injects into pi commands. The pump
/// routes a response back to its waiter by this prefix, so the mint
/// ([`Daemon::next_passthrough_id`]) and the match (`session.rs`) must agree —
/// hence one const rather than a literal at each site.
pub const PASSTHROUGH_ID_PREFIX: &str = "hv-";

/// What [`Daemon::spawn_session`] did. Both arms hand back the live session, so
/// the caller never has to reconstruct the sid it just moved in.
pub enum SpawnOutcome {
    Spawned(Arc<Session>),
    /// The sid was already in the pid table; no second pi was spawned, which is
    /// what keeps "one pi process = one session file" true.
    AlreadyRunning(Arc<Session>),
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

pub struct DaemonInner {
    pub log_dir: PathBuf,
    /// This process's own build fingerprint, computed once at startup and
    /// reported in `ping` (docs/DAEMON.md §ping). Not persisted state — it is
    /// the identity of the running image, and it dies with the process.
    pub build_fingerprint: String,
    /// The pid table. std::sync on purpose: no guard is ever held across an
    /// await, and lookups happen on every passthrough.
    sessions: Mutex<HashMap<Sid, Arc<Session>>>,
    /// Monotonic counter behind [`Daemon::next_passthrough_id`]; private so
    /// the id format cannot be spelled a second way.
    request_counter: std::sync::atomic::AtomicU64,
}

impl Daemon {
    pub fn new(log_dir: PathBuf, build_fingerprint: String) -> Self {
        Daemon {
            inner: Arc::new(DaemonInner {
                log_dir,
                build_fingerprint,
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

    /// The sids with a live pi child, copied out from under the lock (no guard
    /// may cross the `spawn_blocking` boundary the caller is headed for). A set,
    /// not a list: `list_sessions` asks "is this sid running?" once per session
    /// file on disk.
    pub fn running_sids(&self) -> HashSet<String> {
        self.lock_sessions().keys().map(Sid::to_string).collect()
    }

    /// Check-and-spawn under one lock — the table is the per-sid single-
    /// instance guard, so a second `create_session` for a live sid loses here.
    /// `Err` is reserved for a genuine spawn failure; "it was already running"
    /// is a domain outcome and says so in the type instead of being smuggled
    /// through `io::ErrorKind::AlreadyExists` for the caller to sniff.
    pub fn spawn_session(&self, args: SpawnArgs) -> std::io::Result<SpawnOutcome> {
        let mut sessions = self.lock_sessions();

        if let Some(running) = sessions.get(args.sid.as_str()) {
            return Ok(SpawnOutcome::AlreadyRunning(running.clone()));
        }

        let session = Session::spawn(args, self)?;
        sessions.insert(session.sid.clone(), session.clone());

        Ok(SpawnOutcome::Spawned(session))
    }

    /// Vacate `session`'s table entry — only if it is still the stored
    /// generation (`Arc::ptr_eq`); a resumed successor is never evicted.
    /// Called by the stdout pump *before* it broadcasts Exit.
    pub fn remove_session_if_same(&self, session: &Arc<Session>) {
        let mut sessions = self.lock_sessions();

        if sessions
            .get(session.sid.as_str())
            .is_some_and(|stored| Arc::ptr_eq(stored, session))
        {
            sessions.remove(&session.sid);
        }
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<Sid, Arc<Session>>> {
        self.inner.sessions.lock().expect("sessions mutex")
    }
}
