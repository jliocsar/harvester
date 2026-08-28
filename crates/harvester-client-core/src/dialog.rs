use serde_json::{json, Value};

const DIALOG_METHODS: [&str; 4] = ["select", "confirm", "input", "editor"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogRequest {
    pub id: String,
    pub title: String,
    pub kind: DialogKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogKind {
    Select { options: Vec<String> },
    Confirm { message: String },
    Input { placeholder: Option<String> },
    Editor { prefill: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogAnswer {
    Value(String),
    Confirmed(bool),
    Cancelled,
}

impl DialogRequest {
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

fn parse_kind(event: &Value, method: &str) -> Option<DialogKind> {
    match method {
        "select" => {
            let options = event.get("options")?.as_array()?;
            let options: Vec<String> = options
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();

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

fn string_field(event: &Value, field: &str) -> Option<String> {
    event.get(field).and_then(Value::as_str).map(str::to_string)
}

impl DialogAnswer {
    pub(crate) fn to_response(&self, dialog_id: &str) -> Value {
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
        let event = json!({"type": "extension_ui_request", "id": "d", "method": "confirm",
                           "message": "m"});

        assert_eq!(
            DialogRequest::parse(&event).map(|dialog| dialog.title),
            Some(String::new())
        );
    }

    #[test]
    fn a_select_is_answered_with_the_option_text_not_its_index() {
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
        assert_ne!(
            DialogAnswer::Confirmed(false).to_response("d"),
            DialogAnswer::Cancelled.to_response("d"),
            "the wire shapes differ"
        );
    }

    #[test]
    fn a_cancel_carries_nothing_but_the_flag() {
        assert_eq!(
            DialogAnswer::Cancelled.to_response("dlg-1"),
            json!({"type": "extension_ui_response", "id": "dlg-1", "cancelled": true})
        );
    }
}
