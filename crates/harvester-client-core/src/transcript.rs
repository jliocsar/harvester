//! The client's model of one session's conversation: pi's history entries and
//! its live event stream, reduced to a list of items a frontend renders.
//!
//! This is where the client "speaks Pi" (docs/SPEC.md §Rendering pi) — the
//! shapes of `message_end`, `tool_execution_*` and `get_entries` history, all
//! pinned to pi 0.83.0. It lived in the gpui chat pane, which made every one of
//! those facts reachable only through a window and testable only by hand; the
//! two that most needed a test — out-of-order tool correlation and the output
//! cap's UTF-8 boundary — are pinned at the bottom of this file.
//!
//! Deliberately holds no view state. Whether a tool block is expanded is the
//! frontend's business, and a frontend keying that by `tool_call_id` (rather
//! than by position) keeps it across a resync, which replaces every item.

use std::collections::HashMap;

use serde_json::Value;

/// Cap on kept tool output per block, tail-kept (`tool_execution_update`
/// snapshots are replaced wholesale, so capping bounds client memory only —
/// the full output lives in the session file on the machine).
const TOOL_OUTPUT_MAX_BYTES: usize = 64 * 1024;

/// Who said a rendered message. Tool results are [`ToolBlock`]s, not messages;
/// system entries are skipped.
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

/// Where a tool call is in its lifecycle. A history `toolCall` without a
/// matching `toolResult` stays `Running` — either the tool is genuinely in
/// flight (attach mid-turn) or the session died with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Error,
}

/// One tool call, correlated by `toolCallId` — pi runs tool executions in
/// parallel and their ends arrive out of order (verified live against pi
/// 0.83.0), so position never identifies a block.
#[derive(Debug, Clone)]
pub struct ToolBlock {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: Value,
    /// Latest output snapshot, tail-capped at [`TOOL_OUTPUT_MAX_BYTES`].
    pub output: String,
    pub output_truncated: bool,
    /// Edit's pre-rendered numbered diff (`result.details.diff`).
    pub diff: Option<String>,
    pub status: ToolStatus,
}

impl ToolBlock {
    /// One of the call's arguments, as a string.
    pub fn arg(&self, key: &str) -> Option<&str> {
        self.args.get(key)?.as_str()
    }

    /// "+N −N" for a label: write counts the content argument it was called
    /// with (known from the start), edit counts its diff's +/− lines (known
    /// only once the result lands). Pi's diff format is
    /// `" 1 ctx" / "-2 old" / "+2 new"`, so the marker is the first byte. Only
    /// those two tools: no other pi 0.83.0 built-in reports line changes, and
    /// counting a `+` in some future tool's output would be a lie in a label.
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

/// The conversation in render order: messages interleaved with tool blocks (pi
/// emits the assistant `message_end` before its tool executions start, so plain
/// push order is the true order).
#[derive(Debug, Clone)]
pub enum Item {
    Message(Message),
    Tool(ToolBlock),
}

/// One session's conversation. Built from history, advanced by live events.
#[derive(Debug, Default)]
pub struct Transcript {
    items: Vec<Item>,
    streaming: Option<String>,
    busy: bool,
}

impl Transcript {
    /// From `get_entries` history: user/assistant text plus tool blocks rebuilt
    /// from assistant `toolCall` blocks matched to their `toolResult` messages
    /// — the same data the live events carry (verified against pi 0.83.0
    /// session captures).
    pub fn new(entries: &[Value]) -> Self {
        Transcript {
            items: items_from_entries(entries),
            streaming: None,
            busy: false,
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    /// The in-flight assistant message: every `message_update` carries the
    /// whole partial, so this is a replacement, never an accumulation.
    pub fn streaming(&self) -> Option<&str> {
        self.streaming.as_deref()
    }

    /// A turn is running. `agent_settled` ends it — not `agent_end`, after
    /// which retries, compaction and queued follow-ups may still run.
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// A resync answered (docs/DAEMON.md §Events): the entries are
    /// authoritative and replace everything rendered. The in-flight partial
    /// goes with them — the next `message_update` carries the whole partial
    /// again. `busy` survives: the turn's state is the event stream's to say,
    /// and a snapshot fetched mid-turn does not end it.
    pub fn replace_history(&mut self, entries: &[Value]) {
        self.items = items_from_entries(entries);
        self.streaming = None;
    }

    /// The user's prompt, shown before pi echoes anything back, which also
    /// starts the turn: `agent_start` may be milliseconds away, and the input
    /// must not look idle in between.
    pub fn push_user_prompt(&mut self, text: String) {
        self.items.push(Item::Message(Message {
            role: Role::User,
            text,
        }));
        self.busy = true;
    }

    /// No turn is running: `agent_settled`, or a `session_exit` that ended one
    /// the hard way. The partial goes with it — whatever was streaming will
    /// never be completed now.
    pub fn end_turn(&mut self) {
        self.busy = false;
        self.streaming = None;
    }

    /// Advance by one pi event. `false` means nothing here changed — the event
    /// belongs to a slice this model does not cover yet (retries, compaction,
    /// queue), and the frontend need not redraw.
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
            "agent_settled" => self.end_turn(),
            _ => return false,
        }

        true
    }

    fn apply_message_update(&mut self, event: &Value) {
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
    }

    /// A new running block. Pi emits the assistant `message_end` first, so the
    /// block lands after the text that announced it.
    fn apply_tool_start(&mut self, event: &Value) {
        let Some(tool_call_id) = event.get("toolCallId").and_then(Value::as_str) else {
            return;
        };

        self.items.push(Item::Tool(ToolBlock {
            tool_call_id: tool_call_id.to_string(),
            tool_name: event
                .get("toolName")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string(),
            args: event.get("args").cloned().unwrap_or(Value::Null),
            output: String::new(),
            output_truncated: false,
            diff: None,
            status: ToolStatus::Running,
        }));
    }

    /// `partialResult` is the accumulated snapshot — replace the block's
    /// output, never append.
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

    /// Final output, error flag, and (for edit) the pre-rendered numbered diff
    /// in `result.details.diff`.
    fn apply_tool_end(&mut self, event: &Value) {
        let is_error = event
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let result = event.get("result");

        let Some(block) = self.tool_block_for(event) else {
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
    /// back, since live events concern recent blocks.
    fn tool_block_for(&mut self, event: &Value) -> Option<&mut ToolBlock> {
        let tool_call_id = event.get("toolCallId").and_then(Value::as_str)?;

        self.items.iter_mut().rev().find_map(|item| match item {
            Item::Tool(block) if block.tool_call_id == tool_call_id => Some(block),
            _ => None,
        })
    }
}

fn items_from_entries(entries: &[Value]) -> Vec<Item> {
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
            items.push(Item::Message(Message { role, text }));
        }

        if role != Role::Assistant {
            continue;
        }

        let Some(Value::Array(blocks)) = message.get("content") else {
            continue;
        };

        for block in blocks {
            if block.get("type").and_then(Value::as_str) != Some("toolCall") {
                continue;
            }

            if let Some(tool_block) = tool_block_from_history(block, &tool_results) {
                items.push(Item::Tool(tool_block));
            }
        }
    }

    items
}

/// One history `toolCall {id, name, arguments}` → a block, filled from its
/// `toolResult` when one exists. Without a result it stays `Running`: either
/// the tool is genuinely in flight (attach mid-turn — the live
/// `tool_execution_*` events find this block by id) or the session died with it.
fn tool_block_from_history(
    block: &Value,
    tool_results: &HashMap<&str, &Value>,
) -> Option<ToolBlock> {
    let tool_call_id = block.get("id").and_then(Value::as_str)?;
    let mut tool_block = ToolBlock {
        tool_call_id: tool_call_id.to_string(),
        tool_name: block
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool")
            .to_string(),
        args: block.get("arguments").cloned().unwrap_or(Value::Null),
        output: String::new(),
        output_truncated: false,
        diff: None,
        status: ToolStatus::Running,
    };

    let Some(result) = tool_results.get(tool_call_id) else {
        return Some(tool_block);
    };
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

    Some(tool_block)
}

/// The joined text of a result-like object's `content` blocks (live results and
/// history `toolResult` messages share the shape), tail-capped at
/// [`TOOL_OUTPUT_MAX_BYTES`]. The cut is moved forward to a `char` boundary:
/// the cap is a byte count, and pi's output is arbitrary UTF-8 — slicing
/// mid-character panics.
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

/// Text of a pi message: text blocks verbatim, thinking and tool calls skipped
/// (tool calls render as blocks, not as inline text).
fn message_text(message: &Value) -> String {
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

/// Pi 0.83.0's wire shapes, pinned. Every fixture here is the shape the client
/// actually receives — the point of the module living outside the gpui crate is
/// that these run without a window.
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
                Item::Message(_) => None,
            })
            .collect()
    }

    /// The correlation the whole design rests on: pi runs tools in parallel and
    /// their ends arrive in completion order, not start order (verified live on
    /// 0.83.0). Position identifies nothing; the id identifies everything.
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
        // Pi mints unique ids, so this is a contract check rather than an
        // observed case: searching from the back means a repeat can only ever
        // affect the most recent block, never rewrite history.
        let mut transcript = Transcript::default();

        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_start("call-a", "bash"));
        transcript.apply(&tool_end("call-a", "second"));

        let blocks = tool_blocks(&transcript);

        assert_eq!(blocks[0].output, "");
        assert_eq!(blocks[1].output, "second");
    }

    /// The cap is a byte count and pi's output is arbitrary UTF-8, so the cut
    /// lands mid-character sooner or later. Slicing there panics — this is the
    /// case that could only be hit in production before the move.
    #[test]
    fn the_output_cap_cuts_on_a_character_boundary() {
        // Every char is 4 bytes, so no multiple of the cap is a boundary.
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
        // `message_update` carries the whole partial every time; appending
        // would render the answer twice over.
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

    /// Attach mid-turn: the call is in the history, the result is not. The
    /// block must stay running so the live `tool_execution_end` can still find
    /// it by id.
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
}
