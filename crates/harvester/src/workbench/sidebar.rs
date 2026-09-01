use gpui::{div, prelude::*, Context, Entity, EventEmitter, Role, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::ListItem;
use gpui_component::{ActiveTheme as _, Icon, IconName};
use harvester_client_core::{
    folder_name, ChatId, MachineId, MachineSnapshot, MachineStatus, SessionKey, SessionLifecycle,
    SessionSummary,
};
use harvester_proto::SessionStatus;

use super::machine_presentation::{present_machine, MISSING_MACHINE_DETAIL};
use crate::assets::TRASH_ICON_PATH;
use crate::chat::NEW_SESSION_LABEL;
use crate::store::FleetStore;
use crate::ui::{self, KeyboardFocusRing as _, Tone};

const SID_PREVIEW_CHARS: usize = 8;
const FORGET_MACHINE_TOOLTIP: &str = "Forget this machine";
const MACHINES_SECTION_TITLE: &str = "Machines";
const SESSIONS_SECTION_TITLE: &str = "Sessions";
const NO_SESSIONS_NOTE: &str = "No sessions yet";
const OPENING_SESSION_NOTE: &str = "opening…";

pub(super) enum SidebarIntent {
    NewSession,
    OpenSession { session: SessionKey },
    ForgetMachine { machine: MachineId },
}

pub(super) struct Sidebar {
    store: Entity<FleetStore>,
    selected: Option<ChatId>,
    machines_collapsed: bool,
    sessions_collapsed: bool,
    _store_repaint: gpui::Subscription,
}

impl EventEmitter<SidebarIntent> for Sidebar {}

impl Sidebar {
    pub(super) fn new(store: Entity<FleetStore>, cx: &mut Context<'_, Self>) -> Self {
        let store_repaint = cx.observe(&store, |_, _, cx| cx.notify());

        Self {
            store,
            selected: None,
            machines_collapsed: false,
            sessions_collapsed: false,
            _store_repaint: store_repaint,
        }
    }

    pub(super) fn select(&mut self, selected: Option<ChatId>, cx: &mut Context<'_, Self>) {
        self.selected = selected;
        cx.notify();
    }

    fn render_session_list(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let rows: Vec<SessionRow> = snapshot
            .sessions()
            .iter()
            .map(|session| self.session_row(session, snapshot))
            .collect();
        let empty = rows.is_empty();

        div()
            .id("sessions")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_session_row(index, row, cx)),
            )
            .when(empty, |list| {
                list.child(ui::sidebar_empty_note(NO_SESSIONS_NOTE, cx))
            })
    }

    fn render_new_session_button(&self, cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().px(ui::SIDEBAR_EDGE_GUTTER).py_2().child(
            Button::new("new-session")
                .icon(IconName::Plus)
                .label(NEW_SESSION_LABEL)
                .ghost()
                .px_3()
                .on_click(cx.listener(|_, _, _, cx| cx.emit(SidebarIntent::NewSession))),
        )
    }

    fn render_machine_list(&self, cx: &Context<'_, Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let snapshot = store.snapshot();
        let rows: Vec<_> = snapshot
            .machines()
            .iter()
            .enumerate()
            .map(|(index, machine)| self.render_machine_row(index, machine, cx))
            .collect();

        div().flex().flex_col().children(rows)
    }

    fn render_machine_row(
        &self,
        index: usize,
        machine: &MachineSnapshot,
        cx: &Context<'_, Self>,
    ) -> ListItem {
        let presentation = present_machine(&machine.status, &machine.label);
        let detail: Option<SharedString> = match &machine.status {
            MachineStatus::Missing => Some(MISSING_MACHINE_DETAIL.into()),
            _ => machine.provider.clone().map(Into::into),
        };
        let row = ui::sidebar_row(("machine", index))
            .role(Role::ListItem)
            .aria_label(machine.label.clone())
            .disabled(true)
            .tab_stop(false)
            .child(ui::sidebar_stacked_row(
                presentation.tone,
                machine.label.clone().into(),
                detail,
                None,
                cx,
            ));

        if !matches!(&machine.status, MachineStatus::Missing) {
            return row;
        }

        let sidebar = cx.entity();
        let machine_id = machine.id.clone();

        row.suffix(move |_window, _cx| {
            let sidebar = sidebar.clone();
            let machine = machine_id.clone();

            Button::new(("forget-machine", index))
                .icon(Icon::default().path(TRASH_ICON_PATH))
                .ghost()
                .tooltip(FORGET_MACHINE_TOOLTIP)
                .on_click(move |_, _, cx| {
                    let machine = machine.clone();

                    sidebar.update(cx, |_, cx| {
                        cx.emit(SidebarIntent::ForgetMachine { machine });
                    });
                })
        })
    }

    fn session_row(
        &self,
        session: &SessionSummary,
        snapshot: &harvester_client_core::ClientSnapshot,
    ) -> SessionRow {
        let opening = snapshot.live_session(&session.key).is_some_and(|live| {
            matches!(
                live.lifecycle,
                SessionLifecycle::Opening | SessionLifecycle::Starting
            )
        });

        SessionRow {
            title: match &session.name {
                Some(name) => name.clone().into(),
                None => sid_preview(session.key.session().as_str()).into(),
            },
            subtitle: format!(
                "{} · {}",
                folder_name(session.workspace.path()),
                snapshot.machine_label(session.key.machine())
            )
            .into(),
            tone: match session.status {
                SessionStatus::Running => Tone::Live,
                SessionStatus::Exited => Tone::Idle,
            },
            selected: self.selected.as_ref() == Some(&ChatId::Session(session.key.clone())),
            opening,
            key: session.key.clone(),
        }
    }

    fn render_session_row(
        &self,
        index: usize,
        row: SessionRow,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        let session = row.key;
        let title = row.title.clone();
        let body = ui::sidebar_stacked_row(
            row.tone,
            row.title,
            Some(row.subtitle),
            row.opening.then(|| OPENING_SESSION_NOTE.into()),
            cx,
        );

        ui::sidebar_row(("session", index))
            .role(Role::ListItem)
            .aria_label(title)
            .aria_selected(row.selected)
            .selected(row.selected)
            .keyboard_focus_ring(cx)
            .child(body)
            .on_click(cx.listener(move |_, _, _, cx| {
                cx.emit(SidebarIntent::OpenSession {
                    session: session.clone(),
                });
            }))
    }
}

impl Render for Sidebar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut Context<'_, Self>,
    ) -> impl IntoElement {
        div()
            .w_64()
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(self.render_new_session_button(cx))
            .child(ui::collapsible_section(
                "machines-header",
                MACHINES_SECTION_TITLE,
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
                SESSIONS_SECTION_TITLE,
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
    key: SessionKey,
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
