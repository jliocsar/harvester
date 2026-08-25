use gpui::{div, prelude::*, rems, AnyElement, App, Div, FontWeight, Window};
use gpui_component::input::Input;
use gpui_component::v_flex;
use gpui_component::ActiveTheme as _;
use harvester::ui::dialog::{
    action_button, button_row, cancel_button, dismiss_only_row, option_row, options_height,
    title_or_named_condition, ActionAccent, Confirmation, CONFIRMATION_DIALOG_WIDTH_IN_REMS,
    SELECT_DIALOG_WIDTH_IN_REMS, TEXT_DIALOG_WIDTH_IN_REMS,
};

use super::{caption, panel, story_column};
use crate::story::StoryScene;

pub const ACCENT_KNOB: &str = "accent";
pub const TITLE_KNOB: &str = "title";
pub const MESSAGE_KNOB: &str = "message";
pub const CANCEL_LABEL_KNOB: &str = "cancel label";
pub const ACTION_LABEL_KNOB: &str = "action label";

pub const ACCENT_LABELS: &[&str] = &["primary", "danger"];

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

pub fn chrome(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
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
                .child(Input::new(&single_line))
                .child(caption(MULTI_LINE_CAPTION, cx))
                .child(Input::new(&multi_line)),
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
        1 => ActionAccent::Danger,
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
    let buttons = dismiss_only_row(cancel_button(SELECT_CANCEL_BUTTON_ID, "Cancel"));

    panel(cx)
        .w(rems(SELECT_DIALOG_WIDTH_IN_REMS))
        .bg(cx.theme().popover)
        .child(title_line("Which branch should I work on?"))
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
