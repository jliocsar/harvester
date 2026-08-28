use crate::catalog::Catalog;
use crate::session_state::{ChatTarget, PI_SET_MODEL};
use crate::workspaces::Workspace;
use gpui::{Context, EventEmitter};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use harvester_client_core::{
    BridgeEvents, Fleet, Host, MachineCandidate, MachineState, MachinesEvents, RequestTokens,
};
use harvester_proto::SessionInfo;

pub use crate::catalog::MachineSession;

pub enum StoreEvent {
    Bridge { machine: String, event: BridgeEvent },
    Machines(machines::Event),
    Failed(String),
}

pub struct FleetStore {
    argv_machine: Option<String>,
    fleet: Fleet,
    tokens: RequestTokens,
    catalog: Catalog,
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
            catalog: Catalog::load(),
        };

        store.track_machines();

        if let Some(machine) = argv_machine {
            store.ensure_connection(&machine, cx);
        }

        store
    }

    fn pump_bridge_events(mut events: BridgeEvents, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update(cx, |store, cx| {
                        let (machine, event) = store.fleet.observe_bridge_event(unapplied);

                        store.handle_bridge_event(machine, event, cx)
                    })
                    .is_err()
                {
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

    fn handle_bridge_event(&mut self, machine: String, event: BridgeEvent, cx: &mut Context<Self>) {
        if let BridgeEvent::Sessions(sessions) = event {
            self.record_sessions(&machine, sessions);
            cx.notify();

            return;
        }

        self.apply_bridge_event(&machine, &event, cx);
        cx.emit(StoreEvent::Bridge { machine, event });
    }

    fn apply_bridge_event(&mut self, machine: &str, event: &BridgeEvent, cx: &mut Context<Self>) {
        match event {
            BridgeEvent::Connected => cx.notify(),
            BridgeEvent::ConnectFailed { .. } | BridgeEvent::Disconnected => {
                self.catalog.abandon_open_on(machine);
                cx.notify();
            }
            BridgeEvent::RequestFailed { detail } => {
                self.catalog.abandon_open_on(machine);
                self.fail(format!("{machine}: {detail}"), cx);
                cx.notify();
            }
            BridgeEvent::Attached { sid, .. } => {
                self.catalog.session_attached(machine, sid);
                cx.notify();
            }
            BridgeEvent::SessionExit { sid, .. } => {
                self.catalog.session_exited(sid);
                cx.notify();
            }
            BridgeEvent::PiAnswered {
                method,
                result: Err(detail),
                ..
            } if method == PI_SET_MODEL => self.fail(format!("{PI_SET_MODEL}: {detail}"), cx),
            BridgeEvent::Sessions(_)
            | BridgeEvent::AttachmentLeft { .. }
            | BridgeEvent::HistoryReplaced { .. }
            | BridgeEvent::HistoryExtended { .. }
            | BridgeEvent::Pi { .. }
            | BridgeEvent::PromptFailed { .. }
            | BridgeEvent::ModelsListed { .. }
            | BridgeEvent::PiAnswered { .. } => {}
        }
    }

    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<Self>) {
        match event {
            machines::Event::Listed { .. } | machines::Event::Probed { .. } => cx.notify(),
            machines::Event::ProviderFailed { provider, detail } => {
                self.fail(format!("provider {provider}: {detail}"), cx)
            }
            answer => cx.emit(StoreEvent::Machines(answer)),
        }
    }

    fn record_sessions(&mut self, machine: &str, sessions: Vec<SessionInfo>) {
        self.catalog.record_sessions(machine, sessions);
    }

    fn fail(&mut self, detail: String, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::Failed(detail));
    }

    fn track_machines(&mut self) {
        let machines: Vec<String> = self
            .argv_machine
            .iter()
            .chain(
                self.catalog
                    .workspaces()
                    .iter()
                    .map(|workspace| &workspace.machine),
            )
            .cloned()
            .collect();

        self.fleet.track_machines(machines);
    }

    pub fn ensure_connection(&mut self, machine: &str, cx: &mut Context<Self>) -> bool {
        match self.fleet.ensure_connection(machine) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), cx);

                false
            }
        }
    }

    pub fn send(&self, machine: &str, command: Command) {
        self.fleet.send(machine, command);
    }

    pub fn send_gated(&mut self, machine: &str, command: Command, cx: &mut Context<Self>) -> bool {
        if let Some(detail) = self.gate_failure(machine, &command) {
            self.fail(detail, cx);

            return false;
        }

        self.raise_open_guard_for(machine, &command, cx);
        self.send(machine, command);

        true
    }

    fn gate_failure(&mut self, machine: &str, command: &Command) -> Option<String> {
        let label = self.label(machine);

        if is_pre_session_command(command) {
            return match self.fleet.ensure_connection(machine) {
                Ok(()) => None,
                Err(error) => Some(format!("{label}: {error} — nothing was sent")),
            };
        }

        if self.is_live(machine) {
            return None;
        }

        Some(format!("{label}: not connected — nothing was sent"))
    }

    fn raise_open_guard_for(&mut self, machine: &str, command: &Command, cx: &mut Context<Self>) {
        let Command::StartSession { sid, .. } = command else {
            return;
        };

        self.begin_start(machine.to_string(), sid.clone(), cx);
    }

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

        let added = match self.catalog.add_workspace(workspace) {
            Ok(added) => added,
            Err(error) => {
                self.fail(format!("save workspaces: {error}"), cx);

                return;
            }
        };

        if !added {
            return;
        }

        self.track_machines();
        cx.notify();
    }

    pub fn forget_machine(&mut self, machine: &str, cx: &mut Context<Self>) -> bool {
        if let Err(error) = self.catalog.forget_machine(machine) {
            self.fail(format!("save workspaces: {error}"), cx);

            return false;
        }

        if self.argv_machine.as_deref() == Some(machine) {
            self.argv_machine = None;
        }

        self.track_machines();
        cx.notify();

        true
    }

    pub fn begin_start(&mut self, machine: String, sid: String, cx: &mut Context<Self>) {
        self.catalog.begin_open(machine, sid);
        cx.notify();
    }

    pub fn may_open_session(&mut self, sid: &str) -> bool {
        self.catalog.may_open_session(sid)
    }

    pub fn open_session(&mut self, sid: &str, cx: &mut Context<Self>) {
        let Some(open) = self.catalog.open_session(sid) else {
            return;
        };

        if !self.ensure_connection(&open.machine, cx) {
            self.catalog.abandon_open_on(&open.machine);

            return;
        }

        self.fleet.send(
            &open.machine,
            Command::Open {
                sid: open.sid,
                cwd: open.cwd,
            },
        );
        cx.notify();
    }

    pub fn insert_started_session(
        &mut self,
        machine: String,
        sid: String,
        name: String,
        cwd: String,
        cx: &mut Context<Self>,
    ) {
        self.catalog.insert_started_session(machine, sid, name, cwd);
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

    pub fn is_live(&self, machine: &str) -> bool {
        self.fleet.state(machine).is_live()
    }

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

    pub fn repos_on(&self, machine: &str) -> Vec<String> {
        self.catalog.repos_on(machine)
    }

    pub fn new_session_target(&self) -> ChatTarget {
        let machines: Vec<String> = self
            .fleet
            .machine_candidates()
            .into_iter()
            .map(|candidate| candidate.key)
            .collect();

        self.catalog.default_new_session_target(&machines)
    }

    pub fn visible_sessions(&self) -> Vec<&MachineSession> {
        self.catalog.visible_sessions()
    }

    pub fn visible_session_count_on(&self, machine: &str) -> usize {
        self.catalog.visible_session_count_on(machine)
    }

    pub fn session_cwd(&self, sid: &str) -> Option<&str> {
        self.catalog.session_cwd(sid)
    }

    pub fn opening_sid(&self) -> Option<&str> {
        self.catalog.opening_sid()
    }

    pub fn is_selected_session(&self, sid: &str) -> bool {
        self.catalog.is_selected_session(sid)
    }

    pub fn clear_selected_session(&mut self, cx: &mut Context<Self>) {
        self.catalog.clear_selected_session();
        cx.notify();
    }
}

fn is_pre_session_command(command: &Command) -> bool {
    matches!(
        command,
        Command::ListModels { .. } | Command::StartSession { .. }
    )
}
