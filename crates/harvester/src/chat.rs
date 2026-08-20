mod prompt_presentation;

use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, Focusable, ScrollHandle, SharedString,
    Subscription, Window,
};
use gpui_component::input::{InputEvent, InputState};
use serde_json::Value;

use crate::models::ModelChoice;
use crate::session_state::{self, ModelSelection, ModelsState, SessionPhase, SessionState};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;
use harvester_client_core::bridge::Event as BridgeEvent;
use harvester_client_core::{machines, MachineCandidate, RequestToken};
use prompt_presentation::{
    machine_control_label, model_control_state, model_row_label, repo_control_label, ADD_REPO_ROW,
};

pub use session_state::{ChatIntent as ChatRequest, ChatTarget};

impl EventEmitter<session_state::ChatIntent> for Chat {}

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
    _subscriptions: [Subscription; 3],
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
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Message pi — Enter to send")
                .multi_line(true)
                .auto_grow(1, 8)
                .submit_on_enter(true)
        });
        let subscriptions = [
            cx.subscribe_in(&input, window, Self::handle_prompt_submitted),
            cx.observe(&store, |_, _, cx| cx.notify()),
            cx.subscribe(&store, Self::handle_store_event),
        ];
        Self {
            state,
            store,
            transcript_presentation: ui::TranscriptPresentation::default(),
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }

    fn handle_store_event(
        &mut self,
        _store: Entity<FleetStore>,
        event: &StoreEvent,
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
                self.fail_pending_requests("Connection lost".to_string(), cx)
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
            BridgeEvent::SessionModelsListed { sid, token, result } if self.renders(sid) => {
                self.handle_models_listed(*token, result.clone(), cx)
            }
            BridgeEvent::SessionState { sid, token, result } if self.renders(sid) => {
                self.handle_session_state(*token, result.clone(), cx)
            }
            BridgeEvent::ModelSet { sid, token, result } if self.renders(sid) => {
                self.handle_model_set(*token, result, cx)
            }
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
        self.store.read(cx).repos_on(&self.state.target().machine)
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
        for intent in self.state.request_model_controls() {
            cx.emit(intent);
        }
    }

    fn request_model_list(&mut self, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.request_model_list() {
            cx.emit(intent);
        }
    }

    fn request_branch(&mut self, cx: &mut Context<Self>) {
        let Some(token) = self.state.request_branch() else {
            return;
        };
        let target = self.state.target().clone();
        let host = match self.store.read(cx).host_for(&target.machine) {
            Ok(host) => host,
            Err(detail) => {
                eprintln!("harvester: branch indicator: {detail}");
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

    pub fn fail_pending_requests(&mut self, detail: String, cx: &mut Context<Self>) {
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

    fn handle_session_state(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.state.accept_session_state(token, result) {
            cx.notify();
        }
    }

    fn handle_model_set(
        &mut self,
        token: RequestToken,
        result: &Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self
            .state
            .accept_model_set(token, result.as_ref().map_err(|error| error.clone()))
        {
            cx.notify();
        }
    }

    fn model_picks(&self) -> Vec<Option<ModelChoice>> {
        if matches!(self.state.phase(), SessionPhase::Starting { .. }) {
            return Vec::new();
        }

        let offer_default = matches!(self.state.phase(), SessionPhase::Unstarted);

        offer_default
            .then_some(None)
            .into_iter()
            .chain(self.state.model_choices().iter().cloned().map(Some))
            .collect()
    }

    fn is_current_model(&self, pick: &Option<ModelChoice>) -> bool {
        match (pick, self.state.selection()) {
            (None, ModelSelection::LaunchDefault) => true,
            (Some(choice), ModelSelection::Launch(current) | ModelSelection::Live(current)) => {
                choice.provider == current.provider && choice.id == current.id
            }
            _ => false,
        }
    }

    fn pick_model(&mut self, picked: Option<ModelChoice>, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.pick_model(picked) {
            cx.emit(intent);
        }

        cx.notify();
    }

    fn pick_machine(&mut self, machine: String, cx: &mut Context<Self>) {
        if self.state.target().machine == machine {
            cx.notify();

            return;
        }

        self.state.cancel_model_fetch("machine changed".to_string());

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
        if let Some(intent) = self.state.submit_prompt(text) {
            cx.emit(intent);
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
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
            return "Pick a machine and a repository below".into();
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
        let model_picks = self.model_picks();
        let machine_candidates = self.machine_candidates(cx);
        let repo_candidates = self.repo_candidates(cx);
        let strip = self.prompt_strip(
            &model_picks,
            &machine_candidates,
            &repo_candidates,
            window,
            cx,
        );
        let handlers = create_prompt_strip_handlers(
            cx.entity(),
            model_picks,
            machine_candidates,
            repo_candidates,
        );

        ui::prompt_strip(strip, handlers, &self.input, cx)
    }

    fn prompt_strip(
        &self,
        model_picks: &[Option<ModelChoice>],
        machine_candidates: &[MachineCandidate],
        repo_candidates: &[String],
        window: &Window,
        cx: &Context<Self>,
    ) -> ui::PromptStrip {
        ui::PromptStrip {
            notice: self.prompt_strip_notice(),
            model: self.model_control(model_picks),
            machine: self.machine_control(machine_candidates, cx),
            repo: self.repo_control(repo_candidates),
            branch: self.branch_label(),
            can_send: self.can_send(),
            focused: self.input.read(cx).focus_handle(cx).is_focused(window),
        }
    }

    fn prompt_strip_notice(&self) -> Option<ui::PromptStripNotice> {
        match (self.state.exited_notice(), self.state.phase()) {
            (Some(notice), _) => Some(ui::PromptStripNotice::SessionExited(
                notice.to_string().into(),
            )),
            (None, SessionPhase::Starting { .. }) => Some(ui::PromptStripNotice::SessionStarting),
            (None, _) => None,
        }
    }

    fn is_unstarted(&self) -> bool {
        matches!(self.state.phase(), SessionPhase::Unstarted)
    }

    fn can_send(&self) -> bool {
        !self.is_unstarted() || self.state.target().is_resolved()
    }

    fn branch_label(&self) -> Option<SharedString> {
        Some(self.state.branch()?.to_string().into())
    }

    fn model_control_state(&self) -> (SharedString, bool) {
        model_control_state(
            self.state.selection(),
            self.state.models(),
            self.state.model_choices(),
        )
    }

    fn model_control(&self, picks: &[Option<ModelChoice>]) -> ui::PromptStripControl {
        let (label, enabled) = self.model_control_state();
        let rows = picks
            .iter()
            .map(|pick| ui::DropdownRow {
                label: model_row_label(pick.as_ref()),
                selected: self.is_current_model(pick),
                divider_above: false,
            })
            .collect();

        ui::PromptStripControl {
            label,
            enabled,
            rows,
        }
    }

    fn machine_control(
        &self,
        candidates: &[MachineCandidate],
        cx: &Context<Self>,
    ) -> ui::PromptStripControl {
        let label = machine_control_label(
            &self.state.target().machine,
            &self.machine_label(cx),
            candidates,
        );
        let enabled = self.is_unstarted() && !candidates.is_empty();
        let rows = candidates
            .iter()
            .map(|candidate| ui::DropdownRow {
                label: candidate.label.clone().into(),
                selected: candidate.key == self.state.target().machine,
                divider_above: false,
            })
            .collect();

        ui::PromptStripControl {
            label,
            enabled,
            rows,
        }
    }

    fn repo_control(&self, repos: &[String]) -> ui::PromptStripControl {
        let path = &self.state.target().workspace_path;
        let label = repo_control_label(path);
        let enabled = self.is_unstarted() && !self.state.target().machine.is_empty();
        let mut rows: Vec<ui::DropdownRow> = repos
            .iter()
            .map(|repo| ui::DropdownRow {
                label: repo.clone().into(),
                selected: repo == path,
                divider_above: false,
            })
            .collect();

        rows.push(ui::DropdownRow {
            label: ADD_REPO_ROW.into(),
            selected: false,
            divider_above: !repos.is_empty(),
        });

        ui::PromptStripControl {
            label,
            enabled,
            rows,
        }
    }

    fn render_unstarted_body(&self, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .children(match self.state.models() {
                ModelsState::Failed(detail) => Some(ui::inline_note(
                    Tone::Failed,
                    format!("{detail} — pi will use its default model"),
                    cx,
                )),
                _ => None,
            })
    }
}

fn create_prompt_strip_handlers(
    chat: Entity<Chat>,
    model_picks: Vec<Option<ModelChoice>>,
    machine_candidates: Vec<MachineCandidate>,
    repo_candidates: Vec<String>,
) -> ui::PromptStripHandlers {
    let machine_keys: Vec<String> = machine_candidates
        .into_iter()
        .map(|candidate| candidate.key)
        .collect();
    let model_chat = chat.clone();
    let machine_chat = chat.clone();
    let repo_chat = chat.clone();

    ui::PromptStripHandlers::new(
        move |index, _, cx| {
            let Some(pick) = model_picks.get(index).cloned() else {
                return;
            };

            model_chat.update(cx, |chat, cx| chat.pick_model(pick, cx));
        },
        move |index, _, cx| {
            let Some(machine) = machine_keys.get(index).cloned() else {
                return;
            };

            machine_chat.update(cx, |chat, cx| chat.pick_machine(machine, cx));
        },
        move |index, _, cx| {
            let picked = repo_candidates.get(index).cloned();

            repo_chat.update(cx, |chat, cx| match picked {
                Some(repo) => chat.select_repo(repo, cx),
                None => chat.add_repo(cx),
            });
        },
        move |window, cx| {
            chat.update(cx, |chat, cx| chat.submit_prompt(window, cx));
        },
    )
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accepts_input = self.state.exited_notice().is_none()
            && !matches!(self.state.phase(), SessionPhase::Starting { .. });

        if self.focus_input && accepts_input {
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            self.focus_input = false;
        }

        let pane = div().size_full().flex().flex_col();
        let pane = match self.state.phase() {
            SessionPhase::Unstarted | SessionPhase::Starting { .. } => pane
                .child(ui::chat_header(
                    "New session".into(),
                    self.unstarted_subtitle(cx),
                    cx,
                ))
                .child(self.render_unstarted_body(cx)),
            SessionPhase::Live { .. } => pane.child(self.render_transcript(cx)),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}
