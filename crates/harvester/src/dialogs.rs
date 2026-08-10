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

use std::collections::VecDeque;
use std::rc::Rc;

use gpui::{prelude::*, px, App, Context, Entity, SharedString, Window};
use gpui_component::button::Button;
use gpui_component::dialog::{DialogButtonProps, DialogClose};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, WindowExt as _};
use harvester_client_core::{DialogAnswer, DialogKind, DialogRequest};

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

/// A question waiting on the user, and where its answer has to go.
///
/// Holds the machine and sid because an answer is a passthrough to *that*
/// session's pi, and by the time the user clicks, the chat on screen may have
/// moved on.
pub struct PendingDialog {
    pub machine: String,
    pub sid: String,
    pub request: DialogRequest,
    /// The box an `input` or `editor` answer is typed into; `None` for the two
    /// questions that are answered by pointing. Built when the dialog is
    /// queued rather than when it is shown, because queuing is where the
    /// `Window` an input needs is in hand.
    input: Option<Entity<InputState>>,
}

impl PendingDialog {
    pub fn new(
        machine: String,
        sid: String,
        request: DialogRequest,
        window: &mut Window,
        cx: &mut App,
    ) -> PendingDialog {
        let input = create_input(&request.kind, window, cx);

        PendingDialog {
            machine,
            sid,
            request,
            input,
        }
    }

    fn title(&self) -> SharedString {
        if self.request.title.is_empty() {
            return UNTITLED.into();
        }

        self.request.title.clone().into()
    }

    /// What makes two sightings the same question. The sid is half of it: pi
    /// mints dialog ids per process, so an id only means anything alongside
    /// the session that asked.
    fn same_question_as(&self, other: &PendingDialog) -> bool {
        self.sid == other.sid && self.request.id == other.request.id
    }
}

/// The questions waiting on the user, oldest first; the front one is the modal
/// on screen.
///
/// A queue rather than one slot because the daemon replays *every* pending
/// dialog to a client that attaches (docs/DAEMON.md §Dialogs), so several
/// arriving at once is normal after a reconnect — and stacking modals would
/// hide all but the last, leaving pi blocked on a question nobody saw.
///
/// Separate from the view that shows it so the ordering rules can be tested
/// without a window, which is where the mistakes are: a replay treated as a
/// second question, or a queue that never advances.
#[derive(Default)]
pub struct DialogQueue {
    pending: VecDeque<PendingDialog>,
}

impl DialogQueue {
    /// Take a question, answering whether it is now the one that belongs on
    /// screen.
    ///
    /// `false` covers both a question queued behind another and a replay of
    /// one already waiting — the daemon re-sends every pending dialog to a
    /// client that attaches, so the same question arriving twice is by design.
    pub fn push(&mut self, dialog: PendingDialog) -> bool {
        let asked_already = self
            .pending
            .iter()
            .any(|pending| pending.same_question_as(&dialog));

        if asked_already {
            return false;
        }

        self.pending.push_back(dialog);

        self.pending.len() == 1
    }

    pub fn front(&self) -> Option<&PendingDialog> {
        self.pending.front()
    }

    /// The question on screen, now answered and no longer owed one.
    pub fn take_front(&mut self) -> Option<PendingDialog> {
        self.pending.pop_front()
    }

    /// Drop every question `keep` rejects. `true` when the one on screen was
    /// among them — the caller's cue to take the modal down.
    pub fn retain(&mut self, keep: impl Fn(&PendingDialog) -> bool) -> bool {
        let showing = self.pending.front().is_some_and(|dialog| !keep(dialog));

        self.pending.retain(|dialog| keep(dialog));

        showing
    }
}

/// The text box a dialog needs, if it needs one. `editor` is pi's multi-line
/// question and `input` its single-line one, which is `InputState`'s own
/// default.
fn create_input(
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
    dialog: &PendingDialog,
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

    match &dialog.request.kind {
        DialogKind::Select { options } => {
            open_select(dialog.title(), options.clone(), answer, window, cx)
        }
        DialogKind::Confirm { message } => {
            open_confirm(dialog.title(), message.clone(), answer, window, cx)
        }
        DialogKind::Input { .. } | DialogKind::Editor { .. } => {
            let Some(input) = dialog.input.clone() else {
                return;
            };

            open_text(dialog.title(), input, answer, window, cx)
        }
    }
}

/// One button per option, because a pi `select` is a short list written by an
/// extension author — a searchable list would be ceremony around three choices.
/// The option's *text* is the answer; pi has no notion of an index here.
fn open_select<T: 'static>(
    title: SharedString,
    options: Vec<String>,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);

    window.open_dialog(cx, move |dialog, _, _| {
        let answer = answer.clone();
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
                    answer(DialogAnswer::Cancelled, window, cx);

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
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);

    window.open_dialog(cx, move |dialog, _, _| {
        let confirmed = answer.clone();
        let declined = answer.clone();

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
                declined(DialogAnswer::Confirmed(false), window, cx);

                true
            })
    });
}

/// `input` and `editor`: the same dialog over a different box.
fn open_text<T: 'static>(
    title: SharedString,
    input: Entity<InputState>,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let answer = Rc::new(answer);
    let shown = input.clone();

    window.open_dialog(cx, move |dialog, _, _| {
        let submitted = answer.clone();
        let cancelled = answer.clone();
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
                cancelled(DialogAnswer::Cancelled, window, cx);

                true
            })
    });

    // `open_dialog` focuses the dialog frame it just built, not anything
    // inside it — so the box this whole dialog exists to have filled in would
    // need a click before it would take a keystroke.
    input.update(cx, |state, cx| state.focus(window, cx));
}

/// The queue's ordering rules, which is everything about this module that can
/// be wrong without a window being open.
#[cfg(test)]
mod tests {
    use super::*;

    /// A question with no text box, which is every `select` and `confirm` — and
    /// the reason the queue is testable at all: an input would need a `Window`.
    fn question(sid: &str, id: &str) -> PendingDialog {
        PendingDialog {
            machine: "vm".to_string(),
            sid: sid.to_string(),
            request: DialogRequest {
                id: id.to_string(),
                title: "Which branch?".to_string(),
                kind: DialogKind::Confirm {
                    message: "sure?".to_string(),
                },
            },
            input: None,
        }
    }

    #[test]
    fn the_first_question_is_the_one_to_show() {
        let mut queue = DialogQueue::default();

        assert!(queue.push(question("01H", "dlg-1")));
    }

    #[test]
    fn the_replayed_copy_of_a_question_is_not_a_second_one() {
        // The daemon re-sends every pending dialog to a client that attaches
        // (docs/DAEMON.md §Dialogs), so the same question arriving twice is by
        // design. Queuing it again would leave a modal behind for a dialog pi
        // has already had answered.
        let mut queue = DialogQueue::default();

        queue.push(question("01H", "dlg-1"));

        assert!(
            !queue.push(question("01H", "dlg-1")),
            "the replay must not re-open anything"
        );

        queue.take_front();

        assert!(queue.front().is_none(), "one question, one entry");
    }

    #[test]
    fn the_same_id_in_another_session_is_a_different_question() {
        // Pi mints dialog ids per process, so an id only means anything
        // alongside the session that asked.
        let mut queue = DialogQueue::default();

        queue.push(question("01H-first", "dlg-1"));

        assert!(!queue.push(question("01H-second", "dlg-1")), "it queues");
        assert_eq!(queue.pending.len(), 2);
    }

    #[test]
    fn a_second_question_waits_rather_than_covering_the_first() {
        let mut queue = DialogQueue::default();

        queue.push(question("01H", "dlg-1"));

        assert!(
            !queue.push(question("01H", "dlg-2")),
            "stacking modals would hide the first"
        );
        assert_eq!(
            queue.front().map(|dialog| dialog.request.id.as_str()),
            Some("dlg-1")
        );
    }

    #[test]
    fn answering_the_front_promotes_the_one_behind_it() {
        let mut queue = DialogQueue::default();

        queue.push(question("01H", "dlg-1"));
        queue.push(question("01H", "dlg-2"));

        assert_eq!(
            queue.take_front().map(|dialog| dialog.request.id),
            Some("dlg-1".to_string())
        );
        assert_eq!(
            queue.front().map(|dialog| dialog.request.id.as_str()),
            Some("dlg-2"),
            "a queue that does not advance strands pi on the next question"
        );
    }

    #[test]
    fn answering_an_empty_queue_is_nothing_rather_than_a_panic() {
        // Some routes out of a dialog run more than one callback; the second
        // must find the question already gone rather than sending pi a second
        // response.
        let mut queue = DialogQueue::default();

        assert!(queue.take_front().is_none());
    }

    #[test]
    fn a_dead_sessions_questions_come_off_the_screen() {
        let mut queue = DialogQueue::default();

        queue.push(question("01H-dead", "dlg-1"));
        queue.push(question("01H-live", "dlg-2"));

        assert!(
            queue.retain(|dialog| dialog.sid != "01H-dead"),
            "the modal on screen was the dead session's"
        );
        assert_eq!(
            queue.front().map(|dialog| dialog.request.id.as_str()),
            Some("dlg-2")
        );
    }

    #[test]
    fn dropping_a_question_nobody_was_looking_at_leaves_the_screen_alone() {
        // Taking the modal down here would close the *live* session's
        // question, which is still owed an answer.
        let mut queue = DialogQueue::default();

        queue.push(question("01H-live", "dlg-1"));
        queue.push(question("01H-dead", "dlg-2"));

        assert!(!queue.retain(|dialog| dialog.sid != "01H-dead"));
        assert_eq!(
            queue.front().map(|dialog| dialog.request.id.as_str()),
            Some("dlg-1")
        );
    }

    #[test]
    fn attaching_elsewhere_abandons_every_other_sessions_question() {
        // An empty room is where the daemon cancels what is still pending, so
        // those questions have already been answered without us.
        let mut queue = DialogQueue::default();

        queue.push(question("01H-old", "dlg-1"));
        queue.push(question("01H-new", "dlg-2"));

        assert!(queue.retain(|dialog| dialog.sid == "01H-new"));
        assert_eq!(queue.pending.len(), 1);
    }
}
