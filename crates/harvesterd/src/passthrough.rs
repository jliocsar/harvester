use harvester_proto::{Outgoing, PI_EXTENSION_UI_RESPONSE};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::ops::no_such_session_message;
use crate::server::{answer_later, Outbound};
use crate::session::{Daemon, Session};
use crate::sid::Sid;

const SESSION_SWAPPING_COMMANDS: &[&str] = &["new_session", "switch_session", "fork", "clone"];

const SESSION_EXITED_BEFORE_RESPONDING: &str = "session exited before responding";

pub(crate) fn pi_passthrough(
    daemon: &Daemon,
    id: Option<Value>,
    sid: String,
    mut cmd: Value,
    out_tx: &mpsc::Sender<Outbound>,
) -> Option<Outgoing> {
    let sid = match Sid::parse(&sid) {
        Ok(sid) => sid,
        Err(rejection) => return Some(error_response(id, sid, rejection)),
    };

    let Some(session) = daemon.session(&sid) else {
        let message = no_such_session_message(&sid);

        return Some(error_response(id, sid.to_string(), message));
    };

    if let Some(command_type) = session_swapping_command(&cmd) {
        let message = session_swap_refusal_message(command_type);

        return Some(error_response(id, sid.to_string(), message));
    }

    if cmd.get("type").and_then(Value::as_str) == Some(PI_EXTENSION_UI_RESPONSE) {
        return Some(forward_dialog_answer(&session, id, sid.to_string(), &cmd));
    }

    let Some(cmd_object) = cmd.as_object_mut() else {
        return Some(error_response(
            id,
            sid.to_string(),
            "pi cmd must be a JSON object".into(),
        ));
    };
    let request_key = daemon.next_passthrough_id();
    cmd_object.insert("id".into(), Value::String(request_key.clone()));

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
            Err(_closed) => pi_failure_payload(SESSION_EXITED_BEFORE_RESPONDING),
        };

        Outgoing::PiResponse {
            id,
            sid: sid.to_string(),
            response,
        }
    });

    None
}

fn forward_dialog_answer(
    session: &Session,
    id: Option<Value>,
    sid: String,
    cmd: &Value,
) -> Outgoing {
    let dialog_id = cmd.get("id").and_then(Value::as_str).unwrap_or_default();

    if session.remove_pending_dialog(dialog_id) {
        if let Err(error) = session.write_line(cmd) {
            eprintln!("session {sid}: dialog {dialog_id} answer write failed: {error}");
        }
    }

    Outgoing::PiResponse {
        id,
        sid,
        response: Value::Null,
    }
}

fn session_swapping_command(cmd: &Value) -> Option<&str> {
    let command_type = cmd.get("type").and_then(Value::as_str)?;

    SESSION_SWAPPING_COMMANDS
        .contains(&command_type)
        .then_some(command_type)
}

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
        response: pi_failure_payload(error),
    }
}

fn pi_failure_payload(error: impl Into<String>) -> Value {
    json!({"type": "response", "success": false, "error": error.into()})
}

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
            json!({"type": PI_EXTENSION_UI_RESPONSE, "id": "d1", "cancelled": true}),
        ] {
            assert_eq!(session_swapping_command(&cmd), None);
        }
    }

    #[test]
    fn a_command_that_merely_mentions_a_denied_name_is_not_refused() {
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
    fn every_failure_wears_the_shape_of_a_pi_response() {
        for payload in [
            pi_failure_payload(SESSION_EXITED_BEFORE_RESPONDING),
            pi_failure_payload(session_swap_refusal_message("fork")),
        ] {
            assert_eq!(payload["type"], "response");
            assert_eq!(payload["success"], json!(false));
            assert!(payload["error"].is_string());
        }
    }

    #[test]
    fn a_daemon_invented_failure_carries_no_pi_command_name() {
        let payload = pi_failure_payload(SESSION_EXITED_BEFORE_RESPONDING);

        assert!(
            payload.get("command").is_none(),
            "pi echoes the command it answered; the daemon has no command to echo"
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
