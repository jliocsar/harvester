//! Visual primitives for one transcript tool-call block.
//!
//! Tool-specific labeling and body policy live with transcript presentation;
//! this module owns the reusable frame, typography, and status treatment.

use gpui::{div, prelude::*, px, App, Div, ElementId, SharedString, Stateful};
use gpui_component::ActiveTheme as _;

use super::core::{dim, faint, mono_font_family, status_dot, Dot, Tone};

/// A rounded card holding a header and, when expanded, a body.
pub(super) fn block(cx: &App) -> Div {
    div()
        .rounded_lg()
        .bg(cx.theme().sidebar)
        .border_1()
        .border_color(cx.theme().border)
        .flex()
        .flex_col()
        .overflow_hidden()
}

/// The clickable one-line header: status dot, composed label, and chevron.
pub(super) fn header(
    id: impl Into<ElementId>,
    tone: Tone,
    label: Div,
    expanded: bool,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .child(status_dot(Dot::Solid(tone), cx))
        .child(label.flex_1().min_w(px(0.)))
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(faint(cx))
                .child(if expanded { "▾" } else { "▸" }),
        )
}

/// Header label row; compose with [`verb`], [`subject`], and [`stats`].
pub(super) fn label() -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .overflow_hidden()
        .text_sm()
}

/// The label's dim leading verb ("Read", "Edit", "$", …).
pub(super) fn verb(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_none().text_color(dim(cx)).child(text.into())
}

/// The label's subject: basename, pattern, or command line.
pub(super) fn subject(text: SharedString, mono: bool) -> Div {
    div()
        .overflow_hidden()
        .text_ellipsis()
        .when(mono, |subject| subject.font_family(mono_font_family()))
        .child(text)
}

/// "+N −N" line stats for edit and write labels.
pub(super) fn stats(added: usize, removed: usize, cx: &App) -> Div {
    div()
        .flex_none()
        .flex()
        .gap_1()
        .text_xs()
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("−{removed}")),
        )
}

/// Expanded body below the header: monospace, compact, and hairline-separated.
pub(super) fn body(cx: &App) -> Div {
    div()
        .border_t_1()
        .border_color(cx.theme().border)
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .font_family(mono_font_family())
}

/// Faint full-path, full-command, or raw-args context line.
pub(super) fn context(text: SharedString, cx: &App) -> Div {
    div().text_color(faint(cx)).child(text)
}

/// Captured result or live partial output.
pub(super) fn output(text: SharedString, cx: &App) -> Div {
    div().text_color(dim(cx)).child(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffLineKind {
    Context,
    Added,
    Removed,
}

/// One line of an edit or write diff.
pub(super) fn diff_line(kind: DiffLineKind, text: SharedString, cx: &App) -> Div {
    let color = match kind {
        DiffLineKind::Context => dim(cx),
        DiffLineKind::Added => cx.theme().success,
        DiffLineKind::Removed => cx.theme().danger,
    };

    div().text_color(color).child(text)
}

/// Notice above tail-kept output when the head was dropped.
pub(super) fn truncation_notice(cx: &App) -> Div {
    div().text_color(faint(cx)).child("… output truncated")
}
