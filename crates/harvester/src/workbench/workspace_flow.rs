//! Add-workspace orchestration: choose a machine when necessary, then hand the
//! daemon-free host walk to [`FolderPicker`].
//!
//! The flow owns one generation-tagged run and one Root modal lease. Ending a
//! run drops its picker subscription, invalidates the picker, and releases the
//! lease, so delayed host answers or modal callbacks cannot revive it.

use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::WindowExt as _;

use super::picker::{FolderPicker, WorkspacePicked};
use crate::choices;
use crate::modal::{ModalCoordinator, ModalLease, ModalOwner};
use crate::store::FleetStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlowId(u64);

#[derive(Debug, Default, PartialEq, Eq)]
enum FlowStage {
    #[default]
    Idle,
    ChoosingMachine {
        flow: FlowId,
    },
    PickingFolder {
        flow: FlowId,
        machine: String,
    },
}

/// Window-free lifecycle guard for late choice and host callbacks.
#[derive(Debug, Default)]
struct AddWorkspaceState {
    next_flow: u64,
    stage: FlowStage,
}

impl AddWorkspaceState {
    fn is_idle(&self) -> bool {
        self.stage == FlowStage::Idle
    }

    fn begin(&mut self, machine: Option<String>) -> FlowId {
        debug_assert!(self.is_idle());

        let flow = FlowId(self.next_flow);
        self.next_flow = self
            .next_flow
            .checked_add(1)
            .expect("add-workspace flow generation exhausted");
        self.stage = match machine {
            Some(machine) => FlowStage::PickingFolder { flow, machine },
            None => FlowStage::ChoosingMachine { flow },
        };

        flow
    }

    fn choose_machine(&mut self, flow: FlowId, machine: String) -> bool {
        if !matches!(
            &self.stage,
            FlowStage::ChoosingMachine { flow: active_flow } if *active_flow == flow
        ) {
            return false;
        }

        self.stage = FlowStage::PickingFolder { flow, machine };

        true
    }

    fn is_picker(&self, flow: FlowId, machine: &str) -> bool {
        matches!(
            &self.stage,
            FlowStage::PickingFolder {
                flow: active_flow,
                machine: active_machine,
            } if *active_flow == flow && active_machine == machine
        )
    }

    fn finish(&mut self, flow: FlowId) -> bool {
        let is_active = match &self.stage {
            FlowStage::ChoosingMachine { flow: active_flow }
            | FlowStage::PickingFolder {
                flow: active_flow, ..
            } => *active_flow == flow,
            FlowStage::Idle => false,
        };

        if !is_active {
            return false;
        }

        self.stage = FlowStage::Idle;

        true
    }
}

pub(super) struct AddWorkspaceFlow {
    store: Entity<FleetStore>,
    modals: Entity<ModalCoordinator>,
    state: AddWorkspaceState,
    lease: Option<ModalLease>,
    picker: Option<Entity<FolderPicker>>,
    picker_events: Option<Subscription>,
}

impl AddWorkspaceFlow {
    pub(super) fn new(store: Entity<FleetStore>, modals: Entity<ModalCoordinator>) -> Self {
        Self {
            store,
            modals,
            state: AddWorkspaceState::default(),
            lease: None,
            picker: None,
            picker_events: None,
        }
    }

    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_idle() {
            return;
        }

        let candidates = self.store.read(cx).machine_candidates();

        if candidates.is_empty() {
            self.report_no_candidates(window, cx);

            return;
        }

        let Some(lease) = self
            .modals
            .update(cx, |modals, _| modals.acquire(ModalOwner::AddWorkspace))
        else {
            return;
        };
        let only_machine = (candidates.len() == 1).then(|| candidates[0].key.clone());
        let flow = self.state.begin(only_machine.clone());

        self.lease = Some(lease);

        match only_machine {
            Some(machine) => self.open_picker(flow, machine, window, cx),
            None => self.open_machine_choice(flow, candidates, window, cx),
        }
    }

    fn report_no_candidates(&self, window: &mut Window, cx: &mut Context<Self>) {
        window.push_notification(
            (
                NotificationType::Error,
                "no machines available to add a workspace on".to_string(),
            ),
            cx,
        );
    }

    fn open_machine_choice(
        &mut self,
        flow: FlowId,
        candidates: Vec<harvester_client_core::MachineCandidate>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let keys: Vec<String> = candidates
            .iter()
            .map(|candidate| candidate.key.clone())
            .collect();
        let rows = candidates
            .into_iter()
            .map(|candidate| choices::ChoiceRow {
                label: candidate.label.into(),
                selected: false,
            })
            .collect();

        choices::open(
            "Add workspace — choose machine",
            rows,
            move |workspace_flow, index, window, cx| {
                let Some(machine) = keys.get(index).cloned() else {
                    return workspace_flow.cancel(flow, cx);
                };

                if !workspace_flow.state.choose_machine(flow, machine.clone()) {
                    return false;
                }

                // Choices closes its Root dialog only after this callback
                // accepts the pick. Open the picker on the next turn so that
                // close cannot pop the newly opened modal.
                let workspace_flow = cx.entity();

                window.defer(cx, move |window, cx| {
                    workspace_flow.update(cx, |workspace_flow, cx| {
                        workspace_flow.open_picker(flow, machine, window, cx)
                    });
                });

                true
            },
            move |workspace_flow, _, cx| workspace_flow.cancel(flow, cx),
            window,
            cx,
        );
    }

    fn open_picker(
        &mut self,
        flow: FlowId,
        machine: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.state.is_picker(flow, &machine) {
            return;
        }

        let store = self.store.clone();
        let picker = cx.new(|cx| FolderPicker::new(machine, store, window, cx));
        let picker_events = cx.subscribe_in(
            &picker,
            window,
            move |workspace_flow, picker, picked, window, cx| {
                workspace_flow.handle_workspace_picked(flow, picker, picked, window, cx)
            },
        );

        self.picker = Some(picker.clone());
        self.picker_events = Some(picker_events);

        FolderPicker::open_dialog(
            &picker,
            move |workspace_flow, _, cx| workspace_flow.cancel(flow, cx),
            window,
            cx,
        );
        picker.update(cx, |picker, cx| picker.start(cx));
    }

    fn handle_workspace_picked(
        &mut self,
        flow: FlowId,
        picker: &Entity<FolderPicker>,
        picked: &WorkspacePicked,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let is_current_picker = self
            .picker
            .as_ref()
            .is_some_and(|current| current == picker);

        if !is_current_picker || !self.state.is_picker(flow, &picked.machine) {
            return;
        }

        window.close_dialog(cx);

        if !self.end(flow, false, cx) {
            return;
        }

        self.store.update(cx, |store, cx| {
            store.add_workspace(picked.machine.clone(), picked.path.clone(), cx)
        });
    }

    /// End every exit path: Escape, overlay, Cancel, successful validation, or
    /// an invalid machine-choice callback. Root itself is closed by the caller
    /// or by gpui-component's on_cancel ordering.
    fn cancel(&mut self, flow: FlowId, cx: &mut Context<Self>) -> bool {
        self.end(flow, true, cx)
    }

    fn end(&mut self, flow: FlowId, invalidate_picker: bool, cx: &mut Context<Self>) -> bool {
        if !self.state.finish(flow) {
            return false;
        }

        self.picker_events = None;

        if let Some(picker) = self.picker.take() {
            // A successful validation is emitted while FolderPicker is leased,
            // so updating it here would re-enter that entity. Its pending
            // request is already consumed; dropping both retained handles is
            // sufficient. Cancellation happens from Root and can invalidate
            // the picker immediately.
            if invalidate_picker {
                picker.update(cx, |picker, cx| picker.cancel(cx));
            }
        }

        if let Some(lease) = self.lease.take() {
            self.modals
                .update(cx, |modals, cx| modals.release(lease, cx));
        }

        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_choice_transitions_to_the_picker_in_the_same_flow() {
        let mut state = AddWorkspaceState::default();
        let flow = state.begin(None);

        assert!(state.choose_machine(flow, "machine-a".to_string()));
        assert!(state.is_picker(flow, "machine-a"));
        assert!(!state.is_idle());
    }

    #[test]
    fn cancellation_invalidates_choice_and_picker_callbacks() {
        let mut state = AddWorkspaceState::default();
        let cancelled = state.begin(None);

        assert!(state.finish(cancelled));
        assert!(!state.choose_machine(cancelled, "machine-a".to_string()));

        let current = state.begin(Some("machine-b".to_string()));

        assert!(!state.is_picker(cancelled, "machine-a"));
        assert!(state.is_picker(current, "machine-b"));
        assert!(!state.finish(cancelled));
    }

    #[test]
    fn a_late_validation_cannot_finish_a_new_flow() {
        let mut state = AddWorkspaceState::default();
        let old = state.begin(Some("machine-a".to_string()));

        assert!(state.finish(old));

        let current = state.begin(Some("machine-a".to_string()));

        assert!(!state.finish(old));
        assert!(state.is_picker(current, "machine-a"));
    }
}
