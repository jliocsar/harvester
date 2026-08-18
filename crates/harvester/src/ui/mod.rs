//! Harvester's visual architecture, grouped by responsibility.
//!
//! This is not a palette or a parallel widget library. Components use
//! `gpui-component` and resolve every color from `cx.theme()`; the shared core
//! exports semantic meanings and reusable non-color metrics only (SPEC C11).

mod core;
mod prompt;
mod shell;
mod tool_block;
mod transcript;

// Keep the stable `ui::…` seam for shell and prompt callers while ownership
// stays in cohesive modules rather than one broad file.
pub use core::{status_dot, use_mono_font_family, Dot, Tone};
pub use prompt::{
    prompt_box, prompt_box_control, prompt_box_controls, prompt_box_dropdown, prompt_box_input,
    prompt_box_send, DropdownRow,
};
pub use shell::{chat_header, section_chevron, section_header, statusbar_frame};
pub use transcript::TranscriptPresentation;
