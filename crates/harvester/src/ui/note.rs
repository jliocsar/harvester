use gpui::{div, prelude::*, App, Div, ElementId, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{v_flex, Sizable as _};

use super::core::Tone;

pub fn inline_note(tone: Tone, text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(tone.color(cx))
        .child(text.into())
}

pub fn empty_pane_hint(tone: Tone, text: impl Into<SharedString>, cx: &App) -> Div {
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap_4()
        .p_8()
        .text_center()
        .text_color(tone.color(cx))
        .child(text.into())
}

pub fn empty_pane_action(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id).label(label).primary().small()
}
