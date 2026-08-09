//! The design system: every reusable visual primitive lives here, and every
//! primitive has a section in the kitchen (`harvester kitchen`) showing its
//! variants. Components return `Div`/`Stateful<Div>` so callers can keep
//! chaining (`.on_click`, extra children); behavior stays with the caller,
//! looks stay here.

use std::sync::OnceLock;

use gpui::{div, prelude::*, px, rgb, Div, ElementId, FontWeight, SharedString, Stateful};

use crate::theme::{self, Tone};

/// Statusbar height; the workbench and the kitchen sample must agree.
const STATUSBAR_HEIGHT: f32 = 26.;

/// Sidebar section label ("MACHINES", "SESSIONS", …) — Apple-sidebar style:
/// small, semibold, faint. Collapsible: the whole row is the toggle (caller
/// chains `.on_click`); the caller adds optional extras (the workspaces "+")
/// and then `section_chevron` so the disclosure sits rightmost.
pub fn section_header(id: impl Into<ElementId>, title: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .pt_3()
        .pb_1()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::TEXT_FAINT))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(theme::TEXT_DIM)))
        .child(title.to_uppercase())
        .child(div().flex_1())
}

/// The section header's trailing disclosure chevron.
pub fn section_chevron(expanded: bool) -> Div {
    div().flex_none().child(if expanded { "▾" } else { "▸" })
}

/// The one dot: a machine's link, a session's liveness, a tool call's status.
pub fn status_dot(tone: Tone) -> Div {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(rgb(tone.color()))
}

pub enum ButtonKind {
    Neutral,
    Primary,
}

/// A clickable button; caller chains `.on_click`. Disabled buttons keep their
/// footprint but drop the pointer affordances — the caller must also skip the
/// click handler. No animation: buttons are high-frequency chrome.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
) -> Stateful<Div> {
    // Apple dark-mode push buttons: borderless fills, a whisper of shadow on
    // the enabled ones, flat when disabled.
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .child(label.into());

    match (kind, enabled) {
        (ButtonKind::Neutral, true) => base
            .bg(rgb(theme::BUTTON_BG))
            .text_color(rgb(theme::TEXT))
            .shadow_sm()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::BUTTON_HOVER)))
            .active(|style| style.bg(rgb(theme::BUTTON_ACTIVE))),
        (ButtonKind::Primary, true) => base
            .bg(rgb(theme::ACCENT))
            .text_color(rgb(theme::TEXT_ON_ACCENT))
            .shadow_sm()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::ACCENT_HOVER)))
            .active(|style| style.bg(rgb(theme::ACCENT_ACTIVE))),
        (_, false) => base
            .bg(rgb(theme::BUTTON_BG))
            .text_color(rgb(theme::TEXT_FAINT)),
    }
}

/// One chat message: author label over the text; user messages get a bubble.
pub fn chat_message(
    author: &'static str,
    author_tone: Tone,
    text: impl Into<SharedString>,
    bubble: bool,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(rgb(author_tone.color()))
                .child(author),
        )
        .child(
            div()
                .when(bubble, |style| {
                    style.bg(rgb(theme::INPUT_BG)).rounded_md().px_3().py_2()
                })
                .child(text.into()),
        )
}

/// The agent prompt box (SPEC §Flows "Start session", chat input): one
/// rounded card holding the text input on top and a controls row at the
/// bottom of the same frame; accent border while the input is focused.
/// Compose with `prompt_box_input` and `prompt_box_controls`.
pub fn prompt_box(focused: bool) -> Div {
    div()
        .rounded_xl()
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(if focused {
            rgb(theme::ACCENT)
        } else {
            rgb(theme::BORDER)
        })
        .flex()
        .flex_col()
}

/// The prompt box's input slot: padding around the caller's `TextInput`.
pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

/// The prompt box's bottom row. Caller adds `prompt_box_control`s, a
/// `flex_1` spacer, then `prompt_box_send`.
pub fn prompt_box_controls() -> Div {
    div().px_3().pb_2().flex().items_center().gap_2()
}

/// A flat dropdown control inside the prompt box (the model picker trigger):
/// quiet dim text with a caret, brightening on hover. Disabled ones lose the
/// caret and the pointer affordances — the caller must also skip the click
/// handler (loading / no models to offer).
pub fn prompt_box_control(
    id: impl Into<ElementId>,
    label: SharedString,
    enabled: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .text_xs()
        // Cap so an unreasonably long model name leaves room for the rest
        // of the row.
        .max_w(px(220.))
        .overflow_hidden()
        .text_ellipsis();

    if enabled {
        base.text_color(rgb(theme::TEXT_DIM))
            .cursor_pointer()
            .hover(|style| style.text_color(rgb(theme::TEXT)))
            .child(format!("{label} ▾"))
    } else {
        base.text_color(rgb(theme::TEXT_FAINT)).child(label)
    }
}

/// Diameter of the prompt box's round send button.
const SEND_BUTTON_SIZE: f32 = 22.;

/// The prompt box's round accent send button ("↑"). Caller chains
/// `.on_click`; Enter in the input is the other path to the same submit.
pub fn prompt_box_send(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(SEND_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .bg(rgb(theme::ACCENT))
        .text_color(rgb(theme::TEXT_ON_ACCENT))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ACCENT_HOVER)))
        .active(|style| style.bg(rgb(theme::ACCENT_ACTIVE)))
        .child("↑")
}

/// Chat pane header: title + faint subtitle (the unstarted chat's "New
/// session" line — the model control lives in the prompt box, not here).
pub fn chat_header(title: SharedString, subtitle: SharedString) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(subtitle),
        )
}

/// The monospace family tool-block bodies render in. gpui matches families by
/// exact name — there is no generic "monospace" resolution — so someone has to
/// resolve one; that someone is the **application**, at startup
/// ([`main::resolve_mono_font_family`]), because asking the system means
/// spawning a subprocess and a rendering module must not do that.
///
/// Unset, it answers `Menlo`: a family every macOS ships, and on Linux a miss
/// falls back to gpui's own default — so the components stay usable (the
/// kitchen catalog, a test) without an application around them.
pub fn use_mono_font_family(family: SharedString) {
    let _ = MONO_FONT_FAMILY.set(family);
}

pub fn mono_font_family() -> SharedString {
    MONO_FONT_FAMILY.get().cloned().unwrap_or("Menlo".into())
}

static MONO_FONT_FAMILY: OnceLock<SharedString> = OnceLock::new();

/// A tool-call block (SPEC §Rendering pi): rounded bordered card holding a
/// `tool_block_header` and, when expanded, a `tool_block_body`. Sits on the
/// chat background; the panel fill sets it apart from message bubbles.
pub fn tool_block() -> Div {
    div()
        .rounded_lg()
        .bg(rgb(theme::PANEL_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .flex()
        .flex_col()
        .overflow_hidden()
}

/// The block's clickable one-liner: status dot, the caller-composed label
/// (`tool_block_label`), trailing chevron. Caller chains `.on_click` to
/// toggle expansion. Dot color: accent = running, faint = done, red = error.
pub fn tool_block_header(
    id: impl Into<ElementId>,
    dot: Tone,
    label: Div,
    expanded: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
        .child(status_dot(dot))
        .child(label.flex_1().min_w(px(0.)))
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .child(if expanded { "▾" } else { "▸" }),
        )
}

/// The header label row; compose with `tool_block_verb`, `tool_block_subject`
/// and `tool_block_stats`.
pub fn tool_block_label() -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .overflow_hidden()
        .text_sm()
}

/// The label's dim leading verb ("Read", "Edit", "$").
pub fn tool_block_verb(text: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .text_color(rgb(theme::TEXT_DIM))
        .child(text.into())
}

/// The label's subject: file basename, pattern, or the bash command line
/// (`mono` for command text).
pub fn tool_block_subject(text: SharedString, mono: bool) -> Div {
    div()
        .overflow_hidden()
        .text_ellipsis()
        .when(mono, |subject| subject.font_family(mono_font_family()))
        .child(text)
}

/// "+N −N" line stats for edit/write labels.
pub fn tool_block_stats(added: usize, removed: usize) -> Div {
    div()
        .flex_none()
        .flex()
        .gap_1()
        .text_xs()
        .child(
            div()
                .text_color(rgb(theme::DIFF_ADDED))
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(rgb(theme::DIFF_REMOVED))
                .child(format!("−{removed}")),
        )
}

/// The expanded body under the header: monospace, small, separated by a
/// hairline. Compose with `tool_block_context`, `tool_block_output`,
/// `tool_block_diff_line` and `tool_block_truncation_notice`.
pub fn tool_block_body() -> Div {
    div()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .font_family(mono_font_family())
}

/// The body's faint context line (full path, full command, raw args).
pub fn tool_block_context(text: SharedString) -> Div {
    div().text_color(rgb(theme::TEXT_FAINT)).child(text)
}

/// The body's output text (tool result or live partial snapshot).
pub fn tool_block_output(text: SharedString) -> Div {
    div().text_color(rgb(theme::TEXT_DIM)).child(text)
}

/// One line of an edit/write diff body.
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

pub fn tool_block_diff_line(kind: DiffLineKind, text: SharedString) -> Div {
    let color = match kind {
        DiffLineKind::Context => theme::TEXT_DIM,
        DiffLineKind::Added => theme::DIFF_ADDED,
        DiffLineKind::Removed => theme::DIFF_REMOVED,
    };

    div().text_color(rgb(color)).child(text)
}

/// Shown above capped output — the head was dropped, the tail is live.
pub fn tool_block_truncation_notice() -> Div {
    div()
        .text_color(rgb(theme::TEXT_FAINT))
        .child("… output truncated")
}

/// The bottom statusbar container; caller adds its content children.
pub fn statusbar_frame() -> Div {
    div()
        .h(px(STATUSBAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .text_xs()
}
