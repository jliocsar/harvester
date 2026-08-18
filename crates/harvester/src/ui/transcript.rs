//! Transcript presentation: messages, markdown, tool labeling and bodies, and
//! expansion state.
//!
//! The client-core [`Transcript`] remains the verbatim-derived conversation
//! replica. This module adds only visual policy, keyed by stable tool-call ids,
//! so a history repair can replace every item without moving expansion onto a
//! different block.

use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

use gpui::{
    div, prelude::*, px, AnyElement, App, Div, ElementId, ScrollHandle, SharedString, Window,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::ActiveTheme as _;
use harvester_client_core::transcript::{Item, Message, Role, ToolBlock, ToolStatus, Transcript};
use serde_json::Value;

use super::core::Tone;
use super::tool_block::{self, DiffLineKind};

/// Cap on the compact-args summary in unknown-tool labels; the full args are
/// always available in the expanded body.
const ARGS_SUMMARY_MAX_CHARS: usize = 80;

type ToggleToolBlock = Rc<dyn Fn(String, &mut Window, &mut App)>;

/// Stateful presentation policy for a transcript.
///
/// This owns exactly the state that belongs to rendering: which stable
/// `toolCallId`s the user expanded. Session lifecycle, Pi events, prompt state,
/// and scrolling remain in `Chat`.
#[derive(Default)]
pub struct TranscriptPresentation {
    expanded_tool_calls: HashSet<String>,
}

impl TranscriptPresentation {
    /// Toggle one tool block by Pi's stable call id.
    pub fn toggle_tool_block(&mut self, tool_call_id: String) {
        if !self.expanded_tool_calls.remove(&tool_call_id) {
            self.expanded_tool_calls.insert(tool_call_id);
        }
    }

    /// Render settled items plus the in-flight assistant partial.
    ///
    /// The supplied scroll handle is owned by `Chat`: following new events is
    /// lifecycle/adaptation policy, while the scrollable message surface is
    /// presentation policy.
    pub fn render(
        &self,
        transcript: &Transcript,
        scroll: &ScrollHandle,
        toggle_tool_block: impl Fn(String, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> impl IntoElement {
        let toggle_tool_block: ToggleToolBlock = Rc::new(toggle_tool_block);

        div()
            .id("chat-messages")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .track_scroll(scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_3()
                    .children(transcript.items().iter().enumerate().map(|(index, item)| {
                        self.render_item(index, item, toggle_tool_block.clone(), cx)
                    }))
                    .children(render_streaming(transcript, cx)),
            )
    }

    fn render_item(
        &self,
        index: usize,
        item: &Item,
        toggle_tool_block: ToggleToolBlock,
        cx: &App,
    ) -> AnyElement {
        match item {
            Item::Message(message) => render_message(index, message, cx).into_any_element(),
            Item::Tool(block) => self
                .render_tool_block(index, block, toggle_tool_block, cx)
                .into_any_element(),
            Item::Failure(detail) => render_failure(detail, cx).into_any_element(),
        }
    }

    fn render_tool_block(
        &self,
        index: usize,
        block: &ToolBlock,
        toggle_tool_block: ToggleToolBlock,
        cx: &App,
    ) -> Div {
        let tone = match block.status {
            // Running is active rather than pending: it is the live thing the
            // user is watching on screen.
            ToolStatus::Running => Tone::Active,
            ToolStatus::Ok => Tone::Idle,
            ToolStatus::Error => Tone::Failed,
        };
        let expanded = self.expanded_tool_calls.contains(&block.tool_call_id);
        let tool_call_id = block.tool_call_id.clone();
        let header = tool_block::header(
            ("tool-block", index),
            tone,
            tool_label(block, cx),
            expanded,
            cx,
        )
        .on_click(move |_, window, cx| {
            toggle_tool_block(tool_call_id.clone(), window, cx);
        });

        tool_block::block(cx)
            .child(header)
            .when(expanded, |container| {
                container.child(render_tool_body(block, cx))
            })
    }
}

/// Assistant prose rendered as markdown. State lives in the window under
/// `id`; unchanged settled text returns early, while only the one streaming
/// partial reparses per chunk (SPEC C11).
fn markdown(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .style(markdown_style())
}

/// The app is pinned dark, while the library's default highlight theme is
/// light. Build the matching style once.
fn markdown_style() -> TextViewStyle {
    static STYLE: OnceLock<TextViewStyle> = OnceLock::new();

    STYLE
        .get_or_init(|| TextViewStyle {
            highlight_theme: HighlightTheme::default_dark(),
            paragraph_gap: gpui::rems(0.5),
            is_dark: true,
            ..TextViewStyle::default()
        })
        .clone()
}

/// One message frame: author over body; user messages get a bubble.
fn message_frame(
    author: &'static str,
    author_tone: Tone,
    body: impl IntoElement,
    bubble: bool,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(author_tone.color(cx))
                .child(author),
        )
        .child(
            div()
                .when(bubble, |style| {
                    style.bg(cx.theme().muted).rounded_md().px_3().py_2()
                })
                .child(body),
        )
}

/// User text is verbatim; only Pi's prose is markdown.
fn render_message(index: usize, message: &Message, cx: &App) -> Div {
    match message.role {
        Role::User => message_frame("you", Tone::Active, message.text.clone(), true, cx),
        Role::Assistant => message_frame(
            "pi",
            Tone::Muted,
            markdown(("message", index), message.text.clone()),
            false,
            cx,
        ),
    }
}

/// A failed turn is Pi-authored but not markdown: provider error strings must
/// preserve literal `*` and `#` characters.
fn render_failure(detail: &str, cx: &App) -> Div {
    message_frame("pi", Tone::Failed, detail.to_string(), false, cx)
}

/// The turn in flight: its partial markdown, or an activity placeholder before
/// the first partial arrives.
fn render_streaming(transcript: &Transcript, cx: &App) -> Option<Div> {
    match (transcript.streaming(), transcript.is_busy()) {
        (Some(text), _) => Some(message_frame(
            "pi",
            Tone::Muted,
            markdown("streaming-message", format!("{text}▍")),
            false,
            cx,
        )),
        (None, true) => Some(message_frame("pi", Tone::Muted, "…", false, cx)),
        (None, false) => None,
    }
}

/// Header policy for Pi 0.83.0's built-in tools. Unknown tools show a compact
/// raw-args summary rather than inventing a vocabulary for their payloads.
fn tool_label(block: &ToolBlock, cx: &App) -> Div {
    let label = tool_block::label();

    match block.tool_name.as_str() {
        "bash" => {
            let command = block.arg("command").unwrap_or_default();
            let first_line = command.lines().next().unwrap_or_default().to_string();

            label
                .child(tool_block::verb("$", cx))
                .child(tool_block::subject(first_line.into(), true))
        }
        "read" | "write" | "edit" => {
            let verb = match block.tool_name.as_str() {
                "read" => "Read",
                "write" => "Write",
                _ => "Edit",
            };
            let path = block.arg("path").unwrap_or_default();
            let label = label
                .child(tool_block::verb(verb, cx))
                .child(tool_block::subject(
                    basename(path).to_string().into(),
                    false,
                ));

            match block.line_stats() {
                Some((added, removed)) => label.child(tool_block::stats(added, removed, cx)),
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
                .child(tool_block::verb(verb, cx))
                .child(tool_block::subject(pattern.to_string().into(), true))
        }
        "ls" => {
            let path = block.arg("path").unwrap_or(".");

            label
                .child(tool_block::verb("List", cx))
                .child(tool_block::subject(path.to_string().into(), false))
        }
        _ => label
            .child(tool_block::verb(block.tool_name.clone(), cx))
            .child(tool_block::subject(compact_args(&block.args).into(), true)),
    }
}

/// Expanded-body policy: context, then edit/write diff or output snapshot.
/// Errors always show output because that is where their message lives.
fn render_tool_body(block: &ToolBlock, cx: &App) -> Div {
    let mut body = tool_block::body(cx).child(tool_block::context(context_line(block).into(), cx));

    if !matches!(block.status, ToolStatus::Error) {
        if let Some(diff) = &block.diff {
            return body.children(diff.lines().map(|line| render_diff_line(line, cx)));
        }

        if block.tool_name == "write" {
            if let Some(content) = block.arg("content") {
                return body.children(content.lines().enumerate().map(|(index, line)| {
                    tool_block::diff_line(
                        DiffLineKind::Added,
                        format!("+{} {line}", index + 1).into(),
                        cx,
                    )
                }));
            }
        }
    }

    if block.output_truncated {
        body = body.child(tool_block::truncation_notice(cx));
    }

    if block.output.is_empty() {
        let placeholder = match block.status {
            ToolStatus::Running => "running…",
            _ => "(no output)",
        };

        return body.child(tool_block::output(placeholder.into(), cx));
    }

    body.child(tool_block::output(block.output.clone().into(), cx))
}

/// Faint first line with enough data to identify the call in full.
fn context_line(block: &ToolBlock) -> String {
    match block.tool_name.as_str() {
        "read" | "write" | "edit" => block.arg("path").unwrap_or_default().to_string(),
        "bash" => block.arg("command").unwrap_or_default().to_string(),
        _ => block.args.to_string(),
    }
}

/// Classify Pi's numbered diff format (" 1 ctx" / "-2 old" / "+2 new").
fn diff_line_kind(line: &str) -> DiffLineKind {
    match line.as_bytes().first() {
        Some(b'+') => DiffLineKind::Added,
        Some(b'-') => DiffLineKind::Removed,
        _ => DiffLineKind::Context,
    }
}

fn render_diff_line(line: &str, cx: &App) -> Div {
    tool_block::diff_line(diff_line_kind(line), line.to_string().into(), cx)
}

fn basename(path: &str) -> &str {
    let normalized = path.trim_end_matches('/');

    if normalized.is_empty() && path.starts_with('/') {
        return "/";
    }

    normalized.rsplit('/').next().unwrap_or(normalized)
}

/// Raw JSON summary for an unknown tool, capped by characters rather than
/// bytes so a Unicode argument is never split.
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
    use serde_json::json;

    use super::*;

    fn block(tool_name: &str, args: Value) -> ToolBlock {
        ToolBlock {
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            args,
            output: String::new(),
            output_truncated: false,
            diff: None,
            status: ToolStatus::Ok,
        }
    }

    #[test]
    fn basename_uses_the_last_path_component() {
        assert_eq!(basename("/tmp/repo/src/main.rs"), "main.rs");
        assert_eq!(basename("Cargo.toml"), "Cargo.toml");
        assert_eq!(basename("/tmp/repo/"), "repo");
        assert_eq!(basename("/"), "/");
    }

    #[test]
    fn compact_args_preserves_short_json_and_caps_unicode_by_character() {
        assert_eq!(
            compact_args(&json!({"path": "src/main.rs"})),
            r#"{"path":"src/main.rs"}"#
        );

        let summary = compact_args(&Value::String("é".repeat(100)));

        assert_eq!(summary.chars().count(), ARGS_SUMMARY_MAX_CHARS + 1);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn diff_classification_uses_the_numbered_diff_marker() {
        assert_eq!(diff_line_kind("+2 new"), DiffLineKind::Added);
        assert_eq!(diff_line_kind("-2 old"), DiffLineKind::Removed);
        assert_eq!(diff_line_kind(" 1 ctx"), DiffLineKind::Context);
        assert_eq!(diff_line_kind(""), DiffLineKind::Context);
    }

    #[test]
    fn context_policy_keeps_paths_commands_and_unknown_args_verbatim() {
        assert_eq!(
            context_line(&block("read", json!({"path": "/tmp/repo/src/main.rs"}))),
            "/tmp/repo/src/main.rs"
        );
        assert_eq!(
            context_line(&block("bash", json!({"command": "printf 'a\\nb'"}))),
            "printf 'a\\nb'"
        );
        assert_eq!(
            context_line(&block("future_tool", json!({"z": 1, "a": true}))),
            r#"{"z":1,"a":true}"#
        );
    }
}
