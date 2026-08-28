use gpui::{div, prelude::*, AnyElement, App, Div, Window};
use harvester::ui::{status_dot_for_tone, Dot, Tone};

use super::{caption, story_column, story_row, tone_at, tone_index, tone_labels, CORE_GROUP};
use crate::story::{KnobSpec, KnobValue, Preset, Story, StoryScene};

const TONE_KNOB: &str = "tone";

const TONE_SWATCHES_STORY: &str = "Tone swatches";
const DOT_STORY: &str = "Dot";
const TONE_SAMPLE_TEXT: &str = "the quick brown fox";
const SOLID_SHAPE_NAME: &str = "solid";
const HOLLOW_SHAPE_NAME: &str = "hollow";

pub fn stories() -> Vec<Story> {
    vec![
        Story::new(
            CORE_GROUP,
            TONE_SWATCHES_STORY,
            Vec::new(),
            Vec::new(),
            tone_swatches,
        ),
        Story::new(
            CORE_GROUP,
            DOT_STORY,
            vec![KnobSpec::choice(
                TONE_KNOB,
                &tone_labels(),
                tone_index(Tone::Live),
            )],
            vec![
                Preset::new(
                    "running session",
                    vec![(TONE_KNOB, KnobValue::Choice(tone_index(Tone::Live)))],
                ),
                Preset::new(
                    "unreachable machine",
                    vec![(TONE_KNOB, KnobValue::Choice(tone_index(Tone::Failed)))],
                ),
            ],
            dot,
        ),
    ]
}

fn tone_swatches(_scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    story_column()
        .children(Tone::ALL.iter().map(|tone| tone_swatch(*tone, cx)))
        .into_any_element()
}

fn tone_swatch(tone: Tone, cx: &App) -> Div {
    story_row()
        .child(status_dot_for_tone(tone, cx))
        .child(div().w_20().text_color(tone.color(cx)).child(tone.label()))
        .child(caption(tone.label(), cx).w_20())
        .child(
            div()
                .text_color(tone.color(cx))
                .child(TONE_SAMPLE_TEXT)
                .text_sm(),
        )
}

fn dot(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));

    story_row()
        .child(status_dot_for_tone(tone, cx))
        .child(caption(derived_dot_label(tone), cx))
        .into_any_element()
}

fn derived_dot_label(tone: Tone) -> String {
    let shape = match tone.dot() {
        Dot::Solid(_) => SOLID_SHAPE_NAME,
        Dot::Hollow(_) => HOLLOW_SHAPE_NAME,
    };

    format!("{shape} · {}", tone.label())
}
