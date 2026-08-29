use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use harvester_client_core::transcript::{message_text, Transcript};
use harvester_proto::{CR, LF};
use serde_json::Value;

pub const APP_CRATE_FIXTURES_DIR: &str = "../harvester/fixtures";
pub const STREAMING_FIXTURE_NAME: &str = "streaming.jsonl";
pub const TOOLS_FIXTURE_NAME: &str = "tools.jsonl";
pub const MIXED_FIXTURE_NAME: &str = "mixed.jsonl";

pub const TRANSCRIPT_SOURCE_LABELS: &[&str] = &[
    "markdown-heavy answer",
    "tool run",
    "failed tool",
    "mixed session",
];

const MESSAGE_END_EVENT: &str = "message_end";
const USER_ROLE: &str = "user";
const TOOL_EXECUTION_END_EVENT: &str = "tool_execution_end";

pub enum TranscriptSource {
    MarkdownAnswer,
    ToolRun,
    FailedTool,
    MixedSession,
}

impl TranscriptSource {
    pub fn at(index: usize) -> Self {
        match index {
            1 => Self::ToolRun,
            2 => Self::FailedTool,
            3 => Self::MixedSession,
            _ => Self::MarkdownAnswer,
        }
    }
}

pub struct FixtureLibrary {
    streaming: FixtureEvents,
    tools: FixtureEvents,
    mixed: FixtureEvents,
}

type FixtureEvents = Result<Vec<Value>, String>;

impl FixtureLibrary {
    pub fn load() -> Self {
        Self {
            streaming: load_fixture(STREAMING_FIXTURE_NAME),
            tools: load_fixture(TOOLS_FIXTURE_NAME),
            mixed: load_fixture(MIXED_FIXTURE_NAME),
        }
    }

    pub fn streaming_events(&self) -> Result<&[Value], String> {
        borrowed(&self.streaming)
    }

    pub fn failure_summary(&self) -> Option<String> {
        for fixture in [&self.streaming, &self.tools, &self.mixed] {
            if let Err(failure) = fixture {
                return Some(failure.clone());
            }
        }

        None
    }

    pub fn transcript(&self, source: TranscriptSource) -> Result<Transcript, String> {
        match source {
            TranscriptSource::MarkdownAnswer => Ok(fold_events(borrowed(&self.streaming)?.iter())),
            TranscriptSource::ToolRun => Ok(fold_events(borrowed(&self.tools)?.iter())),
            TranscriptSource::FailedTool => {
                Ok(fold_events(failed_tool_events(borrowed(&self.tools)?)))
            }
            TranscriptSource::MixedSession => Ok(fold_events(borrowed(&self.mixed)?.iter())),
        }
    }
}

fn borrowed(fixture: &FixtureEvents) -> Result<&[Value], String> {
    match fixture {
        Ok(events) => Ok(events.as_slice()),
        Err(failure) => Err(failure.clone()),
    }
}

pub fn load_fixture(name: &str) -> FixtureEvents {
    read_events(&fixture_path(name))
}

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join(APP_CRATE_FIXTURES_DIR)
        .join(name)
}

pub fn fold_events<'a>(events: impl Iterator<Item = &'a Value>) -> Transcript {
    let mut transcript = Transcript::new(&[]);

    for event in events {
        apply_event(&mut transcript, event);
    }

    transcript
}

pub fn apply_event(transcript: &mut Transcript, event: &Value) {
    if let Some(prompt) = user_prompt_text(event) {
        transcript.push_user_prompt(prompt);

        return;
    }

    transcript.apply(event);
}

fn read_events(path: &Path) -> Result<Vec<Value>, String> {
    let source = fs::read_to_string(path)
        .map_err(|error| format!("fixture {} is unreadable: {error}", path.display()))?;

    parse_events(&source, path)
}

fn parse_events(source: &str, path: &Path) -> Result<Vec<Value>, String> {
    let mut events = Vec::new();

    for (offset, line) in source.split(LF as char).enumerate() {
        let line = line.strip_suffix(CR as char).unwrap_or(line);

        if line.is_empty() {
            continue;
        }

        let event = serde_json::from_str(line).map_err(|error| {
            format!(
                "fixture {}:{} is not one pi event: {error}",
                path.display(),
                offset + 1
            )
        })?;

        events.push(event);
    }

    Ok(events)
}

fn user_prompt_text(event: &Value) -> Option<String> {
    if event.get("type").and_then(Value::as_str)? != MESSAGE_END_EVENT {
        return None;
    }

    let message = event.get("message")?;

    if message.get("role").and_then(Value::as_str)? != USER_ROLE {
        return None;
    }

    let text = message_text(message);

    if text.is_empty() {
        return None;
    }

    Some(text)
}

fn failed_tool_events(events: &[Value]) -> impl Iterator<Item = &Value> {
    let failed_ids = failed_tool_call_ids(events);

    events.iter().filter(move |event| {
        tool_call_id(event).is_some_and(|call_id| failed_ids.contains(call_id))
    })
}

fn failed_tool_call_ids(events: &[Value]) -> HashSet<&str> {
    events
        .iter()
        .filter(|event| event.get("type").and_then(Value::as_str) == Some(TOOL_EXECUTION_END_EVENT))
        .filter(|event| event.get("isError").and_then(Value::as_bool) == Some(true))
        .filter_map(|event| tool_call_id(event))
        .collect()
}

fn tool_call_id(event: &Value) -> Option<&str> {
    event.get("toolCallId").and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    fn parsed(source: &str) -> Result<Vec<Value>, String> {
        parse_events(source, Path::new(STREAMING_FIXTURE_NAME))
    }

    fn user_message_end(content: Value) -> Value {
        json!({ "type": MESSAGE_END_EVENT, "message": { "role": USER_ROLE, "content": content } })
    }

    #[test]
    fn parsing_splits_on_lf_and_drops_the_blank_lines_a_trailing_newline_leaves() {
        let events = parsed("{\"type\":\"a\"}\n{\"type\":\"b\"}\n").expect("two events");

        assert_eq!(events.len(), 2);
        assert_eq!(events[1]["type"], "b");
    }

    #[test]
    fn parsing_strips_one_trailing_carriage_return_rather_than_failing_on_it() {
        let events = parsed("{\"type\":\"a\"}\r\n{\"type\":\"b\"}\r\n").expect("two events");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "a");
    }

    #[test]
    fn a_malformed_line_names_the_fixture_and_its_one_based_line_number() {
        let failure = parsed("{\"type\":\"a\"}\nnot json\n").expect_err("a parse failure");

        assert!(failure.contains(STREAMING_FIXTURE_NAME), "{failure}");
        assert!(failure.contains(":2 "), "{failure}");
    }

    #[test]
    fn a_user_prompt_joins_its_text_blocks_the_way_the_app_does() {
        let event = user_message_end(json!([
            { "type": "text", "text": "first" },
            { "type": "thinking", "text": "not prose" },
            { "type": "text", "text": "second" },
        ]));

        assert_eq!(user_prompt_text(&event).as_deref(), Some("first\n\nsecond"));
    }

    #[test]
    fn a_user_prompt_sent_as_a_bare_string_still_reads_as_one() {
        let event = user_message_end(json!("just a string"));

        assert_eq!(user_prompt_text(&event).as_deref(), Some("just a string"));
    }

    #[test]
    fn only_a_user_message_end_carrying_text_is_a_prompt() {
        let assistant = json!({
            "type": MESSAGE_END_EVENT,
            "message": { "role": "assistant", "content": [{ "type": "text", "text": "hi" }] },
        });
        let other_event = json!({ "type": "message_update", "message": { "role": USER_ROLE } });
        let empty = user_message_end(json!([]));

        assert_eq!(user_prompt_text(&assistant), None);
        assert_eq!(user_prompt_text(&other_event), None);
        assert_eq!(user_prompt_text(&empty), None);
    }

    #[test]
    fn the_failed_tool_filter_keeps_every_event_naming_a_failing_call() {
        let events = vec![
            json!({ "type": "tool_execution_start", "toolCallId": "a", "toolName": "bash" }),
            json!({ "type": TOOL_EXECUTION_END_EVENT, "toolCallId": "a", "isError": true }),
            json!({ "type": "tool_execution_start", "toolCallId": "b", "toolName": "read" }),
            json!({ "type": TOOL_EXECUTION_END_EVENT, "toolCallId": "b" }),
            json!({ "type": MESSAGE_END_EVENT }),
        ];
        let kept: Vec<&str> = failed_tool_events(&events)
            .filter_map(tool_call_id)
            .collect();

        assert_eq!(kept, ["a", "a"]);
    }

    #[test]
    fn an_unknown_source_index_falls_back_to_the_markdown_answer() {
        assert!(matches!(
            TranscriptSource::at(TRANSCRIPT_SOURCE_LABELS.len()),
            TranscriptSource::MarkdownAnswer
        ));
        assert!(matches!(
            TranscriptSource::at(2),
            TranscriptSource::FailedTool
        ));
    }
}
