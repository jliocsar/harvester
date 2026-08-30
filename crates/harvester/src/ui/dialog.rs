use std::rc::Rc;

use gpui::{
    div, prelude::*, rems, App, Context, Div, ElementId, Entity, Pixels, Rems, SharedString, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::dialog::{CancelDialog, DialogAction, DialogClose, DialogFooter};
use gpui_component::input::InputState;
use gpui_component::list::{List, ListDelegate, ListItem, ListState};
use gpui_component::{ActiveTheme as _, IndexPath, WindowExt as _};

use super::core::text_input;

pub const CONFIRMATION_DIALOG_WIDTH_IN_REMS: f32 = 30.;
pub const SELECT_DIALOG_WIDTH_IN_REMS: f32 = 36.;
pub const TEXT_DIALOG_WIDTH_IN_REMS: f32 = 44.;

pub const CANCEL_LABEL: &str = "Cancel";

const SELECT_OPTION_ROW_HEIGHT_IN_REMS: f32 = 2.;
const SELECT_VISIBLE_OPTION_ROWS: usize = 8;
const EDITOR_ROWS: (usize, usize) = (6, 16);
const TEXT_COMMIT_LABEL: &str = "Reply";
const SELECT_OPTION_ROW_ID: &str = "dialog-option";
const SELECT_CANCEL_BUTTON_ID: &str = "dialog-select-cancel";
const CONFIRMATION_CANCEL_BUTTON_ID: &str = "dialog-confirmation-cancel";
const CONFIRMATION_ACTION_BUTTON_ID: &str = "dialog-confirmation-action";
const TEXT_CANCEL_BUTTON_ID: &str = "dialog-text-cancel";
const TEXT_COMMIT_BUTTON_ID: &str = "dialog-text-commit";

type ChooseOption = Rc<dyn Fn(String, &mut Window, &mut App) -> bool>;

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

pub fn title_or_named_condition(title: &str, named_condition: &str) -> SharedString {
    let title = title.trim();

    if title.is_empty() {
        return named_condition.to_string().into();
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
    dismiss_only_row(cancel).child(action)
}

pub fn dismiss_only_row(cancel: Button) -> Div {
    div().flex().justify_end().gap_2().child(cancel)
}

pub fn option_row(index: usize, option: impl Into<SharedString>, selected: bool) -> ListItem {
    ListItem::new((SELECT_OPTION_ROW_ID, index))
        .selected(selected)
        .child(option.into())
}

pub fn options_height(option_count: usize) -> Rems {
    let visible_rows = option_count.clamp(1, SELECT_VISIBLE_OPTION_ROWS);

    rems(SELECT_OPTION_ROW_HEIGHT_IN_REMS * visible_rows as f32)
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
    cx: &mut Context<'_, T>,
) {
    let cancelled = Rc::new(cancelled);
    let option_count = options.len();
    let delegate = OptionListDelegate {
        options: options.into_iter().map(Into::into).collect(),
        selected: None,
        chosen: Rc::new(chosen),
    };
    let options_list = cx.new(|cx| ListState::new(delegate, window, cx));
    let focused_list = options_list.clone();

    window.open_dialog(cx, move |dialog, window, cx| {
        let cancelled = cancelled.clone();

        dialog
            .title(title.clone())
            .width(dialog_width(SELECT_DIALOG_WIDTH_IN_REMS, window))
            .close_button(false)
            .child(options_pane(&options_list, option_count, cx))
            .footer(dismiss_only_footer(cancel_button(
                SELECT_CANCEL_BUTTON_ID,
                CANCEL_LABEL,
            )))
            .on_cancel(move |_, window, cx| cancelled(window, cx))
    });

    focused_list.update(cx, |list, cx| list.focus(window, cx));
}

pub fn open_confirmation<T: 'static>(
    confirmation: Confirmation,
    confirmed: impl Fn(&mut Window, &mut App) -> bool + 'static,
    declined: impl Fn(&mut Window, &mut App) -> bool + 'static,
    window: &mut Window,
    cx: &mut Context<'_, T>,
) {
    let confirmed = Rc::new(confirmed);
    let declined = Rc::new(declined);

    window.open_dialog(cx, move |dialog, window, _| {
        let confirmed = confirmed.clone();
        let declined = declined.clone();
        let message = confirmation.message.clone();

        dialog
            .title(confirmation.title.clone())
            .width(dialog_width(CONFIRMATION_DIALOG_WIDTH_IN_REMS, window))
            .close_button(false)
            .when(!message.is_empty(), |dialog| dialog.child(message))
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
    cx: &mut Context<'_, T>,
) {
    let submitted = Rc::new(submitted);
    let cancelled = Rc::new(cancelled);
    let shown = input.clone();

    window.open_dialog(cx, move |dialog, window, _| {
        let submitted = submitted.clone();
        let cancelled = cancelled.clone();
        let typed = shown.clone();

        dialog
            .title(title.clone())
            .width(dialog_width(TEXT_DIALOG_WIDTH_IN_REMS, window))
            .close_button(false)
            .child(text_input(&shown))
            .footer(action_footer(
                cancel_button(TEXT_CANCEL_BUTTON_ID, CANCEL_LABEL),
                action_button(
                    TEXT_COMMIT_BUTTON_ID,
                    TEXT_COMMIT_LABEL,
                    ActionAccent::Primary,
                ),
            ))
            .on_ok(move |_, window, cx| {
                let value = typed.read(cx).value().to_string();

                submitted(value, window, cx)
            })
            .on_cancel(move |_, window, cx| cancelled(window, cx))
    });

    input.update(cx, |state, cx| state.focus(window, cx));
}

struct OptionListDelegate {
    options: Vec<SharedString>,
    selected: Option<IndexPath>,
    chosen: ChooseOption,
}

impl OptionListDelegate {
    fn chosen_option(&self) -> Option<SharedString> {
        let selected = self.selected?;

        self.options.get(selected.row).cloned()
    }
}

impl ListDelegate for OptionListDelegate {
    type Item = ListItem;

    fn items_count(&self, _section: usize, _cx: &App) -> usize {
        self.options.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) -> Option<Self::Item> {
        let option = self.options.get(ix.row)?;

        Some(option_row(ix.row, option.clone(), false))
    }

    fn set_selected_index(
        &mut self,
        ix: Option<IndexPath>,
        _window: &mut Window,
        _cx: &mut Context<'_, ListState<Self>>,
    ) {
        self.selected = ix;
    }

    fn confirm(
        &mut self,
        _secondary: bool,
        window: &mut Window,
        cx: &mut Context<'_, ListState<Self>>,
    ) {
        let Some(option) = self.chosen_option() else {
            return;
        };
        let chosen = self.chosen.clone();

        if chosen(option.to_string(), window, cx) {
            window.close_dialog(cx);
        }
    }
}

fn options_pane(
    options_list: &Entity<ListState<OptionListDelegate>>,
    option_count: usize,
    cx: &App,
) -> List<OptionListDelegate> {
    List::new(options_list)
        .h(options_height(option_count))
        .border_1()
        .border_color(cx.theme().border)
        .rounded(cx.theme().radius)
}

fn dialog_width(width_in_rems: f32, window: &Window) -> Pixels {
    rems(width_in_rems).to_pixels(window.rem_size())
}

fn action_footer(cancel: Button, action: Button) -> DialogFooter {
    DialogFooter::new()
        .child(DialogClose::new().child(cancel))
        .child(DialogAction::new().child(action))
}

fn dismiss_only_footer(cancel: Button) -> DialogFooter {
    DialogFooter::new().child(DialogClose::new().child(cancel))
}

#[cfg(test)]
mod tests {
    use super::{
        options_height, title_or_named_condition, SELECT_OPTION_ROW_HEIGHT_IN_REMS,
        SELECT_VISIBLE_OPTION_ROWS,
    };
    use gpui::rems;

    const NAMED_CONDITION: &str = "Choose an option";

    #[test]
    fn a_title_pi_gave_us_is_shown_verbatim() {
        assert_eq!(
            title_or_named_condition("Overwrite main.rs?", NAMED_CONDITION),
            "Overwrite main.rs?"
        );
    }

    #[test]
    fn a_title_is_trimmed_before_it_is_judged_present() {
        assert_eq!(
            title_or_named_condition("  Overwrite main.rs?  ", NAMED_CONDITION),
            "Overwrite main.rs?"
        );
    }

    #[test]
    fn a_blank_title_falls_back_to_the_callers_own_condition() {
        assert_eq!(
            title_or_named_condition("", NAMED_CONDITION),
            NAMED_CONDITION
        );
        assert_eq!(
            title_or_named_condition("   \t ", NAMED_CONDITION),
            NAMED_CONDITION
        );
    }

    #[test]
    fn a_short_list_gets_exactly_its_own_height() {
        assert_eq!(
            options_height(3),
            rems(SELECT_OPTION_ROW_HEIGHT_IN_REMS * 3.)
        );
    }

    #[test]
    fn an_empty_list_still_reserves_one_row() {
        assert_eq!(options_height(0), rems(SELECT_OPTION_ROW_HEIGHT_IN_REMS));
    }

    #[test]
    fn a_long_list_stops_growing_and_scrolls_instead() {
        let capped = rems(SELECT_OPTION_ROW_HEIGHT_IN_REMS * SELECT_VISIBLE_OPTION_ROWS as f32);

        assert_eq!(options_height(SELECT_VISIBLE_OPTION_ROWS), capped);
        assert_eq!(options_height(SELECT_VISIBLE_OPTION_ROWS + 40), capped);
    }
}
