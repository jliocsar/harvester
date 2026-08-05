//! Verbatim pi RPC passthrough (docs/DAEMON.md §The `pi` passthrough).

use harvester_proto::Outgoing;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::daemon::Daemon;
use crate::ops::no_such_session_message;
use crate::server::{answer_later, Outbound};
use crate::session::Session;
use crate::sid::Sid;

/// The pi commands that swap the session file under a live process. Refused by
/// the daemon rather than left to client politeness: "one pi process = one
/// session file, always" is architectural (CLAUDE.md, docs/DAEMON.md §The `pi`
/// passthrough), multiple clients are a first-class guarantee — so the trusted
/// client is not the only client — and the failure this prevents (a session
/// file swapped under a running pi) is unrecoverable.
///
/// These are pi command `type` values, not `method` values like the dialog
/// list in `session.rs`. **Pi-version-coupled**: verified against
/// pi 0.83.0's `modes/rpc/rpc-types.ts:26,60,61,62` (the `RpcCommand` union)
/// and its dispatch arms in `modes/rpc/rpc-mode.ts:432,600,608,616`. This is
/// the one place the daemon deliberately knows pi's vocabulary, so re-check the
/// strings on every pi upgrade (CLAUDE.md §Pi version discipline) — a renamed
/// command silently disables the guard.
const SESSION_SWAPPING_COMMANDS: &[&str] = &["new_session", "switch_session", "fork", "clone"];

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
        let message = no_such_session_message(&sid);

        return Some(error_response(id, sid.into(), message));
    };

    // Refused before the id is injected and before anything is registered: a
    // session-swapping command must never reach pi's stdin at all.
    if let Some(command_type) = session_swapping_command(&cmd) {
        let message = session_swap_refusal_message(command_type);

        return Some(error_response(id, sid.into(), message));
    }

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

    answer_later(out_tx, async move {
        let response = match response_rx.await {
            Ok(response) => response,
            // Sender dropped: the session exited before answering.
            Err(_closed) => json!({"type": "response", "success": false,
                                   "error": "session exited before responding"}),
        };

        Outgoing::PiResponse {
            id,
            sid: sid.into(),
            response,
        }
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

    if session.remove_pending_dialog(dialog_id) {
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

/// The command's `type` when it is one of [`SESSION_SWAPPING_COMMANDS`]. A
/// non-object command (or one without a string `type`) is not this handler's
/// problem — it is rejected further down for not being able to carry the
/// correlation id.
fn session_swapping_command(cmd: &Value) -> Option<&str> {
    let command_type = cmd.get("type").and_then(Value::as_str)?;

    SESSION_SWAPPING_COMMANDS
        .contains(&command_type)
        .then_some(command_type)
}

/// Names the invariant rather than just saying no: the client author reading
/// this needs to know it is a rule, not a bug.
fn session_swap_refusal_message(command_type: &str) -> String {
    format!(
        "pi command '{command_type}' is refused by harvesterd: it would swap the session file \
         under a running pi, and one pi process = one session file, always \
         (docs/DAEMON.md §The `pi` passthrough)"
    )
}

fn error_response(id: Option<Value>, sid: String, error: String) -> Outgoing {
    Outgoing::PiResponse {
        id,
        sid,
        response: json!({"type": "response", "success": false, "error": error}),
    }
}

/// The session-swapping denylist in isolation — no pi child, no daemon. The
/// clause it enforces is docs/DAEMON.md §The `pi` passthrough.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_session_swapping_command_is_recognised() {
        for command_type in SESSION_SWAPPING_COMMANDS {
            let cmd = json!({"type": command_type, "id": "client-1"});

            assert_eq!(
                session_swapping_command(&cmd),
                Some(*command_type),
                "'{command_type}' must be refused"
            );
        }
    }

    #[test]
    fn the_denylist_is_exactly_the_four_documented_commands() {
        // docs/DAEMON.md and CLAUDE.md name these four and only these four;
        // pi 0.83.0 spells them this way (see the const's doc comment).
        assert_eq!(
            SESSION_SWAPPING_COMMANDS,
            ["new_session", "switch_session", "fork", "clone"]
        );
    }

    #[test]
    fn an_ordinary_passthrough_command_is_not_refused() {
        for cmd in [
            json!({"type": "prompt", "message": "fix the tests"}),
            json!({"type": "abort"}),
            json!({"type": "get_entries"}),
            json!({"type": "extension_ui_response", "id": "d1", "cancelled": true}),
        ] {
            assert_eq!(session_swapping_command(&cmd), None);
        }
    }

    #[test]
    fn a_command_that_merely_mentions_a_denied_name_is_not_refused() {
        // The match is on `type` alone: a prompt asking pi to fork a repo is a
        // perfectly ordinary prompt.
        let cmd = json!({"type": "prompt", "message": "fork the upstream repo"});

        assert_eq!(session_swapping_command(&cmd), None);
    }

    #[test]
    fn the_denylist_match_is_exact_and_case_sensitive() {
        for cmd in [
            json!({"type": "New_Session"}),
            json!({"type": "forked"}),
            json!({"type": " fork"}),
        ] {
            assert_eq!(session_swapping_command(&cmd), None);
        }
    }

    #[test]
    fn a_command_without_a_string_type_is_left_to_the_later_checks() {
        for cmd in [
            json!("fork"),
            json!(["fork"]),
            json!({"type": 7}),
            json!({}),
            Value::Null,
        ] {
            assert_eq!(session_swapping_command(&cmd), None);
        }
    }

    #[test]
    fn the_refusal_names_the_command_and_the_invariant() {
        let message = session_swap_refusal_message("fork");

        assert!(message.contains("'fork'"), "{message}");
        assert!(
            message.contains("one pi process = one session file"),
            "{message}"
        );
    }

    #[test]
    fn the_refusal_travels_as_a_failed_pi_response() {
        let outgoing = error_response(
            Some(json!(7)),
            "01H".to_string(),
            session_swap_refusal_message("clone"),
        );
        let parsed = serde_json::to_value(&outgoing).expect("wire enum serializes");

        assert_eq!(parsed["type"], "pi_response");
        assert_eq!(parsed["id"], json!(7));
        assert_eq!(parsed["sid"], "01H");
        assert_eq!(parsed["response"]["success"], json!(false));
        assert!(parsed["response"]["error"]
            .as_str()
            .expect("an error string")
            .contains("'clone'"));
    }
}
