pub mod core;
pub mod dialog;
pub mod note;
pub mod prompt;
pub mod prompt_strip;
pub mod shell;
pub mod sidebar;
pub mod transcript;

use gpui::{div, prelude::*, px, App, Div, SharedString};
use gpui_component::ActiveTheme as _;
use harvester::ui::{inline_note, Tone};

pub const TONES: [Tone; 6] = [
    Tone::Muted,
    Tone::Idle,
    Tone::Active,
    Tone::Pending,
    Tone::Live,
    Tone::Failed,
];

pub const TONE_LABELS: &[&str] = &[
    Tone::Muted.label(),
    Tone::Idle.label(),
    Tone::Active.label(),
    Tone::Pending.label(),
    Tone::Live.label(),
    Tone::Failed.label(),
];

const PANEL_MIN_WIDTH: f32 = 320.;
const TRANSCRIPT_PANEL_HEIGHT: f32 = 420.;

pub fn tone_at(index: usize) -> Tone {
    match TONES.get(index) {
        Some(tone) => *tone,
        None => Tone::Muted,
    }
}

pub fn story_column() -> Div {
    div().flex().flex_col().gap_3()
}

pub fn story_row() -> Div {
    div().flex().items_center().gap_3()
}

pub fn caption(text: impl Into<SharedString>, cx: &App) -> Div {
    inline_note(Tone::Muted, text, cx)
}

pub fn panel(cx: &App) -> Div {
    div()
        .min_w(px(PANEL_MIN_WIDTH))
        .p_3()
        .rounded_lg()
        .border_1()
        .border_color(cx.theme().border)
        .bg(cx.theme().popover)
        .shadow_sm()
        .flex()
        .flex_col()
        .gap_2()
}

pub fn transcript_panel(cx: &App) -> Div {
    panel(cx)
        .h(px(TRANSCRIPT_PANEL_HEIGHT))
        .p_0()
        .overflow_hidden()
        .bg(cx.theme().background)
}
