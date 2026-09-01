use std::collections::{HashSet, VecDeque};

use harvester_proto::{DIALOG_METHODS, PI_EXTENSION_UI_RESPONSE};
use serde_json::{json, Value};

use crate::SessionKey;

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
    pub fn parse(event: &Value) -> Option<Self> {
        if event.get("type").and_then(Value::as_str)? != "extension_ui_request" {
            return None;
        }

        let method = event.get("method").and_then(Value::as_str)?;

        if !DIALOG_METHODS.contains(&method) {
            return None;
        }

        let id = event.get("id").and_then(Value::as_str)?;
        let title = event
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let kind = parse_kind(event, method)?;

        Some(Self {
            id: id.to_string(),
            title,
            kind,
        })
    }
}

fn parse_kind(event: &Value, method: &str) -> Option<DialogKind> {
    match method {
        "select" => parse_select(event),
        "confirm" => Some(DialogKind::Confirm {
            message: event
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        }),
        "input" => Some(DialogKind::Input {
            placeholder: event
                .get("placeholder")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        "editor" => Some(DialogKind::Editor {
            prefill: event
                .get("prefill")
                .and_then(Value::as_str)
                .map(str::to_string),
        }),
        _ => None,
    }
}

fn parse_select(event: &Value) -> Option<DialogKind> {
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

impl DialogAnswer {
    pub(crate) fn to_response(&self, dialog_id: &str) -> Value {
        match self {
            Self::Value(value) => {
                json!({"type": PI_EXTENSION_UI_RESPONSE, "id": dialog_id, "value": value})
            }
            Self::Confirmed(confirmed) => {
                json!({"type": PI_EXTENSION_UI_RESPONSE, "id": dialog_id,
                       "confirmed": confirmed})
            }
            Self::Cancelled => {
                json!({"type": PI_EXTENSION_UI_RESPONSE, "id": dialog_id, "cancelled": true})
            }
        }
    }

    pub const fn dismissal_for(request: &DialogRequest) -> Self {
        match &request.kind {
            DialogKind::Confirm { .. } => Self::Confirmed(false),
            DialogKind::Select { .. } | DialogKind::Input { .. } | DialogKind::Editor { .. } => {
                Self::Cancelled
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DialogRoom {
    pub(crate) session: SessionKey,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogObligation {
    pub session: SessionKey,
    pub request: DialogRequest,
}

impl DialogObligation {
    pub(crate) fn room(&self) -> DialogRoom {
        DialogRoom {
            session: self.session.clone(),
        }
    }

    pub fn key(&self) -> DialogKey {
        DialogKey::new(self.session.clone(), self.request.id.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogKey {
    session: SessionKey,
    dialog_id: String,
}

impl DialogKey {
    pub(crate) fn new(session: SessionKey, dialog_id: String) -> Self {
        Self { session, dialog_id }
    }

    pub(crate) const fn session(&self) -> &SessionKey {
        &self.session
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct AnsweredDialog {
    pub(crate) obligation: DialogObligation,
    pub(crate) answer: DialogAnswer,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct DialogObligations {
    rooms: HashSet<DialogRoom>,
    pending: VecDeque<DialogObligation>,
}

impl DialogObligations {
    pub(crate) fn enter_room(&mut self, room: DialogRoom) {
        self.rooms.insert(room);
    }

    pub(crate) fn leave_room(&mut self, session: &SessionKey) -> Vec<DialogObligation> {
        self.rooms.remove(&DialogRoom {
            session: session.clone(),
        });
        self.retire(|obligation| &obligation.session != session)
    }

    pub(crate) fn leave_machine(&mut self, machine: &crate::MachineId) -> Vec<DialogObligation> {
        self.rooms.retain(|room| room.session.machine() != machine);
        self.retire(|obligation| obligation.session.machine() != machine)
    }

    pub(crate) fn enqueue(&mut self, obligation: DialogObligation) -> bool {
        if !self.rooms.contains(&obligation.room()) {
            return false;
        }

        if self
            .pending
            .iter()
            .any(|pending| pending.key() == obligation.key())
        {
            return false;
        }

        self.pending.push_back(obligation);

        true
    }

    pub(crate) fn front(&self) -> Option<&DialogObligation> {
        self.pending.front()
    }

    pub(crate) fn answer_front(
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

    fn retire(&mut self, keep: impl Fn(&DialogObligation) -> bool) -> Vec<DialogObligation> {
        let (retained, retired): (Vec<DialogObligation>, Vec<DialogObligation>) =
            self.pending.drain(..).partition(keep);

        self.pending = VecDeque::from(retained);

        retired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MachineId, SessionId};

    fn session(machine: &str, sid: &str) -> SessionKey {
        SessionKey::new(
            MachineId::new(machine).expect("non-empty"),
            SessionId::new(sid).expect("non-empty"),
        )
    }

    fn question(session: SessionKey, id: &str) -> DialogObligation {
        DialogObligation {
            session,
            request: DialogRequest {
                id: id.to_string(),
                title: "Question".to_string(),
                kind: DialogKind::Confirm {
                    message: "sure?".to_string(),
                },
            },
        }
    }

    #[test]
    fn every_blocking_method_parses_from_the_flat_pi_envelope() {
        for event in [
            json!({"type": "extension_ui_request", "id": "d", "method": "select",
                   "title": "t", "options": ["a"]}),
            json!({"type": "extension_ui_request", "id": "d", "method": "confirm",
                   "title": "t", "message": "sure?"}),
            json!({"type": "extension_ui_request", "id": "d", "method": "input",
                   "title": "t", "placeholder": "name"}),
            json!({"type": "extension_ui_request", "id": "d", "method": "editor",
                   "title": "t", "prefill": "body"}),
        ] {
            assert!(DialogRequest::parse(&event).is_some());
        }
    }

    #[test]
    fn fire_and_forget_methods_are_not_obligations() {
        for method in ["notify", "setStatus", "setWidget", "setTitle"] {
            let event = json!({
                "type": "extension_ui_request", "id": "d", "method": method, "title": "t"
            });

            assert!(DialogRequest::parse(&event).is_none());
        }
    }

    #[test]
    fn several_attached_sessions_on_one_machine_keep_independent_rooms() {
        let first = session("machine", "first");
        let second = session("machine", "second");
        let mut obligations = DialogObligations::default();

        obligations.enter_room(DialogRoom {
            session: first.clone(),
        });
        obligations.enter_room(DialogRoom {
            session: second.clone(),
        });
        assert!(obligations.enqueue(question(first.clone(), "one")));
        assert!(obligations.enqueue(question(second.clone(), "two")));

        let retired = obligations.leave_room(&first);

        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].session, first);
        assert_eq!(obligations.front().map(|item| &item.session), Some(&second));
    }

    #[test]
    fn duplicate_replay_is_not_a_second_obligation() {
        let key = session("machine", "sid");
        let mut obligations = DialogObligations::default();

        obligations.enter_room(DialogRoom {
            session: key.clone(),
        });
        assert!(obligations.enqueue(question(key.clone(), "dialog")));
        assert!(!obligations.enqueue(question(key, "dialog")));
    }

    #[test]
    fn only_the_visible_front_can_be_answered() {
        let session = session("machine", "sid");
        let mut obligations = DialogObligations::default();

        obligations.enter_room(DialogRoom {
            session: session.clone(),
        });
        obligations.enqueue(question(session.clone(), "first"));
        obligations.enqueue(question(session, "second"));
        let first = obligations.front().expect("front").key();

        assert!(obligations
            .answer_front(&first, DialogAnswer::Confirmed(true))
            .is_some());
        assert!(obligations
            .answer_front(&first, DialogAnswer::Confirmed(false))
            .is_none());
        assert_eq!(
            obligations.front().map(|item| item.request.id.as_str()),
            Some("second")
        );
    }

    #[test]
    fn machine_disconnect_retires_only_that_machines_questions() {
        let first = session("first", "sid");
        let second = session("second", "sid");
        let mut obligations = DialogObligations::default();

        for key in [&first, &second] {
            obligations.enter_room(DialogRoom {
                session: key.clone(),
            });
            obligations.enqueue(question(key.clone(), key.machine().as_str()));
        }

        let retired = obligations.leave_machine(first.machine());

        assert_eq!(retired.len(), 1);
        assert_eq!(obligations.front().map(|item| &item.session), Some(&second));
    }

    #[test]
    fn dismissal_matches_pi_semantics() {
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
