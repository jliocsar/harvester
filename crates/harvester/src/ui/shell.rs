use gpui::{
    div, prelude::*, px, App, ClickEvent, Div, ElementId, FontWeight, SharedString, Stateful,
    Window,
};
use gpui_component::ActiveTheme as _;

use super::core::{dim, disclosure_chevron, faint};

const STATUSBAR_HEIGHT: f32 = 24.;

pub fn collapsible_section(
    id: impl Into<ElementId>,
    title: &'static str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    section_header(id, title, cx)
        .child(div().flex_none().child(disclosure_chevron(expanded)))
        .on_click(on_toggle)
}

fn section_header(id: impl Into<ElementId>, title: &'static str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .pt_2()
        .pb_1()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(faint(cx))
        .cursor_pointer()
        .hover(|style| style.text_color(dim(cx)))
        .active(|style| style.text_color(cx.theme().foreground))
        .child(title.to_uppercase())
        .child(div().flex_1())
}

pub fn chat_header(title: SharedString, subtitle: SharedString, cx: &App) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_1p5()
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
