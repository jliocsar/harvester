use gpui::{div, prelude::*, px, AnyElement, App, Div, FontWeight, Window};
use gpui_component::input::Input;
use gpui_component::ActiveTheme as _;
use harvester::ui::dialog::{
    action_button, button_row, cancel_button, title_or_fallback, ActionAccent, Confirmation,
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
const DIALOG_SURFACE_WIDTH: f32 = 520.;
const SINGLE_LINE_CAPTION: &str = "Single-line input (pi text dialog)";
const MULTI_LINE_CAPTION: &str = "Multi-line input (pi editor dialog)";
const FOUNDATION_CAPTION: &str =
    "Chrome only — rendered inline, never through window.open_dialog; the redesign happens here";

pub fn chrome(scene: &StoryScene, _window: &mut Window, cx: &mut App) -> AnyElement {
    let confirmation = confirmation(scene);
    let runtime = scene.runtime.read(cx);
    let single_line = runtime.dialog_single_line_input.clone();
    let multi_line = runtime.dialog_multi_line_input.clone();

    story_column()
        .child(confirmation_surface(confirmation, cx))
        .child(
            panel(cx)
                .w(px(DIALOG_SURFACE_WIDTH))
                .child(caption(SINGLE_LINE_CAPTION, cx))
                .child(Input::new(&single_line))
                .child(caption(MULTI_LINE_CAPTION, cx))
                .child(Input::new(&multi_line)),
        )
        .child(caption(FOUNDATION_CAPTION, cx))
        .into_any_element()
}

fn confirmation(scene: &StoryScene) -> Confirmation {
    Confirmation {
        title: title_or_fallback(scene.knobs.text(TITLE_KNOB)),
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
        .w(px(DIALOG_SURFACE_WIDTH))
        .bg(cx.theme().popover)
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child(confirmation.title),
        )
        .child(div().child(confirmation.message))
        .child(buttons)
}
