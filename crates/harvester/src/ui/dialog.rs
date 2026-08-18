use std::rc::Rc;

use gpui::{div, prelude::*, px, App, Context, Div, ElementId, Entity, SharedString, Window};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::dialog::{CancelDialog, DialogAction, DialogClose, DialogFooter};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, WindowExt as _};

const DIALOG_WIDTH: f32 = 520.;
const EDITOR_ROWS: (usize, usize) = (6, 16);
const UNTITLED: &str = "The agent is asking";
const SELECT_OPTION_BUTTON_ID: &str = "dialog-option";
const SELECT_DISMISS_BUTTON_ID: &str = "dialog-dismiss";
const CONFIRMATION_CANCEL_BUTTON_ID: &str = "dialog-confirmation-cancel";
const CONFIRMATION_ACTION_BUTTON_ID: &str = "dialog-confirmation-action";
const TEXT_CANCEL_BUTTON_ID: &str = "dialog-cancel";
const TEXT_SEND_BUTTON_ID: &str = "dialog-send";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActionAccent {
    Primary,
    Danger,
}

#[derive(Debug, Clone)]
pub struct Confirmation {
    pub title: SharedString,
    pub message: SharedString,
    pub cancel_label: SharedString,
    pub action_label: SharedString,
    pub action_accent: ActionAccent,
}

pub fn title_or_fallback(title: &str) -> SharedString {
    if title.is_empty() {
        return UNTITLED.into();
    }

    title.to_string().into()
}

pub fn cancel_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    Button::new(id).label(label)
}

pub fn dismiss_button(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Button {
    cancel_button(id, label)
        .on_click(|_, window, cx| window.dispatch_action(Box::new(CancelDialog), cx))
}

pub fn action_button(
    id: impl Into<ElementId>,
    label: impl Into<SharedString>,
    accent: ActionAccent,
) -> Button {
    let button = Button::new(id).label(label);

    match accent {
        ActionAccent::Primary => button.primary(),
        ActionAccent::Danger => button.danger(),
    }
}

pub fn button_row(cancel: Button, action: Button) -> Div {
    div()
        .flex()
        .justify_end()
        .gap_2()
        .child(cancel)
        .child(action)
}

pub fn create_single_line_input(
    placeholder: String,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
}

pub fn create_multi_line_input(
    prefill: String,
    window: &mut Window,
    cx: &mut App,
) -> Entity<InputState> {
    cx.new(|cx| {
        InputState::new(window, cx)
            .multi_line(true)
            .auto_grow(EDITOR_ROWS.0, EDITOR_ROWS.1)
            .default_value(prefill)
    })
}

pub fn open_select<T: 'static>(
    title: SharedString,
    options: Vec<String>,
    chosen: impl Fn(String, &mut Window, &mut App) -> bool + 'static,
    cancelled: impl Fn(&mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let chosen = Rc::new(chosen);
    let cancelled = Rc::new(cancelled);

    window.open_dialog(cx, move |dialog, _, _| {
        let cancelled = cancelled.clone();
        let rows = options.iter().enumerate().map({
            let chosen = chosen.clone();

            move |(index, option)| {
                let option = option.clone();
                let chosen = chosen.clone();

                Button::new((SELECT_OPTION_BUTTON_ID, index))
                    .outline()
                    .w_full()
                    .label(option.clone())
                    .on_click(move |_, window, cx| {
                        if chosen(option.clone(), window, cx) {
                            window.close_dialog(cx);
                        }
                    })
            }
        });

        dialog
            .title(title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(v_flex().gap_2().children(rows))
            .footer(dismissal_footer(
                cancel_button(SELECT_DISMISS_BUTTON_ID, "Cancel").outline(),
            ))
            .on_ok(|_, _, _| false)
            .on_cancel(move |_, window, cx| cancelled(window, cx))
    });
}

pub fn open_confirmation<T: 'static>(
    confirmation: Confirmation,
    confirmed: impl Fn(&mut Window, &mut App) -> bool + 'static,
    declined: impl Fn(&mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let confirmed = Rc::new(confirmed);
    let declined = Rc::new(declined);

    window.open_dialog(cx, move |dialog, _, _| {
        let confirmed = confirmed.clone();
        let declined = declined.clone();

        dialog
            .title(confirmation.title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(confirmation.message.clone())
            .footer(action_footer(
                cancel_button(
                    CONFIRMATION_CANCEL_BUTTON_ID,
                    confirmation.cancel_label.clone(),
                ),
                action_button(
                    CONFIRMATION_ACTION_BUTTON_ID,
                    confirmation.action_label.clone(),
                    confirmation.action_accent,
                ),
            ))
            .on_ok(move |_, window, cx| confirmed(window, cx))
            .on_cancel(move |_, window, cx| declined(window, cx))
    });
}

pub fn open_text<T: 'static>(
    title: SharedString,
    input: Entity<InputState>,
    submitted: impl Fn(String, &mut Window, &mut App) -> bool + 'static,
    cancelled: impl Fn(&mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let submitted = Rc::new(submitted);
    let cancelled = Rc::new(cancelled);
    let shown = input.clone();

    window.open_dialog(cx, move |dialog, _, _| {
        let submitted = submitted.clone();
        let cancelled = cancelled.clone();
        let typed = shown.clone();

        dialog
            .title(title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(Input::new(&shown))
            .footer(action_footer(
                cancel_button(TEXT_CANCEL_BUTTON_ID, "Cancel"),
                action_button(TEXT_SEND_BUTTON_ID, "Send", ActionAccent::Primary),
            ))
            .on_ok(move |_, window, cx| {
                let value = typed.read(cx).value().to_string();

                submitted(value, window, cx)
            })
            .on_cancel(move |_, window, cx| cancelled(window, cx))
    });

    input.update(cx, |state, cx| state.focus(window, cx));
}

fn dismissal_footer(cancel: Button) -> DialogFooter {
    DialogFooter::new().child(DialogClose::new().child(cancel))
}

fn action_footer(cancel: Button, action: Button) -> DialogFooter {
    DialogFooter::new()
        .child(DialogClose::new().child(cancel))
        .child(DialogAction::new().child(action))
}
