use crate::catalog::Catalog;
use crate::session_state::{ChatTarget, PI_SET_MODEL};
use crate::workspaces::Workspace;
use gpui::{Context, EventEmitter};
use harvester_client_core::bridge::{Command, Event as BridgeEvent};
use harvester_client_core::machines;
use harvester_client_core::{
    BridgeEvents, Fleet, Host, MachineCandidate, MachineState, MachinesEvents, RequestTokens,
};

pub(crate) use crate::catalog::MachineSession;

pub(crate) enum StoreEvent {
    Bridge { machine: String, event: BridgeEvent },
    Machines(machines::Event),
    Failed(String),
}

pub(crate) struct FleetStore {
    argv_machine: Option<String>,
    fleet: Fleet,
    tokens: RequestTokens,
    catalog: Catalog,
}

impl EventEmitter<StoreEvent> for FleetStore {}

impl FleetStore {
    pub(crate) fn new(
        argv_machine: Option<String>,
        daemon_binary: Vec<u8>,
        cx: &mut Context<'_, Self>,
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

    fn pump_bridge_events(mut events: BridgeEvents, cx: &mut Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update(cx, |store, cx| {
                        let (machine, event) = store.fleet.observe_bridge_event(unapplied);

                        store.handle_bridge_event(machine, event, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn pump_machines_events(mut events: MachinesEvents, cx: &mut Context<'_, Self>) {
        cx.spawn(async move |this, cx| {
            while let Some(unapplied) = events.next().await {
                if this
                    .update(cx, |store, cx| {
                        let event = store.fleet.observe_machines_event(unapplied);

                        store.handle_machines_event(event, cx);
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
    }

    fn handle_bridge_event(
        &mut self,
        machine: String,
        event: BridgeEvent,
        cx: &mut Context<'_, Self>,
    ) {
        if let BridgeEvent::Sessions(sessions) = event {
            self.catalog.record_sessions(&machine, sessions);
            cx.notify();

            return;
        }

        self.apply_bridge_event(&machine, &event, cx);
        cx.emit(StoreEvent::Bridge { machine, event });
    }

    fn apply_bridge_event(
        &mut self,
        machine: &str,
        event: &BridgeEvent,
        cx: &mut Context<'_, Self>,
    ) {
        match event {
            BridgeEvent::Connected => cx.notify(),
            BridgeEvent::ConnectFailed { .. } | BridgeEvent::Disconnected => {
                self.handle_link_lost(machine, cx);
            }
            BridgeEvent::RequestFailed { detail } => {
                self.handle_request_failed(machine, detail, cx);
            }
            BridgeEvent::Attached { sid, .. } => self.handle_session_attached(machine, sid, cx),
            BridgeEvent::SessionExit { sid, .. } => self.handle_session_exited(sid, cx),
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

    fn handle_link_lost(&mut self, machine: &str, cx: &mut Context<'_, Self>) {
        self.catalog.abandon_open_on(machine);
        cx.notify();
    }

    fn handle_request_failed(&mut self, machine: &str, detail: &str, cx: &mut Context<'_, Self>) {
        self.catalog.abandon_open_on(machine);
        self.fail(format!("{machine}: {detail}"), cx);
        cx.notify();
    }

    fn handle_session_attached(&mut self, machine: &str, sid: &str, cx: &mut Context<'_, Self>) {
        self.catalog.session_attached(machine, sid);
        cx.notify();
    }

    fn handle_session_exited(&mut self, sid: &str, cx: &mut Context<'_, Self>) {
        self.catalog.session_exited(sid);
        cx.notify();
    }

    fn handle_machines_event(&mut self, event: machines::Event, cx: &mut Context<'_, Self>) {
        match event {
            machines::Event::Listed { .. } | machines::Event::Probed { .. } => cx.notify(),
            machines::Event::ProviderFailed { provider, detail } => {
                self.fail(format!("provider {provider}: {detail}"), cx);
            }
            answer => cx.emit(StoreEvent::Machines(answer)),
        }
    }

    fn fail(&mut self, detail: String, cx: &mut Context<'_, Self>) {
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

    pub(crate) fn ensure_connection(&mut self, machine: &str, cx: &mut Context<'_, Self>) -> bool {
        match self.fleet.ensure_connection(machine) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), cx);

                false
            }
        }
    }

    pub(crate) fn send(&self, machine: &str, command: Command) {
        self.fleet.send(machine, command);
    }

    pub(crate) fn send_gated(
        &mut self,
        machine: &str,
        command: Command,
        cx: &mut Context<'_, Self>,
    ) -> bool {
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

    fn raise_open_guard_for(
        &mut self,
        machine: &str,
        command: &Command,
        cx: &mut Context<'_, Self>,
    ) {
        let Command::StartSession { sid, .. } = command else {
            return;
        };

        self.begin_start(machine.to_string(), sid.clone(), cx);
    }

    pub(crate) fn send_host_command(
        &mut self,
        command: machines::Command,
        cx: &mut Context<'_, Self>,
    ) -> bool {
        match self.fleet.send_host_command(command) {
            Ok(()) => true,
            Err(error) => {
                self.fail(error.to_string(), cx);

                false
            }
        }
    }

    pub(crate) fn add_workspace(
        &mut self,
        machine: String,
        path: String,
        cx: &mut Context<'_, Self>,
    ) {
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

    pub(crate) fn forget_machine(&mut self, machine: &str, cx: &mut Context<'_, Self>) -> bool {
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

    pub(crate) fn begin_start(&mut self, machine: String, sid: String, cx: &mut Context<'_, Self>) {
        self.catalog.begin_open(machine, sid);
        cx.notify();
    }

    pub(crate) fn may_open_session(&mut self, sid: &str) -> bool {
        self.catalog.may_open_session(sid)
    }

    pub(crate) fn open_session(&mut self, sid: &str, cx: &mut Context<'_, Self>) {
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

    pub(crate) fn insert_started_session(
        &mut self,
        machine: String,
        sid: String,
        name: String,
        cwd: String,
        cx: &mut Context<'_, Self>,
    ) {
        self.catalog.insert_started_session(machine, sid, name, cwd);
        cx.notify();
    }

    pub(crate) fn argv_machine(&self) -> Option<&str> {
        self.argv_machine.as_deref()
    }

    pub(crate) const fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub(crate) fn tokens(&self) -> RequestTokens {
        self.tokens.clone()
    }

    pub(crate) fn label(&self, machine: &str) -> String {
        self.fleet.label(machine)
    }

    pub(crate) fn state(&self, machine: &str) -> MachineState<'_> {
        self.fleet.state(machine)
    }

    pub(crate) fn is_live(&self, machine: &str) -> bool {
        self.fleet.state(machine).is_live()
    }

    pub(crate) fn host_for(&self, machine: &str) -> harvester_client_core::Result<Host> {
        self.fleet.host_for(machine)
    }

    pub(crate) fn provider_machines(&self) -> &[machines::MachineRow] {
        self.fleet.provider_machines()
    }

    pub(crate) fn missing_tracked_machines(&self) -> Vec<String> {
        self.fleet.missing_tracked_machines()
    }

    pub(crate) fn ad_hoc_machines(&self) -> Vec<String> {
        self.fleet.ad_hoc_machines()
    }

    pub(crate) fn machine_candidates(&self) -> Vec<MachineCandidate> {
        self.fleet.machine_candidates()
    }

    pub(crate) fn connected_machine_count(&self) -> usize {
        self.fleet.connected_machine_count()
    }

    pub(crate) fn linked_machine_count(&self) -> usize {
        self.fleet.linked_machine_count()
    }

    pub(crate) fn new_session_target(&self) -> ChatTarget {
        let machines: Vec<String> = self
            .fleet
            .machine_candidates()
            .into_iter()
            .map(|candidate| candidate.key)
            .collect();

        self.catalog.default_new_session_target(&machines)
    }

    pub(crate) fn clear_selected_session(&mut self, cx: &mut Context<'_, Self>) {
        self.catalog.clear_selected_session();
        cx.notify();
    }
}

const fn is_pre_session_command(command: &Command) -> bool {
    matches!(
        command,
        Command::ListModels { .. } | Command::StartSession { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use harvester_client_core::{DialogAnswer, Pending, RequestToken};
    use serde_json::json;

    fn token() -> RequestToken {
        Pending::default().ask(&RequestTokens::default(), ())
    }

    fn every_command() -> Vec<Command> {
        vec![
            Command::Open {
                sid: "01H".to_string(),
                cwd: "/work/repo".to_string(),
            },
            Command::Prompt {
                sid: "01H".to_string(),
                text: "fix it".to_string(),
            },
            Command::StartSession {
                sid: "01H".to_string(),
                cwd: "/work/repo".to_string(),
                provider: None,
                model: None,
                name: "fix it".to_string(),
                first_prompt: "fix it".to_string(),
            },
            Command::ListModels {
                cwd: "/work/repo".to_string(),
                token: token(),
            },
            Command::PiRequest {
                sid: "01H".to_string(),
                cmd: json!({"type": "get_state"}),
                token: token(),
            },
            Command::Stop {
                sid: "01H".to_string(),
            },
            Command::AnswerDialog {
                sid: "01H".to_string(),
                dialog_id: "dialog-1".to_string(),
                answer: DialogAnswer::Cancelled,
            },
        ]
    }

    #[test]
    fn only_the_two_commands_that_precede_a_session_may_bring_a_machine_up() {
        let pre_session: Vec<bool> = every_command().iter().map(is_pre_session_command).collect();

        assert_eq!(
            pre_session,
            [false, false, true, true, false, false, false],
            "a command naming a running session needs a live link, not a connect attempt"
        );
    }

    #[test]
    fn a_start_is_pre_session_whatever_it_launches_with() {
        let with_model = Command::StartSession {
            sid: "01H".to_string(),
            cwd: "/work/repo".to_string(),
            provider: Some("anthropic".to_string()),
            model: Some("claude".to_string()),
            name: "fix it".to_string(),
            first_prompt: "fix it".to_string(),
        };

        assert!(is_pre_session_command(&with_model));
    }
}
