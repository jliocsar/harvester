//! Window-shell visuals: section headers, pane headers, and the status bar.

use gpui::{div, prelude::*, px, App, Div, ElementId, FontWeight, SharedString, Stateful};
use gpui_component::ActiveTheme as _;

use super::core::{dim, faint};

/// Statusbar height; the workbench and the statusbar frame must agree.
const STATUSBAR_HEIGHT: f32 = 26.;

/// Sidebar section label ("MACHINES", "SESSIONS", …). The whole row is the
/// collapse toggle; callers add optional actions and then [`section_chevron`].
pub fn section_header(id: impl Into<ElementId>, title: &'static str, cx: &App) -> Stateful<Div> {
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
        .text_color(faint(cx))
        .cursor_pointer()
        .hover(|style| style.text_color(dim(cx)))
        .child(title.to_uppercase())
        .child(div().flex_1())
}

/// The section header's trailing disclosure chevron.
pub fn section_chevron(expanded: bool) -> Div {
    div().flex_none().child(if expanded { "▾" } else { "▸" })
}

/// Chat pane header: title plus faint subtitle.
pub fn chat_header(title: SharedString, subtitle: SharedString, cx: &App) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(cx.theme().border)
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
                .text_color(faint(cx))
                .overflow_hidden()
                .text_ellipsis()
                .child(subtitle),
        )
}

/// The bottom statusbar container; callers add its content children.
pub fn statusbar_frame(cx: &App) -> Div {
    div()
        .h(px(STATUSBAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .border_t_1()
        .border_color(cx.theme().status_bar_border)
        .bg(cx.theme().status_bar)
        .text_xs()
}
