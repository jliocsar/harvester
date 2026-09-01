use std::sync::Arc;

use gpui::{Context, EventEmitter};
use harvester_client_core::{
    ChatId, ClientRuntime, ClientSnapshot, DialogAnswer, DialogKey, DraftId, MachineId,
    ModelChoice, RuntimeEvent, RuntimeEvents, SessionKey, SubmissionId, WorkspaceBrowserId,
    WorkspaceId,
};

pub(crate) struct FleetStore {
    runtime: ClientRuntime,
    snapshot: Arc<ClientSnapshot>,
}

impl EventEmitter<RuntimeEvent> for FleetStore {}

impl FleetStore {
    pub(crate) fn new(
        runtime: ClientRuntime,
        events: RuntimeEvents,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let snapshot = runtime.snapshot();

        Self::pump_events(events, cx);

        Self { runtime, snapshot }
    }

    fn pump_events(mut events: RuntimeEvents, cx: &mut Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(event) = events.next().await {
                if this
                    .update(cx, |store, cx| {
                        store.snapshot = store.runtime.snapshot();
                        cx.notify();
                        cx.emit(event);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    pub(crate) fn snapshot(&self) -> &ClientSnapshot {
        &self.snapshot
    }

    pub(crate) fn create_draft(&self) -> DraftId {
        self.runtime.create_draft()
    }

    pub(crate) fn retarget_draft(&self, draft: DraftId, machine: MachineId) {
        self.runtime.retarget_draft(draft, machine);
    }

    pub(crate) fn select_workspace(&self, draft: DraftId, workspace: WorkspaceId) {
        self.runtime.select_workspace(draft, workspace);
    }

    pub(crate) fn request_controls(&self, chat: ChatId) {
        self.runtime.request_controls(chat);
    }

    pub(crate) fn open_session(&self, session: SessionKey) {
        self.runtime.open_session(session);
    }

    pub(crate) fn reserve_prompt_submission(&self) -> SubmissionId {
        self.runtime.reserve_prompt_submission()
    }

    pub(crate) fn submit_prompt(&self, submission: SubmissionId, chat: ChatId, text: String) {
        self.runtime.submit_prompt(submission, chat, text);
    }

    pub(crate) fn pick_model(&self, chat: ChatId, model: Option<ModelChoice>) {
        self.runtime.pick_model(chat, model);
    }

    pub(crate) fn stop_session(&self, session: SessionKey) {
        self.runtime.stop_session(session);
    }

    pub(crate) fn answer_dialog(&self, key: DialogKey, answer: DialogAnswer) {
        self.runtime.answer_dialog(key, answer);
    }

    pub(crate) fn begin_workspace_browser(&self, machine: MachineId) -> WorkspaceBrowserId {
        self.runtime.begin_workspace_browser(machine)
    }

    pub(crate) fn open_browser_directory(&self, browser: WorkspaceBrowserId, name: String) {
        self.runtime.open_browser_directory(browser, name);
    }

    pub(crate) fn browser_up(&self, browser: WorkspaceBrowserId) {
        self.runtime.browser_up(browser);
    }

    pub(crate) fn confirm_browser(&self, browser: WorkspaceBrowserId) {
        self.runtime.confirm_browser(browser);
    }

    pub(crate) fn cancel_browser(&self, browser: WorkspaceBrowserId) {
        self.runtime.cancel_browser(browser);
    }

    pub(crate) fn forget_machine(&self, machine: MachineId) {
        self.runtime.forget_machine(machine);
    }
}
