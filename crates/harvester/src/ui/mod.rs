mod core;
pub mod dialog;
mod note;
mod prompt;
mod shell;
mod sidebar;
mod theme;
mod tool_block;
mod transcript;

pub use core::{status_dot, status_dot_for_tone, Dot, Tone};
pub use note::{empty_pane_action, empty_pane_hint, inline_note};
pub use prompt::{
    prompt_strip, DropdownRow, PickMachine, PickModel, PickRepo, PromptStrip, PromptStripControl,
    PromptStripNotice, ADD_REPO_ROW, DEFAULT_MODEL_ROW,
};
pub use shell::{chat_header, collapsible_section, statusbar_frame, KeyboardFocusRing};
pub use sidebar::{sidebar_empty_note, sidebar_row, sidebar_stacked_row, SIDEBAR_ROW_GUTTER};
pub use theme::install_graphite_theme;
pub use transcript::TranscriptPresentation;
