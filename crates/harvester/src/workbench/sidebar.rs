use gpui::{div, prelude::*, px, Context, Entity, EventEmitter, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::ListItem;
use gpui_component::{ActiveTheme as _, Icon, IconName, Sizable as _};
use harvester_client_core::LOCAL_MACHINE;
use harvester_proto::SessionStatus;

use super::machine_presentation::present_machine;
use crate::assets::TRASH_ICON_PATH;
use crate::store::{FleetStore, MachineSession};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;

const SIDEBAR_WIDTH: f32 = 240.;
const SID_PREVIEW_CHARS: usize = 8;
const MISSING_MACHINE_DETAIL: &str = "no longer listed";
const FORGET_MACHINE_TOOLTIP: &str = "Forget this machine";

#[derive(Clone)]
pub(super) enum SidebarIntent {
    NewSession,
    OpenSession { sid: String },
    ForgetMachine { machine: String },
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
                list.child(ui::sidebar_empty_note("No sessions yet", cx))
            })
    }

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

        ui::sidebar_row(id)
            .disabled(true)
            .child(ui::sidebar_stacked_row(dot, label, detail, None, cx))
    }

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
        let body = ui::sidebar_stacked_row(
            ui::Dot::Solid(row.tone),
            row.title,
            Some(row.subtitle),
            row.opening.then(|| "opening…".into()),
            cx,
        );

        ui::sidebar_row(("session", index))
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
            .child(ui::collapsible_section(
                "machines-header",
                "machines",
                !self.machines_collapsed,
                cx.listener(|sidebar, _, _, cx| {
                    sidebar.machines_collapsed = !sidebar.machines_collapsed;
                    cx.notify();
                }),
                cx,
            ))
            .when(!self.machines_collapsed, |sidebar| {
                sidebar.child(self.render_machine_list(cx))
            })
            .child(ui::collapsible_section(
                "sessions-header",
                "sessions",
                !self.sessions_collapsed,
                cx.listener(|sidebar, _, _, cx| {
                    sidebar.sessions_collapsed = !sidebar.sessions_collapsed;
                    cx.notify();
                }),
                cx,
            ))
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

#[cfg(test)]
mod tests {
    use super::sid_preview;

    #[test]
    fn session_fallback_titles_are_short_and_unicode_safe() {
        assert_eq!(sid_preview("0123456789"), "01234567");
        assert_eq!(sid_preview("αβγ"), "αβγ");
    }
}
