use std::rc::Rc;

use gpui::{
    div, prelude::*, Action, Anchor, AnyElement, App, Div, ElementId, Entity, FocusHandle,
    Focusable as _, SharedString, Window,
};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Enter, Input, InputState};
use gpui_component::menu::{DropdownMenu as _, PopupMenu};
use gpui_component::tag::Tag;
use gpui_component::{ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _};

use crate::assets::GIT_BRANCH_ICON_PATH;

use super::core::Tone;
use super::note::inline_note;

const MODEL_CONTROL_ID: &str = "chat-model";
const MACHINE_CONTROL_ID: &str = "chat-machine";
const REPO_CONTROL_ID: &str = "chat-repo";
const SEND_CONTROL_ID: &str = "chat-send";
const STOP_CONTROL_ID: &str = "chat-stop";

pub const DEFAULT_MODEL_ROW: &str = "Default (pi chooses)";
pub const ADD_REPO_ROW: &str = "Add new…";

const SESSION_STARTING_NOTICE: &str = "Starting session…";
const SEND_TOOLTIP: &str = "Send";
const STOP_LABEL: &str = "Stop";
const STOP_TOOLTIP: &str = "Stop pi — the session can be resumed from the sidebar";
const INPUT_KEY_CONTEXT: &str = "Input";

const CONTROL_LABEL_MAX_CHARS: usize = 24;
const CONTROL_LABEL_ELLIPSIS: char = '\u{2026}';

const SEND_KEYSTROKE: Enter = Enter {
    secondary: false,
    shift: false,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Action)]
#[action(namespace = prompt, no_json)]
pub struct PickModel {
    pub index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Action)]
#[action(namespace = prompt, no_json)]
pub struct PickMachine {
    pub index: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Action)]
#[action(namespace = prompt, no_json)]
pub struct PickRepo {
    pub index: usize,
}

fn prompt_box(focused: bool, cx: &App) -> Div {
    let theme = cx.theme();
    let border = if focused { theme.ring } else { theme.border };

    div()
        .rounded(theme.radius_lg)
        .bg(theme.input)
        .border_1()
        .border_color(border)
        .flex()
        .flex_col()
}

fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

fn prompt_box_controls() -> Div {
    div().pl_2().pr_3().pb_2().flex().items_center().gap_2()
}

#[derive(Clone)]
pub struct DropdownRow {
    pub label: SharedString,
    pub selected: bool,
    pub divider_above: bool,
}

fn prompt_box_control(
    id: impl Into<ElementId>,
    icon: Icon,
    label: SharedString,
    enabled: bool,
) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .icon(icon)
        .label(truncated_control_label(label))
        .dropdown_caret(true)
        .disabled(!enabled)
        .max_w_56()
}

fn truncated_control_label(label: SharedString) -> SharedString {
    if label.chars().count() <= CONTROL_LABEL_MAX_CHARS {
        return label;
    }

    let kept: String = label.chars().take(CONTROL_LABEL_MAX_CHARS - 1).collect();

    format!("{}{CONTROL_LABEL_ELLIPSIS}", kept.trim_end()).into()
}

fn prompt_box_dropdown(
    trigger: Button,
    rows: Vec<DropdownRow>,
    action_for_index: impl Fn(usize) -> Box<dyn Action> + 'static,
    action_context: Option<FocusHandle>,
) -> impl IntoElement {
    let rows = Rc::new(rows);
    let action_for_index = Rc::new(action_for_index);

    trigger.dropdown_menu_with_anchor(Anchor::BottomLeft, move |menu, _window, _cx| {
        create_dropdown_menu(
            menu,
            &rows,
            action_for_index.as_ref(),
            action_context.clone(),
        )
    })
}

fn create_dropdown_menu(
    menu: PopupMenu,
    rows: &[DropdownRow],
    action_for_index: &dyn Fn(usize) -> Box<dyn Action>,
    action_context: Option<FocusHandle>,
) -> PopupMenu {
    let mut menu = menu.scrollable(true);

    if let Some(handle) = action_context {
        menu = menu.action_context(handle);
    }

    for (index, row) in rows.iter().enumerate() {
        if row.divider_above {
            menu = menu.separator();
        }

        menu = menu.menu_with_check(row.label.clone(), row.selected, action_for_index(index));
    }

    menu
}

fn prompt_box_stop(id: impl Into<ElementId>) -> Button {
    Button::new(id)
        .outline()
        .small()
        .icon(Icon::new(IconName::CircleX))
        .label(STOP_LABEL)
        .tooltip(STOP_TOOLTIP)
}

fn prompt_box_send(id: impl Into<ElementId>, enabled: bool) -> Button {
    Button::new(id)
        .primary()
        .small()
        .icon(Icon::new(IconName::ArrowUp))
        .rounded_full()
        .disabled(!enabled)
        .tooltip_with_action(SEND_TOOLTIP, &SEND_KEYSTROKE, Some(INPUT_KEY_CONTEXT))
}

pub struct PromptStripControl {
    pub label: SharedString,
    pub enabled: bool,
    pub rows: Vec<DropdownRow>,
}

pub enum PromptStripNotice {
    SessionStarting,
    SessionExited(SharedString),
}

impl PromptStripNotice {
    fn text(self) -> SharedString {
        match self {
            PromptStripNotice::SessionStarting => SESSION_STARTING_NOTICE.into(),
            PromptStripNotice::SessionExited(detail) => detail,
        }
    }
}

pub struct PromptStrip {
    pub notice: Option<PromptStripNotice>,
    pub model: PromptStripControl,
    pub machine: PromptStripControl,
    pub repo: PromptStripControl,
    pub branch: Option<SharedString>,
    pub can_send: bool,
    pub can_stop: bool,
    pub focused: bool,
}

pub fn prompt_strip(
    strip: PromptStrip,
    send: impl Fn(&mut Window, &mut App) + 'static,
    stop: impl Fn(&mut Window, &mut App) + 'static,
    input: &Entity<InputState>,
    cx: &App,
) -> Div {
    let row = div().flex_none().px_3().pb_3();

    if let Some(notice) = strip.notice {
        return row.child(inline_note(Tone::Idle, notice.text(), cx));
    }

    row.child(prompt_strip_box(strip, send, stop, input, cx))
}

fn prompt_strip_box(
    strip: PromptStrip,
    send: impl Fn(&mut Window, &mut App) + 'static,
    stop: impl Fn(&mut Window, &mut App) + 'static,
    input: &Entity<InputState>,
    cx: &App,
) -> Div {
    let action_context = input.read(cx).focus_handle(cx);
    let model = dropdown_control(
        MODEL_CONTROL_ID,
        Icon::new(IconName::Bot),
        strip.model,
        |index| Box::new(PickModel { index }),
        &action_context,
    );
    let machine = dropdown_control(
        MACHINE_CONTROL_ID,
        Icon::new(IconName::HardDrive),
        strip.machine,
        |index| Box::new(PickMachine { index }),
        &action_context,
    );
    let repo = dropdown_control(
        REPO_CONTROL_ID,
        Icon::new(IconName::Folder),
        strip.repo,
        |index| Box::new(PickRepo { index }),
        &action_context,
    );
    let send_button = prompt_box_send(SEND_CONTROL_ID, strip.can_send)
        .on_click(move |_, window, cx| send(window, cx));
    let stop_button = strip
        .can_stop
        .then(|| prompt_box_stop(STOP_CONTROL_ID).on_click(move |_, window, cx| stop(window, cx)));

    prompt_box(strip.focused, cx)
        .child(prompt_box_input(Input::new(input).appearance(false).p_0()))
        .child(
            prompt_box_controls()
                .child(model)
                .child(machine)
                .child(repo)
                .children(branch_indicator(strip.branch))
                .child(div().flex_1())
                .children(stop_button)
                .child(send_button),
        )
}

fn dropdown_control(
    control_id: &'static str,
    icon: Icon,
    control: PromptStripControl,
    action_for_index: impl Fn(usize) -> Box<dyn Action> + 'static,
    action_context: &FocusHandle,
) -> AnyElement {
    let trigger = prompt_box_control(control_id, icon, control.label, control.enabled);

    if !control.enabled || control.rows.is_empty() {
        return trigger.into_any_element();
    }

    prompt_box_dropdown(
        trigger,
        control.rows,
        action_for_index,
        Some(action_context.clone()),
    )
    .into_any_element()
}

fn branch_indicator(branch: Option<SharedString>) -> Option<Tag> {
    let branch = branch?;

    Some(
        Tag::secondary()
            .small()
            .flex_none()
            .child(branch_indicator_body(branch)),
    )
}

fn branch_indicator_body(branch: SharedString) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1()
        .child(Icon::default().path(GIT_BRANCH_ICON_PATH).size_3())
        .child(branch)
}

#[cfg(test)]
mod tests {
    use super::{truncated_control_label, CONTROL_LABEL_MAX_CHARS};

    #[test]
    fn a_label_that_fits_is_left_alone() {
        assert_eq!(truncated_control_label("Sonnet".into()), "Sonnet");
    }

    #[test]
    fn a_label_at_the_budget_is_still_left_alone() {
        let exact: String = "x".repeat(CONTROL_LABEL_MAX_CHARS);

        assert_eq!(truncated_control_label(exact.clone().into()), exact);
    }

    #[test]
    fn a_longer_label_is_cut_to_the_budget_and_marked() {
        let long: String = "x".repeat(CONTROL_LABEL_MAX_CHARS + 10);
        let truncated = truncated_control_label(long.into());

        assert_eq!(truncated.chars().count(), CONTROL_LABEL_MAX_CHARS);
        assert!(truncated.ends_with('\u{2026}'));
    }

    #[test]
    fn the_cut_does_not_leave_a_space_before_the_ellipsis() {
        let padded = format!("{} tail", "y".repeat(CONTROL_LABEL_MAX_CHARS - 2));

        assert_eq!(
            truncated_control_label(padded.into()),
            format!("{}\u{2026}", "y".repeat(CONTROL_LABEL_MAX_CHARS - 2))
        );
    }

    #[test]
    fn a_multibyte_label_is_cut_on_character_boundaries() {
        let wide: String = "\u{3042}".repeat(CONTROL_LABEL_MAX_CHARS + 5);
        let truncated = truncated_control_label(wide.into());

        assert_eq!(truncated.chars().count(), CONTROL_LABEL_MAX_CHARS);
    }
}
