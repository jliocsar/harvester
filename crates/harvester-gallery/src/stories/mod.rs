pub mod core;
pub mod dialog;
pub mod note;
pub mod prompt_strip;
pub mod shell;
pub mod sidebar;
pub mod transcript;

use gpui::{div, prelude::*, px, App, Div, SharedString};
use gpui_component::ActiveTheme as _;
use harvester::ui::{inline_note, Tone};

pub const CORE_GROUP: &str = "Core";
pub const NOTE_GROUP: &str = "Note";
pub const SHELL_GROUP: &str = "Shell";
pub const SIDEBAR_GROUP: &str = "Sidebar";
pub const FLAGSHIP_GROUP: &str = "Flagship";
pub const DIALOG_GROUP: &str = "Dialog";

const PANEL_MIN_WIDTH: f32 = 320.;
const TRANSCRIPT_PANEL_HEIGHT: f32 = 420.;
const FALLBACK_TONE: Tone = Tone::Muted;
const FALLBACK_TONE_INDEX: usize = 0;

pub fn tone_labels() -> Vec<&'static str> {
    Tone::ALL.iter().map(|tone| tone.label()).collect()
}

pub fn tone_at(index: usize) -> Tone {
    match Tone::ALL.get(index) {
        Some(tone) => *tone,
        None => FALLBACK_TONE,
    }
}

pub fn tone_index(tone: Tone) -> usize {
    match Tone::ALL.iter().position(|candidate| *candidate == tone) {
        Some(index) => index,
        None => FALLBACK_TONE_INDEX,
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
