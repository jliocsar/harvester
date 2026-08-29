use harvester_proto::PI_AGENT_SETTLED;
use serde_json::Value;

const TOOL_OUTPUT_MAX_BYTES: usize = 64 * 1024;

const UNNAMED_TOOL_NAME: &str = "tool";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

#[derive(Debug, Clone)]
pub struct ToolBlock {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    pub output: String,
    pub output_truncated: bool,
    pub diff: Option<String>,
    pub status: ToolStatus,
}

impl ToolBlock {
    pub fn running(tool_call_id: String, tool_name: String, args: Value) -> Self {
        Self {
            tool_call_id,
            tool_name,
            args,
            output: String::new(),
            output_truncated: false,
            diff: None,
            status: ToolStatus::Running,
        }
    }

    pub fn arg(&self, key: &str) -> Option<&str> {
        self.args.get(key)?.as_str()
    }

    pub fn line_stats(&self) -> Option<(usize, usize)> {
        match self.tool_name.as_str() {
            "write" => Some((self.arg("content")?.lines().count(), 0)),
            "edit" => {
                let diff = self.diff.as_deref()?;
                let added = diff.lines().filter(|line| line.starts_with('+')).count();
                let removed = diff.lines().filter(|line| line.starts_with('-')).count();

                Some((added, removed))
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Item {
    Message(Message),
    Tool(ToolBlock),
    Failure(String),
}

#[derive(Debug, Default)]
pub struct Transcript {
    items: Vec<Item>,
    history_len: usize,
    streaming: Option<String>,
    busy: bool,
}

impl Transcript {
    pub fn new(entries: &[Value]) -> Self {
        let items = items_from_entries(entries);

        Self {
            history_len: items.len(),
            items,
            streaming: None,
            busy: false,
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn streaming(&self) -> Option<&str> {
        self.streaming.as_deref()
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn replace_history(&mut self, entries: &[Value]) {
        self.items = items_from_entries(entries);
        self.history_len = self.items.len();
        self.streaming = None;
    }

    pub fn extend_history(&mut self, entries: &[Value]) {
        self.items.truncate(self.history_len);
        self.streaming = None;

        extend_items(&mut self.items, entries);
        self.history_len = self.items.len();
    }

    pub fn push_user_prompt(&mut self, text: String) {
        self.items.push(Item::Message(Message {
            role: Role::User,
            text,
        }));
        self.busy = true;
    }

    pub fn end_turn(&mut self) {
        self.busy = false;
        self.streaming = None;
    }

    pub fn fail_turn(&mut self, detail: String) {
        self.end_turn();
        self.items.push(Item::Failure(detail));
    }

    pub fn apply(&mut self, event: &Value) -> bool {
        let Some(event_type) = event.get("type").and_then(Value::as_str) else {
            return false;
        };

        match event_type {
            "agent_start" => self.busy = true,
            "message_update" => self.apply_message_update(event),
            "message_end" => self.apply_message_end(event),
            "tool_execution_start" => self.apply_tool_start(event),
            "tool_execution_update" => self.apply_tool_update(event),
            "tool_execution_end" => self.apply_tool_end(event),
            PI_AGENT_SETTLED => self.end_turn(),
            _ => return false,
        }

        true
    }

    fn apply_message_update(&mut self, event: &Value) {
        let Some(update) = event.get("assistantMessageEvent") else {
            return;
        };

        if let Some(partial) = update.get("partial") {
            let text = message_text(partial);

            if !text.is_empty() {
                self.streaming = Some(text);
            }

            return;
        }

        if update.get("type").and_then(Value::as_str) != Some("text_delta") {
            return;
        }

        let Some(delta) = update.get("delta").and_then(Value::as_str) else {
            return;
        };

        if delta.is_empty() {
            return;
        }

        self.streaming
            .get_or_insert_with(String::new)
            .push_str(delta);
    }

    fn apply_message_end(&mut self, event: &Value) {
        self.streaming = None;

        let Some(message) = event.get("message") else {
            return;
        };

        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }

        let text = message_text(message);

        if !text.is_empty() {
            self.items.push(Item::Message(Message {
                role: Role::Assistant,
                text,
            }));
        }

        if let Some(failure) = message_failure(message) {
            self.items.push(Item::Failure(failure));
        }
    }

    fn apply_tool_start(&mut self, event: &Value) {
        let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            return;
        };

        self.items.push(Item::Tool(ToolBlock::running(
            tool_call_id.to_string(),
            tool_name_or_fallback(event.get("toolName")),
            event.get("args").cloned().unwrap_or(Value::Null),
        )));
    }

    fn apply_tool_update(&mut self, event: &Value) {
        let Some(partial_result) = event.get("partialResult") else {
            return;
        };
        let (output, truncated) = capped_content_text(partial_result);

        if let Some(block) = self.tool_block_for(event) {
            block.output = output;
            block.output_truncated = truncated;
        }
    }

    fn apply_tool_end(&mut self, event: &Value) {
        let is_error = is_error_flag(event);
        let result = event.get("result");

        let Some(block) = self.tool_block_for(event) else {
            return;
        };

        finish_tool_block(block, is_error, result);
    }

    fn tool_block_for(&mut self, event: &Value) -> Option<&mut ToolBlock> {
        let tool_call_id = event.get("toolCallId").and_then(Value::as_str)?;

        find_tool_block(&mut self.items, tool_call_id)
    }
}

fn items_from_entries(entries: &[Value]) -> Vec<Item> {
    let mut items = Vec::new();

    extend_items(&mut items, entries);

    items
}

fn extend_items(items: &mut Vec<Item>, entries: &[Value]) {
    for entry in entries {
        let Some(message) = entry.get("message") else {
            continue;
        };

        match message.get("role").and_then(Value::as_str) {
            Some("toolResult") => apply_tool_result(items, message),
            Some("user") => push_message_items(items, message, Role::User),
            Some("assistant") => push_message_items(items, message, Role::Assistant),
            _ => {}
        }
    }
}

fn apply_tool_result(items: &mut [Item], message: &Value) {
    let Some(tool_call_id) = message.get("toolCallId").and_then(Value::as_str) else {
        return;
    };

    let Some(block) = find_tool_block(items, tool_call_id) else {
        return;
    };

    fill_tool_block_from_result(block, message);
}

fn push_message_items(items: &mut Vec<Item>, message: &Value, role: Role) {
    let text = message_text(message);

    if !text.is_empty() {
        items.push(Item::Message(Message { role, text }));
    }

    if role != Role::Assistant {
        return;
    }

    if let Some(failure) = message_failure(message) {
        items.push(Item::Failure(failure));
    }

    let Some(Value::Array(blocks)) = message.get("content") else {
        return;
    };

    for block in blocks {
        if block.get("type").and_then(Value::as_str) != Some("toolCall") {
            continue;
        }

        if let Some(tool_block) = tool_block_from_history(block) {
            items.push(Item::Tool(tool_block));
        }
    }
}

fn find_tool_block<'a>(items: &'a mut [Item], tool_call_id: &str) -> Option<&'a mut ToolBlock> {
    items.iter_mut().rev().find_map(|item| match item {
        Item::Tool(block) if block.tool_call_id == tool_call_id => Some(block),
        _ => None,
    })
}

fn tool_block_from_history(block: &Value) -> Option<ToolBlock> {
    let tool_call_id = block.get("id").and_then(Value::as_str)?;

    Some(ToolBlock::running(
        tool_call_id.to_string(),
        tool_name_or_fallback(block.get("name")),
        block.get("arguments").cloned().unwrap_or(Value::Null),
    ))
}

fn fill_tool_block_from_result(tool_block: &mut ToolBlock, result: &Value) {
    finish_tool_block(tool_block, is_error_flag(result), Some(result));
}

fn finish_tool_block(tool_block: &mut ToolBlock, is_error: bool, result: Option<&Value>) {
    tool_block.status = if is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Ok
    };

    let Some(result) = result else {
        return;
    };

    (tool_block.output, tool_block.output_truncated) = capped_content_text(result);
    tool_block.diff = extract_diff(result);
}

fn is_error_flag(container: &Value) -> bool {
    container
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn tool_name_or_fallback(name: Option<&Value>) -> String {
    name.and_then(Value::as_str)
        .unwrap_or(UNNAMED_TOOL_NAME)
        .to_string()
}

fn capped_content_text(container: &Value) -> (String, bool) {
    let mut text = String::new();

    if let Some(Value::Array(blocks)) = container.get("content") {
        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("text") {
                continue;
            }

            if let Some(piece) = block.get("text").and_then(Value::as_str) {
                if !text.is_empty() {
                    text.push('\n');
                }

                text.push_str(piece);
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

fn extract_diff(result: &Value) -> Option<String> {
    result
        .get("details")?
        .get("diff")?
        .as_str()
        .map(str::to_string)
}

fn message_failure(message: &Value) -> Option<String> {
    let stop_reason = message.get("stopReason").and_then(Value::as_str)?;

    if !matches!(stop_reason, "error" | "aborted") {
        return None;
    }

    let reason = message
        .get("errorMessage")
        .and_then(Value::as_str)
        .filter(|reason| !reason.trim().is_empty());

    Some(match reason {
        Some(reason) => reason.to_string(),
        None => format!("turn {stop_reason}"),
    })
}

pub fn message_text(message: &Value) -> String {
    let mut pieces: Vec<String> = Vec::new();

    match message.get("content") {
        Some(Value::String(text)) => pieces.push(text.clone()),
        Some(Value::Array(blocks)) => {
            for block in blocks {
                if block.get("type").and_then(Value::as_str) != Some("text") {
                    continue;
                }

                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    if !text.is_empty() {
                        pieces.push(text.to_string());
                    }
                }
            }
        }
        _ => {}
    }

    pieces.join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn text_content(text: &str) -> Value {
        json!([{ "type": "text", "text": text }])
    }

    fn tool_start(tool_call_id: &str, tool_name: &str) -> Value {
        json!({
            "type": "tool_execution_start",
            "toolCallId": tool_call_id,
            "toolName": tool_name,
            "args": { "command": "true" },
        })
    }

    fn tool_end(tool_call_id: &str, output: &str) -> Value {
        json!({
            "type": "tool_execution_end",
            "toolCallId": tool_call_id,
            "result": { "content": text_content(output) },
        })
    }

    fn tool_blocks(transcript: &Transcript) -> Vec<&ToolBlock> {
        transcript
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Tool(block) => Some(block),
                Item::Message(_) | Item::Failure(_) => None,
            })
            .collect()
    }

    fn user_entry(text: &str) -> Value {
        json!({"message": {"role": "user", "content": text}})
    }

    fn assistant_entry(text: &str) -> Value {
        json!({"message": {"role": "assistant", "content": text_content(text)}})
    }

    fn message_texts(transcript: &Transcript) -> Vec<&str> {
        transcript
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Message(message) => Some(message.text.as_str()),
                Item::Tool(_) | Item::Failure(_) => None,
            })
            .collect()
    }

    fn failures(transcript: &Transcript) -> Vec<&str> {
        transcript
            .items()
            .iter()
            .filter_map(|item| match item {
                Item::Failure(detail) => Some(detail.as_str()),
                Item::Message(_) | Item::Tool(_) => None,
            })
            .collect()
    }

    fn failed_turn(reason: &str) -> Value {
        json!({
            "role": "assistant",
            "content": [{"type": "text", "text": ""}],
            "stopReason": "error",
            "errorMessage": reason,
        })
    }

    #[test]
    fn tool_results_land_on_their_own_block_when_ends_arrive_out_of_order() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_start("call-b", "read"));
        transcript.apply(&tool_end("call-b", "b finished first"));
        transcript.apply(&tool_end("call-a", "a finished second"));

        let blocks = tool_blocks(&transcript);

        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].tool_call_id, "call-a");
        assert_eq!(blocks[0].output, "a finished second");
        assert_eq!(blocks[0].status, ToolStatus::Ok);
        assert_eq!(blocks[1].tool_call_id, "call-b");
        assert_eq!(blocks[1].output, "b finished first");
    }

    #[test]
    fn an_event_for_an_unknown_tool_call_changes_nothing() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));

        assert!(transcript.apply(&tool_end("call-ghost", "nowhere to land")));
        assert_eq!(tool_blocks(&transcript)[0].output, "");
    }

    #[test]
    fn the_newest_block_wins_when_a_tool_call_id_repeats() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_end("call-a", "second"));

        let blocks = tool_blocks(&transcript);

        assert_eq!(blocks[0].output, "");
        assert_eq!(blocks[1].output, "second");
    }

    #[test]
    fn the_output_cap_cuts_on_a_character_boundary() {
        let oversized = "🌍".repeat(TOOL_OUTPUT_MAX_BYTES);
        let result = json!({ "content": text_content(&oversized) });

        let (output, truncated) = capped_content_text(&result);

        assert!(truncated);
        assert!(output.len() <= TOOL_OUTPUT_MAX_BYTES);
        assert!(
            oversized.ends_with(&output),
            "the tail is kept, so the last bytes are the ones on screen"
        );
        assert!(
            output.chars().all(|character| character == '🌍'),
            "a cut inside a character would leave a replacement char or panic"
        );
    }

    #[test]
    fn output_at_or_below_the_cap_is_kept_whole() {
        let text = "🌍".repeat(8);
        let result = json!({ "content": text_content(&text) });

        assert_eq!(capped_content_text(&result), (text, false));
    }

    #[test]
    fn a_partial_message_replaces_the_one_before_it() {
        let mut transcript = Transcript::default();

        for partial in ["Hel", "Hello", "Hello there"] {
            transcript.apply(&json!({
                "type": "message_update",
                "assistantMessageEvent": { "partial": { "content": text_content(partial) } },
            }));
        }

        assert_eq!(transcript.streaming(), Some("Hello there"));
    }

    #[test]
    fn bare_deltas_accumulate_when_no_partial_is_sent() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({
            "type": "message_update",
            "usage": { "totalTokens": 0 },
            "assistantMessageEvent": { "type": "text_start", "contentIndex": 1 },
        }));

        for delta in ["Riv", "ers ", "run"] {
            transcript.apply(&json!({
                "type": "message_update",
                "usage": { "totalTokens": 0 },
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "contentIndex": 1,
                    "delta": delta,
                },
            }));
        }

        assert_eq!(transcript.streaming(), Some("Rivers run"));
    }

    #[test]
    fn a_delta_carrying_a_partial_replaces_rather_than_appends() {
        let mut transcript = Transcript::default();

        for _ in 0..2 {
            transcript.apply(&json!({
                "type": "message_update",
                "assistantMessageEvent": {
                    "type": "text_delta",
                    "delta": "Hello",
                    "partial": { "content": text_content("Hello") },
                },
            }));
        }

        assert_eq!(
            transcript.streaming(),
            Some("Hello"),
            "the cumulative partial is authoritative wherever pi still sends it"
        );
    }

    #[test]
    fn thinking_deltas_never_reach_the_streamed_text() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "thinking_delta",
                "contentIndex": 0,
                "delta": "pondering rivers",
            },
        }));

        assert_eq!(transcript.streaming(), None);
    }

    #[test]
    fn a_finished_message_ends_a_bare_delta_stream_too() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "half an ans" },
        }));
        transcript.apply(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": text_content("the whole answer") },
        }));

        assert_eq!(transcript.streaming(), None);
        assert!(matches!(
            transcript.items(),
            [Item::Message(Message { role: Role::Assistant, text })] if text == "the whole answer"
        ));
    }

    #[test]
    fn a_finished_message_becomes_an_item_and_ends_the_stream() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({"type": "agent_start"}));
        transcript.apply(&json!({
            "type": "message_update",
            "assistantMessageEvent": { "partial": { "content": text_content("partial") } },
        }));
        transcript.apply(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": text_content("the whole answer") },
        }));

        assert_eq!(transcript.streaming(), None);
        assert!(transcript.is_busy(), "the turn ends at agent_settled");
        assert!(matches!(
            transcript.items(),
            [Item::Message(Message { role: Role::Assistant, text })] if text == "the whole answer"
        ));
    }

    #[test]
    fn only_agent_settled_ends_the_turn() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({"type": "agent_start"}));
        transcript.apply(&json!({"type": "agent_end"}));

        assert!(transcript.is_busy(), "agent_end is not the end of the turn");

        transcript.apply(&json!({"type": "agent_settled"}));

        assert!(!transcript.is_busy());
    }

    #[test]
    fn an_uncovered_event_reports_that_nothing_changed() {
        let mut transcript = Transcript::default();

        assert!(!transcript.apply(&json!({"type": "compaction_start"})));
        assert!(!transcript.apply(&json!({"no type at all": true})));
    }

    #[test]
    fn history_matches_tool_calls_to_their_results_across_entries() {
        let entries = vec![
            json!({"message": {"role": "user", "content": "run it"}}),
            json!({"message": {"role": "assistant", "content": [
                {"type": "text", "text": "on it"},
                {"type": "toolCall", "id": "call-a", "name": "bash", "arguments": {"command": "ls"}},
            ]}}),
            json!({"message": {"role": "toolResult", "toolCallId": "call-a",
                               "content": text_content("a\nb"),
                               "details": {"diff": " 1 ctx"}}}),
        ];

        let transcript = Transcript::new(&entries);
        let blocks = tool_blocks(&transcript);

        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].status, ToolStatus::Ok);
        assert_eq!(blocks[0].output, "a\nb");
        assert_eq!(blocks[0].diff.as_deref(), Some(" 1 ctx"));
        assert_eq!(blocks[0].arg("command"), Some("ls"));
    }

    #[test]
    fn a_history_call_without_a_result_stays_running() {
        let entries = vec![json!({"message": {"role": "assistant", "content": [
            {"type": "toolCall", "id": "call-a", "name": "bash", "arguments": {}},
        ]}})];

        let mut transcript = Transcript::new(&entries);

        assert_eq!(tool_blocks(&transcript)[0].status, ToolStatus::Running);

        transcript.apply(&tool_end("call-a", "landed after the snapshot"));

        assert_eq!(tool_blocks(&transcript)[0].status, ToolStatus::Ok);
        assert_eq!(
            tool_blocks(&transcript)[0].output,
            "landed after the snapshot"
        );
    }

    #[test]
    fn an_errored_result_is_marked_as_one() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&json!({
            "type": "tool_execution_end",
            "toolCallId": "call-a",
            "isError": true,
            "result": { "content": text_content("command not found") },
        }));

        assert_eq!(tool_blocks(&transcript)[0].status, ToolStatus::Error);
    }

    #[test]
    fn thinking_and_tool_call_blocks_are_not_message_text() {
        let message = json!({"role": "assistant", "content": [
            {"type": "thinking", "text": "let me see"},
            {"type": "text", "text": "here it is"},
            {"type": "toolCall", "id": "call-a", "name": "bash"},
        ]});

        assert_eq!(message_text(&message), "here it is");
    }

    #[test]
    fn a_plain_string_message_is_its_own_text() {
        assert_eq!(
            message_text(&json!({"role": "user", "content": "just a string"})),
            "just a string"
        );
    }

    #[test]
    fn a_resync_replaces_every_item_but_not_the_running_turn() {
        let mut transcript = Transcript::default();

        transcript.push_user_prompt("first".to_string());
        transcript.apply(&json!({
            "type": "message_update",
            "assistantMessageEvent": { "partial": { "content": text_content("half an ans") } },
        }));

        transcript.replace_history(&[json!({"message": {"role": "user", "content": "first"}})]);

        assert_eq!(transcript.items().len(), 1);
        assert_eq!(transcript.streaming(), None, "the snapshot supersedes it");
        assert!(
            transcript.is_busy(),
            "a snapshot fetched mid-turn does not end the turn"
        );
    }

    #[test]
    fn a_prompt_is_rendered_before_pi_says_anything() {
        let mut transcript = Transcript::default();

        transcript.push_user_prompt("fix the tests".to_string());

        assert!(transcript.is_busy());
        assert!(matches!(
            transcript.items(),
            [Item::Message(Message { role: Role::User, text })] if text == "fix the tests"
        ));
    }

    #[test]
    fn write_counts_the_lines_it_was_asked_to_write() {
        let mut transcript = Transcript::default();

        transcript.apply(&json!({
            "type": "tool_execution_start",
            "toolCallId": "call-a",
            "toolName": "write",
            "args": { "content": "one\ntwo\nthree" },
        }));

        assert_eq!(tool_blocks(&transcript)[0].line_stats(), Some((3, 0)));
    }

    #[test]
    fn a_tool_that_does_not_report_line_changes_has_no_stats() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_end("call-a", "output"));

        assert_eq!(tool_blocks(&transcript)[0].line_stats(), None);
    }

    #[test]
    fn edit_counts_the_lines_its_diff_changed() {
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "edit"));
        transcript.apply(&json!({
            "type": "tool_execution_end",
            "toolCallId": "call-a",
            "result": {
                "content": text_content("done"),
                "details": {"diff": " 1 kept\n-2 gone\n+2 new\n+3 also new"},
            },
        }));

        assert_eq!(tool_blocks(&transcript)[0].line_stats(), Some((2, 1)));
    }

    #[test]
    fn a_turn_that_failed_says_why() {
        let mut transcript = Transcript::new(&[user_entry("hi bb")]);

        transcript.apply(&json!({
            "type": "message_end",
            "message": failed_turn("Provided authentication token is expired."),
        }));

        assert_eq!(
            failures(&transcript),
            ["Provided authentication token is expired."]
        );
    }

    #[test]
    fn a_failure_survives_a_reopen() {
        let transcript = Transcript::new(&[
            user_entry("hi bb"),
            json!({"message": failed_turn("Provided authentication token is expired.")}),
        ]);

        assert_eq!(message_texts(&transcript), ["hi bb"]);
        assert_eq!(
            failures(&transcript),
            ["Provided authentication token is expired."]
        );
    }

    #[test]
    fn a_partial_answer_survives_the_failure_that_followed_it() {
        let mut transcript = Transcript::new(&[]);

        transcript.apply(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": text_content("here is half an answer"),
                "stopReason": "error",
                "errorMessage": "stream closed",
            },
        }));

        assert_eq!(message_texts(&transcript), ["here is half an answer"]);
        assert_eq!(failures(&transcript), ["stream closed"]);
    }

    #[test]
    fn a_failure_with_no_message_still_reports_the_stop_reason() {
        let mut transcript = Transcript::new(&[]);

        transcript.apply(&json!({
            "type": "message_end",
            "message": {"role": "assistant", "content": [], "stopReason": "aborted"},
        }));

        assert_eq!(failures(&transcript), ["turn aborted"]);
    }

    #[test]
    fn a_turn_that_ended_well_reports_nothing() {
        let mut transcript = Transcript::new(&[]);

        transcript.apply(&json!({
            "type": "message_end",
            "message": {
                "role": "assistant",
                "content": text_content("all done"),
                "stopReason": "endTurn",
            },
        }));

        assert!(failures(&transcript).is_empty());
    }

    #[test]
    fn a_delta_extends_the_confirmed_history() {
        let mut transcript = Transcript::new(&[user_entry("first"), assistant_entry("answer")]);

        transcript.extend_history(&[user_entry("second")]);

        assert_eq!(message_texts(&transcript), ["first", "answer", "second"]);
    }

    #[test]
    fn a_delta_replaces_what_live_events_had_guessed() {
        let mut transcript = Transcript::new(&[user_entry("first")]);

        transcript.apply(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "content": text_content("streamed answer") },
        }));

        assert_eq!(message_texts(&transcript), ["first", "streamed answer"]);

        transcript.extend_history(&[assistant_entry("streamed answer")]);

        assert_eq!(
            message_texts(&transcript),
            ["first", "streamed answer"],
            "the fetched entry stands in for the streamed one, not beside it"
        );
    }

    #[test]
    fn a_delta_finishes_a_tool_call_left_running_before_the_cursor() {
        let mut transcript = Transcript::new(&[
            json!({"message": {"role": "assistant", "content": [
                {"type": "toolCall", "id": "call-a", "name": "bash", "arguments": {"command": "ls"}},
            ]}}),
        ]);

        assert_eq!(tool_blocks(&transcript).len(), 1);
        assert!(matches!(
            tool_blocks(&transcript)[0].status,
            ToolStatus::Running
        ));

        transcript.extend_history(&[json!({"message": {
            "role": "toolResult", "toolCallId": "call-a", "content": text_content("a\nb"),
        }})]);

        let blocks = tool_blocks(&transcript);

        assert_eq!(blocks.len(), 1, "the call is one block, not two");
        assert!(matches!(blocks[0].status, ToolStatus::Ok));
        assert_eq!(blocks[0].output, "a\nb");
    }

    #[test]
    fn a_delta_leaves_the_turn_running() {
        let mut transcript = Transcript::new(&[user_entry("first")]);

        transcript.apply(&json!({"type": "agent_start"}));
        transcript.extend_history(&[assistant_entry("partial work")]);

        assert!(transcript.is_busy());
    }

    #[test]
    fn an_empty_delta_keeps_the_confirmed_history() {
        let mut transcript = Transcript::new(&[user_entry("first"), assistant_entry("answer")]);

        transcript.extend_history(&[]);

        assert_eq!(message_texts(&transcript), ["first", "answer"]);
    }
}
