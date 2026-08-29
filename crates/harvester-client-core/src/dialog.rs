use std::collections::{HashMap, VecDeque};

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

    pub fn dismissal_for(request: &DialogRequest) -> Self {
        match &request.kind {
            DialogKind::Confirm { .. } => DialogAnswer::Confirmed(false),
            DialogKind::Select { .. } | DialogKind::Input { .. } | DialogKind::Editor { .. } => {
                DialogAnswer::Cancelled
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogRoom {
    pub machine: String,
    pub sid: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogObligation {
    pub machine: String,
    pub sid: String,
    pub request: DialogRequest,
}

impl DialogObligation {
    pub fn room(&self) -> DialogRoom {
        DialogRoom {
            machine: self.machine.clone(),
            sid: self.sid.clone(),
        }
    }

    pub fn key(&self) -> DialogKey {
        DialogKey {
            room: self.room(),
            dialog_id: self.request.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogKey {
    pub room: DialogRoom,
    pub dialog_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    Accepted,
    Rejected,
}

impl EnqueueResult {
    #[must_use]
    pub fn accepted(self) -> bool {
        self == EnqueueResult::Accepted
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct AnsweredDialog {
    pub obligation: DialogObligation,
    pub answer: DialogAnswer,
}

#[derive(Debug, Default)]
pub struct DialogObligations {
    rooms_by_machine: HashMap<String, DialogRoom>,
    pending: VecDeque<DialogObligation>,
}

impl DialogObligations {
    pub fn enter_room(&mut self, room: DialogRoom) -> Vec<DialogObligation> {
        if self.rooms_by_machine.get(&room.machine) == Some(&room) {
            return Vec::new();
        }

        self.rooms_by_machine
            .insert(room.machine.clone(), room.clone());

        self.retire_machine(&room.machine)
    }

    pub fn leave_machine(&mut self, machine: &str) -> Vec<DialogObligation> {
        if self.rooms_by_machine.remove(machine).is_none() {
            return Vec::new();
        }

        self.retire_machine(machine)
    }

    pub fn retire_room(&mut self, machine: &str, sid: &str) -> Vec<DialogObligation> {
        let room = DialogRoom {
            machine: machine.to_string(),
            sid: sid.to_string(),
        };

        if self.rooms_by_machine.get(machine) == Some(&room) {
            self.rooms_by_machine.remove(machine);
        }

        self.retire(|obligation| obligation.room() != room)
    }

    pub fn enqueue(&mut self, obligation: DialogObligation) -> EnqueueResult {
        if self.rooms_by_machine.get(&obligation.machine) != Some(&obligation.room()) {
            return EnqueueResult::Rejected;
        }

        if self
            .pending
            .iter()
            .any(|pending| pending.key() == obligation.key())
        {
            return EnqueueResult::Rejected;
        }

        self.pending.push_back(obligation);

        EnqueueResult::Accepted
    }

    pub fn front(&self) -> Option<&DialogObligation> {
        self.pending.front()
    }

    pub fn answer_front(
        &mut self,
        key: &DialogKey,
        answer: DialogAnswer,
    ) -> Option<AnsweredDialog> {
        if self.pending.front().map(DialogObligation::key).as_ref() != Some(key) {
            return None;
        }

        self.pending
            .pop_front()
            .map(|obligation| AnsweredDialog { obligation, answer })
    }

    fn retire_machine(&mut self, machine: &str) -> Vec<DialogObligation> {
        self.retire(|obligation| obligation.machine != machine)
    }

    fn retire(&mut self, keep: impl Fn(&DialogObligation) -> bool) -> Vec<DialogObligation> {
        let (retained, retired): (Vec<DialogObligation>, Vec<DialogObligation>) =
            self.pending.drain(..).partition(keep);

        self.pending = VecDeque::from(retained);

        retired
    }

    #[cfg(test)]
    fn room(&self, machine: &str) -> Option<&DialogRoom> {
        self.rooms_by_machine.get(machine)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.pending.is_empty()
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

    fn room(machine: &str, sid: &str) -> DialogRoom {
        DialogRoom {
            machine: machine.to_string(),
            sid: sid.to_string(),
        }
    }

    fn question(machine: &str, sid: &str, id: &str) -> DialogObligation {
        DialogObligation {
            machine: machine.to_string(),
            sid: sid.to_string(),
            request: DialogRequest {
                id: id.to_string(),
                title: "Which branch?".to_string(),
                kind: DialogKind::Confirm {
                    message: "sure?".to_string(),
                },
            },
        }
    }

    #[test]
    fn a_forwarded_dialog_is_visible_only_in_its_active_room() {
        let mut obligations = DialogObligations::default();
        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Rejected
        );

        obligations.enter_room(room("vm", "sid-1"));
        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Accepted
        );
    }

    #[test]
    fn replay_is_a_duplicate_not_a_second_obligation() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));

        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Rejected
        );
        assert_eq!(obligations.len(), 1);
    }

    #[test]
    fn obligations_advance_fifo_and_only_the_front_can_be_answered() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));
        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-2")),
            EnqueueResult::Accepted
        );

        let first_key = obligations.front().expect("first obligation").key();
        let answered = obligations
            .answer_front(&first_key, DialogAnswer::Confirmed(true))
            .expect("front obligation");
        assert_eq!(answered.obligation.request.id, "d-1");
        assert_eq!(
            obligations.front().map(|dialog| dialog.request.id.as_str()),
            Some("d-2")
        );

        let second_key = obligations.front().expect("second obligation").key();
        assert!(obligations
            .answer_front(&second_key, DialogAnswer::Confirmed(false))
            .is_some());
        assert!(obligations
            .answer_front(&second_key, DialogAnswer::Confirmed(false))
            .is_none());
    }

    #[test]
    fn retirement_never_sends_an_answer_and_advances_the_visible_front() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));
        obligations.enqueue(question("vm", "sid-1", "d-2"));

        let retired = obligations.retire_room("vm", "sid-1");

        assert_eq!(retired.len(), 2);
        assert!(obligations.front().is_none());
        assert!(obligations.room("vm").is_none());
    }

    #[test]
    fn switching_rooms_retires_old_questions_without_client_cancellation() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "old"));
        obligations.enqueue(question("vm", "old", "d-1"));

        let retired = obligations.enter_room(room("vm", "new"));

        assert_eq!(retired[0].sid, "old");
        assert_eq!(obligations.room("vm"), Some(&room("vm", "new")));
        assert_eq!(
            obligations.enqueue(question("vm", "old", "late")),
            EnqueueResult::Rejected
        );
        assert_eq!(
            obligations.enqueue(question("vm", "new", "d-2")),
            EnqueueResult::Accepted
        );
    }

    #[test]
    fn an_unrelated_bridge_disconnect_does_not_retire_the_active_room() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));

        assert!(obligations.leave_machine("other").is_empty());
        assert_eq!(
            obligations.front().map(|dialog| dialog.request.id.as_str()),
            Some("d-1")
        );
    }

    #[test]
    fn disconnect_leaves_no_stale_modal_obligations() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));

        let retired = obligations.leave_machine("vm");

        assert_eq!(retired.len(), 1);
        assert!(obligations.room("vm").is_none());
        assert!(obligations.front().is_none());
    }

    #[test]
    fn rooms_on_two_machines_share_one_fifo_without_retiring_each_other() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("machine-a", "sid-a"));
        obligations.enter_room(room("machine-b", "sid-b"));

        assert_eq!(
            obligations.enqueue(question("machine-a", "sid-a", "dialog-a")),
            EnqueueResult::Accepted
        );
        assert_eq!(
            obligations.enqueue(question("machine-b", "sid-b", "dialog-b")),
            EnqueueResult::Accepted
        );
        assert_eq!(obligations.len(), 2);

        let retired = obligations.enter_room(room("machine-b", "sid-b-2"));

        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].request.id, "dialog-b");
        assert_eq!(
            obligations.front().map(|dialog| dialog.request.id.as_str()),
            Some("dialog-a")
        );
        assert_eq!(
            obligations.room("machine-a"),
            Some(&room("machine-a", "sid-a"))
        );
    }

    #[test]
    fn attachment_leave_and_disconnect_retire_only_the_matching_machine_room() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("machine-a", "shared-sid"));
        obligations.enter_room(room("machine-b", "shared-sid"));
        obligations.enqueue(question("machine-a", "shared-sid", "dialog-a"));
        obligations.enqueue(question("machine-b", "shared-sid", "dialog-b"));

        let retired = obligations.retire_room("machine-b", "shared-sid");

        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].machine, "machine-b");
        assert!(obligations.room("machine-a").is_some());
        assert!(obligations.room("machine-b").is_none());

        let retired = obligations.leave_machine("machine-a");

        assert_eq!(retired.len(), 1);
        assert!(obligations.is_empty());
    }

    #[test]
    fn duplicate_identity_includes_the_room() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("machine-a", "sid-a"));
        obligations.enter_room(room("machine-b", "sid-b"));
        obligations.enqueue(question("machine-a", "sid-a", "dialog"));

        assert_eq!(
            obligations.enqueue(question("machine-b", "sid-b", "dialog")),
            EnqueueResult::Accepted
        );
        assert_eq!(obligations.len(), 2);
    }

    #[test]
    fn a_stale_modal_callback_cannot_answer_the_next_obligation() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid"));
        obligations.enqueue(question("vm", "sid", "first"));
        obligations.enqueue(question("vm", "sid", "second"));
        let first_key = obligations.front().expect("first").key();

        obligations
            .answer_front(&first_key, DialogAnswer::Confirmed(true))
            .expect("first answer");

        assert!(obligations
            .answer_front(&first_key, DialogAnswer::Confirmed(false))
            .is_none());
        assert_eq!(
            obligations.front().map(|dialog| dialog.request.id.as_str()),
            Some("second")
        );
    }

    #[test]
    fn a_stale_session_exit_cannot_close_the_room_that_replaced_it() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "old"));
        obligations.enter_room(room("vm", "new"));

        let retired = obligations.retire_room("vm", "old");

        assert!(retired.is_empty());
        assert_eq!(obligations.room("vm"), Some(&room("vm", "new")));
        assert_eq!(
            obligations.enqueue(question("vm", "new", "d-1")),
            EnqueueResult::Accepted
        );
    }

    #[test]
    fn dismissal_matches_pi_dialog_semantics() {
        let confirm = DialogRequest {
            id: "confirm".to_string(),
            title: String::new(),
            kind: DialogKind::Confirm {
                message: String::new(),
            },
        };
        let input = DialogRequest {
            id: "input".to_string(),
            title: String::new(),
            kind: DialogKind::Input { placeholder: None },
        };

        assert_eq!(
            DialogAnswer::dismissal_for(&confirm),
            DialogAnswer::Confirmed(false)
        );
        assert_eq!(DialogAnswer::dismissal_for(&input), DialogAnswer::Cancelled);
    }
}
