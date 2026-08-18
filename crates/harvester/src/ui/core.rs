use std::sync::OnceLock;

use gpui::{div, prelude::*, px, App, Div, SharedString};
use gpui_component::ActiveTheme as _;

pub const BASE_TEXT_SIZE: f32 = 13.;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Muted,
    Idle,
    Active,
    Pending,
    Live,
    Failed,
}

impl Tone {
    pub fn color(self, cx: &App) -> gpui::Hsla {
        let theme = cx.theme();

        match self {
            Tone::Muted => theme.muted_foreground,
            Tone::Idle => theme.muted_foreground.opacity(IDLE_OPACITY),
            Tone::Active => theme.blue,
            Tone::Pending => theme.warning,
            Tone::Live => theme.success,
            Tone::Failed => theme.danger,
        }
    }
}

const IDLE_OPACITY: f32 = 0.6;

pub(super) fn dim(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground
}

pub(super) fn faint(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground.opacity(IDLE_OPACITY)
}

#[derive(Clone, Copy)]
pub enum Dot {
    Solid(Tone),
    Hollow(Tone),
}

impl Dot {
    pub fn tone(self) -> Tone {
        match self {
            Dot::Solid(tone) | Dot::Hollow(tone) => tone,
        }
    }
}

pub fn status_dot(dot: Dot, cx: &App) -> Div {
    let base = div().flex_none().size(px(6.)).rounded_full();

    match dot {
        Dot::Solid(tone) => base.bg(tone.color(cx)),
        Dot::Hollow(tone) => base.border_1().border_color(tone.color(cx)),
    }
}

pub fn use_mono_font_family(family: SharedString) {
    let _ = MONO_FONT_FAMILY.set(family);
}

pub(super) fn mono_font_family() -> SharedString {
    MONO_FONT_FAMILY.get().cloned().unwrap_or("Menlo".into())
}

static MONO_FONT_FAMILY: OnceLock<SharedString> = OnceLock::new();
