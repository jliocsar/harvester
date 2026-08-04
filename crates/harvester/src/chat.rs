//! One attached session's conversation: replayed history plus the live turn.
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, ScrollHandle, SharedString, Subscription,
    Window,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::Command;
use crate::input::{PromptSubmitted, TextInput};
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme;
use crate::ui;

/// Who said a rendered message — only what slice 4 draws. Tool-result and
/// system entries are skipped, tool calls render as placeholders.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    User,
    Assistant,
}

struct ChatMessage {
    role: Role,
    text: String,
}

pub struct Chat {
    sid: String,
    /// Display label of the machine this session runs on — shown as the
    /// prompt box's read-only machine control (the machine was picked while
    /// the chat was a ghost; a running session never moves).
    machine_label: SharedString,
    messages: Vec<ChatMessage>,
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
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<TextInput>,
    scroll: ScrollHandle,
    commands: UnboundedSender<Command>,
    _input_subscription: Subscription,
}

impl Chat {
    /// `first_prompt` seeds a ghost-chat start: the bridge already sent that
    /// prompt as part of the create → attach chain, so the conversation opens
    /// with it rendered and the turn marked busy.
    pub fn new(
        sid: String,
        machine_label: SharedString,
        entries: &[Value],
        first_prompt: Option<String>,
        commands: UnboundedSender<Command>,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Message pi — Enter to send", cx));
        let input_subscription = cx.subscribe(&input, Self::handle_prompt_submitted);
        let mut messages: Vec<ChatMessage> = entries.iter().filter_map(entry_message).collect();
        let busy = first_prompt.is_some();

        if let Some(text) = first_prompt {
            messages.push(ChatMessage {
                role: Role::User,
                text,
            });
        }

        // The model control needs the list and the current model; both queue
        // on pi's in-order stdin. A dead bridge is handled by `Disconnected`.
        let _ = commands.send(Command::ListSessionModels { sid: sid.clone() });
        let _ = commands.send(Command::GetSessionState { sid: sid.clone() });

        Self {
            sid,
            machine_label,
            messages,
            streaming: None,
            busy,
            exited_notice: None,
            models: ModelsState::Loading,
            current_model: None,
            picker_open: false,
            focus_input: true,
            input,
            scroll: ScrollHandle::new(),
            commands,
            _input_subscription: input_subscription,
        }
    }

    pub fn sid(&self) -> &str {
        &self.sid
    }

    /// A `ListSessionModels` answered. On failure the control just stays
    /// disabled — the session keeps running on its current model.
    pub fn handle_session_models(&mut self, result: Result<Value, String>, cx: &mut Context<Self>) {
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
    pub fn handle_session_state(&mut self, result: Result<Value, String>, cx: &mut Context<Self>) {
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
    pub fn handle_model_set(&mut self, data: &Value, cx: &mut Context<Self>) {
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

        if self
            .commands
            .send(Command::SetModel {
                sid: self.sid.clone(),
                provider: choice.provider.clone(),
                model_id: choice.id.clone(),
            })
            .is_err()
        {
            // Bridge gone; the Disconnected event drives the visible state.
            eprintln!("harvester: bridge gone; set_model not sent");
        }

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

        self.messages.push(ChatMessage {
            role: Role::User,
            text: event.0.clone(),
        });
        self.busy = true;

        if self
            .commands
            .send(Command::Prompt {
                sid: self.sid.clone(),
                text: event.0.clone(),
            })
            .is_err()
        {
            // Bridge gone; the Disconnected event drives the visible state.
            eprintln!("harvester: bridge gone; prompt not sent");
        }

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
                            self.messages.push(ChatMessage {
                                role: Role::Assistant,
                                text,
                            });
                        }
                    }
                }
                self.streaming = None;
            }
            "agent_settled" => {
                self.busy = false;
                self.streaming = None;
            }
            // Everything else (tool executions, retries, compaction, queue)
            // is later-slice material.
            _ => return,
        }

        self.scroll.scroll_to_bottom();
        cx.notify();
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
                        // Read-only: the machine was picked at ghost time; a
                        // running session never moves.
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
                            .children(self.messages.iter().map(Self::render_message))
                            .children(self.render_streaming()),
                    ),
            )
            .child(self.render_input_row(window, cx))
    }
}

/// One `get_entries` entry → a rendered message, if it is one.
fn entry_message(entry: &Value) -> Option<ChatMessage> {
    let message = entry.get("message")?;
    let role = match message.get("role").and_then(Value::as_str)? {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        _ => return None,
    };
    let text = message_text(message);

    if text.is_empty() {
        return None;
    }

    Some(ChatMessage { role, text })
}

/// Slice-4 text extraction from a pi message: text blocks verbatim, tool
/// calls as one-line placeholders, thinking skipped. Rich blocks are a later
/// slice.
fn message_text(message: &Value) -> String {
    let mut pieces: Vec<String> = Vec::new();

    match message.get("content") {
        Some(Value::String(text)) => pieces.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                match block.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                pieces.push(text.to_string());
                            }
                        }
                    }
                    Some("toolCall") => {
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                        pieces.push(format!("[{name}]"));
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }

    pieces.join("\n\n")
}
