//! One attached session's conversation: replayed history plus the live turn.
//! Streaming rule (docs/SPEC.md §Rendering pi): `message_update` carries the
//! whole partial message — replace, never append. `agent_settled` (not
//! `agent_end`) ends the turn.

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, ScrollHandle, Subscription, Window,
};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::Command;
use crate::input::{PromptSubmitted, TextInput};
use crate::theme;

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
    /// Focus the prompt input on the next frame (set at creation; render has
    /// the `Window` that focusing needs).
    focus_input: bool,
    input: Entity<TextInput>,
    scroll: ScrollHandle,
    commands: UnboundedSender<Command>,
    _input_subscription: Subscription,
}

impl Chat {
    pub fn new(
        sid: String,
        entries: &[Value],
        commands: UnboundedSender<Command>,
        cx: &mut Context<Self>,
    ) -> Self {
        let input = cx.new(|cx| TextInput::new("Message pi — Enter to send", cx));
        let input_subscription = cx.subscribe(&input, Self::handle_prompt_submitted);
        let messages = entries.iter().filter_map(entry_message).collect();

        Self {
            sid,
            messages,
            streaming: None,
            busy: false,
            exited_notice: None,
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

        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(div().text_xs().text_color(rgb(label_color)).child(label))
            .child(
                div()
                    .when(message.role == Role::User, |style| {
                        style.bg(rgb(theme::INPUT_BG)).rounded_md().px_3().py_2()
                    })
                    .child(message.text.clone()),
            )
    }

    fn render_streaming(&self) -> Option<impl IntoElement> {
        let text = match (&self.streaming, self.busy) {
            (Some(text), _) => format!("{text}▍"),
            // Prompt accepted, no partial yet: show that pi is working.
            (None, true) => "…".to_string(),
            (None, false) => return None,
        };

        Some(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_xs().text_color(rgb(theme::TEXT_DIM)).child("pi"))
                .child(div().child(text)),
        )
    }

    fn render_input_row(&self, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let row = div()
            .flex_none()
            .p_2()
            .border_t_1()
            .border_color(rgb(theme::BORDER));

        if let Some(notice) = &self.exited_notice {
            return row.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child(notice.clone()),
            );
        }

        let input_focused = self.input.read(cx).focus_handle(cx).is_focused(window);

        row.child(
            div()
                .bg(rgb(theme::INPUT_BG))
                .rounded_md()
                .border_1()
                .border_color(if input_focused {
                    rgb(theme::ACCENT)
                } else {
                    rgb(theme::BORDER)
                })
                .px_2()
                .py_1()
                .child(self.input.clone()),
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
