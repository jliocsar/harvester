use std::rc::Rc;

use gpui::{
    div, prelude::*, px, Anchor, AnyElement, App, Div, ElementId, Entity, SharedString, Stateful,
    Window,
};
use gpui_component::input::{Input, InputState};
use gpui_component::popover::Popover;
use gpui_component::{ActiveTheme as _, Icon, IconName, Selectable};

use crate::assets::GIT_BRANCH_ICON_PATH;

use super::core::{dim, faint, Tone};
use super::note::inline_note;

const PROMPT_BOX_BORDER_OPACITY: f32 = 0.5;
const DROPDOWN_MAX_HEIGHT: f32 = 280.;
const DROPDOWN_MAX_WIDTH: f32 = 360.;
const DROPDOWN_CHECK_WIDTH: f32 = 12.;
const SEND_BUTTON_SIZE: f32 = 22.;
const SEND_ICON_SIZE: f32 = 13.;
const TRIGGER_CHEVRON_SIZE: f32 = 12.;
const PROMPT_CONTROL_MAX_WIDTH: f32 = 220.;

const MODEL_CONTROL_ID: &str = "chat-model";
const MODEL_DROPDOWN_ID: &str = "chat-model-dropdown";
const MACHINE_CONTROL_ID: &str = "chat-machine";
const MACHINE_DROPDOWN_ID: &str = "chat-machine-dropdown";
const REPO_CONTROL_ID: &str = "chat-repo";
const REPO_DROPDOWN_ID: &str = "chat-repo-dropdown";
const BRANCH_CONTROL_ID: &str = "chat-branch";
const SEND_CONTROL_ID: &str = "chat-send";
const SESSION_STARTING_NOTICE: &str = "Starting session…";

pub fn prompt_box(focused: bool, cx: &App) -> Div {
    div()
        .rounded_xl()
        .bg(cx.theme().input)
        .border_1()
        .border_color(if focused {
            cx.theme().ring
        } else {
            cx.theme().border.opacity(PROMPT_BOX_BORDER_OPACITY)
        })
        .shadow_md()
        .flex()
        .flex_col()
}

pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

pub fn prompt_box_controls() -> Div {
    div().pl_1p5().pr_3().pb_2().flex().items_center().gap_2()
}

pub struct DropdownRow {
    pub label: SharedString,
    pub selected: bool,
    pub divider_above: bool,
}

#[derive(IntoElement)]
struct PlainTrigger(Stateful<Div>);

impl RenderOnce for PlainTrigger {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        self.0
    }
}

impl Selectable for PlainTrigger {
    fn selected(self, _selected: bool) -> Self {
        self
    }

    fn is_selected(&self) -> bool {
        false
    }
}

pub fn prompt_box_dropdown(
    id: &'static str,
    trigger: Stateful<Div>,
    rows: Vec<DropdownRow>,
    picked: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> impl IntoElement {
    let rows = Rc::new(rows);
    let picked = Rc::new(picked);

    Popover::new(id)
        .anchor(Anchor::BottomLeft)
        .p_1()
        .border_color(cx.theme().secondary_active)
        .trigger(PlainTrigger(trigger))
        .content(move |_, _, cx| {
            let popover = cx.entity();
            let rows = rows.clone();
            let picked = picked.clone();

            div()
                .id("dropdown-rows")
                .flex()
                .flex_col()
                .max_w(px(DROPDOWN_MAX_WIDTH))
                .max_h(px(DROPDOWN_MAX_HEIGHT))
                .overflow_y_scroll()
                .children(rows.iter().enumerate().map(|(index, row)| {
                    let popover = popover.clone();
                    let picked = picked.clone();

                    dropdown_row(index, row, cx).on_click(move |_, window, cx| {
                        popover.update(cx, |state, cx| state.dismiss(window, cx));
                        picked(index, window, cx);
                    })
                }))
        })
}

fn dropdown_row(index: usize, row: &DropdownRow, cx: &App) -> Stateful<Div> {
    div()
        .id(("dropdown-row", index))
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .when(row.divider_above, |row| {
            row.mt_1()
                .pt_2()
                .border_t_1()
                .border_color(cx.theme().border)
        })
        .hover(|style| style.bg(cx.theme().accent))
        .active(|style| style.bg(cx.theme().secondary_hover))
        .child(
            div()
                .flex_1()
                .overflow_hidden()
                .text_ellipsis()
                .child(row.label.clone()),
        )
        .child(
            div()
                .flex_none()
                .w(px(DROPDOWN_CHECK_WIDTH))
                .text_color(cx.theme().muted_foreground)
                .when(row.selected, |slot| {
                    slot.child(Icon::new(IconName::Check).size(px(DROPDOWN_CHECK_WIDTH)))
                }),
        )
}

pub fn prompt_box_control(
    id: impl Into<ElementId>,
    icon: Icon,
    label: SharedString,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .flex()
        .items_center()
        .gap_1p5()
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .text_xs()
        .max_w(px(PROMPT_CONTROL_MAX_WIDTH))
        .child(icon);
    let label_text = div().overflow_hidden().text_ellipsis();

    if enabled {
        base.text_color(dim(cx))
            .cursor_pointer()
            .hover(|style| {
                style
                    .bg(cx.theme().secondary)
                    .text_color(cx.theme().foreground)
            })
            .active(|style| style.bg(cx.theme().secondary_hover))
            .child(label_text.child(label))
            .child(
                div()
                    .flex_none()
                    .child(Icon::new(IconName::ChevronDown).size(px(TRIGGER_CHEVRON_SIZE))),
            )
    } else {
        base.text_color(faint(cx)).child(label_text.child(label))
    }
}

pub fn prompt_box_send(id: impl Into<ElementId>, enabled: bool, cx: &App) -> Stateful<Div> {
    let base = div()
        .id(id)
        .size(px(SEND_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .child(Icon::new(IconName::ArrowUp).size(px(SEND_ICON_SIZE)));

    if !enabled {
        return base
            .bg(cx.theme().muted)
            .text_color(cx.theme().muted_foreground);
    }

    base.bg(cx.theme().primary)
        .text_color(cx.theme().primary_foreground)
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().primary_hover))
        .active(|style| style.bg(cx.theme().primary_active))
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
    pub focused: bool,
}

pub struct PromptStripHandlers {
    pick_model: Box<dyn Fn(usize, &mut Window, &mut App)>,
    pick_machine: Box<dyn Fn(usize, &mut Window, &mut App)>,
    pick_repo: Box<dyn Fn(usize, &mut Window, &mut App)>,
    send: Box<dyn Fn(&mut Window, &mut App)>,
}

impl PromptStripHandlers {
    pub fn new(
        pick_model: impl Fn(usize, &mut Window, &mut App) + 'static,
        pick_machine: impl Fn(usize, &mut Window, &mut App) + 'static,
        pick_repo: impl Fn(usize, &mut Window, &mut App) + 'static,
        send: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            pick_model: Box::new(pick_model),
            pick_machine: Box::new(pick_machine),
            pick_repo: Box::new(pick_repo),
            send: Box::new(send),
        }
    }
}

pub fn prompt_strip(
    strip: PromptStrip,
    handlers: PromptStripHandlers,
    input: &Entity<InputState>,
    cx: &App,
) -> Div {
    let row = div().flex_none().px_3().pb_3();

    if let Some(notice) = strip.notice {
        return row.child(inline_note(Tone::Idle, notice.text(), cx));
    }

    row.child(prompt_strip_box(strip, handlers, input, cx))
}

fn prompt_strip_box(
    strip: PromptStrip,
    handlers: PromptStripHandlers,
    input: &Entity<InputState>,
    cx: &App,
) -> Div {
    let PromptStripHandlers {
        pick_model,
        pick_machine,
        pick_repo,
        send,
    } = handlers;
    let model = dropdown_control(
        MODEL_CONTROL_ID,
        MODEL_DROPDOWN_ID,
        Icon::new(IconName::Bot),
        strip.model,
        pick_model,
        cx,
    );
    let machine = dropdown_control(
        MACHINE_CONTROL_ID,
        MACHINE_DROPDOWN_ID,
        Icon::new(IconName::HardDrive),
        strip.machine,
        pick_machine,
        cx,
    );
    let repo = dropdown_control(
        REPO_CONTROL_ID,
        REPO_DROPDOWN_ID,
        Icon::new(IconName::Folder),
        strip.repo,
        pick_repo,
        cx,
    );
    let send_button = prompt_box_send(SEND_CONTROL_ID, strip.can_send, cx)
        .on_click(move |_, window, cx| send(window, cx));

    prompt_box(strip.focused, cx)
        .child(prompt_box_input(Input::new(input).appearance(false).p_0()))
        .child(
            prompt_box_controls()
                .child(model)
                .child(machine)
                .child(repo)
                .children(branch_indicator(strip.branch, cx))
                .child(div().flex_1())
                .child(send_button),
        )
}

fn dropdown_control(
    control_id: &'static str,
    dropdown_id: &'static str,
    icon: Icon,
    control: PromptStripControl,
    on_pick: impl Fn(usize, &mut Window, &mut App) + 'static,
    cx: &App,
) -> AnyElement {
    let trigger = prompt_box_control(control_id, icon, control.label, control.enabled, cx);

    if !control.enabled || control.rows.is_empty() {
        return trigger.into_any_element();
    }

    prompt_box_dropdown(dropdown_id, trigger, control.rows, on_pick, cx).into_any_element()
}

fn branch_indicator(branch: Option<SharedString>, cx: &App) -> Option<Stateful<Div>> {
    let branch = branch?;

    Some(prompt_box_control(
        BRANCH_CONTROL_ID,
        Icon::default().path(GIT_BRANCH_ICON_PATH),
        branch,
        false,
        cx,
    ))
}
