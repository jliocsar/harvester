//! What is left of the hand-rolled design system: the prompt box, the tool
//! block, and the frame around a chat message. Rows, dialogs, lists, the text
//! input and a message's own markdown are `gpui-component`'s (SPEC C11).
//!
//! Every helper takes `&App` and reads `cx.theme()`. Nothing here holds a
//! colour of its own — that is the whole point of the palette being gone.
//! Components return `Div`/`Stateful<Div>` so callers can keep chaining
//! (`.on_click`, extra children); behavior stays with the caller, looks stay
//! here.

use std::sync::OnceLock;

use gpui::{div, prelude::*, px, rems, App, Div, ElementId, FontWeight, SharedString, Stateful};
use gpui_component::highlighter::HighlightTheme;
use gpui_component::text::{TextView, TextViewStyle};
use gpui_component::ActiveTheme as _;

/// Statusbar height; the workbench and the statusbar frame must agree.
const STATUSBAR_HEIGHT: f32 = 26.;

/// What a component is being told about its subject — the vocabulary that
/// crosses the seam into `ui`, in place of the pixels it used to carry. A
/// component handed a colour has had a theme decision made for it by its
/// caller and can never be re-themed; one handed [`Tone::Live`] has not.
///
/// Deliberately small, and named for meaning rather than for colour: `Live`
/// rather than `Green`, because the day a theme paints "live" differently is
/// the day the name has to survive it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Present, but not what the user is looking for: secondary labels.
    Muted,
    /// Backgrounded: an unprobed machine, a tool that has finished.
    Idle,
    /// What the user is doing right now.
    Active,
    /// Working; the outcome is not known yet.
    Pending,
    /// Confirmed good — a connected daemon, a running session.
    Live,
    /// Failed, and the user needs to see that it did.
    Failed,
}

impl Tone {
    /// The one place a meaning becomes a colour.
    ///
    /// `Active` is `blue` rather than `primary`: in the library's dark theme
    /// `primary` is near-white, which is right for a filled button and wrong
    /// for the word "you". `Idle` is `Muted` faded, the one distinction the
    /// library has no token for.
    pub fn color(self, cx: &App) -> gpui::Hsla {
        let theme = cx.theme();

        match self {
            Tone::Muted => theme.muted_foreground,
            Tone::Idle => theme.muted_foreground.opacity(IDLE_OPACITY),
            Tone::Active => theme.blue,
            Tone::Pending => theme.warning,
            Tone::Live => theme.success,
            Tone::Failed => theme.danger,
        }
    }
}

/// How far `Tone::Idle` sits behind `Tone::Muted`, and what the faint text
/// throughout this module is faded by.
const IDLE_OPACITY: f32 = 0.6;

/// Secondary text: labels, subtitles, the dim half of a two-tone line.
fn dim(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground
}

/// Tertiary text: context lines, disabled controls, disclosure chevrons.
fn faint(cx: &App) -> gpui::Hsla {
    cx.theme().muted_foreground.opacity(IDLE_OPACITY)
}

/// Sidebar section label ("MACHINES", "SESSIONS", …) — Apple-sidebar style:
/// small, semibold, faint. Collapsible: the whole row is the toggle (caller
/// chains `.on_click`); the caller adds optional extras (the workspaces "+")
/// and then `section_chevron` so the disclosure sits rightmost.
pub fn section_header(id: impl Into<ElementId>, title: &'static str, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .pt_3()
        .pb_1()
        .flex()
        .items_center()
        .gap_2()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(faint(cx))
        .cursor_pointer()
        .hover(|style| style.text_color(dim(cx)))
        .child(title.to_uppercase())
        .child(div().flex_1())
}

/// The section header's trailing disclosure chevron.
pub fn section_chevron(expanded: bool) -> Div {
    div().flex_none().child(if expanded { "▾" } else { "▸" })
}

/// The one dot: a machine's link, a session's liveness, a tool call's status.
pub fn status_dot(tone: Tone, cx: &App) -> Div {
    div()
        .flex_none()
        .size(px(6.))
        .rounded_full()
        .bg(tone.color(cx))
}

/// Assistant prose, rendered as markdown.
///
/// The state behind this lives in the window, keyed by `id`, and
/// `TextViewState::set_text` returns early when the text is unchanged — so a
/// settled message parses once and costs nothing on later frames, while the
/// streaming partial reparses per chunk. That reparse is bounded by one
/// message, and it is synchronous, which is what keeps the laid-out height
/// correct on the first frame (SPEC C11).
pub fn markdown(id: impl Into<ElementId>, text: impl Into<SharedString>) -> TextView {
    TextView::markdown(id, text)
        .selectable(true)
        .style(markdown_style())
}

/// The one markdown style, built once: the library's default is the light
/// highlight theme, and the app is pinned dark.
fn markdown_style() -> TextViewStyle {
    static STYLE: OnceLock<TextViewStyle> = OnceLock::new();

    STYLE
        .get_or_init(|| TextViewStyle {
            highlight_theme: HighlightTheme::default_dark(),
            is_dark: true,
            // The library's 1rem is book spacing; a chat turn is denser.
            paragraph_gap: rems(0.5),
            ..TextViewStyle::default()
        })
        .clone()
}

/// One chat message: author label over the body; user messages get a bubble.
pub fn chat_message(
    author: &'static str,
    author_tone: Tone,
    body: impl IntoElement,
    bubble: bool,
    cx: &App,
) -> Div {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xs()
                .text_color(author_tone.color(cx))
                .child(author),
        )
        .child(
            div()
                .when(bubble, |style| {
                    style.bg(cx.theme().muted).rounded_md().px_3().py_2()
                })
                .child(body),
        )
}

/// The agent prompt box (SPEC §Flows "Start session", chat input): one
/// rounded card holding the text input on top and a controls row at the
/// bottom of the same frame; the focus ring while the input is focused.
/// Compose with `prompt_box_input` and `prompt_box_controls`.
pub fn prompt_box(focused: bool, cx: &App) -> Div {
    div()
        .rounded_xl()
        .bg(cx.theme().input)
        .border_1()
        .border_color(if focused {
            cx.theme().ring
        } else {
            cx.theme().border
        })
        .flex()
        .flex_col()
}

/// The prompt box's input slot: padding around the caller's `Input`.
pub fn prompt_box_input(input: impl IntoElement) -> Div {
    div().px_3().pt_3().pb_2().child(input)
}

/// The prompt box's bottom row. Caller adds `prompt_box_control`s, a
/// `flex_1` spacer, then `prompt_box_send`.
pub fn prompt_box_controls() -> Div {
    div().px_3().pb_2().flex().items_center().gap_2()
}

/// A flat dropdown control inside the prompt box (the model picker trigger):
/// quiet dim text with a caret, brightening on hover. Disabled ones lose the
/// caret and the pointer affordances — the caller must also skip the click
/// handler (loading / no models to offer).
pub fn prompt_box_control(
    id: impl Into<ElementId>,
    label: SharedString,
    enabled: bool,
    cx: &App,
) -> Stateful<Div> {
    let base = div()
        .id(id)
        .text_xs()
        // Cap so an unreasonably long model name leaves room for the rest
        // of the row.
        .max_w(px(220.))
        .overflow_hidden()
        .text_ellipsis();

    if enabled {
        base.text_color(dim(cx))
            .cursor_pointer()
            .hover(|style| style.text_color(cx.theme().foreground))
            .child(format!("{label} ▾"))
    } else {
        base.text_color(faint(cx)).child(label)
    }
}

/// Diameter of the prompt box's round send button.
const SEND_BUTTON_SIZE: f32 = 22.;

/// The prompt box's round send button ("↑"). Caller chains `.on_click`; Enter
/// in the input is the other path to the same submit.
pub fn prompt_box_send(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(SEND_BUTTON_SIZE))
        .flex_none()
        .rounded_full()
        .bg(cx.theme().primary)
        .text_color(cx.theme().primary_foreground)
        .flex()
        .items_center()
        .justify_center()
        .text_xs()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().primary_hover))
        .active(|style| style.bg(cx.theme().primary_active))
        .child("↑")
}

/// Chat pane header: title + faint subtitle (the unstarted chat's "New
/// session" line — the model control lives in the prompt box, not here).
pub fn chat_header(title: SharedString, subtitle: SharedString, cx: &App) -> Div {
    div()
        .flex_none()
        .px_3()
        .py_2()
        .border_b_1()
        .border_color(cx.theme().border)
        .flex()
        .items_center()
        .gap_2()
        .child(
            div()
                .flex_none()
                .font_weight(FontWeight::MEDIUM)
                .child(title),
        )
        .child(
            div()
                .text_xs()
                .text_color(faint(cx))
                .overflow_hidden()
                .text_ellipsis()
                .child(subtitle),
        )
}

/// The monospace family tool-block bodies render in. gpui matches families by
/// exact name — there is no generic "monospace" resolution — so someone has to
/// resolve one; that someone is the **application**, at startup
/// ([`main::resolve_mono_font_family`]), because asking the system means
/// spawning a subprocess and a rendering module must not do that.
///
/// Unset, it answers `Menlo`: a family every macOS ships, and on Linux a miss
/// falls back to gpui's own default — so the components stay usable without an
/// application around them.
pub fn use_mono_font_family(family: SharedString) {
    let _ = MONO_FONT_FAMILY.set(family);
}

pub fn mono_font_family() -> SharedString {
    MONO_FONT_FAMILY.get().cloned().unwrap_or("Menlo".into())
}

static MONO_FONT_FAMILY: OnceLock<SharedString> = OnceLock::new();

/// A tool-call block (SPEC §Rendering pi): rounded bordered card holding a
/// `tool_block_header` and, when expanded, a `tool_block_body`. Sits on the
/// chat background; the sidebar fill sets it apart from message bubbles.
pub fn tool_block(cx: &App) -> Div {
    div()
        .rounded_lg()
        .bg(cx.theme().sidebar)
        .border_1()
        .border_color(cx.theme().border)
        .flex()
        .flex_col()
        .overflow_hidden()
}

/// The block's clickable one-liner: status dot, the caller-composed label
/// (`tool_block_label`), trailing chevron. Caller chains `.on_click` to
/// toggle expansion. Dot color: accent = running, faint = done, red = error.
pub fn tool_block_header(
    id: impl Into<ElementId>,
    dot: Tone,
    label: Div,
    expanded: bool,
    cx: &App,
) -> Stateful<Div> {
    div()
        .id(id)
        .px_3()
        .py(px(6.))
        .flex()
        .items_center()
        .gap_2()
        .cursor_pointer()
        .hover(|style| style.bg(cx.theme().list_hover))
        .child(status_dot(dot, cx))
        .child(label.flex_1().min_w(px(0.)))
        .child(
            div()
                .flex_none()
                .text_xs()
                .text_color(faint(cx))
                .child(if expanded { "▾" } else { "▸" }),
        )
}

/// The header label row; compose with `tool_block_verb`, `tool_block_subject`
/// and `tool_block_stats`.
pub fn tool_block_label() -> Div {
    div()
        .flex()
        .items_baseline()
        .gap_2()
        .overflow_hidden()
        .text_sm()
}

/// The label's dim leading verb ("Read", "Edit", "$").
pub fn tool_block_verb(text: impl Into<SharedString>, cx: &App) -> Div {
    div().flex_none().text_color(dim(cx)).child(text.into())
}

/// The label's subject: file basename, pattern, or the bash command line
/// (`mono` for command text).
pub fn tool_block_subject(text: SharedString, mono: bool) -> Div {
    div()
        .overflow_hidden()
        .text_ellipsis()
        .when(mono, |subject| subject.font_family(mono_font_family()))
        .child(text)
}

/// "+N −N" line stats for edit/write labels.
pub fn tool_block_stats(added: usize, removed: usize, cx: &App) -> Div {
    div()
        .flex_none()
        .flex()
        .gap_1()
        .text_xs()
        .child(
            div()
                .text_color(cx.theme().success)
                .child(format!("+{added}")),
        )
        .child(
            div()
                .text_color(cx.theme().danger)
                .child(format!("−{removed}")),
        )
}

/// The expanded body under the header: monospace, small, separated by a
/// hairline. Compose with `tool_block_context`, `tool_block_output`,
/// `tool_block_diff_line` and `tool_block_truncation_notice`.
pub fn tool_block_body(cx: &App) -> Div {
    div()
        .border_t_1()
        .border_color(cx.theme().border)
        .px_3()
        .py_2()
        .flex()
        .flex_col()
        .gap_1()
        .text_xs()
        .font_family(mono_font_family())
}

/// The body's faint context line (full path, full command, raw args).
pub fn tool_block_context(text: SharedString, cx: &App) -> Div {
    div().text_color(faint(cx)).child(text)
}

/// The body's output text (tool result or live partial snapshot).
pub fn tool_block_output(text: SharedString, cx: &App) -> Div {
    div().text_color(dim(cx)).child(text)
}

/// One line of an edit/write diff body.
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
}

pub fn tool_block_diff_line(kind: DiffLineKind, text: SharedString, cx: &App) -> Div {
    let color = match kind {
        DiffLineKind::Context => dim(cx),
        DiffLineKind::Added => cx.theme().success,
        DiffLineKind::Removed => cx.theme().danger,
    };

    div().text_color(color).child(text)
}

/// Shown above capped output — the head was dropped, the tail is live.
pub fn tool_block_truncation_notice(cx: &App) -> Div {
    div().text_color(faint(cx)).child("… output truncated")
}

/// The bottom statusbar container; caller adds its content children.
pub fn statusbar_frame(cx: &App) -> Div {
    div()
        .h(px(STATUSBAR_HEIGHT))
        .flex_none()
        .flex()
        .items_center()
        .gap_3()
        .px_3()
        .border_t_1()
        .border_color(cx.theme().status_bar_border)
        .bg(cx.theme().status_bar)
        .text_xs()
}
