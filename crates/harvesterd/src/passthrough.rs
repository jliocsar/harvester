//! Verbatim pi RPC passthrough (docs/DAEMON.md §The `pi` passthrough).

use harvester_proto::Outgoing;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::daemon::Daemon;
use crate::server::Outbound;
use crate::session::Session;
use crate::sid::Sid;

/// Inject a correlation id, queue the command for pi's stdin, then hand the
/// wait to its own task — the connection loop must never head-of-line block
/// behind a pi command (`bash`/`compact` are legitimately unbounded, and
/// `stop` has to stay reachable). The write itself cannot block either: it
/// only enqueues, and the session's stdin pump owns the pipe. Returns `None`
/// when the spawned waiter will answer.
///
/// Deliberately no timeout: pi's contract is "every command gets a response
/// line or the process dies", and the pump fails every registered waiter on
/// exit (the same reject-pending-on-exit shape as pi's own RPC client). A
/// daemon-invented timeout would fabricate failures for slow-but-fine
/// commands and silently drop pi's real answer.
pub fn pi_passthrough(
    daemon: &Daemon,
    id: Option<Value>,
    sid: String,
    mut cmd: Value,
    out_tx: &mpsc::Sender<Outbound>,
) -> Option<Outgoing> {
    // Validate before anything else touches it: the sid is a remote string
    // that becomes a pid-table key here and a path component downstream. The
    // rejected sid is echoed back verbatim so the client can see what it sent.
    let sid = match Sid::parse(&sid) {
        Ok(sid) => sid,
        Err(rejection) => return Some(error_response(id, sid, rejection)),
    };

    let Some(session) = daemon.session(sid.as_str()) else {
        return Some(error_response(
            id,
            sid.to_string(),
            "no running session".into(),
        ));
    };

    if cmd.get("type").and_then(Value::as_str) == Some("extension_ui_response") {
        return Some(forward_dialog_answer(&session, id, sid.to_string(), &cmd));
    }

    // Reject before registering anything: a non-object command cannot carry
    // the correlation id, so its response would never route back and the
    // waiter would pend until the session exits.
    let Some(cmd_object) = cmd.as_object_mut() else {
        return Some(error_response(
            id,
            sid.to_string(),
            "pi cmd must be a JSON object".into(),
        ));
    };
    let request_key = daemon.next_passthrough_id();
    cmd_object.insert("id".into(), Value::String(request_key.clone()));

    // Register before writing so the response cannot race the waiter. If the
    // write fails, the stale entry is harmless: stdin is gone, so pi is
    // exiting and the pump drains the map shortly.
    let response_rx = session.register_response_waiter(request_key);

    if let Err(error) = session.write_line(&cmd) {
        return Some(error_response(
            id,
            sid.to_string(),
            format!("write to pi failed: {error}"),
        ));
    }

    let out_tx = out_tx.clone();

    tokio::spawn(async move {
        let response = match response_rx.await {
            Ok(response) => response,
            // Sender dropped: the session exited before answering.
            Err(_closed) => json!({"type": "response", "success": false,
                                   "error": "session exited before responding"}),
        };
        let outgoing = Outgoing::PiResponse {
            id,
            sid: sid.into(),
            response,
        };

        let _ = out_tx.send(Outbound::Typed(outgoing)).await;
    });

    None
}

/// `extension_ui_response` is fire-and-forget on pi's side — it resolves
/// immediately with `response: null`. First answer wins: only the claim
/// holder's answer is forwarded; the rest are dropped (docs/DAEMON.md).
fn forward_dialog_answer(
    session: &Session,
    id: Option<Value>,
    sid: String,
    cmd: &Value,
) -> Outgoing {
    let dialog_id = cmd.get("id").and_then(Value::as_str).unwrap_or_default();

    if session.claim_pending_dialog(dialog_id) {
        if let Err(error) = session.write_line(cmd) {
            // The winning answer never reached pi — the dialog policy's
            // invariant is at stake, so this must not vanish silently.
            eprintln!("session {sid}: dialog {dialog_id} answer write failed: {error}");
        }
    }

    Outgoing::PiResponse {
        id,
        sid,
        response: Value::Null,
    }
}

fn error_response(id: Option<Value>, sid: String, error: String) -> Outgoing {
    Outgoing::PiResponse {
        id,
        sid,
        response: json!({"type": "response", "success": false, "error": error}),
    }
}
