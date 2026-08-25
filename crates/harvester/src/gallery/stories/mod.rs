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

pub const TONE_LABELS: &[&str] = &["muted", "idle", "active", "pending", "live", "failed"];

const PANEL_MIN_WIDTH: f32 = 320.;
const TRANSCRIPT_PANEL_HEIGHT: f32 = 420.;

pub fn tone_at(index: usize) -> Tone {
    match index {
        1 => Tone::Idle,
        2 => Tone::Active,
        3 => Tone::Pending,
        4 => Tone::Live,
        5 => Tone::Failed,
        _ => Tone::Muted,
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
