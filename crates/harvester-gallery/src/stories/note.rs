use gpui::{prelude::*, AnyElement, App, Window};
use harvester::ui::{empty_pane_action, empty_pane_hint, inline_note, Tone};

use super::{caption, panel, story_column, tone_at, tone_index, tone_labels, NOTE_GROUP};
use crate::story::{KnobSpec, KnobValue, Preset, Story, StoryScene};

const TONE_KNOB: &str = "tone";
const TEXT_KNOB: &str = "text";

const INLINE_NOTE_STORY: &str = "inline_note";
const EMPTY_PANE_HINT_STORY: &str = "empty_pane_hint";
const PROGRESS_TEXT: &str = "Starting session…";
const FAILURE_TEXT: &str = "Session exited (code 137)";
const EMPTY_PANE_TEXT: &str = "No session selected";
const HINT_ACTION_ID: &str = "story-empty-pane-action";
const HINT_ACTION_LABEL: &str = "New session";
const HINT_ACTION_CAPTION: &str = "The trailing slot carries the one action the empty state names";

pub fn stories() -> Vec<Story> {
    vec![
        Story::new(
            NOTE_GROUP,
            INLINE_NOTE_STORY,
            vec![
                KnobSpec::choice(TONE_KNOB, &tone_labels(), tone_index(Tone::Muted)),
                KnobSpec::text(TEXT_KNOB, PROGRESS_TEXT),
            ],
            vec![
                Preset::new(
                    "progress",
                    vec![
                        (TONE_KNOB, KnobValue::Choice(tone_index(Tone::Muted))),
                        (TEXT_KNOB, KnobValue::Text(PROGRESS_TEXT.into())),
                    ],
                ),
                Preset::new(
                    "failure",
                    vec![
                        (TONE_KNOB, KnobValue::Choice(tone_index(Tone::Failed))),
                        (TEXT_KNOB, KnobValue::Text(FAILURE_TEXT.into())),
                    ],
                ),
            ],
            inline,
        ),
        Story::new(
            NOTE_GROUP,
            EMPTY_PANE_HINT_STORY,
            vec![
                KnobSpec::choice(TONE_KNOB, &tone_labels(), tone_index(Tone::Muted)),
                KnobSpec::text(TEXT_KNOB, EMPTY_PANE_TEXT),
            ],
            Vec::new(),
            empty_pane,
        ),
    ]
}

fn inline(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    story_column()
        .child(inline_note(tone, text, cx))
        .into_any_element()
}

fn empty_pane(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    story_column()
        .child(
            panel(cx).h_48().child(
                empty_pane_hint(tone, text, cx)
                    .child(empty_pane_action(HINT_ACTION_ID, HINT_ACTION_LABEL)),
            ),
        )
        .child(caption(HINT_ACTION_CAPTION, cx))
        .into_any_element()
}
