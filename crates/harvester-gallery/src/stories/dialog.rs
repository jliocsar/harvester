use gpui::{div, prelude::*, rems, AnyElement, App, Div, FontWeight, Window};
use gpui_component::v_flex;
use gpui_component::ActiveTheme as _;
use harvester::ui::dialog::{
    action_button, button_row, cancel_button, dismiss_only_row, option_row, options_height,
    title_or_named_condition, ActionAccent, Confirmation, CANCEL_LABEL,
    CONFIRMATION_DIALOG_WIDTH_IN_REMS, SELECT_DIALOG_WIDTH_IN_REMS, TEXT_DIALOG_WIDTH_IN_REMS,
};

use harvester::ui::text_input;

use super::{caption, panel, story_column, DIALOG_GROUP};
use crate::story::{KnobSpec, KnobValue, Preset, Story, StoryScene};

const ACCENT_KNOB: &str = "accent";
const TITLE_KNOB: &str = "title";
const MESSAGE_KNOB: &str = "message";
const CANCEL_LABEL_KNOB: &str = "cancel label";
const ACTION_LABEL_KNOB: &str = "action label";

const DIALOG_CHROME_STORY: &str = "Dialog chrome";
const ACCENT_LABELS: &[&str] = &["primary", "danger"];
const PRIMARY_ACCENT_INDEX: usize = 0;
const DANGER_ACCENT_INDEX: usize = 1;
const CONFIRM_TITLE: &str = "Start a session on exe-vm-7fa2?";
const CONFIRM_MESSAGE: &str = "pi will run in ~/harvester on the branch backend-hardening.";
const CONFIRM_ACTION_LABEL: &str = "Start";
const DESTRUCTIVE_TITLE: &str = "Forget exe-vm-0c19?";
const DESTRUCTIVE_MESSAGE: &str =
    "The machine is no longer listed by its provider. Sessions on its disk are untouched.";
const DESTRUCTIVE_ACTION_LABEL: &str = "Forget";
const SELECT_TITLE: &str = "Which branch should I work on?";
const CANCEL_BUTTON_ID: &str = "story-dialog-cancel";
const ACTION_BUTTON_ID: &str = "story-dialog-action";
const SELECT_CANCEL_BUTTON_ID: &str = "story-dialog-select-cancel";
const UNTITLED_CONFIRM_CONDITION: &str = "The agent needs a yes or no";
const CONFIRMATION_CAPTION: &str = "Confirmation width — one decision, no form";
const SELECT_CAPTION: &str = "Select width — a scannable option list, Enter commits";
const TEXT_CAPTION: &str = "Text width — single-line input (pi text dialog)";
const MULTI_LINE_CAPTION: &str = "Text width — multi-line input (pi editor dialog)";
const FOUNDATION_CAPTION: &str =
    "Chrome only — rendered inline, never through window.open_dialog; the redesign happens here";
const SELECTED_OPTION_INDEX: usize = 1;
const STORY_OPTIONS: &[&str] = &[
    "main",
    "backend-hardening",
    "design-pass",
    "release/0.4",
    "spike/pi-0.84",
];

pub fn stories() -> Vec<Story> {
    vec![Story {
        group: DIALOG_GROUP,
        name: DIALOG_CHROME_STORY,
        knobs: knobs(),
        presets: presets(),
        render: chrome,
    }]
}

fn knobs() -> Vec<KnobSpec> {
    vec![
        KnobSpec::choice(ACCENT_KNOB, ACCENT_LABELS, PRIMARY_ACCENT_INDEX),
        KnobSpec::text(TITLE_KNOB, CONFIRM_TITLE),
        KnobSpec::text(MESSAGE_KNOB, CONFIRM_MESSAGE),
        KnobSpec::text(CANCEL_LABEL_KNOB, CANCEL_LABEL),
        KnobSpec::text(ACTION_LABEL_KNOB, CONFIRM_ACTION_LABEL),
    ]
}

fn presets() -> Vec<Preset> {
    vec![
        Preset::new(
            "confirm",
            vec![
                (ACCENT_KNOB, KnobValue::Choice(PRIMARY_ACCENT_INDEX)),
                (TITLE_KNOB, KnobValue::Text(CONFIRM_TITLE.into())),
                (MESSAGE_KNOB, KnobValue::Text(CONFIRM_MESSAGE.into())),
                (
                    ACTION_LABEL_KNOB,
                    KnobValue::Text(CONFIRM_ACTION_LABEL.into()),
                ),
            ],
        ),
        Preset::new(
            "destructive",
            vec![
                (ACCENT_KNOB, KnobValue::Choice(DANGER_ACCENT_INDEX)),
                (TITLE_KNOB, KnobValue::Text(DESTRUCTIVE_TITLE.into())),
                (MESSAGE_KNOB, KnobValue::Text(DESTRUCTIVE_MESSAGE.into())),
                (
                    ACTION_LABEL_KNOB,
                    KnobValue::Text(DESTRUCTIVE_ACTION_LABEL.into()),
                ),
            ],
        ),
        Preset::new(
            "untitled",
            vec![(TITLE_KNOB, KnobValue::Text(String::new()))],
        ),
    ]
}

fn chrome(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let confirmation = confirmation(scene);
    let runtime = scene.runtime.read(cx);
    let single_line = runtime.dialog_single_line_input.clone();
    let multi_line = runtime.dialog_multi_line_input.clone();

    story_column()
        .child(caption(CONFIRMATION_CAPTION, cx))
        .child(confirmation_surface(confirmation, cx))
        .child(caption(SELECT_CAPTION, cx))
        .child(select_surface(cx))
        .child(
            panel(cx)
                .w(rems(TEXT_DIALOG_WIDTH_IN_REMS))
                .child(caption(TEXT_CAPTION, cx))
                .child(text_input(&single_line))
                .child(caption(MULTI_LINE_CAPTION, cx))
                .child(text_input(&multi_line)),
        )
        .child(caption(FOUNDATION_CAPTION, cx))
        .into_any_element()
}

fn confirmation(scene: &StoryScene) -> Confirmation {
    Confirmation {
        title: title_or_named_condition(scene.knobs.text(TITLE_KNOB), UNTITLED_CONFIRM_CONDITION),
        message: scene.knobs.text(MESSAGE_KNOB).to_string().into(),
        cancel_label: scene.knobs.text(CANCEL_LABEL_KNOB).to_string().into(),
        action_label: scene.knobs.text(ACTION_LABEL_KNOB).to_string().into(),
        action_accent: accent(scene.knobs.choice(ACCENT_KNOB)),
    }
}

fn accent(index: usize) -> ActionAccent {
    match index {
        DANGER_ACCENT_INDEX => ActionAccent::Danger,
        _ => ActionAccent::Primary,
    }
}

fn confirmation_surface(confirmation: Confirmation, cx: &App) -> Div {
    let buttons = button_row(
        cancel_button(CANCEL_BUTTON_ID, confirmation.cancel_label),
        action_button(
            ACTION_BUTTON_ID,
            confirmation.action_label,
            confirmation.action_accent,
        ),
    );

    panel(cx)
        .w(rems(CONFIRMATION_DIALOG_WIDTH_IN_REMS))
        .bg(cx.theme().popover)
        .child(title_line(confirmation.title))
        .child(div().child(confirmation.message))
        .child(buttons)
}

fn select_surface(cx: &App) -> Div {
    let buttons = dismiss_only_row(cancel_button(SELECT_CANCEL_BUTTON_ID, CANCEL_LABEL));

    panel(cx)
        .w(rems(SELECT_DIALOG_WIDTH_IN_REMS))
        .bg(cx.theme().popover)
        .child(title_line(SELECT_TITLE))
        .child(option_rows(cx))
        .child(buttons)
}

fn option_rows(cx: &App) -> Div {
    let rows = STORY_OPTIONS
        .iter()
        .enumerate()
        .map(|(index, option)| option_row(index, *option, index == SELECTED_OPTION_INDEX));

    v_flex()
        .h(options_height(STORY_OPTIONS.len()))
        .overflow_hidden()
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius)
        .children(rows)
}

fn title_line(title: impl Into<gpui::SharedString>) -> Div {
    div().font_weight(FontWeight::MEDIUM).child(title.into())
}
