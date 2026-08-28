use std::rc::Rc;

use gpui::{App, Context, Entity, SharedString, Window};
use gpui_component::input::InputState;
use harvester_client_core::{DialogAnswer, DialogKind, DialogObligation};

use crate::ui::dialog::{
    create_multi_line_input, create_single_line_input, open_confirmation, open_select, open_text,
    title_or_named_condition, ActionAccent, Confirmation,
};

type AnswerCallback = Rc<dyn Fn(DialogAnswer, &mut Window, &mut App) -> bool>;

const UNTITLED_SELECT_CONDITION: &str = "Choose an option";
const UNTITLED_INPUT_CONDITION: &str = "Enter a value";
const UNTITLED_EDITOR_CONDITION: &str = "Write a response";
const UNTITLED_CONFIRM_CONDITION: &str = "The agent needs a yes or no";
const CONFIRM_CANCEL_LABEL: &str = "Cancel";
const CONFIRM_ACTION_LABEL: &str = "Confirm";

pub(crate) fn open_pi_dialog<T: 'static>(
    obligation: &DialogObligation,
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
    let title = obligation.request.title.as_str();
    let dismissal = DialogAnswer::dismissal_for(&obligation.request);

    match &obligation.request.kind {
        DialogKind::Select { options } => open_option_list(
            title_or_named_condition(title, UNTITLED_SELECT_CONDITION),
            options.clone(),
            dismissal,
            answer,
            window,
            cx,
        ),
        DialogKind::Confirm { message } => {
            open_question(title, message, dismissal, answer, window, cx)
        }
        DialogKind::Input { placeholder } => open_single_line_entry(
            title_or_named_condition(title, UNTITLED_INPUT_CONDITION),
            placeholder.clone().unwrap_or_default(),
            dismissal,
            answer,
            window,
            cx,
        ),
        DialogKind::Editor { prefill } => open_multi_line_entry(
            title_or_named_condition(title, UNTITLED_EDITOR_CONDITION),
            prefill.clone().unwrap_or_default(),
            dismissal,
            answer,
            window,
            cx,
        ),
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
    title: &str,
    message: &str,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let confirmed = answer.clone();
    let question = confirmation_copy(title, message);

    open_confirmation(
        question,
        move |window, cx| confirmed(DialogAnswer::Confirmed(true), window, cx),
        move |window, cx| answer(dismissal.clone(), window, cx),
        window,
        cx,
    );
}

fn confirmation_copy(title: &str, message: &str) -> Confirmation {
    let (title, message) = condition_and_consequence(title, message);

    Confirmation {
        title,
        message,
        cancel_label: CONFIRM_CANCEL_LABEL.into(),
        action_label: CONFIRM_ACTION_LABEL.into(),
        action_accent: ActionAccent::Primary,
    }
}

fn condition_and_consequence(title: &str, message: &str) -> (SharedString, SharedString) {
    let title = title.trim();
    let message = message.trim();

    if title.is_empty() {
        return (
            title_or_named_condition(message, UNTITLED_CONFIRM_CONDITION),
            SharedString::default(),
        );
    }

    if message == title {
        return (title.to_string().into(), SharedString::default());
    }

    (title.to_string().into(), message.to_string().into())
}

fn open_single_line_entry<T: 'static>(
    title: SharedString,
    placeholder: String,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let input = create_single_line_input(placeholder, window, cx);

    open_value_entry(title, input, dismissal, answer, window, cx);
}

fn open_multi_line_entry<T: 'static>(
    title: SharedString,
    prefill: String,
    dismissal: DialogAnswer,
    answer: AnswerCallback,
    window: &mut Window,
    cx: &mut Context<T>,
) {
    let input = create_multi_line_input(prefill, window, cx);

    open_value_entry(title, input, dismissal, answer, window, cx);
}

fn open_value_entry<T: 'static>(
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
