//! Top-level UI state and layout (docs/SPEC.md §UI): sidebar (machine +
//! sessions), content (chat or empty state), statusbar.
//!
//! Everything that is *not* layout or user intent lives in client-core's
//! [`Fleet`], which owns the bridge pool, the machine table, the probe
//! verdicts and the connect policy. What is left here reads a machine's one
//! [`MachineState`] and renders it, routes commands by machine key, and keeps
//! the surfaces (the chat, the pickers) the user actually touches.

mod picker;

use gpui::{div, prelude::*, px, rgb, Context, Entity, SharedString, Subscription, Window};
use gpui_component::Root;
use harvester_client_core::{
    BridgeEvents, Fleet, MachineState, MachinesEvents, RequestToken, RequestTokens, LOCAL_MACHINE,
};
use harvester_proto::{SessionInfo, SessionStatus};
use serde_json::Value;

use crate::chat::{Chat, ChatRequest, ChatTarget, MachineCandidate};
use crate::models::ModelChoice;
use crate::theme::{self, Tone};
use crate::ui;
use crate::workspaces::{self, folder_name, Workspace};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use picker::FolderPicker;

const SIDEBAR_WIDTH: f32 = 240.;
/// Enough of a uuidv7 to tell sessions apart at a glance.
const SID_PREVIEW_CHARS: usize = 8;
/// The empty content pane when there is no argv machine to report on — the
/// workspace rows are the way in.
const NO_ARGV_MACHINE_HINT: &str = "open a workspace to begin";

/// One sidebar session row, tagged with the machine whose daemon reported it
/// (commands about it must route back to that machine's bridge).
struct MachineSession {
    machine: String,
    info: SessionInfo,
}

/// A session opening on a machine: what the sidebar spinner reads, and what a
/// failure has to match before it may give up on it.
struct OpeningSession {
    machine: String,
    sid: String,
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
    /// The session we asked a bridge to open, until `Attached` promotes it to
    /// a chat (also the "don't double-open" guard). Carries the machine as
    /// well as the sid because it is cleared by *that machine's* failures:
    /// keyed by sid alone, any other machine losing its bridge cancelled this
    /// guard, and the next click issued a duplicate `Open`.
    opening: Option<OpeningSession>,
    /// Collapsed sidebar sections (all expanded at launch).
    machines_collapsed: bool,
    workspaces_collapsed: bool,
    sessions_collapsed: bool,
    /// The one open conversation, whether or not a session exists behind it
    /// yet — an unstarted chat is the same entity in an earlier state (see
    /// [`Chat`]), so there is nothing else here for it to be.
    chat: Option<Entity<Chat>>,
    /// Keeps [`Workbench::handle_chat_request`] subscribed for as long as the
    /// chat it belongs to is the open one; dropped with the next chat's.
    chat_requests: Option<Subscription>,
    /// Last request failure; sticks in the statusbar until the next one.
    last_error: Option<String>,
}

impl Workbench {
    pub fn new(
        argv_machine: Option<String>,
        daemon_binary: Vec<u8>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let (fleet, events) = Fleet::start(daemon_binary);

        Self::pump_bridge_events(events.bridges, window, cx);

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
            opening: None,
            machines_collapsed: false,
            workspaces_collapsed: false,
            sessions_collapsed: false,
            chat: None,
            chat_requests: None,
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
    fn pump_bridge_events(mut events: BridgeEvents, window: &mut Window, cx: &mut Context<Self>) {
        cx.spawn_in(window, async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update_in(cx, |workbench, window, cx| {
                        // The fleet opens the event, taking the link state on
                        // the way through; what comes back is this window's.
                        let (machine, event) = workbench.fleet.observe_bridge_event(unapplied);

                        workbench.handle_bridge_event(&machine, event, window, cx)
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

    fn pump_machines_events(mut events: MachinesEvents, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update(cx, |workbench, cx| {
                        let event = workbench.fleet.observe_machines_event(unapplied);

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

    /// Pure router for machines-worker events; the fleet already took the
    /// machine table and the probe verdicts off it, so what is left is this
    /// window's.
    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<Self>) {
        match event {
            // The listing and the probes were the fleet's, and it took them.
            machines::Event::Listed(_) | machines::Event::Probed { .. } => {}
            machines::Event::ProviderFailed { provider, detail } => {
                self.last_error = Some(format!("provider {provider}: {detail}"))
            }
            machines::Event::HomeResolved { token, result } => {
                self.handle_home_resolved(token, result, cx)
            }
            machines::Event::DirListed { token, result } => {
                self.handle_dir_listed(token, result, cx)
            }
            machines::Event::WorkspaceValidated { token, error } => {
                self.handle_workspace_validated(token, error, cx)
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
    /// alone — sids are uuids, unique across machines. The fleet already took
    /// the link state off the same event.
    fn handle_bridge_event(
        &mut self,
        machine: &str,
        event: BridgeEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            // The link state was the fleet's, and it took it; a successful
            // connect asks nothing else of this window.
            BridgeEvent::Connected { .. } => {}
            BridgeEvent::ConnectFailed { detail } => self.abort_machine_flows(machine, &detail, cx),
            BridgeEvent::Sessions(sessions) => self.handle_sessions_listed(machine, sessions),
            BridgeEvent::Attached { sid, entries } => {
                self.handle_attached(machine, sid, entries, window, cx)
            }
            BridgeEvent::HistoryReplaced { sid, entries } => {
                self.with_chat(&sid, cx, |chat, cx| chat.replace_history(&entries, cx))
            }
            BridgeEvent::HistoryExtended { sid, entries } => {
                self.with_chat(&sid, cx, |chat, cx| chat.extend_history(&entries, cx))
            }
            BridgeEvent::Pi { sid, event } => self.handle_pi_event(sid, event, cx),
            BridgeEvent::SessionExit { sid, exit_code } => {
                self.handle_session_exit(sid, exit_code, cx)
            }
            BridgeEvent::RequestFailed { detail } => {
                self.handle_request_failed(machine, detail, cx)
            }
            BridgeEvent::ModelsListed { token, result } => {
                self.handle_models_listed(machine, token, result, cx)
            }
            BridgeEvent::SessionModelsListed { sid, token, result } => {
                self.with_chat(&sid, cx, |chat, cx| {
                    chat.handle_models_listed(token, result, cx)
                })
            }
            BridgeEvent::SessionState { sid, token, result } => {
                self.with_chat(&sid, cx, |chat, cx| {
                    chat.handle_session_state(token, result, cx)
                })
            }
            BridgeEvent::ModelSet { sid, token, result } => {
                self.handle_model_set(sid, token, result, cx)
            }
            BridgeEvent::Disconnected => self.abort_machine_flows(machine, "connection lost", cx),
        }

        cx.notify();
    }

    /// Commands queued for this machine died with its bridge: give up on an
    /// open *on this machine* and tell a chat targeting it that nothing it
    /// asked for will arrive. Every clause is machine-scoped — another
    /// machine's sessions are unaffected by this one losing its bridge.
    fn abort_machine_flows(&mut self, machine: &str, detail: &str, cx: &mut Context<Self>) {
        self.abandon_open_on(machine);

        let Some(chat) = self.chat_on(machine, cx) else {
            return;
        };

        chat.update(cx, |chat, cx| {
            chat.fail_pending_requests(detail.to_string(), cx)
        });
    }

    /// Stop waiting for an open on `machine`. An open on any other machine is
    /// still in flight and keeps its guard.
    fn abandon_open_on(&mut self, machine: &str) {
        if self
            .opening
            .as_ref()
            .is_some_and(|opening| opening.machine == machine)
        {
            self.opening = None;
        }
    }

    /// The open chat, if it targets `machine`. Scoping by machine is what
    /// keeps one machine's failure from cancelling a start still in flight on
    /// another — without it a retry mints a second sid and starts a second pi
    /// for one prompt.
    fn chat_on(&self, machine: &str, cx: &Context<Self>) -> Option<Entity<Chat>> {
        self.chat
            .as_ref()
            .filter(|chat| chat.read(cx).machine() == machine)
            .cloned()
    }

    fn handle_sessions_listed(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
        self.sessions.retain(|session| session.machine != machine);
        self.sessions
            .extend(sessions.into_iter().map(|info| MachineSession {
                machine: machine.to_string(),
                info,
            }));
        // Newest first by last *activity*, which is the daemon's own order
        // (docs/DAEMON.md §list_sessions) and the only one that survives
        // merging machines. Not by sid: a uuidv7 sorts by creation, so a
        // session prompted five minutes ago sank below one created after it
        // and untouched since — and pi's id charset is wider than uuidv7, so a
        // sid already on disk may not sort chronologically at all. The sid
        // breaks ties only to keep the order stable between listings.
        self.sessions.sort_by(|a, b| {
            b.info
                .mtime_ms
                .cmp(&a.info.mtime_ms)
                .then_with(|| b.info.sid.cmp(&a.info.sid))
        });
    }

    fn handle_attached(
        &mut self,
        machine: &str,
        sid: String,
        entries: Vec<Value>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The daemon's list may predate a resume we just did; disk on the VM
        // stays authoritative, this is display state only.
        self.set_session_status(&sid, SessionStatus::Running);
        self.abandon_open_on(machine);

        // The chat that minted this sid is already on screen with the user's
        // first prompt rendered in it, so it is promoted in place. Building a
        // fresh entity here would throw away the pane they are already
        // watching — the same mistake C7 fixed for resyncs.
        if let Some(chat) = self.take_starting_chat(&sid, cx) {
            chat.update(cx, |chat, cx| {
                chat.promote_to_live(sid, &entries, cx);
                chat.request_model_controls(cx);
            });

            return;
        }

        let target = self.chat_target(machine, &sid);
        let chat = cx.new(|cx| Chat::live(sid, target, &entries, self.tokens.clone(), window, cx));

        // Subscribe before the chat asks for anything: an entity cannot emit
        // to a subscriber that does not exist yet.
        self.chat_requests = Some(cx.subscribe_in(&chat, window, Self::handle_chat_request));
        chat.update(cx, |chat, cx| chat.request_model_controls(cx));
        self.chat = Some(chat);
    }

    /// Where a chat for `sid` runs, as the sidebar knows it. The session is in
    /// the list by now — a start inserts its row before `Attached` lands, and
    /// an open can only have come from a row that was already there.
    fn chat_target(&self, machine: &str, sid: &str) -> ChatTarget {
        let workspace_path = self
            .sessions
            .iter()
            .find(|session| session.info.sid == sid)
            .map(|session| session.info.cwd.clone())
            .unwrap_or_default();

        ChatTarget {
            machine: machine.to_string(),
            machine_label: self.fleet.label(machine).into(),
            workspace_path,
        }
    }

    /// The chat holds no command sender (see [`ChatTarget::machine`]), so every
    /// request it makes is routed here against the bridge that is live *now*.
    /// A machine with no usable bridge is the case that used to vanish: the
    /// prompt was rendered, the turn went busy, and the send failed into
    /// stderr. It says so in the statusbar instead.
    fn handle_chat_request(
        &mut self,
        chat: &Entity<Chat>,
        request: &ChatRequest,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let machine = chat.read(cx).machine().to_string();

        if !self.machine_ready_for(request, &machine) {
            let detail = format!(
                "{}: not connected — nothing was sent",
                self.fleet.label(&machine)
            );

            // The refusal has to reach the chat, not just the statusbar: a
            // model-list request records as outstanding when it is *made*
            // (`Pending::ask`), so a chat whose request was never sent would
            // sit on "loading models…" for the life of the pane.
            chat.update(cx, |chat, cx| {
                chat.fail_pending_requests(detail.clone(), cx)
            });
            self.last_error = Some(detail);
            cx.notify();

            return;
        }

        match request {
            ChatRequest::ListModels { cwd, token } => self.send_command(
                &machine,
                Command::ListModels {
                    cwd: cwd.clone(),
                    token: *token,
                },
            ),
            ChatRequest::Start {
                sid,
                cwd,
                provider,
                model,
                name,
                first_prompt,
            } => {
                // The open guard goes up against the session the chat just
                // minted: the sidebar spinner reads it, and only a failure
                // naming this machine may clear it.
                self.opening = Some(OpeningSession {
                    machine: machine.clone(),
                    sid: sid.clone(),
                });
                self.send_command(
                    &machine,
                    Command::StartSession {
                        sid: sid.clone(),
                        cwd: cwd.clone(),
                        provider: provider.clone(),
                        model: model.clone(),
                        name: name.clone(),
                        first_prompt: first_prompt.clone(),
                    },
                );
            }
            ChatRequest::Prompt { sid, text } => self.send_command(
                &machine,
                Command::Prompt {
                    sid: sid.clone(),
                    text: text.clone(),
                },
            ),
            ChatRequest::SetModel {
                sid,
                provider,
                model_id,
                token,
            } => self.send_command(
                &machine,
                Command::SetModel {
                    sid: sid.clone(),
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    token: *token,
                },
            ),
            ChatRequest::ModelControls { sid, models, state } => {
                self.send_command(
                    &machine,
                    Command::ListSessionModels {
                        sid: sid.clone(),
                        token: *models,
                    },
                );
                self.send_command(
                    &machine,
                    Command::GetSessionState {
                        sid: sid.clone(),
                        token: *state,
                    },
                );
            }
        }

        cx.notify();
    }

    /// Whether `machine` can carry this request now. The two halves of a chat
    /// want different answers: a chat with no session yet connects lazily and
    /// lets its commands queue in the bridge channel behind the bootstrap
    /// (SPEC §Flows "Start session" — no readiness gate), while a live one
    /// refuses to send into a bridge that is not up, because a prompt that
    /// fails into stderr looks to the user exactly like one pi ignored.
    fn machine_ready_for(&mut self, request: &ChatRequest, machine: &str) -> bool {
        if request.is_pre_session() {
            return self.ensure_connection(machine);
        }

        self.fleet.state(machine).is_live()
    }

    fn handle_pi_event(&mut self, sid: String, event: Value, cx: &mut Context<Self>) {
        self.with_chat(&sid, cx, |chat, cx| chat.handle_pi_event(&event, cx));
    }

    fn handle_session_exit(&mut self, sid: String, exit_code: Option<i32>, cx: &mut Context<Self>) {
        self.set_session_status(&sid, SessionStatus::Exited);
        self.with_chat(&sid, cx, |chat, cx| chat.handle_session_exit(exit_code, cx));
    }

    fn handle_request_failed(&mut self, machine: &str, detail: String, cx: &mut Context<Self>) {
        self.abandon_open_on(machine);

        if let Some(chat) = self.chat_on(machine, cx) {
            chat.update(cx, Chat::abandon_start);
        }

        self.last_error = Some(format!("{machine}: {detail}"));
    }

    /// A `list_models` answered (the unstarted chat's throwaway pi). Routed by
    /// machine, not sid, because the chat that asked has no session yet; a
    /// chat that has since been retargeted is waiting on a different token, so
    /// the abandoned request's answer is dropped by the control itself.
    fn handle_models_listed(
        &mut self,
        machine: &str,
        token: RequestToken,
        result: Result<Value, String>,
        cx: &mut Context<Self>,
    ) {
        let Some(chat) = self.chat_on(machine, cx) else {
            return;
        };

        chat.update(cx, |chat, cx| chat.handle_models_listed(token, result, cx));
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
                self.with_chat(&sid, cx, |chat, cx| chat.handle_model_set(token, &data, cx))
            }
            // The session keeps its previous model; statusbar material.
            Err(detail) => self.last_error = Some(format!("set_model: {detail}")),
        }
    }

    fn chat_for(&self, sid: &str, cx: &Context<Self>) -> Option<Entity<Chat>> {
        self.chat
            .as_ref()
            .filter(|chat| chat.read(cx).sid() == Some(sid))
            .cloned()
    }

    /// Act on the open chat if it is the one rendering `sid` — the shape every
    /// sid-scoped bridge event shares.
    fn with_chat(
        &mut self,
        sid: &str,
        cx: &mut Context<Self>,
        act: impl FnOnce(&mut Chat, &mut Context<Chat>),
    ) {
        let Some(chat) = self.chat_for(sid, cx) else {
            return;
        };

        chat.update(cx, act);
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
        if let Some(opening) = &self.opening {
            if opening.sid != sid {
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

        // Navigating to a session abandons a chat that never became one —
        // deliberate move away from something that exists nowhere but here.
        // A chat mid-start keeps its place: its pi is on the way.
        self.discard_unstarted_chat(cx);

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

        self.opening = Some(OpeningSession {
            machine: machine.clone(),
            sid: sid.clone(),
        });
        self.send_command(&machine, Command::Open { sid, cwd });
        cx.notify();
    }

    /// A chat with no session behind it exists only here, so navigating away
    /// from it is the whole of discarding it.
    fn discard_unstarted_chat(&mut self, cx: &Context<Self>) {
        let unstarted = self
            .chat
            .as_ref()
            .is_some_and(|chat| chat.read(cx).sid().is_none());

        if unstarted {
            self.chat = None;
            self.chat_requests = None;
        }
    }

    fn add_workspace(&mut self, machine: String, path: String, cx: &mut Context<Self>) {
        let workspace = Workspace { machine, path };

        if self.workspaces.contains(&workspace) {
            return;
        }

        self.workspaces.push(workspace);
        self.refresh_chat_machine_candidates(cx);
        // A new ad-hoc machine may have entered the probe set, and the picker
        // no longer connects the machine it browses (its execs need no
        // daemon) — so this is where a machine that just earned a workspace
        // gets its bridge, same rule as at launch.
        self.track_machines();

        if let Err(error) = workspaces::save(&self.workspaces) {
            self.last_error = Some(format!("save workspaces: {error}"));
        }
    }

    /// The workspace list changed, so which machines an unstarted chat could
    /// run on changed with it. The chat holds the answer rather than deriving
    /// it (see [`Chat::set_machine_candidates`]), so it has to be told.
    fn refresh_chat_machine_candidates(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.clone() else {
            return;
        };
        let candidates = self.machine_candidates_for(chat.read(cx).workspace_path());

        chat.update(cx, |chat, _cx| chat.set_machine_candidates(candidates));
    }

    /// If this `Attached` completes a start the open chat made: add its
    /// session to the sidebar and hand the chat back to be promoted in place.
    /// `None` for plain opens and resumes, which build their own chat.
    fn take_starting_chat(&mut self, sid: &str, cx: &Context<Self>) -> Option<Entity<Chat>> {
        let chat = self.chat.clone()?;
        let (machine, cwd, name) = {
            let chat = chat.read(cx);
            let (starting_sid, name) = chat.starting_session()?;

            if starting_sid != sid {
                return None;
            }

            (
                chat.machine().to_string(),
                chat.workspace_path().to_string(),
                name.to_string(),
            )
        };

        // Synthesized sidebar row, newest-first (uuidv7 now is the maximum).
        // Display state only, replaced by the next real `list_sessions` —
        // disk on the VM stays truth. `file` is absent rather than blank
        // because pi has not written one yet, and `mtime_ms` is now because
        // being started is this session's last activity — which is what the
        // list is ordered by, so the row sorts where the user expects even
        // before the daemon confirms it.
        self.sessions.insert(
            0,
            MachineSession {
                machine,
                info: SessionInfo {
                    sid: sid.to_string(),
                    name: Some(name),
                    cwd,
                    file: None,
                    mtime_ms: unix_millis_now(),
                    status: SessionStatus::Running,
                },
            },
        );

        Some(chat)
    }

    /// A workspace row's "+" was clicked: open a chat there with no session
    /// behind it yet (SPEC §Flows "Start session"). The workspace's machine
    /// connects lazily when the chat's first request routes through
    /// [`Workbench::machine_ready_for`].
    fn open_new_session_chat(
        &mut self,
        workspace_index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspaces.get(workspace_index).cloned() else {
            return;
        };

        // Re-clicking the row of the chat already on screen keeps it, and the
        // text typed into it.
        let same_workspace = self.chat.as_ref().is_some_and(|chat| {
            let chat = chat.read(cx);

            chat.starting_session().is_none()
                && chat.sid().is_none()
                && chat.machine() == workspace.machine
                && chat.workspace_path() == workspace.path
        });

        if self.opening.is_some() || same_workspace {
            return;
        }

        let target = ChatTarget {
            machine_label: self.fleet.label(&workspace.machine).into(),
            machine: workspace.machine,
            workspace_path: workspace.path,
        };
        let candidates = self.machine_candidates_for(&target.workspace_path);
        let chat =
            cx.new(|cx| Chat::unstarted(target, candidates, self.tokens.clone(), window, cx));

        // Subscribed before anything can be emitted — including the model
        // fetch the constructor itself starts.
        self.chat_requests = Some(cx.subscribe_in(&chat, window, Self::handle_chat_request));
        self.chat = Some(chat);
        cx.notify();
    }

    /// Machines an unstarted chat on `path` could be retargeted to: the stored
    /// workspaces sharing that path, as (machine key, display label) pairs.
    fn machine_candidates_for(&self, path: &str) -> Vec<MachineCandidate> {
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

    /// The workspaces-header "+": one qualifying machine goes straight to
    /// browsing; several open the "which machine?" step first.
    fn open_add_workspace(&mut self, cx: &mut Context<Self>) {
        if self.picker.is_some() || self.machine_picker_open {
            return;
        }

        let candidates = self.fleet.machine_candidates();

        match candidates.len() {
            0 => self.last_error = Some("no machines available to add a workspace on".to_string()),
            1 => self.open_picker(candidates[0].key.clone(), cx),
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
        self.open_picker(machine, cx);
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

    /// Read-only rows; the per-row "+" is the one click target (opens an
    /// unstarted chat in that workspace).
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
                                move |workbench, _, window, cx| {
                                    workbench.open_new_session_chat(index, window, cx)
                                },
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
    fn machine_dot(&self, key: &str) -> Tone {
        present_machine(self.fleet.state(key), key).tone
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
            .is_some_and(|chat| chat.read(cx).sid() == Some(session.sid.as_str()));
        let opening = self
            .opening
            .as_ref()
            .is_some_and(|opening| opening.sid == session.sid);
        let tone = match session.status {
            SessionStatus::Running => Tone::Live,
            SessionStatus::Exited => Tone::Idle,
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
            tone,
            selected,
            opening,
        )
        .on_click(cx.listener(move |workbench, _, _, cx| workbench.open_session(&sid, cx)))
    }

    fn render_content(&self) -> impl IntoElement {
        let content = div().flex_1().min_w(px(0.)).h_full();

        if let Some(chat) = &self.chat {
            return content.child(chat.clone());
        }

        let (hint, color) = match &self.argv_machine {
            Some(machine) => self.argv_machine_hint(machine),
            // No argv machine: the workspace rows are the way in.
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
        };

        content.child(
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(rgb(color.color()))
                .child(hint),
        )
    }

    /// The empty content pane when the argv machine has a bridge; a machine
    /// that only ever got probed falls back to the workspace-rows hint,
    /// because there is nothing about its daemon to report yet.
    fn argv_machine_hint(&self, machine: &str) -> (String, Tone) {
        let display = present_machine(self.fleet.state(machine), machine);

        match display.pane_message {
            Some(message) => (message, display.tone),
            None => (NO_ARGV_MACHINE_HINT.to_string(), Tone::Idle),
        }
    }

    /// A picker row was clicked (`None` = the "default (pi chooses)" row).
    /// There is one chat and it owns its own picker, so there is no longer a
    /// question of whose modal this is.
    fn model_picked(&mut self, choice: Option<usize>, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.clone() else {
            return;
        };

        chat.update(cx, |chat, cx| chat.pick_model(choice, cx));
    }

    fn model_picker_closed(&mut self, cx: &mut Context<Self>) {
        let Some(chat) = self.chat.clone() else {
            return;
        };

        chat.update(cx, |chat, cx| chat.close_model_picker(cx));
    }

    /// Only a chat with no session yet offers the "default (pi chooses)" row —
    /// a running session always has a concrete model — and that difference is
    /// an argument, not a stored flag saying which state this modal is for.
    fn render_model_picker(
        &self,
        models: &[ModelChoice],
        selected_model: Option<usize>,
        offer_default: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let default_row = offer_default.then(|| {
            ui::model_row(
                "model-default",
                "default (pi chooses)".into(),
                selected_model.is_none(),
            )
            .on_click(cx.listener(move |workbench, _, _, cx| workbench.model_picked(None, cx)))
            .into_any_element()
        });
        let rows: Vec<_> = default_row
            .into_iter()
            .chain(models.iter().enumerate().map(|(index, choice)| {
                ui::model_row(
                    ("model", index),
                    choice.name.clone(),
                    selected_model == Some(index),
                )
                .on_click(
                    cx.listener(move |workbench, _, _, cx| workbench.model_picked(Some(index), cx)),
                )
                .into_any_element()
            }))
            .collect();

        ui::modal_list("model", "Choose model", ui::ModalListHeight::Fixed, rows)
            .on_click(cx.listener(move |workbench, _, _, cx| workbench.model_picker_closed(cx)))
    }

    /// The add-workspace machine step: same modal-list pattern as the model
    /// picker; picking a machine opens the folder picker there.
    fn render_machine_picker(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let candidate_rows: Vec<_> = self
            .fleet
            .machine_candidates()
            .into_iter()
            .enumerate()
            .map(|(index, candidate)| {
                ui::model_row(("workspace-machine", index), candidate.label.into(), false)
                    .on_click(cx.listener(move |workbench, _, _, cx| {
                        workbench.workspace_machine_picked(index, cx)
                    }))
                    .into_any_element()
            })
            .collect();

        ui::modal_list(
            "workspace-machine",
            "Add workspace — choose machine",
            ui::ModalListHeight::FitContent,
            candidate_rows,
        )
        .on_click(cx.listener(|workbench, _, _, cx| workbench.close_machine_picker(cx)))
    }

    fn render_statusbar(&self) -> impl IntoElement {
        let (link_text, link_tone) = self.statusbar_link();

        ui::statusbar_frame()
            .child(div().text_color(rgb(link_tone.color())).child(link_text))
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
    fn statusbar_link(&self) -> (String, Tone) {
        if let Some(link) = self.argv_statusbar_link() {
            return link;
        }

        (
            format!(
                "{} of {} machines connected",
                self.fleet.connected_machine_count(),
                self.fleet.linked_machine_count()
            ),
            Tone::Idle,
        )
    }

    /// `None` when there is no argv machine, or when it has no bridge yet —
    /// the statusbar then shows the fleet-wide count instead. Returning the
    /// absence rather than inventing a string for the probe-only states is
    /// what keeps this a total match over [`MachineState`].
    fn argv_statusbar_link(&self) -> Option<(String, Tone)> {
        let machine = self.argv_machine.as_ref()?;
        let display = present_machine(self.fleet.state(machine), machine);

        Some((display.statusbar_link?, display.tone))
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Cloned out of the chat here so the entity borrow doesn't fight the
        // `&mut cx` the modal's listeners need.
        let model_picker: Option<(Vec<ModelChoice>, Option<usize>, bool)> = self
            .chat
            .as_ref()
            .and_then(|chat| chat.read(cx).picker_contents());
        let machine_picker = self
            .chat
            .as_ref()
            .filter(|chat| chat.read(cx).machine_picker_open())
            .cloned();

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
            // Painted last so the scrims cover everything above.
            .children(
                self.machine_picker_open
                    .then(|| self.render_machine_picker(cx)),
            )
            .children(machine_picker.map(|chat| {
                // The chat owns this modal's rows and clicks; the workbench
                // only decides where in the stack it is painted.
                chat.update(cx, |chat, cx| chat.render_machine_picker(cx))
                    .into_any_element()
            }))
            .children(
                self.picker
                    .as_ref()
                    .map(|picker| self.render_picker(picker, cx)),
            )
            .children(model_picker.map(|(models, selected_model, offer_default)| {
                self.render_model_picker(&models, selected_model, offer_default, cx)
            }))
            // `Root` holds the dialogs and notifications but renders neither;
            // placing their layers is the root view's job, and omitting them
            // makes `open_dialog` a silent no-op. Above the hand-rolled scrims
            // above, which they replace over the course of the migration.
            .children(Root::render_dialog_layer(window, cx))
            .children(Root::render_notification_layer(window, cx))
    }
}

/// Wall-clock milliseconds, the units `SessionInfo::mtime_ms` is ordered by.
/// A clock before the epoch reads as 0, which sorts the row last rather than
/// panicking on a machine with a broken clock.
fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since_epoch| since_epoch.as_millis() as u64)
        .unwrap_or(0)
}

fn sid_preview(sid: &str) -> String {
    sid.chars().take(SID_PREVIEW_CHARS).collect()
}

/// Everything the UI says about one machine's link, decided in one place.
///
/// There used to be three matches over [`MachineState`] in this file — the
/// dot, the empty pane's sentence, the statusbar link — and a fourth would
/// have been free to disagree with them. A state that means "failed" cannot
/// now be red in one and gray in another, because the tone is chosen once.
/// The strings differ per surface because they are different sentences, not
/// because the state means different things.
struct MachineDisplay {
    tone: Tone,
    /// The content pane's sentence when this machine is the one on screen.
    /// `None` for the probe-only states: nothing about a daemon to report.
    pane_message: Option<String>,
    /// The statusbar's link text, same rule.
    statusbar_link: Option<String>,
}

fn present_machine(state: MachineState<'_>, machine: &str) -> MachineDisplay {
    match state {
        MachineState::Connecting => MachineDisplay {
            tone: Tone::Pending,
            pane_message: Some(format!("connecting to {machine}…")),
            statusbar_link: Some("connecting…".to_string()),
        },
        MachineState::Connected { daemon_version } => MachineDisplay {
            tone: Tone::Live,
            pane_message: Some("select a session".to_string()),
            statusbar_link: Some(format!("{machine} · harvesterd {daemon_version}")),
        },
        MachineState::ConnectFailed { detail } => MachineDisplay {
            tone: Tone::Failed,
            pane_message: Some(detail.to_string()),
            statusbar_link: Some("connect failed".to_string()),
        },
        MachineState::ConnectionLost => MachineDisplay {
            tone: Tone::Failed,
            pane_message: Some("connection lost".to_string()),
            statusbar_link: Some("connection lost".to_string()),
        },
        // A probe verdict is not a link: unreachable still reads as failed,
        // but there is nothing to say about a daemon nobody has reached for.
        MachineState::Unreachable => MachineDisplay {
            tone: Tone::Failed,
            pane_message: None,
            statusbar_link: None,
        },
        MachineState::Unknown | MachineState::Reachable => MachineDisplay {
            tone: Tone::Idle,
            pane_message: None,
            statusbar_link: None,
        },
    }
}
