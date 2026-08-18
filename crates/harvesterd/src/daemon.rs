use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::session::{Session, SpawnArgs};
use crate::sid::Sid;

pub const PASSTHROUGH_ID_PREFIX: &str = "hv-";

pub enum SpawnOutcome {
    Spawned(Arc<Session>),
    AlreadyRunning(Arc<Session>),
}

pub struct RunningSession {
    pub cwd: String,
    pub spawned_at_ms: u64,
}

#[derive(Clone)]
pub struct Daemon {
    inner: Arc<DaemonInner>,
}

struct DaemonInner {
    build_fingerprint: String,
    sessions: Mutex<HashMap<Sid, Arc<Session>>>,
    request_counter: std::sync::atomic::AtomicU64,
}

impl Daemon {
    pub fn new(build_fingerprint: String) -> Self {
        Daemon {
            inner: Arc::new(DaemonInner {
                build_fingerprint,
                sessions: Mutex::new(HashMap::new()),
                request_counter: std::sync::atomic::AtomicU64::new(0),
            }),
        }
    }

    pub fn build_fingerprint(&self) -> &str {
        &self.inner.build_fingerprint
    }

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

    pub fn running_sessions(&self) -> HashMap<Sid, RunningSession> {
        self.lock_sessions()
            .iter()
            .map(|(sid, session)| {
                let running = RunningSession {
                    cwd: session.cwd.clone(),
                    spawned_at_ms: session.spawned_at_ms,
                };

                (sid.clone(), running)
            })
            .collect()
    }

    pub fn spawn_session(&self, args: SpawnArgs) -> std::io::Result<SpawnOutcome> {
        let mut sessions = self.lock_sessions();

        if let Some(running) = sessions.get(args.sid.as_str()) {
            return Ok(SpawnOutcome::AlreadyRunning(running.clone()));
        }

        let session = Session::spawn(args, self)?;
        sessions.insert(session.sid.clone(), session.clone());

        Ok(SpawnOutcome::Spawned(session))
    }

    pub fn is_current_generation(&self, session: &Arc<Session>) -> bool {
        is_current_generation_in(&self.lock_sessions(), session)
    }

    pub fn remove_session_if_same(&self, session: &Arc<Session>) {
        let mut sessions = self.lock_sessions();

        if is_current_generation_in(&sessions, session) {
            sessions.remove(&session.sid);
        }
    }

    fn lock_sessions(&self) -> MutexGuard<'_, HashMap<Sid, Arc<Session>>> {
        self.inner.sessions.lock().expect("sessions mutex")
    }
}

fn is_current_generation_in(sessions: &HashMap<Sid, Arc<Session>>, session: &Arc<Session>) -> bool {
    sessions
        .get(session.sid.as_str())
        .is_some_and(|stored| Arc::ptr_eq(stored, session))
}
