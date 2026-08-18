use gpui::{AppContext as _, Context, Entity, EventEmitter, Subscription, Window};
use gpui_component::WindowExt as _;

use super::picker::{FolderPicker, WorkspacePicked};
use crate::modal::{ModalCoordinator, ModalLease, ModalOwner};
use crate::store::FleetStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FlowId(u64);

pub(super) struct RepoAdded {
    pub machine: String,
    pub path: String,
}

#[derive(Debug, Default)]
struct AddRepoState {
    next_flow: u64,
    running: Option<(FlowId, String)>,
}

impl AddRepoState {
    fn is_idle(&self) -> bool {
        self.running.is_none()
    }

    fn begin(&mut self, machine: String) -> FlowId {
        debug_assert!(self.is_idle());

        let flow = FlowId(self.next_flow);

        self.next_flow = self
            .next_flow
            .checked_add(1)
            .expect("add-repo flow generation exhausted");
        self.running = Some((flow, machine));

        flow
    }

    fn is_picker(&self, flow: FlowId, machine: &str) -> bool {
        matches!(
            &self.running,
            Some((active_flow, active_machine)) if *active_flow == flow && active_machine == machine
        )
    }

    fn finish(&mut self, flow: FlowId) -> bool {
        if !matches!(&self.running, Some((active_flow, _)) if *active_flow == flow) {
            return false;
        }

        self.running = None;

        true
    }
}

pub(super) struct AddRepoFlow {
    store: Entity<FleetStore>,
    modals: Entity<ModalCoordinator>,
    state: AddRepoState,
    lease: Option<ModalLease>,
    picker: Option<Entity<FolderPicker>>,
    picker_events: Option<Subscription>,
}

impl EventEmitter<RepoAdded> for AddRepoFlow {}

impl AddRepoFlow {
    pub(super) fn new(store: Entity<FleetStore>, modals: Entity<ModalCoordinator>) -> Self {
        Self {
            store,
            modals,
            state: AddRepoState::default(),
            lease: None,
            picker: None,
            picker_events: None,
        }
    }

    pub(super) fn open(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        if !self.state.is_idle() {
            return;
        }

        let Some(lease) = self
            .modals
            .update(cx, |modals, _| modals.acquire(ModalOwner::AddRepo))
        else {
            return;
        };
        let flow = self.state.begin(machine.clone());

        self.lease = Some(lease);
        self.open_picker(flow, machine, window, cx);
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
            move |flow_view, picker, picked, window, cx| {
                flow_view.handle_workspace_picked(flow, picker, picked, window, cx)
            },
        );

        self.picker = Some(picker.clone());
        self.picker_events = Some(picker_events);

        FolderPicker::open_dialog(
            &picker,
            move |flow_view, _, cx| flow_view.cancel(flow, cx),
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
        cx.emit(RepoAdded {
            machine: picked.machine.clone(),
            path: picked.path.clone(),
        });
    }

    fn cancel(&mut self, flow: FlowId, cx: &mut Context<Self>) -> bool {
        self.end(flow, true, cx)
    }

    fn end(&mut self, flow: FlowId, invalidate_picker: bool, cx: &mut Context<Self>) -> bool {
        if !self.state.finish(flow) {
            return false;
        }

        self.picker_events = None;

        if let Some(picker) = self.picker.take() {
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
    fn a_run_is_the_picker_for_its_own_machine_only() {
        let mut state = AddRepoState::default();
        let flow = state.begin("machine-a".to_string());

        assert!(state.is_picker(flow, "machine-a"));
        assert!(!state.is_picker(flow, "machine-b"));
        assert!(!state.is_idle());
    }

    #[test]
    fn cancellation_invalidates_the_picker_callbacks_that_follow_it() {
        let mut state = AddRepoState::default();
        let cancelled = state.begin("machine-a".to_string());

        assert!(state.finish(cancelled));
        assert!(state.is_idle());
        assert!(!state.is_picker(cancelled, "machine-a"));

        let current = state.begin("machine-b".to_string());

        assert!(!state.is_picker(cancelled, "machine-a"));
        assert!(state.is_picker(current, "machine-b"));
    }

    #[test]
    fn a_late_validation_cannot_finish_a_new_flow() {
        let mut state = AddRepoState::default();
        let old = state.begin("machine-a".to_string());

        assert!(state.finish(old));

        let current = state.begin("machine-a".to_string());

        assert!(!state.finish(old));
        assert!(state.is_picker(current, "machine-a"));
    }
}
