//! The client's state, in one entity that views observe (docs/SPEC.md C12).
//!
//! Everything that is not layout or user intent lives here: client-core's
//! [`Fleet`], the two event pumps, and the machine, workspace and session
//! tables. Views hold an `Entity<FleetStore>`, read it whenever they render,
//! and repaint when it notifies — nothing is pushed down into them and nothing
//! keeps a copy that can go stale.
//!
//! The store has no window, so it never renders and never raises a dialog or a
//! notification. What a view has to act on leaves as a [`StoreEvent`]; what a
//! view only has to *see* is a `cx.notify()` away.

use gpui::{Context, EventEmitter};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use harvester_client_core::{
    BridgeEvents, Fleet, Host, MachineCandidate, MachineState, MachinesEvents, RequestTokens,
    LOCAL_MACHINE,
};
use harvester_proto::{SessionInfo, SessionStatus};

use crate::workspaces::{self, Workspace};

/// One sidebar session row, tagged with the machine whose daemon reported it
/// (commands about it must route back to that machine's bridge).
pub struct MachineSession {
    pub machine: String,
    pub info: SessionInfo,
}

/// A session opening on a machine: what the sidebar spinner reads, and what a
/// failure has to match before it may give up on it.
struct OpeningSession {
    machine: String,
    sid: String,
}

/// What the store cannot finish on its own.
///
/// Deliberately not a vocabulary of its own: a bridge event the tables have
/// taken their share of is passed on *verbatim*, because the only thing left
/// in it is pi's, and translating pi is what this codebase does not do
/// (SPEC §Architecture, "the client speaks Pi").
pub enum StoreEvent {
    /// A bridge event, after the fleet took the link state and the tables took
    /// the session status and the open guard.
    Bridge { machine: String, event: BridgeEvent },
    /// A machines-worker answer no table wanted — the folder picker's.
    Machines(machines::Event),
    /// Something the user has to be told about. The store has no window, so it
    /// says so and the workbench raises the notification.
    Failed(String),
}

pub struct FleetStore {
    /// The machine given on argv, if any — an ad-hoc SSH destination,
    /// connected eagerly to preserve the argv workflow.
    argv_machine: Option<String>,
    /// Every machine, its state and its bridge — all of it client-core's.
    fleet: Fleet,
    /// Mints the tokens that correlate every worker answer with the request
    /// that asked for it; one per process, cloned into each surface.
    tokens: RequestTokens,
    /// All connected machines' sessions, newest first.
    sessions: Vec<MachineSession>,
    /// User's workspace list (`~/.harvester/workspaces.json`), all machines.
    workspaces: Vec<Workspace>,
    /// The session we asked a bridge to open, until `Attached` promotes it to
    /// a chat (also the "don't double-open" guard). Carries the machine as
    /// well as the sid because it is cleared by *that machine's* failures:
    /// keyed by sid alone, any other machine losing its bridge cancelled this
    /// guard, and the next click issued a duplicate `Open`.
    opening: Option<OpeningSession>,
}

impl EventEmitter<StoreEvent> for FleetStore {}

impl FleetStore {
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

        let mut store = Self {
            argv_machine: argv_machine.clone(),
            fleet,
            tokens: RequestTokens::default(),
            sessions: Vec::new(),
            workspaces: workspaces::load(),
            opening: None,
        };

        // SPEC §Flows "Reopen client": machines with stored workspaces
        // connect in the background so their sessions reappear unprompted.
        // Ad-hoc ones connect on this call; provider-key ones as soon as the
        // worker's first listing makes them resolvable.
        store.track_machines();

        if let Some(machine) = argv_machine {
            store.ensure_connection(&machine, cx);
        }

        store
    }

    /// Every bridge's events on one machine-tagged channel, pumped by a gpui
    /// task — an mpsc receiver is reactor-independent, so awaiting it from
    /// gpui's executor is exactly what the split is designed for.
    ///
    /// No window is threaded through either pump: the store builds nothing
    /// that needs one. The views that do are subscribers, and a subscription
    /// carries its own window (`cx.subscribe_in`).
    fn pump_bridge_events(mut events: BridgeEvents, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update(cx, |store, cx| {
                        // The fleet opens the event, taking the link state on
                        // the way through; what comes back is this store's.
                        let (machine, event) = store.fleet.observe_bridge_event(unapplied);

                        store.handle_bridge_event(machine, event, cx)
                    })
                    .is_err()
                {
                    // Store dropped — window closed; stop pumping.
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
                    .update(cx, |store, cx| {
                        let event = store.fleet.observe_machines_event(unapplied);

                        store.handle_machines_event(event, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    /// The one bridge event the tables consume outright: a machine's session
    /// list is the sidebar's, and no view has a second question about it.
    /// Everything else is handed on once the tables have taken their share.
    fn handle_bridge_event(&mut self, machine: String, event: BridgeEvent, cx: &mut Context<Self>) {
        if let BridgeEvent::Sessions(sessions) = event {
            self.record_sessions(&machine, sessions);
            cx.notify();

            return;
        }

        self.apply_bridge_event(&machine, &event, cx);
        cx.emit(StoreEvent::Bridge { machine, event });
    }

    /// What the tables take off a bridge event before the views see it. Each
    /// arm notifies only when something the sidebar or the statusbar draws
    /// actually moved — a streamed pi token moves nothing here.
    fn apply_bridge_event(&mut self, machine: &str, event: &BridgeEvent, cx: &mut Context<Self>) {
        match event {
            // The fleet took the link state; the dot it draws still moved.
            BridgeEvent::Connected { .. } => cx.notify(),
            BridgeEvent::ConnectFailed { .. } | BridgeEvent::Disconnected => {
                self.abandon_open_on(machine);
                cx.notify();
            }
            BridgeEvent::RequestFailed { detail } => {
                self.abandon_open_on(machine);
                self.fail(format!("{machine}: {detail}"), cx);
                cx.notify();
            }
            BridgeEvent::Attached { sid, .. } => {
                // The daemon's list may predate a resume we just did; disk on
                // the VM stays authoritative, this is display state only.
                self.set_session_status(sid, SessionStatus::Running);
                self.abandon_open_on(machine);
                cx.notify();
            }
            BridgeEvent::SessionExit { sid, .. } => {
                self.set_session_status(sid, SessionStatus::Exited);
                cx.notify();
            }
            // The session keeps the model it had; the answer still goes on to
            // the chat, whose token guard drops it.
            BridgeEvent::ModelSet {
                result: Err(detail),
                ..
            } => self.fail(format!("set_model: {detail}"), cx),
            // A chat's answers. No table here has a stake in them, and the
            // chat that does repaints itself.
            BridgeEvent::Sessions(_)
            | BridgeEvent::HistoryReplaced { .. }
            | BridgeEvent::HistoryExtended { .. }
            | BridgeEvent::Pi { .. }
            | BridgeEvent::ModelsListed { .. }
            | BridgeEvent::SessionModelsListed { .. }
            | BridgeEvent::SessionState { .. }
            | BridgeEvent::ModelSet { .. } => {}
        }
    }

    /// Pure router for machines-worker events; the fleet already took the
    /// machine table and the probe verdicts off it, so what is left is either
    /// a failure to report or the folder picker's.
    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<Self>) {
        match event {
            // The listing and the probes were the fleet's, and it took them —
            // but a listing renames machines, which every label reads through.
            machines::Event::Listed { .. } | machines::Event::Probed { .. } => cx.notify(),
            machines::Event::ProviderFailed { provider, detail } => {
                self.fail(format!("provider {provider}: {detail}"), cx)
            }
            answer => cx.emit(StoreEvent::Machines(answer)),
        }
    }

    fn record_sessions(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
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

    fn set_session_status(&mut self, sid: &str, status: SessionStatus) {
        for session in &mut self.sessions {
            if session.info.sid == sid {
                session.info.status = status;
            }
        }
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

    /// Something the user has to be told about, on its way to a notification.
    fn fail(&mut self, detail: String, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::Failed(detail));
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

    /// Lazily connects through the fleet; `false` when the machine can't be
    /// connected. The fleet's reason is carried into the notification verbatim
    /// — it is the only diagnosis the user gets for a machine that will not
    /// come up.
    pub fn ensure_connection(&mut self, machine: &str, cx: &mut Context<Self>) -> bool {
        match self.fleet.ensure_connection(machine) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), cx);

                false
            }
        }
    }

    /// Routes to the machine's bridge. A dead or missing bridge needs no
    /// handling here — `Disconnected`/`ConnectFailed` events drive the
    /// visible state.
    pub fn send(&self, machine: &str, command: Command) {
        self.fleet.send(machine, command);
    }

    /// Routes a host op to the machines worker. `false` (with the reason
    /// reported) when the worker never started — without it there is no
    /// runtime to exec on, and the caller has nothing to wait for.
    pub fn send_host_command(
        &mut self,
        command: machines::Command,
        cx: &mut Context<Self>,
    ) -> bool {
        match self.fleet.send_host_command(command) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), cx);

                false
            }
        }
    }

    pub fn add_workspace(&mut self, machine: String, path: String, cx: &mut Context<Self>) {
        let workspace = Workspace { machine, path };

        if self.workspaces.contains(&workspace) {
            return;
        }

        self.workspaces.push(workspace);
        // A new ad-hoc machine may have entered the probe set, and the picker
        // does not connect the machine it browses (its execs need no daemon)
        // — so this is where a machine that just earned a workspace gets its
        // bridge, same rule as at launch.
        self.track_machines();
        cx.notify();

        if let Err(error) = workspaces::save(&self.workspaces) {
            self.fail(format!("save workspaces: {error}"), cx);
        }
    }

    /// The open guard goes up against the session a chat just minted: the
    /// sidebar spinner reads it, and only a failure naming this machine may
    /// clear it.
    pub fn begin_start(&mut self, machine: String, sid: String, cx: &mut Context<Self>) {
        self.opening = Some(OpeningSession { machine, sid });
        cx.notify();
    }

    /// Whether an `Open` for `sid` may go out now.
    ///
    /// The guard is cleared by `Attached`/`RequestFailed`/`ConnectFailed`,
    /// none of which arrive from a daemon that is live but silent — so
    /// clicking the same row again is the escape hatch: it clears the guard
    /// and lets the reissue through (create-or-resume + attach is idempotent).
    pub fn may_open_session(&mut self, sid: &str) -> bool {
        let Some(opening) = &self.opening else {
            return true;
        };

        if opening.sid != sid {
            return false;
        }

        self.opening = None;

        true
    }

    /// Open (or resume) a session by sid, not by index — the sidebar renders
    /// a filtered view of the table, so display positions don't map back to it.
    pub fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        let Some(session) = self.sessions.iter().find(|session| session.info.sid == sid) else {
            return;
        };
        let machine = session.machine.clone();
        let sid = session.info.sid.clone();
        let cwd = session.info.cwd.clone();

        if !self.ensure_connection(&machine, cx) {
            return;
        }

        self.opening = Some(OpeningSession {
            machine: machine.clone(),
            sid: sid.clone(),
        });
        self.fleet.send(&machine, Command::Open { sid, cwd });
        cx.notify();
    }

    /// The sidebar row for a session a chat just started, synthesized so the
    /// list shows it before the daemon's next `list_sessions` confirms it.
    ///
    /// Newest-first (uuidv7 now is the maximum). Display state only — disk on
    /// the VM stays truth. `file` is absent rather than blank because pi has
    /// not written one yet, and `mtime_ms` is now because being started is
    /// this session's last activity, which is what the list is ordered by.
    pub fn insert_started_session(
        &mut self,
        machine: String,
        sid: String,
        name: String,
        cwd: String,
        cx: &mut Context<Self>,
    ) {
        self.sessions.insert(
            0,
            MachineSession {
                machine,
                info: SessionInfo {
                    sid,
                    name: Some(name),
                    cwd,
                    file: None,
                    mtime_ms: unix_millis_now(),
                    status: SessionStatus::Running,
                },
            },
        );
        cx.notify();
    }

    pub fn argv_machine(&self) -> Option<&str> {
        self.argv_machine.as_deref()
    }

    pub fn tokens(&self) -> RequestTokens {
        self.tokens.clone()
    }

    pub fn label(&self, machine: &str) -> String {
        self.fleet.label(machine)
    }

    pub fn state(&self, machine: &str) -> MachineState<'_> {
        self.fleet.state(machine)
    }

    /// Whether `machine` has a daemon link a live session's commands may go
    /// down. A prompt that fails into stderr looks to the user exactly like
    /// one pi ignored, so the caller checks before sending.
    pub fn is_live(&self, machine: &str) -> bool {
        self.fleet.state(machine).is_live()
    }

    /// The fleet's own sentence when a machine resolves to no host, carried
    /// verbatim — it is what the folder picker shows.
    pub fn host_for(&self, machine: &str) -> Result<Host, String> {
        self.fleet
            .host_for(machine)
            .map_err(|error| error.to_string())
    }

    pub fn provider_machines(&self) -> &[machines::MachineRow] {
        self.fleet.provider_machines()
    }

    pub fn missing_tracked_machines(&self) -> Vec<String> {
        self.fleet.missing_tracked_machines()
    }

    pub fn ad_hoc_machines(&self) -> Vec<String> {
        self.fleet.ad_hoc_machines()
    }

    pub fn machine_candidates(&self) -> Vec<MachineCandidate> {
        self.fleet.machine_candidates()
    }

    pub fn connected_machine_count(&self) -> usize {
        self.fleet.connected_machine_count()
    }

    pub fn linked_machine_count(&self) -> usize {
        self.fleet.linked_machine_count()
    }

    pub fn workspaces(&self) -> &[Workspace] {
        &self.workspaces
    }

    /// Machines a chat on `path` could run: the stored workspaces sharing that
    /// path, as (machine key, display label) pairs. Derived on every read
    /// rather than pushed into the chat, so adding a workspace or renaming a
    /// machine reaches it without anyone remembering to say so.
    pub fn machine_candidates_for(&self, path: &str) -> Vec<(String, String)> {
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

    /// The sessions the sidebar shows.
    ///
    /// Remote machines list everything — their disk is the only source of
    /// truth and there is nothing client-side to scope by. The local machine
    /// is different: the daemon would surface every repo this computer ever
    /// ran pi in, so its list is scoped to the added workspaces. The full
    /// list stays in memory (`list_sessions` runs once per connect); adding
    /// a workspace reveals its sessions without a refetch.
    pub fn visible_sessions(&self) -> Vec<&MachineSession> {
        self.sessions
            .iter()
            .filter(|session| self.session_visible(session))
            .collect()
    }

    fn session_visible(&self, session: &MachineSession) -> bool {
        if session.machine != LOCAL_MACHINE {
            return true;
        }

        self.workspaces.iter().any(|workspace| {
            workspace.machine == LOCAL_MACHINE && workspace.path == session.info.cwd
        })
    }

    /// Where a session runs, as the sidebar knows it. Present by the time a
    /// chat asks: a start inserts its row before `Attached` lands, and an open
    /// can only have come from a row that was already there.
    pub fn session_cwd(&self, sid: &str) -> Option<&str> {
        self.sessions
            .iter()
            .find(|session| session.info.sid == sid)
            .map(|session| session.info.cwd.as_str())
    }

    /// The session with an `Open` in flight, if any — the sidebar's spinner.
    pub fn opening_sid(&self) -> Option<&str> {
        self.opening.as_ref().map(|opening| opening.sid.as_str())
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
