//! Forget-machine orchestration: confirm, then let one file write cascade.
//!
//! Only a machine its provider no longer lists reaches this flow (the sidebar
//! puts the trash on no other row), so there is nothing to disconnect and
//! nothing on the machine to delete — the removal is entirely about what
//! Harvester still claims to know (docs/SPEC.md §Flows "Forget machine").
//!
//! The flow owns one generation-tagged confirmation and one Root lease, the
//! same shape [`AddRepoFlow`](super::repo_flow::AddRepoFlow) uses: ending a run
//! releases the lease, so a delayed dialog callback cannot forget a second
//! machine or free another flow's modal.

use gpui::{px, Context, Entity, EventEmitter, ParentElement as _, Window};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::WindowExt as _;

use crate::modal::{ModalCoordinator, ModalLease, ModalOwner};
use crate::store::FleetStore;

/// Same width as the Pi dialogs, for the same reason: a machine label and a
/// sentence about it are not worth a second modal geometry.
const CONFIRM_WIDTH: f32 = 520.;
/// The reassurance every removal count carries. A missing machine is
/// unreachable by definition, so this is a statement of fact, not a promise.
const NOTHING_IS_DELETED: &str = "Nothing on the machine itself is deleted.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ConfirmationId(u64);

/// A machine the user confirmed forgetting, on its way to the shell that has to
/// close whatever is still pointed at it.
pub(super) struct MachineForgotten {
    pub machine: String,
}

/// Window-free lifecycle guard for late dialog callbacks.
#[derive(Debug, Default)]
struct ForgetMachineState {
    next_confirmation: u64,
    /// The confirmation on screen: its generation and the machine it names.
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

    /// The machine a still-current confirmation names, if it is still current.
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

    /// Ask about `machine`. A second ask while a confirmation is open is
    /// ignored: Root holds one dialog and the lease is spent on this one.
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
        self.open_confirmation(confirmation, title, message, window, cx);
    }

    fn open_confirmation(
        &mut self,
        confirmation: ConfirmationId,
        title: String,
        message: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let flow = cx.entity();

        window.open_dialog(cx, move |dialog, _, _| {
            let confirmed = flow.clone();
            let declined = flow.clone();

            dialog
                .title(title.clone())
                .width(px(CONFIRM_WIDTH))
                .close_button(false)
                .child(message.clone())
                .footer(
                    DialogFooter::new()
                        .child(
                            DialogClose::new()
                                .child(Button::new("forget-machine-cancel").label("Cancel")),
                        )
                        .child(
                            DialogAction::new().child(
                                Button::new("forget-machine-remove")
                                    .danger()
                                    .label("Remove"),
                            ),
                        ),
                )
                .on_ok(move |_, _, cx| {
                    confirmed.update(cx, |flow, cx| flow.forget(confirmation, cx))
                })
                .on_cancel(move |_, _, cx| {
                    declined.update(cx, |flow, cx| flow.cancel(confirmation, cx))
                })
        });
    }

    /// Confirmed. The store owns the cascade — one `workspaces.json` write, and
    /// tracking, the ghost row and the machine's sessions fall out of it — so
    /// all this flow does is end its run and say whose chat is now homeless.
    ///
    /// A failed write reports itself and forgets nothing, but the dialog still
    /// closes: leaving it up would only offer the same failing button again.
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

    /// Every other exit: Escape, the overlay, or Cancel.
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

/// What the confirmation promises to remove. The count is sidebar rows, which
/// is all a missing machine's sessions can still be: their transcripts live on
/// a disk this client will never reach again.
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
