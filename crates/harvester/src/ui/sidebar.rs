use gpui::{div, prelude::*, px, App, Div, ElementId, SharedString};
use gpui_component::list::ListItem;
use gpui_component::{h_flex, v_flex};

use super::core::{status_dot, Dot, Tone, BASE_TEXT_SIZE};

const SIDEBAR_ROW_INSET: f32 = 6.;
const STACKED_ROW_DETAIL_INDENT: f32 = 14.;

pub fn sidebar_row(id: impl Into<ElementId>) -> ListItem {
    ListItem::new(id)
        .mx(px(SIDEBAR_ROW_INSET))
        .px(px(SIDEBAR_ROW_INSET))
        .py_1()
        .rounded_md()
        .text_size(px(BASE_TEXT_SIZE))
}

pub fn sidebar_stacked_row(
    dot: Dot,
    title: SharedString,
    detail: Option<SharedString>,
    note: Option<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .min_w(px(0.))
        .child(
            h_flex()
                .gap_2()
                .child(status_dot(dot, cx))
                .child(div().overflow_hidden().text_ellipsis().child(title))
                .children(note.map(|note| {
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(Tone::Muted.color(cx))
                        .child(note)
                })),
        )
        .children(detail.map(|detail| {
            div()
                .pl(px(STACKED_ROW_DETAIL_INDENT))
                .text_xs()
                .text_color(Tone::Muted.color(cx))
                .overflow_hidden()
                .text_ellipsis()
                .child(detail)
        }))
}

pub fn sidebar_empty_note(text: &'static str, cx: &App) -> Div {
    div()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(Tone::Muted.color(cx))
        .child(text)
}
