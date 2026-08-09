//! One session's conversation, from before it exists until after its pi exits.
//!
//! A chat opens **unstarted** — client-side only, no session on any machine
//! (docs/SPEC.md §Flows "Start session") — and its first prompt mints the sid
//! that makes it live. These are one component with a state rather than two
//! components with a shared vocabulary, because everything the user touches is
//! the same on both sides of that line: the same pane, the same prompt box,
//! the same model control, the same input. What actually differs is small and
//! named in [`ChatState`]: what a model pick *means* (a launch-line argument
//! versus a `set_model` the running pi must confirm), and whether the machine
//! can still be changed.
//!
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use std::collections::HashSet;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, Entity, EventEmitter, Focusable,
    ScrollHandle, SharedString, Subscription, Window,
};
use gpui_component::input::{Input, InputEvent, InputState};
use serde_json::Value;

use crate::model_control::ModelControl;
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme::{self, Tone};
use crate::ui;
use crate::workspaces::folder_name;
use harvester_client_core::transcript::{Item, Message, Role, ToolBlock, ToolStatus, Transcript};
use harvester_client_core::{Pending, RequestToken, RequestTokens};

/// Cap on the session name derived from the first prompt's first line — enough
/// to recognize, short enough for pi's session list and the sidebar.
const SESSION_NAME_MAX_CHARS: usize = 48;

/// One (machine key, display label) a chat could run on.
pub type MachineCandidate = (String, String);

/// How far along the path from "a typed prompt" to "a pi process" this chat is.
///
/// The states are ordered and one-way: a chat never goes back to unstarted,
/// because the sid it minted names a session file that now exists.
enum ChatState {
    /// No session anywhere yet. The machine is still switchable and a model
    /// pick is just an argument for a launch line that has not been written.
    Unstarted,
    /// The create → attach → prompt chain is out and its sid is minted, but no
    /// pi has answered yet. Input is closed here on purpose: a second prompt
    /// would mint a second sid and start a second pi for one turn.
    Starting {
        sid: String,
        /// Pi's `--name`, held for the sidebar row that `Attached` synthesizes.
        name: String,
    },
    /// Attached: `sid` names a pi the daemon has spawned. Whether that pi is
    /// still alive is [`Chat::exited_notice`]'s question, not this one's.
    Live { sid: String },
}

impl ChatState {
    /// The session id, once one exists. `None` is the unstarted chat — no pi,
    /// no session file, nothing on any machine to name.
    fn sid(&self) -> Option<&str> {
        match self {
            ChatState::Unstarted => None,
            ChatState::Starting { sid, .. } | ChatState::Live { sid } => Some(sid),
        }
    }
}

/// Where a chat runs. The machine is switchable only while unstarted — a
/// running session never moves — so this is near-immutable by construction.
pub struct ChatTarget {
    /// Machine key; the workbench routes every request by it. Deliberately the
    /// key and not a command sender: `Fleet` replaces a machine's bridge
    /// wholesale on reconnect, so a sender cloned at attach time is dead the
    /// moment the machine comes back, and every prompt sent through it was
    /// swallowed with one line on stderr (found in review). A key cannot go
    /// stale.
    pub machine: String,
    pub machine_label: SharedString,
    /// The session's cwd — pi's working directory, and the repository the
    /// prompt box names.
    pub workspace_path: String,
}

/// What a chat asks of its machine's bridge. The chat emits; the workbench
/// looks up the live bridge and sends — see [`ChatTarget::machine`] for why the
/// chat may not hold the sender itself. Intents rather than `bridge::Command`s
/// because the workbench is the layer that knows a request's routing (and
/// because a `Command` is not `Clone`, so an event cannot carry one).
///
/// The sid rides along on the variants that have one, so the workbench never
/// has to ask a chat for a session id that its state says may not exist.
pub enum ChatRequest {
    /// Unstarted only: the models pi would offer in this chat's cwd, from a
    /// throwaway pi (the running session's list is `ModelControls`).
    ListModels {
        cwd: String,
        token: RequestToken,
    },
    /// Unstarted → starting: the create → attach → prompt chain, as the launch
    /// line's arguments rather than a built `Command` — the chat owns what pi
    /// is asked for, the workbench owns which bridge is asked.
    Start {
        sid: String,
        cwd: String,
        provider: Option<String>,
        model: Option<String>,
        name: String,
        first_prompt: String,
    },
    Prompt {
        sid: String,
        text: String,
    },
    SetModel {
        sid: String,
        provider: String,
        model_id: String,
        token: RequestToken,
    },
    /// The prompt box's model control, filled at open: the session's model
    /// list and its current model. Both queue on pi's in-order stdin.
    ModelControls {
        sid: String,
        models: RequestToken,
        state: RequestToken,
    },
}

impl ChatRequest {
    /// Whether this request is the unstarted chat's. The two halves reach a
    /// machine differently — an unstarted chat connects lazily and lets its
    /// commands queue behind the bootstrap, while a live one refuses to send
    /// into a bridge that is not up — and this is the one place that says so.
    pub fn is_pre_session(&self) -> bool {
        matches!(
            self,
            ChatRequest::ListModels { .. } | ChatRequest::Start { .. }
        )
    }
}

impl EventEmitter<ChatRequest> for Chat {}

pub struct Chat {
    state: ChatState,
    target: ChatTarget,
    /// The machines this chat could still be retargeted to, as the workbench
    /// resolved them: the stored workspaces sharing this cwd. Pushed in rather
    /// than derived because the workspace list and the machine labels are both
    /// the workbench's; refreshed whenever that list changes. Only ever read
    /// while unstarted.
    machine_candidates: Vec<MachineCandidate>,
    machine_picker_open: bool,
    /// What is rendered, and the pi vocabulary behind it — client-core's, so
    /// the wire shapes are testable without a window.
    transcript: Transcript,
    /// Tool blocks the user has opened, by `toolCallId`. Not by position: a
    /// resync replaces every item, and expansion keyed by index would land on
    /// whatever block took that slot.
    expanded: HashSet<String>,
    /// Set when the pi process exited; shown where the input was. Prompting
    /// again requires a resume (re-open from the sidebar — resume is
    /// `create_session` with the same sid).
    exited_notice: Option<String>,
    /// The models this chat can pick from and the modal over them, however
    /// they were loaded (throwaway `list_models` unstarted, passthrough
    /// `get_available_models` live).
    model: ModelControl,
    /// The model the prompt box names. Unstarted it is the launch line's pick,
    /// where `None` means "let pi choose"; live it is the session's current
    /// model (`get_state` at open, replaced by each confirmed `set_model`),
    /// where `None` merely means the answer has not arrived yet.
    current_model: Option<ModelChoice>,
    /// The two answers a live chat can be waiting on. An answer carrying any
    /// other token belongs to a request already superseded (two `set_model`s
    /// from a fast double-pick are the live case) and is dropped.
    session_state: Pending<()>,
    set_model: Pending<()>,
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<InputState>,
    scroll: ScrollHandle,
    tokens: RequestTokens,
    _input_subscription: Subscription,
}

impl Chat {
    /// A chat with no session behind it yet, already asking for the models on
    /// the machine behind it — the seconds the uncached `list_models` costs
    /// hide behind the user typing.
    pub fn unstarted(
        target: ChatTarget,
        machine_candidates: Vec<MachineCandidate>,
        tokens: RequestTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chat = Self::new(ChatState::Unstarted, target, tokens, window, cx);

        chat.machine_candidates = machine_candidates;
        chat.request_model_list(cx);

        chat
    }

    /// A chat over a session that already exists: `entries` is pi's history,
    /// verbatim (empty for a session that has never been prompted).
    pub fn live(
        sid: String,
        target: ChatTarget,
        entries: &[Value],
        tokens: RequestTokens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut chat = Self::new(ChatState::Live { sid }, target, tokens, window, cx);

        chat.transcript.replace_history(entries);

        chat
    }

    fn new(
        state: ChatState,
        target: ChatTarget,
        tokens: RequestTokens,
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
        let input_subscription = cx.subscribe_in(&input, window, Self::handle_prompt_submitted);

        Self {
            state,
            target,
            machine_candidates: Vec::new(),
            machine_picker_open: false,
            transcript: Transcript::new(&[]),
            expanded: HashSet::new(),
            exited_notice: None,
            model: ModelControl::default(),
            current_model: None,
            session_state: Pending::default(),
            set_model: Pending::default(),
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            tokens,
            _input_subscription: input_subscription,
        }
    }

    /// `None` for an unstarted chat — it names no session (see [`ChatState`]).
    pub fn sid(&self) -> Option<&str> {
        self.state.sid()
    }

    pub fn machine(&self) -> &str {
        &self.target.machine
    }

    /// The sid this chat is waiting on `Attached` for, and the name it minted
    /// for it — what the workbench needs to synthesize the sidebar row.
    pub fn starting_session(&self) -> Option<(&str, &str)> {
        match &self.state {
            ChatState::Starting { sid, name } => Some((sid, name)),
            _ => None,
        }
    }

    pub fn workspace_path(&self) -> &str {
        &self.target.workspace_path
    }

    /// Machines the user may retarget an unstarted chat to. Refreshed by the
    /// workbench whenever the workspace list changes; ignored once started,
    /// because a running session never moves.
    pub fn set_machine_candidates(&mut self, candidates: Vec<MachineCandidate>) {
        self.machine_candidates = candidates;
    }

    /// Ask for what a live chat's prompt box needs. Separate from the
    /// constructor because an entity cannot emit to a subscriber that does not
    /// exist yet: the workbench subscribes to the chat it just created, then
    /// calls this.
    pub fn request_model_controls(&mut self, cx: &mut Context<Self>) {
        let Some(sid) = self.state.sid().map(str::to_string) else {
            return;
        };

        cx.emit(ChatRequest::ModelControls {
            sid,
            models: self.model.fetching(&self.tokens),
            state: self.session_state.ask(&self.tokens, ()),
        });
    }

    /// The unstarted chat's model list: a throwaway pi in this chat's cwd.
    fn request_model_list(&mut self, cx: &mut Context<Self>) {
        cx.emit(ChatRequest::ListModels {
            cwd: self.target.workspace_path.clone(),
            token: self.model.fetching(&self.tokens),
        });
    }

    /// The create → attach → prompt chain landed: this chat is a live session
    /// now. An empty snapshot is the brand-new-session case (docs/SPEC.md C7),
    /// where the prompt already on screen is the only history there is — so it
    /// stays rather than being blanked out from under the user.
    pub fn promote_to_live(&mut self, sid: String, entries: &[Value], cx: &mut Context<Self>) {
        if !entries.is_empty() {
            self.transcript.replace_history(entries);
        }

        self.state = ChatState::Live { sid };
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// A resync (`event_gap`, or a reconnect's refetch) answered: the entries
    /// are authoritative, so they replace what is rendered. Deliberately not a
    /// new chat entity — rebuilding is what let a late resync clobber the chat
    /// on screen, discarding the input's contents and its focus with it. The
    /// in-flight partial message goes because the snapshot supersedes it; the
    /// next `message_update` carries the whole partial again.
    pub fn replace_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        self.transcript.replace_history(entries);

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// The same repair, cheap: only the entries after the last fetch's cursor
    /// (docs/SPEC.md C6). The transcript owns what that means for the items it
    /// built from live events in the meantime.
    pub fn extend_history(&mut self, entries: &[Value], cx: &mut Context<Self>) {
        self.transcript.extend_history(entries);

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// The start failed. Back to editable — a retry mints a fresh sid.
    pub fn abandon_start(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ChatState::Starting { .. }) {
            self.state = ChatState::Unstarted;
            cx.notify();
        }
    }

    /// The machine's bridge died: nothing this chat asked for will arrive.
    pub fn fail_pending_requests(&mut self, detail: String, cx: &mut Context<Self>) {
        self.abandon_start(cx);
        self.model.fail(detail);
        cx.notify();
    }

    /// A model list answered, from either source. A chat that has since been
    /// retargeted is waiting on a different token, so the abandoned request's
    /// answer is dropped.
    pub fn handle_models_listed(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.model.accept(token, result) {
            cx.notify();
        }
    }

    /// A `GetSessionState` answered; the current model labels the control.
    /// Failure is cosmetic (generic label), so it is only logged.
    pub fn handle_session_state(
        &mut self,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if self.session_state.accept(token).is_none() {
            return;
        }

        match result {
            Ok(data) => {
                self.current_model = data.get("model").and_then(models::model_choice_from_value);
                cx.notify();
            }
            Err(detail) => eprintln!(
                "harvester: get_state ({}): {detail}",
                self.state.sid().unwrap_or("?")
            ),
        }
    }

    /// A `set_model` succeeded; pi's `data` is the full Model object now in
    /// effect (failures surface in the statusbar, workbench-side).
    pub fn handle_model_set(&mut self, token: RequestToken, data: &Value, cx: &mut Context<Self>) {
        if self.set_model.accept(token).is_none() {
            return;
        }

        if let Some(choice) = models::model_choice_from_value(data) {
            self.current_model = Some(choice);
            cx.notify();
        }
    }

    /// A started chat's model is already on its launch line or in its pi;
    /// only an unstarted one is still choosing.
    pub fn open_model_picker(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.state, ChatState::Starting { .. }) {
            self.model.open();
            cx.notify();
        }
    }

    pub fn close_model_picker(&mut self, cx: &mut Context<Self>) {
        self.model.close();
        cx.notify();
    }

    /// What the workbench's shared model modal renders for this chat: the
    /// loaded list, the current model's index, and whether to offer the
    /// "default (pi chooses)" row — which only an unstarted chat may, since a
    /// running session always has a concrete model (SPEC §Flows "Switch
    /// model"). The flag is derived from the state, not stored: a stored
    /// answer would be one more thing that could disagree with it.
    pub fn picker_contents(&self) -> Option<(Vec<ModelChoice>, Option<usize>, bool)> {
        if !self.model.is_open() {
            return None;
        }

        let choices = self.model.choices();
        let current_index = self.current_model.as_ref().and_then(|current| {
            choices
                .iter()
                .position(|choice| choice.provider == current.provider && choice.id == current.id)
        });
        let offer_default = matches!(self.state, ChatState::Unstarted);

        Some((choices.to_vec(), current_index, offer_default))
    }

    /// A picker row was clicked; `None` is the "default (pi chooses)" row.
    /// Unstarted the pick is local (it becomes a launch-line argument); live it
    /// costs a `set_model` the session must confirm, so the label does not move
    /// until [`Chat::handle_model_set`] says pi agreed.
    pub fn pick_model(&mut self, choice: Option<usize>, cx: &mut Context<Self>) {
        self.model.close();

        let picked = choice.and_then(|index| self.model.choices().get(index).cloned());

        match &self.state {
            ChatState::Unstarted => self.current_model = picked,
            ChatState::Live { sid } => {
                if let Some(choice) = picked {
                    // A second pick supersedes the first: only the answer to
                    // this token may move the label, so a slow reply for the
                    // abandoned model cannot land on top of a newer one.
                    let request = ChatRequest::SetModel {
                        sid: sid.clone(),
                        provider: choice.provider,
                        model_id: choice.id,
                        token: self.set_model.ask(&self.tokens, ()),
                    };

                    cx.emit(request);
                }
            }
            // The launch line is already out; its model is what this session has.
            ChatState::Starting { .. } => {}
        }

        cx.notify();
    }

    pub fn machine_picker_open(&self) -> bool {
        self.machine_picker_open
    }

    /// Offered only while unstarted and at least two stored workspaces share
    /// this path — below that the control reads as the fixed destination.
    pub fn open_machine_picker(&mut self, cx: &mut Context<Self>) {
        if matches!(self.state, ChatState::Unstarted) && self.can_switch_machine() {
            self.machine_picker_open = true;
            cx.notify();
        }
    }

    pub fn close_machine_picker(&mut self, cx: &mut Context<Self>) {
        self.machine_picker_open = false;
        cx.notify();
    }

    fn can_switch_machine(&self) -> bool {
        self.machine_candidates.len() >= 2
    }

    /// Retarget to another machine: the typed prompt survives, only the
    /// destination changes. A pick of the current machine changes nothing, so
    /// nothing is refetched. The stale answer from the old machine is dropped
    /// by the model control's own token.
    pub fn pick_machine(&mut self, candidate_index: usize, cx: &mut Context<Self>) {
        self.machine_picker_open = false;

        let Some((machine, label)) = self.machine_candidates.get(candidate_index).cloned() else {
            cx.notify();

            return;
        };

        if self.target.machine == machine {
            cx.notify();

            return;
        }

        self.target.machine = machine;
        self.target.machine_label = label.into();
        self.current_model = None;
        self.request_model_list(cx);
        cx.notify();
    }

    /// Whether this chat's pi is still believed to be running. Based on the
    /// `session_exit` events this chat actually received, so unlike a cached
    /// list status it cannot claim "running" about a session that has died.
    pub fn is_live(&self) -> bool {
        self.exited_notice.is_none()
    }

    pub fn handle_session_exit(&mut self, exit_code: Option<i32>, cx: &mut Context<Self>) {
        let code_text = exit_code
            .map(|code| format!(" (exit code {code})"))
            .unwrap_or_default();

        self.transcript.end_turn();
        self.exited_notice = Some(format!(
            "session exited{code_text} — click it in the sidebar to resume"
        ));
        cx.notify();
    }

    pub fn handle_pi_event(&mut self, event: &Value, cx: &mut Context<Self>) {
        // `false` is an event this model does not cover (retries, compaction,
        // queue): nothing moved, so nothing is redrawn.
        if !self.transcript.apply(event) {
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
        if self.exited_notice.is_some() {
            return;
        }

        match &self.state {
            ChatState::Unstarted => self.start(text, cx),
            ChatState::Live { sid } => {
                let request = ChatRequest::Prompt {
                    sid: sid.clone(),
                    text: text.clone(),
                };

                self.transcript.push_user_prompt(text);
                cx.emit(request);
                self.scroll.scroll_to_bottom();
                cx.notify();
            }
            // The input is not rendered while starting; nothing to accept.
            ChatState::Starting { .. } => {}
        }
    }

    /// The first prompt: mint the sid, derive the name, resolve the launch
    /// line's model, and hand the chain to the workbench.
    ///
    /// The sid is a **uuidv7 minted here** because that is pi's own session-id
    /// format and the client is the one that mints it (docs/DAEMON.md §the pi
    /// launch line). It sits in a gpui module only because that is where the
    /// prompt is; nothing about it is presentation.
    fn start(&mut self, prompt: String, cx: &mut Context<Self>) {
        if prompt.trim().is_empty() {
            return;
        }

        let sid = uuid::Uuid::now_v7().to_string();
        let name = session_name_from_prompt(&prompt);
        let (provider, model) = match &self.current_model {
            Some(choice) => (Some(choice.provider.clone()), Some(choice.id.clone())),
            // Nothing picked (or no list ever arrived): pi's own default.
            None => (None, None),
        };
        let request = ChatRequest::Start {
            sid: sid.clone(),
            cwd: self.target.workspace_path.clone(),
            provider,
            model,
            name: name.clone(),
            first_prompt: prompt.clone(),
        };

        // Rendered immediately: the bridge is sending this prompt as part of
        // the chain, so the turn is already under way and the pane should say
        // so rather than going blank until `Attached` lands.
        self.transcript.push_user_prompt(prompt);
        self.state = ChatState::Starting { sid, name };
        cx.emit(request);
        self.scroll.scroll_to_bottom();
        cx.notify();
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
    fn unstarted_subtitle(&self) -> SharedString {
        format!(
            "{} on {}",
            folder_name(&self.target.workspace_path),
            self.target.machine_label
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
                        self.transcript
                            .items()
                            .iter()
                            .enumerate()
                            .map(|(index, item)| self.render_item(index, item, cx)),
                    )
                    .children(self.render_streaming()),
            )
    }

    /// One conversation item. The index is the element id only — what a click
    /// toggles is the block's `toolCallId`, which survives a resync.
    fn render_item(&self, index: usize, item: &Item, cx: &mut Context<Self>) -> AnyElement {
        match item {
            Item::Message(message) => render_message(message).into_any_element(),
            Item::Tool(block) => self.render_tool_block(index, block, cx).into_any_element(),
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
            tool_block_label(block),
            expanded,
        )
        .on_click(
            cx.listener(move |chat, _, _, cx| chat.toggle_tool_block(tool_call_id.clone(), cx)),
        );

        ui::tool_block().child(header).when(expanded, |container| {
            container.child(render_tool_body(block))
        })
    }

    fn render_streaming(&self) -> Option<impl IntoElement> {
        let text = match (self.transcript.streaming(), self.transcript.is_busy()) {
            (Some(text), _) => format!("{text}▍"),
            // Prompt accepted, no partial yet: show that pi is working.
            (None, true) => "…".to_string(),
            (None, false) => return None,
        };

        Some(ui::chat_message("pi", Tone::Muted, text, false))
    }

    /// The pane's bottom row: the prompt box, or the one sentence that
    /// replaces it when this chat cannot accept input.
    fn render_bottom_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The prompt box floats in the pane's padding (no separator line —
        // the card's own border is the boundary).
        let row = div().flex_none().px_3().pb_3();
        let notice = match (&self.exited_notice, &self.state) {
            (Some(notice), _) => Some(notice.clone()),
            (None, ChatState::Starting { .. }) => Some("starting session…".to_string()),
            (None, _) => None,
        };

        if let Some(notice) = notice {
            return row.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_FAINT))
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
            matches!(self.state, ChatState::Unstarted) && self.can_switch_machine();

        ui::prompt_box(input_focused)
            .child(ui::prompt_box_input(Input::new(&self.input)))
            .child(
                ui::prompt_box_controls()
                    .child(
                        ui::prompt_box_control("chat-model", model_label, model_enabled).when(
                            model_enabled,
                            |control| {
                                control.on_click(
                                    cx.listener(|chat, _, _, cx| chat.open_model_picker(cx)),
                                )
                            },
                        ),
                    )
                    .child(ui::prompt_box_control(
                        "chat-repo",
                        folder_name(&self.target.workspace_path).to_string().into(),
                        false,
                    ))
                    .child(
                        ui::prompt_box_control(
                            "chat-machine",
                            self.target.machine_label.clone(),
                            machine_enabled,
                        )
                        .when(machine_enabled, |control| {
                            control.on_click(
                                cx.listener(|chat, _, _, cx| chat.open_machine_picker(cx)),
                            )
                        }),
                    )
                    .child(div().flex_1())
                    .child(ui::prompt_box_send("chat-send").on_click(
                        cx.listener(|chat, _, window, cx| chat.submit_prompt(window, cx)),
                    )),
            )
    }

    /// What the model control says and whether it can be clicked. Unstarted, a
    /// list that never arrived is not an error the user must clear — pi still
    /// launches with its default — so the control just reads that way.
    fn model_control_state(&self) -> (SharedString, bool) {
        if let Some(choice) = &self.current_model {
            return (choice.name.clone(), !self.model.choices().is_empty());
        }

        match (self.model.state(), &self.state) {
            (ModelsState::Loading, ChatState::Unstarted) => ("loading models…".into(), false),
            (ModelsState::Failed(_), ChatState::Unstarted) => ("default model".into(), false),
            (ModelsState::Loaded(_), ChatState::Unstarted) => ("default model".into(), true),
            // Live, before `get_state` answers: a generic label rather than a
            // claim about which model the session is on.
            _ => ("model".into(), !self.model.choices().is_empty()),
        }
    }

    /// The unstarted body: empty, except when the model fetch failed — which
    /// is informational, not blocking, since pi still launches with a default.
    fn render_unstarted_body(&self) -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(0.))
            .p_3()
            .children(match self.model.state() {
                ModelsState::Failed(detail) => Some(
                    div()
                        .text_xs()
                        .text_color(rgb(theme::STATUS_ERROR))
                        .child(format!("{detail} — pi will use its default model")),
                ),
                _ => None,
            })
    }

    /// The machine modal: the workspaces sharing this chat's path, the current
    /// destination checked.
    pub fn render_machine_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<_> = self
            .machine_candidates
            .iter()
            .enumerate()
            .map(|(index, (machine, label))| {
                ui::model_row(
                    ("chat-machine-choice", index),
                    label.clone().into(),
                    machine == &self.target.machine,
                )
                .on_click(cx.listener(move |chat, _, _, cx| chat.pick_machine(index, cx)))
                .into_any_element()
            })
            .collect();

        ui::modal_list(
            "chat-machine-picker",
            "Choose machine",
            ui::ModalListHeight::FitContent,
            rows,
        )
        .on_click(cx.listener(|chat, _, _, cx| chat.close_machine_picker(cx)))
    }
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let accepts_input =
            self.exited_notice.is_none() && !matches!(self.state, ChatState::Starting { .. });

        if self.focus_input && accepts_input {
            window.focus(&self.input.read(cx).focus_handle(cx), cx);
            self.focus_input = false;
        }

        let pane = div().size_full().flex().flex_col();
        let pane = match self.state {
            // No conversation yet, so the header is what identifies the pane.
            ChatState::Unstarted | ChatState::Starting { .. } => pane
                .child(ui::chat_header(
                    "New session".into(),
                    self.unstarted_subtitle(),
                ))
                .child(self.render_unstarted_body()),
            ChatState::Live { .. } => pane.child(self.render_messages(cx)),
        };

        pane.child(self.render_bottom_row(window, cx))
    }
}

/// Pi's `--name` for the session, from the prompt that started it: its first
/// non-blank line, capped. Presentation, not protocol — pi accepts any string.
fn session_name_from_prompt(prompt: &str) -> String {
    prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(SESSION_NAME_MAX_CHARS)
        .collect()
}

fn render_message(message: &Message) -> impl IntoElement {
    let (label, tone) = match message.role {
        Role::User => ("you", Tone::Active),
        Role::Assistant => ("pi", Tone::Muted),
    };

    ui::chat_message(
        label,
        tone,
        message.text.clone(),
        message.role == Role::User,
    )
}

/// Cap on the compact-args summary in unknown-tool labels; the full args
/// render in the expanded body.
const ARGS_SUMMARY_MAX_CHARS: usize = 80;

/// The header one-liner per tool (built-ins verified against pi 0.83.0:
/// read/bash/edit/write/grep/find/ls). Bash gets a `$ command` form — pi has
/// no human titles for calls, the command itself is the best label.
fn tool_block_label(block: &ToolBlock) -> Div {
    let label = ui::tool_block_label();

    match block.tool_name.as_str() {
        "bash" => {
            let command = block.arg("command").unwrap_or_default();
            let first_line = command.lines().next().unwrap_or_default().to_string();

            label
                .child(ui::tool_block_verb("$"))
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
                .child(ui::tool_block_verb(verb))
                .child(ui::tool_block_subject(
                    basename(path).to_string().into(),
                    false,
                ));

            match block.line_stats() {
                Some((added, removed)) => label.child(ui::tool_block_stats(added, removed)),
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
                .child(ui::tool_block_verb(verb))
                .child(ui::tool_block_subject(pattern.to_string().into(), true))
        }
        "ls" => {
            let path = block.arg("path").unwrap_or(".");

            label
                .child(ui::tool_block_verb("List"))
                .child(ui::tool_block_subject(path.to_string().into(), false))
        }
        _ => label
            .child(ui::tool_block_verb(block.tool_name.clone()))
            .child(ui::tool_block_subject(
                compact_args(&block.args).into(),
                true,
            )),
    }
}

/// The expanded body: context line, then a diff (edit/write) or the output
/// snapshot. Errors always show the output — that is where the message is.
fn render_tool_body(block: &ToolBlock) -> Div {
    let mut body = ui::tool_block_body().child(ui::tool_block_context(context_line(block).into()));

    if !matches!(block.status, ToolStatus::Error) {
        // Edit: pi's pre-rendered numbered diff (`details.diff`).
        if let Some(diff) = &block.diff {
            return body.children(diff.lines().map(diff_line));
        }

        // Write: synthesize the all-added diff from the content argument.
        if block.tool_name == "write" {
            if let Some(content) = block.arg("content") {
                return body.children(content.lines().enumerate().map(|(line_index, line)| {
                    ui::tool_block_diff_line(
                        ui::DiffLineKind::Added,
                        format!("+{} {line}", line_index + 1).into(),
                    )
                }));
            }
        }
    }

    if block.output_truncated {
        body = body.child(ui::tool_block_truncation_notice());
    }

    if block.output.is_empty() {
        let placeholder = match block.status {
            ToolStatus::Running => "running…",
            _ => "(no output)",
        };

        return body.child(ui::tool_block_output(placeholder.into()));
    }

    body.child(ui::tool_block_output(block.output.clone().into()))
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
fn diff_line(line: &str) -> Div {
    let kind = match line.as_bytes().first() {
        Some(b'+') => ui::DiffLineKind::Added,
        Some(b'-') => ui::DiffLineKind::Removed,
        _ => ui::DiffLineKind::Context,
    };

    ui::tool_block_diff_line(kind, line.to_string().into())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_prompt_is_its_own_session_name() {
        assert_eq!(session_name_from_prompt("fix the tests"), "fix the tests");
    }

    #[test]
    fn only_the_first_line_names_the_session() {
        assert_eq!(
            session_name_from_prompt("fix the tests\nand also the docs"),
            "fix the tests"
        );
    }

    #[test]
    fn a_long_first_line_is_capped() {
        let name = session_name_from_prompt(&"x".repeat(SESSION_NAME_MAX_CHARS + 10));

        assert_eq!(name.chars().count(), SESSION_NAME_MAX_CHARS);
    }

    /// Counted in characters, not bytes — `take` on chars, so a multi-byte
    /// line is never cut mid-character and never capped early.
    #[test]
    fn the_cap_counts_characters_not_bytes() {
        let name = session_name_from_prompt(&"é".repeat(SESSION_NAME_MAX_CHARS + 5));

        assert_eq!(name.chars().count(), SESSION_NAME_MAX_CHARS);
    }

    #[test]
    fn a_leading_blank_line_is_not_the_name() {
        assert_eq!(session_name_from_prompt("\n\n  fix it  "), "fix it");
    }
}
