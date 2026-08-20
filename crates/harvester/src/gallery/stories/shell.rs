use gpui::{div, prelude::*, AnyElement, App, Window};
use harvester::ui::{
    chat_header, collapsible_section, sidebar_empty_note, status_dot, statusbar_frame,
};

use super::{caption, dot_at, panel, story_column};
use crate::story::StoryScene;

pub const TITLE_KNOB: &str = "title";
pub const SUBTITLE_KNOB: &str = "subtitle";
pub const SECTION_TITLE_KNOB: &str = "section";
pub const STATUS_TEXT_KNOB: &str = "status";
pub const STATUS_TONE_KNOB: &str = "tone";

pub const SECTION_TITLES: &[&str] = &["machines", "sessions", "stories"];

const SECTION_HEADER_ID: &str = "story-collapsible-section";
const SECTION_BODY_NOTE: &str = "The section body is collapsed by the header above";
const STATUSBAR_TRAILING_NOTE: &str = "harvesterd · connected";

pub fn header(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let title = scene.knobs.text(TITLE_KNOB).to_string();
    let subtitle = scene.knobs.text(SUBTITLE_KNOB).to_string();

    panel(cx)
        .p_0()
        .child(chat_header(title.into(), subtitle.into(), cx))
        .into_any_element()
}

pub fn section(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let runtime = scene.runtime.clone();
    let expanded = scene.runtime.read(cx).section_expanded;
    let title = SECTION_TITLES
        .get(scene.knobs.choice(SECTION_TITLE_KNOB))
        .copied()
        .unwrap_or("machines");
    let header = collapsible_section(
        SECTION_HEADER_ID,
        title,
        expanded,
        move |_, _, cx| {
            runtime.update(cx, |runtime, cx| {
                runtime.section_expanded = !runtime.section_expanded;
                cx.notify();
            });
        },
        cx,
    );

    panel(cx)
        .child(header)
        .when(expanded, |body| {
            body.child(sidebar_empty_note(SECTION_BODY_NOTE, cx))
        })
        .into_any_element()
}

pub fn statusbar(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let text = scene.knobs.text(STATUS_TEXT_KNOB).to_string();
    let tone_index = scene.knobs.choice(STATUS_TONE_KNOB);

    story_column()
        .child(
            panel(cx).p_0().child(
                statusbar_frame(cx)
                    .child(status_dot(dot_at(tone_index, true), cx))
                    .child(div().child(text))
                    .child(div().flex_1())
                    .child(caption(STATUSBAR_TRAILING_NOTE, cx)),
            ),
        )
        .into_any_element()
}
