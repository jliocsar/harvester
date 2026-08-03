//! Top-level UI state and layout (docs/SPEC.md §UI, slice-4 cut): sidebar
//! (machine + sessions), content (chat or empty state), statusbar. Owns the
//! bridge: commands out synchronously, events in on a gpui task awaiting the
//! bridge's channel.

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, SharedString, Subscription, Window,
};
use harvester_client_core::RemoteDirEntry;
use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::bridge::{Bridge, Command, Event as BridgeEvent};
use crate::chat::Chat;
use crate::input::{PromptSubmitted, TextInput};
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme;
use crate::ui;
use crate::workspaces::{self, Workspace};

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// Fixed height for modal lists (folder picker directories, model picker),
/// so the dialogs don't resize with their contents.
const MODAL_LIST_HEIGHT: f32 = 280.;
/// Cap on the session name derived from the first prompt's first line —
/// enough to recognize, short enough for pi's session list and the sidebar.
const SESSION_NAME_MAX_CHARS: usize = 48;

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

/// Which surface the open model picker belongs to: the ghost chat (choice
/// rides the launch line) or the live chat (choice becomes a `set_model`).
#[derive(Clone, Copy)]
enum ModelPickerTarget {
    Ghost,
    Chat,
}

/// A chat that exists only client-side (SPEC §Flows "Start session (ghost
/// chat)"): opened from a workspace row, becomes a real session on the first
/// prompt. Dropped without a trace if navigated away from.
struct GhostChat {
    /// Where the session will run; the path is the pi cwd.
    workspace: Workspace,
    models: ModelsState,
    /// Index into the `Loaded` list; `None` = let pi pick its default.
    selected_model: Option<usize>,
    picker_open: bool,
    /// Set once the first prompt fired the create → attach chain; cleared by
    /// `Attached` (ghost becomes a chat) or `RequestFailed` (retryable).
    started: Option<StartedSession>,
    /// Focus the prompt input on the next frame (same trick as `Chat`).
    focus_input: bool,
    input: Entity<TextInput>,
    _input_subscription: Subscription,
}

/// What `Attached` needs to finish a ghost start: the sid to match on, the
/// name for the synthesized sidebar row, the prompt to seed the chat with.
struct StartedSession {
    sid: String,
    name: String,
    first_prompt: String,
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
    ghost: Option<GhostChat>,
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
            ghost: None,
            opening: None,
            chat: None,
            last_error: None,
            commands,
        }
    }

    /// Pure router (house rule: every non-trivial match arm delegates to a
    /// dedicated handler; only one-line assignments stay inline).
    fn handle_bridge_event(&mut self, event: BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::Connected {
                daemon_version,
                remote_home,
            } => self.handle_connected(daemon_version, remote_home),
            BridgeEvent::ConnectFailed { detail } => self.link = Link::Failed { detail },
            BridgeEvent::Sessions(sessions) => self.handle_sessions_listed(sessions),
            BridgeEvent::Attached { sid, entries } => self.handle_attached(sid, entries, cx),
            BridgeEvent::Pi { sid, event } => self.handle_pi_event(sid, event, cx),
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.handle_session_exit(sid, exit_code, cx)
            }
            BridgeEvent::RequestFailed { detail } => self.handle_request_failed(detail),
            BridgeEvent::ModelsListed { cwd, result } => self.handle_models_listed(cwd, result),
            BridgeEvent::SessionModelsListed { sid, result } => {
                self.handle_session_models_listed(sid, result, cx)
            }
            BridgeEvent::SessionState { sid, result } => self.handle_session_state(sid, result, cx),
            BridgeEvent::ModelSet { sid, result } => self.handle_model_set(sid, result, cx),
            BridgeEvent::DirListed { path, result } => self.handle_dir_listed(path, result),
            BridgeEvent::WorkspaceValidated { path, error } => {
                self.handle_workspace_validated(path, error)
            }
            BridgeEvent::Disconnected => self.link = Link::Lost,
        }

        cx.notify();
    }

    fn handle_connected(&mut self, daemon_version: String, remote_home: String) {
        self.link = Link::Connected { daemon_version };
        self.remote_home = Some(remote_home);
    }

    fn handle_sessions_listed(&mut self, mut sessions: Vec<SessionInfo>) {
        // Newest first; uuidv7 sids sort chronologically, so the sid itself
        // is the created-at key.
        sessions.sort_by(|a, b| b.sid.cmp(&a.sid));
        self.sessions = sessions;
    }

    fn handle_attached(&mut self, sid: String, entries: Vec<Value>, cx: &mut Context<Self>) {
        // The daemon's list may predate a resume we just did; disk on the VM
        // stays authoritative, this is display state only.
        self.set_session_status(&sid, SessionStatus::Running);
        self.opening = None;

        let first_prompt = self.take_started_ghost(&sid);
        let commands = self.commands.clone();

        self.chat = Some(cx.new(|cx| Chat::new(sid, &entries, first_prompt, commands, cx)));
    }

    fn handle_pi_event(&mut self, sid: String, event: Value, cx: &mut Context<Self>) {
        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_pi_event(&event, cx));
        }
    }

    fn handle_session_exit(&mut self, sid: String, exit_code: Option<i32>, cx: &mut Context<Self>) {
        self.set_session_status(&sid, SessionStatus::Exited);

        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_session_exit(exit_code, cx));
        }
    }

    fn handle_request_failed(&mut self, detail: String) {
        self.opening = None;

        // A failed ghost start returns the ghost to editable so the user can
        // retry (a fresh sid is minted on the next submit).
        if let Some(ghost) = &mut self.ghost {
            ghost.started = None;
        }

        self.last_error = Some(detail);
    }

    fn handle_session_models_listed(
        &mut self,
        sid: String,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_session_models(result, cx));
        }
    }

    fn handle_session_state(
        &mut self,
        sid: String,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_session_state(result, cx));
        }
    }

    fn handle_model_set(
        &mut self,
        sid: String,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(data) => {
                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_model_set(&data, cx));
                }
            }
            // The session keeps its previous model; statusbar material.
            Err(detail) => self.last_error = Some(format!("set_model: {detail}")),
        }
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

        // Navigating to a session abandons any ghost chat — deliberate move
        // away from something that never existed daemon-side.
        self.ghost = None;

        let already_open = self
            .chat_for(&session.sid, cx)
            .is_some_and(|_| session.status == SessionStatus::Running);

        if already_open {
            cx.notify();

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

    /// If this `Attached` completes a ghost start: drop the ghost, add the
    /// new session to the sidebar, hand back the first prompt for seeding
    /// the chat. `None` for plain opens/resumes.
    fn take_started_ghost(&mut self, sid: &str) -> Option<String> {
        let started_matches = self
            .ghost
            .as_ref()
            .and_then(|ghost| ghost.started.as_ref())
            .is_some_and(|started| started.sid == sid);

        if !started_matches {
            return None;
        }

        let ghost = self.ghost.take().expect("checked above");
        let started = ghost.started.expect("checked above");

        // Synthesized sidebar row, newest-first (uuidv7 now is the maximum).
        // `file`/`mtime_ms` are placeholders: display state only, replaced
        // by the next real `list_sessions` — disk on the VM stays truth.
        self.sessions.insert(
            0,
            SessionInfo {
                sid: started.sid,
                name: Some(started.name),
                cwd: ghost.workspace.path,
                file: String::new(),
                mtime_ms: 0,
                status: SessionStatus::Running,
            },
        );

        Some(started.first_prompt)
    }

    /// A `list_models` answered. A ghost for a different workspace than the
    /// one that asked (closed and reopened meanwhile) ignores it.
    fn handle_models_listed(&mut self, cwd: String, result: Result<Value, String>) {
        let Some(ghost) = &mut self.ghost else {
            return;
        };

        if ghost.workspace.path != cwd {
            return;
        }

        ghost.models = match result {
            Ok(data) => ModelsState::Loaded(models::extract_model_choices(&data)),
            Err(detail) => ModelsState::Failed(detail),
        };
    }

    /// A workspace row was clicked: open a ghost chat there (SPEC §Flows
    /// "Start session") and fetch the models behind it — the seconds the
    /// uncached `list_models` costs hide behind the user typing the prompt.
    fn open_ghost(&mut self, workspace_index: usize, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.get(workspace_index).cloned() else {
            return;
        };

        // Until the machines list exists, the machine is the argv destination
        // (SPEC §Flows) — rows for other machines can't start anything here.
        if workspace.machine != self.dest {
            self.last_error = Some(format!(
                "{} lives on {} — this window is connected to {}",
                folder_name(&workspace.path),
                workspace.machine,
                self.dest
            ));
            cx.notify();

            return;
        }

        if !matches!(self.link, Link::Connected { .. }) || self.opening.is_some() {
            return;
        }

        // Re-clicking the current ghost's row keeps it (and any typed text).
        let same_ghost = self
            .ghost
            .as_ref()
            .is_some_and(|ghost| ghost.workspace == workspace);

        if same_ghost {
            return;
        }

        let input = cx.new(|cx| TextInput::new("Message pi — Enter to send", cx));
        let input_subscription = cx.subscribe(&input, Self::handle_ghost_prompt);

        self.send_command(Command::ListModels {
            cwd: workspace.path.clone(),
        });
        self.ghost = Some(GhostChat {
            workspace,
            models: ModelsState::Loading,
            selected_model: None,
            picker_open: false,
            started: None,
            focus_input: true,
            input,
            _input_subscription: input_subscription,
        });
        cx.notify();
    }

    /// The ghost's first prompt: mint the sid, derive the name, fire the
    /// create → attach → prompt chain. The ghost stays visible (input
    /// replaced by "starting…") until `Attached` swaps it for the real chat.
    fn handle_ghost_prompt(
        &mut self,
        _input: Entity<TextInput>,
        event: &PromptSubmitted,
        cx: &mut Context<Self>,
    ) {
        if event.0.trim().is_empty() {
            return;
        }

        let Some(ghost) = &mut self.ghost else {
            return;
        };

        if ghost.started.is_some() {
            return;
        }

        let sid = uuid::Uuid::now_v7().to_string();
        let name = session_name_from_prompt(&event.0);
        let (provider, model) = match (&ghost.models, ghost.selected_model) {
            (ModelsState::Loaded(models), Some(index)) => match models.get(index) {
                Some(choice) => (Some(choice.provider.clone()), Some(choice.id.clone())),
                None => (None, None),
            },
            // No selection (or no list): pi launches with its own default.
            _ => (None, None),
        };
        let command = Command::StartSession {
            sid: sid.clone(),
            cwd: ghost.workspace.path.clone(),
            provider,
            model,
            name: name.clone(),
            first_prompt: event.0.clone(),
        };

        ghost.started = Some(StartedSession {
            sid: sid.clone(),
            name,
            first_prompt: event.0.clone(),
        });
        self.opening = Some(sid);
        self.send_command(command);
        cx.notify();
    }

    fn ghost_open_model_picker(&mut self, cx: &mut Context<Self>) {
        let Some(ghost) = &mut self.ghost else {
            return;
        };

        if matches!(ghost.models, ModelsState::Loaded(_)) && ghost.started.is_none() {
            ghost.picker_open = true;
            cx.notify();
        }
    }

    fn ghost_close_model_picker(&mut self, cx: &mut Context<Self>) {
        if let Some(ghost) = &mut self.ghost {
            ghost.picker_open = false;
            cx.notify();
        }
    }

    /// `None` selects pi's default model.
    fn ghost_select_model(&mut self, choice: Option<usize>, cx: &mut Context<Self>) {
        if let Some(ghost) = &mut self.ghost {
            ghost.selected_model = choice;
            ghost.picker_open = false;
            cx.notify();
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
            .child(self.render_workspace_list(cx))
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

    fn render_workspace_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .children(
                self.workspaces
                    .iter()
                    .enumerate()
                    .map(|(index, workspace)| {
                        ui::workspace_row(
                            ("workspace", index),
                            folder_name(&workspace.path).to_string().into(),
                            workspace.machine.clone().into(),
                        )
                        .on_click(
                            cx.listener(move |workbench, _, _, cx| workbench.open_ghost(index, cx)),
                        )
                    }),
            )
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

    fn render_content(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        // The ghost wins over an open chat: the user just asked for a new
        // session; the chat comes back when they navigate to it.
        if let Some(ghost) = &self.ghost {
            return content.child(self.render_ghost(ghost, window, cx));
        }

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

    /// The send button's path; Enter inside the input reaches the same
    /// `PromptSubmitted` through the `Submit` action.
    fn ghost_submit_prompt(&mut self, cx: &mut Context<Self>) {
        let Some(input) = self.ghost.as_ref().map(|ghost| ghost.input.clone()) else {
            return;
        };

        input.update(cx, |input, cx| input.submit_prompt(cx));
    }

    /// The ghost chat pane: header (title + folder), empty body, the prompt
    /// box (model control lives inside it) — or "starting…" once the first
    /// prompt fired.
    fn render_ghost(
        &self,
        ghost: &GhostChat,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let subtitle = format!(
            "{} on {}",
            folder_name(&ghost.workspace.path),
            ghost.workspace.machine
        );
        // The prompt box only renders before the start fires, so `Loaded`
        // alone decides whether the model control is clickable.
        let (model_label, model_enabled): (SharedString, bool) = match &ghost.models {
            ModelsState::Loading => ("loading models…".into(), false),
            ModelsState::Failed(_) => ("default model".into(), false),
            ModelsState::Loaded(models) => (
                match ghost.selected_model.and_then(|index| models.get(index)) {
                    Some(choice) => choice.name.clone(),
                    None => "default model".into(),
                },
                true,
            ),
        };
        let input_focused = ghost.input.read(cx).focus_handle(cx).is_focused(window);
        let bottom_row = div().flex_none().px_3().pb_3();
        let bottom_row = if ghost.started.is_some() {
            bottom_row.child(
                div()
                    .text_xs()
                    .text_color(rgb(theme::TEXT_FAINT))
                    .child("starting session…"),
            )
        } else {
            bottom_row.child(
                ui::prompt_box(input_focused)
                    .child(ui::prompt_box_input(ghost.input.clone()))
                    .child(
                        ui::prompt_box_controls()
                            .child(
                                ui::prompt_box_control("ghost-model", model_label, model_enabled)
                                    .when(model_enabled, |control| {
                                        control.on_click(cx.listener(|workbench, _, _, cx| {
                                            workbench.ghost_open_model_picker(cx)
                                        }))
                                    }),
                            )
                            .child(div().flex_1())
                            .child(ui::prompt_box_send("ghost-send").on_click(cx.listener(
                                |workbench, _, _, cx| workbench.ghost_submit_prompt(cx),
                            ))),
                    ),
            )
        };

        div()
            .size_full()
            .flex()
            .flex_col()
            .child(ui::chat_header("New session".into(), subtitle.into()))
            .child(
                div().flex_1().min_h(px(0.)).p_3().children(
                    // Model failure is informational, not blocking: pi still
                    // launches with its default.
                    match &ghost.models {
                        ModelsState::Failed(detail) => Some(
                            div()
                                .text_xs()
                                .text_color(rgb(theme::STATUS_ERROR))
                                .child(format!("{detail} — pi will use its default model")),
                        ),
                        _ => None,
                    },
                ),
            )
            .child(bottom_row)
    }

    /// A picker row was clicked (`None` = the ghost's "default" row): route
    /// to the surface the picker belongs to.
    fn model_picked(
        &mut self,
        target: ModelPickerTarget,
        choice: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        match target {
            ModelPickerTarget::Ghost => self.ghost_select_model(choice, cx),
            ModelPickerTarget::Chat => {
                let Some(index) = choice else {
                    return;
                };

                if let Some(chat) = &self.chat {
                    chat.update(cx, |chat, cx| chat.pick_model(index, cx));
                }
            }
        }
    }

    fn model_picker_closed(&mut self, target: ModelPickerTarget, cx: &mut Context<Self>) {
        match target {
            ModelPickerTarget::Ghost => self.ghost_close_model_picker(cx),
            ModelPickerTarget::Chat => {
                if let Some(chat) = &self.chat {
                    chat.update(cx, |chat, cx| chat.close_model_picker(cx));
                }
            }
        }
    }

    /// The model picker modal, painted over everything (same scrim/occlusion
    /// wiring as the folder picker). One render for both surfaces; only the
    /// ghost offers the "default (pi chooses)" row — a live session always
    /// has a concrete model.
    fn render_model_picker(
        &self,
        target: ModelPickerTarget,
        models: &[ModelChoice],
        selected_model: Option<usize>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let model_rows: Vec<_> = models
            .iter()
            .enumerate()
            .map(|(index, choice)| {
                ui::model_row(
                    ("model", index),
                    choice.name.clone(),
                    selected_model == Some(index),
                )
                .on_click(cx.listener(move |workbench, _, _, cx| {
                    workbench.model_picked(target, Some(index), cx)
                }))
            })
            .collect();
        let default_row =
            match target {
                ModelPickerTarget::Ghost => Some(
                    ui::model_row(
                        "model-default",
                        "default (pi chooses)".into(),
                        selected_model.is_none(),
                    )
                    .on_click(cx.listener(move |workbench, _, _, cx| {
                        workbench.model_picked(target, None, cx)
                    })),
                ),
                ModelPickerTarget::Chat => None,
            };

        ui::modal_scrim("model-scrim")
            .on_click(
                cx.listener(move |workbench, _, _, cx| workbench.model_picker_closed(target, cx)),
            )
            .child(
                ui::modal_frame("Choose model")
                    // Keeps clicks inside the dialog from reaching the
                    // scrim's dismiss handler.
                    .occlude()
                    .child(
                        div()
                            .id("model-entries")
                            .h(px(MODAL_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(default_row)
                            .children(model_rows),
                    ),
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
                            .h(px(MODAL_LIST_HEIGHT))
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
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(ghost) = &mut self.ghost {
            if ghost.focus_input {
                window.focus(&ghost.input.read(cx).focus_handle(cx));
                ghost.focus_input = false;
            }
        }

        // The model picker renders from whichever surface opened it (ghost
        // wins — it also wins the content pane). Data is cloned out here so
        // the entity borrows don't fight the `&mut cx` the listeners need.
        let model_picker: Option<(ModelPickerTarget, Vec<ModelChoice>, Option<usize>)> =
            if let Some(ghost) = self.ghost.as_ref().filter(|ghost| ghost.picker_open) {
                match &ghost.models {
                    ModelsState::Loaded(models) => Some((
                        ModelPickerTarget::Ghost,
                        models.clone(),
                        ghost.selected_model,
                    )),
                    _ => None,
                }
            } else if let Some(chat) = &self.chat {
                chat.read(cx)
                    .picker_contents()
                    .map(|(models, current_index)| (ModelPickerTarget::Chat, models, current_index))
            } else {
                None
            };

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
                    .child(self.render_content(window, cx)),
            )
            .child(self.render_statusbar())
            // Painted last so the scrims cover everything above.
            .children(
                self.picker
                    .as_ref()
                    .map(|picker| self.render_picker(picker, cx)),
            )
            .children(model_picker.map(|(target, models, selected_model)| {
                self.render_model_picker(target, &models, selected_model, cx)
            }))
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

/// Session name from the first prompt: its first non-blank line, capped.
fn session_name_from_prompt(prompt: &str) -> String {
    prompt
        .trim()
        .lines()
        .next()
        .unwrap_or("")
        .chars()
        .take(SESSION_NAME_MAX_CHARS)
        .collect()
}
