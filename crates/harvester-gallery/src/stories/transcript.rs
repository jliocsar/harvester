use gpui::{div, prelude::*, AnyElement, App, Div, Entity, Window};
use gpui_component::button::Button;
use gpui_component::Sizable as _;
use harvester::ui::{empty_pane_hint, Tone};

use super::{caption, story_column, story_row, transcript_panel, FLAGSHIP_GROUP};
use crate::fixtures::TRANSCRIPT_SOURCE_LABELS;
use crate::runtime::{StoryRuntime, REPLAY_SPEED_LABELS};
use crate::story::{KnobSpec, KnobValue, Preset, Story, StoryScene};

pub const PLAYING_KNOB: &str = "playing";
pub const SPEED_KNOB: &str = "speed";
pub const STREAMING_TRANSCRIPT_STORY: &str = "Transcript — streaming";

const SOURCE_KNOB: &str = "source";

const RENDERED_TRANSCRIPT_STORY: &str = "Transcript — rendered";
const FIRST_TRANSCRIPT_SOURCE_INDEX: usize = 0;
const DEFAULT_SPEED_INDEX: usize = 1;
const FAST_FORWARD_SPEED_INDEX: usize = 2;
const RESTART_BUTTON_ID: &str = "story-replay-restart";
const REPLAY_DONE_NOTE: &str = "Replay finished — press restart";
const REPLAY_PAUSED_NOTE: &str = "Paused — flip the playing knob to resume";
const RENDERED_CAPTION: &str =
    "Tool block headers are tab stops — click, Enter, or Space toggles one";

pub fn stories() -> Vec<Story> {
    vec![
        Story {
            group: FLAGSHIP_GROUP,
            name: RENDERED_TRANSCRIPT_STORY,
            knobs: vec![KnobSpec::choice(
                SOURCE_KNOB,
                TRANSCRIPT_SOURCE_LABELS,
                FIRST_TRANSCRIPT_SOURCE_INDEX,
            )],
            presets: source_presets(),
            render: rendered,
        },
        Story {
            group: FLAGSHIP_GROUP,
            name: STREAMING_TRANSCRIPT_STORY,
            knobs: vec![
                KnobSpec::flag(PLAYING_KNOB, true),
                KnobSpec::choice(SPEED_KNOB, REPLAY_SPEED_LABELS, DEFAULT_SPEED_INDEX),
            ],
            presets: replay_presets(),
            render: streaming,
        },
    ]
}

fn source_presets() -> Vec<Preset> {
    TRANSCRIPT_SOURCE_LABELS
        .iter()
        .enumerate()
        .map(|(index, label)| Preset::new(label, vec![(SOURCE_KNOB, KnobValue::Choice(index))]))
        .collect()
}

fn replay_presets() -> Vec<Preset> {
    vec![
        Preset::new(
            "play",
            vec![
                (PLAYING_KNOB, KnobValue::Bool(true)),
                (SPEED_KNOB, KnobValue::Choice(DEFAULT_SPEED_INDEX)),
            ],
        ),
        Preset::new("pause", vec![(PLAYING_KNOB, KnobValue::Bool(false))]),
        Preset::new(
            "fast forward",
            vec![
                (PLAYING_KNOB, KnobValue::Bool(true)),
                (SPEED_KNOB, KnobValue::Choice(FAST_FORWARD_SPEED_INDEX)),
            ],
        ),
    ]
}

fn rendered(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let source_index = scene.knobs.choice(SOURCE_KNOB);
    let toggling_runtime = scene.runtime.clone();

    scene.runtime.update(cx, |runtime, _| {
        runtime.ensure_rendered_source(source_index)
    });

    let runtime = scene.runtime.read(cx);
    let transcript = match runtime.rendered.transcript() {
        Ok(transcript) => transcript,
        Err(failure) => return fixture_failure(failure, cx),
    };
    let body = runtime.rendered.presentation.render(
        transcript,
        &runtime.rendered.scroll,
        move |tool_call_id, _, cx| {
            toggling_runtime.update(cx, |runtime, cx| {
                runtime
                    .rendered
                    .presentation
                    .toggle_tool_block(tool_call_id);
                cx.notify();
            });
        },
        cx,
    );

    story_column()
        .child(transcript_panel(cx).child(body))
        .child(caption(RENDERED_CAPTION, cx))
        .into_any_element()
}

fn streaming(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let playing = scene.knobs.flag(PLAYING_KNOB);
    let speed = REPLAY_SPEED_LABELS
        .get(scene.knobs.choice(SPEED_KNOB))
        .copied()
        .unwrap_or("1x");
    let restarting_runtime = scene.runtime.clone();
    let toggling_runtime = scene.runtime.clone();
    let runtime = scene.runtime.read(cx);
    let transcript = match runtime.replay.transcript() {
        Ok(transcript) => transcript,
        Err(failure) => return fixture_failure(failure, cx),
    };
    let body = runtime.replay.presentation.render(
        transcript,
        &runtime.replay.scroll,
        move |tool_call_id, _, cx| {
            toggling_runtime.update(cx, |runtime, cx| {
                runtime.replay.presentation.toggle_tool_block(tool_call_id);
                cx.notify();
            });
        },
        cx,
    );

    story_column()
        .child(replay_controls(
            runtime,
            playing,
            speed,
            restarting_runtime,
            cx,
        ))
        .child(transcript_panel(cx).child(body))
        .into_any_element()
}

fn replay_controls(
    runtime: &StoryRuntime,
    playing: bool,
    speed: &'static str,
    restarting_runtime: Entity<StoryRuntime>,
    cx: &App,
) -> Div {
    let (applied, total) = runtime.replay.progress();
    let finished = runtime.replay.finished();
    let restart = Button::new(RESTART_BUTTON_ID)
        .label("Restart")
        .outline()
        .small()
        .on_click(move |_, _, cx| {
            restarting_runtime.update(cx, |runtime, cx| {
                runtime.replay.restart();
                cx.notify();
            });
        });

    story_row()
        .child(restart)
        .child(caption(format!("{applied} / {total} events · {speed}"), cx))
        .child(replay_state_note(playing, finished, cx))
}

fn replay_state_note(playing: bool, finished: bool, cx: &App) -> Div {
    if finished {
        return caption(REPLAY_DONE_NOTE, cx);
    }

    if !playing {
        return caption(REPLAY_PAUSED_NOTE, cx);
    }

    div()
}

fn fixture_failure(failure: &str, cx: &App) -> AnyElement {
    transcript_panel(cx)
        .child(empty_pane_hint(Tone::Failed, failure, cx))
        .into_any_element()
}
