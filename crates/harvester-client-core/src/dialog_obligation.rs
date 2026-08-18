//! The client-side obligation created by a forwarded Pi dialog.
//!
//! A daemon forwards a blocking dialog while a client is attached and Pi stays
//! suspended until that dialog is answered. This module owns the small state
//! machine around that fact: which room is active, which questions are owed,
//! duplicate replay, FIFO order, and retirement when the room or session goes
//! away. It is deliberately window-free; a GPUI modal is only an adapter for
//! the front obligation.
//!
//! Empty-room cancellation remains daemon policy. Leaving a room here only
//! retires the client's view of questions that the daemon has already
//! cancelled (or is about to cancel); it never manufactures a cancellation
//! answer.

use std::collections::VecDeque;

use crate::{DialogAnswer, DialogRequest};

/// The machine/session pair whose attached client is receiving dialogs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogRoom {
    pub machine: String,
    pub sid: String,
}

impl DialogRoom {
    pub fn new(machine: String, sid: String) -> Self {
        Self { machine, sid }
    }
}

/// A blocking Pi dialog, plus the route its typed answer must take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialogObligation {
    pub machine: String,
    pub sid: String,
    pub request: DialogRequest,
}

impl DialogObligation {
    pub fn new(machine: String, sid: String, request: DialogRequest) -> Self {
        Self {
            machine,
            sid,
            request,
        }
    }

    pub fn room(&self) -> DialogRoom {
        DialogRoom::new(self.machine.clone(), self.sid.clone())
    }

    pub fn key(&self) -> DialogKey {
        DialogKey {
            room: self.room(),
            dialog_id: self.request.id.clone(),
        }
    }
}

/// Stable identity for duplicate replay and for the GPUI adapter's input map.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DialogKey {
    pub room: DialogRoom,
    pub dialog_id: String,
}

/// Why an enqueue did not create another modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueResult {
    /// The obligation is now the front one and should be shown.
    Visible,
    /// The obligation is owed, but an older one is already visible.
    Queued,
    /// The daemon replayed an obligation already present in this room.
    Duplicate,
    /// The event came from no active room or from a room left behind.
    Ignored,
}

/// Questions removed without an answer because Pi is no longer waiting on
/// them. The adapter uses this to discard view-only input state and to close
/// the modal only when its front obligation was retired.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Retirement {
    pub retired: Vec<DialogObligation>,
    pub front_removed: bool,
}

/// The one answer that was accepted for the front obligation.
#[derive(Debug, PartialEq, Eq)]
pub struct AnsweredDialog {
    pub obligation: DialogObligation,
    pub answer: DialogAnswer,
}

/// Window-free state for the dialog obligation policy.
#[derive(Debug, Default)]
pub struct DialogObligations {
    room: Option<DialogRoom>,
    pending: VecDeque<DialogObligation>,
}

impl DialogObligations {
    /// Enter an attachment room. Questions from the previous room are
    /// retired without sending answers; the daemon's empty-room policy owns
    /// that cancellation.
    pub fn enter_room(&mut self, room: DialogRoom) -> Retirement {
        if self.room.as_ref() == Some(&room) {
            return Retirement::default();
        }

        self.room = Some(room.clone());
        self.retire(|obligation| obligation.room() == room)
    }

    /// Leave every room, as when the active bridge disconnects. No
    /// cancellation is sent here because an empty room is daemon policy, not
    /// client policy.
    pub fn leave_room(&mut self) -> Retirement {
        self.room = None;
        self.retire(|_| false)
    }

    /// A bridge can disconnect even when it is not the active dialog room.
    /// Only that machine's loss may retire the active room.
    pub fn leave_machine(&mut self, machine: &str) -> Retirement {
        let active_machine = self.room.as_ref().map(|room| room.machine.as_str());

        if active_machine != Some(machine) {
            return Retirement::default();
        }

        self.leave_room()
    }

    /// Pi's authoritative session-exit event retires that session's dialogs.
    /// If it was the active room, there is no longer an attached room either.
    pub fn retire_session(&mut self, sid: &str) -> Retirement {
        if self.room.as_ref().is_some_and(|room| room.sid == sid) {
            self.room = None;
        }

        self.retire(|obligation| obligation.sid != sid)
    }

    /// Add a forwarded request if it belongs to the currently attached room.
    /// Pi replays pending requests on attach, so identity is room + dialog id,
    /// not the payload's complete value.
    pub fn enqueue(&mut self, obligation: DialogObligation) -> EnqueueResult {
        if self.room.as_ref() != Some(&obligation.room()) {
            return EnqueueResult::Ignored;
        }

        if self
            .pending
            .iter()
            .any(|pending| pending.key() == obligation.key())
        {
            return EnqueueResult::Duplicate;
        }

        let visible = self.pending.is_empty();
        self.pending.push_back(obligation);

        if visible {
            EnqueueResult::Visible
        } else {
            EnqueueResult::Queued
        }
    }

    pub fn room(&self) -> Option<&DialogRoom> {
        self.room.as_ref()
    }

    pub fn front(&self) -> Option<&DialogObligation> {
        self.pending.front()
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    /// Accept exactly one answer. A second callback after the modal was
    /// closed finds no front obligation and therefore cannot answer Pi twice.
    pub fn answer_front(&mut self, answer: DialogAnswer) -> Option<AnsweredDialog> {
        self.pending
            .pop_front()
            .map(|obligation| AnsweredDialog { obligation, answer })
    }

    fn retire(&mut self, keep: impl Fn(&DialogObligation) -> bool) -> Retirement {
        let old_front = self.pending.front().map(DialogObligation::key);
        let mut retired = Vec::new();
        let mut retained = VecDeque::new();

        while let Some(obligation) = self.pending.pop_front() {
            if keep(&obligation) {
                retained.push_back(obligation);
            } else {
                retired.push(obligation);
            }
        }

        let new_front = retained.front().map(DialogObligation::key);
        self.pending = retained;

        Retirement {
            front_removed: old_front != new_front,
            retired,
        }
    }
}

/// The answer for Escape, an overlay dismissal, or a Cancel button. Pi's
/// confirm has no distinct cancellation shape: false is its refusal.
impl DialogAnswer {
    pub fn dismissal_for(request: &DialogRequest) -> Self {
        match &request.kind {
            crate::DialogKind::Confirm { .. } => DialogAnswer::Confirmed(false),
            crate::DialogKind::Select { .. }
            | crate::DialogKind::Input { .. }
            | crate::DialogKind::Editor { .. } => DialogAnswer::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DialogKind;

    fn room(machine: &str, sid: &str) -> DialogRoom {
        DialogRoom::new(machine.to_string(), sid.to_string())
    }

    fn question(machine: &str, sid: &str, id: &str) -> DialogObligation {
        DialogObligation::new(
            machine.to_string(),
            sid.to_string(),
            DialogRequest {
                id: id.to_string(),
                title: "Which branch?".to_string(),
                kind: DialogKind::Confirm {
                    message: "sure?".to_string(),
                },
            },
        )
    }

    #[test]
    fn a_forwarded_dialog_is_visible_only_in_its_active_room() {
        let mut obligations = DialogObligations::default();
        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Ignored
        );

        obligations.enter_room(room("vm", "sid-1"));
        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Visible
        );
    }

    #[test]
    fn replay_is_a_duplicate_not_a_second_obligation() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));

        assert_eq!(
            obligations.enqueue(question("vm", "sid-1", "d-1")),
            EnqueueResult::Duplicate
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
            EnqueueResult::Queued
        );

        let answered = obligations
            .answer_front(DialogAnswer::Confirmed(true))
            .expect("front obligation");
        assert_eq!(answered.obligation.request.id, "d-1");
        assert_eq!(
            obligations.front().map(|dialog| dialog.request.id.as_str()),
            Some("d-2")
        );
        assert!(obligations
            .answer_front(DialogAnswer::Confirmed(false))
            .is_some());
        assert!(obligations
            .answer_front(DialogAnswer::Confirmed(false))
            .is_none());
    }

    #[test]
    fn retirement_never_sends_an_answer_and_advances_the_visible_front() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));
        obligations.enqueue(question("vm", "sid-1", "d-2"));

        let retirement = obligations.retire_session("sid-1");
        assert!(retirement.front_removed);
        assert_eq!(retirement.retired.len(), 2);
        assert!(obligations.front().is_none());
        assert!(obligations.room().is_none());
    }

    #[test]
    fn switching_rooms_retires_old_questions_without_client_cancellation() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "old"));
        obligations.enqueue(question("vm", "old", "d-1"));

        let retirement = obligations.enter_room(room("vm", "new"));
        assert!(retirement.front_removed);
        assert_eq!(retirement.retired[0].sid, "old");
        assert_eq!(obligations.room(), Some(&room("vm", "new")));
        assert_eq!(
            obligations.enqueue(question("vm", "old", "late")),
            EnqueueResult::Ignored
        );
        assert_eq!(
            obligations.enqueue(question("vm", "new", "d-2")),
            EnqueueResult::Visible
        );
    }

    #[test]
    fn an_unrelated_bridge_disconnect_does_not_retire_the_active_room() {
        let mut obligations = DialogObligations::default();
        obligations.enter_room(room("vm", "sid-1"));
        obligations.enqueue(question("vm", "sid-1", "d-1"));

        assert_eq!(obligations.leave_machine("other").retired.len(), 0);
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

        let retirement = obligations.leave_machine("vm");
        assert_eq!(retirement.retired.len(), 1);
        assert!(obligations.room().is_none());
        assert!(obligations.front().is_none());
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
