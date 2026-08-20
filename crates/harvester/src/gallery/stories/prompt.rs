use gpui::{div, prelude::*, AnyElement, App, Window};
use gpui_component::{Icon, IconName};
use harvester::ui::{
    prompt_box, prompt_box_control, prompt_box_controls, prompt_box_dropdown, prompt_box_send,
    DropdownRow,
};

use super::{caption, panel, story_column, story_row};
use crate::story::StoryScene;

pub const LABEL_KNOB: &str = "label";
pub const ENABLED_KNOB: &str = "enabled";

const CONTROL_STORY_ID: &str = "story-prompt-control";
const SEND_STORY_ID: &str = "story-prompt-send";
const DROPDOWN_TRIGGER_ID: &str = "story-prompt-dropdown-trigger";
const DROPDOWN_STORY_ID: &str = "story-prompt-dropdown";
const DROPDOWN_ROW_LABELS: &[&str] = &[
    "claude-sonnet-4-6",
    "claude-opus-4-1",
    "gpt-5-codex",
    "Add a model…",
];
const DROPDOWN_DIVIDER_INDEX: usize = 3;
const DROPDOWN_CAPTION: &str = "The popover is live — pick a row and the trigger label follows";
const SEND_CAPTION: &str = "Send is the only control that carries the primary accent";

pub fn control(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let label = scene.knobs.text(LABEL_KNOB).to_string();
    let enabled = scene.knobs.flag(ENABLED_KNOB);

    panel(cx)
        .child(prompt_box_control(
            CONTROL_STORY_ID,
            Icon::new(IconName::Bot),
            label.into(),
            enabled,
            cx,
        ))
        .into_any_element()
}

pub fn send(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let enabled = scene.knobs.flag(ENABLED_KNOB);

    story_column()
        .child(panel(cx).child(story_row().child(prompt_box_send(SEND_STORY_ID, enabled, cx))))
        .child(caption(SEND_CAPTION, cx))
        .into_any_element()
}

pub fn dropdown(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let runtime = scene.runtime.clone();
    let selected = scene.runtime.read(cx).dropdown_selection;
    let label = DROPDOWN_ROW_LABELS
        .get(selected)
        .copied()
        .unwrap_or(DROPDOWN_ROW_LABELS[0]);
    let trigger = prompt_box_control(
        DROPDOWN_TRIGGER_ID,
        Icon::new(IconName::Bot),
        label.into(),
        true,
        cx,
    );
    let dropdown = prompt_box_dropdown(
        DROPDOWN_STORY_ID,
        trigger,
        dropdown_rows(selected),
        move |index, _, cx| {
            runtime.update(cx, |runtime, cx| {
                runtime.dropdown_selection = index;
                cx.notify();
            });
        },
        cx,
    );

    story_column()
        .child(
            panel(cx).child(
                prompt_box(false, cx)
                    .child(prompt_box_controls().child(dropdown).child(div().flex_1())),
            ),
        )
        .child(caption(DROPDOWN_CAPTION, cx))
        .into_any_element()
}

fn dropdown_rows(selected: usize) -> Vec<DropdownRow> {
    DROPDOWN_ROW_LABELS
        .iter()
        .enumerate()
        .map(|(index, label)| DropdownRow {
            label: (*label).into(),
            selected: index == selected,
            divider_above: index == DROPDOWN_DIVIDER_INDEX,
        })
        .collect()
}
