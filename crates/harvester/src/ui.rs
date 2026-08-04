//! The design system: every reusable visual primitive lives here, and every
//! primitive has a section in the kitchen (`harvester kitchen`) showing its
//! variants. Components return `Div`/`Stateful<Div>` so callers can keep
//! chaining (`.on_click`, extra children); behavior stays with the caller,
//! looks stay here.

use gpui::{div, prelude::*, px, rgb, rgba, Div, ElementId, FontWeight, SharedString, Stateful};

use crate::theme;

/// Statusbar height; the workbench and the kitchen sample must agree.
pub const STATUSBAR_HEIGHT: f32 = 26.;
/// Modal panel width — wide enough for a path, narrow enough to stay a dialog.
pub const MODAL_WIDTH: f32 = 420.;

/// Sidebar section label ("MACHINE", "SESSIONS", …) — Apple-sidebar style:
/// small, semibold, faint.
pub fn section_header(title: &'static str) -> Div {
    div()
        .px_3()
        .pt_3()
        .pb_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(rgb(theme::TEXT_FAINT))
        .child(title.to_uppercase())
}

/// Session liveness indicator (`STATUS_RUNNING` green / `TEXT_FAINT` exited).
pub fn status_dot(color: u32) -> Div {
    div().flex_none().size(px(6.)).rounded_full().bg(rgb(color))
}

pub enum ButtonKind {
    Neutral,
    Primary,
}

/// A clickable button; caller chains `.on_click`. Disabled buttons keep their
/// footprint but drop the pointer affordances — the caller must also skip the
/// click handler. No animation: buttons are high-frequency chrome.
pub fn button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    kind: ButtonKind,
    enabled: bool,
) -> Stateful<Div> {
    // Apple dark-mode push buttons: borderless fills, a whisper of shadow on
    // the enabled ones, flat when disabled.
    let base = div()
        .id(id)
        .px_3()
        .py_1()
        .rounded_md()
        .text_sm()
        .child(label.into());

    match (kind, enabled) {
        (ButtonKind::Neutral, true) => base
            .bg(rgb(theme::BUTTON_BG))
            .text_color(rgb(theme::TEXT))
            .shadow_sm()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::BUTTON_HOVER)))
            .active(|style| style.bg(rgb(theme::BUTTON_ACTIVE))),
        (ButtonKind::Primary, true) => base
            .bg(rgb(theme::ACCENT))
            .text_color(rgb(theme::TEXT_ON_ACCENT))
            .shadow_sm()
            .cursor_pointer()
            .hover(|style| style.bg(rgb(theme::ACCENT_HOVER)))
            .active(|style| style.bg(rgb(theme::ACCENT_ACTIVE))),
        (_, false) => base
            .bg(rgb(theme::BUTTON_BG))
            .text_color(rgb(theme::TEXT_FAINT)),
    }
}

/// One sidebar session row: status dot + title line, workspace line under it,
/// hover/selection as an Apple-sidebar rounded pill (inset from the edges).
/// Caller chains `.on_click`.
pub fn session_row(
    id: impl Into<ElementId>,
    title: SharedString,
    workspace_name: SharedString,
    dot_color: u32,
    selected: bool,
    opening: bool,
) -> Stateful<Div> {
    div()
        .id(id)
        // 6px margin + 6px padding = the 12px text inset section_header uses.
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .rounded_md()
        .flex()
        .flex_col()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
        .when(selected, |style| style.bg(rgb(theme::ROW_SELECTED)))
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(status_dot(dot_color))
                .child(div().overflow_hidden().text_ellipsis().child(title))
                .when(opening, |row| {
                    row.child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_FAINT))
                            .child("opening…"),
                    )
                }),
        )
        .child(
            div()
                .pl(px(14.))
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(workspace_name),
        )
}

/// One sidebar machine row — read-only by design (machines are listed, not
/// clicked): reachability dot + label, detail line under it. The dot is
/// Harvester's own SSH probe, never provider-reported status (SPEC §Machine).
pub fn machine_row(label: SharedString, detail: SharedString, dot_color: u32) -> Div {
    div()
        // Same 6+6 inset as session_row, aligning with section_header text.
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .flex()
        .flex_col()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(status_dot(dot_color))
                .child(div().overflow_hidden().text_ellipsis().child(label)),
        )
        .child(
            div()
                .pl(px(14.))
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(detail),
        )
}

/// One chat message: author label over the text; user messages get a bubble.
pub fn chat_message(
    author: &'static str,
    author_color: u32,
    text: impl Into<SharedString>,
    bubble: bool,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(div().text_xs().text_color(rgb(author_color)).child(author))
        .child(
            div()
                .when(bubble, |style| {
                    style.bg(rgb(theme::INPUT_BG)).rounded_md().px_3().py_2()
                })
                .child(text.into()),
        )
}

/// The agent prompt box (SPEC §Flows "Start session", chat input): one
/// rounded card holding the text input on top and a controls row at the
/// bottom of the same frame; accent border while the input is focused.
/// Compose with `prompt_box_input` and `prompt_box_controls`.
pub fn prompt_box(focused: bool) -> Div {
    div()
        .rounded_xl()
        .bg(rgb(theme::INPUT_BG))
        .border_1()
        .border_color(if focused {
            rgb(theme::ACCENT)
        } else {
            rgb(theme::BORDER)
        })
        .flex()
        .flex_col()
}

/// The prompt box's input slot: padding around the caller's `TextInput`.
pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

/// The prompt box's bottom row. Caller adds `prompt_box_control`s, a
/// `flex_1` spacer, then `prompt_box_send`.
pub fn prompt_box_controls() -> Div {
    div().px_3().pb_2().flex().items_center().gap_2()
}

/// A flat dropdown control inside the prompt box (the model picker trigger):
/// quiet dim text with a caret, brightening on hover. Disabled ones lose the
/// caret and the pointer affordances — the caller must also skip the click
/// handler (loading / no models to offer).
pub fn prompt_box_control(
    id: impl Into<ElementId>,
    label: SharedString,
    enabled: bool,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .text_xs()
        // Cap so an unreasonably long model name leaves room for the rest
        // of the row.
        .max_w(px(220.))
        .overflow_hidden()
        .text_ellipsis();

    if enabled {
        base.text_color(rgb(theme::TEXT_DIM))
            .cursor_pointer()
            .hover(|style| style.text_color(rgb(theme::TEXT)))
            .child(format!("{label} ▾"))
    } else {
        base.text_color(rgb(theme::TEXT_FAINT)).child(label)
    }
}

/// Diameter of the prompt box's round send button.
const SEND_BUTTON_SIZE: f32 = 22.;

/// The prompt box's round accent send button ("↑"). Caller chains
/// `.on_click`; Enter in the input is the other path to the same submit.
pub fn prompt_box_send(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(SEND_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .bg(rgb(theme::ACCENT))
        .text_color(rgb(theme::TEXT_ON_ACCENT))
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ACCENT_HOVER)))
        .active(|style| style.bg(rgb(theme::ACCENT_ACTIVE)))
        .child("↑")
}

/// One sidebar workspace row: `folder_name in machine_label` — read-only by
/// design (workspaces are listed, not clicked). Compose with
/// `workspace_row_add` for the trailing "+" that starts a ghost chat there.
pub fn workspace_row(name: SharedString, machine: SharedString) -> Div {
    div()
        // Same 6+6 inset as session_row, aligning with section_header text.
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .flex()
        .items_baseline()
        .gap_1()
        .child(
            div()
                .flex_none()
                // Cap the folder name so a long one still leaves the machine
                // label readable inside the 240px sidebar.
                .max_w(px(140.))
                .overflow_hidden()
                .text_ellipsis()
                .child(name),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(format!("in {machine}")),
        )
        .child(div().flex_1())
}

/// The workspace row's "+" — the one click target on the row (SPEC §Flows
/// "Start session"): starts a ghost chat in that workspace. Caller chains
/// `.on_click`.
pub fn workspace_row_add(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .px_1()
        .rounded_md()
        .text_color(rgb(theme::TEXT_FAINT))
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(theme::TEXT)).bg(rgb(theme::ROW_HOVER)))
        .child("+")
}

/// One model in the ghost chat's picker; the selected one carries a leading
/// check, the others an equally wide blank so names align. Caller chains
/// `.on_click` (select-and-close — no confirm step).
pub fn model_row(id: impl Into<ElementId>, name: SharedString, selected: bool) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
        .when(selected, |style| style.bg(rgb(theme::ROW_SELECTED)))
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_none()
                .w(px(14.))
                .text_color(rgb(theme::ACCENT))
                .when(selected, |check| check.child("✓")),
        )
        .child(div().overflow_hidden().text_ellipsis().child(name))
}

/// Chat pane header: title + faint subtitle (the ghost chat's "New session"
/// line — the model control lives in the prompt box, not here).
pub fn chat_header(title: SharedString, subtitle: SharedString) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(rgb(theme::BORDER))
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
                .text_color(rgb(theme::TEXT_FAINT))
                .overflow_hidden()
                .text_ellipsis()
                .child(subtitle),
        )
}

/// A clickable directory row in the folder picker (trailing `/` marks it as
/// enterable). Caller chains `.on_click` to navigate into it.
pub fn picker_dir_row(id: impl Into<ElementId>, name: SharedString) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .hover(|style| style.bg(rgb(theme::ROW_HOVER)))
        .overflow_hidden()
        .text_ellipsis()
        .child(format!("{name}/"))
}

/// A non-directory entry in the folder picker: shown faint for orientation,
/// not clickable.
pub fn picker_file_row(name: SharedString) -> Div {
    div()
        .px_2()
        .py_1()
        .text_color(rgb(theme::TEXT_FAINT))
        .overflow_hidden()
        .text_ellipsis()
        .child(name)
}

/// Full-window backdrop that centers its child (the modal). Caller chains
/// `.on_click` for click-outside dismissal; the modal itself must `.occlude()`
/// so clicks inside it don't reach the scrim's hitbox.
pub fn modal_scrim(id: impl Into<ElementId>) -> Stateful<Div> {
    div()
        .id(id)
        .absolute()
        .inset_0()
        .bg(rgba(theme::SCRIM_RGBA))
        .flex()
        .items_center()
        .justify_center()
        .occlude()
}

/// A modal dialog panel: title header, then whatever body the caller adds.
/// Overlay/backdrop wiring belongs to the caller (it owns dismissal).
pub fn modal_frame(title: impl Into<SharedString>) -> Div {
    div()
        .w(px(MODAL_WIDTH))
        .bg(rgb(theme::ELEVATED_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .rounded_xl()
        .shadow_lg()
        .p_4()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_color(rgb(theme::TEXT))
                .font_weight(FontWeight::MEDIUM)
                .child(title.into()),
        )
}

/// The bottom statusbar container; caller adds its content children.
pub fn statusbar_frame() -> Div {
    div()
        .h(px(STATUSBAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .border_t_1()
        .border_color(rgb(theme::BORDER))
        .bg(rgb(theme::PANEL_BG))
        .text_xs()
}
