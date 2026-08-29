use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Window};
use gpui_component::WindowExt as _;
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::{
    DialogAnswer, DialogKey, DialogObligation, DialogObligations, DialogRequest, DialogRoom,
};

use super::picker::{FolderPicker, WorkspacePicked};
use crate::dialogs::open_pi_dialog;
use crate::store::{FleetStore, StoreEvent};
use crate::ui::dialog::{open_confirmation, ActionAccent, Confirmation, CANCEL_LABEL};

const NOTHING_IS_DELETED: &str = "Nothing on the machine itself is deleted.";
const FORGET_ACTION_LABEL: &str = "Remove";

pub(super) enum ModalOutcome {
    RepoAdded { machine: String, path: String },
    MachineForgotten { machine: String },
}

enum ActiveModal {
    PiQuestion {
        key: DialogKey,
    },
    AddRepo {
        machine: String,
        picker: Entity<FolderPicker>,
        _picker_events: Subscription,
    },
    ForgetMachine {
        machine: String,
    },
}

pub(super) struct ModalCoordinator {
    store: Entity<FleetStore>,
    obligations: DialogObligations,
    active: Option<ActiveModal>,
    _store_events: Subscription,
}

impl EventEmitter<ModalOutcome> for ModalCoordinator {}

impl ModalCoordinator {
    pub(super) fn new(
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            store,
            obligations: DialogObligations::default(),
            active: None,
            _store_events: store_events,
        }
    }

    pub(super) fn open_add_repo(
        &mut self,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active.is_some() {
            return;
        }

        let store = self.store.clone();
        let picker = cx.new(|cx| FolderPicker::new(machine.clone(), store, window, cx));
        let picker_events = cx.subscribe_in(&picker, window, Self::handle_workspace_picked);

        self.active = Some(ActiveModal::AddRepo {
            machine,
            picker: picker.clone(),
            _picker_events: picker_events,
        });
        FolderPicker::open_dialog(&picker, Self::cancel_add_repo, window, cx);
        picker.update(cx, |picker, cx| picker.start(cx));
    }

    pub(super) fn open_forget_machine(
        &mut self,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.active.is_some() {
            return;
        }

        let store = self.store.read(cx);
        let removal = Confirmation {
            title: format!("Remove machine {}?", store.label(&machine)).into(),
            message: removal_message(store.catalog().visible_session_count_on(&machine)).into(),
            cancel_label: CANCEL_LABEL.into(),
            action_label: FORGET_ACTION_LABEL.into(),
            action_accent: ActionAccent::Danger,
        };

        self.active = Some(ActiveModal::ForgetMachine { machine });
        self.open_removal_confirmation(removal, window, cx);
    }

    fn open_removal_confirmation(
        &mut self,
        removal: Confirmation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = cx.entity();
        let declined = cx.entity();

        open_confirmation(
            removal,
            move |window, cx| confirmed.update(cx, |modal, cx| modal.forget_machine(window, cx)),
            move |window, cx| {
                declined.update(cx, |modal, cx| modal.decline_forget_machine(window, cx))
            },
            window,
            cx,
        );
    }

    fn forget_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ActiveModal::ForgetMachine { machine }) = &self.active else {
            return false;
        };
        let machine = machine.clone();
        let forgotten = self
            .store
            .update(cx, |store, cx| store.forget_machine(&machine, cx));

        self.release(window, cx);

        if forgotten {
            cx.emit(ModalOutcome::MachineForgotten { machine });
        }

        true
    }

    fn decline_forget_machine(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !matches!(self.active, Some(ActiveModal::ForgetMachine { .. })) {
            return false;
        }

        self.release(window, cx);

        true
    }

    fn handle_workspace_picked(
        &mut self,
        _picker: &Entity<FolderPicker>,
        picked: &WorkspacePicked,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(ActiveModal::AddRepo { machine, .. }) = &self.active else {
            return;
        };
        let machine = machine.clone();
        let path = picked.path.clone();

        self.active = None;
        window.close_dialog(cx);
        self.store.update(cx, |store, cx| {
            store.add_workspace(machine.clone(), path.clone(), cx)
        });
        cx.emit(ModalOutcome::RepoAdded { machine, path });
        self.defer_advance(window, cx);
    }

    fn cancel_add_repo(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ActiveModal::AddRepo { picker, .. }) = &self.active else {
            return false;
        };

        picker.update(cx, |picker, cx| picker.cancel(cx));

        self.release(window, cx);

        true
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::AttachmentLeft { sid },
            } => self.retire_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Attached { sid, .. },
            } => self.enter_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Pi { sid, event },
            } => self.handle_pi_event(machine, sid, event, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::SessionExit { sid, .. },
            } => self.retire_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Disconnected,
            } => self.leave_machine(machine, window, cx),
            StoreEvent::Bridge { .. } | StoreEvent::Machines(_) | StoreEvent::Failed(_) => {}
        }
    }

    fn handle_pi_event(
        &mut self,
        machine: &str,
        sid: &str,
        event: &serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = DialogRequest::parse(event) else {
            return;
        };

        self.enqueue(machine, sid, request, window, cx);
    }

    fn enqueue(
        &mut self,
        machine: &str,
        sid: &str,
        request: DialogRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let obligation = DialogObligation {
            machine: machine.to_string(),
            sid: sid.to_string(),
            request,
        };

        if !self.obligations.enqueue(obligation).accepted() {
            return;
        }

        self.advance(window, cx);
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.active.is_some() {
            return;
        }

        let Some(obligation) = self.obligations.front().cloned() else {
            return;
        };
        let key = obligation.key();
        let answered_key = key.clone();

        self.active = Some(ActiveModal::PiQuestion { key });
        open_pi_dialog(
            &obligation,
            move |modal, answer, window, cx| modal.answer_front(&answered_key, answer, window, cx),
            window,
            cx,
        );
    }

    fn answer_front(
        &mut self,
        key: &DialogKey,
        answer: DialogAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.displays(key) {
            return false;
        }

        let Some(answered) = self.obligations.answer_front(key, answer) else {
            return false;
        };
        let obligation = answered.obligation;

        self.active = None;
        self.store.read(cx).send(
            &obligation.machine,
            Command::AnswerDialog {
                sid: obligation.sid,
                dialog_id: obligation.request.id,
                answer: answered.answer,
            },
        );
        self.defer_advance(window, cx);

        true
    }

    fn enter_room(
        &mut self,
        machine: &str,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let retired = self.obligations.enter_room(DialogRoom {
            machine: machine.to_string(),
            sid: sid.to_string(),
        });

        self.apply_retirement(retired, window, cx);
    }

    fn retire_room(
        &mut self,
        machine: &str,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let retired = self.obligations.retire_room(machine, sid);

        self.apply_retirement(retired, window, cx);
    }

    fn leave_machine(&mut self, machine: &str, window: &mut Window, cx: &mut Context<Self>) {
        let retired = self.obligations.leave_machine(machine);

        self.apply_retirement(retired, window, cx);
    }

    fn apply_retirement(
        &mut self,
        retired: Vec<DialogObligation>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let displayed_retired = retired
            .iter()
            .any(|obligation| self.displays(&obligation.key()));

        if !displayed_retired {
            return;
        }

        self.active = None;
        window.close_dialog(cx);
        self.defer_advance(window, cx);
    }

    fn displays(&self, key: &DialogKey) -> bool {
        matches!(&self.active, Some(ActiveModal::PiQuestion { key: displayed }) if displayed == key)
    }

    fn release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active = None;
        self.defer_advance(window, cx);
    }

    fn defer_advance(&self, window: &mut Window, cx: &mut Context<Self>) {
        let coordinator = cx.entity();

        window.defer(cx, move |window, cx| {
            coordinator.update(cx, |coordinator, cx| coordinator.advance(window, cx));
        });
    }
}

fn removal_message(sessions: usize) -> String {
    let lead = match sessions {
        0 => "Harvester forgets the machine and the repositories stored for it.".to_string(),
        1 => "Its 1 session will be removed from the list.".to_string(),
        sessions => format!("Its {sessions} sessions will be removed from the list."),
    };

    format!("{lead} {NOTHING_IS_DELETED}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_counts_the_rows_that_disappear() {
        assert!(removal_message(0).starts_with("Harvester forgets the machine"));
        assert!(removal_message(1).starts_with("Its 1 session will be"));
        assert!(removal_message(4).starts_with("Its 4 sessions will be"));
        assert!(removal_message(4).ends_with(NOTHING_IS_DELETED));
    }
}
