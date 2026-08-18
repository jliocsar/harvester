use gpui::{div, prelude::*, App, Div, SharedString};

use super::core::Tone;

pub fn inline_note(tone: Tone, text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_xs()
        .text_color(tone.color(cx))
        .child(text.into())
}

pub fn empty_pane_hint(tone: Tone, text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .p_4()
        .text_color(tone.color(cx))
        .child(text.into())
}
