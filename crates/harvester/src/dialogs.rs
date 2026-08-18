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
//! wire shape. This file owns the client obligation/modal lifecycle and how a
//! click becomes a [`DialogAnswer`].

use std::rc::Rc;

use gpui::{prelude::*, px, App, Context, Entity, SharedString, Window};
use gpui_component::button::Button;
use gpui_component::dialog::{DialogButtonProps, DialogClose};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, WindowExt as _};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::{
    DialogAnswer, DialogKey, DialogKind, DialogObligation, DialogObligations, DialogRequest,
    DialogRoom, EnqueueResult, Retirement,
};

use crate::store::{FleetStore, StoreEvent};

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

/// Owns the complete client-side lifecycle of Pi's blocking extension dialogs.
///
/// The window-free [`DialogObligations`] state decides ordering, duplicate
/// replay, answers, and retirement. This coordinator is the GPUI adapter: it
/// subscribes to the one store, owns text inputs, places the front modal, and
/// sends exactly one answer back through the same machine route.
pub(crate) struct DialogCoordinator {
    store: Entity<FleetStore>,
    obligations: DialogObligations,
    inputs: std::collections::HashMap<DialogKey, Entity<InputState>>,
    _store: gpui::Subscription,
}

impl DialogCoordinator {
    pub(crate) fn new(
        store: Entity<FleetStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let store_subscription = cx.subscribe_in(&store, window, Self::handle_store_event);

        Self {
            store,
            obligations: DialogObligations::default(),
            inputs: std::collections::HashMap::new(),
            _store: store_subscription,
        }
    }

    fn handle_store_event(
        &mut self,
        _store: &Entity<FleetStore>,
        event: &StoreEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Attached { sid, .. },
            } => self.enter_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Pi { sid, event },
            } => self.handle_pi_event(machine, sid, event, window, cx),
            StoreEvent::Bridge {
                event: BridgeEvent::SessionExit { sid, .. },
                ..
            } => self.retire_session(sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Disconnected,
            } => self.leave_machine(machine, window, cx),
            StoreEvent::Bridge { .. } | StoreEvent::Machines(_) | StoreEvent::Failed(_) => {}
        }
    }

    fn handle_pi_event(
        &mut self,
        machine: &str,
        sid: &str,
        event: &serde_json::Value,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(request) = DialogRequest::parse(event) else {
            return;
        };

        self.enqueue(machine, sid, request, window, cx);
    }

    fn enqueue(
        &mut self,
        machine: &str,
        sid: &str,
        request: DialogRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let obligation = DialogObligation::new(machine.to_string(), sid.to_string(), request);
        let result = self.obligations.enqueue(obligation.clone());

        if !matches!(result, EnqueueResult::Visible | EnqueueResult::Queued) {
            return;
        }

        if let Some(input) = create_input(&obligation.request.kind, window, cx) {
            self.inputs.insert(obligation.key(), input);
        }

        if result == EnqueueResult::Visible {
            self.show_front(window, cx);
        }
    }

    fn show_front(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(obligation) = self.obligations.front().cloned() else {
            return;
        };
        let input = self.inputs.get(&obligation.key()).cloned();

        open(&obligation, input, Self::answer_front, window, cx);
    }

    fn answer_front(&mut self, answer: DialogAnswer, window: &mut Window, cx: &mut Context<Self>) {
        // This pop is the duplicate-answer guard. A second callback from the
        // same modal finds no front obligation and sends nothing.
        let Some(answered) = self.obligations.answer_front(answer) else {
            return;
        };
        let obligation = answered.obligation;

        self.inputs.remove(&obligation.key());
        self.store.read(cx).send(
            &obligation.machine,
            Command::AnswerDialog {
                sid: obligation.sid,
                dialog_id: obligation.request.id,
                answer: answered.answer,
            },
        );

        // gpui-component closes the answered dialog after this callback. Open
        // the next one only after that pop, or the library would close the new
        // front instead.
        let coordinator = cx.entity();

        window.defer(cx, move |window, cx| {
            coordinator.update(cx, |coordinator, cx| coordinator.show_front(window, cx));
        });
    }

    fn enter_room(
        &mut self,
        machine: &str,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let retirement = self
            .obligations
            .enter_room(DialogRoom::new(machine.to_string(), sid.to_string()));

        self.apply_retirement(retirement, window, cx);
    }

    fn retire_session(&mut self, sid: &str, window: &mut Window, cx: &mut Context<Self>) {
        let retirement = self.obligations.retire_session(sid);

        self.apply_retirement(retirement, window, cx);
    }

    fn leave_machine(&mut self, machine: &str, window: &mut Window, cx: &mut Context<Self>) {
        let retirement = self.obligations.leave_machine(machine);

        self.apply_retirement(retirement, window, cx);
    }

    fn apply_retirement(
        &mut self,
        retirement: Retirement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for obligation in retirement.retired {
            self.inputs.remove(&obligation.key());
        }

        if !retirement.front_removed {
            return;
        }

        // Programmatic close runs no answer callback. Empty-room cancellation
        // remains daemon policy; retiring a client modal never manufactures an
        // answer.
        window.close_dialog(cx);
        self.show_front(window, cx);
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
fn open<T: 'static>(
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
