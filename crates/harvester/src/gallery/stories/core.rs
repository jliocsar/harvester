use gpui::{div, prelude::*, px, AnyElement, App, Div, Window};
use harvester::ui::{status_dot, Dot};

use super::{caption, dot_at, story_column, story_row, tone_at, TONE_LABELS};
use crate::story::StoryScene;

pub const TONE_KNOB: &str = "tone";
pub const SOLID_KNOB: &str = "solid";

const TONE_NAME_WIDTH: f32 = 70.;
const TONE_SAMPLE_TEXT: &str = "the quick brown fox";

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
        .child(status_dot(Dot::Solid(tone), cx))
        .child(status_dot(Dot::Hollow(tone), cx))
        .child(
            div()
                .w(px(TONE_NAME_WIDTH))
                .text_color(tone.color(cx))
                .child(label),
        )
        .child(
            div()
                .text_color(tone.color(cx))
                .child(TONE_SAMPLE_TEXT)
                .text_xs(),
        )
}

pub fn dot(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone_index = scene.knobs.choice(TONE_KNOB);
    let solid = scene.knobs.flag(SOLID_KNOB);
    let label = dot_label(tone_index, solid);

    story_row()
        .child(status_dot(dot_at(tone_index, solid), cx))
        .child(caption(label, cx))
        .into_any_element()
}

fn dot_label(tone_index: usize, solid: bool) -> String {
    let shape = if solid { "solid" } else { "hollow" };
    let tone = TONE_LABELS.get(tone_index).copied().unwrap_or("muted");

    format!("{shape} · {tone}")
}
