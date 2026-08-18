use std::rc::Rc;

use gpui::{div, prelude::*, px, Anchor, App, Div, ElementId, SharedString, Stateful, Window};
use gpui_component::popover::Popover;
use gpui_component::{ActiveTheme as _, Icon, Selectable};

use super::core::{dim, faint};

const PROMPT_BOX_BORDER_OPACITY: f32 = 0.5;
const DROPDOWN_MAX_HEIGHT: f32 = 280.;
const DROPDOWN_MIN_WIDTH: f32 = 200.;
const DROPDOWN_CHECK_WIDTH: f32 = 12.;
const SEND_BUTTON_SIZE: f32 = 22.;
const PROMPT_CONTROL_MAX_WIDTH: f32 = 220.;

pub fn prompt_box(focused: bool, cx: &App) -> Div {
    div()
        .rounded_xl()
        .bg(cx.theme().input)
        .border_1()
        .border_color(if focused {
            cx.theme().ring
        } else {
            cx.theme().border.opacity(PROMPT_BOX_BORDER_OPACITY)
        })
        .flex()
        .flex_col()
}

pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

pub fn prompt_box_controls() -> Div {
    div().px_3().pb_2().flex().items_center().gap_4()
}

pub struct DropdownRow {
    pub label: SharedString,
    pub selected: bool,
    pub divider_above: bool,
}

#[derive(IntoElement)]
struct PlainTrigger(Stateful<Div>);

impl RenderOnce for PlainTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        self.0
    }
}

impl Selectable for PlainTrigger {
    fn selected(self, _selected: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

pub fn prompt_box_dropdown(
    id: &'static str,
    trigger: Stateful<Div>,
    rows: Vec<DropdownRow>,
    picked: impl Fn(usize, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let rows = Rc::new(rows);
    let picked = Rc::new(picked);

    Popover::new(id)
        .anchor(Anchor::BottomLeft)
        .trigger(PlainTrigger(trigger))
        .content(move |_, _, cx| {
            let popover = cx.entity();
            let rows = rows.clone();
            let picked = picked.clone();

            div()
                .id("dropdown-rows")
                .flex()
                .flex_col()
                .min_w(px(DROPDOWN_MIN_WIDTH))
                .max_h(px(DROPDOWN_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(rows.iter().enumerate().map(|(index, row)| {
                    let popover = popover.clone();
                    let picked = picked.clone();

                    dropdown_row(index, row, cx).on_click(move |_, window, cx| {
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                        picked(index, window, cx);
                    })
                }))
        })
}

fn dropdown_row(index: usize, row: &DropdownRow, cx: &App) -> Stateful<Div> {
    div()
        .id(("dropdown-row", index))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .when(row.divider_above, |row| {
            row.mt_1()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
        })
        .hover(|style| style.bg(cx.theme().accent))
        .child(
            div()
                .flex_none()
                .w(px(DROPDOWN_CHECK_WIDTH))
                .text_color(cx.theme().muted_foreground)
                .child(if row.selected { "✓" } else { "" }),
        )
        .child(
            div()
                .overflow_hidden()
                .text_ellipsis()
                .child(row.label.clone()),
        )
}

pub fn prompt_box_control(
    id: impl Into<ElementId>,
    icon: Icon,
    label: SharedString,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1()
        .text_xs()
        .max_w(px(PROMPT_CONTROL_MAX_WIDTH))
        .child(icon);
    let label_text = div().overflow_hidden().text_ellipsis();

    if enabled {
        base.text_color(dim(cx))
            .cursor_pointer()
            .hover(|style| style.text_color(cx.theme().foreground))
            .child(label_text.child(format!("{label} ▾")))
    } else {
        base.text_color(faint(cx)).child(label_text.child(label))
    }
}

pub fn prompt_box_send(id: impl Into<ElementId>, enabled: bool, cx: &App) -> Stateful<Div> {
    let base = div()
        .id(id)
        .size(px(SEND_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .child("↑");

    if !enabled {
        return base
            .bg(cx.theme().muted)
            .text_color(cx.theme().muted_foreground);
    }

    base.bg(cx.theme().primary)
        .text_color(cx.theme().primary_foreground)
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().primary_hover))
        .active(|style| style.bg(cx.theme().primary_active))
}
