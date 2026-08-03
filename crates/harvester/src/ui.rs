//! The design system: every reusable visual primitive lives here, and every
//! primitive has a section in the kitchen (`harvester kitchen`) showing its
//! variants. Components return `Div`/`Stateful<Div>` so callers can keep
//! chaining (`.on_click`, extra children); behavior stays with the caller,
//! looks stay here.

use gpui::{div, prelude::*, px, rgb, Div, ElementId, FontWeight, SharedString, Stateful};

use crate::theme;

/// Statusbar height; the workbench and the kitchen sample must agree.
pub const STATUSBAR_HEIGHT: f32 = 26.;
/// Modal panel width — wide enough for a path, narrow enough to stay a dialog.
pub const MODAL_WIDTH: f32 = 420.;

/// Sidebar section label ("MACHINE", "SESSIONS", …) — Apple-sidebar style:
/// small, semibold, faint.
pub fn section_header(title: &'static str) -> Div {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::TEXT_FAINT))
        .child(title.to_uppercase())
}

/// Session liveness indicator (`STATUS_RUNNING` green / `TEXT_FAINT` exited).
pub fn status_dot(color: u32) -> Div {
    div().flex_none().size(px(6.)).rounded_full().bg(rgb(color))
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

/// One sidebar session row: status dot + title line, workspace line under it,
/// hover/selection as an Apple-sidebar rounded pill (inset from the edges).
/// Caller chains `.on_click`.
pub fn session_row(
    id: impl Into<ElementId>,
    title: SharedString,
    workspace_name: SharedString,
    dot_color: u32,
    selected: bool,
    opening: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        // 6px margin + 6px padding = the 12px text inset section_header uses.
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .rounded_md()
        .flex()
        .flex_col()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
        .when(selected, |style| style.bg(rgb(theme::ROW_SELECTED)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(status_dot(dot_color))
                .child(div().overflow_hidden().text_ellipsis().child(title))
                .when(opening, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_FAINT))
                            .child("opening…"),
                    )
                }),
        )
        .child(
            div()
                .pl(px(14.))
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(workspace_name),
        )
}

/// One chat message: author label over the text; user messages get a bubble.
pub fn chat_message(
    author: &'static str,
    author_color: u32,
    text: impl Into<SharedString>,
    bubble: bool,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(author_color)).child(author))
        .child(
            div()
                .when(bubble, |style| {
                    style.bg(rgb(theme::INPUT_BG)).rounded_md().px_3().py_2()
                })
                .child(text.into()),
        )
}

/// The bordered container around a `TextInput`; accent border while focused.
/// Caller adds the input as a child.
pub fn input_frame(focused: bool) -> Div {
    div()
        .bg(rgb(theme::INPUT_BG))
        .rounded_md()
        .border_1()
        .border_color(if focused {
            rgb(theme::ACCENT)
        } else {
            rgb(theme::BORDER)
        })
        .px_2()
        .py_1()
}

/// A modal dialog panel: title header, then whatever body the caller adds.
/// Overlay/backdrop wiring belongs to the caller (it owns dismissal).
pub fn modal_frame(title: impl Into<SharedString>) -> Div {
    div()
        .w(px(MODAL_WIDTH))
        .bg(rgb(theme::ELEVATED_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .rounded_xl()
        .shadow_lg()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_color(rgb(theme::TEXT))
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        )
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
