//! Harvester-native command implementations, one function per command.
//! `server.rs` is the controller: it decodes, routes here, encodes.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use harvester_proto::{HarvesterCmd, HarvesterResponse, Outgoing, CR, LF};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
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

/// Ceiling on the whole throwaway `list_models` pi run (spawn → answer →
/// exit; measured ~1s on exe.dev). Same role as `STOP_KILL_BACKSTOP`: only a
/// wedged pi hits it, and `kill_on_drop` SIGKILLs on expiry — safe here
/// because a `--no-session` pi has nothing durable to truncate.
const LIST_MODELS_TIMEOUT: Duration = Duration::from_secs(15);

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
        HarvesterCmd::ListModels { cwd } => list_models(cwd).await,
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

/// The models pi would offer in `cwd`, fetched fresh every call (DAEMON.md:
/// deterministic, touches no session, no caching): a throwaway
/// `PI_OFFLINE=1 pi --mode rpc --no-session` answers `get_available_models`
/// and pi's `data` is returned verbatim. Runs inline in the connection's
/// command loop (~1s) — attached event streams ride their own forwarder
/// tasks, so nothing user-visible stalls.
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
    const REQUEST_LINE: &[u8] = br#"{"id":"hv-models","type":"get_available_models"}"#;

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

    let mut request = REQUEST_LINE.to_vec();
    request.push(LF);
    stdin
        .write_all(&request)
        .await
        .map_err(|error| format!("write to pi: {error}"))?;
    stdin
        .flush()
        .await
        .map_err(|error| format!("write to pi: {error}"))?;

    // LF-only framing, like session.rs's pump: read_until(LF), strip a
    // trailing CR, skip non-JSON and non-matching lines (pi may emit events
    // before the response).
    let mut reader = BufReader::new(stdout);
    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);

    loop {
        buf.clear();

        let read = reader
            .read_until(LF, &mut buf)
            .await
            .map_err(|error| format!("read from pi: {error}"))?;

        if read == 0 {
            return Err("pi exited before answering".to_string());
        }

        let mut line = buf.as_slice();

        if line.last() == Some(&LF) {
            line = &line[..line.len() - 1];
        }

        if line.last() == Some(&CR) {
            line = &line[..line.len() - 1];
        }

        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            continue;
        };

        if value.get("id").and_then(Value::as_str) != Some("hv-models") {
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
