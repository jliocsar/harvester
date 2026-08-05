//! One attached session's conversation: replayed history plus the live turn.
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use std::collections::HashSet;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, Entity, EventEmitter, Focusable,
    ScrollHandle, SharedString, Subscription, Window,
};
use serde_json::Value;

use crate::input::{PromptSubmitted, TextInput};
use crate::model_control::ModelControl;
use crate::models::{self, ModelChoice};
use crate::theme::{self, Tone};
use crate::ui;
use harvester_client_core::transcript::{Item, Message, Role, ToolBlock, ToolStatus, Transcript};
use harvester_client_core::{Pending, RequestToken, RequestTokens};

pub struct Chat {
    sid: String,
    /// The session's repository (its cwd's folder name) — shown as a
    /// read-only prompt-box control between the model and the machine.
    workspace_label: SharedString,
    /// Display label of the machine this session runs on — shown as the
    /// prompt box's read-only machine control (the machine was picked while
    /// the chat was a ghost; a running session never moves).
    machine_label: SharedString,
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
    /// The session's switchable models (passthrough `get_available_models`,
    /// fetched at open — milliseconds against the running pi) and the modal
    /// over them.
    model: ModelControl,
    /// The session's current model (`get_state` at open, replaced by each
    /// successful `set_model`); drives the control's label and check mark.
    current_model: Option<ModelChoice>,
    /// The three answers this chat can be waiting on. An answer carrying any
    /// other token belongs to a request already superseded (two `set_model`s
    /// from a fast double-pick are the live case) and is dropped.
    session_state: Pending<()>,
    set_model: Pending<()>,
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<TextInput>,
    scroll: ScrollHandle,
    /// Which machine's bridge serves this session — the workbench routes by
    /// it. Deliberately the key and not a command sender: `Fleet` replaces a
    /// machine's bridge wholesale on reconnect, so a sender cloned at attach
    /// time is dead the moment the machine comes back, and every prompt sent
    /// through it was swallowed with one line on stderr (found in review). A
    /// key cannot go stale.
    machine: String,
    tokens: RequestTokens,
    _input_subscription: Subscription,
}

/// Which session a chat renders, as opposed to what it renders: identity plus
/// the two labels the prompt box shows read-only. Grouped because they are
/// looked up together at attach time and never change afterwards — a running
/// session does not move machines or repositories.
pub struct ChatSession {
    pub sid: String,
    pub machine: String,
    pub workspace_label: SharedString,
    pub machine_label: SharedString,
}

/// What a chat asks of its machine's bridge. The chat emits; the workbench
/// looks up the live bridge and sends — see [`Chat::machine`] for why the chat
/// may not hold the sender itself. Intents rather than `bridge::Command`s
/// because the workbench is the layer that knows a request's routing (and
/// because a `Command` is not `Clone`, so an event cannot carry one).
pub enum ChatRequest {
    Prompt(String),
    SetModel {
        provider: String,
        model_id: String,
        token: RequestToken,
    },
    /// The prompt box's model control, filled at open: the session's model
    /// list and its current model. Both queue on pi's in-order stdin.
    ModelControls {
        models: RequestToken,
        state: RequestToken,
    },
}

impl EventEmitter<ChatRequest> for Chat {}

impl Chat {
    /// `first_prompt` seeds a ghost-chat start: the bridge already sent that
    /// prompt as part of the create → attach chain, so the conversation opens
    /// with it rendered and the turn marked busy.
    pub fn new(
        session: ChatSession,
        entries: &[Value],
        first_prompt: Option<String>,
        tokens: RequestTokens,
        cx: &mut Context<Self>,
    ) -> Self {
        let ChatSession {
            sid,
            machine,
            workspace_label,
            machine_label,
        } = session;
        let input = cx.new(|cx| TextInput::new("Message pi — Enter to send", cx));
        let input_subscription = cx.subscribe(&input, Self::handle_prompt_submitted);
        let mut transcript = Transcript::new(entries);

        // The bridge already sent this prompt as part of the create → attach
        // chain, so the conversation opens with it rendered and busy.
        if let Some(text) = first_prompt {
            transcript.push_user_prompt(text);
        }

        Self {
            sid,
            workspace_label,
            machine_label,
            transcript,
            expanded: HashSet::new(),
            exited_notice: None,
            model: ModelControl::default(),
            current_model: None,
            session_state: Pending::default(),
            set_model: Pending::default(),
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            machine,
            tokens,
            _input_subscription: input_subscription,
        }
    }

    pub fn sid(&self) -> &str {
        &self.sid
    }

    pub fn machine(&self) -> &str {
        &self.machine
    }

    /// Ask for what the prompt box's model control needs. Separate from
    /// `new` because an entity cannot emit to a subscriber that does not exist
    /// yet: the workbench subscribes to the chat it just created, then calls
    /// this.
    pub fn request_model_controls(&mut self, cx: &mut Context<Self>) {
        cx.emit(ChatRequest::ModelControls {
            models: self.model.fetching(&self.tokens),
            state: self.session_state.ask(&self.tokens, ()),
        });
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

    /// A `ListSessionModels` answered. On failure the control just stays
    /// disabled — the session keeps running on its current model.
    pub fn handle_session_models(
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
            Err(detail) => eprintln!("harvester: get_state ({}): {detail}", self.sid),
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

    pub fn open_model_picker(&mut self, cx: &mut Context<Self>) {
        self.model.open();
        cx.notify();
    }

    pub fn close_model_picker(&mut self, cx: &mut Context<Self>) {
        self.model.close();
        cx.notify();
    }

    /// The fetch this chat asked for will never be answered.
    pub fn fail_model_list(&mut self, detail: String, cx: &mut Context<Self>) {
        self.model.fail(detail);
        cx.notify();
    }

    /// A picker row was clicked: fire `set_model` on the running session.
    /// No optimism — the label changes when `ModelSet` confirms.
    pub fn pick_model(&mut self, index: usize, cx: &mut Context<Self>) {
        self.model.close();

        let Some(choice) = self.model.choices().get(index) else {
            return;
        };
        // A second pick supersedes the first: only the answer to this token
        // may move the label, so a slow reply for the abandoned model cannot
        // land on top of a newer one.
        let request = ChatRequest::SetModel {
            provider: choice.provider.clone(),
            model_id: choice.id.clone(),
            token: self.set_model.ask(&self.tokens, ()),
        };

        cx.emit(request);
        cx.notify();
    }

    /// What the workbench's shared picker modal renders when this chat's
    /// picker is open: the loaded list and the current model's index.
    pub fn picker_contents(&self) -> Option<(Vec<ModelChoice>, Option<usize>)> {
        if !self.model.is_open() {
            return None;
        }

        let model_choices = self.model.choices();
        let current_index = self.current_model.as_ref().and_then(|current| {
            model_choices
                .iter()
                .position(|choice| choice.provider == current.provider && choice.id == current.id)
        });

        Some((model_choices.to_vec(), current_index))
    }

    fn handle_prompt_submitted(
        &mut self,
        _input: Entity<TextInput>,
        event: &PromptSubmitted,
        cx: &mut Context<Self>,
    ) {
        if self.exited_notice.is_some() {
            return;
        }

        self.transcript.push_user_prompt(event.0.clone());
        cx.emit(ChatRequest::Prompt(event.0.clone()));
        self.scroll.scroll_to_bottom();
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

    /// A tool block's header was clicked: toggle its body.
    fn toggle_tool_block(&mut self, tool_call_id: String, cx: &mut Context<Self>) {
        if !self.expanded.remove(&tool_call_id) {
            self.expanded.insert(tool_call_id);
        }

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

    /// The send button's path; Enter inside the input reaches the same
    /// `PromptSubmitted` through the `Submit` action.
    fn submit_prompt(&mut self, cx: &mut Context<Self>) {
        self.input
            .clone()
            .update(cx, |input, cx| input.submit_prompt(cx));
    }

    fn render_input_row(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The prompt box floats in the pane's padding (no separator line —
        // the card's own border is the boundary).
        let row = div().flex_none().px_3().pb_3();

        if let Some(notice) = &self.exited_notice {
            return row.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child(notice.clone()),
            );
        }

        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);
        // Label = the session's current model once `get_state` answered;
        // clickable once the switchable list is loaded.
        let model_label: SharedString = match &self.current_model {
            Some(choice) => choice.name.clone(),
            None => "model".into(),
        };
        let model_enabled = !self.model.choices().is_empty();

        row.child(
            ui::prompt_box(input_focused)
                .child(ui::prompt_box_input(self.input.clone()))
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
                        // Read-only pair: the repository is the session's
                        // cwd, the machine was picked at ghost time; a
                        // running session never moves.
                        .child(ui::prompt_box_control(
                            "chat-repo",
                            self.workspace_label.clone(),
                            false,
                        ))
                        .child(ui::prompt_box_control(
                            "chat-machine",
                            self.machine_label.clone(),
                            false,
                        ))
                        .child(div().flex_1())
                        .child(
                            ui::prompt_box_send("chat-send")
                                .on_click(cx.listener(|chat, _, _, cx| chat.submit_prompt(cx))),
                        ),
                ),
        )
    }
}

impl Render for Chat {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_input && self.exited_notice.is_none() {
            window.focus(&self.input.read(cx).focus_handle(cx));
            self.focus_input = false;
        }

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(
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
                    ),
            )
            .child(self.render_input_row(window, cx))
    }
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
