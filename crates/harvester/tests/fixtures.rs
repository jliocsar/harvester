use std::fs;
use std::path::PathBuf;

use harvester_client_core::{Item, Role, ToolBlock, ToolStatus, Transcript};
use harvester_proto::{CR, LF};
use serde_json::Value;

const FIXTURE_NAMES: &[&str] = &["streaming.jsonl", "tools.jsonl", "mixed.jsonl"];
const BUILTIN_TOOL_NAMES: &[&str] = &["bash", "read", "write", "edit", "grep", "find", "ls"];

fn fixture_events(name: &str) -> Vec<Value> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let source = fs::read_to_string(&path).expect("readable fixture file");

    assert!(!source.contains(CR as char), "{name} must be LF-only");

    source
        .split(LF as char)
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).expect("every fixture line is one JSON value"))
        .collect()
}

fn folded(events: &[Value]) -> Transcript {
    let mut transcript = Transcript::new(&[]);

    for event in events {
        transcript.apply(event);
    }

    transcript
}

fn tool_blocks(transcript: &Transcript) -> Vec<&ToolBlock> {
    transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Tool(block) => Some(block.as_ref()),
            Item::Message(_) | Item::Failure(_) => None,
        })
        .collect()
}

fn assistant_texts(transcript: &Transcript) -> Vec<&str> {
    transcript
        .items()
        .iter()
        .filter_map(|item| match item {
            Item::Message(message) if message.role == Role::Assistant => {
                Some(message.text.as_str())
            }
            Item::Message(_) | Item::Tool(_) | Item::Failure(_) => None,
        })
        .collect()
}

#[test]
fn every_fixture_folds_into_a_non_empty_transcript() {
    for name in FIXTURE_NAMES {
        let events = fixture_events(name);
        let transcript = folded(&events);

        assert!(!events.is_empty(), "{name} has no events");
        assert!(
            !transcript.items().is_empty(),
            "{name} folded into no items"
        );
        assert_eq!(transcript.streaming(), None, "{name} ends mid-stream");
        assert!(!transcript.is_busy(), "{name} never settled");
    }
}

#[test]
fn the_streaming_fixture_grows_its_answer_before_it_lands() {
    let events = fixture_events("streaming.jsonl");
    let mut transcript = Transcript::new(&[]);
    let mut partials: Vec<String> = Vec::new();

    for event in &events {
        transcript.apply(event);

        if let Some(partial) = transcript.streaming() {
            partials.push(partial.to_string());
        }
    }

    assert!(
        partials.len() > 4,
        "a replay needs several frames, got {}",
        partials.len()
    );
    assert!(
        partials
            .windows(2)
            .all(|pair| pair[1].len() >= pair[0].len()),
        "each frame carries the whole answer so far"
    );
    assert!(
        partials.last().expect("a last frame").len() > partials[0].len(),
        "the answer never grew"
    );

    let answers = assistant_texts(&transcript);

    assert_eq!(answers.len(), 1);
    assert!(answers[0].starts_with(partials[0].as_str()));
    assert!(answers[0].contains("## "));
    assert!(answers[0].contains("```python"));
    assert!(answers[0].contains("\n- "));
}

#[test]
fn the_tools_fixture_covers_every_label_path() {
    let transcript = folded(&fixture_events("tools.jsonl"));
    let blocks = tool_blocks(&transcript);
    let names: Vec<&str> = blocks
        .iter()
        .map(|block| block.tool_name.as_str())
        .collect();

    for expected in ["bash", "read", "write", "edit", "grep", "ls"] {
        assert!(
            names.contains(&expected),
            "no {expected} call in the fixture"
        );
    }

    assert!(
        names.iter().any(|name| !BUILTIN_TOOL_NAMES.contains(name)),
        "no unrecognized tool for the fallback label"
    );
    assert!(
        blocks.iter().any(|block| block.status == ToolStatus::Error),
        "no failed tool execution"
    );
    assert!(
        blocks
            .iter()
            .any(|block| block.tool_name == "edit" && block.line_stats().is_some()),
        "the edit call carries no diff to count lines from"
    );
    assert!(
        blocks
            .iter()
            .any(|block| block.tool_name == "write" && block.line_stats().is_some()),
        "the write call carries no content to count lines from"
    );
    assert!(
        blocks
            .iter()
            .all(|block| block.status != ToolStatus::Running),
        "every call in the fixture finished"
    );
}

#[test]
fn the_mixed_fixture_interleaves_tools_and_markdown_answers() {
    let transcript = folded(&fixture_events("mixed.jsonl"));
    let blocks = tool_blocks(&transcript);
    let answers = assistant_texts(&transcript);

    assert!(blocks.len() >= 3, "not enough tool runs to interleave");
    assert!(answers.len() >= 3, "not enough answers to interleave");
    assert!(
        answers.iter().any(|answer| answer.contains("```")),
        "no code fence in any answer"
    );
    assert!(
        answers.iter().any(|answer| answer.contains("## ")),
        "no heading in any answer"
    );
    assert!(
        answers.iter().any(|answer| answer.contains("1. ")),
        "no numbered list in any answer"
    );
    assert!(
        transcript
            .items()
            .windows(2)
            .any(|pair| matches!(pair, [Item::Message(_), Item::Tool(_)])),
        "no answer is followed by a tool run"
    );
    assert!(
        transcript
            .items()
            .windows(2)
            .any(|pair| matches!(pair, [Item::Tool(_), Item::Message(_)])),
        "no tool run is followed by an answer"
    );
}
