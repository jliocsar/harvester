//! Top-level UI state and layout (docs/SPEC.md §UI): sidebar (machine +
//! sessions), content (chat or empty state), statusbar.
//!
//! Everything that is *not* layout or user intent lives in client-core's
//! [`Fleet`], which owns the bridge pool, the machine table, the probe
//! verdicts and the connect policy. What is left here reads a machine's one
//! [`MachineState`] and renders it, routes commands by machine key, and keeps
//! the surfaces (ghost chat, chat, pickers) the user actually touches.

mod picker;

use gpui::{
    div, prelude::*, px, rgb, Context, Entity, Focusable, SharedString, Subscription, Window,
};
use harvester_client_core::{Fleet, MachineState, RequestToken, RequestTokens, LOCAL_MACHINE};
use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::chat::{BridgeLink, Chat};
use crate::input::{PromptSubmitted, TextInput};
use crate::models::{self, ModelChoice, ModelsState};
use crate::theme;
use crate::ui;
use crate::workspaces::{self, Workspace};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use picker::FolderPicker;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// Fixed height for modal lists (folder picker directories, model picker),
/// so the dialogs don't resize with their contents.
const MODAL_LIST_HEIGHT: f32 = 280.;
/// Cap on the session name derived from the first prompt's first line —
/// enough to recognize, short enough for pi's session list and the sidebar.
const SESSION_NAME_MAX_CHARS: usize = 48;
/// The empty content pane when there is no argv machine to report on — the
/// workspace rows are the way in.
const NO_ARGV_MACHINE_HINT: &str = "open a workspace to begin";

/// One sidebar session row, tagged with the machine whose daemon reported it
/// (commands about it must route back to that machine's bridge).
struct MachineSession {
    machine: String,
    info: SessionInfo,
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
    /// The `ListModels` this ghost is waiting on; an answer to any other
    /// token was asked for by a ghost (or a machine) it has moved on from.
    models_request: Option<RequestToken>,
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
    /// Every machine, its state and its bridge — all of it client-core's.
    fleet: Fleet,
    /// Mints the tokens that correlate every worker answer with the request
    /// that asked for it; cloned into each chat.
    tokens: RequestTokens,
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
    /// Collapsed sidebar sections (all expanded at launch).
    machines_collapsed: bool,
    workspaces_collapsed: bool,
    sessions_collapsed: bool,
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
        let (fleet, events) = Fleet::start(daemon_binary);

        Self::pump_bridge_events(events.bridges, cx);

        if let Some(machine_events) = events.machines {
            Self::pump_machines_events(machine_events, cx);
        }

        let mut workbench = Self {
            argv_machine: argv_machine.clone(),
            fleet,
            tokens: RequestTokens::default(),
            sessions: Vec::new(),
            workspaces: workspaces::load(),
            picker: None,
            machine_picker_open: false,
            ghost: None,
            opening: None,
            machines_collapsed: false,
            workspaces_collapsed: false,
            sessions_collapsed: false,
            chat: None,
            last_error: None,
        };

        // SPEC §Flows "Reopen client": machines with stored workspaces
        // connect in the background so their sessions reappear unprompted.
        // Ad-hoc ones connect on this call; provider-key ones as soon as the
        // worker's first listing makes them resolvable.
        workbench.track_machines();

        if let Some(machine) = argv_machine {
            workbench.ensure_connection(&machine);
        }

        workbench
    }

    /// The machines this client cares about: every stored workspace's machine
    /// plus the argv destination. What that implies — which get probed, which
    /// connect in the background — is the fleet's call, not the UI's. Called
    /// again whenever the workspace list changes.
    fn track_machines(&mut self) {
        let machines: Vec<String> = self
            .argv_machine
            .iter()
            .chain(self.workspaces.iter().map(|workspace| &workspace.machine))
            .cloned()
            .collect();

        self.fleet.track_machines(machines);
    }

    /// Every bridge's events on one machine-tagged channel, pumped by a gpui
    /// task — an mpsc receiver is reactor-independent, so awaiting it from
    /// gpui's executor is exactly what the split is designed for.
    fn pump_bridge_events(
        mut events: UnboundedReceiver<(String, BridgeEvent)>,
        cx: &mut Context<Self>,
    ) {
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
    }

    fn pump_machines_events(
        mut events: UnboundedReceiver<machines::Event>,
        cx: &mut Context<Self>,
    ) {
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
    }

    /// Pure router for machines-worker events. The fleet takes the machine
    /// table and the probe verdicts first; what is left is this window's.
    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<Self>) {
        self.fleet.record_machines_event(&event);

        match event {
            // The listing and the probes were the fleet's, and it took them.
            machines::Event::Listed(_) | machines::Event::Probed { .. } => {}
            machines::Event::ProviderFailed { provider, detail } => {
                self.last_error = Some(format!("provider {provider}: {detail}"))
            }
            machines::Event::HomeResolved { token, result } => {
                self.handle_home_resolved(token, result)
            }
            machines::Event::DirListed { token, result } => self.handle_dir_listed(token, result),
            machines::Event::WorkspaceValidated { token, error } => {
                self.handle_workspace_validated(token, error)
            }
        }

        cx.notify();
    }

    /// Routes a host op to the machines worker. `false` (with the reason in
    /// `last_error`) when the worker never started — without it there is no
    /// runtime to exec on, and the caller has nothing to wait for.
    fn send_host_command(&mut self, command: machines::Command) -> bool {
        match self.fleet.send_host_command(command) {
            Ok(()) => true,
            Err(error) => {
                self.last_error = Some(error.to_string());

                false
            }
        }
    }

    /// Lazily connects through the fleet; `false` when the machine can't be
    /// connected. The fleet's reason is carried into `last_error` verbatim —
    /// it is the only diagnosis the user gets for a machine that will not
    /// come up.
    fn ensure_connection(&mut self, machine: &str) -> bool {
        match self.fleet.ensure_connection(machine) {
            Ok(()) => true,
            Err(error) => {
                self.last_error = Some(error.to_string());

                false
            }
        }
    }

    /// Pure router (house rule: every non-trivial match arm delegates to a
    /// dedicated handler; only one-line assignments stay inline). `machine`
    /// is the bridge the event came from; sid-scoped events route by sid
    /// alone — sids are uuids, unique across machines. The fleet takes the
    /// link state off the same event first.
    fn handle_bridge_event(&mut self, machine: &str, event: BridgeEvent, cx: &mut Context<Self>) {
        self.fleet.record_bridge_event(machine, &event);

        match event {
            // The link state was the fleet's, and it took it; a successful
            // connect asks nothing else of this window.
            BridgeEvent::Connected { .. } => {}
            BridgeEvent::ConnectFailed { detail } => self.abort_machine_flows(machine, &detail),
            BridgeEvent::Sessions(sessions) => self.handle_sessions_listed(machine, sessions),
            BridgeEvent::Attached { sid, entries } => {
                self.handle_attached(machine, sid, entries, cx)
            }
            // TODO(gpui wave): a repair must REPLACE the existing chat's
            // entries, never rebuild the chat entity — rebuilding is what let a
            // late resync clobber the chat on screen. Routed to `handle_attached`
            // only to keep today's behaviour unchanged until that lands.
            BridgeEvent::HistoryReplaced { sid, entries } => {
                self.handle_attached(machine, sid, entries, cx)
            }
            BridgeEvent::Pi { sid, event } => self.handle_pi_event(sid, event, cx),
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.handle_session_exit(sid, exit_code, cx)
            }
            BridgeEvent::RequestFailed { detail } => self.handle_request_failed(machine, detail),
            BridgeEvent::ModelsListed { token, result } => self.handle_models_listed(token, result),
            BridgeEvent::SessionModelsListed { sid, token, result } => {
                self.handle_session_models_listed(sid, token, result, cx)
            }
            BridgeEvent::SessionState { sid, token, result } => {
                self.handle_session_state(sid, token, result, cx)
            }
            BridgeEvent::ModelSet { sid, token, result } => {
                self.handle_model_set(sid, token, result, cx)
            }
            BridgeEvent::Disconnected => self.abort_machine_flows(machine, "connection lost"),
        }

        cx.notify();
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
        let Some(commands) = self.fleet.commands_for(machine) else {
            return;
        };
        let bridge = BridgeLink {
            commands,
            tokens: self.tokens.clone(),
        };
        let machine_label: SharedString = self.fleet.label(machine).into();
        // The session is in the list by now (a ghost start just inserted its
        // row above); its cwd names the repository in the prompt box.
        let workspace_label: SharedString = self
            .sessions
            .iter()
            .find(|session| session.info.sid == sid)
            .map(|session| folder_name(&session.info.cwd).to_string())
            .unwrap_or_default()
            .into();

        self.chat = Some(cx.new(|cx| {
            Chat::new(
                sid,
                workspace_label,
                machine_label,
                &entries,
                first_prompt,
                bridge,
                cx,
            )
        }));
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
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_session_models(token, result, cx));
        }
    }

    fn handle_session_state(
        &mut self,
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        if let Some(chat) = self.chat_for(&sid, cx) {
            chat.update(cx, |chat, cx| chat.handle_session_state(token, result, cx));
        }
    }

    fn handle_model_set(
        &mut self,
        sid: String,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(data) => {
                if let Some(chat) = self.chat_for(&sid, cx) {
                    chat.update(cx, |chat, cx| chat.handle_model_set(token, &data, cx));
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

    /// Remote machines list everything — their disk is the only source of
    /// truth and there is nothing client-side to scope by. The local machine
    /// is different: the daemon would surface every repo this computer ever
    /// ran pi in, so its list is scoped to the added workspaces. The full
    /// list stays in memory (`list_sessions` runs once per connect); adding
    /// a workspace reveals its sessions without a refetch.
    fn session_visible(&self, session: &MachineSession) -> bool {
        if session.machine != LOCAL_MACHINE {
            return true;
        }

        self.workspaces.iter().any(|workspace| {
            workspace.machine == LOCAL_MACHINE && workspace.path == session.info.cwd
        })
    }

    fn set_session_status(&mut self, sid: &str, status: SessionStatus) {
        for session in &mut self.sessions {
            if session.info.sid == sid {
                session.info.status = status;
            }
        }
    }

    /// By sid, not index — the sidebar renders a filtered view of
    /// `self.sessions`, so display positions don't map back to the list.
    fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        // The guard is cleared by Attached/RequestFailed/ConnectFailed, none
        // of which arrive from a daemon that is live but silent — so clicking
        // the same row again is the escape hatch: it clears the guard and
        // reissues Open (create-or-resume + attach is idempotent).
        if let Some(opening) = self.opening.as_deref() {
            if opening != sid {
                return;
            }

            self.opening = None;
        }

        let Some(session) = self.sessions.iter().find(|session| session.info.sid == sid) else {
            return;
        };
        let machine = session.machine.clone();
        let sid = session.info.sid.clone();
        let cwd = session.info.cwd.clone();

        // Navigating to a session abandons any ghost chat — deliberate move
        // away from something that never existed daemon-side.
        self.ghost = None;

        // Only an open chat we have not seen exit can be re-shown as-is;
        // anything else goes through Open, which resumes if needed.
        let already_open = self
            .chat_for(&sid, cx)
            .is_some_and(|chat| chat.read(cx).is_live());

        if already_open {
            cx.notify();

            return;
        }

        if !self.ensure_connection(&machine) {
            cx.notify();

            return;
        }

        self.opening = Some(sid.clone());
        self.send_command(&machine, Command::Open { sid, cwd });
        cx.notify();
    }

    fn add_workspace(&mut self, machine: String, path: String) {
        let workspace = Workspace { machine, path };

        if self.workspaces.contains(&workspace) {
            return;
        }

        self.workspaces.push(workspace);
        // A new ad-hoc machine may have entered the probe set, and the picker
        // no longer connects the machine it browses (its execs need no
        // daemon) — so this is where a machine that just earned a workspace
        // gets its bridge, same rule as at launch.
        self.track_machines();

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

    /// A `list_models` answered. A ghost that has since been retargeted (or
    /// closed and reopened) is waiting on a different token, so the answer to
    /// the abandoned request is dropped.
    fn handle_models_listed(&mut self, token: RequestToken, result: Result<Value, String>) {
        let Some(ghost) = &mut self.ghost else {
            return;
        };

        if ghost.models_request != Some(token) {
            return;
        }

        ghost.models_request = None;
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
        let models_token = self.tokens.mint();

        self.send_command(
            &workspace.machine,
            Command::ListModels {
                cwd: workspace.path.clone(),
                token: models_token,
            },
        );
        self.ghost = Some(GhostChat {
            workspace,
            models: ModelsState::Loading,
            models_request: Some(models_token),
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
                    self.fleet.label(&workspace.machine),
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

        let models_token = self.tokens.mint();
        let Some(ghost) = &mut self.ghost else {
            return;
        };
        let cwd = ghost.workspace.path.clone();

        ghost.workspace.machine = machine.clone();
        ghost.models = ModelsState::Loading;
        ghost.models_request = Some(models_token);
        ghost.selected_model = None;
        self.send_command(
            &machine,
            Command::ListModels {
                cwd,
                token: models_token,
            },
        );
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

    /// The workspaces-header "+": one qualifying machine goes straight to
    /// browsing; several open the "which machine?" step first.
    fn open_add_workspace(&mut self, cx: &mut Context<Self>) {
        if self.picker.is_some() || self.machine_picker_open {
            return;
        }

        let candidates = self.fleet.machine_candidates();

        match candidates.len() {
            0 => self.last_error = Some("no machines available to add a workspace on".to_string()),
            1 => self.open_picker(candidates[0].key.clone()),
            _ => self.machine_picker_open = true,
        }

        cx.notify();
    }

    /// A machine row in the add-workspace machine picker was clicked.
    fn workspace_machine_picked(&mut self, candidate_index: usize, cx: &mut Context<Self>) {
        let candidates = self.fleet.machine_candidates();
        let Some(candidate) = candidates.get(candidate_index) else {
            return;
        };
        let machine = candidate.key.clone();

        self.machine_picker_open = false;
        self.open_picker(machine);
        cx.notify();
    }

    fn close_machine_picker(&mut self, cx: &mut Context<Self>) {
        self.machine_picker_open = false;
        cx.notify();
    }

    /// Routes to the machine's bridge. A dead or missing bridge needs no
    /// handling here — `Disconnected`/`ConnectFailed` events drive the
    /// visible state.
    fn send_command(&self, machine: &str, command: Command) {
        self.fleet.send(machine, command);
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
            .child(
                ui::section_header("machines-header", "machines")
                    .child(ui::section_chevron(!self.machines_collapsed))
                    .on_click(cx.listener(|workbench, _, _, cx| {
                        workbench.machines_collapsed = !workbench.machines_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.machines_collapsed, |sidebar| {
                sidebar.child(self.render_machine_list())
            })
            .child(self.render_workspaces_header(cx))
            .when(!self.workspaces_collapsed, |sidebar| {
                sidebar.child(self.render_workspace_list(cx))
            })
            .child(
                ui::section_header("sessions-header", "sessions")
                    .child(ui::section_chevron(!self.sessions_collapsed))
                    .on_click(cx.listener(|workbench, _, _, cx| {
                        workbench.sessions_collapsed = !workbench.sessions_collapsed;
                        cx.notify();
                    })),
            )
            .when(!self.sessions_collapsed, |sidebar| {
                sidebar.child(self.render_session_list(cx))
            })
    }

    /// The sessions the sidebar shows: `self.sessions` through the
    /// `session_visible` filter — which is why rows navigate by sid, never
    /// by position.
    fn render_session_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let visible: Vec<&MachineSession> = self
            .sessions
            .iter()
            .filter(|session| self.session_visible(session))
            .collect();
        let empty = visible.is_empty();

        div()
            .id("sessions")
            .flex_1()
            .min_h(px(0.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .children(
                visible
                    .into_iter()
                    .enumerate()
                    .map(|(index, session)| self.render_session_row(index, session, cx)),
            )
            .when(empty, |list| {
                list.child(
                    div()
                        .px_3()
                        .py_1()
                        .text_xs()
                        .text_color(rgb(theme::TEXT_FAINT))
                        .child("no sessions yet"),
                )
            })
    }

    /// "WORKSPACES" with a "+" before the chevron (SPEC §Flows "Add
    /// workspace"); the "+" enables once at least one machine qualifies and
    /// stops propagation so adding doesn't also collapse the section.
    fn render_workspaces_header(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let can_add = !self.fleet.machine_candidates().is_empty()
            && self.picker.is_none()
            && !self.machine_picker_open;

        ui::section_header("workspaces-header", "workspaces")
            .child(
                ui::button("add-workspace", "+", ui::ButtonKind::Neutral, can_add).when(
                    can_add,
                    |button| {
                        button.on_click(cx.listener(|workbench, _, _, cx| {
                            cx.stop_propagation();
                            workbench.open_add_workspace(cx)
                        }))
                    },
                ),
            )
            .child(ui::section_chevron(!self.workspaces_collapsed))
            .on_click(cx.listener(|workbench, _, _, cx| {
                workbench.workspaces_collapsed = !workbench.workspaces_collapsed;
                cx.notify();
            }))
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
                            self.fleet.label(&workspace.machine).into(),
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
            .fleet
            .provider_machines()
            .iter()
            .map(|row| {
                ui::machine_row(
                    row.label.clone().into(),
                    Some(row.provider.clone().into()),
                    self.machine_dot(&row.key),
                )
            })
            .collect();
        let ad_hoc_rows: Vec<_> = self
            .fleet
            .ad_hoc_machines()
            .iter()
            .map(|machine| {
                ui::machine_row(
                    machine.clone().into(),
                    Some("ssh".into()),
                    self.machine_dot(machine),
                )
            })
            .collect();

        div()
            .flex()
            .flex_col()
            // The local row has no subtitle — it comes from nowhere.
            .child(ui::machine_row(
                LOCAL_MACHINE.into(),
                None,
                self.machine_dot(LOCAL_MACHINE),
            ))
            .children(provider_rows)
            .children(ad_hoc_rows)
    }

    /// The dot is the machine's whole story now that rows carry no link
    /// text (SPEC §UI machine dots): green = daemon connected, yellow =
    /// connecting/bootstrapping, red = connect failed/lost or the worker's
    /// `ssh <machine> true` probe says unreachable, gray = no daemon link
    /// yet (reachable or simply not probed — indistinguishable without
    /// connecting). Never provider-reported status (SPEC §Machine).
    ///
    /// One `match` over one value: the precedence between a probe verdict and
    /// a daemon link used to live here, in a renderer, reading two maps. It
    /// is the fleet's invariant now.
    fn machine_dot(&self, key: &str) -> u32 {
        match self.fleet.state(key) {
            MachineState::Connected { .. } => theme::STATUS_RUNNING,
            MachineState::Connecting => theme::STATUS_PENDING,
            MachineState::ConnectFailed { .. }
            | MachineState::ConnectionLost
            | MachineState::Unreachable => theme::STATUS_ERROR,
            MachineState::Unknown | MachineState::Reachable => theme::TEXT_FAINT,
        }
    }

    fn render_session_row(
        &self,
        index: usize,
        session: &MachineSession,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let machine_label = self.fleet.label(&session.machine);
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
        let workspace_name: SharedString =
            format!("{} · {machine_label}", folder_name(&session.cwd)).into();
        let sid = session.sid.clone();

        ui::session_row(
            ("session", index),
            title,
            workspace_name,
            dot_color,
            selected,
            opening,
        )
        .on_click(cx.listener(move |workbench, _, _, cx| workbench.open_session(&sid, cx)))
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

        let (hint, color) = match &self.argv_machine {
            Some(machine) => self.argv_machine_hint(machine),
            // No argv machine: the workspace rows are the way in.
            None => (NO_ARGV_MACHINE_HINT.to_string(), theme::TEXT_FAINT),
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

    /// The empty content pane when the argv machine has a bridge; a machine
    /// that only ever got probed falls back to the workspace-rows hint,
    /// because there is nothing about its daemon to report yet.
    fn argv_machine_hint(&self, machine: &str) -> (String, u32) {
        match self.fleet.state(machine) {
            MachineState::Connecting => (format!("connecting to {machine}…"), theme::TEXT_FAINT),
            MachineState::Connected { .. } => ("select a session".to_string(), theme::TEXT_FAINT),
            MachineState::ConnectFailed { detail } => (detail.clone(), theme::STATUS_ERROR),
            MachineState::ConnectionLost => ("connection lost".to_string(), theme::STATUS_ERROR),
            MachineState::Unknown | MachineState::Reachable | MachineState::Unreachable => {
                (NO_ARGV_MACHINE_HINT.to_string(), theme::TEXT_FAINT)
            }
        }
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
            self.fleet.label(&ghost.workspace.machine)
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
            let machine_label: SharedString = self.fleet.label(&ghost.workspace.machine).into();

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
                            // Read-only: the repository is the workspace the
                            // ghost was opened from.
                            .child(ui::prompt_box_control(
                                "ghost-repo",
                                folder_name(&ghost.workspace.path).to_string().into(),
                                false,
                            ))
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
        let candidate_rows: Vec<_> =
            self.fleet
                .machine_candidates()
                .into_iter()
                .enumerate()
                .map(|(index, candidate)| {
                    ui::model_row(("workspace-machine", index), candidate.label.into(), false)
                        .on_click(cx.listener(move |workbench, _, _, cx| {
                            workbench.workspace_machine_picked(index, cx)
                        }))
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
    /// link when it has one, else a connected-machine count.
    fn statusbar_link(&self) -> (String, u32) {
        if let Some(link) = self.argv_statusbar_link() {
            return link;
        }

        (
            format!(
                "{} of {} machines connected",
                self.fleet.connected_machine_count(),
                self.fleet.linked_machine_count()
            ),
            theme::TEXT_FAINT,
        )
    }

    /// `None` when there is no argv machine, or when it has no bridge yet —
    /// the statusbar then shows the fleet-wide count instead. Returning the
    /// absence rather than inventing a string for the probe-only states is
    /// what keeps this a total match over [`MachineState`].
    fn argv_statusbar_link(&self) -> Option<(String, u32)> {
        let machine = self.argv_machine.as_ref()?;

        match self.fleet.state(machine) {
            MachineState::Connecting => Some(("connecting…".to_string(), theme::TEXT_FAINT)),
            MachineState::Connected { daemon_version } => Some((
                format!("{machine} · harvesterd {daemon_version}"),
                theme::TEXT_FAINT,
            )),
            MachineState::ConnectFailed { .. } => {
                Some(("connect failed".to_string(), theme::STATUS_ERROR))
            }
            MachineState::ConnectionLost => {
                Some(("connection lost".to_string(), theme::STATUS_ERROR))
            }
            MachineState::Unknown | MachineState::Reachable | MachineState::Unreachable => None,
        }
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
