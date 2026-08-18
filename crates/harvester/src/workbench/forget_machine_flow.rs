use gpui::{Context, Entity, EventEmitter, Window};

use crate::modal::{ModalCoordinator, ModalLease, ModalOwner};
use crate::store::FleetStore;
use crate::ui::dialog::{open_confirmation, ActionAccent, Confirmation};

const NOTHING_IS_DELETED: &str = "Nothing on the machine itself is deleted.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfirmationId(u64);

pub(super) struct MachineForgotten {
    pub machine: String,
}

#[derive(Debug, Default)]
struct ForgetMachineState {
    next_confirmation: u64,
    confirming: Option<(ConfirmationId, String)>,
}

impl ForgetMachineState {
    fn is_idle(&self) -> bool {
        self.confirming.is_none()
    }

    fn begin(&mut self, machine: String) -> ConfirmationId {
        debug_assert!(self.is_idle());

        let confirmation = ConfirmationId(self.next_confirmation);

        self.next_confirmation = self
            .next_confirmation
            .checked_add(1)
            .expect("forget-machine confirmation generation exhausted");
        self.confirming = Some((confirmation, machine));

        confirmation
    }

    fn machine_of(&self, confirmation: ConfirmationId) -> Option<&str> {
        match &self.confirming {
            Some((active, machine)) if *active == confirmation => Some(machine),
            _ => None,
        }
    }

    fn finish(&mut self, confirmation: ConfirmationId) -> bool {
        if self.machine_of(confirmation).is_none() {
            return false;
        }

        self.confirming = None;

        true
    }
}

pub(super) struct ForgetMachineFlow {
    store: Entity<FleetStore>,
    modals: Entity<ModalCoordinator>,
    state: ForgetMachineState,
    lease: Option<ModalLease>,
}

impl EventEmitter<MachineForgotten> for ForgetMachineFlow {}

impl ForgetMachineFlow {
    pub(super) fn new(store: Entity<FleetStore>, modals: Entity<ModalCoordinator>) -> Self {
        Self {
            store,
            modals,
            state: ForgetMachineState::default(),
            lease: None,
        }
    }

    pub(super) fn open(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_idle() {
            return;
        }

        let Some(lease) = self
            .modals
            .update(cx, |modals, _| modals.acquire(ModalOwner::ForgetMachine))
        else {
            return;
        };
        let store = self.store.read(cx);
        let title = format!("Remove machine {}?", store.label(&machine));
        let message = removal_message(store.visible_session_count_on(&machine));
        let confirmation = self.state.begin(machine);

        self.lease = Some(lease);
        self.open_removal_confirmation(confirmation, title, message, window, cx);
    }

    fn open_removal_confirmation(
        &mut self,
        confirmation: ConfirmationId,
        title: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let confirmed = cx.entity();
        let declined = cx.entity();
        let removal = Confirmation {
            title: title.into(),
            message: message.into(),
            cancel_label: "Cancel".into(),
            action_label: "Remove".into(),
            action_accent: ActionAccent::Danger,
        };

        open_confirmation(
            removal,
            move |_, cx| confirmed.update(cx, |flow, cx| flow.forget(confirmation, cx)),
            move |_, cx| declined.update(cx, |flow, cx| flow.cancel(confirmation, cx)),
            window,
            cx,
        );
    }

    fn forget(&mut self, confirmation: ConfirmationId, cx: &mut Context<Self>) -> bool {
        let Some(machine) = self.state.machine_of(confirmation).map(str::to_string) else {
            return false;
        };
        let forgotten = self
            .store
            .update(cx, |store, cx| store.forget_machine(&machine, cx));

        if !self.end(confirmation, cx) {
            return false;
        }

        if forgotten {
            cx.emit(MachineForgotten { machine });
        }

        true
    }

    fn cancel(&mut self, confirmation: ConfirmationId, cx: &mut Context<Self>) -> bool {
        self.end(confirmation, cx)
    }

    fn end(&mut self, confirmation: ConfirmationId, cx: &mut Context<Self>) -> bool {
        if !self.state.finish(confirmation) {
            return false;
        }

        if let Some(lease) = self.lease.take() {
            self.modals
                .update(cx, |modals, cx| modals.release(lease, cx));
        }

        true
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
    fn a_confirmation_answers_for_its_own_machine_only() {
        let mut state = ForgetMachineState::default();
        let confirmation = state.begin("machine-a".to_string());

        assert_eq!(state.machine_of(confirmation), Some("machine-a"));
        assert!(!state.is_idle());
        assert!(state.finish(confirmation));
        assert!(state.is_idle());
    }

    #[test]
    fn a_late_callback_cannot_forget_the_next_machine() {
        let mut state = ForgetMachineState::default();
        let stale = state.begin("machine-a".to_string());

        assert!(state.finish(stale));

        let current = state.begin("machine-b".to_string());

        assert_eq!(state.machine_of(stale), None);
        assert!(!state.finish(stale));
        assert_eq!(state.machine_of(current), Some("machine-b"));
    }

    #[test]
    fn the_message_counts_the_rows_that_disappear() {
        assert!(removal_message(0).starts_with("Harvester forgets the machine"));
        assert!(removal_message(1).starts_with("Its 1 session will be"));
        assert!(removal_message(4).starts_with("Its 4 sessions will be"));
        assert!(removal_message(4).ends_with(NOTHING_IS_DELETED));
    }
}
