//! Sidebar presentation and intent.
//!
//! The sidebar observes [`FleetStore`] directly and owns only its local view
//! state (the two collapse toggles and the selected-session highlight). It
//! emits product intents with stable values rather than asking the composition
//! shell to interpret row indexes.
//!
//! Top to bottom: the New session button, the machines listing, the sessions
//! list (docs/SPEC.md §UI "Layout"). There is no workspaces section — a
//! repository is picked in the chat that is about to use one, and
//! `workspaces.json` is that picker's backing store rather than a citizen with
//! a list of its own.

use gpui::{div, prelude::*, px, App, Context, Div, Entity, EventEmitter, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::ListItem;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Icon, IconName, Sizable as _};
use harvester_client_core::LOCAL_MACHINE;
use harvester_proto::SessionStatus;

use super::machine_presentation::present_machine;
use crate::assets::TRASH_ICON_PATH;
use crate::store::{FleetStore, MachineSession};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// The subtitle of a machine its provider stopped listing, and the only row
/// kind that can be forgotten.
const MISSING_MACHINE_DETAIL: &str = "no longer listed";
/// What the trash button on a missing machine row says it does. "Forget" rather
/// than "delete": nothing on the machine is touched, and there is nothing
/// reachable to touch.
const FORGET_MACHINE_TOOLTIP: &str = "Forget this machine";

/// What a sidebar click asks the composition shell to do. Values are captured
/// from the rendered row so a later store repaint cannot move the click to a
/// different session.
#[derive(Clone)]
pub(super) enum SidebarIntent {
    /// Open a chat with no session behind it. Where it runs is the chat's own
    /// question now — the prompt strip picks machine and repository — so this
    /// intent carries nothing (docs/SPEC.md §Flows "Start session").
    NewSession,
    OpenSession {
        sid: String,
    },
    /// Forget a machine its provider no longer lists, after the shell has asked
    /// the user to confirm it (docs/SPEC.md §Flows "Forget machine").
    ForgetMachine {
        machine: String,
    },
}

pub(super) struct Sidebar {
    store: Entity<FleetStore>,
    selected_sid: Option<String>,
    machines_collapsed: bool,
    sessions_collapsed: bool,
    _store: gpui::Subscription,
}

impl EventEmitter<SidebarIntent> for Sidebar {}

impl Sidebar {
    pub(super) fn new(store: Entity<FleetStore>, cx: &mut Context<Self>) -> Self {
        let store_subscription = cx.observe(&store, |_, _, cx| cx.notify());

        Self {
            store,
            selected_sid: None,
            machines_collapsed: false,
            sessions_collapsed: false,
            _store: store_subscription,
        }
    }

    /// The shell owns which chat is current; this is only the highlight that
    /// presents that choice. It is not a projection of fleet state.
    pub(super) fn select_session(&mut self, sid: Option<String>, cx: &mut Context<Self>) {
        if self.selected_sid == sid {
            return;
        }

        self.selected_sid = sid;
        cx.notify();
    }

    fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let rows: Vec<SessionRow> = self
            .store
            .read(cx)
            .visible_sessions()
            .into_iter()
            .map(|session| self.session_row(session, cx))
            .collect();
        let empty = rows.is_empty();

        div()
            .id("sessions")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_session_row(index, row, cx)),
            )
            .when(empty, |list| {
                list.child(sidebar_empty_note("no sessions yet", cx))
            })
    }

    /// The sidebar's one action, and the way into every session: a chat opens
    /// with no session behind it, and picks where it runs in its own prompt
    /// strip. There is no machine or repository to choose here first.
    fn render_new_session_button(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div().px_2().pt_2().child(
            Button::new("new-session")
                .icon(IconName::Plus)
                .label("New session")
                .ghost()
                .small()
                .w_full()
                .justify_start()
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarIntent::NewSession))),
        )
    }

    fn render_machine_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let provider_rows: Vec<_> = store
            .provider_machines()
            .iter()
            .enumerate()
            .map(|(index, row)| {
                self.render_machine_row(
                    ("provider-machine", index),
                    row.label.clone().into(),
                    Some(row.provider.clone().into()),
                    &row.key,
                    cx,
                )
            })
            .collect();
        let missing_rows: Vec<_> = store
            .missing_tracked_machines()
            .into_iter()
            .enumerate()
            .map(|(index, machine)| self.render_missing_machine_row(index, machine, cx))
            .collect();
        let ad_hoc_rows: Vec<_> = store
            .ad_hoc_machines()
            .into_iter()
            .enumerate()
            .map(|(index, machine)| {
                self.render_machine_row(
                    ("ad-hoc-machine", index),
                    machine.clone().into(),
                    Some("ssh".into()),
                    &machine,
                    cx,
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .child(self.render_machine_row(
                "local-machine",
                LOCAL_MACHINE.into(),
                None,
                LOCAL_MACHINE,
                cx,
            ))
            .children(provider_rows)
            .children(missing_rows)
            .children(ad_hoc_rows)
    }

    fn render_machine_row(
        &self,
        id: impl Into<gpui::ElementId>,
        label: SharedString,
        detail: Option<SharedString>,
        key: &str,
        cx: &Context<Self>,
    ) -> ListItem {
        let dot = present_machine(self.store.read(cx).state(key), key).dot;

        sidebar_row(id)
            .disabled(true)
            .child(stacked_row(dot, label, detail, None, cx))
    }

    /// The one machine row that can be acted on. A machine its provider stopped
    /// listing is the only one Harvester can be sure is gone, so it is the only
    /// one worth offering to forget — every other row is still listed, and a
    /// reachable machine's rows are not the user's to clean up here.
    ///
    /// The trash lives in the row's suffix, which `ListItem` renders outside
    /// its own click gating: the row itself stays disabled, exactly as the
    /// other machine rows are.
    fn render_missing_machine_row(
        &self,
        index: usize,
        machine: String,
        cx: &Context<Self>,
    ) -> ListItem {
        let label = self.store.read(cx).label(&machine);
        let sidebar = cx.entity();

        self.render_machine_row(
            ("missing-machine", index),
            label.into(),
            Some(MISSING_MACHINE_DETAIL.into()),
            &machine,
            cx,
        )
        .suffix(move |_window, _cx| {
            let sidebar = sidebar.clone();
            let machine = machine.clone();

            Button::new(("forget-machine", index))
                .icon(Icon::default().path(TRASH_ICON_PATH))
                .ghost()
                .xsmall()
                .tooltip(FORGET_MACHINE_TOOLTIP)
                .on_click(move |_, _, cx| {
                    let machine = machine.clone();

                    sidebar.update(cx, |_, cx| {
                        cx.emit(SidebarIntent::ForgetMachine { machine })
                    });
                })
        })
    }

    fn session_row(&self, session: &MachineSession, cx: &Context<Self>) -> SessionRow {
        let store = self.store.read(cx);
        let info = &session.info;

        SessionRow {
            title: match &info.name {
                Some(name) => name.clone().into(),
                None => sid_preview(&info.sid).into(),
            },
            subtitle: format!(
                "{} · {}",
                folder_name(&info.cwd),
                store.label(&session.machine)
            )
            .into(),
            tone: match info.status {
                SessionStatus::Running => Tone::Live,
                SessionStatus::Exited => Tone::Idle,
            },
            selected: self.selected_sid.as_deref() == Some(info.sid.as_str()),
            opening: store.opening_sid() == Some(info.sid.as_str()),
            sid: info.sid.clone(),
        }
    }

    fn render_session_row(
        &self,
        index: usize,
        row: SessionRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let sid = row.sid;
        let body = stacked_row(
            ui::Dot::Solid(row.tone),
            row.title,
            Some(row.subtitle),
            row.opening.then(|| "opening…".into()),
            cx,
        );

        sidebar_row(("session", index))
            .selected(row.selected)
            .child(body)
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarIntent::OpenSession { sid: sid.clone() })
            }))
    }
}

impl Render for Sidebar {
    fn render(&mut self, _window: &mut gpui::Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().sidebar_border)
            .child(self.render_new_session_button(cx))
            .child(
                ui::section_header("machines-header", "machines", cx)
                    .child(ui::section_chevron(!self.machines_collapsed))
                    .on_click(cx.listener(|sidebar, _, _, cx| {
                        sidebar.machines_collapsed = !sidebar.machines_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.machines_collapsed, |sidebar| {
                sidebar.child(self.render_machine_list(cx))
            })
            .child(
                ui::section_header("sessions-header", "sessions", cx)
                    .child(ui::section_chevron(!self.sessions_collapsed))
                    .on_click(cx.listener(|sidebar, _, _, cx| {
                        sidebar.sessions_collapsed = !sidebar.sessions_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.sessions_collapsed, |sidebar| {
                sidebar.child(self.render_session_list(cx))
            })
    }
}

struct SessionRow {
    title: SharedString,
    subtitle: SharedString,
    tone: Tone,
    selected: bool,
    opening: bool,
    sid: String,
}

fn sid_preview(sid: &str) -> String {
    sid.chars().take(SID_PREVIEW_CHARS).collect()
}

fn sidebar_row(id: impl Into<gpui::ElementId>) -> ListItem {
    ListItem::new(id)
        .mx(px(6.))
        .px(px(6.))
        .py_1()
        .rounded_md()
        .text_size(px(13.))
}

fn stacked_row(
    dot: ui::Dot,
    title: SharedString,
    detail: Option<SharedString>,
    note: Option<SharedString>,
    cx: &App,
) -> Div {
    v_flex()
        .w_full()
        .min_w(px(0.))
        .child(
            h_flex()
                .gap_2()
                .child(ui::status_dot(dot, cx))
                .child(div().overflow_hidden().text_ellipsis().child(title))
                .children(note.map(|note| {
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(note)
                })),
        )
        .children(detail.map(|detail| {
            div()
                .pl(px(14.))
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .overflow_hidden()
                .text_ellipsis()
                .child(detail)
        }))
}

fn sidebar_empty_note(text: &'static str, cx: &App) -> Div {
    div()
        .px_3()
        .py_1()
        .text_xs()
        .text_color(cx.theme().muted_foreground)
        .child(text)
}

#[cfg(test)]
mod tests {
    use super::sid_preview;

    #[test]
    fn session_fallback_titles_are_short_and_unicode_safe() {
        assert_eq!(sid_preview("0123456789"), "01234567");
        assert_eq!(sid_preview("αβγ"), "αβγ");
    }
}
