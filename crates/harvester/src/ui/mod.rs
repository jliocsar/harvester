mod core;
pub mod dialog;
mod prompt;
mod shell;
mod theme;
mod transcript;

pub use core::{
    empty_pane_action, empty_pane_hint, inline_note, status_dot, status_dot_for_tone, Dot, Tone,
};
pub use prompt::{
    prompt_strip, DropdownRow, PickMachine, PickModel, PickRepo, PromptStrip, PromptStripControl,
    PromptStripNotice, ADD_REPO_ROW, DEFAULT_MODEL_ROW,
};
pub use shell::{
    chat_header, collapsible_section, sidebar_empty_note, sidebar_row, sidebar_stacked_row,
    statusbar_frame, KeyboardFocusRing, SIDEBAR_ROW_GUTTER,
};
pub use theme::install_graphite_theme;
pub use transcript::TranscriptPresentation;
