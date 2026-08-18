//! Add-workspace orchestration: choose a machine when necessary, then hand the
//! daemon-free host walk to [`FolderPicker`].
//!
//! The flow reads the one [`FleetStore`] and never copies fleet or workspace
//! state. Its only retained state is the picker entity whose correlated host
//! answers may still be in flight.

use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use gpui_component::notification::NotificationType;
use gpui_component::WindowExt as _;

use super::picker::{FolderPicker, WorkspacePicked};
use crate::choices;
use crate::store::FleetStore;

pub(super) struct AddWorkspaceFlow {
    store: Entity<FleetStore>,
    picker: Option<Entity<FolderPicker>>,
    picker_events: Option<Subscription>,
}

impl AddWorkspaceFlow {
    pub(super) fn new(store: Entity<FleetStore>) -> Self {
        Self {
            store,
            picker: None,
            picker_events: None,
        }
    }

    pub(super) fn open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let candidates = self.store.read(cx).machine_candidates();

        match candidates.len() {
            0 => self.report_no_candidates(window, cx),
            1 => self.open_picker(candidates[0].key.clone(), window, cx),
            _ => self.open_machine_choice(candidates, window, cx),
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
            move |flow, index, window, cx| {
                let Some(machine) = keys.get(index).cloned() else {
                    return;
                };

                flow.open_picker(machine, window, cx);
            },
            window,
            cx,
        );
    }

    fn open_picker(&mut self, machine: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.store.clone();
        let picker = cx.new(|cx| FolderPicker::new(machine, store, window, cx));
        let picker_events = cx.subscribe_in(&picker, window, Self::handle_workspace_picked);

        FolderPicker::open_dialog(&picker, window, cx);
        picker.update(cx, |picker, cx| picker.start(cx));

        self.picker = Some(picker);
        self.picker_events = Some(picker_events);
    }

    fn handle_workspace_picked(
        &mut self,
        _picker: &Entity<FolderPicker>,
        picked: &WorkspacePicked,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.close_dialog(cx);
        self.picker = None;
        self.picker_events = None;
        self.store.update(cx, |store, cx| {
            store.add_workspace(picked.machine.clone(), picked.path.clone(), cx)
        });
    }
}
