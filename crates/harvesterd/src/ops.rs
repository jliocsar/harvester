//! Harvester-native command implementations, one function per command.
//! `server.rs` is the controller: it decodes, routes here, encodes.

use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{HarvesterCmd, HarvesterResponse, Outgoing};
use serde_json::json;
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::daemon::Daemon;
use crate::fslist;
use crate::session::{Session, SessionOutput, SpawnArgs};

/// Backstop between stdin EOF and SIGKILL on `stop`. Not a correctness knob:
/// EOF triggers pi's immediate shutdown path — verified in rpc-mode.js, it
/// never waits for an in-flight turn. This only breaks a genuinely wedged
/// process (hung extension `session_shutdown` handler, stuck runtime).
const STOP_KILL_BACKSTOP: Duration = Duration::from_secs(30);

/// One live attachment: the forwarder task plus the exact session object whose
/// `attached` counter it incremented. Holding the `Arc<Session>` (not just the
/// sid) matters — a sid is ambiguous across resume generations, and releasing
/// against the wrong generation would underflow the new session's counter and
/// wedge the empty-room dialog policy (found live in the e2e run).
pub struct Attachment {
    session: Arc<Session>,
    forwarder: JoinHandle<()>,
}

/// Connection-local state the attach/detach commands operate on: sid → attachment.
pub type AttachedMap = HashMap<String, Attachment>;

/// The single place an attachment is torn down: abort the forwarder, decrement
/// the counter on the same session object `attach` incremented.
pub fn release_attachment(attachment: Attachment) {
    attachment.forwarder.abort();
    attachment.session.attached.fetch_sub(1, Ordering::SeqCst);
}

pub async fn dispatch(
    daemon: &Daemon,
    cmd: HarvesterCmd,
    out_tx: &mpsc::Sender<Outgoing>,
    attached: &mut AttachedMap,
) -> HarvesterResponse {
    match cmd {
        HarvesterCmd::Ping { version } => ping(version),
        HarvesterCmd::ListSessions { cwd } => list_sessions(daemon, cwd).await,
        HarvesterCmd::CreateSession {
            sid,
            cwd,
            provider,
            model,
            name,
        } => {
            create_session(
                daemon,
                SpawnArgs {
                    sid,
                    cwd,
                    provider,
                    model,
                    name,
                },
            )
            .await
        }
        HarvesterCmd::ListModels { cwd } => list_models(cwd),
        HarvesterCmd::Attach { sid } => attach(daemon, sid, out_tx, attached).await,
        HarvesterCmd::Detach { sid } => detach(sid, attached),
        HarvesterCmd::Stop { sid } => stop(daemon, sid).await,
    }
}

fn ping(_client_version: String) -> HarvesterResponse {
    // The client decides what to do on mismatch; we just report ours.
    HarvesterResponse::ok(json!({"version": harvester_proto::PROTOCOL_VERSION}))
}

async fn list_sessions(daemon: &Daemon, cwd: Option<String>) -> HarvesterResponse {
    let running = daemon.running_sids().await;
    let sessions = fslist::list_sessions(cwd.as_deref(), &running);

    HarvesterResponse::ok(json!({ "sessions": sessions }))
}

async fn create_session(daemon: &Daemon, args: SpawnArgs) -> HarvesterResponse {
    let mut sessions = daemon.inner().sessions.lock().await;

    if sessions.contains_key(&args.sid) {
        return HarvesterResponse::err(format!("session '{}' already running", args.sid));
    }

    match Session::spawn(args, &daemon.inner().log_dir).await {
        Ok(session) => {
            let sid = session.sid.clone();
            sessions.insert(sid.clone(), session.clone());
            drop(sessions);
            daemon.reap_on_exit(session);

            HarvesterResponse::ok(json!({"sid": sid}))
        }
        Err(error) => HarvesterResponse::err(format!("spawn failed: {error}")),
    }
}

fn list_models(_cwd: String) -> HarvesterResponse {
    // TODO(list_models): throwaway `pi --mode rpc --no-session` in cwd,
    // ask get_available_models, close stdin. Needs the ghost-chat UI first.
    HarvesterResponse::err("list_models: not implemented yet")
}

async fn attach(
    daemon: &Daemon,
    sid: String,
    out_tx: &mpsc::Sender<Outgoing>,
    attached: &mut AttachedMap,
) -> HarvesterResponse {
    let Some(session) = daemon.session(&sid).await else {
        return HarvesterResponse::err(format!("no running session '{sid}'"));
    };

    if let Some(existing) = attached.get(&sid) {
        if Arc::ptr_eq(&existing.session, &session) {
            return HarvesterResponse::ok(json!({"already": true}));
        }

        // Different generation: the sid was stopped and resumed since this
        // attachment was made. Release the stale one, attach to the new one.
        let stale = attached.remove(&sid).expect("checked above");
        release_attachment(stale);
    }

    session.attached.fetch_add(1, Ordering::SeqCst);
    let forwarder = spawn_forwarder(&session, sid.clone(), out_tx.clone());
    attached.insert(sid, Attachment { session, forwarder });

    HarvesterResponse::ok(json!({}))
}

/// Stream one session's events/exit to one connection until either side goes away.
fn spawn_forwarder(
    session: &Session,
    sid: String,
    out_tx: mpsc::Sender<Outgoing>,
) -> JoinHandle<()> {
    let mut events = session.output.subscribe();

    tokio::spawn(async move {
        while let Ok(output) = events.recv().await {
            // Exit is terminal: deliver it and end the forwarder.
            let is_exit = matches!(output, SessionOutput::Exit { .. });
            let outgoing = match output {
                SessionOutput::Event(event) => Outgoing::Event {
                    sid: sid.clone(),
                    event,
                },
                SessionOutput::Exit { exit_code } => Outgoing::SessionExit {
                    sid: sid.clone(),
                    exit_code,
                },
                SessionOutput::Response { .. } => continue,
            };

            if out_tx.send(outgoing).await.is_err() || is_exit {
                break;
            }
        }
    })
}

fn detach(sid: String, attached: &mut AttachedMap) -> HarvesterResponse {
    if let Some(attachment) = attached.remove(&sid) {
        release_attachment(attachment);
    }

    HarvesterResponse::ok(json!({}))
}

/// Non-blocking stop: polite escalation ladder, response returns immediately.
/// `session_exit` is the authoritative "it's done" signal — this response only
/// means "shutdown initiated". Table removal is `reap_on_exit`'s job.
async fn stop(daemon: &Daemon, sid: String) -> HarvesterResponse {
    let Some(session) = daemon.session(&sid).await else {
        return HarvesterResponse::err(format!("no running session '{sid}'"));
    };

    // Subscribe before acting so the Exit broadcast cannot slip past the backstop.
    let events = session.output.subscribe();

    // Rung 1: abort any in-flight turn so pi records it cleanly in the session
    // file instead of dying mid-stream. Failure is fine — a dead or deaf pi is
    // exactly what the rungs below are for.
    let request_number = daemon
        .inner()
        .request_counter
        .fetch_add(1, Ordering::Relaxed);
    let abort = json!({"id": format!("hv-{request_number}"), "type": "abort"});

    if let Err(error) = session.write_line(&abort).await {
        eprintln!("stop {sid}: abort write failed (continuing): {error}");
    }

    // Rung 2: EOF — pi's shutdown path. Rung 3 (SIGKILL) runs detached.
    session.close_stdin().await;
    tokio::spawn(kill_backstop(session, events));

    HarvesterResponse::ok(json!({"stopping": true}))
}

/// Rung 3: SIGKILL a pi that ignored stdin EOF for `STOP_KILL_BACKSTOP`.
async fn kill_backstop(session: Arc<Session>, mut events: broadcast::Receiver<SessionOutput>) {
    let exited = tokio::time::timeout(STOP_KILL_BACKSTOP, async {
        while let Ok(output) = events.recv().await {
            if matches!(output, SessionOutput::Exit { .. }) {
                return true;
            }
        }

        false
    })
    .await
    .unwrap_or(false);

    if !exited {
        eprintln!(
            "stop {}: pi ignored stdin EOF for {STOP_KILL_BACKSTOP:?}, killing",
            session.sid
        );
        session.kill();
    }
}
