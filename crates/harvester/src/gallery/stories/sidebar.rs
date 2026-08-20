use gpui::{div, prelude::*, px, AnyElement, App, Window};
use gpui_component::ActiveTheme as _;
use harvester::ui::{sidebar_empty_note, sidebar_row, sidebar_stacked_row};

use super::{caption, dot_at, story_column};
use crate::story::StoryScene;

pub const LABEL_KNOB: &str = "label";
pub const SELECTED_KNOB: &str = "selected";
pub const DISABLED_KNOB: &str = "disabled";
pub const TONE_KNOB: &str = "dot tone";
pub const SOLID_KNOB: &str = "dot solid";
pub const TITLE_KNOB: &str = "title";
pub const DETAIL_KNOB: &str = "detail";
pub const NOTE_KNOB: &str = "note";
pub const EMPTY_TEXT_KNOB: &str = "text";

pub const EMPTY_NOTE_TEXTS: &[&str] = &["No sessions yet", "No machines yet", "No stories yet"];

const ROW_STORY_ID: &str = "story-sidebar-row";
const STACKED_ROW_STORY_ID: &str = "story-sidebar-stacked-row";
const SIDEBAR_FRAME_WIDTH: f32 = 240.;
const ROW_STATE_CAPTION: &str = "Click state comes from the knobs, not from the mouse";

pub fn row(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let label = scene.knobs.text(LABEL_KNOB).to_string();
    let selected = scene.knobs.flag(SELECTED_KNOB);
    let disabled = scene.knobs.flag(DISABLED_KNOB);

    story_column()
        .child(
            sidebar_frame(cx).child(
                sidebar_row(ROW_STORY_ID)
                    .selected(selected)
                    .disabled(disabled)
                    .child(div().child(label)),
            ),
        )
        .child(caption(ROW_STATE_CAPTION, cx))
        .into_any_element()
}

pub fn stacked_row(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let dot = dot_at(scene.knobs.choice(TONE_KNOB), scene.knobs.flag(SOLID_KNOB));
    let title = scene.knobs.text(TITLE_KNOB).to_string();
    let detail = scene.knobs.optional_text(DETAIL_KNOB);
    let note = scene.knobs.optional_text(NOTE_KNOB);
    let selected = scene.knobs.flag(SELECTED_KNOB);

    sidebar_frame(cx)
        .child(
            sidebar_row(STACKED_ROW_STORY_ID)
                .selected(selected)
                .child(sidebar_stacked_row(dot, title.into(), detail, note, cx)),
        )
        .into_any_element()
}

pub fn empty_note(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let text = EMPTY_NOTE_TEXTS
        .get(scene.knobs.choice(EMPTY_TEXT_KNOB))
        .copied()
        .unwrap_or("No sessions yet");

    sidebar_frame(cx)
        .child(sidebar_empty_note(text, cx))
        .into_any_element()
}

fn sidebar_frame(cx: &App) -> gpui::Div {
    div()
        .w(px(SIDEBAR_FRAME_WIDTH))
        .py_2()
        .rounded_md()
        .bg(cx.theme().sidebar)
        .border_1()
        .border_color(cx.theme().sidebar_border)
        .flex()
        .flex_col()
}
