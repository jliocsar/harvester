//! Top-level UI state and layout (docs/SPEC.md §UI): sidebar (machine +
//! sessions), content (chat or empty state), statusbar. Owns the bridges —
//! one per machine, created lazily on first touch and kept for the window's
//! lifetime: commands out synchronously to the right machine's sender,
//! events in on one gpui task awaiting the shared machine-tagged channel.

use std::collections::HashMap;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, SharedString, Subscription, Window,
};
use harvester_client_core::{MachineKey, RemoteDirEntry, SshTarget};
use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::bridge::{self, BridgeTarget, Command, Event as BridgeEvent};
use crate::chat::Chat;
use crate::input::{PromptSubmitted, TextInput};
use crate::machines::{self, MachineRow};
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme;
use crate::ui;
use crate::workspaces::{self, Workspace};

const SIDEBAR_WIDTH: f32 = 240.;
/// The hardcoded local machine's key (SPEC §Machine): not a provider
/// machine, never deletable — harvesterd runs as a detached child, no SSH.
const LOCAL_MACHINE: &str = "local";
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

/// One machine's live bridge plus what its events reported so far.
struct MachineConnection {
    commands: UnboundedSender<Command>,
    link: Link,
    /// Remote `$HOME`, from `Connected` — the folder picker's start directory.
    remote_home: Option<String>,
}

/// One sidebar session row, tagged with the machine whose daemon reported it
/// (commands about it must route back to that machine's bridge).
struct MachineSession {
    machine: String,
    info: SessionInfo,
}

/// The add-workspace modal's state (SPEC §Flows "Add workspace").
struct FolderPicker {
    /// The machine being browsed — where listings and validation run.
    machine: String,
    /// Directory whose entries are displayed — what "Add" would store.
    path: String,
    entries: Vec<RemoteDirEntry>,
    /// A listing is in flight for this path; also the staleness guard —
    /// only the response matching it is applied.
    pending_path: Option<String>,
    /// The machine's bridge is still bootstrapping; browsing starts when its
    /// `Connected` delivers the remote `$HOME`.
    waiting_for_home: bool,
    validating: bool,
    /// Listing or validation failure, rendered inside the modal.
    error: Option<String>,
}

impl FolderPicker {
    /// While busy, rows and the Add button ignore clicks — one in-flight
    /// request at a time keeps responses unambiguous.
    fn busy(&self) -> bool {
        self.pending_path.is_some() || self.waiting_for_home || self.validating
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
    /// The prompt box's machine picker is open (only offered while ≥2 stored
    /// workspaces share this path — the control lists those machines).
    machine_picker_open: bool,
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
    /// The machine given on argv, if any — an ad-hoc SSH destination shown
    /// in place of the machines list (which arrives next stage); connected
    /// eagerly to preserve the argv workflow.
    argv_machine: Option<String>,
    /// The embedded musl harvesterd, shared by every bridge's bootstrap.
    daemon_binary: Arc<Vec<u8>>,
    /// The shared machine-tagged event channel; cloned into each bridge.
    event_sender: UnboundedSender<(String, BridgeEvent)>,
    /// Live bridges, one per machine (lazy — see `ensure_connection`).
    connections: HashMap<String, MachineConnection>,
    /// Provider machines, from the machines worker's last listing cycle.
    machines: Vec<MachineRow>,
    /// Probe verdicts by machine key; absent = not probed yet.
    probes: HashMap<String, bool>,
    /// The machines worker's command sender; `None` if its thread failed to
    /// start (the machines list then stays empty — statusbar has the reason).
    machine_commands: Option<UnboundedSender<machines::Command>>,
    /// All connected machines' sessions, newest first.
    sessions: Vec<MachineSession>,
    /// User's workspace list (`~/.harvester/workspaces.json`), all machines.
    workspaces: Vec<Workspace>,
    picker: Option<FolderPicker>,
    /// The add-workspace flow's first step when several machines qualify:
    /// "which machine?" — the folder picker follows the choice.
    machine_picker_open: bool,
    ghost: Option<GhostChat>,
    /// Sid we asked a bridge to open, until `Attached` promotes it to a
    /// chat (also the "don't double-open" guard).
    opening: Option<String>,
    chat: Option<Entity<Chat>>,
    /// Last request failure; sticks in the statusbar until the next one.
    last_error: Option<String>,
}

impl Workbench {
    pub fn new(
        argv_machine: Option<String>,
        daemon_binary: Vec<u8>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (event_sender, mut events) = unbounded_channel::<(String, BridgeEvent)>();

        cx.spawn(async move |this, cx| {
            while let Some((machine, event)) = events.recv().await {
                if this
                    .update(cx, |workbench, cx| {
                        workbench.handle_bridge_event(&machine, event, cx)
                    })
                    .is_err()
                {
                    // Workbench dropped — window closed; stop pumping.
                    break;
                }
            }
        })
        .detach();

        let machine_commands = Self::start_machines_worker(cx);
        let mut workbench = Self {
            argv_machine: argv_machine.clone(),
            daemon_binary: Arc::new(daemon_binary),
            event_sender,
            connections: HashMap::new(),
            machines: Vec::new(),
            probes: HashMap::new(),
            machine_commands,
            sessions: Vec::new(),
            workspaces: workspaces::load(),
            picker: None,
            machine_picker_open: false,
            ghost: None,
            opening: None,
            chat: None,
            last_error: None,
        };

        workbench.publish_ad_hoc_machines();

        if let Some(machine) = argv_machine {
            workbench.ensure_connection(&machine);
        }

        // SPEC §Flows "Reopen client": machines with stored workspaces
        // connect in the background so their sessions reappear unprompted.
        // Ad-hoc ones connect here; provider-key ones as soon as the worker's
        // first listing makes them resolvable.
        workbench.auto_connect_workspace_machines();

        workbench
    }

    /// Connects every not-yet-touched workspace machine that can resolve to
    /// a target. Quiet by design: unresolvable machines (provider not listed
    /// yet, argv ssh form) are skipped without an error — the next listing
    /// cycle retries them.
    fn auto_connect_workspace_machines(&mut self) {
        let mut machines: Vec<String> = self
            .workspaces
            .iter()
            .map(|workspace| workspace.machine.clone())
            .collect();

        machines.sort();
        machines.dedup();

        for machine in machines {
            let untouched = !self.connections.contains_key(&machine);

            if untouched && self.resolve_target(&machine).is_some() {
                self.ensure_connection(&machine);
            }
        }
    }

    /// Spawns the machines worker thread and its event pump; `None` (with a
    /// stderr note) if the thread cannot start.
    fn start_machines_worker(cx: &mut Context<Self>) -> Option<UnboundedSender<machines::Command>> {
        let worker = match machines::start() {
            Ok(worker) => worker,
            Err(error) => {
                eprintln!("harvester: start machines worker: {error}");

                return None;
            }
        };
        let machines::MachinesWorker {
            commands,
            mut events,
        } = worker;

        cx.spawn(async move |this, cx| {
            while let Some(event) = events.recv().await {
                if this
                    .update(cx, |workbench, cx| {
                        workbench.handle_machines_event(event, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();

        Some(commands)
    }

    /// Pure router for machines-worker events.
    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<Self>) {
        match event {
            machines::Event::Listed(rows) => self.handle_machines_listed(rows),
            machines::Event::ProviderFailed { provider, detail } => {
                self.last_error = Some(format!("provider {provider}: {detail}"))
            }
            machines::Event::Probed { key, reachable } => {
                self.probes.insert(key, reachable);
            }
        }

        cx.notify();
    }

    fn handle_machines_listed(&mut self, mut rows: Vec<MachineRow>) {
        rows.sort_by(|a, b| a.key.cmp(&b.key));
        self.machines = rows;
        // Provider-key workspace machines become resolvable with the first
        // listing — connect them so their sessions appear.
        self.auto_connect_workspace_machines();
    }

    /// The ad-hoc destinations the worker should probe alongside provider
    /// machines: the argv machine plus legacy (keyless) workspace machines.
    /// `local` is excluded — it has its own row and no SSH to probe.
    fn ad_hoc_machines(&self) -> Vec<String> {
        let mut machines: Vec<String> = self
            .argv_machine
            .iter()
            .cloned()
            .chain(self.workspaces.iter().filter_map(|workspace| {
                match MachineKey::parse(&workspace.machine) {
                    Some(_) => None,
                    None => Some(workspace.machine.clone()),
                }
            }))
            .filter(|machine| machine != LOCAL_MACHINE)
            .collect();

        machines.sort();
        machines.dedup();

        machines
    }

    fn publish_ad_hoc_machines(&self) {
        if let Some(commands) = &self.machine_commands {
            let _ = commands.send(machines::Command::SetAdHocMachines(self.ad_hoc_machines()));
        }
    }

    /// Lazily connects: reuses a live bridge, respawns a failed or lost one
    /// (retouching a machine is the retry), spawns a fresh one otherwise.
    /// `false` when the machine can't be connected — the reason lands in
    /// `last_error`.
    fn ensure_connection(&mut self, machine: &str) -> bool {
        let usable = match self.connections.get(machine) {
            Some(connection) => !matches!(connection.link, Link::Failed { .. } | Link::Lost),
            None => false,
        };

        if usable {
            return true;
        }

        let Some(target) = self.resolve_target(machine) else {
            self.last_error = Some(format!("machine {machine} is not connectable"));

            return false;
        };

        match bridge::start(
            machine.to_string(),
            target,
            self.daemon_binary.clone(),
            self.event_sender.clone(),
        ) {
            Ok(handle) => {
                self.connections.insert(
                    machine.to_string(),
                    MachineConnection {
                        commands: handle.commands,
                        link: Link::Connecting,
                        remote_home: None,
                    },
                );

                true
            }
            Err(error) => {
                self.last_error = Some(format!("start bridge for {machine}: {error}"));

                false
            }
        }
    }

    /// Pure router (house rule: every non-trivial match arm delegates to a
    /// dedicated handler; only one-line assignments stay inline). `machine`
    /// is the bridge the event came from; sid-scoped events route by sid
    /// alone — sids are uuids, unique across machines.
    fn handle_bridge_event(&mut self, machine: &str, event: BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::Connected {
                daemon_version,
                remote_home,
            } => self.handle_connected(machine, daemon_version, remote_home),
            BridgeEvent::ConnectFailed { detail } => self.handle_connect_failed(machine, detail),
            BridgeEvent::Sessions(sessions) => self.handle_sessions_listed(machine, sessions),
            BridgeEvent::Attached { sid, entries } => {
                self.handle_attached(machine, sid, entries, cx)
            }
            BridgeEvent::Pi { sid, event } => self.handle_pi_event(sid, event, cx),
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.handle_session_exit(sid, exit_code, cx)
            }
            BridgeEvent::RequestFailed { detail } => self.handle_request_failed(machine, detail),
            BridgeEvent::ModelsListed { cwd, result } => {
                self.handle_models_listed(machine, cwd, result)
            }
            BridgeEvent::SessionModelsListed { sid, result } => {
                self.handle_session_models_listed(sid, result, cx)
            }
            BridgeEvent::SessionState { sid, result } => self.handle_session_state(sid, result, cx),
            BridgeEvent::ModelSet { sid, result } => self.handle_model_set(sid, result, cx),
            BridgeEvent::DirListed { path, result } => {
                self.handle_dir_listed(machine, path, result)
            }
            BridgeEvent::WorkspaceValidated { path, error } => {
                self.handle_workspace_validated(machine, path, error)
            }
            BridgeEvent::Disconnected => self.handle_disconnected(machine),
        }

        cx.notify();
    }

    fn handle_connected(&mut self, machine: &str, daemon_version: String, remote_home: String) {
        if let Some(connection) = self.connections.get_mut(machine) {
            connection.link = Link::Connected { daemon_version };
            connection.remote_home = Some(remote_home.clone());
        }

        self.start_waiting_picker(machine, remote_home);
    }

    /// A folder picker opened on a machine that was still bootstrapping
    /// starts browsing the moment the connection delivers the remote home.
    fn start_waiting_picker(&mut self, machine: &str, remote_home: String) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if !picker.waiting_for_home || picker.machine != machine {
            return;
        }

        picker.waiting_for_home = false;
        picker.path = remote_home.clone();
        picker.pending_path = Some(remote_home.clone());
        self.send_command(machine, Command::ListDir { path: remote_home });
    }

    fn handle_connect_failed(&mut self, machine: &str, detail: String) {
        self.abort_machine_flows(machine, &detail);

        if let Some(connection) = self.connections.get_mut(machine) {
            connection.link = Link::Failed { detail };
        }
    }

    fn handle_disconnected(&mut self, machine: &str) {
        self.abort_machine_flows(machine, "connection lost");

        if let Some(connection) = self.connections.get_mut(machine) {
            connection.link = Link::Lost;
        }
    }

    /// Commands queued for this machine died with its bridge: return any
    /// ghost start on it to editable, fail its pending model fetch, and
    /// clear the opening guard (over-broad for other machines' opens, but
    /// harmless — a re-click retries).
    fn abort_machine_flows(&mut self, machine: &str, detail: &str) {
        self.opening = None;

        if let Some(ghost) = &mut self.ghost {
            if ghost.workspace.machine == machine {
                ghost.started = None;

                if matches!(ghost.models, ModelsState::Loading) {
                    ghost.models = ModelsState::Failed(detail.to_string());
                }
            }
        }
    }

    fn handle_sessions_listed(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
        self.sessions.retain(|session| session.machine != machine);
        self.sessions
            .extend(sessions.into_iter().map(|info| MachineSession {
                machine: machine.to_string(),
                info,
            }));
        // Newest first across machines; uuidv7 sids sort chronologically, so
        // the sid itself is the created-at key.
        self.sessions.sort_by(|a, b| b.info.sid.cmp(&a.info.sid));
    }

    fn handle_attached(
        &mut self,
        machine: &str,
        sid: String,
        entries: Vec<Value>,
        cx: &mut Context<Self>,
    ) {
        // The daemon's list may predate a resume we just did; disk on the VM
        // stays authoritative, this is display state only.
        self.set_session_status(&sid, SessionStatus::Running);
        self.opening = None;

        let first_prompt = self.take_started_ghost(&sid);
        let Some(connection) = self.connections.get(machine) else {
            return;
        };
        let commands = connection.commands.clone();
        let machine_label: SharedString = self.machine_label(machine).into();

        self.chat =
            Some(cx.new(|cx| Chat::new(sid, machine_label, &entries, first_prompt, commands, cx)));
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

    fn handle_request_failed(&mut self, machine: &str, detail: String) {
        self.opening = None;

        // A failed ghost start returns the ghost to editable so the user can
        // retry (a fresh sid is minted on the next submit).
        if let Some(ghost) = &mut self.ghost {
            ghost.started = None;
        }

        self.last_error = Some(format!("{machine}: {detail}"));
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

    /// How many distinct machines the session list spans.
    fn session_machine_count(&self) -> usize {
        let mut machines: Vec<&str> = self
            .sessions
            .iter()
            .map(|session| session.machine.as_str())
            .collect();

        machines.sort();
        machines.dedup();

        machines.len()
    }

    fn set_session_status(&mut self, sid: &str, status: SessionStatus) {
        for session in &mut self.sessions {
            if session.info.sid == sid {
                session.info.status = status;
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
        let machine = session.machine.clone();
        let sid = session.info.sid.clone();
        let cwd = session.info.cwd.clone();
        let running = session.info.status == SessionStatus::Running;

        // Navigating to a session abandons any ghost chat — deliberate move
        // away from something that never existed daemon-side.
        self.ghost = None;

        let already_open = self.chat_for(&sid, cx).is_some_and(|_| running);

        if already_open {
            cx.notify();

            return;
        }

        if !self.ensure_connection(&machine) {
            cx.notify();

            return;
        }

        self.opening = Some(sid.clone());
        self.send_command(&machine, Command::Open { sid, cwd, running });
        cx.notify();
    }

    /// A `list_dir` answered. A response that doesn't match the pending
    /// machine + path is stale (the picker was closed and reopened
    /// meanwhile) — dropped.
    fn handle_dir_listed(
        &mut self,
        machine: &str,
        path: String,
        result: Result<Vec<RemoteDirEntry>, String>,
    ) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.machine != machine || picker.pending_path.as_deref() != Some(path.as_str()) {
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

    fn handle_workspace_validated(&mut self, machine: &str, path: String, error: Option<String>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.machine != machine || !picker.validating || picker.path != path {
            return;
        }

        picker.validating = false;

        match error {
            // Not a git repo: clear error in the modal, user re-picks.
            Some(detail) => picker.error = Some(detail),
            None => {
                let machine = picker.machine.clone();

                self.picker = None;
                self.add_workspace(machine, path);
            }
        }
    }

    /// Machine string → bridge target. `local` is the hardcoded no-SSH
    /// machine; `<provider>/<id>` keys resolve through the worker's machine
    /// list; anything else is an ad-hoc ssh destination used verbatim (argv
    /// machines, legacy workspaces.json entries). `None` when the key is
    /// unknown or the machine uses the unsupported argv ssh form.
    fn resolve_target(&self, machine: &str) -> Option<BridgeTarget> {
        if machine == LOCAL_MACHINE {
            return Some(BridgeTarget::Local);
        }

        if MachineKey::parse(machine).is_some() {
            return self
                .machines
                .iter()
                .find(|row| row.key == machine)
                .and_then(|row| row.target.clone())
                .map(BridgeTarget::Ssh);
        }

        Some(BridgeTarget::Ssh(SshTarget {
            dest: machine.to_string(),
            port: None,
        }))
    }

    fn add_workspace(&mut self, machine: String, path: String) {
        let workspace = Workspace { machine, path };

        if self.workspaces.contains(&workspace) {
            return;
        }

        self.workspaces.push(workspace);
        // A new ad-hoc machine may have entered the probe set.
        self.publish_ad_hoc_machines();

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
            MachineSession {
                machine: ghost.workspace.machine,
                info: SessionInfo {
                    sid: started.sid,
                    name: Some(started.name),
                    cwd: ghost.workspace.path,
                    file: String::new(),
                    mtime_ms: 0,
                    status: SessionStatus::Running,
                },
            },
        );

        Some(started.first_prompt)
    }

    /// A `list_models` answered. A ghost for a different workspace than the
    /// one that asked (closed and reopened meanwhile) ignores it.
    fn handle_models_listed(&mut self, machine: &str, cwd: String, result: Result<Value, String>) {
        let Some(ghost) = &mut self.ghost else {
            return;
        };

        if ghost.workspace.machine != machine || ghost.workspace.path != cwd {
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
    /// The workspace's machine connects lazily here; commands queue in the
    /// bridge channel while the bootstrap runs, so no readiness gate.
    fn open_ghost(&mut self, workspace_index: usize, cx: &mut Context<Self>) {
        let Some(workspace) = self.workspaces.get(workspace_index).cloned() else {
            return;
        };

        if self.opening.is_some() {
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

        if !self.ensure_connection(&workspace.machine) {
            cx.notify();

            return;
        }

        let input = cx.new(|cx| TextInput::new("Message pi — Enter to send", cx));
        let input_subscription = cx.subscribe(&input, Self::handle_ghost_prompt);

        self.send_command(
            &workspace.machine,
            Command::ListModels {
                cwd: workspace.path.clone(),
            },
        );
        self.ghost = Some(GhostChat {
            workspace,
            models: ModelsState::Loading,
            selected_model: None,
            picker_open: false,
            machine_picker_open: false,
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
        let machine = ghost.workspace.machine.clone();
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
        self.send_command(&machine, command);
        cx.notify();
    }

    /// Machines the ghost could spawn on: the stored workspaces sharing its
    /// path, as (machine key, display label) pairs. The control is offered
    /// only when there are at least two.
    fn ghost_machine_candidates(&self, path: &str) -> Vec<(String, String)> {
        self.workspaces
            .iter()
            .filter(|workspace| workspace.path == path)
            .map(|workspace| {
                (
                    workspace.machine.clone(),
                    self.machine_label(&workspace.machine),
                )
            })
            .collect()
    }

    fn ghost_open_machine_picker(&mut self, cx: &mut Context<Self>) {
        let Some(ghost) = &self.ghost else {
            return;
        };

        if ghost.started.is_none()
            && self.ghost_machine_candidates(&ghost.workspace.path).len() >= 2
        {
            if let Some(ghost) = &mut self.ghost {
                ghost.machine_picker_open = true;
                cx.notify();
            }
        }
    }

    fn ghost_close_machine_picker(&mut self, cx: &mut Context<Self>) {
        if let Some(ghost) = &mut self.ghost {
            ghost.machine_picker_open = false;
            cx.notify();
        }
    }

    /// A machine was picked for the ghost: retarget the workspace, connect
    /// the new machine lazily, and refetch its model list (the stale
    /// `ModelsListed` from the old machine is dropped by the machine+cwd
    /// guard). The typed prompt survives — only the destination changes.
    fn ghost_pick_machine(&mut self, candidate_index: usize, cx: &mut Context<Self>) {
        let Some(ghost) = &self.ghost else {
            return;
        };
        let candidates = self.ghost_machine_candidates(&ghost.workspace.path);
        let Some((machine, _)) = candidates.get(candidate_index).cloned() else {
            return;
        };
        let same_machine = ghost.workspace.machine == machine;

        if let Some(ghost) = &mut self.ghost {
            ghost.machine_picker_open = false;
        }

        if same_machine {
            cx.notify();

            return;
        }

        if !self.ensure_connection(&machine) {
            cx.notify();

            return;
        }

        let Some(ghost) = &mut self.ghost else {
            return;
        };
        let cwd = ghost.workspace.path.clone();

        ghost.workspace.machine = machine.clone();
        ghost.models = ModelsState::Loading;
        ghost.selected_model = None;
        self.send_command(&machine, Command::ListModels { cwd });
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

    /// Machines a workspace can be added on (or a session spawned on): every
    /// connectable machine — provider machines with a resolved target plus
    /// the ad-hoc destinations. `local` joins when its daemon support lands.
    /// Returned as (machine key, display label) pairs.
    fn machine_candidates(&self) -> Vec<(String, String)> {
        let mut candidates: Vec<(String, String)> =
            vec![(LOCAL_MACHINE.to_string(), LOCAL_MACHINE.to_string())];

        candidates.extend(
            self.machines
                .iter()
                .filter(|row| row.target.is_some())
                .map(|row| (row.key.clone(), row.label.clone())),
        );

        for machine in self.ad_hoc_machines() {
            candidates.push((machine.clone(), machine));
        }

        candidates
    }

    /// The workspaces-header "+": one qualifying machine goes straight to
    /// browsing; several open the "which machine?" step first.
    fn open_add_workspace(&mut self, cx: &mut Context<Self>) {
        if self.picker.is_some() || self.machine_picker_open {
            return;
        }

        let candidates = self.machine_candidates();

        match candidates.len() {
            0 => self.last_error = Some("no machines available to add a workspace on".to_string()),
            1 => self.open_picker(candidates[0].0.clone()),
            _ => self.machine_picker_open = true,
        }

        cx.notify();
    }

    /// A machine row in the add-workspace machine picker was clicked.
    fn workspace_machine_picked(&mut self, candidate_index: usize, cx: &mut Context<Self>) {
        let candidates = self.machine_candidates();
        let Some((machine, _)) = candidates.get(candidate_index) else {
            return;
        };

        self.machine_picker_open = false;
        self.open_picker(machine.clone());
        cx.notify();
    }

    fn close_machine_picker(&mut self, cx: &mut Context<Self>) {
        self.machine_picker_open = false;
        cx.notify();
    }

    /// Opens the folder picker on `machine`, connecting it lazily: with a
    /// known remote home browsing starts immediately, otherwise the picker
    /// waits for the bridge's `Connected`.
    fn open_picker(&mut self, machine: String) {
        if !self.ensure_connection(&machine) {
            return;
        }

        let known_home = self
            .connections
            .get(&machine)
            .and_then(|connection| connection.remote_home.clone());

        match known_home {
            Some(home) => {
                self.picker = Some(FolderPicker {
                    machine: machine.clone(),
                    path: home.clone(),
                    entries: Vec::new(),
                    pending_path: Some(home.clone()),
                    waiting_for_home: false,
                    validating: false,
                    error: None,
                });
                self.send_command(&machine, Command::ListDir { path: home });
            }
            None => {
                self.picker = Some(FolderPicker {
                    machine,
                    path: String::new(),
                    entries: Vec::new(),
                    pending_path: None,
                    waiting_for_home: true,
                    validating: false,
                    error: None,
                });
            }
        }
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
        let machine = picker.machine.clone();
        let target_path = join_remote_path(&picker.path, &entry.name);

        picker.pending_path = Some(target_path.clone());
        picker.error = None;
        self.send_command(&machine, Command::ListDir { path: target_path });
        cx.notify();
    }

    fn picker_go_up(&mut self, cx: &mut Context<Self>) {
        let Some(picker) = &mut self.picker else {
            return;
        };

        if picker.busy() {
            return;
        }

        let machine = picker.machine.clone();
        let target_path = parent_remote_path(&picker.path);

        picker.pending_path = Some(target_path.clone());
        picker.error = None;
        self.send_command(&machine, Command::ListDir { path: target_path });
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
        let machine = picker.machine.clone();
        let path = picker.path.clone();

        self.send_command(&machine, Command::ValidateWorkspace { path });
        cx.notify();
    }

    /// Routes to the machine's bridge. A dead or missing bridge needs no
    /// handling here — `Disconnected`/`ConnectFailed` events drive the
    /// visible state.
    fn send_command(&self, machine: &str, command: Command) {
        if let Some(connection) = self.connections.get(machine) {
            let _ = connection.commands.send(command);
        }
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
            .child(ui::section_header("machines"))
            .child(self.render_machine_list())
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
    /// enabled once at least one machine qualifies.
    fn render_workspaces_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_add = !self.machine_candidates().is_empty()
            && self.picker.is_none()
            && !self.machine_picker_open;

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
                        button.on_click(
                            cx.listener(|workbench, _, _, cx| workbench.open_add_workspace(cx)),
                        )
                    }),
            )
    }

    /// Read-only rows; the per-row "+" is the one click target (ghost chat).
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
                            folder_name(&workspace.path).to_string().into(),
                            self.machine_label(&workspace.machine).into(),
                        )
                        .child(
                            ui::workspace_row_add(("workspace-add", index)).on_click(cx.listener(
                                move |workbench, _, _, cx| workbench.open_ghost(index, cx),
                            )),
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

    /// The machines section: the hardcoded `local` row first (SPEC §Machine —
    /// connection support lands with the local-daemon stage), then provider
    /// machines, then ad-hoc SSH destinations. All rows read-only.
    fn render_machine_list(&self) -> impl IntoElement {
        let provider_rows: Vec<_> = self
            .machines
            .iter()
            .map(|row| {
                ui::machine_row(
                    row.label.clone().into(),
                    self.machine_detail(&row.key, &row.provider).into(),
                    self.machine_dot(&row.key),
                )
            })
            .collect();
        let ad_hoc_rows: Vec<_> = self
            .ad_hoc_machines()
            .into_iter()
            .map(|machine| {
                ui::machine_row(
                    machine.clone().into(),
                    self.machine_detail(&machine, "ssh").into(),
                    self.machine_dot(&machine),
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            .child(ui::machine_row(
                LOCAL_MACHINE.into(),
                self.machine_detail(LOCAL_MACHINE, "this computer").into(),
                self.local_machine_dot(),
            ))
            .children(provider_rows)
            .children(ad_hoc_rows)
    }

    /// The local host is reachable by definition — only a failed or lost
    /// local daemon turns its dot red.
    fn local_machine_dot(&self) -> u32 {
        match self.connections.get(LOCAL_MACHINE).map(|c| &c.link) {
            Some(Link::Failed { .. }) | Some(Link::Lost) => theme::STATUS_ERROR,
            _ => theme::STATUS_RUNNING,
        }
    }

    /// The dot tells reachability: a live bridge is proof, otherwise the
    /// worker's `ssh <machine> true` verdict (never provider-reported
    /// status). Unknown until the first probe answers.
    fn machine_dot(&self, key: &str) -> u32 {
        let bridge_connected = self
            .connections
            .get(key)
            .is_some_and(|connection| matches!(connection.link, Link::Connected { .. }));

        if bridge_connected {
            return theme::STATUS_RUNNING;
        }

        match self.probes.get(key) {
            Some(true) => theme::STATUS_RUNNING,
            Some(false) => theme::STATUS_ERROR,
            None => theme::TEXT_FAINT,
        }
    }

    /// Display label for a stored machine string: the provider machine's
    /// label when listed, the key's id half as a fallback, the string itself
    /// for ad-hoc destinations.
    fn machine_label(&self, machine: &str) -> String {
        match MachineKey::parse(machine) {
            Some(key) => self
                .machines
                .iter()
                .find(|row| row.key == machine)
                .map(|row| row.label.clone())
                .unwrap_or(key.id),
            None => machine.to_string(),
        }
    }

    /// The row's detail line: where the machine comes from, plus the bridge
    /// state once one exists.
    fn machine_detail(&self, key: &str, source: &str) -> String {
        let link_state = self
            .connections
            .get(key)
            .map(|connection| match &connection.link {
                Link::Connecting => "connecting…".to_string(),
                Link::Connected { daemon_version } => format!("harvesterd {daemon_version}"),
                Link::Failed { .. } => "connect failed".to_string(),
                Link::Lost => "connection lost".to_string(),
            });

        match link_state {
            Some(state) => format!("{source} · {state}"),
            None => source.to_string(),
        }
    }

    fn render_session_row(
        &self,
        index: usize,
        session: &MachineSession,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let machine_label = self.machine_label(&session.machine);
        let session = &session.info;
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
        // The machine joins the subtitle only once sessions span several
        // machines — a single-machine list doesn't need the noise.
        let multi_machine = self.session_machine_count() > 1;
        let workspace_name: SharedString = if multi_machine {
            format!("{} · {machine_label}", folder_name(&session.cwd)).into()
        } else {
            folder_name(&session.cwd).to_string().into()
        };

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

        let argv_connection = self
            .argv_machine
            .as_ref()
            .map(|machine| (machine, self.connections.get(machine)));
        let (hint, color) = match argv_connection {
            Some((machine, Some(connection))) => match &connection.link {
                Link::Connecting => (format!("connecting to {machine}…"), theme::TEXT_FAINT),
                Link::Connected { .. } => ("select a session".to_string(), theme::TEXT_FAINT),
                Link::Failed { detail } => (detail.clone(), theme::STATUS_ERROR),
                Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
            },
            // No argv machine (or its bridge never started): the workspace
            // rows are the way in.
            _ => ("open a workspace to begin".to_string(), theme::TEXT_FAINT),
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
            self.machine_label(&ghost.workspace.machine)
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
            // The machine control is live only while ≥2 stored workspaces
            // share this path; otherwise it reads as the fixed destination.
            let machine_enabled = self.ghost_machine_candidates(&ghost.workspace.path).len() >= 2;
            let machine_label: SharedString = self.machine_label(&ghost.workspace.machine).into();

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
                            .child(
                                ui::prompt_box_control(
                                    "ghost-machine",
                                    machine_label,
                                    machine_enabled,
                                )
                                .when(machine_enabled, |control| {
                                    control.on_click(cx.listener(|workbench, _, _, cx| {
                                        workbench.ghost_open_machine_picker(cx)
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

    /// The add-workspace machine step: same modal-list pattern as the model
    /// picker; picking a machine opens the folder picker there.
    fn render_machine_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let candidate_rows: Vec<_> = self
            .machine_candidates()
            .into_iter()
            .enumerate()
            .map(|(index, (_, label))| {
                ui::model_row(("workspace-machine", index), label.into(), false).on_click(
                    cx.listener(move |workbench, _, _, cx| {
                        workbench.workspace_machine_picked(index, cx)
                    }),
                )
            })
            .collect();

        ui::modal_scrim("workspace-machine-scrim")
            .on_click(cx.listener(|workbench, _, _, cx| workbench.close_machine_picker(cx)))
            .child(
                ui::modal_frame("Add workspace — choose machine")
                    // Keeps clicks inside the dialog from reaching the
                    // scrim's dismiss handler.
                    .occlude()
                    .child(
                        div()
                            .id("workspace-machine-entries")
                            .max_h(px(MODAL_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(candidate_rows),
                    ),
            )
    }

    /// The ghost's prompt-box machine picker: the workspaces sharing its
    /// path, the current destination checked.
    fn render_ghost_machine_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (path, current_machine) = match &self.ghost {
            Some(ghost) => (
                ghost.workspace.path.clone(),
                ghost.workspace.machine.clone(),
            ),
            None => (String::new(), String::new()),
        };
        let candidate_rows: Vec<_> = self
            .ghost_machine_candidates(&path)
            .into_iter()
            .enumerate()
            .map(|(index, (machine, label))| {
                ui::model_row(
                    ("ghost-machine-choice", index),
                    label.into(),
                    machine == current_machine,
                )
                .on_click(
                    cx.listener(move |workbench, _, _, cx| workbench.ghost_pick_machine(index, cx)),
                )
            })
            .collect();

        ui::modal_scrim("ghost-machine-scrim")
            .on_click(cx.listener(|workbench, _, _, cx| workbench.ghost_close_machine_picker(cx)))
            .child(
                ui::modal_frame("Choose machine")
                    // Keeps clicks inside the dialog from reaching the
                    // scrim's dismiss handler.
                    .occlude()
                    .child(
                        div()
                            .id("ghost-machine-entries")
                            .max_h(px(MODAL_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .children(candidate_rows),
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
                            .child(format!(
                                "on {} — {}",
                                self.machine_label(&picker.machine),
                                picker.path
                            )),
                    )
                    .child(
                        div()
                            .id("picker-entries")
                            .h(px(MODAL_LIST_HEIGHT))
                            .overflow_y_scroll()
                            .flex()
                            .flex_col()
                            .when(!picker.waiting_for_home && picker.path != "/", |list| {
                                list.child(ui::picker_dir_row("picker-up", "..".into()).on_click(
                                    cx.listener(|workbench, _, _, cx| workbench.picker_go_up(cx)),
                                ))
                            })
                            .children(directory_rows)
                            .children(file_rows)
                            .when(picker.waiting_for_home, |list| {
                                list.child(
                                    div()
                                        .px_2()
                                        .py_1()
                                        .text_xs()
                                        .text_color(rgb(theme::TEXT_FAINT))
                                        .child("connecting…"),
                                )
                            })
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
        let (link_text, link_color) = self.statusbar_link();

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

    /// Transitional until the machines list (next stage): the argv machine's
    /// link when present, else a connected-machine count.
    fn statusbar_link(&self) -> (String, u32) {
        if let Some(machine) = &self.argv_machine {
            if let Some(connection) = self.connections.get(machine) {
                return match &connection.link {
                    Link::Connecting => ("connecting…".to_string(), theme::TEXT_FAINT),
                    Link::Connected { daemon_version } => (
                        format!("{machine} · harvesterd {daemon_version}"),
                        theme::TEXT_FAINT,
                    ),
                    Link::Failed { .. } => ("connect failed".to_string(), theme::STATUS_ERROR),
                    Link::Lost => ("connection lost".to_string(), theme::STATUS_ERROR),
                };
            }
        }

        let connected_count = self
            .connections
            .values()
            .filter(|connection| matches!(connection.link, Link::Connected { .. }))
            .count();

        (
            format!(
                "{connected_count} of {} machines connected",
                self.connections.len()
            ),
            theme::TEXT_FAINT,
        )
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
                self.machine_picker_open
                    .then(|| self.render_machine_picker(cx)),
            )
            .children(
                self.ghost
                    .as_ref()
                    .is_some_and(|ghost| ghost.machine_picker_open)
                    .then(|| self.render_ghost_machine_picker(cx)),
            )
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
