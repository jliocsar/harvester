//! The kitchen: a live catalog of the design system (`harvester kitchen`).
//! Every `ui.rs` primitive gets a section showing its variants with real
//! hover/active/focus states — new components are cooked and tasted here
//! before production use. No bridge, no daemon: pure rendering.

use gpui::{div, prelude::*, px, rgb, rgba, Context, Entity, Focusable, Window};

use crate::input::TextInput;
use crate::theme;
use crate::ui;

/// Palette swatches, in theme.rs order. `SELECTION_RGBA` is rendered
/// separately (it needs `rgba`).
const PALETTE: &[(&str, u32)] = &[
    ("WINDOW_BG", theme::WINDOW_BG),
    ("PANEL_BG", theme::PANEL_BG),
    ("ELEVATED_BG", theme::ELEVATED_BG),
    ("INPUT_BG", theme::INPUT_BG),
    ("BORDER", theme::BORDER),
    ("TEXT", theme::TEXT),
    ("TEXT_DIM", theme::TEXT_DIM),
    ("TEXT_FAINT", theme::TEXT_FAINT),
    ("ACCENT", theme::ACCENT),
    ("ACCENT_HOVER", theme::ACCENT_HOVER),
    ("ACCENT_ACTIVE", theme::ACCENT_ACTIVE),
    ("STATUS_RUNNING", theme::STATUS_RUNNING),
    ("STATUS_ERROR", theme::STATUS_ERROR),
    ("BUTTON_BG", theme::BUTTON_BG),
    ("BUTTON_HOVER", theme::BUTTON_HOVER),
    ("BUTTON_ACTIVE", theme::BUTTON_ACTIVE),
    ("ROW_HOVER", theme::ROW_HOVER),
    ("ROW_SELECTED", theme::ROW_SELECTED),
];

pub struct Kitchen {
    prompt_input: Entity<TextInput>,
    path_input: Entity<TextInput>,
}

impl Kitchen {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            prompt_input: cx.new(|cx| TextInput::new("Message pi — Enter to send", cx)),
            path_input: cx.new(|cx| TextInput::new("/absolute/path/to/workspace", cx)),
        }
    }
}

impl Render for Kitchen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let prompt_focused = self
            .prompt_input
            .read(cx)
            .focus_handle(cx)
            .is_focused(window);
        let path_focused = self.path_input.read(cx).focus_handle(cx).is_focused(window);

        div()
            .id("kitchen")
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(theme::WINDOW_BG))
            .text_color(rgb(theme::TEXT))
            .text_size(px(13.))
            .child(
                div()
                    .max_w(px(760.))
                    .p_6()
                    .flex()
                    .flex_col()
                    .gap_6()
                    .child(section("palette", render_palette()))
                    .child(section("typography", render_typography()))
                    .child(section("buttons", render_buttons()))
                    .child(section("status dots", render_status_dots()))
                    .child(section("session rows", render_session_rows()))
                    .child(section(
                        "text input",
                        div()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(labeled(
                                "chat prompt (click to focus)",
                                ui::input_frame(prompt_focused).child(self.prompt_input.clone()),
                            ))
                            .child(labeled(
                                "path field",
                                ui::input_frame(path_focused).child(self.path_input.clone()),
                            )),
                    ))
                    .child(section("chat messages", render_chat_messages()))
                    .child(section("modal", render_modal_sample()))
                    .child(section("statusbar", render_statusbar_sample())),
            )
    }
}

/// A catalog section: faint uppercase title over its samples.
fn section(title: &'static str, body: impl IntoElement) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .child(title.to_uppercase()),
        )
        .child(body)
}

/// A sample with a small caption under it.
fn labeled(caption: &'static str, sample: impl IntoElement) -> impl IntoElement {
    div().flex().flex_col().gap_1().child(sample).child(
        div()
            .text_xs()
            .text_color(rgb(theme::TEXT_FAINT))
            .child(caption),
    )
}

fn render_palette() -> impl IntoElement {
    div()
        .flex()
        .flex_wrap()
        .gap_3()
        .children(
            PALETTE
                .iter()
                .map(|&(name, color)| swatch(name, div().bg(rgb(color)))),
        )
        .child(swatch(
            "SELECTION_RGBA",
            div().bg(rgba(theme::SELECTION_RGBA)),
        ))
}

fn swatch(name: &'static str, fill: gpui::Div) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            fill.w(px(96.))
                .h(px(28.))
                .rounded_md()
                .border_1()
                .border_color(rgb(theme::BORDER)),
        )
        .child(
            div()
                .text_xs()
                .text_color(rgb(theme::TEXT_FAINT))
                .child(name),
        )
}

fn render_typography() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_2()
        .child(div().text_xs().child("text_xs — chrome captions, labels"))
        .child(div().text_sm().child("text_sm — buttons, secondary copy"))
        .child(div().child("13px base — chat and content"))
        .child(div().text_lg().child("text_lg — rare emphasis"))
        .child(
            div()
                .flex()
                .gap_4()
                .child(div().text_color(rgb(theme::TEXT)).child("TEXT"))
                .child(div().text_color(rgb(theme::TEXT_DIM)).child("TEXT_DIM"))
                .child(div().text_color(rgb(theme::TEXT_FAINT)).child("TEXT_FAINT"))
                .child(div().text_color(rgb(theme::ACCENT)).child("ACCENT"))
                .child(
                    div()
                        .text_color(rgb(theme::STATUS_ERROR))
                        .child("STATUS_ERROR"),
                ),
        )
}

fn render_buttons() -> impl IntoElement {
    div()
        .flex()
        .items_start()
        .gap_3()
        .child(labeled(
            "neutral",
            ui::button(
                "kitchen-btn-neutral",
                "Cancel",
                ui::ButtonKind::Neutral,
                true,
            ),
        ))
        .child(labeled(
            "primary",
            ui::button("kitchen-btn-primary", "Add", ui::ButtonKind::Primary, true),
        ))
        .child(labeled(
            "neutral disabled",
            ui::button(
                "kitchen-btn-neutral-off",
                "Cancel",
                ui::ButtonKind::Neutral,
                false,
            ),
        ))
        .child(labeled(
            "primary disabled",
            ui::button(
                "kitchen-btn-primary-off",
                "Add",
                ui::ButtonKind::Primary,
                false,
            ),
        ))
}

fn render_status_dots() -> impl IntoElement {
    div()
        .flex()
        .gap_4()
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(ui::status_dot(theme::STATUS_RUNNING))
                .child(div().text_xs().child("running")),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(ui::status_dot(theme::TEXT_FAINT))
                .child(div().text_xs().child("exited")),
        )
}

fn render_session_rows() -> impl IntoElement {
    // Sidebar-width container so ellipsis behavior shows realistically.
    div()
        .w(px(240.))
        .bg(rgb(theme::PANEL_BG))
        .border_1()
        .border_color(rgb(theme::BORDER))
        .rounded_md()
        .py_1()
        .flex()
        .flex_col()
        .child(ui::session_row(
            "kitchen-row-selected",
            "Refactor auth".into(),
            "bloggy".into(),
            theme::STATUS_RUNNING,
            true,
            false,
        ))
        .child(ui::session_row(
            "kitchen-row-opening",
            "019fc40d".into(),
            "harvester".into(),
            theme::STATUS_RUNNING,
            false,
            true,
        ))
        .child(ui::session_row(
            "kitchen-row-exited",
            "A very long session title that should ellipsize".into(),
            "some-deeply-nested-workspace-name".into(),
            theme::TEXT_FAINT,
            false,
            false,
        ))
}

fn render_chat_messages() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(ui::chat_message(
            "you",
            theme::ACCENT,
            "Deploy the fix to staging and run the smoke tests.",
            true,
        ))
        .child(ui::chat_message(
            "pi",
            theme::TEXT_DIM,
            "Running the deploy now.\n\n[bash]",
            false,
        ))
        .child(ui::chat_message(
            "pi",
            theme::TEXT_DIM,
            "Streaming a partial answer▍",
            false,
        ))
}

fn render_modal_sample() -> impl IntoElement {
    ui::modal_frame("Add workspace")
        .child(
            div()
                .text_sm()
                .text_color(rgb(theme::TEXT_DIM))
                .child("Path to a git repository on the machine."),
        )
        .child(
            ui::input_frame(false).child(
                div()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child("/home/exedev/bloggy"),
            ),
        )
        .child(
            div()
                .flex()
                .justify_end()
                .gap_2()
                .child(ui::button(
                    "kitchen-modal-cancel",
                    "Cancel",
                    ui::ButtonKind::Neutral,
                    true,
                ))
                .child(ui::button(
                    "kitchen-modal-add",
                    "Add",
                    ui::ButtonKind::Primary,
                    true,
                )),
        )
}

fn render_statusbar_sample() -> impl IntoElement {
    ui::statusbar_frame()
        .child(
            div()
                .text_color(rgb(theme::TEXT_FAINT))
                .child("exedev@egg-by-penguin.exe.xyz · harvesterd 0.1.0"),
        )
        .child(div().flex_1())
        .child(
            div()
                .text_color(rgb(theme::STATUS_ERROR))
                .child("request failed: no running session"),
        )
}
