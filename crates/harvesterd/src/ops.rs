//! Harvester-native command implementations, one function per command.
//! `server.rs` is the controller: it decodes, routes here, encodes.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{
    encode_frame, read_frame, HarvesterCmd, HarvesterResponse, Outgoing, PingData, SessionsData,
};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use crate::daemon::{Daemon, SpawnOutcome};
use crate::fslist;
use crate::server::{answer_later, Outbound};
use crate::session::{self, Session, SessionOutput, SpawnArgs};
use crate::sid::Sid;

/// Backstop between stdin EOF and SIGKILL on `stop`. Not a correctness knob:
/// EOF triggers pi's immediate shutdown path — verified in rpc-mode.js, it
/// never waits for an in-flight turn. This only breaks a genuinely wedged
/// process (hung extension `session_shutdown` handler, stuck runtime).
const STOP_KILL_BACKSTOP: Duration = Duration::from_secs(30);

/// Ceiling on the whole throwaway `list_models` pi run (spawn → answer →
/// exit; measured ~1s on exe.dev). Same role as `STOP_KILL_BACKSTOP`: only a
/// wedged pi hits it, and `kill_on_drop` SIGKILLs on expiry — safe here
/// because a `--no-session` pi has nothing durable to truncate.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(15);

/// Correlation id for the throwaway pi's one request. Deliberately inside the
/// `hv-` namespace the pump routes on, though this pi has no pump — the reply
/// filter below matches it directly.
const MODELS_REQUEST_ID: &str = "hv-models";

/// `list_models` read buffer starting size; grows to the largest pi line seen.
const MODELS_LINE_BUFFER: usize = 64 * 1024;

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
pub type AttachedMap = HashMap<Sid, Attachment>;

/// Teardown is the type's own job: abort the forwarder, then leave the room on
/// the same session object `attach` joined. Leaving returns the dialogs to
/// cancel — non-empty only when this was the last watcher, so no dialog is ever
/// left unanswered (docs/DAEMON.md). As a `Drop` impl rather than a function
/// every caller must remember, dropping an `Attachment` *anywhere* — an
/// explicit `remove`, a `clear` on disconnect, an overwriting `insert` — is the
/// release, and forgetting it is unrepresentable.
impl Drop for Attachment {
    fn drop(&mut self) {
        self.forwarder.abort();

        for dialog_id in self.session.detach() {
            session::cancel_dialog(&self.session, &dialog_id);
        }
    }
}

/// Route one harvester command. `None` means a spawned task owns the response
/// (slow commands must not head-of-line block the connection loop).
pub fn dispatch(
    daemon: &Daemon,
    id: Option<Value>,
    cmd: HarvesterCmd,
    out_tx: &mpsc::Sender<Outbound>,
    attached: &mut AttachedMap,
) -> Option<Outgoing> {
    let response = match cmd {
        HarvesterCmd::Ping { version } => ping(daemon, version),
        HarvesterCmd::ListSessions { cwd } => {
            spawn_list_sessions(daemon, id, cwd, out_tx);

            return None;
        }
        HarvesterCmd::CreateSession {
            sid,
            cwd,
            provider,
            model,
            name,
        } => with_checked_sid(&sid, |sid| {
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
        }),
        HarvesterCmd::ListModels { cwd } => {
            spawn_list_models(id, cwd, out_tx);

            return None;
        }
        HarvesterCmd::Attach { sid } => {
            with_checked_sid(&sid, |sid| attach(daemon, sid, out_tx, attached))
        }
        HarvesterCmd::Detach { sid } => with_checked_sid(&sid, |sid| detach(sid, attached)),
        HarvesterCmd::Stop { sid } => with_checked_sid(&sid, |sid| stop(daemon, sid)),
    };

    Some(Outgoing::HarvesterResponse { id, response })
}

/// The gate every sid-carrying command passes through: a session id off the
/// wire is a remote string, and it goes on to be a pid-table key, a path
/// component and an argv element. Rejecting here means no handler below ever
/// sees an unchecked one.
fn with_checked_sid(
    raw_sid: &str,
    handle: impl FnOnce(Sid) -> HarvesterResponse,
) -> HarvesterResponse {
    match Sid::parse(raw_sid) {
        Ok(sid) => handle(sid),
        Err(rejection) => HarvesterResponse::err(rejection),
    }
}

/// Two independent answers, deliberately not one: `version` is envelope
/// compatibility, `build` identifies this exact running image (docs/DAEMON.md
/// §ping). The client decides what to do with either; we just report ours.
fn ping(daemon: &Daemon, _client_version: u32) -> HarvesterResponse {
    data_response(PingData {
        version: harvester_proto::PROTOCOL_VERSION,
        build: daemon.inner().build_fingerprint.clone(),
    })
}

/// The one wording for "that sid is not in the pid table", shared by every
/// command that looks a session up — including the pi passthrough, which wraps
/// the same message in pi's own response shape.
pub fn no_such_session_message(sid: &Sid) -> String {
    format!("no running session '{sid}'")
}

fn no_such_session(sid: &Sid) -> HarvesterResponse {
    HarvesterResponse::err(no_such_session_message(sid))
}

/// Answer with a typed daemon-native payload. The payloads are our own types,
/// so serialization cannot fail for any reason worth a runtime branch.
fn data_response(data: impl serde::Serialize) -> HarvesterResponse {
    HarvesterResponse::ok(serde_json::to_value(data).expect("serializable"))
}

/// The disk scan rides the blocking pool: session files can be large, and a
/// synchronous read on the daemon's single runtime thread would stall every
/// pump and connection meanwhile.
fn spawn_list_sessions(
    daemon: &Daemon,
    id: Option<Value>,
    cwd: Option<String>,
    out_tx: &mpsc::Sender<Outbound>,
) {
    let running = daemon.running_sids();

    answer_later(out_tx, async move {
        let scan = tokio::task::spawn_blocking(move || {
            // Borrowed only for the scan: the owned copy stays on this task.
            let running_sids = running.iter().map(String::as_str).collect();

            fslist::list_sessions(cwd.as_deref(), &running_sids)
        });
        let response = match scan.await {
            Ok(sessions) => data_response(SessionsData { sessions }),
            Err(join_error) => HarvesterResponse::err(format!("list_sessions: {join_error}")),
        };

        Outgoing::HarvesterResponse { id, response }
    });
}

/// Resume-or-create, and idempotent on top of that: a sid that is already
/// running succeeds with `already: true` rather than erroring. The daemon owns
/// this truth — a client cannot know whether a session died while it was
/// detached — so "make it running" is the honest contract, and it lets every
/// open be the same unconditional create → attach chain. No second pi is ever
/// spawned: the pid table check inside `spawn_session` is what answers here,
/// so one pi = one session file still holds.
fn create_session(daemon: &Daemon, args: SpawnArgs) -> HarvesterResponse {
    match daemon.spawn_session(args) {
        Ok(SpawnOutcome::Spawned(session)) => HarvesterResponse::ok(json!({"sid": session.sid})),
        Ok(SpawnOutcome::AlreadyRunning(session)) => {
            HarvesterResponse::ok(json!({"sid": session.sid, "already": true}))
        }
        Err(error) => HarvesterResponse::err(format!("spawn failed: {error}")),
    }
}

/// The models pi would offer in `cwd`, fetched fresh every call (DAEMON.md:
/// deterministic, touches no session, no caching): a throwaway
/// `PI_OFFLINE=1 pi --mode rpc --no-session` answers `get_available_models`
/// and pi's `data` is returned verbatim. Rides its own task — the ~1s pi
/// startup (worst case `LIST_MODELS_TIMEOUT`) must not block sibling requests
/// on this connection.
fn spawn_list_models(id: Option<Value>, cwd: String, out_tx: &mpsc::Sender<Outbound>) {
    answer_later(out_tx, async move {
        let response = list_models(cwd).await;

        Outgoing::HarvesterResponse { id, response }
    });
}

async fn list_models(cwd: String) -> HarvesterResponse {
    match tokio::time::timeout(LIST_MODELS_TIMEOUT, query_available_models(&cwd)).await {
        Ok(Ok(data)) => HarvesterResponse::ok(data),
        Ok(Err(detail)) => HarvesterResponse::err(format!("list_models: {detail}")),
        Err(_elapsed) => HarvesterResponse::err(format!(
            "list_models: pi gave no answer within {LIST_MODELS_TIMEOUT:?}"
        )),
    }
}

/// Spawn the throwaway pi, ask, read the one answer. Stdin must stay open
/// until the response line is read — pi treats EOF as shutdown and dies
/// before answering otherwise (verified live on pi 0.83.0). The child is
/// `kill_on_drop`: on every exit path it is either already exiting (stdin
/// dropped) or gets SIGKILLed, so no pi outlives this function.
async fn query_available_models(cwd: &str) -> Result<Value, String> {
    let mut child = tokio::process::Command::new("pi")
        .current_dir(cwd)
        .env("PI_OFFLINE", "1")
        .args(["--mode", "rpc", "--no-session"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| format!("spawn pi: {error}"))?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");

    let request = encode_frame(&json!({"id": MODELS_REQUEST_ID, "type": "get_available_models"}))
        .expect("serializable");
    stdin
        .write_all(&request)
        .await
        .map_err(|error| format!("write to pi: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("write to pi: {error}"))?;

    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(MODELS_LINE_BUFFER);

    loop {
        // Skip non-JSON and non-matching lines (pi may emit events before the
        // response).
        let line = match read_frame(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) => return Err("pi exited before answering".to_string()),
            Err(error) => return Err(format!("read from pi: {error}")),
        };

        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };

        if value.get("id").and_then(Value::as_str) != Some(MODELS_REQUEST_ID) {
            continue;
        }

        if value
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            // EOF starts pi's graceful exit; the wait is bounded by the
            // caller's timeout (EOF shutdown is immediate — source-verified).
            drop(stdin);
            let _ = child.wait().await;

            return Ok(value.get("data").cloned().unwrap_or(json!({})));
        }

        let detail = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("pi rejected get_available_models");

        return Err(detail.to_string());
    }
}

fn attach(
    daemon: &Daemon,
    sid: Sid,
    out_tx: &mpsc::Sender<Outbound>,
    attached: &mut AttachedMap,
) -> HarvesterResponse {
    let Some(session) = daemon.session(sid.as_str()) else {
        return no_such_session(&sid);
    };

    // Attaching twice to the same session object is a no-op; an entry for an
    // *older* generation of this sid (stopped and resumed since) is not, and
    // needs no handling of its own — the `insert` below overwrites it, and the
    // overwritten value's `Drop` leaves the room on the session object it
    // actually joined.
    let already_attached_to_this_generation = attached
        .get(sid.as_str())
        .is_some_and(|existing| Arc::ptr_eq(&existing.session, &session));

    if already_attached_to_this_generation {
        return HarvesterResponse::ok(json!({"already": true}));
    }

    // Subscribe before joining the room, never the other way round. A dialog
    // arriving between the two steps is then auto-cancelled (the room still
    // reads as empty), which costs one spurious "Escape" — whereas joining
    // first would let that dialog be remembered and broadcast before this
    // subscriber exists, so nobody would ever see it and pi would wait forever.
    // Safe direction over convenient direction.
    let events = session.output.subscribe();
    let pending_dialogs = session.attach();
    let forwarder = spawn_forwarder(events, pending_dialogs, sid.clone(), out_tx.clone());
    attached.insert(sid, Attachment { session, forwarder });

    HarvesterResponse::ok(json!({}))
}

/// Stream one session's events/exit to one connection until either side goes
/// away, after replaying the dialogs that were already waiting — a client
/// joining mid-dialog must be able to answer it, or pi waits forever.
fn spawn_forwarder(
    mut events: broadcast::Receiver<SessionOutput>,
    pending_dialogs: Vec<Arc<[u8]>>,
    sid: Sid,
    out_tx: mpsc::Sender<Outbound>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        for dialog in pending_dialogs {
            if out_tx.send(Outbound::Raw(dialog)).await.is_err() {
                return;
            }
        }

        loop {
            let output = match events.recv().await {
                Ok(output) => output,
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // Recoverable, but never silently: the ring dropped
                    // `skipped` events for this subscriber, so tell the client
                    // to resync (it can `get_entries` from its last seen one)
                    // and keep streaming from the oldest retained event.
                    eprintln!("forwarder {sid}: skipped {skipped} events (slow connection)");

                    let gap = Outgoing::EventGap {
                        sid: sid.to_string(),
                        skipped,
                    };

                    if out_tx.send(Outbound::Typed(gap)).await.is_err() {
                        break;
                    }

                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            match output {
                SessionOutput::Event(line) => {
                    if out_tx.send(Outbound::Raw(line)).await.is_err() {
                        break;
                    }
                }
                // Exit is terminal: deliver it and end the forwarder.
                SessionOutput::Exit { exit_code } => {
                    let outgoing = Outgoing::SessionExit {
                        sid: sid.to_string(),
                        exit_code,
                    };
                    let _ = out_tx.send(Outbound::Typed(outgoing)).await;

                    break;
                }
            }
        }
    })
}

/// Removing the entry *is* the release — `Attachment::drop` aborts the
/// forwarder and leaves the room.
fn detach(sid: Sid, attached: &mut AttachedMap) -> HarvesterResponse {
    attached.remove(sid.as_str());

    HarvesterResponse::ok(json!({}))
}

/// Non-blocking stop: polite escalation ladder, response returns immediately.
/// `session_exit` is the authoritative "it's done" signal — this response only
/// means "shutdown initiated". The stdout pump vacates the pid table on exit.
///
/// Rung 3 is armed *first*, on purpose: the ladder exists for a wedged pi, so
/// no rung may depend on that pi cooperating. Both rungs below only touch the
/// stdin queue (never the pipe), so neither can block — but ordering the
/// backstop first keeps that guarantee independent of the queue's behaviour.
fn stop(daemon: &Daemon, sid: Sid) -> HarvesterResponse {
    let Some(session) = daemon.session(sid.as_str()) else {
        return no_such_session(&sid);
    };

    tokio::spawn(kill_backstop(daemon.clone(), session.clone()));

    // Rung 1: abort any in-flight turn so pi records it cleanly in the session
    // file instead of dying mid-stream. Failure is fine — a dead or deaf pi is
    // exactly what the rungs below are for. Registering the waiter (and
    // dropping its receiver) keeps the pump's "no waiter means a task died"
    // branch honest when pi acknowledges the abort.
    let request_key = daemon.next_passthrough_id();
    // Nothing awaits this abort's answer; the receiver is dropped immediately
    // and the pump's "closed receiver" path swallows the reply.
    drop(session.register_response_waiter(request_key.clone()));
    let abort = json!({"id": request_key, "type": "abort"});

    if let Err(error) = session.write_line(&abort) {
        eprintln!("stop {sid}: abort write failed (continuing): {error}");
    }

    // Rung 2: EOF — pi's shutdown path, after the queued abort drains.
    session.close_stdin();

    HarvesterResponse::ok(json!({"stopping": true}))
}

/// Rung 3: SIGKILL a pi that ignored stdin EOF for `STOP_KILL_BACKSTOP`.
/// "Still in the pid table" is the liveness test — the pump vacates the entry
/// before broadcasting Exit, and `ptr_eq` ignores a resumed successor.
async fn kill_backstop(daemon: Daemon, session: Arc<Session>) {
    tokio::time::sleep(STOP_KILL_BACKSTOP).await;

    let still_running = daemon
        .session(session.sid.as_str())
        .is_some_and(|stored| Arc::ptr_eq(&stored, &session));

    if still_running {
        eprintln!(
            "stop {}: pi ignored stdin EOF for {STOP_KILL_BACKSTOP:?}, killing",
            session.sid
        );
        session.kill();
    }
}
