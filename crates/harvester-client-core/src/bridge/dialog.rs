//! INTERIM: the unanswered-dialog stopgap.
//!
//! Delete this whole module — plus its `mod` line, the `answered_dialogs`
//! field on [`BridgeContext`](super::BridgeContext) and the one
//! [`auto_cancel_if_dialog`] call in `mod.rs`'s `handle_pi_event` — when the
//! dialogs-as-modals slice lands (docs/SPEC.md §Flows, docs/DAEMON.md §Dialogs
//! and its decision C5). Its own file precisely so that deletion is a `git rm`.
//!
//! It exists because harvesterd's auto-cancel is an *empty-room* safety net
//! only: with a client attached — the normal state — a dialog nobody answers
//! blocks pi indefinitely, and no client answers today.

use std::collections::HashSet;
use std::sync::Mutex;

use serde_json::{json, Value};

use super::{report_failure, spawn_flow, BridgeContext};

/// The pi `extension_ui_request` methods that block the agent until they are
/// answered. Same list as harvesterd's `DIALOG_METHODS`; the fire-and-forget
/// methods (`notify`, `setStatus`, …) are forwarded and never answered.
const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// Dialog ids already answered, per session. Owned by the [`BridgeContext`]
/// rather than by the select loop because a flushed attach buffer replays
/// through `handle_pi_event` from inside a flow. std::sync — never held across
/// an await.
#[derive(Default)]
pub(super) struct AnsweredDialogs {
    ids: Mutex<HashSet<(String, String)>>,
}

impl AnsweredDialogs {
    /// `true` the first time this `(sid, dialog id)` is seen. The daemon
    /// replays a pending dialog to every client that attaches later, so the
    /// same id arriving twice is by design (DAEMON.md §Dialogs) — answer each
    /// id once. Unbounded by session lifetime, but a dialog needs a
    /// user-installed pi extension to exist at all.
    fn claim(&self, sid: &str, dialog_id: &str) -> bool {
        self.ids
            .lock()
            .expect("answered dialogs mutex")
            .insert((sid.to_string(), dialog_id.to_string()))
    }
}

/// Cancels `event` if it is a dialog request Harvester owes an answer for, and
/// does nothing otherwise. Called on every pi event on its way to the
/// frontend, live and from a flushed attach window alike.
pub(super) fn auto_cancel_if_dialog(context: &BridgeContext, sid: &str, event: &Value) {
    let Some((dialog_id, method)) = forwarded_dialog(event) else {
        return;
    };

    if !context.answered_dialogs.claim(sid, dialog_id) {
        return;
    }

    spawn_flow(cancel_dialog_flow(
        context.clone(),
        sid.to_string(),
        dialog_id.to_string(),
        method.to_string(),
    ));
}

/// `(id, method)` when `event` is a dialog request Harvester owes an answer
/// for. Reads pi's payload without touching it — the client speaks Pi.
fn forwarded_dialog(event: &Value) -> Option<(&str, &str)> {
    if event.get("type").and_then(Value::as_str)? != "extension_ui_request" {
        return None;
    }

    let method = event.get("method").and_then(Value::as_str)?;

    if !DIALOG_METHODS.contains(&method) {
        return None;
    }

    let id = event.get("id").and_then(Value::as_str)?;

    Some((id, method))
}

/// Cancels one dialog and tells the user an agent asked something Harvester
/// could not show. `cancelled: true` is exactly what pi's extension host sees
/// when a human presses Escape, and it is fire-and-forget (`response: null`).
async fn cancel_dialog_flow(
    context: BridgeContext,
    sid: String,
    dialog_id: String,
    method: String,
) {
    let cancel = json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true});

    if let Err(error) = context.client.pi(&sid, cancel).await {
        report_failure(&context, "cancel dialog", error.to_string());

        return;
    }

    context.events.send(super::Event::RequestFailed {
        detail: format!(
            "an extension asked a `{method}` question; Harvester cannot show dialogs yet, \
             so it was cancelled"
        ),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dialog_event(method: &str) -> Value {
        json!({
            "type": "extension_ui_request",
            "id": "dlg-1",
            "method": method,
            "params": {"message": "pick one", "options": ["a", "b"]},
        })
    }

    #[test]
    fn every_blocking_dialog_method_is_recognised() {
        for method in DIALOG_METHODS {
            assert_eq!(
                forwarded_dialog(&dialog_event(method)),
                Some(("dlg-1", method)),
                "{method} blocks pi until answered, so it must be cancelled"
            );
        }
    }

    #[test]
    fn fire_and_forget_ui_methods_are_left_alone() {
        // docs/DAEMON.md §Dialogs: forwarded, never answered — cancelling one
        // would send pi a response for an id it is not waiting on.
        for method in ["notify", "setStatus", "setWidget", "setTitle"] {
            assert_eq!(forwarded_dialog(&dialog_event(method)), None);
        }
    }

    #[test]
    fn ordinary_pi_events_are_not_dialogs() {
        assert_eq!(forwarded_dialog(&json!({"type": "agent_settled"})), None);
        assert_eq!(
            forwarded_dialog(&json!({"type": "message_update", "method": "select"})),
            None,
            "the type is what makes it a dialog request, not the method"
        );
    }

    #[test]
    fn a_dialog_without_a_string_id_is_not_answerable() {
        let mut event = dialog_event("confirm");
        event["id"] = json!(7);

        assert_eq!(forwarded_dialog(&event), None);
    }

    #[test]
    fn the_replayed_copy_of_a_dialog_is_answered_only_once() {
        // The daemon replays every pending dialog to a client that attaches
        // later (docs/DAEMON.md §Dialogs), so the same id arrives twice by
        // design; the second cancel must be dropped, not sent.
        let answered = AnsweredDialogs::default();

        assert!(answered.claim("01H", "dlg-1"), "first sighting cancels");
        assert!(
            !answered.claim("01H", "dlg-1"),
            "the replay must not cancel again"
        );
    }

    #[test]
    fn the_same_dialog_id_in_another_session_is_a_different_dialog() {
        let answered = AnsweredDialogs::default();

        assert!(answered.claim("01H", "dlg-1"));
        assert!(answered.claim("02H", "dlg-1"));
    }
}
