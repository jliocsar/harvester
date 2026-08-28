use gpui::{div, prelude::*, AnyElement, App, Window};
use harvester::ui::{
    chat_header, collapsible_section, sidebar_empty_note, status_dot_for_tone, statusbar_frame,
    Tone,
};

use super::{caption, panel, story_column, tone_at, tone_index, tone_labels, SHELL_GROUP};
use crate::story::{KnobSpec, Story, StoryScene};

const TITLE_KNOB: &str = "title";
const SUBTITLE_KNOB: &str = "subtitle";
const SECTION_TITLE_KNOB: &str = "section";
const STATUS_TEXT_KNOB: &str = "status";
const STATUS_TONE_KNOB: &str = "tone";

const CHAT_HEADER_STORY: &str = "chat_header";
const COLLAPSIBLE_SECTION_STORY: &str = "collapsible_section";
const STATUSBAR_FRAME_STORY: &str = "statusbar_frame";
const HEADER_TITLE: &str = "harvester on exe-vm-7fa2";
const HEADER_SUBTITLE: &str = "backend-hardening · pi 0.83.0";
const STATUS_TEXT: &str = "2 machines · 3 sessions";
const SECTION_TITLES: &[&str] = &["Machines", "Sessions", "Stories"];
const SECTION_HEADER_ID: &str = "story-collapsible-section";
const DEFAULT_SECTION_TITLE: &str = "Machines";
const SECTION_BODY_NOTE: &str = "The section body is collapsed by the header above";
const STATUSBAR_TRAILING_NOTE: &str = "harvesterd · connected";

pub fn stories() -> Vec<Story> {
    vec![
        Story::new(
            SHELL_GROUP,
            CHAT_HEADER_STORY,
            vec![
                KnobSpec::text(TITLE_KNOB, HEADER_TITLE),
                KnobSpec::text(SUBTITLE_KNOB, HEADER_SUBTITLE),
            ],
            Vec::new(),
            header,
        ),
        Story::new(
            SHELL_GROUP,
            COLLAPSIBLE_SECTION_STORY,
            vec![KnobSpec::choice(SECTION_TITLE_KNOB, SECTION_TITLES, 0)],
            Vec::new(),
            section,
        ),
        Story::new(
            SHELL_GROUP,
            STATUSBAR_FRAME_STORY,
            vec![
                KnobSpec::text(STATUS_TEXT_KNOB, STATUS_TEXT),
                KnobSpec::choice(STATUS_TONE_KNOB, &tone_labels(), tone_index(Tone::Live)),
            ],
            Vec::new(),
            statusbar,
        ),
    ]
}

fn header(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let title = scene.knobs.text(TITLE_KNOB).to_string();
    let subtitle = scene.knobs.text(SUBTITLE_KNOB).to_string();

    panel(cx)
        .p_0()
        .child(chat_header(title.into(), subtitle.into(), cx))
        .into_any_element()
}

fn section(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let runtime = scene.runtime.clone();
    let expanded = scene.runtime.read(cx).section_expanded;
    let title = SECTION_TITLES
        .get(scene.knobs.choice(SECTION_TITLE_KNOB))
        .copied()
        .unwrap_or(DEFAULT_SECTION_TITLE);
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

fn statusbar(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let text = scene.knobs.text(STATUS_TEXT_KNOB).to_string();
    let status_tone = tone_at(scene.knobs.choice(STATUS_TONE_KNOB));

    story_column()
        .child(
            panel(cx).p_0().child(
                statusbar_frame(cx)
                    .child(status_dot_for_tone(status_tone, cx))
                    .child(div().child(text))
                    .child(div().flex_1())
                    .child(caption(STATUSBAR_TRAILING_NOTE, cx)),
            ),
        )
        .into_any_element()
}
