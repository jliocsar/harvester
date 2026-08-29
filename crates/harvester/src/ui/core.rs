use gpui::{div, prelude::*, px, App, Div, ElementId, Entity, Hsla, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, ActiveTheme as _, Icon, IconName};

const FAINT_TEXT_OPACITY: f32 = 0.6;

const STATUS_DOT_DIAMETER: f32 = 6.;

const TRUNCATION_ELLIPSIS: char = '\u{2026}';

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

    pub fn is_hollow(self) -> bool {
        match self {
            Tone::Idle | Tone::Pending => true,
            Tone::Muted | Tone::Active | Tone::Live | Tone::Failed => false,
        }
    }
}

pub(super) fn truncated_to_max_chars(text: &str, max_chars: usize) -> Option<String> {
    if text.chars().count() <= max_chars {
        return None;
    }

    let kept: String = text.chars().take(max_chars.saturating_sub(1)).collect();

    Some(format!("{}{TRUNCATION_ELLIPSIS}", kept.trim_end()))
}

pub(super) fn dim(cx: &App) -> Hsla {
    Tone::Muted.color(cx)
}

pub(super) fn faint(cx: &App) -> Hsla {
    cx.theme().muted_foreground.opacity(FAINT_TEXT_OPACITY)
}

pub fn status_dot_for_tone(tone: Tone, cx: &App) -> Div {
    let base = div()
        .flex_none()
        .size(px(STATUS_DOT_DIAMETER))
        .rounded_full();

    if tone.is_hollow() {
        return base.border_1().border_color(tone.color(cx));
    }

    base.bg(tone.color(cx))
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
    Button::new(id).label(label).primary()
}

pub fn text_input(input: &Entity<InputState>) -> Input {
    Input::new(input).text_base()
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

#[cfg(test)]
mod tests {
    use super::{truncated_to_max_chars, TRUNCATION_ELLIPSIS};

    #[test]
    fn text_that_fits_the_budget_is_left_alone() {
        assert_eq!(truncated_to_max_chars("Sonnet", 24), None);
        assert_eq!(truncated_to_max_chars(&"x".repeat(24), 24), None);
    }

    #[test]
    fn the_ellipsis_is_inside_the_budget_not_beyond_it() {
        let truncated = truncated_to_max_chars(&"x".repeat(40), 24).expect("a cut");

        assert_eq!(truncated.chars().count(), 24);
        assert!(truncated.ends_with(TRUNCATION_ELLIPSIS));
    }

    #[test]
    fn a_multibyte_cut_lands_on_a_character_boundary() {
        let truncated = truncated_to_max_chars(&"\u{3042}".repeat(40), 24).expect("a cut");

        assert_eq!(truncated.chars().count(), 24);
    }

    #[test]
    fn no_space_is_left_stranded_before_the_ellipsis() {
        let padded = format!("{} tail", "y".repeat(22));
        let truncated = truncated_to_max_chars(&padded, 24).expect("a cut");

        assert_eq!(
            truncated,
            format!("{}{TRUNCATION_ELLIPSIS}", "y".repeat(22))
        );
    }
}
