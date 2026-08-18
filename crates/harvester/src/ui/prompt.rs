//! The agent prompt card and its in-place model and machine controls.

use std::rc::Rc;

use gpui::{div, prelude::*, px, Anchor, App, Div, ElementId, SharedString, Stateful, Window};
use gpui_component::popover::Popover;
use gpui_component::{ActiveTheme as _, Icon, Selectable};

use super::core::{dim, faint};

/// How far the prompt box's resting border is faded. Focus still brings the
/// full theme ring.
const PROMPT_BOX_BORDER_OPACITY: f32 = 0.5;
/// Cap an open dropdown so a long model list scrolls within the window.
const DROPDOWN_MAX_HEIGHT: f32 = 280.;
/// Keep short-label dropdowns from opening as a sliver beneath a wide control.
const DROPDOWN_MIN_WIDTH: f32 = 200.;
/// Fixed check gutter keeps picked and unpicked row labels aligned.
const DROPDOWN_CHECK_WIDTH: f32 = 12.;
/// Diameter of the prompt box's round send button.
const SEND_BUTTON_SIZE: f32 = 22.;
/// How wide one prompt-strip control may grow before its label ellipsizes;
/// a long model or repository name must not push the others off the row.
const PROMPT_CONTROL_MAX_WIDTH: f32 = 220.;

/// The agent prompt box: one rounded card holding the input and controls row.
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

/// Padding around the caller's borderless, padding-free `Input`.
pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

/// The prompt box's bottom row.
///
/// Controls sit a `gap_4` apart — wide enough that each icon reads as the
/// prefix of the label to its right, which is what separates the controls
/// instead of a rule or a middle dot.
pub fn prompt_box_controls() -> Div {
    div().px_3().pb_2().flex().items_center().gap_4()
}

/// One row of an in-place dropdown.
pub struct DropdownRow {
    pub label: SharedString,
    /// The choice already in effect — it carries the check mark.
    pub selected: bool,
    /// A rule above this row: what follows belongs to a different group than
    /// what precedes it (the repo picker's "Add new…" is an action, not one
    /// more repository).
    pub divider_above: bool,
}

/// A [`Popover`] trigger with no selected appearance.
///
/// `Popover::trigger` requires [`Selectable`], which the library does not
/// implement for a styled `div`. The prompt controls stay one text run and a
/// caret, so this local newtype carries the trait without changing their look.
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

/// A prompt-box control that opens its choices in place; `picked` receives the
/// row index.
///
/// Bottom-left anchoring opens upward because the prompt box sits at the pane's
/// bottom. A downward list would run beyond the window and snap over its own
/// trigger.
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

/// One dropdown row: check gutter, label, and hover highlight. A row that
/// opens a group carries the rule that separates it from the one before.
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

/// A flat dropdown control inside the prompt box, prefixed by its icon.
///
/// The icon is given neither a size nor a color: `Icon` falls back to the
/// ambient text style, which is this control's own `text_xs` and muted color,
/// so the glyph and its label always match without a second set of tokens.
/// Hover is resolved after layout and so brightens the label alone, leaving
/// the icon as the quiet marker of *which* control this is.
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

/// The prompt box's round send button. Enter in the input is the other path to
/// the same submit.
///
/// Disabled is a look, not a guard: a chat with no machine or repository
/// refuses the prompt in its own state machine, and this is how the strip says
/// so before the user types.
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
