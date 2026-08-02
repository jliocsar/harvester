//! Verbatim pi RPC passthrough (docs/DAEMON.md §The `pi` passthrough).

use std::sync::atomic::Ordering;

use harvester_proto::Outgoing;
use serde_json::{json, Value};
use tokio::time::Duration;

use crate::daemon::Daemon;
use crate::session::SessionOutput;

/// How long a passthrough waits for pi's response line before giving up.
const PI_RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Inject a correlation id, write to pi's stdin, wait for the matching
/// response line. `extension_ui_response` is fire-and-forget on pi's side —
/// it resolves immediately with `response: null`.
pub async fn pi_passthrough(
    daemon: &Daemon,
    id: Option<Value>,
    sid: String,
    mut cmd: Value,
) -> Outgoing {
    let Some(session) = daemon.session(&sid).await else {
        return error_response(id, sid, "no running session".into());
    };

    let cmd_type = cmd
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();

    if cmd_type == "extension_ui_response" {
        // First answer wins: only forward if the dialog is still pending.
        let dialog_id = cmd
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let still_pending = session
            .pending_dialogs
            .lock()
            .await
            .remove(&dialog_id)
            .is_some();

        if still_pending {
            let _ = session.write_line(&cmd).await;
        }

        return Outgoing::PiResponse {
            id,
            sid,
            response: Value::Null,
        };
    }

    let request_number = daemon
        .inner()
        .request_counter
        .fetch_add(1, Ordering::Relaxed);
    let request_key = format!("hv-{request_number}");

    if let Some(cmd_object) = cmd.as_object_mut() {
        cmd_object.insert("id".into(), Value::String(request_key.clone()));
    }

    // Subscribe before writing so the response cannot slip past us.
    let mut events = session.output.subscribe();

    if let Err(error) = session.write_line(&cmd).await {
        return error_response(id, sid, format!("write to pi failed: {error}"));
    }

    let response = tokio::time::timeout(PI_RESPONSE_TIMEOUT, async {
        while let Ok(output) = events.recv().await {
            match output {
                SessionOutput::Response {
                    request_key: key,
                    response,
                } if key == request_key => {
                    return Some(response);
                }
                SessionOutput::Exit { .. } => return None,
                _ => {}
            }
        }

        None
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_else(|| {
        json!({"type": "response", "success": false,
               "error": "pi did not respond (timeout or session exited)"})
    });

    Outgoing::PiResponse { id, sid, response }
}

fn error_response(id: Option<Value>, sid: String, error: String) -> Outgoing {
    Outgoing::PiResponse {
        id,
        sid,
        response: json!({"type": "response", "success": false, "error": error}),
    }
}
