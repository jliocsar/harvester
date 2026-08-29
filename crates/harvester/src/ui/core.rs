use gpui::{div, prelude::*, px, App, Div, ElementId, Hsla, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::{v_flex, ActiveTheme as _, Icon, IconName, Sizable as _};

const FAINT_TEXT_OPACITY: f32 = 0.6;

const STATUS_DOT_DIAMETER: f32 = 6.;

const MUTED_LABEL: &str = "Neutral";
const IDLE_LABEL: &str = "Idle";
const ACTIVE_LABEL: &str = "Running";
const PENDING_LABEL: &str = "Pending";
const LIVE_LABEL: &str = "Live";
const FAILED_LABEL: &str = "Failed";

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
    pub const ALL: &'static [Tone] = &[
        Tone::Muted,
        Tone::Idle,
        Tone::Active,
        Tone::Pending,
        Tone::Live,
        Tone::Failed,
    ];

    pub fn color(self, cx: &App) -> Hsla {
        let theme = cx.theme();

        match self {
            Tone::Muted => theme.muted_foreground,
            Tone::Idle => faint(cx),
            Tone::Active => theme.info,
            Tone::Pending => theme.warning,
            Tone::Live => theme.success,
            Tone::Failed => theme.danger,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Tone::Muted => MUTED_LABEL,
            Tone::Idle => IDLE_LABEL,
            Tone::Active => ACTIVE_LABEL,
            Tone::Pending => PENDING_LABEL,
            Tone::Live => LIVE_LABEL,
            Tone::Failed => FAILED_LABEL,
        }
    }

    pub fn dot(self) -> Dot {
        match self {
            Tone::Idle | Tone::Pending => Dot::Hollow(self),
            Tone::Muted | Tone::Active | Tone::Live | Tone::Failed => Dot::Solid(self),
        }
    }
}

pub(super) fn dim(cx: &App) -> Hsla {
    Tone::Muted.color(cx)
}

pub(super) fn faint(cx: &App) -> Hsla {
    cx.theme().muted_foreground.opacity(FAINT_TEXT_OPACITY)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

pub fn status_dot_for_tone(tone: Tone, cx: &App) -> Div {
    status_dot(tone.dot(), cx)
}

pub fn status_dot(dot: Dot, cx: &App) -> Div {
    let base = div()
        .flex_none()
        .size(px(STATUS_DOT_DIAMETER))
        .rounded_full();

    match dot {
        Dot::Solid(tone) => base.bg(tone.color(cx)),
        Dot::Hollow(tone) => base.border_1().border_color(tone.color(cx)),
    }
}

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

pub(super) fn disclosure_chevron(expanded: bool) -> Icon {
    if expanded {
        return Icon::new(IconName::ChevronDown).size_3p5();
    }

    Icon::new(IconName::ChevronRight).size_3p5()
}

pub(super) fn mono_font_family(cx: &App) -> SharedString {
    cx.theme().mono_font_family.clone()
}
