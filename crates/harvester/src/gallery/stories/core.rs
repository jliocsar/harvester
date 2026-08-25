use gpui::{div, prelude::*, AnyElement, App, Div, Window};
use harvester::ui::{status_dot_for_tone, Dot, Tone};

use super::{caption, story_column, story_row, tone_at, TONE_LABELS};
use crate::story::StoryScene;

pub const TONE_KNOB: &str = "tone";

const TONE_SAMPLE_TEXT: &str = "the quick brown fox";
const SOLID_SHAPE_NAME: &str = "solid";
const HOLLOW_SHAPE_NAME: &str = "hollow";

pub fn tone_swatches(_scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    story_column()
        .children(
            TONE_LABELS
                .iter()
                .enumerate()
                .map(|(index, label)| tone_swatch(index, label, cx)),
        )
        .into_any_element()
}

fn tone_swatch(index: usize, label: &'static str, cx: &App) -> Div {
    let tone = tone_at(index);

    story_row()
        .child(status_dot_for_tone(tone, cx))
        .child(div().w_20().text_color(tone.color(cx)).child(label))
        .child(caption(tone.label(), cx).w_20())
        .child(
            div()
                .text_color(tone.color(cx))
                .child(TONE_SAMPLE_TEXT)
                .text_sm(),
        )
}

pub fn dot(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
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
