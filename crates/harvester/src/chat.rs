//! One session's conversation, from before it exists until after its pi exits.
//!
//! A chat opens **unstarted** — client-side only, no session on any machine
//! (docs/SPEC.md §Flows "Start session") — and its first prompt mints the sid
//! that makes it live. These are one component with a state rather than two
//! components with a shared vocabulary, because everything the user touches is
//! the same on both sides of that line: the same pane, the same prompt box,
//! the same model control, the same input. What actually differs is small and
//! named in [`SessionPhase`] and [`ModelSelection`]: what a model pick *means* (a launch-line argument
//! versus a `set_model` the running pi must confirm), and whether the machine
//! can still be changed.
//!
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Entity, EventEmitter, Focusable, ScrollHandle,
    SharedString, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use serde_json::Value;

use crate::models::ModelChoice;
use crate::session_state::{self, ModelSelection, ModelsState, SessionPhase, SessionState};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;
use harvester_client_core::bridge::Event as BridgeEvent;
use harvester_client_core::RequestToken;

/// The unstarted chat's "no model chosen" row: pi launches with its own
/// default, and saying so is more honest than naming a model we did not pick.
const DEFAULT_MODEL_ROW: &str = "default (pi chooses)";

/// One (machine key, display label) a chat could run on.
pub type MachineCandidate = (String, String);

pub use session_state::{ChatIntent as ChatRequest, ChatTarget};

impl EventEmitter<session_state::ChatIntent> for Chat {}

pub struct Chat {
    /// The window-free owner of the session lifecycle, model state, request
    /// correlation and transcript. The fields below are GPUI adaptation only.
    state: SessionState,
    /// Where this chat's machine label and its retargeting candidates come
    /// from, read on every render. Observed, so a provider listing that
    /// renames a machine or a workspace the user just added reaches this pane
    /// without anyone remembering to push it (SPEC C12).
    store: Entity<FleetStore>,
    /// Message/tool/markdown policy and stable tool-expansion state. It reads
    /// the client-core transcript without copying or normalizing its items.
    transcript_presentation: ui::TranscriptPresentation,
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    /// The input's Enter, the store's repaints, and the store's events. Held
    /// rather than detached so all three die with this pane.
    _subscriptions: [Subscription; 3],
}

impl Chat {
    /// A chat with no session behind it yet, already asking for the models on
    /// the machine behind it — the seconds the uncached `list_models` costs
    /// hide behind the user typing.
    pub fn unstarted(
        target: ChatTarget,
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let tokens = store.read(cx).tokens();
        let mut chat = Self::new(SessionState::unstarted(target, tokens), store, window, cx);

        chat.request_model_list(cx);

        chat
    }

    /// A chat over a session that already exists: `entries` is pi's history,
    /// verbatim (empty for a session that has never been prompted).
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
        // Multi-line so a prompt can hold a paragraph, but Enter still sends —
        // `submit_on_enter` defaults to false, and without it Enter would only
        // ever insert a newline. Shift-Enter is the newline.
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

    /// The store's events, filtered to this chat's own.
    ///
    /// Every answer pi produces for this pane arrives here rather than being
    /// routed in from above: a chat knows which sid it renders and which
    /// machine it runs on, and that is the whole of the routing rule (SPEC
    /// C12). Guards rather than a total match because *whose* an event is,
    /// is the question — the fall-through is another chat's, another
    /// machine's, or `Attached`, which builds chats and so is the workbench's.
    fn handle_store_event(
        &mut self,
        _store: Entity<FleetStore>,
        event: &StoreEvent,
        cx: &mut Context<Self>,
    ) {
        let StoreEvent::Bridge { machine, event } = event else {
            return;
        };

        match event {
            // Machine-scoped: the bridge this chat runs on died, or answered
            // the throwaway `list_models` an unstarted chat asked it for —
            // that one has no sid to be found by.
            BridgeEvent::ConnectFailed { detail } if self.runs_on(machine) => {
                self.fail_pending_requests(detail.clone(), cx)
            }
            BridgeEvent::Disconnected if self.runs_on(machine) => {
                self.fail_pending_requests("connection lost".to_string(), cx)
            }
            BridgeEvent::RequestFailed { .. } if self.runs_on(machine) => {
                if self.state.fail_start() {
                    cx.notify();
                }
            }
            BridgeEvent::ModelsListed { token, result } if self.runs_on(machine) => {
                self.handle_models_listed(*token, result.clone(), cx)
            }
            // Sid-scoped: sids are uuids, unique across machines, so these
            // need no machine to be routed by.
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
            // A failed `set_model` is the store's to report; this pane only
            // moves its label when pi confirms one.
            BridgeEvent::ModelSet { sid, token, result } if self.renders(sid) => {
                self.handle_model_set(*token, result, cx)
            }
            _ => {}
        }
    }

    /// Whether this chat runs on `machine`.
    ///
    /// Scoping by machine is what keeps one machine's failure from cancelling
    /// a start still in flight on another — without it a retry mints a second
    /// sid and starts a second pi for one prompt.
    fn runs_on(&self, machine: &str) -> bool {
        self.state.target().machine == machine
    }

    /// Whether this chat is the one rendering `sid`.
    fn renders(&self, sid: &str) -> bool {
        self.state.sid() == Some(sid)
    }

    /// How this chat's machine is named on screen — the store's answer, never
    /// a copy of it.
    fn machine_label(&self, cx: &Context<Self>) -> SharedString {
        self.store
            .read(cx)
            .label(&self.state.target().machine)
            .into()
    }

    /// The machines this chat could still be retargeted to: the stored
    /// workspaces sharing its cwd. Derived on every read, so adding a
    /// workspace enables the control without anyone saying so. Only ever
    /// consulted while unstarted — a running session never moves.
    fn machine_candidates(&self, cx: &Context<Self>) -> Vec<MachineCandidate> {
        self.store
            .read(cx)
            .machine_candidates_for(&self.state.target().workspace_path)
    }

    /// `None` for an unstarted chat — it names no session (see [`SessionPhase`]).
    pub fn sid(&self) -> Option<&str> {
        self.state.sid()
    }

    pub fn machine(&self) -> &str {
        &self.state.target().machine
    }

    /// The sid this chat is waiting on `Attached` for, and the name it minted
    /// for it — what the workbench needs to synthesize the sidebar row.
    pub fn starting_session(&self) -> Option<(&str, &str)> {
        self.state.phase().starting_session()
    }

    pub fn workspace_path(&self) -> &str {
        &self.state.target().workspace_path
    }

    /// Ask for what a live chat's prompt box needs. Separate from the
    /// constructor because an entity cannot emit to a subscriber that does not
    /// exist yet: the workbench subscribes to the chat it just created, then
    /// calls this.
    pub fn request_model_controls(&mut self, cx: &mut Context<Self>) {
        for intent in self.state.request_model_controls() {
            cx.emit(intent);
        }
    }

    /// The unstarted chat's model list: a throwaway pi in this chat's cwd.
    fn request_model_list(&mut self, cx: &mut Context<Self>) {
        cx.emit(self.state.request_model_list());
    }

    /// The create → attach → prompt chain landed: this chat is a live session
    /// now. An empty snapshot is the brand-new-session case (docs/SPEC.md C7),
    /// where the prompt already on screen is the only history there is — so it
    /// stays rather than being blanked out from under the user.
    pub fn promote_to_live(&mut self, sid: String, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.promote_to_live(sid, entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// A resync (`event_gap`, or a reconnect's refetch) answered: the entries
    /// are authoritative, so they replace what is rendered. Deliberately not a
    /// new chat entity — rebuilding is what let a late resync clobber the chat
    /// on screen, discarding the input's contents and its focus with it. The
    /// in-flight partial message goes because the snapshot supersedes it; the
    /// next `message_update` carries the whole partial again.
    fn replace_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.replace_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The same repair, cheap: only the entries after the last fetch's cursor
    /// (docs/SPEC.md C6). The transcript owns what that means for the items it
    /// built from live events in the meantime.
    fn extend_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        if self.state.extend_history(entries) {
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The machine's bridge died: nothing this chat asked for will arrive.
    pub fn fail_pending_requests(&mut self, detail: String, cx: &mut Context<Self>) {
        if self.state.fail_pending(detail) {
            cx.notify();
        }
    }

    /// A model list answered, from either source. A chat that has since been
    /// retargeted is waiting on a different token, so the abandoned request's
    /// answer is dropped.
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

    /// A `GetSessionState` answered; the current model labels the control.
    /// Failure is cosmetic (generic label), so it is only logged.
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

    /// A `set_model` succeeded; pi's `data` is the full Model object now in
    /// effect (failures surface in the statusbar, workbench-side).
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

    /// Every model this chat can offer, in row order.
    ///
    /// Only a chat with no session yet offers the "default (pi chooses)" row —
    /// a running session always has a concrete model (SPEC §Flows "Switch
    /// model") — and that is derived from the state, not stored: a stored
    /// answer would be one more thing that could disagree with it.
    ///
    /// The picked model travels with the row rather than as an index into a
    /// list that may be refetched while the dropdown is open, so a pick always
    /// applies the model the user actually read.
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

    /// Whether `pick` is the model this chat is already on.
    fn is_current_model(&self, pick: &Option<ModelChoice>) -> bool {
        match (pick, self.state.selection()) {
            (None, ModelSelection::LaunchDefault) => true,
            (Some(choice), ModelSelection::Launch(current) | ModelSelection::Live(current)) => {
                choice.provider == current.provider && choice.id == current.id
            }
            _ => false,
        }
    }

    /// A picker row was clicked; `None` is the "default (pi chooses)" row.
    /// Unstarted the pick is local (it becomes a launch-line argument); live it
    /// costs a `set_model` the session must confirm, so the label does not move
    /// until [`Chat::handle_model_set`] says pi agreed.
    fn pick_model(&mut self, picked: Option<ModelChoice>, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.pick_model(picked) {
            cx.emit(intent);
        }

        cx.notify();
    }

    fn can_switch_machine(&self, cx: &Context<Self>) -> bool {
        self.machine_candidates(cx).len() >= 2
    }

    /// Retarget to another machine: the typed prompt survives, only the
    /// destination changes. A pick of the current machine changes nothing, so
    /// nothing is refetched. The stale answer from the old machine is dropped
    /// by the model control's own token.
    fn pick_machine(&mut self, machine: String, cx: &mut Context<Self>) {
        if self.state.target().machine == machine {
            cx.notify();

            return;
        }

        if self.state.cancel_model_fetch("machine changed".to_string()) {
            cx.notify();
        }

        if self.state.retarget(machine) {
            self.request_model_list(cx);
            cx.notify();
        }
    }

    /// Whether this chat's pi is still believed to be running. Based on the
    /// `session_exit` events this chat actually received, so unlike a cached
    /// list status it cannot claim "running" about a session that has died.
    pub fn is_live(&self) -> bool {
        self.state.is_live()
    }

    fn handle_session_exit(&mut self, exit_code: Option<i32>, cx: &mut Context<Self>) {
        if self.state.exit(exit_code) {
            cx.notify();
        }
    }

    fn handle_pi_event(&mut self, event: &Value, cx: &mut Context<Self>) {
        // `false` is an event this model does not cover (retries, compaction,
        // queue): nothing moved, so nothing is redrawn.
        if !self.state.apply_pi_event(event) {
            return;
        }

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// Enter in the prompt box. The input is multi-line, so `submit_on_enter`
    /// is what separates a send from a newline: shift-Enter never reaches
    /// here.
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

    /// Both send paths end here: take what is typed, clear the box, route it
    /// by state.
    fn submit_text(&mut self, text: String, cx: &mut Context<Self>) {
        if let Some(intent) = self.state.submit_prompt(text) {
            cx.emit(intent);
            self.scroll.scroll_to_bottom();
            cx.notify();
        }
    }

    /// The send button's path; Enter inside the input reaches the same
    /// `PromptSubmitted` through the `Submit` action.
    fn submit_prompt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input = self.input.clone();
        let text = input.read(cx).value().to_string();

        if text.trim().is_empty() {
            return;
        }

        input.update(cx, |state, cx| state.set_value("", window, cx));
        self.submit_text(text, cx);
    }

    /// The unstarted chat's header line: which repository, on which machine.
    /// A live chat has no header — its prompt box already names both, and the
    /// conversation identifies it better than any title could.
    fn unstarted_subtitle(&self, cx: &Context<Self>) -> SharedString {
        format!(
            "{} on {}",
            folder_name(&self.state.target().workspace_path),
            self.machine_label(cx)
        )
        .into()
    }

    /// Adapt the transcript presentation's tool toggle back into this chat's
    /// GPUI entity. The presentation owns expansion; the chat owns the entity
    /// lifetime and notification that follows a click.
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

    /// The pane's bottom row: the prompt box, or the one sentence that
    /// replaces it when this chat cannot accept input.
    fn render_bottom_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The prompt box floats in the pane's padding (no separator line —
        // the card's own border is the boundary).
        let row = div().flex_none().px_3().pb_3();
        let notice = match (self.state.exited_notice(), self.state.phase()) {
            (Some(notice), _) => Some(notice.to_string()),
            (None, SessionPhase::Starting { .. }) => Some("starting session…".to_string()),
            (None, _) => None,
        };

        if let Some(notice) = notice {
            return row.child(
                div()
                    .text_xs()
                    .text_color(Tone::Idle.color(cx))
                    .child(notice),
            );
        }

        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);

        row.child(self.render_prompt_box(input_focused, cx))
    }

    /// One prompt box for both states (SPEC §Flows: the agent-input pattern,
    /// `model · repository · machine`). The repository is always read-only;
    /// the machine is read-only too once a session exists, because a running
    /// session never moves.
    fn render_prompt_box(&self, input_focused: bool, cx: &mut Context<Self>) -> impl IntoElement {
        let (model_label, model_enabled) = self.model_control_state();
        let machine_enabled =
            matches!(self.state.phase(), SessionPhase::Unstarted) && self.can_switch_machine(cx);
        let machine_label = self.machine_label(cx);

        ui::prompt_box(input_focused, cx)
            .child(ui::prompt_box_input(
                Input::new(&self.input).appearance(false).p_0(),
            ))
            .child(
                ui::prompt_box_controls()
                    .child(self.render_model_control(model_label, model_enabled, cx))
                    .child(ui::prompt_box_control(
                        "chat-repo",
                        folder_name(&self.state.target().workspace_path)
                            .to_string()
                            .into(),
                        false,
                        cx,
                    ))
                    .child(self.render_machine_control(machine_label, machine_enabled, cx))
                    .child(div().flex_1())
                    .child(ui::prompt_box_send("chat-send", cx).on_click(
                        cx.listener(|chat, _, window, cx| chat.submit_prompt(window, cx)),
                    )),
            )
    }

    /// What the model control says and whether it can be clicked. Unstarted, a
    /// list that never arrived is not an error the user must clear — pi still
    /// launches with its default — so the control just reads that way.
    fn model_control_state(&self) -> (SharedString, bool) {
        let choices_available = !self.state.model_choices().is_empty();

        match self.state.selection() {
            ModelSelection::Launch(choice) | ModelSelection::Live(choice) => {
                (choice.name.clone().into(), choices_available)
            }
            ModelSelection::LaunchDefault => match self.state.models() {
                ModelsState::Loading => ("loading models…".into(), false),
                ModelsState::Loaded(_) => ("default model".into(), true),
                ModelsState::NotRequested | ModelsState::Failed(_) => {
                    ("default model".into(), false)
                }
            },
            ModelSelection::LivePending => ("model".into(), choices_available),
            ModelSelection::LiveUnavailable => ("model unavailable".into(), choices_available),
        }
    }

    /// The model control, and the list it drops. A control with nothing to
    /// offer renders as the bare label: a dropdown that opens onto no rows is
    /// worse than one that does not open.
    fn render_model_control(
        &self,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = ui::prompt_box_control("chat-model", label, enabled, cx);
        let picks = self.model_picks();

        if !enabled || picks.is_empty() {
            return control.into_any_element();
        }

        let rows = picks
            .iter()
            .map(|pick| ui::DropdownRow {
                label: match pick {
                    Some(choice) => choice.name.clone().into(),
                    None => DEFAULT_MODEL_ROW.into(),
                },
                selected: self.is_current_model(pick),
            })
            .collect();
        let chat = cx.entity();

        ui::prompt_box_dropdown("chat-model-dropdown", control, rows, move |index, _, cx| {
            let Some(pick) = picks.get(index).cloned() else {
                return;
            };

            chat.update(cx, |chat, cx| chat.pick_model(pick, cx));
        })
        .into_any_element()
    }

    /// The machine control, and the workspaces sharing this chat's path. Only
    /// an unstarted chat drops a list — a running session never moves.
    fn render_machine_control(
        &self,
        label: SharedString,
        enabled: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let control = ui::prompt_box_control("chat-machine", label, enabled, cx);

        if !enabled {
            return control.into_any_element();
        }

        let candidates = self.machine_candidates(cx);
        let rows = candidates
            .iter()
            .map(|(machine, label)| ui::DropdownRow {
                label: label.clone().into(),
                selected: machine == &self.state.target().machine,
            })
            .collect();
        // The keys travel with the pick, so a listing landing while the list is
        // open cannot move a row out from under the click.
        let keys: Vec<String> = candidates
            .into_iter()
            .map(|(machine, _label)| machine)
            .collect();
        let chat = cx.entity();

        ui::prompt_box_dropdown(
            "chat-machine-dropdown",
            control,
            rows,
            move |index, _, cx| {
                let Some(machine) = keys.get(index).cloned() else {
                    return;
                };

                chat.update(cx, |chat, cx| chat.pick_machine(machine, cx));
            },
        )
        .into_any_element()
    }

    /// The unstarted body: empty, except when the model fetch failed — which
    /// is informational, not blocking, since pi still launches with a default.
    fn render_unstarted_body(&self, cx: &App) -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .children(match self.state.models() {
                ModelsState::Failed(detail) => Some(
                    div()
                        .text_xs()
                        .text_color(Tone::Failed.color(cx))
                        .child(format!("{detail} — pi will use its default model")),
                ),
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

        let pane = div().size_full().flex().flex_col();
        let pane = match self.state.phase() {
            // No conversation yet, so the header is what identifies the pane.
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
