use gpui::{div, prelude::*, rems, App, Div, ElementId, Rems, SharedString, Stateful};
use gpui_component::list::ListItem;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex};

use super::core::{status_dot_for_tone, Tone};

pub const SIDEBAR_ROW_GUTTER: Rems = rems(0.25);
pub(super) const SIDEBAR_ROW_PADDING: Rems = rems(0.75);
const SIDEBAR_CONTENT_INSET: Rems = rems(SIDEBAR_ROW_GUTTER.0 + SIDEBAR_ROW_PADDING.0);
const SIDEBAR_STATUS_LANE: Rems = rems(1.);

const SIDEBAR_ROW_TAB_INDEX: isize = 0;
const STATUS_LANE_ELEMENT_ID: &str = "status";

pub fn sidebar_row(id: impl Into<ElementId>) -> ListItem {
    ListItem::new(id)
        .mx(SIDEBAR_ROW_GUTTER)
        .px(SIDEBAR_ROW_PADDING)
        .py_2()
        .rounded_md()
        .tab_index(SIDEBAR_ROW_TAB_INDEX)
}

pub fn sidebar_stacked_row(
    tone: Tone,
    title: SharedString,
    detail: Option<SharedString>,
    note: Option<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .min_w_0()
        .child(
            h_flex()
                .child(status_lane(tone, cx))
                .child(stacked_row_title(title, note, cx)),
        )
        .children(detail.map(|detail| stacked_row_detail(detail, cx)))
}

fn status_lane(tone: Tone, cx: &App) -> Stateful<Div> {
    let label = tone.label();

    h_flex()
        .id(STATUS_LANE_ELEMENT_ID)
        .flex_none()
        .w(SIDEBAR_STATUS_LANE)
        .aria_label(label)
        .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
        .child(status_dot_for_tone(tone, cx))
}

fn stacked_row_title(title: SharedString, note: Option<SharedString>, cx: &App) -> Div {
    h_flex()
        .flex_1()
        .min_w_0()
        .gap_2()
        .child(div().overflow_hidden().text_ellipsis().child(title))
        .children(note.map(|note| {
            div()
                .flex_none()
                .text_sm()
                .text_color(Tone::Muted.color(cx))
                .child(note)
        }))
}

fn stacked_row_detail(detail: SharedString, cx: &App) -> Div {
    div()
        .pl(SIDEBAR_STATUS_LANE)
        .text_sm()
        .text_color(Tone::Muted.color(cx))
        .overflow_hidden()
        .text_ellipsis()
        .child(detail)
}

pub fn sidebar_empty_note(text: &'static str, cx: &App) -> Div {
    div()
        .px(SIDEBAR_CONTENT_INSET)
        .py_2()
        .text_sm()
        .text_color(Tone::Muted.color(cx))
        .child(text)
}
