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
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::dialog::{DialogAction, DialogClose, DialogFooter};
use gpui_component::input::{Input, InputState};
use gpui_component::{v_flex, WindowExt as _};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::{
    DialogAnswer, DialogKey, DialogKind, DialogObligation, DialogObligations, DialogRequest,
    DialogRoom, EnqueueResult, Retirement,
};

use crate::modal::{ModalAvailable, ModalCoordinator, ModalLease, ModalOwner};
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
    modals: Entity<ModalCoordinator>,
    obligations: DialogObligations,
    inputs: std::collections::HashMap<DialogKey, Entity<InputState>>,
    lease: Option<ModalLease>,
    displayed: Option<DialogKey>,
    _subscriptions: [gpui::Subscription; 2],
}

impl DialogCoordinator {
    pub(crate) fn new(
        store: Entity<FleetStore>,
        modals: Entity<ModalCoordinator>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let subscriptions = [
            cx.subscribe_in(&store, window, Self::handle_store_event),
            cx.subscribe_in(&modals, window, Self::handle_modal_available),
        ];

        Self {
            store,
            modals,
            obligations: DialogObligations::default(),
            inputs: std::collections::HashMap::new(),
            lease: None,
            displayed: None,
            _subscriptions: subscriptions,
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
                event: BridgeEvent::AttachmentLeft { sid },
            } => self.retire_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Attached { sid, .. },
            } => self.enter_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Pi { sid, event },
            } => self.handle_pi_event(machine, sid, event, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::SessionExit { sid, .. },
            } => self.retire_room(machine, sid, window, cx),
            StoreEvent::Bridge {
                machine,
                event: BridgeEvent::Disconnected,
            } => self.leave_machine(machine, window, cx),
            StoreEvent::Bridge { .. } | StoreEvent::Machines(_) | StoreEvent::Failed(_) => {}
        }
    }

    fn handle_modal_available(
        &mut self,
        _modals: &Entity<ModalCoordinator>,
        _event: &ModalAvailable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let coordinator = cx.entity();

        // Add Workspace releases from an on_cancel callback before Root does
        // its automatic pop. Wait until that callback and pop are both done.
        window.defer(cx, move |window, cx| {
            coordinator.update(cx, |coordinator, cx| coordinator.advance(window, cx));
        });
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
            self.advance(window, cx);
        }
    }

    /// Show the global FIFO front only after acquiring Root for Pi. While Add
    /// Workspace owns it, obligations remain queued and no Root call occurs.
    fn advance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.displayed.is_some() {
            return;
        }

        let Some(obligation) = self.obligations.front().cloned() else {
            self.release_modal(cx);

            return;
        };

        if self.lease.is_none() {
            self.lease = self
                .modals
                .update(cx, |modals, _| modals.acquire(ModalOwner::PiDialogs));
        }

        let Some(lease) = self.lease else {
            return;
        };

        if !self.modals.read(cx).owns(lease) {
            self.lease = None;

            return;
        }

        let key = obligation.key();
        let callback_key = key.clone();
        let input = self.inputs.get(&key).cloned();

        self.displayed = Some(key);
        open(
            &obligation,
            input,
            move |coordinator, answer, window, cx| {
                coordinator.answer_front(&callback_key, answer, window, cx)
            },
            window,
            cx,
        );
    }

    fn answer_front(
        &mut self,
        key: &DialogKey,
        answer: DialogAnswer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.displayed.as_ref() != Some(key) {
            return false;
        }

        // The key is also checked by the window-free queue. A duplicate or
        // delayed callback from the old modal can never consume its successor.
        let Some(answered) = self.obligations.answer_front(key, answer) else {
            return false;
        };
        let obligation = answered.obligation;

        self.displayed = None;
        self.inputs.remove(&obligation.key());
        self.store.read(cx).send(
            &obligation.machine,
            Command::AnswerDialog {
                sid: obligation.sid,
                dialog_id: obligation.request.id,
                answer: answered.answer,
            },
        );

        // gpui-component pops after its answer callback; select performs the
        // equivalent guarded pop in its button handler. Defer advancement so
        // the next owner never lands beneath the modal being closed.
        self.defer_advance(window, cx);

        true
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

    fn retire_room(
        &mut self,
        machine: &str,
        sid: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let retirement = self.obligations.retire_room(machine, sid);

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
        let displayed_retired = self.displayed.as_ref().is_some_and(|displayed| {
            retirement
                .retired
                .iter()
                .any(|obligation| obligation.key() == *displayed)
        });

        for obligation in retirement.retired {
            self.inputs.remove(&obligation.key());
        }

        if !retirement.front_removed || !displayed_retired {
            return;
        }

        let Some(lease) = self.lease else {
            return;
        };

        if !self.modals.read(cx).owns(lease) {
            self.lease = None;
            self.displayed = None;

            return;
        }

        // Only the flow that owns Root may pop it. In particular, retiring a
        // queued Pi front while Add Workspace owns the modal leaves that
        // workspace modal untouched.
        self.displayed = None;
        window.close_dialog(cx);
        self.defer_advance(window, cx);
    }

    fn defer_advance(&self, window: &mut Window, cx: &mut Context<Self>) {
        let coordinator = cx.entity();

        window.defer(cx, move |window, cx| {
            coordinator.update(cx, |coordinator, cx| coordinator.advance(window, cx));
        });
    }

    fn release_modal(&mut self, cx: &mut Context<Self>) {
        let Some(lease) = self.lease.take() else {
            return;
        };

        self.modals
            .update(cx, |modals, cx| modals.release(lease, cx));
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

/// Put `dialog` on screen. Every path out lands in `answered`, including
/// dismissal. Its boolean tells Root whether this callback still owns the
/// modal; delayed duplicates return false and cannot pop a successor.
fn open<T: 'static>(
    dialog: &DialogObligation,
    input: Option<Entity<InputState>>,
    answered: impl Fn(&mut T, DialogAnswer, &mut Window, &mut Context<T>) -> bool + 'static,
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
            owner.update(cx, |owner, cx| answered(owner, answer, window, cx))
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
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) -> bool + 'static,
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
                        if answer(DialogAnswer::Value(option.clone()), window, cx) {
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
            // A custom footer replaces the library's OK button, which would be
            // meaningless here: picking *is* confirming.
            .footer(DialogFooter::new().child(
                DialogClose::new().child(Button::new("dialog-dismiss").outline().label("Cancel")),
            ))
            // Root maps bare Enter to ConfirmDialog. A select is confirmed only
            // by an option button, never by the dialog frame's default
            // `on_ok: true`.
            .on_ok(|_, _, _| false)
            .on_cancel({
                let answer = answer.clone();

                move |_, window, cx| answer(dismissal.clone(), window, cx)
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
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) -> bool + 'static,
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
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("dialog-no").label("No")))
                    .child(
                        DialogAction::new().child(Button::new("dialog-yes").primary().label("Yes")),
                    ),
            )
            .on_ok(move |_, window, cx| confirmed(DialogAnswer::Confirmed(true), window, cx))
            .on_cancel(move |_, window, cx| declined(dismissal.clone(), window, cx))
    });
}

/// `input` and `editor`: the same dialog over a different box.
fn open_text<T: 'static>(
    title: SharedString,
    input: Entity<InputState>,
    dismissal: DialogAnswer,
    answer: impl Fn(DialogAnswer, &mut Window, &mut App) -> bool + 'static,
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
            .footer(
                DialogFooter::new()
                    .child(DialogClose::new().child(Button::new("dialog-cancel").label("Cancel")))
                    .child(
                        DialogAction::new()
                            .child(Button::new("dialog-send").primary().label("Send")),
                    ),
            )
            .on_ok(move |_, window, cx| {
                let value = typed.read(cx).value().to_string();

                submitted(DialogAnswer::Value(value), window, cx)
            })
            .on_cancel(move |_, window, cx| cancelled(dismissal.clone(), window, cx))
    });

    // `open_dialog` focuses the dialog frame it just built, not anything
    // inside it — so the box this whole dialog exists to have filled in would
    // need a click before it would take a keystroke.
    input.update(cx, |state, cx| state.focus(window, cx));
}
