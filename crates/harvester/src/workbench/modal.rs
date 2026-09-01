use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Window};
use gpui_component::WindowExt as _;
use harvester_client_core::{
    DialogAnswer, DialogKey, DialogObligation, DraftId, MachineId, RuntimeEvent,
    WorkspaceBrowserId, WorkspaceId,
};

use super::picker::FolderPicker;
use crate::dialogs::open_pi_dialog;
use crate::store::FleetStore;
use crate::ui::dialog::{open_confirmation, ActionAccent, Confirmation, CANCEL_LABEL};

const NOTHING_IS_DELETED: &str = "Nothing on the machine itself is deleted.";
const FORGET_ACTION_LABEL: &str = "Remove";

pub(super) enum ModalOutcome {
    RepoAdded {
        draft: DraftId,
        workspace: WorkspaceId,
    },
}

enum ActiveModal {
    PiQuestion {
        key: DialogKey,
    },
    AddRepo {
        draft: DraftId,
        browser: WorkspaceBrowserId,
        picker: Entity<FolderPicker>,
    },
    ForgetMachine {
        machine: MachineId,
        requested: bool,
    },
}

pub(super) struct ModalCoordinator {
    store: Entity<FleetStore>,
    active: Option<ActiveModal>,
    awaiting_dialog: Option<DialogKey>,
    _store_events: Subscription,
}

impl EventEmitter<ModalOutcome> for ModalCoordinator {}

impl ModalCoordinator {
    pub(super) fn new(
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            store,
            active: None,
            awaiting_dialog: None,
            _store_events: store_events,
        }
    }

    pub(super) fn open_add_repo(
        &mut self,
        draft: DraftId,
        machine: MachineId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.active.is_some() || self.awaiting_dialog.is_some() {
            return;
        }

        let browser = self.store.read(cx).begin_workspace_browser(machine.clone());
        let store = self.store.clone();
        let picker = cx.new(|cx| FolderPicker::new(browser, store, window, cx));

        self.active = Some(ActiveModal::AddRepo {
            draft,
            browser,
            picker: picker.clone(),
        });
        FolderPicker::open_dialog(&picker, Self::cancel_add_repo, window, cx);
    }

    pub(super) fn open_forget_machine(
        &mut self,
        machine: MachineId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.active.is_some() || self.awaiting_dialog.is_some() {
            return;
        }

        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let sessions = snapshot
            .sessions()
            .iter()
            .filter(|session| session.key.machine() == &machine)
            .count();
        let removal = Confirmation {
            title: format!("Remove machine {}?", snapshot.machine_label(&machine)).into(),
            message: removal_message(sessions).into(),
            cancel_label: CANCEL_LABEL.into(),
            action_label: FORGET_ACTION_LABEL.into(),
            action_accent: ActionAccent::Danger,
        };

        self.active = Some(ActiveModal::ForgetMachine {
            machine,
            requested: false,
        });
        self.open_removal_confirmation(removal, window, cx);
    }

    fn open_removal_confirmation(
        &mut self,
        removal: Confirmation,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
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

    fn forget_machine(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let Some(ActiveModal::ForgetMachine { machine, requested }) = &mut self.active else {
            return false;
        };

        if *requested {
            return false;
        }

        *requested = true;
        self.store.read(cx).forget_machine(machine.clone());

        false
    }

    fn decline_forget_machine(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        if !matches!(self.active, Some(ActiveModal::ForgetMachine { .. })) {
            return false;
        }

        self.release(window, cx);

        true
    }

    fn cancel_add_repo(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> bool {
        let Some(ActiveModal::AddRepo { picker, .. }) = &self.active else {
            return false;
        };

        picker.read(cx).cancel(cx);
        self.release(window, cx);

        true
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            RuntimeEvent::Changed => self.sync_dialog(window, cx),
            RuntimeEvent::WorkspaceAdded { browser, workspace } => {
                self.handle_workspace_added(*browser, workspace, window, cx);
            }
            RuntimeEvent::MachineForgotten(machine) => {
                self.handle_machine_forgotten(machine, window, cx);
            }
            RuntimeEvent::MachineForgetFailed { machine, .. } => {
                self.handle_machine_forget_failed(machine);
            }
            RuntimeEvent::DialogAnswerFailed { dialog, .. } => {
                self.handle_dialog_answer_failed(dialog, window, cx);
            }
            RuntimeEvent::Failed(_)
            | RuntimeEvent::Rejected(_)
            | RuntimeEvent::PromptAccepted { .. }
            | RuntimeEvent::PromptRejected { .. }
            | RuntimeEvent::PromptCompleted { .. }
            | RuntimeEvent::DraftPromoted { .. } => {}
        }
    }

    fn handle_workspace_added(
        &mut self,
        browser: WorkspaceBrowserId,
        workspace: &WorkspaceId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(ActiveModal::AddRepo {
            draft,
            browser: active,
            ..
        }) = &self.active
        else {
            return;
        };

        if *active != browser {
            return;
        }

        let draft = *draft;
        let workspace = workspace.clone();

        self.active = None;
        window.close_dialog(cx);
        cx.emit(ModalOutcome::RepoAdded { draft, workspace });
        self.defer_advance(window, cx);
    }

    fn handle_machine_forgotten(
        &mut self,
        machine: &MachineId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let active = matches!(
            &self.active,
            Some(ActiveModal::ForgetMachine {
                machine: active,
                ..
            }) if active == machine
        );

        if !active {
            return;
        }

        self.active = None;
        window.close_dialog(cx);
        self.defer_advance(window, cx);
    }

    fn handle_machine_forget_failed(&mut self, machine: &MachineId) {
        let Some(ActiveModal::ForgetMachine {
            machine: active,
            requested,
        }) = &mut self.active
        else {
            return;
        };

        if active == machine {
            *requested = false;
        }
    }

    fn handle_dialog_answer_failed(
        &mut self,
        dialog: &DialogKey,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if self.awaiting_dialog.as_ref() != Some(dialog) {
            return;
        }

        self.awaiting_dialog = None;
        self.defer_advance(window, cx);
    }

    fn sync_dialog(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        let pending = self.store.read(cx).snapshot().pending_dialog().cloned();
        let pending_key = pending.as_ref().map(DialogObligation::key);

        if let Some(awaiting) = &self.awaiting_dialog {
            if pending_key.as_ref() == Some(awaiting) {
                return;
            }

            self.awaiting_dialog = None;
        }

        if let Some(ActiveModal::PiQuestion { key }) = &self.active {
            if pending_key.as_ref() == Some(key) {
                return;
            }

            self.active = None;
            window.close_dialog(cx);
            self.defer_advance(window, cx);

            return;
        }

        self.advance(window, cx);
    }

    fn advance(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        if self.active.is_some() || self.awaiting_dialog.is_some() {
            return;
        }

        let Some(obligation) = self.store.read(cx).snapshot().pending_dialog().cloned() else {
            return;
        };
        let key = obligation.key();
        let answered_key = key.clone();

        self.active = Some(ActiveModal::PiQuestion { key });
        open_pi_dialog(
            &obligation,
            move |modal, answer, window, cx| modal.answer(&answered_key, answer, window, cx),
            window,
            cx,
        );
    }

    fn answer(
        &mut self,
        key: &DialogKey,
        answer: DialogAnswer,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        if !matches!(
            &self.active,
            Some(ActiveModal::PiQuestion { key: displayed }) if displayed == key
        ) {
            return false;
        }

        self.active = None;
        self.awaiting_dialog = Some(key.clone());
        self.store.read(cx).answer_dialog(key.clone(), answer);

        true
    }

    fn release(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) {
        self.active = None;
        self.defer_advance(window, cx);
    }

    fn defer_advance(&self, window: &mut Window, cx: &mut Context<'_, Self>) {
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
