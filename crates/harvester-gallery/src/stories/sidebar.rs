use gpui::{div, prelude::*, AnyElement, App, Window};
use gpui_component::ActiveTheme as _;
use harvester::ui::{
    sidebar_empty_note, sidebar_row, sidebar_stacked_row, KeyboardFocusRing as _, Tone,
};

use super::{caption, story_column, tone_at, tone_index, tone_labels, SIDEBAR_GROUP};
use crate::story::{KnobSpec, KnobValue, Preset, Story, StoryScene};

const LABEL_KNOB: &str = "label";
const SELECTED_KNOB: &str = "selected";
const DISABLED_KNOB: &str = "disabled";
const TONE_KNOB: &str = "dot tone";
const TITLE_KNOB: &str = "title";
const DETAIL_KNOB: &str = "detail";
const NOTE_KNOB: &str = "note";
const EMPTY_TEXT_KNOB: &str = "text";

const ROW_STORY: &str = "sidebar_row";
const STACKED_ROW_STORY: &str = "sidebar_stacked_row";
const EMPTY_NOTE_STORY: &str = "sidebar_empty_note";
const ROW_LABEL: &str = "fix the dialog room";
const STACKED_DETAIL: &str = "harvester · exe-vm-7fa2";
const STACKED_NOTE: &str = "opening…";
const EMPTY_NOTE_TEXTS: &[&str] = &["No sessions yet", "No machines yet", "No stories yet"];
const ROW_STORY_ID: &str = "story-sidebar-row";
const STACKED_ROW_STORY_ID: &str = "story-sidebar-stacked-row";
const DEFAULT_EMPTY_NOTE_TEXT: &str = "No sessions yet";
const ROW_STATE_CAPTION: &str = "Click state comes from the knobs, not from the mouse";
const DERIVED_SHAPE_CAPTION: &str = "The dot shape is derived from the tone, not chosen per row";

pub fn stories() -> Vec<Story> {
    vec![
        Story {
            group: SIDEBAR_GROUP,
            name: ROW_STORY,
            knobs: vec![
                KnobSpec::text(LABEL_KNOB, ROW_LABEL),
                KnobSpec::flag(SELECTED_KNOB, false),
                KnobSpec::flag(DISABLED_KNOB, false),
            ],
            presets: vec![
                Preset::new(
                    "idle",
                    vec![
                        (SELECTED_KNOB, KnobValue::Bool(false)),
                        (DISABLED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
                Preset::new(
                    "selected",
                    vec![
                        (SELECTED_KNOB, KnobValue::Bool(true)),
                        (DISABLED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
                Preset::new(
                    "disabled",
                    vec![
                        (SELECTED_KNOB, KnobValue::Bool(false)),
                        (DISABLED_KNOB, KnobValue::Bool(true)),
                    ],
                ),
            ],
            render: row,
        },
        Story {
            group: SIDEBAR_GROUP,
            name: STACKED_ROW_STORY,
            knobs: vec![
                KnobSpec::choice(TONE_KNOB, &tone_labels(), tone_index(Tone::Live)),
                KnobSpec::text(TITLE_KNOB, ROW_LABEL),
                KnobSpec::text(DETAIL_KNOB, STACKED_DETAIL),
                KnobSpec::text(NOTE_KNOB, STACKED_NOTE),
                KnobSpec::flag(SELECTED_KNOB, false),
            ],
            presets: vec![
                Preset::new(
                    "running session",
                    vec![
                        (TONE_KNOB, KnobValue::Choice(tone_index(Tone::Live))),
                        (NOTE_KNOB, KnobValue::Text(String::new())),
                        (SELECTED_KNOB, KnobValue::Bool(true)),
                    ],
                ),
                Preset::new(
                    "opening session",
                    vec![
                        (TONE_KNOB, KnobValue::Choice(tone_index(Tone::Idle))),
                        (NOTE_KNOB, KnobValue::Text(STACKED_NOTE.into())),
                        (SELECTED_KNOB, KnobValue::Bool(false)),
                    ],
                ),
            ],
            render: stacked_row,
        },
        Story {
            group: SIDEBAR_GROUP,
            name: EMPTY_NOTE_STORY,
            knobs: vec![KnobSpec::choice(EMPTY_TEXT_KNOB, EMPTY_NOTE_TEXTS, 0)],
            presets: Vec::new(),
            render: empty_note,
        },
    ]
}

fn row(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let label = scene.knobs.text(LABEL_KNOB).to_string();
    let selected = scene.knobs.flag(SELECTED_KNOB);
    let disabled = scene.knobs.flag(DISABLED_KNOB);

    story_column()
        .child(
            sidebar_frame(cx).child(
                sidebar_row(ROW_STORY_ID)
                    .selected(selected)
                    .disabled(disabled)
                    .tab_stop(!disabled)
                    .keyboard_focus_ring(cx)
                    .child(div().child(label)),
            ),
        )
        .child(caption(ROW_STATE_CAPTION, cx))
        .into_any_element()
}

fn stacked_row(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let tone = tone_at(scene.knobs.choice(TONE_KNOB));
    let title = scene.knobs.text(TITLE_KNOB).to_string();
    let detail = scene.knobs.optional_text(DETAIL_KNOB);
    let note = scene.knobs.optional_text(NOTE_KNOB);
    let selected = scene.knobs.flag(SELECTED_KNOB);

    story_column()
        .child(
            sidebar_frame(cx).child(
                sidebar_row(STACKED_ROW_STORY_ID)
                    .selected(selected)
                    .keyboard_focus_ring(cx)
                    .child(sidebar_stacked_row(tone, title.into(), detail, note, cx)),
            ),
        )
        .child(caption(DERIVED_SHAPE_CAPTION, cx))
        .into_any_element()
}

fn empty_note(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let text = EMPTY_NOTE_TEXTS
        .get(scene.knobs.choice(EMPTY_TEXT_KNOB))
        .copied()
        .unwrap_or(DEFAULT_EMPTY_NOTE_TEXT);

    sidebar_frame(cx)
        .child(sidebar_empty_note(text, cx))
        .into_any_element()
}

fn sidebar_frame(cx: &App) -> gpui::Div {
    div()
        .w_64()
        .py_2()
        .rounded_md()
        .bg(cx.theme().sidebar)
        .border_1()
        .border_color(cx.theme().sidebar_border)
        .flex()
        .flex_col()
}
