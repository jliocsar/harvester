//! The one dark palette, aligned to Apple's dark-mode system colors (macOS
//! HIG): elevated surfaces get lighter, chrome stays darker than content,
//! accents are the system-blue/green/red family. Values are `0xRRGGBB` for
//! `gpui::rgb` unless the name says `_RGBA` (then `0xRRGGBBAA` for
//! `gpui::rgba`).

/// Content background (iOS `systemBackground` elevated, dark).
pub const WINDOW_BG: u32 = 0x1c1c1e;
/// Sidebar/statusbar chrome — darker than content, Apple-sidebar style.
pub const PANEL_BG: u32 = 0x161618;
/// Floating surfaces (modals): one elevation step above the window, Apple
/// dark-mode style (higher = lighter).
pub const ELEVATED_BG: u32 = 0x242426;
/// Elevated fills: inputs, bubbles (`systemGray6` dark).
pub const INPUT_BG: u32 = 0x2c2c2e;
/// Hairline separators on dark surfaces.
pub const BORDER: u32 = 0x38383a;

pub const TEXT: u32 = 0xededf0;
pub const TEXT_DIM: u32 = 0x9c9ca1;
pub const TEXT_FAINT: u32 = 0x64646a;

/// `systemBlue`, dark mode.
pub const ACCENT: u32 = 0x0a84ff;
pub const ACCENT_HOVER: u32 = 0x2f97ff;
pub const ACCENT_ACTIVE: u32 = 0x0771dd;
/// Text on accent-filled surfaces (primary buttons).
pub const TEXT_ON_ACCENT: u32 = 0xffffff;
/// Text-selection fill in the prompt input: the accent at ~25% alpha.
pub const SELECTION_RGBA: u32 = 0x0a84ff40;

/// `systemGreen` / `systemRed`, dark mode.
pub const STATUS_RUNNING: u32 = 0x30d158;
pub const STATUS_ERROR: u32 = 0xff453a;
/// `systemYellow`, dark mode — a machine whose bridge is still
/// connecting/bootstrapping (SPEC §UI machine dots).
pub const STATUS_PENDING: u32 = 0xffd60a;

/// Diff line colors in tool-block bodies (edit/write). Same system green/red
/// as the status pair, but named for what they mean here.
pub const DIFF_ADDED: u32 = 0x30d158;
pub const DIFF_REMOVED: u32 = 0xff453a;

/// Neutral button fill (`systemGray5` dark) and its press states.
pub const BUTTON_BG: u32 = 0x3a3a3c;
pub const BUTTON_HOVER: u32 = 0x48484a;
pub const BUTTON_ACTIVE: u32 = 0x2c2c2e;

/// Modal backdrop: black at 50%, dimming the workbench while a dialog is up.
pub const SCRIM_RGBA: u32 = 0x00000080;

pub const ROW_HOVER: u32 = 0x232325;
/// Sidebar selection pill (Apple uses a gray pill; accent stays for text).
pub const ROW_SELECTED: u32 = 0x3a3a3c;

/// What a component is being told about its subject — the vocabulary that
/// crosses the seam into `ui`, in place of the pixels it used to carry. A
/// component handed `0x30d158` has had a theme decision made for it by its
/// caller and can never be re-themed; one handed [`Tone::Live`] has not.
///
/// Deliberately small, and named for meaning rather than for colour: `Live`
/// rather than `Green`, because the day a theme paints "live" differently is
/// the day the name has to survive it.
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
    /// The one place a meaning becomes a colour.
    pub fn color(self) -> u32 {
        match self {
            Tone::Muted => TEXT_DIM,
            Tone::Idle => TEXT_FAINT,
            Tone::Active => ACCENT,
            Tone::Pending => STATUS_PENDING,
            Tone::Live => STATUS_RUNNING,
            Tone::Failed => STATUS_ERROR,
        }
    }
}
