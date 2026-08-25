use gpui::{div, prelude::*, relative, transparent_black, App, Div, Role, SharedString, Stateful};
use gpui_component::tooltip::Tooltip;
use gpui_component::ActiveTheme as _;

use super::core::{dim, disclosure_chevron, faint, mono_font_family, status_dot_for_tone, Tone};

const HEADER_ELEMENT_ID: &str = "tool-block-header";
const STATUS_ELEMENT_ID: &str = "tool-block-status";

const HEADER_TAB_INDEX: isize = 0;

const CODE_LINE_HEIGHT: f32 = 1.45;

const TRUNCATION_NOTICE: &str = "Output truncated";

pub(super) fn block(cx: &App) -> Div {
    div()
        .rounded(cx.theme().radius_lg)
        .bg(cx.theme().popover)
        .border_1()
        .border_color(cx.theme().border)
        .flex()
        .flex_col()
        .overflow_hidden()
}

pub(super) fn header(
    index: usize,
    tone: Tone,
    status_label: SharedString,
    accessible_name: SharedString,
    label: Div,
    expanded: bool,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id((HEADER_ELEMENT_ID, index))
        .role(Role::Button)
        .aria_label(accessible_name)
        .aria_expanded(expanded)
        .px_3()
        .py_2()
        .border_1()
        .border_color(transparent_black())
        .tab_index(HEADER_TAB_INDEX)
        .focus_visible(|style| style.border_color(cx.theme().ring))
        .flex()
        .items_center()
        .gap_2()
        .hover(|style| style.bg(cx.theme().list_hover))
        .active(|style| style.bg(cx.theme().muted))
        .child(status(index, tone, status_label, cx))
        .child(label.flex_1().min_w_0())
        .child(
            div()
                .flex_none()
                .text_color(faint(cx))
                .child(disclosure_chevron(expanded)),
        )
}

fn status(index: usize, tone: Tone, status_label: SharedString, cx: &App) -> Stateful<Div> {
    div()
        .id((STATUS_ELEMENT_ID, index))
        .flex_none()
        .size_4()
        .flex()
        .items_center()
        .justify_center()
        .tooltip(move |window, cx| Tooltip::new(status_label.clone()).build(window, cx))
        .child(status_dot_for_tone(tone, cx))
}

pub(super) fn label() -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .overflow_hidden()
        .text_sm()
}

pub(super) fn verb(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_none().text_color(dim(cx)).child(text.into())
}

pub(super) fn subject(text: SharedString, mono: bool, cx: &App) -> Div {
    div()
        .overflow_hidden()
        .text_ellipsis()
        .when(mono, |subject| subject.font_family(mono_font_family(cx)))
        .child(text)
}

pub(super) fn stats(added: usize, removed: usize, cx: &App) -> Div {
    div()
        .flex_none()
        .text_color(faint(cx))
        .child(format!("+{added} −{removed}"))
}

pub(super) fn body(cx: &App) -> Div {
    div()
        .border_t_1()
        .border_color(cx.theme().border)
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_sm()
        .line_height(relative(CODE_LINE_HEIGHT))
        .font_family(mono_font_family(cx))
}

pub(super) fn code_lines() -> Div {
    div().flex().flex_col()
}

pub(super) fn context(text: SharedString, cx: &App) -> Div {
    div().text_color(faint(cx)).child(text)
}

pub(super) fn output(text: SharedString, cx: &App) -> Div {
    div().text_color(dim(cx)).child(text)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DiffLineKind {
    Context,
    Added,
    Removed,
}

pub(super) fn diff_line(kind: DiffLineKind, text: SharedString, cx: &App) -> Div {
    let color = match kind {
        DiffLineKind::Context => dim(cx),
        DiffLineKind::Added => cx.theme().success,
        DiffLineKind::Removed => cx.theme().danger,
    };

    div().text_color(color).child(text)
}

pub(super) fn truncation_notice(cx: &App) -> Div {
    div().text_color(faint(cx)).child(TRUNCATION_NOTICE)
}
