use std::collections::HashSet;
use std::rc::Rc;
use std::sync::OnceLock;

use gpui::{
    div, prelude::*, relative, AnyElement, App, Div, ElementId, ScrollHandle, SharedString, Window,
};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::ActiveTheme as _;
use harvester_client_core::transcript::{Item, Message, Role, ToolBlock, ToolStatus, Transcript};
use serde_json::Value;

use super::core::Tone;
use super::tool_block::{self, DiffLineKind};

const ARGS_SUMMARY_MAX_CHARS: usize = 80;
const MESSAGE_LINE_HEIGHT: f32 = 1.7;
const USER_BUBBLE_MAX_WIDTH: f32 = 0.7;

const RUNNING_PLACEHOLDER: &str = "Running…";
const NO_OUTPUT_PLACEHOLDER: &str = "No output";

const RUNNING_STATUS_LABEL: &str = "Running";
const COMPLETED_STATUS_LABEL: &str = "Completed";
const FAILED_STATUS_LABEL: &str = "Failed";

type ToggleToolBlock = Rc<dyn Fn(String, &mut Window, &mut App)>;

#[derive(Default)]
pub struct TranscriptPresentation {
    expanded_tool_calls: HashSet<String>,
}

impl TranscriptPresentation {
    pub fn toggle_tool_block(&mut self, tool_call_id: String) {
        if !self.expanded_tool_calls.remove(&tool_call_id) {
            self.expanded_tool_calls.insert(tool_call_id);
        }
    }

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
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(scroll)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_4()
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
        let tone = tool_tone(block.status);
        let status_label = tool_status_label(block.status);
        let summary = tool_summary(block);
        let expanded = self.expanded_tool_calls.contains(&block.tool_call_id);
        let tool_call_id = block.tool_call_id.clone();
        let header = tool_block::header(
            index,
            tone,
            status_label.into(),
            tool_accessible_name(&summary, status_label),
            tool_label(&summary, cx),
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

fn markdown(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .style(markdown_style())
}

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

fn message_frame(body: impl IntoElement, bubble: bool, cx: &App) -> Div {
    let content = div()
        .line_height(relative(MESSAGE_LINE_HEIGHT))
        .when(bubble, |style| {
            style
                .max_w(relative(USER_BUBBLE_MAX_WIDTH))
                .bg(cx.theme().muted)
                .rounded(cx.theme().radius)
                .px_3()
                .py_2()
        })
        .child(body);

    if bubble {
        return div().flex().justify_end().child(content);
    }

    content
}

fn render_message(index: usize, message: &Message, cx: &App) -> Div {
    match message.role {
        Role::User => message_frame(message.text.clone(), true, cx),
        Role::Assistant => message_frame(
            markdown(("message", index), message.text.clone()),
            false,
            cx,
        ),
    }
}

fn render_failure(detail: &str, cx: &App) -> Div {
    message_frame(
        div()
            .text_color(Tone::Failed.color(cx))
            .child(detail.to_string()),
        false,
        cx,
    )
}

fn render_streaming(transcript: &Transcript, cx: &App) -> Option<Div> {
    match (transcript.streaming(), transcript.is_busy()) {
        (Some(text), _) => Some(message_frame(
            markdown("streaming-message", format!("{text}▍")),
            false,
            cx,
        )),
        (None, true) => Some(message_frame("…", false, cx)),
        (None, false) => None,
    }
}

fn tool_tone(status: ToolStatus) -> Tone {
    match status {
        ToolStatus::Running => Tone::Active,
        ToolStatus::Ok => Tone::Muted,
        ToolStatus::Error => Tone::Failed,
    }
}

fn tool_status_label(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => RUNNING_STATUS_LABEL,
        ToolStatus::Ok => COMPLETED_STATUS_LABEL,
        ToolStatus::Error => FAILED_STATUS_LABEL,
    }
}

struct ToolSummary {
    verb: SharedString,
    accessible_verb: SharedString,
    subject: SharedString,
    subject_is_monospace: bool,
    line_stats: Option<(usize, usize)>,
}

fn tool_summary(block: &ToolBlock) -> ToolSummary {
    match block.tool_name.as_str() {
        "bash" => bash_summary(block),
        "read" | "write" | "edit" => file_summary(block),
        "grep" | "find" => search_summary(block),
        "ls" => list_summary(block),
        _ => unknown_tool_summary(block),
    }
}

fn bash_summary(block: &ToolBlock) -> ToolSummary {
    let command = block.arg("command").unwrap_or_default();
    let first_line = command.lines().next().unwrap_or_default().to_string();

    ToolSummary {
        verb: "$".into(),
        accessible_verb: "Run".into(),
        subject: first_line.into(),
        subject_is_monospace: true,
        line_stats: None,
    }
}

fn file_summary(block: &ToolBlock) -> ToolSummary {
    let verb: SharedString = match block.tool_name.as_str() {
        "read" => "Read".into(),
        "write" => "Write".into(),
        _ => "Edit".into(),
    };
    let path = block.arg("path").unwrap_or_default();

    ToolSummary {
        verb: verb.clone(),
        accessible_verb: verb,
        subject: basename(path).to_string().into(),
        subject_is_monospace: false,
        line_stats: block.line_stats(),
    }
}

fn search_summary(block: &ToolBlock) -> ToolSummary {
    let verb: SharedString = if block.tool_name == "grep" {
        "Grep".into()
    } else {
        "Find".into()
    };
    let pattern = block.arg("pattern").unwrap_or_default();

    ToolSummary {
        verb: verb.clone(),
        accessible_verb: verb,
        subject: pattern.to_string().into(),
        subject_is_monospace: true,
        line_stats: None,
    }
}

fn list_summary(block: &ToolBlock) -> ToolSummary {
    let path = block.arg("path").unwrap_or(".");

    ToolSummary {
        verb: "List".into(),
        accessible_verb: "List".into(),
        subject: path.to_string().into(),
        subject_is_monospace: false,
        line_stats: None,
    }
}

fn unknown_tool_summary(block: &ToolBlock) -> ToolSummary {
    let verb: SharedString = block.tool_name.clone().into();

    ToolSummary {
        verb: verb.clone(),
        accessible_verb: verb,
        subject: compact_args(&block.args).into(),
        subject_is_monospace: true,
        line_stats: None,
    }
}

fn tool_label(summary: &ToolSummary, cx: &App) -> Div {
    let label = tool_block::label()
        .child(tool_block::verb(summary.verb.clone(), cx))
        .child(tool_block::subject(
            summary.subject.clone(),
            summary.subject_is_monospace,
            cx,
        ));

    match summary.line_stats {
        Some((added, removed)) => label.child(tool_block::stats(added, removed, cx)),
        None => label,
    }
}

fn tool_accessible_name(summary: &ToolSummary, status_label: &'static str) -> SharedString {
    format!(
        "{} {}, {status_label}",
        summary.accessible_verb, summary.subject
    )
    .into()
}

fn render_tool_body(block: &ToolBlock, cx: &App) -> Div {
    let body = tool_block::body(cx).child(tool_block::context(context_line(block).into(), cx));

    if let Some(content) = render_tool_content(block, cx) {
        return body.child(content);
    }

    let body = body.child(tool_block::output(output_text(block), cx));

    if block.output_truncated {
        return body.child(tool_block::truncation_notice(cx));
    }

    body
}

fn render_tool_content(block: &ToolBlock, cx: &App) -> Option<Div> {
    if matches!(block.status, ToolStatus::Error) {
        return None;
    }

    if let Some(diff) = &block.diff {
        return Some(render_diff(diff, cx));
    }

    render_written_content(block, cx)
}

fn render_diff(diff: &str, cx: &App) -> Div {
    tool_block::code_lines().children(diff.lines().map(|line| render_diff_line(line, cx)))
}

fn render_written_content(block: &ToolBlock, cx: &App) -> Option<Div> {
    if block.tool_name != "write" {
        return None;
    }

    let content = block.arg("content")?;

    Some(
        tool_block::code_lines().children(
            content
                .lines()
                .enumerate()
                .map(|(index, line)| render_written_line(index, line, cx)),
        ),
    )
}

fn render_written_line(index: usize, line: &str, cx: &App) -> Div {
    tool_block::diff_line(
        DiffLineKind::Added,
        format!("+{} {line}", index + 1).into(),
        cx,
    )
}

fn output_text(block: &ToolBlock) -> SharedString {
    if !block.output.is_empty() {
        return block.output.clone().into();
    }

    match block.status {
        ToolStatus::Running => RUNNING_PLACEHOLDER.into(),
        _ => NO_OUTPUT_PLACEHOLDER.into(),
    }
}

fn context_line(block: &ToolBlock) -> String {
    match block.tool_name.as_str() {
        "read" | "write" | "edit" => block.arg("path").unwrap_or_default().to_string(),
        "bash" => block.arg("command").unwrap_or_default().to_string(),
        _ => block.args.to_string(),
    }
}

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

    #[test]
    fn accessible_name_states_the_action_the_subject_and_the_status() {
        let summary = tool_summary(&block("read", json!({"path": "/tmp/repo/src/main.rs"})));

        assert_eq!(
            tool_accessible_name(&summary, tool_status_label(ToolStatus::Ok)).as_ref(),
            "Read main.rs, Completed"
        );

        let summary = tool_summary(&block("bash", json!({"command": "cargo test\nnext"})));

        assert_eq!(
            tool_accessible_name(&summary, tool_status_label(ToolStatus::Running)).as_ref(),
            "Run cargo test, Running"
        );
    }

    #[test]
    fn empty_output_reads_as_a_state_label_not_a_parenthesised_aside() {
        let mut running = block("bash", json!({"command": "sleep 1"}));
        running.status = ToolStatus::Running;

        assert_eq!(output_text(&running).as_ref(), RUNNING_PLACEHOLDER);
        assert_eq!(
            output_text(&block("bash", json!({"command": "true"}))).as_ref(),
            NO_OUTPUT_PLACEHOLDER
        );
    }
}
