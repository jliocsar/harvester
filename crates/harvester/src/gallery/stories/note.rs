use gpui::{prelude::*, px, AnyElement, App, Window};
use harvester::ui::{empty_pane_hint, inline_note};

use super::{panel, story_column, tone_at};
use crate::story::StoryScene;

pub const TONE_KNOB: &str = "tone";
pub const TEXT_KNOB: &str = "text";

const HINT_PANEL_HEIGHT: f32 = 160.;

pub fn inline(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    story_column()
        .child(inline_note(tone, text, cx))
        .into_any_element()
}

pub fn empty_pane(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let text = scene.knobs.text(TEXT_KNOB).to_string();

    panel(cx)
        .h(px(HINT_PANEL_HEIGHT))
        .child(empty_pane_hint(tone, text, cx))
        .into_any_element()
}
