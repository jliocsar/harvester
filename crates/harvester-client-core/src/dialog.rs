//! Pi's dialog vocabulary: the four questions an extension can block on, and
//! the three shapes an answer takes.
//!
//! This is the one place that reads a dialog's payload. Everywhere else the
//! client speaks Pi verbatim — but a modal has to know whether it is drawing a
//! list, a yes/no or a text box, and *that* decision is pi's wire format, not a
//! rendering choice. Keeping it here rather than in the gpui crate is what lets
//! the whole vocabulary be tested without a window.
//!
//! **Pi-version-coupled** (pi 0.83.0, `dist/modes/rpc/rpc-types.d.ts:385-455`).
//! Two things about the shape are easy to get wrong and are the reason this
//! module exists:
//!
//! - Both envelopes are **flat**. There is no `params` object: `title`,
//!   `options`, `message` and friends sit beside `type`/`id`/`method`, because
//!   pi spreads the request into the envelope (`rpc-mode.js:76`).
//! - The answer field is **not** called `response`. It is `value` (a string —
//!   for `select` the option's *text*, never its index), `confirmed` (a bool),
//!   or `cancelled: true`.

use serde_json::{json, Value};

/// The `extension_ui_request` methods that block pi until they are answered.
/// The fire-and-forget ones (`notify`, `setStatus`, …) are forwarded and never
/// answered, so they are deliberately absent — [`DialogRequest::parse`]
/// returning `None` for them is what keeps a response off the wire for an id
/// pi is not waiting on.
const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// A question an extension is blocked on.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogRequest {
    /// Pi's dialog id, echoed back in the answer. The only correlation there
    /// is — a response carries no method.
    pub id: String,
    pub title: String,
    pub kind: DialogKind,
}

/// Which of pi's four dialogs this is, carrying only what that one needs.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogKind {
    /// One of `options`. Pi wants the chosen string back, not its index.
    Select { options: Vec<String> },
    /// A yes/no over `message`.
    Confirm { message: String },
    /// One line of text. `placeholder` is a hint, not a default: pi has no
    /// field for a prefilled `input`.
    Input { placeholder: Option<String> },
    /// Multi-line text, optionally starting from `prefill`.
    ///
    /// Alone among the four it has **no timeout and no abort path** in pi
    /// (`rpc-mode.js:173-192`), so an unanswered `editor` blocks the extension
    /// for the life of the process. Nothing rescues it but an answer.
    Editor { prefill: Option<String> },
}

/// What the user decided.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogAnswer {
    /// `select`, `input` and `editor` all answer with a string.
    Value(String),
    /// `confirm` only.
    Confirmed(bool),
    /// Escape, or a question Harvester could not put on screen. Identical to a
    /// human dismissing the dialog in pi's own TUI.
    Cancelled,
}

impl DialogRequest {
    /// The dialog `event` asks, or `None` when it is not a blocking dialog
    /// request at all — an ordinary pi event, a fire-and-forget UI method, or
    /// a payload too malformed to answer.
    ///
    /// A missing `title` is not fatal: the dialog still blocks pi, so it is
    /// better to show it untitled than to leave it unanswerable.
    pub fn parse(event: &Value) -> Option<DialogRequest> {
        if event.get("type").and_then(Value::as_str)? != "extension_ui_request" {
            return None;
        }

        let method = event.get("method").and_then(Value::as_str)?;

        if !DIALOG_METHODS.contains(&method) {
            return None;
        }

        let id = event.get("id").and_then(Value::as_str)?;
        let title = string_field(event, "title").unwrap_or_default();
        let kind = parse_kind(event, method)?;

        Some(DialogRequest {
            id: id.to_string(),
            title,
            kind,
        })
    }
}

/// The method-specific half of a request. `None` only when a method's required
/// field is missing or the wrong type — an answer would be a guess.
fn parse_kind(event: &Value, method: &str) -> Option<DialogKind> {
    match method {
        "select" => {
            let options = event.get("options")?.as_array()?;
            let options: Vec<String> = options
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();

            // Nothing to pick from is not a question anyone can answer.
            if options.is_empty() {
                return None;
            }

            Some(DialogKind::Select { options })
        }
        "confirm" => Some(DialogKind::Confirm {
            message: string_field(event, "message").unwrap_or_default(),
        }),
        "input" => Some(DialogKind::Input {
            placeholder: string_field(event, "placeholder"),
        }),
        "editor" => Some(DialogKind::Editor {
            prefill: string_field(event, "prefill"),
        }),
        _ => None,
    }
}

/// Pi omits absent optionals rather than sending `null` (it serializes with
/// `JSON.stringify`), so absent and null are the same answer here.
fn string_field(event: &Value, field: &str) -> Option<String> {
    event.get(field).and_then(Value::as_str).map(str::to_string)
}

impl DialogAnswer {
    /// The `extension_ui_response` line for `dialog_id`.
    ///
    /// Exactly one of `value`/`confirmed`/`cancelled` is set, because pi reads
    /// them in that priority and a second field would be silently ignored
    /// (`rpc-mode.js:83-85`).
    pub fn to_response(&self, dialog_id: &str) -> Value {
        match self {
            DialogAnswer::Value(value) => {
                json!({"type": "extension_ui_response", "id": dialog_id, "value": value})
            }
            DialogAnswer::Confirmed(confirmed) => {
                json!({"type": "extension_ui_response", "id": dialog_id, "confirmed": confirmed})
            }
            DialogAnswer::Cancelled => {
                json!({"type": "extension_ui_response", "id": dialog_id, "cancelled": true})
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_select_carries_its_options_from_the_flat_envelope() {
        // The trap this module exists for: no `params` wrapper anywhere.
        let event = json!({
            "type": "extension_ui_request",
            "id": "dlg-1",
            "method": "select",
            "title": "Which branch?",
            "options": ["main", "backend-hardening"],
        });

        assert_eq!(
            DialogRequest::parse(&event),
            Some(DialogRequest {
                id: "dlg-1".to_string(),
                title: "Which branch?".to_string(),
                kind: DialogKind::Select {
                    options: vec!["main".to_string(), "backend-hardening".to_string()],
                },
            })
        );
    }

    #[test]
    fn every_blocking_method_parses() {
        let events = [
            json!({"type": "extension_ui_request", "id": "d", "method": "select",
                   "title": "t", "options": ["a"]}),
            json!({"type": "extension_ui_request", "id": "d", "method": "confirm",
                   "title": "t", "message": "sure?"}),
            json!({"type": "extension_ui_request", "id": "d", "method": "input",
                   "title": "t", "placeholder": "name"}),
            json!({"type": "extension_ui_request", "id": "d", "method": "editor",
                   "title": "t", "prefill": "body"}),
        ];

        for event in events {
            assert!(
                DialogRequest::parse(&event).is_some(),
                "{event} blocks pi, so it must be answerable"
            );
        }
    }

    #[test]
    fn fire_and_forget_ui_methods_are_not_dialogs() {
        // docs/DAEMON.md §Dialogs: forwarded, never answered. Answering one
        // would send pi a response for an id it is not waiting on.
        for method in ["notify", "setStatus", "setWidget", "setTitle"] {
            let event = json!({
                "type": "extension_ui_request", "id": "d", "method": method, "title": "t",
            });

            assert_eq!(DialogRequest::parse(&event), None);
        }
    }

    #[test]
    fn ordinary_pi_events_are_not_dialogs() {
        assert_eq!(
            DialogRequest::parse(&json!({"type": "agent_settled"})),
            None
        );
        assert_eq!(
            DialogRequest::parse(&json!({"type": "message_update", "method": "select"})),
            None,
            "the type is what makes it a dialog request, not the method"
        );
    }

    #[test]
    fn a_dialog_without_a_string_id_is_not_answerable() {
        let event = json!({"type": "extension_ui_request", "id": 7, "method": "confirm",
                           "title": "t", "message": "m"});

        assert_eq!(DialogRequest::parse(&event), None);
    }

    #[test]
    fn a_select_with_nothing_to_select_is_refused() {
        for options in [json!([]), json!("main"), json!([7])] {
            let event = json!({"type": "extension_ui_request", "id": "d", "method": "select",
                               "title": "t", "options": options});

            assert_eq!(
                DialogRequest::parse(&event),
                None,
                "{options} offers no answerable choice"
            );
        }
    }

    #[test]
    fn the_optional_fields_pi_omits_are_absent_not_empty() {
        // `JSON.stringify` drops undefined, so a placeholder-less input has no
        // `placeholder` key at all — not `null`, and not `""`.
        let input = json!({"type": "extension_ui_request", "id": "d", "method": "input",
                           "title": "t"});
        let editor = json!({"type": "extension_ui_request", "id": "d", "method": "editor",
                            "title": "t"});

        assert_eq!(
            DialogRequest::parse(&input).map(|dialog| dialog.kind),
            Some(DialogKind::Input { placeholder: None })
        );
        assert_eq!(
            DialogRequest::parse(&editor).map(|dialog| dialog.kind),
            Some(DialogKind::Editor { prefill: None })
        );
    }

    #[test]
    fn a_titleless_dialog_is_shown_rather_than_dropped() {
        // It still blocks pi; refusing to parse it would strand the agent.
        let event = json!({"type": "extension_ui_request", "id": "d", "method": "confirm",
                           "message": "m"});

        assert_eq!(
            DialogRequest::parse(&event).map(|dialog| dialog.title),
            Some(String::new())
        );
    }

    #[test]
    fn a_select_is_answered_with_the_option_text_not_its_index() {
        // pi hands the extension whatever `value` holds, with no type check
        // (`rpc-mode.js:83`), so an index would arrive as a number where a
        // string is typed — a bug the extension would report, not us.
        let answer = DialogAnswer::Value("backend-hardening".to_string());

        assert_eq!(
            answer.to_response("dlg-1"),
            json!({"type": "extension_ui_response", "id": "dlg-1",
                   "value": "backend-hardening"})
        );
    }

    #[test]
    fn a_confirm_answers_with_confirmed_and_never_with_value() {
        for confirmed in [true, false] {
            assert_eq!(
                DialogAnswer::Confirmed(confirmed).to_response("dlg-1"),
                json!({"type": "extension_ui_response", "id": "dlg-1",
                       "confirmed": confirmed})
            );
        }
    }

    #[test]
    fn declining_a_confirm_and_cancelling_it_are_the_same_answer_to_pi() {
        // Different bytes, identical outcome: pi resolves a cancelled
        // `confirm` to `false`, which is what `confirmed: false` says too
        // (`rpc-mode.js:84`). That equivalence is why the modal is free to map
        // Escape onto "No" — the one dialog where dismissing is a real answer
        // rather than a refusal to give one.
        assert_ne!(
            DialogAnswer::Confirmed(false).to_response("d"),
            DialogAnswer::Cancelled.to_response("d"),
            "the wire shapes differ"
        );
    }

    #[test]
    fn a_cancel_carries_nothing_but_the_flag() {
        // pi checks `cancelled` first and ignores everything else, so sending a
        // value alongside it would be dead weight that only confuses a reader.
        assert_eq!(
            DialogAnswer::Cancelled.to_response("dlg-1"),
            json!({"type": "extension_ui_response", "id": "dlg-1", "cancelled": true})
        );
    }
}
