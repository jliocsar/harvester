//! One attached session's conversation: replayed history plus the live turn.
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use std::collections::HashMap;

use gpui::{
    div, prelude::*, px, rgb, AnyElement, Context, Div, Entity, EventEmitter, Focusable,
    ScrollHandle, SharedString, Subscription, Window,
};
use serde_json::Value;

use crate::input::{PromptSubmitted, TextInput};
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme;
use crate::ui;
use harvester_client_core::{Pending, RequestToken, RequestTokens};

/// Who said a rendered message. Tool results render as `ToolBlock`s, not
/// messages; system entries are skipped.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

struct ChatMessage {
    role: Role,
    text: String,
}

/// Cap on kept tool output per block, tail-kept (`tool_execution_update`
/// snapshots are replaced wholesale, so capping bounds client memory only —
/// the full output lives in the session file on the machine).
const TOOL_OUTPUT_MAX_BYTES: usize = 64 * 1024;

/// Where a tool call is in its lifecycle; drives the header dot. A history
/// `toolCall` without a matching `toolResult` stays `Running` — either the
/// tool is genuinely in flight (attach mid-turn) or the session died with it.
enum ToolStatus {
    Running,
    Ok,
    Error,
}

/// One tool call rendered as a collapsible block (SPEC §Rendering pi),
/// correlated by `toolCallId` — pi runs tool executions in parallel and ends
/// arrive out of order (verified live against pi 0.83.0).
struct ToolBlock {
    tool_call_id: String,
    tool_name: String,
    args: Value,
    /// Latest output snapshot, tail-capped at `TOOL_OUTPUT_MAX_BYTES`.
    output: String,
    output_truncated: bool,
    /// Edit's pre-rendered numbered diff (`result.details.diff`).
    diff: Option<String>,
    status: ToolStatus,
    expanded: bool,
}

/// The conversation in render order: messages interleaved with tool blocks
/// (pi emits the assistant `message_end` before its tool executions start,
/// so plain push order is the true order).
enum ChatItem {
    Message(ChatMessage),
    Tool(ToolBlock),
}

pub struct Chat {
    sid: String,
    /// The session's repository (its cwd's folder name) — shown as a
    /// read-only prompt-box control between the model and the machine.
    workspace_label: SharedString,
    /// Display label of the machine this session runs on — shown as the
    /// prompt box's read-only machine control (the machine was picked while
    /// the chat was a ghost; a running session never moves).
    machine_label: SharedString,
    items: Vec<ChatItem>,
    /// The in-flight assistant message, replaced wholesale on every
    /// `message_update` (the event's `partial` is the full message so far).
    streaming: Option<String>,
    /// Prompt sent, `agent_settled` not yet seen.
    busy: bool,
    /// Set when the pi process exited; shown where the input was. Prompting
    /// again requires a resume (re-open from the sidebar — resume is
    /// `create_session` with the same sid).
    exited_notice: Option<String>,
    /// The session's switchable models (passthrough `get_available_models`,
    /// fetched at open — milliseconds against the running pi).
    models: ModelsState,
    /// The session's current model (`get_state` at open, replaced by each
    /// successful `set_model`); drives the control's label and check mark.
    current_model: Option<ModelChoice>,
    /// The model picker modal is open. Rendered by the workbench — the
    /// scrim has to cover the whole window, not just this pane.
    picker_open: bool,
    /// The three answers this chat can be waiting on. An answer carrying any
    /// other token belongs to a request already superseded (two `set_model`s
    /// from a fast double-pick are the live case) and is dropped.
    session_models: Pending<()>,
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
        let mut items = items_from_entries(entries);
        let busy = first_prompt.is_some();

        if let Some(text) = first_prompt {
            items.push(ChatItem::Message(ChatMessage {
                role: Role::User,
                text,
            }));
        }

        Self {
            sid,
            workspace_label,
            machine_label,
            items,
            streaming: None,
            busy,
            exited_notice: None,
            models: ModelsState::Loading,
            current_model: None,
            picker_open: false,
            session_models: Pending::default(),
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
            models: self.session_models.ask(&self.tokens, ()),
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
        self.items = items_from_entries(entries);
        self.streaming = None;

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
        if self.session_models.accept(token).is_none() {
            return;
        }

        self.models = match result {
            Ok(data) => ModelsState::Loaded(models::extract_model_choices(&data)),
            Err(detail) => {
                eprintln!("harvester: get_available_models ({}): {detail}", self.sid);

                ModelsState::Failed(detail)
            }
        };
        cx.notify();
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
        if matches!(self.models, ModelsState::Loaded(_)) {
            self.picker_open = true;
            cx.notify();
        }
    }

    pub fn close_model_picker(&mut self, cx: &mut Context<Self>) {
        self.picker_open = false;
        cx.notify();
    }

    /// A picker row was clicked: fire `set_model` on the running session.
    /// No optimism — the label changes when `ModelSet` confirms.
    pub fn pick_model(&mut self, index: usize, cx: &mut Context<Self>) {
        self.picker_open = false;

        let ModelsState::Loaded(model_choices) = &self.models else {
            return;
        };
        let Some(choice) = model_choices.get(index) else {
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
        if !self.picker_open {
            return None;
        }

        let ModelsState::Loaded(model_choices) = &self.models else {
            return None;
        };
        let current_index = self.current_model.as_ref().and_then(|current| {
            model_choices
                .iter()
                .position(|choice| choice.provider == current.provider && choice.id == current.id)
        });

        Some((model_choices.clone(), current_index))
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

        self.items.push(ChatItem::Message(ChatMessage {
            role: Role::User,
            text: event.0.clone(),
        }));
        self.busy = true;

        cx.emit(ChatRequest::Prompt(event.0.clone()));
        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    pub fn handle_pi_event(&mut self, event: &Value, cx: &mut Context<Self>) {
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            return;
        };

        match event_type {
            "agent_start" => self.busy = true,
            "message_update" => {
                let partial_text = event
                    .get("assistantMessageEvent")
                    .and_then(|update| update.get("partial"))
                    .map(message_text);

                if let Some(text) = partial_text {
                    if !text.is_empty() {
                        self.streaming = Some(text);
                    }
                }
            }
            "message_end" => {
                if let Some(message) = event.get("message") {
                    if message.get("role").and_then(Value::as_str) == Some("assistant") {
                        let text = message_text(message);

                        if !text.is_empty() {
                            self.items.push(ChatItem::Message(ChatMessage {
                                role: Role::Assistant,
                                text,
                            }));
                        }
                    }
                }
                self.streaming = None;
            }
            "tool_execution_start" => self.handle_tool_start(event),
            "tool_execution_update" => self.handle_tool_update(event),
            "tool_execution_end" => self.handle_tool_end(event),
            "agent_settled" => {
                self.busy = false;
                self.streaming = None;
            }
            // Everything else (retries, compaction, queue) is later-slice
            // material.
            _ => return,
        }

        self.scroll.scroll_to_bottom();
        cx.notify();
    }

    /// `tool_execution_start`: a new running block. Pi emits the assistant
    /// `message_end` first, so the block lands after its announcing text.
    fn handle_tool_start(&mut self, event: &Value) {
        let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            return;
        };
        let tool_name = event
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or("tool");

        self.items.push(ChatItem::Tool(ToolBlock {
            tool_call_id: tool_call_id.to_string(),
            tool_name: tool_name.to_string(),
            args: event.get("args").cloned().unwrap_or(Value::Null),
            output: String::new(),
            output_truncated: false,
            diff: None,
            status: ToolStatus::Running,
            expanded: false,
        }));
    }

    /// `tool_execution_update`: `partialResult` is the accumulated snapshot —
    /// replace the block's output, never append.
    fn handle_tool_update(&mut self, event: &Value) {
        let Some(partial_result) = event.get("partialResult") else {
            return;
        };
        let (output, truncated) = capped_content_text(partial_result);

        if let Some(block) = self.find_tool_block(event) {
            block.output = output;
            block.output_truncated = truncated;
        }
    }

    /// `tool_execution_end`: final output, error flag, and (for edit) the
    /// pre-rendered numbered diff in `result.details.diff`.
    fn handle_tool_end(&mut self, event: &Value) {
        let is_error = event
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = event.get("result");

        let Some(block) = self.find_tool_block(event) else {
            return;
        };

        block.status = if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        };

        if let Some(result) = result {
            (block.output, block.output_truncated) = capped_content_text(result);
            block.diff = extract_diff(result);
        }
    }

    /// The block a tool event belongs to, by `toolCallId` — searched from the
    /// back since live events concern recent blocks.
    fn find_tool_block(&mut self, event: &Value) -> Option<&mut ToolBlock> {
        let tool_call_id = event.get("toolCallId").and_then(Value::as_str)?;

        self.items.iter_mut().rev().find_map(|item| match item {
            ChatItem::Tool(block) if block.tool_call_id == tool_call_id => Some(block),
            _ => None,
        })
    }

    /// A tool block's header was clicked: toggle its body.
    fn toggle_tool_block(&mut self, index: usize, cx: &mut Context<Self>) {
        if let Some(ChatItem::Tool(block)) = self.items.get_mut(index) {
            block.expanded = !block.expanded;
            cx.notify();
        }
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

        self.busy = false;
        self.streaming = None;
        self.exited_notice = Some(format!(
            "session exited{code_text} — click it in the sidebar to resume"
        ));
        cx.notify();
    }

    fn render_streaming(&self) -> Option<impl IntoElement> {
        let text = match (&self.streaming, self.busy) {
            (Some(text), _) => format!("{text}▍"),
            // Prompt accepted, no partial yet: show that pi is working.
            (None, true) => "…".to_string(),
            (None, false) => return None,
        };

        Some(ui::chat_message("pi", theme::TEXT_DIM, text, false))
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
        let model_enabled = matches!(self.models, ModelsState::Loaded(_));

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
                                self.items
                                    .iter()
                                    .enumerate()
                                    .map(|(index, item)| render_item(index, item, cx)),
                            )
                            .children(self.render_streaming()),
                    ),
            )
            .child(self.render_input_row(window, cx))
    }
}

/// One rendered conversation item; tool blocks carry their item index so
/// their click listeners can find them again.
fn render_item(index: usize, item: &ChatItem, cx: &mut Context<Chat>) -> AnyElement {
    match item {
        ChatItem::Message(message) => render_message(message).into_any_element(),
        ChatItem::Tool(block) => render_tool_block(index, block, cx).into_any_element(),
    }
}

fn render_message(message: &ChatMessage) -> impl IntoElement {
    let (label, label_color) = match message.role {
        Role::User => ("you", theme::ACCENT),
        Role::Assistant => ("pi", theme::TEXT_DIM),
    };

    ui::chat_message(
        label,
        label_color,
        message.text.clone(),
        message.role == Role::User,
    )
}

fn render_tool_block(index: usize, block: &ToolBlock, cx: &mut Context<Chat>) -> impl IntoElement {
    let dot_color = match block.status {
        ToolStatus::Running => theme::ACCENT,
        ToolStatus::Ok => theme::TEXT_FAINT,
        ToolStatus::Error => theme::STATUS_ERROR,
    };
    let header = ui::tool_block_header(
        ("tool-block", index),
        dot_color,
        tool_block_label(block),
        block.expanded,
    )
    .on_click(cx.listener(move |chat, _, _, cx| chat.toggle_tool_block(index, cx)));

    ui::tool_block()
        .child(header)
        .when(block.expanded, |container| {
            container.child(render_tool_body(block))
        })
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
            let command = arg_str(&block.args, "command").unwrap_or_default();
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
            let path = arg_str(&block.args, "path").unwrap_or_default();
            let label = label
                .child(ui::tool_block_verb(verb))
                .child(ui::tool_block_subject(
                    basename(path).to_string().into(),
                    false,
                ));

            match line_stats(block) {
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
            let pattern = arg_str(&block.args, "pattern").unwrap_or_default();

            label
                .child(ui::tool_block_verb(verb))
                .child(ui::tool_block_subject(pattern.to_string().into(), true))
        }
        "ls" => {
            let path = arg_str(&block.args, "path").unwrap_or(".");

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
            if let Some(content) = arg_str(&block.args, "content") {
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
        "read" | "write" | "edit" => arg_str(&block.args, "path").unwrap_or_default().to_string(),
        "bash" => arg_str(&block.args, "command")
            .unwrap_or_default()
            .to_string(),
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

/// "+N −N" for the label: write counts its content argument (known from the
/// start), edit counts its diff's +/− lines (known once the result lands).
fn line_stats(block: &ToolBlock) -> Option<(usize, usize)> {
    match block.tool_name.as_str() {
        "write" => {
            let content = arg_str(&block.args, "content")?;

            Some((content.lines().count(), 0))
        }
        "edit" => {
            let diff = block.diff.as_deref()?;
            let added = diff.lines().filter(|line| line.starts_with('+')).count();
            let removed = diff.lines().filter(|line| line.starts_with('-')).count();

            Some((added, removed))
        }
        _ => None,
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
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

/// `get_entries` history → render items: user/assistant text plus tool
/// blocks rebuilt from assistant `toolCall` blocks matched to their
/// `toolResult` messages — the same data the live events carry (verified
/// against pi 0.83.0 session captures).
fn items_from_entries(entries: &[Value]) -> Vec<ChatItem> {
    // toolCallId → toolResult message, collected ahead of the walk (results
    // land in later entries than their calls).
    let mut tool_results: HashMap<&str, &Value> = HashMap::new();

    for entry in entries {
        let Some(message) = entry.get("message") else {
            continue;
        };

        if message.get("role").and_then(Value::as_str) == Some("toolResult") {
            if let Some(tool_call_id) = message.get("toolCallId").and_then(Value::as_str) {
                tool_results.insert(tool_call_id, message);
            }
        }
    }

    let mut items = Vec::new();

    for entry in entries {
        let Some(message) = entry.get("message") else {
            continue;
        };
        let role = match message.get("role").and_then(Value::as_str) {
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            _ => continue,
        };
        let text = message_text(message);

        if !text.is_empty() {
            items.push(ChatItem::Message(ChatMessage { role, text }));
        }

        if role == Role::Assistant {
            let Some(Value::Array(blocks)) = message.get("content") else {
                continue;
            };

            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                    if let Some(tool_block) = tool_block_from_history(block, &tool_results) {
                        items.push(ChatItem::Tool(tool_block));
                    }
                }
            }
        }
    }

    items
}

/// One history `toolCall {id, name, arguments}` → a finished block, filled
/// from its `toolResult` when one exists. Without a result the block stays
/// `Running`: either the tool is genuinely in flight (attach mid-turn — the
/// live `tool_execution_*` events will find this block by id) or the session
/// died with it (the exited notice tells that story).
fn tool_block_from_history(
    block: &Value,
    tool_results: &HashMap<&str, &Value>,
) -> Option<ToolBlock> {
    let tool_call_id = block.get("id").and_then(Value::as_str)?;
    let tool_name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
    let mut tool_block = ToolBlock {
        tool_call_id: tool_call_id.to_string(),
        tool_name: tool_name.to_string(),
        args: block.get("arguments").cloned().unwrap_or(Value::Null),
        output: String::new(),
        output_truncated: false,
        diff: None,
        status: ToolStatus::Running,
        expanded: false,
    };

    if let Some(result) = tool_results.get(tool_call_id) {
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        tool_block.status = if is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Ok
        };
        (tool_block.output, tool_block.output_truncated) = capped_content_text(result);
        tool_block.diff = extract_diff(result);
    }

    Some(tool_block)
}

/// The joined text of a result-like object's `content` blocks (live results
/// and history `toolResult` messages share the shape), tail-capped at
/// `TOOL_OUTPUT_MAX_BYTES`.
fn capped_content_text(container: &Value) -> (String, bool) {
    let mut text = String::new();

    if let Some(Value::Array(blocks)) = container.get("content") {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) == Some("text") {
                if let Some(piece) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(piece);
                }
            }
        }
    }

    if text.len() <= TOOL_OUTPUT_MAX_BYTES {
        return (text, false);
    }

    let cut_start = text.len() - TOOL_OUTPUT_MAX_BYTES;
    let boundary = (cut_start..text.len())
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or(text.len());

    (text[boundary..].to_string(), true)
}

/// Edit's pre-rendered numbered diff, when the result carries one.
fn extract_diff(result: &Value) -> Option<String> {
    result
        .get("details")?
        .get("diff")?
        .as_str()
        .map(str::to_string)
}

/// Text extraction from a pi message: text blocks verbatim, thinking and
/// tool calls skipped (tool calls render as blocks, not inline text).
fn message_text(message: &Value) -> String {
    let mut pieces: Vec<String> = Vec::new();

    match message.get("content") {
        Some(Value::String(text)) => pieces.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(text) = block.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            pieces.push(text.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }

    pieces.join("\n\n")
}
