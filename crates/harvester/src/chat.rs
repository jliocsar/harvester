use gpui::{
    div, prelude::*, App, Context, Entity, EventEmitter, Focusable, ScrollHandle, SharedString,
    Subscription, Window,
};
use gpui_component::input::{InputEvent, InputState};
use serde_json::Value;

use crate::models::{self, ModelChoice};
use crate::session_state::{self, ModelsState, SessionPhase, SessionState};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::{folder_name, machines, MachineCandidate, RequestToken};

pub use session_state::ChatTarget;

pub const NEW_SESSION_LABEL: &str = "New session";

const UNSENT_REQUEST_DETAIL: &str = "Not connected";
const CONNECTION_LOST_DETAIL: &str = "Connection lost";
const MACHINE_CHANGED_DETAIL: &str = "machine changed";
const MODEL_FALLBACK_SUFFIX: &str = " — pi will use its default model";
const UNRESOLVED_TARGET_SUBTITLE: &str = "Pick a machine and a repository below";

pub enum ChatFlowRequest {
    AddRepo { machine: String },
}

impl EventEmitter<ChatFlowRequest> for Chat {}

pub struct Chat {
    state: SessionState,
    store: Entity<FleetStore>,
    transcript_presentation: ui::TranscriptPresentation,
    focus_input: bool,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    _prompt_submitted: Subscription,
    _store_repaint: Subscription,
    _store_events: Subscription,
}

impl Chat {
    pub fn unstarted(
        target: ChatTarget,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokens = store.read(cx).tokens();

        Self::new(SessionState::unstarted(target, tokens), store, window, cx)
    }

    pub fn live(
        sid: String,
        target: ChatTarget,
        entries: &[Value],
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokens = store.read(cx).tokens();
        Self::new(
            SessionState::live(sid, target, entries, tokens),
            store,
            window,
            cx,
        )
    }

    fn new(
        state: SessionState,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = ui::create_prompt_input(window, cx);
        let prompt_submitted = cx.subscribe_in(&input, window, Self::handle_prompt_submitted);
        let store_repaint = cx.observe(&store, |_, _, cx| cx.notify());
        let store_events = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            state,
            store,
            transcript_presentation: ui::TranscriptPresentation::default(),
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            _prompt_submitted: prompt_submitted,
            _store_repaint: store_repaint,
            _store_events: store_events,
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Bridge { machine, event } => self.handle_bridge_event(machine, event, cx),
            StoreEvent::Machines(machines::Event::BranchResolved { token, result }) => {
                self.handle_branch_resolved(*token, result.clone(), cx)
            }
            StoreEvent::Machines(_) | StoreEvent::Failed(_) => {}
        }
    }

    fn handle_bridge_event(&mut self, machine: &str, event: &BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::ConnectFailed { detail } if self.runs_on(machine) => {
                self.fail_pending_requests(detail.clone(), cx)
            }
            BridgeEvent::Disconnected if self.runs_on(machine) => {
                self.fail_pending_requests(CONNECTION_LOST_DETAIL.to_string(), cx)
            }
            BridgeEvent::RequestFailed { .. } if self.runs_on(machine) => {
                if self.state.fail_start() {
                    cx.notify();
                }
            }
            BridgeEvent::ModelsListed { token, result } if self.runs_on(machine) => {
                self.handle_models_listed(*token, result.clone(), cx)
            }
            BridgeEvent::HistoryReplaced { sid, entries } if self.renders(sid) => {
                self.replace_history(entries, cx)
            }
            BridgeEvent::HistoryExtended { sid, entries } if self.renders(sid) => {
                self.extend_history(entries, cx)
            }
            BridgeEvent::Pi { sid, event } if self.renders(sid) => self.handle_pi_event(event, cx),
            BridgeEvent::SessionExit { sid, exit_code } if self.renders(sid) => {
                self.handle_session_exit(*exit_code, cx)
            }
            BridgeEvent::PromptFailed { sid, detail } if self.renders(sid) => {
                self.handle_prompt_failed(detail.clone(), cx)
            }
            BridgeEvent::PiAnswered {
                sid,
                token,
                method,
                result,
            } if self.renders(sid) => self.handle_pi_answered(method, *token, result.clone(), cx),
            _ => {}
        }
    }

    fn runs_on(&self, machine: &str) -> bool {
        self.state.target().machine == machine
    }

    fn renders(&self, sid: &str) -> bool {
        self.state.sid() == Some(sid)
    }

    fn machine_label(&self, cx: &Context<Self>) -> SharedString {
        self.store
            .read(cx)
            .label(&self.state.target().machine)
            .into()
    }

    fn machine_candidates(&self, cx: &Context<Self>) -> Vec<MachineCandidate> {
        self.store.read(cx).machine_candidates()
    }

    fn repo_candidates(&self, cx: &Context<Self>) -> Vec<String> {
        self.store
            .read(cx)
            .catalog()
            .repos_on(&self.state.target().machine)
    }

    pub fn sid(&self) -> Option<&str> {
        self.state.sid()
    }

    pub fn machine(&self) -> &str {
        &self.state.target().machine
    }

    pub fn starting_session(&self) -> Option<(&str, &str)> {
        self.state.phase().starting_session()
    }

    pub fn workspace_path(&self) -> &str {
        &self.state.target().workspace_path
    }

    pub fn request_prompt_controls(&mut self, cx: &mut Context<Self>) {
        self.request_models(cx);
        self.request_branch(cx);
    }

    fn request_models(&mut self, cx: &mut Context<Self>) {
        match self.state.phase() {
            SessionPhase::Unstarted => self.request_model_list(cx),
            SessionPhase::Live { .. } => self.request_model_controls(cx),
            SessionPhase::Starting { .. } => {}
        }
    }

    fn request_model_controls(&mut self, cx: &mut Context<Self>) {
        let Some(commands) = self.state.request_model_controls() else {
            return;
        };

        for command in commands {
            self.send(command, cx);
        }
    }

    fn request_model_list(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self.state.request_model_list() else {
            return;
        };

        self.send(command, cx);
    }

    fn send(&mut self, command: Command, cx: &mut Context<Self>) {
        let machine = self.state.target().machine.clone();
        let ends_the_turn = matches!(command, Command::Prompt { .. });
        let sent = self
            .store
            .update(cx, |store, cx| store.send_gated(&machine, command, cx));

        if sent {
            return;
        }

        self.fail_unsent(ends_the_turn, cx);
    }

    fn fail_unsent(&mut self, ends_the_turn: bool, cx: &mut Context<Self>) {
        let mut changed = self.state.fail_pending(UNSENT_REQUEST_DETAIL.to_string());

        if ends_the_turn {
            changed |= self.state.fail_prompt(UNSENT_REQUEST_DETAIL.to_string());
        }

        if changed {
            cx.notify();
        }
    }

    fn request_branch(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.state.request_branch() else {
            return;
        };
        let target = self.state.target().clone();
        let host = match self.store.read(cx).host_for(&target.machine) {
            Ok(host) => host,
            Err(error) => {
                eprintln!("harvester: branch indicator: {error}");
                self.state.cancel_branch();

                return;
            }
        };
        let command = machines::Command::ResolveBranch {
            host,
            path: target.workspace_path,
            token,
        };
        let sent = self
            .store
            .update(cx, |store, cx| store.send_host_command(command, cx));

        if !sent {
            self.state.cancel_branch();
        }
    }

    fn handle_branch_resolved(
        &mut self,
        token: RequestToken,
        result: Result<String, String>,
        cx: &mut Context<Self>,
    ) {
        if let Err(detail) = &result {
            eprintln!("harvester: branch indicator: {detail}");
        }

        if self.state.accept_branch(token, result) {
            cx.notify();
        }
    }

    pub fn promote_to_live(&mut self, sid: String, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.promote_to_live(sid, entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    fn replace_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.replace_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    fn extend_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.extend_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    fn fail_pending_requests(&mut self, detail: String, cx: &mut Context<Self>) {
        if self.state.fail_pending(detail) {
            cx.notify();
        }
    }

    fn handle_models_listed(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.accept_models(token, result) {
            cx.notify();
        }
    }

    fn handle_pi_answered(
        &mut self,
        method: &str,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.accept_pi_answer(method, token, result) {
            cx.notify();
        }
    }

    fn handle_prompt_failed(&mut self, detail: String, cx: &mut Context<Self>) {
        if self.state.fail_prompt(detail) {
            cx.notify();
        }
    }

    fn pick_model(&mut self, picked: Option<ModelChoice>, cx: &mut Context<Self>) {
        if let Some(command) = self.state.pick_model(picked) {
            self.send(command, cx);
        }

        cx.notify();
    }

    fn pick_machine(&mut self, machine: String, cx: &mut Context<Self>) {
        if self.state.target().machine == machine {
            cx.notify();

            return;
        }

        self.state
            .cancel_model_fetch(MACHINE_CHANGED_DETAIL.to_string());

        if self.state.retarget(machine) {
            let first_repo = self.repo_candidates(cx).first().cloned();

            self.state.pick_repo(first_repo.unwrap_or_default());
            self.request_prompt_controls(cx);
        }

        cx.notify();
    }

    pub fn select_repo(&mut self, path: String, cx: &mut Context<Self>) {
        if self.state.pick_repo(path) {
            self.request_prompt_controls(cx);
        }

        cx.notify();
    }

    fn add_repo(&mut self, cx: &mut Context<Self>) {
        let machine = self.state.target().machine.clone();

        if machine.is_empty() {
            return;
        }

        cx.emit(ChatFlowRequest::AddRepo { machine });
    }

    pub fn is_live(&self) -> bool {
        self.state.is_live()
    }

    fn handle_session_exit(&mut self, exit_code: Option<i32>, cx: &mut Context<Self>) {
        if self.state.exit(exit_code) {
            cx.notify();
        }
    }

    fn handle_pi_event(&mut self, event: &Value, cx: &mut Context<Self>) {
        if session_state::is_turn_settled(event) {
            self.request_branch(cx);
        }

        if !self.state.apply_pi_event(event) {
            return;
        }

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    fn handle_prompt_submitted(
        &mut self,
        _input: &Entity<InputState>,
        event: &InputEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !matches!(event, InputEvent::PressEnter { .. }) {
            return;
        }

        self.submit_prompt(window, cx);
    }

    fn submit_text(&mut self, text: String, cx: &mut Context<Self>) {
        let Some(command) = self.state.submit_prompt(text) else {
            return;
        };

        self.scroll.scroll_to_bottom();
        cx.notify();
        self.send(command, cx);
    }

    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.clone();
        let text = input.read(cx).value().to_string();

        if text.trim().is_empty() {
            return;
        }

        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.submit_text(text, cx);
    }

    fn unstarted_subtitle(&self, cx: &Context<Self>) -> SharedString {
        if !self.state.target().is_resolved() {
            return UNRESOLVED_TARGET_SUBTITLE.into();
        }

        format!(
            "{} on {}",
            folder_name(&self.state.target().workspace_path),
            self.machine_label(cx)
        )
        .into()
    }

    fn render_transcript(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chat = cx.entity();

        self.transcript_presentation.render(
            self.state.transcript(),
            &self.scroll,
            move |tool_call_id, _, cx| {
                chat.update(cx, |chat, cx| {
                    chat.transcript_presentation.toggle_tool_block(tool_call_id);
                    cx.notify();
                });
            },
            cx,
        )
    }

    fn render_bottom_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model_picks = models::model_picks(&self.state);
        let machine_candidates = self.machine_candidates(cx);
        let repo_candidates = self.repo_candidates(cx);
        let focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        let strip = models::present_prompt_strip(
            &self.state,
            &model_picks,
            &machine_candidates,
            &self.machine_label(cx),
            &repo_candidates,
            focused,
        );
        let chat_for_send = cx.entity();
        let chat_for_stop = chat_for_send.clone();
        let send = move |window: &mut Window, cx: &mut App| {
            chat_for_send.update(cx, |chat, cx| chat.submit_prompt(window, cx));
        };
        let stop = move |_window: &mut Window, cx: &mut App| {
            chat_for_stop.update(cx, |chat, cx| chat.stop_session(cx));
        };

        ui::prompt_strip(strip, send, stop, &self.input, cx)
    }

    fn stop_session(&mut self, cx: &mut Context<Self>) {
        let Some(command) = self.state.stop_command() else {
            return;
        };

        self.send(command, cx);
    }

    fn handle_pick_model(
        &mut self,
        action: &ui::PickModel,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pick) = models::model_picks(&self.state).get(action.index).cloned() else {
            return;
        };

        self.pick_model(pick, cx);
    }

    fn handle_pick_machine(
        &mut self,
        action: &ui::PickMachine,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(candidate) = self.machine_candidates(cx).get(action.index).cloned() else {
            return;
        };

        self.pick_machine(candidate.key, cx);
    }

    fn handle_pick_repo(
        &mut self,
        action: &ui::PickRepo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo) = self.repo_candidates(cx).get(action.index).cloned() else {
            self.add_repo(cx);

            return;
        };

        self.select_repo(repo, cx);
    }

    fn render_unstarted_body(&self, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_h_0()
            .p_3()
            .children(match self.state.models() {
                ModelsState::Failed(detail) => Some(ui::inline_note(
                    Tone::Failed,
                    format!("{detail}{MODEL_FALLBACK_SUFFIX}"),
                    cx,
                )),
                _ => None,
            })
    }
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accepts_input = self.state.exited_notice().is_none()
            && !matches!(self.state.phase(), SessionPhase::Starting { .. });

        if self.focus_input && accepts_input {
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
        let pane = match self.state.phase() {
            SessionPhase::Unstarted | SessionPhase::Starting { .. } => pane
                .child(ui::chat_header(
                    NEW_SESSION_LABEL.into(),
                    self.unstarted_subtitle(cx),
                    cx,
                ))
                .child(self.render_unstarted_body(cx)),
            SessionPhase::Live { .. } => pane.child(self.render_transcript(cx)),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}
