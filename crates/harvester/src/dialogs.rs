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

const DIALOG_WIDTH: f32 = 520.;
const EDITOR_ROWS: (usize, usize) = (6, 16);
const UNTITLED: &str = "The agent is asking";

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

fn open<T: 'static>(
    dialog: &DialogObligation,
    input: Option<Entity<InputState>>,
    answered: impl Fn(&mut T, DialogAnswer, &mut Window, &mut Context<T>) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
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
            .footer(DialogFooter::new().child(
                DialogClose::new().child(Button::new("dialog-dismiss").outline().label("Cancel")),
            ))
            .on_ok(|_, _, _| false)
            .on_cancel({
                let answer = answer.clone();

                move |_, window, cx| answer(dismissal.clone(), window, cx)
            })
    });
}

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

    input.update(cx, |state, cx| state.focus(window, cx));
}
