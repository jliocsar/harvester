use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use harvester_client_core::transcript::Transcript;
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
const TEXT_CONTENT_TYPE: &str = "text";
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
            1 => TranscriptSource::ToolRun,
            2 => TranscriptSource::FailedTool,
            3 => TranscriptSource::MixedSession,
            _ => TranscriptSource::MarkdownAnswer,
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

fn message_text(message: &Value) -> String {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return String::new();
    };

    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some(TEXT_CONTENT_TYPE))
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<&str>>()
        .join("")
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
