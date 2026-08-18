//! Sidebar presentation and intent.
//!
//! The sidebar observes [`FleetStore`] directly and owns only its local view
//! state (the three collapse toggles and the selected-session highlight). It
//! emits product intents with stable values rather than asking the composition
//! shell to interpret row indexes.

use gpui::{div, prelude::*, px, App, Context, Div, Entity, EventEmitter, SharedString};
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::list::ListItem;
use gpui_component::{h_flex, v_flex, ActiveTheme as _, Disableable as _, IconName, Sizable as _};
use harvester_client_core::LOCAL_MACHINE;
use harvester_proto::SessionStatus;

use super::machine_presentation::present_machine;
use crate::store::{FleetStore, MachineSession};
use crate::ui::{self, Tone};
use crate::workspaces::folder_name;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;

/// What a sidebar click asks the composition shell to do. Values are captured
/// from the rendered row so a later store repaint cannot move the click to a
/// different workspace or session.
#[derive(Clone)]
pub(super) enum SidebarIntent {
    AddWorkspace,
    NewSession { machine: String, path: String },
    OpenSession { sid: String },
}

pub(super) struct Sidebar {
    store: Entity<FleetStore>,
    selected_sid: Option<String>,
    machines_collapsed: bool,
    workspaces_collapsed: bool,
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
            workspaces_collapsed: false,
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

    fn render_workspaces_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_add = !self.store.read(cx).machine_candidates().is_empty();

        ui::section_header("workspaces-header", "workspaces", cx)
            .child(
                Button::new("add-workspace")
                    .icon(IconName::Plus)
                    .ghost()
                    .xsmall()
                    .disabled(!can_add)
                    .on_click(cx.listener(|_, _, _, cx| {
                        cx.stop_propagation();
                        cx.emit(SidebarIntent::AddWorkspace);
                    })),
            )
            .child(ui::section_chevron(!self.workspaces_collapsed))
            .on_click(cx.listener(|sidebar, _, _, cx| {
                sidebar.workspaces_collapsed = !sidebar.workspaces_collapsed;
                cx.notify();
            }))
    }

    fn render_workspace_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let rows: Vec<WorkspaceRow> = store
            .workspaces()
            .iter()
            .map(|workspace| WorkspaceRow {
                machine: workspace.machine.clone(),
                path: workspace.path.clone(),
                folder: folder_name(&workspace.path).to_string().into(),
                machine_label: store.label(&workspace.machine).into(),
            })
            .collect();
        let empty = rows.is_empty();

        div()
            .flex()
            .flex_col()
            .children(
                rows.into_iter()
                    .enumerate()
                    .map(|(index, row)| self.render_workspace_row(index, row, cx)),
            )
            .when(empty, |list| {
                list.child(sidebar_empty_note("no workspaces yet", cx))
            })
    }

    fn render_workspace_row(
        &self,
        index: usize,
        row: WorkspaceRow,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let machine = row.machine;
        let path = row.path;

        sidebar_row(("workspace", index))
            .disabled(true)
            .child(
                h_flex()
                    .w_full()
                    .items_baseline()
                    .gap_1()
                    .child(
                        div()
                            .flex_none()
                            .max_w(px(140.))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(row.folder),
                    )
                    .child(
                        div()
                            .text_xs()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(format!("in {}", row.machine_label)),
                    )
                    .child(div().flex_1()),
            )
            .suffix({
                let sidebar = cx.entity();

                move |window, _| {
                    let machine = machine.clone();
                    let path = path.clone();

                    Button::new(("workspace-add", index))
                        .icon(IconName::Plus)
                        .ghost()
                        .xsmall()
                        .on_click(window.listener_for(&sidebar, move |_, _, _, cx| {
                            cx.emit(SidebarIntent::NewSession {
                                machine: machine.clone(),
                                path: path.clone(),
                            });
                        }))
                }
            })
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
            .map(|(index, machine)| {
                self.render_machine_row(
                    ("missing-machine", index),
                    store.label(&machine).into(),
                    Some("no longer listed".into()),
                    &machine,
                    cx,
                )
            })
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
    ) -> impl IntoElement {
        let dot = present_machine(self.store.read(cx).state(key), key).dot;

        sidebar_row(id)
            .disabled(true)
            .child(stacked_row(dot, label, detail, None, cx))
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
            .child(self.render_workspaces_header(cx))
            .when(!self.workspaces_collapsed, |sidebar| {
                sidebar.child(self.render_workspace_list(cx))
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

struct WorkspaceRow {
    machine: String,
    path: String,
    folder: SharedString,
    machine_label: SharedString,
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
