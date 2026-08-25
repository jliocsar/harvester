use gpui::{
    div, prelude::*, px, App, BoxShadow, ClickEvent, Div, ElementId, FontWeight, Pixels, Role,
    SharedString, Stateful, Window,
};
use gpui_component::ActiveTheme as _;

use super::core::{dim, disclosure_chevron, faint};
use super::sidebar::{SIDEBAR_ROW_GUTTER, SIDEBAR_ROW_PADDING};

const FOCUS_RING_SPREAD: Pixels = px(1.5);
const FOCUS_RING_OFFSET: Pixels = px(0.);

const SECTION_HEADER_TAB_INDEX: isize = 0;

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
