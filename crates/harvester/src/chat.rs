use std::collections::HashMap;

use gpui::{
    div, prelude::*, App, Context, Entity, EventEmitter, Focusable, ScrollHandle, SharedString,
    Subscription, Window,
};
use gpui_component::input::{InputEvent, InputState};
use harvester_client_core::{
    folder_name, ChatId, ClientSnapshot, DraftId, MachineId, ModelsState, RuntimeEvent,
    SubmissionId,
};

use crate::models;
use crate::store::FleetStore;
use crate::ui::{self, Tone};

pub(crate) const NEW_SESSION_LABEL: &str = "New session";

const MODEL_FALLBACK_SUFFIX: &str = " — pi will use its default model";
const UNRESOLVED_TARGET_SUBTITLE: &str = "Pick a machine and a repository below";
const OPENING_SESSION_HINT: &str = "Opening session…";

type TranscriptSignature = u64;

pub(crate) enum ChatFlowRequest {
    AddRepo { draft: DraftId, machine: MachineId },
}

impl EventEmitter<ChatFlowRequest> for Chat {}

struct SubmittedInput {
    text: String,
    accepted: bool,
}

pub(crate) struct Chat {
    id: ChatId,
    store: Entity<FleetStore>,
    transcript_presentation: ui::TranscriptPresentation,
    transcript_signature: Option<TranscriptSignature>,
    focus_input: bool,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    submissions: HashMap<SubmissionId, SubmittedInput>,
    _prompt_submitted: Subscription,
    _store_repaint: Subscription,
    _store_events: Subscription,
}

impl Chat {
    pub(crate) fn new(
        id: ChatId,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) -> Self {
        let input = ui::create_prompt_input(window, cx);
        let prompt_submitted = cx.subscribe_in(&input, window, Self::handle_prompt_submitted);
        let store_repaint = cx.observe(&store, |_, _, cx| cx.notify());
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);
        let transcript_signature = transcript_signature(store.read(cx).snapshot(), &id);

        Self {
            id,
            store,
            transcript_presentation: ui::TranscriptPresentation::default(),
            transcript_signature,
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            submissions: HashMap::new(),
            _prompt_submitted: prompt_submitted,
            _store_repaint: store_repaint,
            _store_events: store_events,
        }
    }

    pub(crate) fn set_id(&mut self, id: ChatId, cx: &mut Context<'_, Self>) {
        self.id = id;
        self.transcript_signature = transcript_signature(self.store.read(cx).snapshot(), &self.id);
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &RuntimeEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            RuntimeEvent::Changed => self.handle_snapshot_changed(cx),
            RuntimeEvent::PromptAccepted { submission, .. } => {
                self.handle_prompt_accepted(*submission, window, cx);
            }
            RuntimeEvent::PromptRejected {
                submission, text, ..
            } => self.handle_prompt_rejected(*submission, text, window, cx),
            RuntimeEvent::PromptCompleted { submission } => {
                self.submissions.remove(submission);
            }
            RuntimeEvent::Failed(_)
            | RuntimeEvent::Rejected(_)
            | RuntimeEvent::DialogAnswerFailed { .. }
            | RuntimeEvent::DraftPromoted { .. }
            | RuntimeEvent::WorkspaceAdded { .. }
            | RuntimeEvent::MachineForgotten(_)
            | RuntimeEvent::MachineForgetFailed { .. } => {}
        }
    }

    fn handle_snapshot_changed(&mut self, cx: &mut Context<'_, Self>) {
        let signature = transcript_signature(self.store.read(cx).snapshot(), &self.id);

        if signature != self.transcript_signature {
            self.transcript_signature = signature;
            self.scroll.scroll_to_bottom();
        }
    }

    fn handle_prompt_accepted(
        &mut self,
        submission: SubmissionId,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(submitted) = self.submissions.get_mut(&submission) else {
            return;
        };
        let current = self.input.read(cx).value().to_string();

        submitted.accepted = true;

        if current == submitted.text {
            self.input
                .update(cx, |state, cx| state.set_value("", window, cx));
        }

        self.scroll.scroll_to_bottom();
    }

    fn handle_prompt_rejected(
        &mut self,
        submission: SubmissionId,
        text: &str,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let Some(submitted) = self.submissions.remove(&submission) else {
            return;
        };
        let current = self.input.read(cx).value().to_string();

        if current.is_empty() {
            self.input.update(cx, |state, cx| {
                state.set_value(submitted.text, window, cx);
            });
        } else if current == text {
            self.focus_input = true;
        }

        self.scroll.scroll_to_bottom();
    }

    pub(crate) fn request_controls(&self, cx: &App) {
        self.store.read(cx).request_controls(self.id.clone());
    }

    fn handle_prompt_submitted(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        if !matches!(event, InputEvent::PressEnter { .. }) {
            return;
        }

        self.submit_prompt(window, cx);
    }

    fn submit_prompt(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let input = self.input.clone();
        let text = input.read(cx).value().to_string();
        let can_send = models::can_send(self.store.read(cx).snapshot(), &self.id);

        if text.trim().is_empty() || !can_send || self.awaiting_prompt_acceptance() {
            return;
        }

        let submission = self.store.read(cx).reserve_prompt_submission();

        self.submissions.insert(
            submission,
            SubmittedInput {
                text: text.clone(),
                accepted: false,
            },
        );
        self.store
            .read(cx)
            .submit_prompt(submission, self.id.clone(), text);
        self.scroll.scroll_to_bottom();
    }

    fn awaiting_prompt_acceptance(&self) -> bool {
        self.submissions
            .values()
            .any(|submission| !submission.accepted)
    }

    fn add_repo(&self, cx: &mut Context<'_, Self>) {
        let ChatId::Draft(draft) = &self.id else {
            return;
        };
        let draft = *draft;
        let Some(machine) = self
            .store
            .read(cx)
            .snapshot()
            .draft(draft)
            .and_then(|draft| draft.target.machine.clone())
        else {
            return;
        };

        cx.emit(ChatFlowRequest::AddRepo { draft, machine });
    }

    fn unstarted_subtitle(&self, cx: &Context<'_, Self>) -> SharedString {
        let ChatId::Draft(id) = &self.id else {
            return SharedString::default();
        };
        let id = *id;
        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let Some(draft) = snapshot.draft(id) else {
            return UNRESOLVED_TARGET_SUBTITLE.into();
        };
        let (Some(machine), Some(workspace)) = (
            draft.target.machine.as_ref(),
            draft.target.workspace.as_ref(),
        ) else {
            return UNRESOLVED_TARGET_SUBTITLE.into();
        };

        format!(
            "{} on {}",
            folder_name(workspace.path()),
            snapshot.machine_label(machine)
        )
        .into()
    }

    fn render_transcript(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let chat = cx.entity();
        let ChatId::Session(key) = &self.id else {
            return div().into_any_element();
        };
        let store = self.store.read(cx);
        let Some(session) = store.snapshot().live_session(key) else {
            return ui::empty_pane_hint(Tone::Pending, OPENING_SESSION_HINT, cx).into_any_element();
        };

        self.transcript_presentation
            .render(
                session.replica.transcript(),
                &self.scroll,
                move |tool_call_id, _, cx| {
                    chat.update(cx, |chat, cx| {
                        chat.transcript_presentation.toggle_tool_block(tool_call_id);
                        cx.notify();
                    });
                },
                cx,
            )
            .into_any_element()
    }

    fn render_bottom_row(&self, window: &Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let strip = models::present_prompt_strip(
            self.store.read(cx).snapshot(),
            &self.id,
            focused,
            !self.awaiting_prompt_acceptance(),
        );
        let chat_for_send = cx.entity();
        let chat_for_stop = chat_for_send.clone();
        let send = move |window: &mut Window, cx: &mut App| {
            chat_for_send.update(cx, |chat, cx| chat.submit_prompt(window, cx));
        };
        let stop = move |_window: &mut Window, cx: &mut App| {
            chat_for_stop.update(cx, Self::stop_session);
        };

        ui::prompt_strip(strip, send, stop, &self.input, cx)
    }

    fn stop_session(&mut self, cx: &mut Context<'_, Self>) {
        let ChatId::Session(key) = &self.id else {
            return;
        };

        self.store.read(cx).stop_session(key.clone());
    }

    fn handle_pick_model(
        &mut self,
        action: &ui::PickModel,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let picked = models::model_picks(self.store.read(cx).snapshot(), &self.id)
            .get(action.index)
            .cloned();
        let Some(picked) = picked else {
            return;
        };

        self.store.read(cx).pick_model(self.id.clone(), picked);
    }

    fn handle_pick_machine(
        &mut self,
        action: &ui::PickMachine,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let ChatId::Draft(draft) = &self.id else {
            return;
        };
        let draft = *draft;
        let machine = models::machine_candidates(self.store.read(cx).snapshot())
            .get(action.index)
            .map(|candidate| candidate.id.clone());
        let Some(machine) = machine else {
            return;
        };

        self.store.read(cx).retarget_draft(draft, machine);
        self.store.read(cx).request_controls(self.id.clone());
    }

    fn handle_pick_repo(
        &mut self,
        action: &ui::PickRepo,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let ChatId::Draft(draft) = &self.id else {
            return;
        };
        let draft = *draft;
        let repo = models::repo_candidates(self.store.read(cx).snapshot(), &self.id)
            .get(action.index)
            .map(|repo| (*repo).clone());
        let Some(repo) = repo else {
            self.add_repo(cx);

            return;
        };

        self.store.read(cx).select_workspace(draft, repo);
        self.store.read(cx).request_controls(self.id.clone());
    }

    fn render_unstarted_body(&self, cx: &App) -> impl IntoElement {
        let ChatId::Draft(id) = &self.id else {
            return div().flex_1().min_h_0();
        };
        let id = *id;
        let failure =
            self.store
                .read(cx)
                .snapshot()
                .draft(id)
                .and_then(|draft| match &draft.models {
                    ModelsState::Failed(detail) => Some(format!("{detail}{MODEL_FALLBACK_SUFFIX}")),
                    ModelsState::NotRequested | ModelsState::Loading | ModelsState::Loaded(_) => {
                        None
                    }
                });

        div()
            .flex_1()
            .min_h_0()
            .p_3()
            .children(failure.map(|detail| ui::inline_note(Tone::Failed, detail, cx)))
    }
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        if self.focus_input {
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            self.focus_input = false;
        }

        let pane = div()
            .size_full()
            .flex()
            .flex_col()
            .on_action(cx.listener(Self::handle_pick_model))
            .on_action(cx.listener(Self::handle_pick_machine))
            .on_action(cx.listener(Self::handle_pick_repo));
        let pane = match &self.id {
            ChatId::Draft(_) => pane
                .child(ui::chat_header(
                    NEW_SESSION_LABEL.into(),
                    self.unstarted_subtitle(cx),
                    cx,
                ))
                .child(self.render_unstarted_body(cx).into_any_element()),
            ChatId::Session(_) => pane.child(self.render_transcript(cx).into_any_element()),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}

fn transcript_signature(snapshot: &ClientSnapshot, chat: &ChatId) -> Option<TranscriptSignature> {
    let ChatId::Session(key) = chat else {
        return None;
    };
    Some(snapshot.live_session(key)?.replica.revision())
}
