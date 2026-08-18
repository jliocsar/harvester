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

use std::collections::HashSet;

use gpui::{
    div, prelude::*, px, AnyElement, App, Context, Div, Entity, EventEmitter, Focusable,
    ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use serde_json::Value;

use crate::models::ModelChoice;
use crate::session_state::{self, ModelSelection, ModelsState, SessionPhase, SessionState};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;
use harvester_client_core::bridge::Event as BridgeEvent;
use harvester_client_core::transcript::{Item, Message, Role, ToolBlock, ToolStatus};
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
    /// Tool blocks the user has opened, by `toolCallId`. Not by position: a
    /// resync replaces every item, and expansion keyed by index would land on
    /// whatever block took that slot.
    expanded: HashSet<String>,
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
            expanded: HashSet::new(),
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

    /// A tool block's header was clicked: toggle its body.
    fn toggle_tool_block(&mut self, tool_call_id: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&tool_call_id) {
            self.expanded.insert(tool_call_id);
        }

        cx.notify();
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

    fn render_messages(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("chat-messages")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_3()
                    .children(
                        self.state
                            .transcript()
                            .items()
                            .iter()
                            .enumerate()
                            .map(|(index, item)| self.render_item(index, item, cx)),
                    )
                    .children(self.render_streaming(cx)),
            )
    }

    /// One conversation item. The index is the element id only — what a click
    /// toggles is the block's `toolCallId`, which survives a resync.
    fn render_item(&self, index: usize, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        match item {
            Item::Message(message) => render_message(index, message, cx).into_any_element(),
            Item::Tool(block) => self.render_tool_block(index, block, cx).into_any_element(),
            Item::Failure(detail) => render_failure(detail, cx).into_any_element(),
        }
    }

    fn render_tool_block(
        &self,
        index: usize,
        block: &ToolBlock,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tone = match block.status {
            // Running is the accent, not "pending": a tool the user is
            // watching run is the live thing on screen.
            ToolStatus::Running => Tone::Active,
            ToolStatus::Ok => Tone::Idle,
            ToolStatus::Error => Tone::Failed,
        };
        let expanded = self.expanded.contains(&block.tool_call_id);
        let tool_call_id = block.tool_call_id.clone();
        let header = ui::tool_block_header(
            ("tool-block", index),
            tone,
            tool_block_label(block, cx),
            expanded,
            cx,
        )
        .on_click(
            cx.listener(move |chat, _, _, cx| chat.toggle_tool_block(tool_call_id.clone(), cx)),
        );

        ui::tool_block(cx)
            .child(header)
            .when(expanded, |container| {
                container.child(render_tool_body(block, cx))
            })
    }

    /// The turn in flight: the partial message, or that pi is thinking.
    ///
    /// The partial is markdown like any other pi message, so a fence that has
    /// opened but not closed formats as it arrives rather than snapping into
    /// shape when the turn settles. It parses on every chunk — bounded by the
    /// one message, and only while a turn is live.
    fn render_streaming(&self, cx: &App) -> Option<Div> {
        match (
            self.state.transcript().streaming(),
            self.state.transcript().is_busy(),
        ) {
            (Some(text), _) => Some(ui::chat_message(
                "pi",
                Tone::Muted,
                ui::markdown("streaming-message", format!("{text}▍")),
                false,
                cx,
            )),
            // Prompt accepted, no partial yet: show that pi is working.
            (None, true) => Some(ui::chat_message("pi", Tone::Muted, "…", false, cx)),
            (None, false) => None,
        }
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
            SessionPhase::Live { .. } => pane.child(self.render_messages(cx)),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}

/// One settled message. The user's own text renders verbatim — it is what they
/// typed, and a `#` or a `*` in it is a character, not a heading or emphasis.
/// Pi's is markdown, which is what pi writes.
fn render_message(index: usize, message: &Message, cx: &App) -> Div {
    match message.role {
        Role::User => ui::chat_message("you", Tone::Active, message.text.clone(), true, cx),
        Role::Assistant => ui::chat_message(
            "pi",
            Tone::Muted,
            ui::markdown(("message", index), message.text.clone()),
            false,
            cx,
        ),
    }
}

/// A turn that ended badly. Authored as "pi" because it is that turn's
/// outcome, but in the failed tone and without markdown — the reason is a
/// provider's error string, not prose, and running it through a renderer that
/// eats `*` and `#` would corrupt exactly the detail being reported.
fn render_failure(detail: &str, cx: &App) -> Div {
    ui::chat_message("pi", Tone::Failed, detail.to_string(), false, cx)
}

/// Cap on the compact-args summary in unknown-tool labels; the full args
/// render in the expanded body.
const ARGS_SUMMARY_MAX_CHARS: usize = 80;

/// The header one-liner per tool (built-ins verified against pi 0.83.0:
/// read/bash/edit/write/grep/find/ls). Bash gets a `$ command` form — pi has
/// no human titles for calls, the command itself is the best label.
fn tool_block_label(block: &ToolBlock, cx: &App) -> Div {
    let label = ui::tool_block_label();

    match block.tool_name.as_str() {
        "bash" => {
            let command = block.arg("command").unwrap_or_default();
            let first_line = command.lines().next().unwrap_or_default().to_string();

            label
                .child(ui::tool_block_verb("$", cx))
                .child(ui::tool_block_subject(first_line.into(), true))
        }
        "read" | "write" | "edit" => {
            let verb = match block.tool_name.as_str() {
                "read" => "Read",
                "write" => "Write",
                _ => "Edit",
            };
            let path = block.arg("path").unwrap_or_default();
            let label = label
                .child(ui::tool_block_verb(verb, cx))
                .child(ui::tool_block_subject(
                    basename(path).to_string().into(),
                    false,
                ));

            match block.line_stats() {
                Some((added, removed)) => label.child(ui::tool_block_stats(added, removed, cx)),
                None => label,
            }
        }
        "grep" | "find" => {
            let verb = if block.tool_name == "grep" {
                "Grep"
            } else {
                "Find"
            };
            let pattern = block.arg("pattern").unwrap_or_default();

            label
                .child(ui::tool_block_verb(verb, cx))
                .child(ui::tool_block_subject(pattern.to_string().into(), true))
        }
        "ls" => {
            let path = block.arg("path").unwrap_or(".");

            label
                .child(ui::tool_block_verb("List", cx))
                .child(ui::tool_block_subject(path.to_string().into(), false))
        }
        _ => label
            .child(ui::tool_block_verb(block.tool_name.clone(), cx))
            .child(ui::tool_block_subject(
                compact_args(&block.args).into(),
                true,
            )),
    }
}

/// The expanded body: context line, then a diff (edit/write) or the output
/// snapshot. Errors always show the output — that is where the message is.
fn render_tool_body(block: &ToolBlock, cx: &App) -> Div {
    let mut body =
        ui::tool_block_body(cx).child(ui::tool_block_context(context_line(block).into(), cx));

    if !matches!(block.status, ToolStatus::Error) {
        // Edit: pi's pre-rendered numbered diff (`details.diff`).
        if let Some(diff) = &block.diff {
            return body.children(diff.lines().map(|line| diff_line(line, cx)));
        }

        // Write: synthesize the all-added diff from the content argument.
        if block.tool_name == "write" {
            if let Some(content) = block.arg("content") {
                return body.children(content.lines().enumerate().map(|(line_index, line)| {
                    ui::tool_block_diff_line(
                        ui::DiffLineKind::Added,
                        format!("+{} {line}", line_index + 1).into(),
                        cx,
                    )
                }));
            }
        }
    }

    if block.output_truncated {
        body = body.child(ui::tool_block_truncation_notice(cx));
    }

    if block.output.is_empty() {
        let placeholder = match block.status {
            ToolStatus::Running => "running…",
            _ => "(no output)",
        };

        return body.child(ui::tool_block_output(placeholder.into(), cx));
    }

    body.child(ui::tool_block_output(block.output.clone().into(), cx))
}

/// The body's faint first line: enough to identify the call in full.
fn context_line(block: &ToolBlock) -> String {
    match block.tool_name.as_str() {
        "read" | "write" | "edit" => block.arg("path").unwrap_or_default().to_string(),
        "bash" => block.arg("command").unwrap_or_default().to_string(),
        _ => block.args.to_string(),
    }
}

/// One line of pi's numbered diff format (" 1 ctx" / "-2 old" / "+2 new").
fn diff_line(line: &str, cx: &App) -> Div {
    let kind = match line.as_bytes().first() {
        Some(b'+') => ui::DiffLineKind::Added,
        Some(b'-') => ui::DiffLineKind::Removed,
        _ => ui::DiffLineKind::Context,
    };

    ui::tool_block_diff_line(kind, line.to_string().into(), cx)
}

fn basename(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Unknown-tool label summary: the raw args JSON, capped (full args live in
/// the expanded body's context line).
fn compact_args(args: &Value) -> String {
    let json = args.to_string();

    if json.chars().count() <= ARGS_SUMMARY_MAX_CHARS {
        return json;
    }

    let truncated: String = json.chars().take(ARGS_SUMMARY_MAX_CHARS).collect();

    format!("{truncated}…")
}
