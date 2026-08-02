//! The one dark palette (Cursor-adjacent): dense chrome, quiet borders,
//! readable text. Values are `0xRRGGBB` for `gpui::rgb` unless the name says
//! `_RGBA` (then `0xRRGGBBAA` for `gpui::rgba`).

pub const WINDOW_BG: u32 = 0x1f1f1f;
pub const PANEL_BG: u32 = 0x181818;
pub const INPUT_BG: u32 = 0x252525;
pub const BORDER: u32 = 0x2b2b2b;

pub const TEXT: u32 = 0xd6d6d6;
pub const TEXT_DIM: u32 = 0x8f8f8f;
pub const TEXT_FAINT: u32 = 0x5c5c5c;

pub const ACCENT: u32 = 0x4f9cf9;
/// Text-selection fill in the prompt input: the accent at ~25% alpha.
pub const SELECTION_RGBA: u32 = 0x4f9cf940;

pub const STATUS_RUNNING: u32 = 0x3fb950;
pub const STATUS_ERROR: u32 = 0xf25555;

pub const ROW_HOVER: u32 = 0x222222;
pub const ROW_SELECTED: u32 = 0x2a2d2e;
