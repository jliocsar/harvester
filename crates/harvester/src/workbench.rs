//! Top-level UI state and layout (docs/SPEC.md §UI, slice-4 cut): sidebar
//! (machine + sessions), content (chat or empty state), statusbar. Owns the
//! bridge: commands out synchronously, events in on a gpui task awaiting the
//! bridge's channel.

use gpui::{div, prelude::*, px, rgb, Context, Entity, SharedString, Window};
use harvester_client_core::RemoteDirEntry;
use harvester_proto::{SessionInfo, SessionStatus};
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::{Bridge, Command, Event as BridgeEvent};
use crate::chat::Chat;
use crate::theme;
use crate::ui;
use crate::workspaces::{self, Workspace};

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// Fixed height for the picker's directory listing, so the modal doesn't
/// resize as the user walks between small and large directories.
const PICKER_LIST_HEIGHT: f32 = 280.;

/// Connection lifecycle as the statusbar tells it.
enum Link {
    Connecting,
    Connected { daemon_version: String },
    Failed { detail: String },
    Lost,
}

/// The add-workspace modal's state (SPEC §Flows "Add workspace").
struct FolderPicker {
    /// Directory whose entries are displayed — what "Add" would store.
    path: String,
    entries: Vec<RemoteDirEntry>,
    /// A listing is in flight for this path; also the staleness guard —
    /// only the response matching it is applied.
    pending_path: Option<String>,
    validating: bool,
    /// Listing or validation failure, rendered inside the modal.
    error: Option<String>,
}

impl FolderPicker {
    /// While busy, rows and the Add button ignore clicks — one in-flight
    /// request at a time keeps responses unambiguous.
    fn busy(&self) -> bool {
        self.pending_path.is_some() || self.validating
    }
}

pub struct Workbench {
    dest: String,
    link: Link,
    sessions: Vec<SessionInfo>,
    /// User's workspace list (`~/.harvester/workspaces.json`), all machines.
    workspaces: Vec<Workspace>,
    /// Remote `$HOME`, from `Connected` — the picker's start directory.
    remote_home: Option<String>,
    picker: Option<FolderPicker>,
    /// Sid we asked the bridge to open, until `Attached` promotes it to a
    /// chat (also the "don't double-open" guard).
    opening: Option<String>,
    chat: Option<Entity<Chat>>,
    /// Last request failure; sticks in the statusbar until the next one.
    last_error: Option<String>,
    commands: UnboundedSender<Command>,
}

impl Workbench {
    pub fn new(dest: String, bridge: Bridge, cx: &mut Context<Self>) -> Self {
        let Bridge {
            commands,
            mut events,
        } = bridge;

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |workbench, cx| workbench.handle_bridge_event(event, cx))
                    .is_err()
                {
                    // Workbench dropped — window closed; stop pumping.
                    break;
                }
            }
        })
        .detach();

        Self {
            dest,
            link: Link::Connecting,
            sessions: Vec::new(),
            workspaces: workspaces::load(),
            remote_home: None,
            picker: None,
            opening: None,
            chat: None,
            last_error: None,
            commands,
        }
    }

    fn handle_bridge_event(&mut self, event: BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::Connected {
                daemon_version,
                remote_home,
            } => {
                self.link = Link::Connected { daemon_version };
                self.remote_home = Some(remote_home);
            }
            BridgeEvent::ConnectFailed { detail } => self.link = Link::Failed { detail },
            BridgeEvent::Sessions(mut sessions) => {
                // Newest first; uuidv7 sids sort chronologically, so the sid
                // itself is the created-at key.
                sessions.sort_by(|a, b| b.sid.cmp(&a.sid));
                self.sessions = sessions;
            }
            BridgeEvent::Attached { sid, entries } => {
                // The daemon's list may predate a resume we just did; disk on
                // the VM stays authoritative, this is display state only.
                self.set_session_status(&sid, SessionStatus::Running);
                self.opening = None;
                let commands = self.commands.clone();
                self.chat = Some(cx.new(|cx| Chat::new(sid, &entries, commands, cx)));
            }
            BridgeEvent::Pi { sid, event } => {
                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_pi_event(&event, cx));
                }
            }
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.set_session_status(&sid, SessionStatus::Exited);

                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_session_exit(exit_code, cx));
                }
            }
            BridgeEvent::RequestFailed { detail } => {
                self.opening = None;
                self.last_error = Some(detail);
            }
            BridgeEvent::DirListed { path, result } => self.handle_dir_listed(path, result),
            BridgeEvent::WorkspaceValidated { path, error } => {
                self.handle_workspace_validated(path, error)
            }
            BridgeEvent::Disconnected => self.link = Link::Lost,
        }

        cx.notify();
    }

    fn chat_for(&self, sid: &str, cx: &Context<Self>) -> Option<Entity<Chat>> {
        self.chat
            .as_ref()
            .filter(|chat| chat.read(cx).sid() == sid)
            .cloned()
    }

    fn set_session_status(&mut self, sid: &str, status: SessionStatus) {
        for session in &mut self.sessions {
            if session.sid == sid {
                session.status = status;
            }
        }
    }

    fn open_session(&mut self, index: usize, cx: &mut Context<Self>) {
        if self.opening.is_some() {
            return;
        }

        let Some(session) = self.sessions.get(index) else {
            return;
        };
        let already_open = self
            .chat_for(&session.sid, cx)
            .is_some_and(|_| session.status == SessionStatus::Running);

        if already_open {
            return;
        }

        self.opening = Some(session.sid.clone());

        if self
            .commands
            .send(Command::Open {
                sid: session.sid.clone(),
                cwd: session.cwd.clone(),
                running: session.status == SessionStatus::Running,
            })
            .is_err()
        {
            // Bridge gone; the Disconnected event drives the visible state.
            self.opening = None;
        }

        cx.notify();
    }

    /// A `list_dir` answered. A response that doesn't match the pending path
    /// is stale (the picker was closed and reopened meanwhile) — dropped.
    fn handle_dir_listed(&mut self, path: String, result: Result<Vec<RemoteDirEntry>, String>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.pending_path.as_deref() != Some(path.as_str()) {
            return;
        }

        picker.pending_path = None;

        match result {
            Ok(entries) => {
                picker.path = path;
                picker.entries = entries;
                picker.error = None;
            }
            // Listing failed (permissions, vanished dir): keep showing the
            // previous directory, surface the reason.
            Err(detail) => picker.error = Some(detail),
        }
    }

    fn handle_workspace_validated(&mut self, path: String, error: Option<String>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if !picker.validating || picker.path != path {
            return;
        }

        picker.validating = false;

        match error {
            // Not a git repo: clear error in the modal, user re-picks.
            Some(detail) => picker.error = Some(detail),
            None => {
                self.picker = None;
                self.add_workspace(path);
            }
        }
    }

    fn add_workspace(&mut self, path: String) {
        let workspace = Workspace {
            machine: self.dest.clone(),
            path,
        };

        if self.workspaces.contains(&workspace) {
            return;
        }

        self.workspaces.push(workspace);

        if let Err(error) = workspaces::save(&self.workspaces) {
            self.last_error = Some(format!("save workspaces: {error}"));
        }
    }

    fn open_picker(&mut self, cx: &mut Context<Self>) {
        let Some(home) = self.remote_home.clone() else {
            return;
        };

        if self.picker.is_some() {
            return;
        }

        self.picker = Some(FolderPicker {
            path: home.clone(),
            entries: Vec::new(),
            pending_path: Some(home.clone()),
            validating: false,
            error: None,
        });
        self.send_command(Command::ListDir { path: home });
        cx.notify();
    }

    fn close_picker(&mut self, cx: &mut Context<Self>) {
        self.picker = None;
        cx.notify();
    }

    fn picker_open_dir(&mut self, entry_index: usize, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let Some(entry) = picker.entries.get(entry_index) else {
            return;
        };
        let target_path = join_remote_path(&picker.path, &entry.name);

        picker.pending_path = Some(target_path.clone());
        picker.error = None;
        self.send_command(Command::ListDir { path: target_path });
        cx.notify();
    }

    fn picker_go_up(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let target_path = parent_remote_path(&picker.path);

        picker.pending_path = Some(target_path.clone());
        picker.error = None;
        self.send_command(Command::ListDir { path: target_path });
        cx.notify();
    }

    /// "Add" — validate the displayed directory; the store write happens on
    /// `WorkspaceValidated` success.
    fn picker_confirm(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        picker.validating = true;
        picker.error = None;
        let path = picker.path.clone();

        self.send_command(Command::ValidateWorkspace { path });
        cx.notify();
    }

    /// A dead bridge also emits `Disconnected`, which drives the visible
    /// state — send failures need no extra handling here.
    fn send_command(&self, command: Command) {
        let _ = self.commands.send(command);
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .bg(rgb(theme::PANEL_BG))
            .border_r_1()
            .border_color(rgb(theme::BORDER))
            .child(ui::section_header("machine"))
            .child(
                div()
                    .px_3()
                    .pb_2()
                    .flex()
                    .flex_col()
                    .child(div().child(self.dest.clone()))
                    .child(self.render_link_line()),
            )
            .child(self.render_workspaces_header(cx))
            .child(self.render_workspace_list())
            .child(ui::section_header("sessions"))
            .child(
                div()
                    .id("sessions")
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_y_scroll()
                    .flex()
                    .flex_col()
                    .children(
                        self.sessions
                            .iter()
                            .enumerate()
                            .map(|(index, session)| self.render_session_row(index, session, cx)),
                    )
                    .when(self.sessions.is_empty(), |sidebar| {
                        sidebar.child(
                            div()
                                .px_3()
                                .py_1()
                                .text_xs()
                                .text_color(rgb(theme::TEXT_FAINT))
                                .child("no sessions yet"),
                        )
                    }),
            )
    }

    /// "WORKSPACES" with a trailing "+" (SPEC §Flows "Add workspace");
    /// the button waits for `Connected` — the picker needs the remote home.
    fn render_workspaces_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_add = self.remote_home.is_some() && self.picker.is_none();

        div()
            .flex()
            .items_center()
            .pr(px(6.))
            .child(ui::section_header("workspaces"))
            .child(div().flex_1())
            .child(
                ui::button("add-workspace", "+", ui::ButtonKind::Neutral, can_add)
                    // Offset the header's pt_3/pb_1 asymmetry so the button
                    // centers on the label text, not the padded box.
                    .mt(px(8.))
                    .when(can_add, |button| {
                        button
                            .on_click(cx.listener(|workbench, _, _, cx| workbench.open_picker(cx)))
                    }),
            )
    }

    fn render_workspace_list(&self) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .children(self.workspaces.iter().map(|workspace| {
                ui::workspace_row(
                    folder_name(&workspace.path).to_string().into(),
                    workspace.machine.clone().into(),
                )
            }))
            .when(self.workspaces.is_empty(), |list| {
                list.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_FAINT))
                        .child("no workspaces yet"),
                )
            })
    }

    fn render_link_line(&self) -> impl IntoElement {
        let (text, color) = match &self.link {
            Link::Connecting => ("connecting…".to_string(), theme::TEXT_FAINT),
            Link::Connected { daemon_version } => {
                (format!("harvesterd {daemon_version}"), theme::TEXT_FAINT)
            }
            Link::Failed { .. } => ("connect failed".to_string(), theme::STATUS_ERROR),
            Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
        };

        div().text_xs().text_color(rgb(color)).child(text)
    }

    fn render_session_row(
        &self,
        index: usize,
        session: &SessionInfo,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let selected = self
            .chat
            .as_ref()
            .is_some_and(|chat| chat.read(cx).sid() == session.sid);
        let opening = self.opening.as_deref() == Some(session.sid.as_str());
        let dot_color = match session.status {
            SessionStatus::Running => theme::STATUS_RUNNING,
            SessionStatus::Exited => theme::TEXT_FAINT,
        };
        let title: SharedString = match &session.name {
            Some(name) => name.clone().into(),
            None => sid_preview(&session.sid).into(),
        };
        let workspace_name: SharedString = folder_name(&session.cwd).to_string().into();

        ui::session_row(
            ("session", index),
            title,
            workspace_name,
            dot_color,
            selected,
            opening,
        )
        .on_click(cx.listener(move |workbench, _, _, cx| workbench.open_session(index, cx)))
    }

    fn render_content(&self) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        if let Some(chat) = &self.chat {
            return content.child(chat.clone());
        }

        let (hint, color) = match &self.link {
            Link::Connecting => (format!("connecting to {}…", self.dest), theme::TEXT_FAINT),
            Link::Connected { .. } => ("select a session".to_string(), theme::TEXT_FAINT),
            Link::Failed { detail } => (detail.clone(), theme::STATUS_ERROR),
            Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
        };

        content.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(rgb(color))
                .child(hint),
        )
    }

    fn render_picker(&self, picker: &FolderPicker, cx: &mut Context<Self>) -> impl IntoElement {
        // Directories before files, both in ls's alphabetical order.
        let directory_rows: Vec<_> = picker
            .entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry.is_dir)
            .map(|(entry_index, entry)| {
                ui::picker_dir_row(("picker-dir", entry_index), entry.name.clone().into()).on_click(
                    cx.listener(move |workbench, _, _, cx| {
                        workbench.picker_open_dir(entry_index, cx)
                    }),
                )
            })
            .collect();
        let file_rows: Vec<_> = picker
            .entries
            .iter()
            .filter(|entry| !entry.is_dir)
            .map(|entry| ui::picker_file_row(entry.name.clone().into()))
            .collect();
        let can_submit = !picker.busy();

        ui::modal_scrim("picker-scrim")
            .on_click(cx.listener(|workbench, _, _, cx| workbench.close_picker(cx)))
            .child(
                ui::modal_frame("Add workspace")
                    // Keeps clicks inside the dialog from reaching the
                    // scrim's dismiss handler.
                    .occlude()
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(theme::TEXT_DIM))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(picker.path.clone()),
                    )
                    .child(
                        div()
                            .id("picker-entries")
                            .h(px(PICKER_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .when(picker.path != "/", |list| {
                                list.child(ui::picker_dir_row("picker-up", "..".into()).on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.picker_go_up(cx)),
                                ))
                            })
                            .children(directory_rows)
                            .children(file_rows)
                            .when(picker.pending_path.is_some(), |list| {
                                list.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_FAINT))
                                        .child("loading…"),
                                )
                            }),
                    )
                    .children(picker.error.clone().map(|error| {
                        div()
                            .text_xs()
                            .text_color(rgb(theme::STATUS_ERROR))
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(error)
                    }))
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                ui::button(
                                    "picker-cancel",
                                    "Cancel",
                                    ui::ButtonKind::Neutral,
                                    true,
                                )
                                .on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.close_picker(cx)),
                                ),
                            )
                            .child(
                                ui::button(
                                    "picker-add",
                                    "Add",
                                    ui::ButtonKind::Primary,
                                    can_submit,
                                )
                                .when(can_submit, |button| {
                                    button.on_click(cx.listener(|workbench, _, _, cx| {
                                        workbench.picker_confirm(cx)
                                    }))
                                }),
                            ),
                    ),
            )
    }

    fn render_statusbar(&self) -> impl IntoElement {
        let link_text = match &self.link {
            Link::Connecting => "connecting…".to_string(),
            Link::Connected { daemon_version } => {
                format!("{} · harvesterd {daemon_version}", self.dest)
            }
            Link::Failed { .. } => "connect failed".to_string(),
            Link::Lost => "connection lost".to_string(),
        };
        let link_color = match &self.link {
            Link::Failed { .. } | Link::Lost => theme::STATUS_ERROR,
            _ => theme::TEXT_FAINT,
        };

        ui::statusbar_frame()
            .child(div().text_color(rgb(link_color)).child(link_text))
            .child(div().flex_1())
            .children(self.last_error.clone().map(|error| {
                div()
                    .text_color(rgb(theme::STATUS_ERROR))
                    .overflow_hidden()
                    .text_ellipsis()
                    .child(error)
            }))
    }
}

impl Render for Workbench {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(theme::WINDOW_BG))
            .text_color(rgb(theme::TEXT))
            .text_size(px(13.))
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .child(self.render_sidebar(cx))
                    .child(self.render_content()),
            )
            .child(self.render_statusbar())
            // Painted last so the scrim covers everything above.
            .children(
                self.picker
                    .as_ref()
                    .map(|picker| self.render_picker(picker, cx)),
            )
    }
}

fn sid_preview(sid: &str) -> String {
    sid.chars().take(SID_PREVIEW_CHARS).collect()
}

/// Last path component; whole path if there is no separator.
fn folder_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Appends one entry name; `path` is always absolute (the walk starts at the
/// remote `$HOME`), so the only special case is the root's trailing slash.
fn join_remote_path(path: &str, name: &str) -> String {
    format!("{}/{name}", path.trim_end_matches('/'))
}

/// One step up; the root is its own parent.
fn parent_remote_path(path: &str) -> String {
    match path.trim_end_matches('/').rfind('/') {
        Some(0) | None => "/".to_string(),
        Some(separator_index) => path[..separator_index].to_string(),
    }
}
