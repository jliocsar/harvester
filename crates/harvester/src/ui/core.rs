//! Semantic visual vocabulary shared across Harvester surfaces.
//!
//! Meanings cross module boundaries; colors do not. [`Tone`] is the only
//! place those meanings become colors, and every color still comes from
//! `cx.theme()` (SPEC C11).

use std::sync::OnceLock;

use gpui::{div, prelude::*, px, App, Div, SharedString};
use gpui_component::ActiveTheme as _;

/// What a component is being told about its subject, in place of raw color.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Present, but not what the user is looking for: secondary labels.
    Muted,
    /// Backgrounded: an unprobed machine, a tool that has finished.
    Idle,
    /// What the user is doing right now.
    Active,
    /// Working; the outcome is not known yet.
    Pending,
    /// Confirmed good — a connected daemon, a running session.
    Live,
    /// Failed, and the user needs to see that it did.
    Failed,
}

impl Tone {
    /// The one place a meaning becomes a color.
    ///
    /// `Active` is `blue` rather than `primary`: in the library's dark theme
    /// `primary` is near-white, which is right for a filled button and wrong
    /// for the word "you". `Idle` is `Muted` faded, the one distinction the
    /// library has no token for.
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

/// How far idle and tertiary content sits behind ordinary secondary content.
const IDLE_OPACITY: f32 = 0.6;

/// Secondary text: labels, subtitles, the dim half of a two-tone line.
pub(super) fn dim(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground
}

/// Tertiary text: context lines, disabled controls, disclosure chevrons.
pub(super) fn faint(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground.opacity(IDLE_OPACITY)
}

/// A status dot: its semantic tone, and whether it stands for evidence or the
/// absence of any.
///
/// `Hollow` exists because one machine state — never probed, never connected —
/// is not a verdict at all. An outline says *no information* without spending
/// a color the theme does not have.
#[derive(Clone, Copy)]
pub enum Dot {
    Solid(Tone),
    Hollow(Tone),
}

impl Dot {
    /// The tone, for text a dot's row or link is colored to match.
    pub fn tone(self) -> Tone {
        match self {
            Dot::Solid(tone) | Dot::Hollow(tone) => tone,
        }
    }
}

/// The one dot used for machine links, session liveness, and tool status.
pub fn status_dot(dot: Dot, cx: &App) -> Div {
    let base = div().flex_none().size(px(6.)).rounded_full();

    match dot {
        Dot::Solid(tone) => base.bg(tone.color(cx)),
        Dot::Hollow(tone) => base.border_1().border_color(tone.color(cx)),
    }
}

/// Set the monospace family used for captured tool output and diffs.
///
/// gpui matches exact family names, so the application resolves one at startup
/// rather than making a rendering module spawn a subprocess.
pub fn use_mono_font_family(family: SharedString) {
    let _ = MONO_FONT_FAMILY.set(family);
}

pub(super) fn mono_font_family() -> SharedString {
    MONO_FONT_FAMILY.get().cloned().unwrap_or("Menlo".into())
}

static MONO_FONT_FAMILY: OnceLock<SharedString> = OnceLock::new();
