use gpui::{
    div, prelude::*, px, rems, App, BoxShadow, ClickEvent, Div, ElementId, FontWeight, Pixels,
    Rems, Role, SharedString, Stateful, Window,
};
use gpui_component::list::ListItem;
use gpui_component::tooltip::Tooltip;
use gpui_component::{h_flex, v_flex, ActiveTheme as _};

use super::core::{dim, disclosure_chevron, faint, status_dot_for_tone, Tone};

pub const SIDEBAR_ROW_GUTTER: Rems = rems(0.25);
const SIDEBAR_ROW_PADDING: Rems = rems(0.75);
const SIDEBAR_CONTENT_INSET: Rems = rems(SIDEBAR_ROW_GUTTER.0 + SIDEBAR_ROW_PADDING.0);
const SIDEBAR_STATUS_LANE: Rems = rems(1.);

const FOCUS_RING_SPREAD: Pixels = px(1.5);
const FOCUS_RING_OFFSET: Pixels = px(0.);

const SIDEBAR_ROW_TAB_INDEX: isize = 0;
const SECTION_HEADER_TAB_INDEX: isize = 0;

const STATUS_LANE_ELEMENT_ID: &str = "status";

pub trait KeyboardFocusRing: InteractiveElement + Sized {
    fn keyboard_focus_ring(self, cx: &App) -> Self {
        let ring = vec![
            BoxShadow::new(FOCUS_RING_OFFSET, FOCUS_RING_OFFSET, cx.theme().ring)
                .spread_radius(FOCUS_RING_SPREAD),
        ];

        self.focus_visible(move |style| style.shadow(ring))
    }
}

impl<E: InteractiveElement + Sized> KeyboardFocusRing for E {}

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

fn section_header(id: impl Into<ElementId>, title: &'static str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .mx(SIDEBAR_ROW_GUTTER)
        .px(SIDEBAR_ROW_PADDING)
        .pt_4()
        .pb_2()
        .rounded_md()
        .flex()
        .items_center()
        .gap_2()
        .text_sm()
        .text_color(faint(cx))
        .tab_index(SECTION_HEADER_TAB_INDEX)
        .keyboard_focus_ring(cx)
        .hover(|style| style.text_color(dim(cx)))
        .active(|style| style.text_color(cx.theme().foreground))
        .child(title)
        .child(div().flex_1())
}

pub fn collapsible_section(
    id: impl Into<ElementId>,
    title: &'static str,
    expanded: bool,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    cx: &App,
) -> Stateful<Div> {
    section_header(id, title, cx)
        .role(Role::Button)
        .aria_label(title)
        .aria_expanded(expanded)
        .child(div().flex_none().child(disclosure_chevron(expanded)))
        .on_click(on_toggle)
}

pub fn chat_header(title: SharedString, subtitle: SharedString, cx: &App) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_3()
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
                .text_sm()
                .text_color(faint(cx))
                .overflow_hidden()
                .text_ellipsis()
                .child(subtitle),
        )
}

pub fn statusbar_frame(cx: &App) -> Div {
    div()
        .h_8()
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .border_t_1()
        .border_color(cx.theme().status_bar_border)
        .bg(cx.theme().status_bar)
        .text_sm()
}
