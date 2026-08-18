use std::rc::Rc;

use gpui::{App, Context, Entity, SharedString, Window};
use gpui_component::input::InputState;
use gpui_component::WindowExt as _;
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::{
    DialogAnswer, DialogKey, DialogKind, DialogObligation, DialogObligations, DialogRequest,
    DialogRoom, EnqueueResult, Retirement,
};

use crate::modal::{ModalAvailable, ModalCoordinator, ModalLease, ModalOwner};
use crate::store::{FleetStore, StoreEvent};
use crate::ui::dialog::{
    create_multi_line_input, create_single_line_input, open_confirmation, open_select, open_text,
    title_or_fallback, ActionAccent, Confirmation,
};

type AnswerCallback = Rc<dyn Fn(DialogAnswer, &mut Window, &mut App) -> bool>;

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
        DialogKind::Input { placeholder } => Some(create_single_line_input(
            placeholder.clone().unwrap_or_default(),
            window,
            cx,
        )),
        DialogKind::Editor { prefill } => Some(create_multi_line_input(
            prefill.clone().unwrap_or_default(),
            window,
            cx,
        )),
        DialogKind::Select { .. } | DialogKind::Confirm { .. } => None,
    }
}

fn open<T: 'static>(
    obligation: &DialogObligation,
    input: Option<Entity<InputState>>,
    answered: impl Fn(&mut T, DialogAnswer, &mut Window, &mut Context<T>) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let owner = cx.entity();
    let answer: AnswerCallback = Rc::new(
        move |answer: DialogAnswer, window: &mut Window, cx: &mut App| {
            owner.update(cx, |owner, cx| answered(owner, answer, window, cx))
        },
    );
    let title = title_or_fallback(&obligation.request.title);
    let dismissal = DialogAnswer::dismissal_for(&obligation.request);

    match &obligation.request.kind {
        DialogKind::Select { options } => {
            open_option_list(title, options.clone(), dismissal, answer, window, cx)
        }
        DialogKind::Confirm { message } => {
            open_question(title, message.clone(), dismissal, answer, window, cx)
        }
        DialogKind::Input { .. } | DialogKind::Editor { .. } => {
            let Some(input) = input else {
                return;
            };

            open_text_entry(title, input, dismissal, answer, window, cx)
        }
    }
}

fn open_option_list<T: 'static>(
    title: SharedString,
    options: Vec<String>,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let chosen = answer.clone();

    open_select(
        title,
        options,
        move |option, window, cx| chosen(DialogAnswer::Value(option), window, cx),
        move |window, cx| answer(dismissal.clone(), window, cx),
        window,
        cx,
    );
}

fn open_question<T: 'static>(
    title: SharedString,
    message: String,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let confirmed = answer.clone();
    let question = Confirmation {
        title,
        message: message.into(),
        cancel_label: "No".into(),
        action_label: "Yes".into(),
        action_accent: ActionAccent::Primary,
    };

    open_confirmation(
        question,
        move |window, cx| confirmed(DialogAnswer::Confirmed(true), window, cx),
        move |window, cx| answer(dismissal.clone(), window, cx),
        window,
        cx,
    );
}

fn open_text_entry<T: 'static>(
    title: SharedString,
    input: Entity<InputState>,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let submitted = answer.clone();

    open_text(
        title,
        input,
        move |value, window, cx| submitted(DialogAnswer::Value(value), window, cx),
        move |window, cx| answer(dismissal.clone(), window, cx),
        window,
        cx,
    );
}
