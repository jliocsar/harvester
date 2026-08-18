//! The extension-dialog modal: pi asks, the user answers, pi unblocks.
//!
//! An extension calling `select`/`confirm`/`input`/`editor` suspends the agent
//! until a response comes back (docs/DAEMON.md §Dialogs). The daemon only
//! auto-cancels into an *empty* room, so with this window attached the
//! obligation is ours — and until this module existed, Harvester discharged it
//! by cancelling every question sight-unseen (SPEC C5).
//!
//! What the question *is* comes from
//! [`DialogRequest`](harvester_client_core::DialogRequest), which owns pi's
//! wire shape. This file owns only how it looks and how a click becomes a
//! [`DialogAnswer`].

use std::rc::Rc;

use gpui::{prelude::*, px, App, Context, Entity, SharedString, Window};
use gpui_component::button::Button;
use gpui_component::dialog::{DialogButtonProps, DialogClose};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, WindowExt as _};
use harvester_client_core::{DialogAnswer, DialogKind, DialogObligation};

/// Wide enough for a prompt an extension wrote without knowing our layout, and
/// narrow enough to stay a dialog rather than a second window. The library's
/// own default is 448px, which crops a `select` option of any length.
const DIALOG_WIDTH: f32 = 520.;
/// Visible rows for an `editor` before it scrolls. A `set_editor_text`-style
/// body is usually a commit message, so a handful of lines is the shape.
const EDITOR_ROWS: (usize, usize) = (6, 16);
/// The title shown for a dialog whose own title pi omitted. It still blocks the
/// agent, so it is shown rather than dropped.
const UNTITLED: &str = "The agent is asking";

/// The text box a dialog needs, if it needs one. `editor` is pi's multi-line
/// question and `input` its single-line one, which is `InputState`'s own
/// default.
pub fn create_input(
    kind: &DialogKind,
    window: &mut Window,
    cx: &mut App,
) -> Option<Entity<InputState>> {
    match kind {
        DialogKind::Input { placeholder } => {
            let placeholder = placeholder.clone().unwrap_or_default();

            Some(cx.new(|cx| InputState::new(window, cx).placeholder(placeholder)))
        }
        DialogKind::Editor { prefill } => {
            let prefill = prefill.clone().unwrap_or_default();

            Some(cx.new(|cx| {
                InputState::new(window, cx)
                    .multi_line(true)
                    .auto_grow(EDITOR_ROWS.0, EDITOR_ROWS.1)
                    .default_value(prefill)
            }))
        }
        DialogKind::Select { .. } | DialogKind::Confirm { .. } => None,
    }
}

/// Put `dialog` on screen. `answered` runs on the owner with what the user
/// decided, once per dialog — the modal is already gone by then.
///
/// Every path out of a dialog lands in `answered`, including dismissal, which
/// is the point: an `extension_ui_request` that goes unanswered leaves pi
/// blocked for the life of the process.
pub fn open<T: 'static>(
    dialog: &DialogObligation,
    input: Option<Entity<InputState>>,
    answered: impl Fn(&mut T, DialogAnswer, &mut Window, &mut Context<T>) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    // `Rc` because every route out of the dialog needs it and the library's
    // builder is an `Fn` re-run on each frame, so nothing here can be consumed.
    let answered = Rc::new(answered);
    let owner = cx.entity();
    let answer = {
        let answered = answered.clone();

        move |answer: DialogAnswer, window: &mut Window, cx: &mut App| {
            owner.update(cx, |owner, cx| answered(owner, answer, window, cx));
        }
    };

    let title = dialog_title(&dialog.request.title);
    let dismissal = DialogAnswer::dismissal_for(&dialog.request);

    match &dialog.request.kind {
        DialogKind::Select { options } => {
            open_select(title, options.clone(), dismissal, answer, window, cx)
        }
        DialogKind::Confirm { message } => {
            open_confirm(title, message.clone(), dismissal, answer, window, cx)
        }
        DialogKind::Input { .. } | DialogKind::Editor { .. } => {
            let Some(input) = input else {
                return;
            };

            open_text(title, input, dismissal, answer, window, cx)
        }
    }
}

fn dialog_title(title: &str) -> SharedString {
    if title.is_empty() {
        return UNTITLED.into();
    }

    title.to_string().into()
}

/// One button per option, because a pi `select` is a short list written by an
/// extension author — a searchable list would be ceremony around three choices.
/// The option's *text* is the answer; pi has no notion of an index here.
fn open_select<T: 'static>(
    title: SharedString,
    options: Vec<String>,
    dismissal: DialogAnswer,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);

    window.open_dialog(cx, move |dialog, _, _| {
        let answer = answer.clone();
        let dismissal = dismissal.clone();
        let rows = options.iter().enumerate().map({
            let answer = answer.clone();

            move |(index, option)| {
                let option = option.clone();
                let answer = answer.clone();

                Button::new(("dialog-option", index))
                    .outline()
                    .w_full()
                    .label(option.clone())
                    .on_click(move |_, window, cx| {
                        window.close_dialog(cx);
                        answer(DialogAnswer::Value(option.clone()), window, cx);
                    })
            }
        });

        dialog
            .title(title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(v_flex().gap_2().children(rows))
            // A custom footer replaces the library's OK button, which would be
            // meaningless here: picking *is* confirming.
            .footer(
                DialogClose::new().child(Button::new("dialog-dismiss").outline().label("Cancel")),
            )
            .on_cancel({
                let answer = answer.clone();

                move |_, window, cx| {
                    answer(dismissal.clone(), window, cx);

                    true
                }
            })
    });
}

/// Yes/no. Dismissal answers "no" rather than cancelling, because pi resolves a
/// cancelled `confirm` to `false` anyway — so treating Escape as a refusal
/// loses nothing and leaves one less way for the agent to hang.
fn open_confirm<T: 'static>(
    title: SharedString,
    message: String,
    dismissal: DialogAnswer,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);

    window.open_dialog(cx, move |dialog, _, _| {
        let confirmed = answer.clone();
        let declined = answer.clone();
        let dismissal = dismissal.clone();

        dialog
            .title(title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(message.clone())
            // Before the callbacks: `button_props` replaces the whole struct
            // they are stored in, so setting it after would drop them.
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Yes")
                    .cancel_text("No")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                confirmed(DialogAnswer::Confirmed(true), window, cx);

                true
            })
            .on_cancel(move |_, window, cx| {
                declined(dismissal.clone(), window, cx);

                true
            })
    });
}

/// `input` and `editor`: the same dialog over a different box.
fn open_text<T: 'static>(
    title: SharedString,
    input: Entity<InputState>,
    dismissal: DialogAnswer,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);
    let shown = input.clone();

    window.open_dialog(cx, move |dialog, _, _| {
        let submitted = answer.clone();
        let cancelled = answer.clone();
        let dismissal = dismissal.clone();
        let typed = shown.clone();

        dialog
            .title(title.clone())
            .width(px(DIALOG_WIDTH))
            .close_button(false)
            .child(Input::new(&shown))
            .button_props(
                DialogButtonProps::default()
                    .ok_text("Send")
                    .cancel_text("Cancel")
                    .show_cancel(true),
            )
            .on_ok(move |_, window, cx| {
                let value = typed.read(cx).value().to_string();

                submitted(DialogAnswer::Value(value), window, cx);

                true
            })
            .on_cancel(move |_, window, cx| {
                cancelled(dismissal.clone(), window, cx);

                true
            })
    });

    // `open_dialog` focuses the dialog frame it just built, not anything
    // inside it — so the box this whole dialog exists to have filled in would
    // need a click before it would take a keystroke.
    input.update(cx, |state, cx| state.focus(window, cx));
}
