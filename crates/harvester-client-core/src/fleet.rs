use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::bridge::{self, BridgeHandle};
use crate::host::Host;
use crate::machine::{MachineRef, LOCAL_MACHINE};
use crate::machines::{self, MachineRow};
use crate::ssh::SshTarget;

enum Machine {
    Probed { reachable: bool },
    Linked { bridge: BridgeHandle, link: Link },
}

enum Link {
    Connecting,
    Connected,
    Failed { detail: String },
    Lost,
}

impl Machine {
    fn record_probe(&mut self, reachable: bool) {
        if let Machine::Probed { .. } = self {
            *self = Machine::Probed { reachable };
        }
    }

    fn state(&self) -> MachineState<'_> {
        match self {
            Machine::Probed { reachable: true } => MachineState::Reachable,
            Machine::Probed { reachable: false } => MachineState::Unreachable,
            Machine::Linked { link, .. } => link.state(),
        }
    }
}

impl Link {
    fn state(&self) -> MachineState<'_> {
        match self {
            Link::Connecting => MachineState::Connecting,
            Link::Connected => MachineState::Connected,
            Link::Failed { detail } => MachineState::ConnectFailed { detail },
            Link::Lost => MachineState::ConnectionLost,
        }
    }
}

#[derive(Debug)]
pub enum MachineState<'a> {
    Missing,
    Unknown,
    Reachable,
    Unreachable,
    Connecting,
    Connected,
    ConnectFailed { detail: &'a str },
    ConnectionLost,
}

impl MachineState<'_> {
    pub fn is_live(&self) -> bool {
        matches!(self, MachineState::Connecting | MachineState::Connected)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCandidate {
    pub key: String,
    pub label: String,
}

#[derive(Debug)]
pub enum FleetError {
    NotConnectable { machine: String },
    BridgeStart { machine: String, detail: String },
    HostWorkerMissing,
    HostWorkerStopped,
}

impl std::fmt::Display for FleetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FleetError::NotConnectable { machine } => {
                write!(f, "machine {machine} is not connectable")
            }
            FleetError::BridgeStart { machine, detail } => {
                write!(f, "start bridge for {machine}: {detail}")
            }
            FleetError::HostWorkerMissing => write!(f, "machines worker is not running"),
            FleetError::HostWorkerStopped => write!(f, "machines worker stopped"),
        }
    }
}

impl std::error::Error for FleetError {}

pub struct FleetEvents {
    pub bridges: BridgeEvents,
    pub machines: Option<MachinesEvents>,
}

pub struct Unapplied<T>(T);

pub struct BridgeEvents(UnboundedReceiver<(String, bridge::Event)>);

impl BridgeEvents {
    pub async fn next(&mut self) -> Option<Unapplied<(String, bridge::Event)>> {
        self.0.recv().await.map(Unapplied)
    }
}

pub struct MachinesEvents(UnboundedReceiver<machines::Event>);

impl MachinesEvents {
    pub async fn next(&mut self) -> Option<Unapplied<machines::Event>> {
        self.0.recv().await.map(Unapplied)
    }
}

pub struct Fleet {
    daemon_binary: Arc<Vec<u8>>,
    bridge_events: UnboundedSender<(String, bridge::Event)>,
    machines: HashMap<String, Machine>,
    provider_rows: Vec<MachineRow>,
    listed_providers: HashSet<String>,
    host_commands: Option<UnboundedSender<machines::Command>>,
    tracked: Vec<String>,
}

impl Fleet {
    pub fn start(daemon_binary: Vec<u8>) -> (Fleet, FleetEvents) {
        let (bridge_sender, bridge_receiver) = unbounded_channel();
        let worker = match machines::start() {
            Ok(worker) => Some(worker),
            Err(error) => {
                eprintln!("fleet: start machines worker: {error}");

                None
            }
        };
        let (host_commands, machine_events) = match worker {
            Some(worker) => (Some(worker.commands), Some(worker.events)),
            None => (None, None),
        };
        let fleet = Fleet {
            daemon_binary: Arc::new(daemon_binary),
            bridge_events: bridge_sender,
            machines: HashMap::new(),
            provider_rows: Vec::new(),
            listed_providers: HashSet::new(),
            host_commands,
            tracked: Vec::new(),
        };
        let events = FleetEvents {
            bridges: BridgeEvents(bridge_receiver),
            machines: machine_events.map(MachinesEvents),
        };

        (fleet, events)
    }

    pub fn track_machines(&mut self, machines: Vec<String>) {
        let mut tracked = machines;

        tracked.sort();
        tracked.dedup();

        self.tracked = tracked;
        self.forget_unknown_machines();
        self.publish_ad_hoc_machines();
        self.auto_connect_tracked();
    }

    fn publish_ad_hoc_machines(&self) {
        if let Some(commands) = &self.host_commands {
            let _ = commands.send(machines::Command::SetAdHocMachines(self.ad_hoc_machines()));
        }
    }

    fn forget_unknown_machines(&mut self) {
        let Fleet {
            machines,
            tracked,
            provider_rows,
            ..
        } = self;

        machines.retain(|key, _| {
            tracked.contains(key) || provider_rows.iter().any(|row| &row.key == key)
        });
    }

    fn auto_connect_tracked(&mut self) {
        for machine in self.connectable_tracked_machines() {
            if let Err(error) = self.ensure_connection(&machine) {
                eprintln!("fleet: auto-connect {machine}: {error}");
            }
        }
    }

    fn connectable_tracked_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter(|machine| !self.is_linked(machine))
            .filter(|machine| self.host_for(machine).is_ok())
            .cloned()
            .collect()
    }

    pub fn ensure_connection(&mut self, machine: &str) -> Result<(), FleetError> {
        match self.connect_decision(machine)? {
            ConnectDecision::Reuse => Ok(()),
            ConnectDecision::Spawn(host) => self.spawn_bridge(machine, host),
        }
    }

    fn connect_decision(&self, machine: &str) -> Result<ConnectDecision, FleetError> {
        if self.state(machine).is_live() {
            return Ok(ConnectDecision::Reuse);
        }

        Ok(ConnectDecision::Spawn(self.host_for(machine)?))
    }

    fn spawn_bridge(&mut self, machine: &str, host: Host) -> Result<(), FleetError> {
        let started = bridge::start(
            machine.to_string(),
            host,
            self.daemon_binary.clone(),
            self.bridge_events.clone(),
        );
        let handle = match started {
            Ok(handle) => handle,
            Err(error) => {
                return Err(FleetError::BridgeStart {
                    machine: machine.to_string(),
                    detail: error.to_string(),
                })
            }
        };

        self.machines.insert(
            machine.to_string(),
            Machine::Linked {
                bridge: handle,
                link: Link::Connecting,
            },
        );

        Ok(())
    }

    pub fn send(&self, machine: &str, command: bridge::Command) {
        let Some(Machine::Linked { bridge, .. }) = self.machines.get(machine) else {
            eprintln!("fleet: dropping a command for {machine}, which has no bridge");

            return;
        };

        if bridge.commands.send(command).is_err() {
            eprintln!("fleet: dropping a command for {machine}: its bridge thread has stopped");
        }
    }

    pub fn send_host_command(&self, command: machines::Command) -> Result<(), FleetError> {
        let Some(commands) = &self.host_commands else {
            return Err(FleetError::HostWorkerMissing);
        };

        if commands.send(command).is_err() {
            return Err(FleetError::HostWorkerStopped);
        }

        Ok(())
    }

    pub fn observe_bridge_event(
        &mut self,
        event: Unapplied<(String, bridge::Event)>,
    ) -> (String, bridge::Event) {
        let Unapplied((machine, event)) = event;

        self.record_bridge_event(&machine, &event);

        (machine, event)
    }

    pub fn observe_machines_event(&mut self, event: Unapplied<machines::Event>) -> machines::Event {
        let Unapplied(event) = event;

        self.record_machines_event(&event);

        event
    }

    fn record_bridge_event(&mut self, machine: &str, event: &bridge::Event) {
        match event {
            bridge::Event::Connected => self.set_link(machine, Link::Connected),
            bridge::Event::ConnectFailed { detail } => self.set_link(
                machine,
                Link::Failed {
                    detail: detail.clone(),
                },
            ),
            bridge::Event::Disconnected => self.set_link(machine, Link::Lost),
            _ => {}
        }
    }

    fn set_link(&mut self, machine: &str, link: Link) {
        let Some(Machine::Linked { link: current, .. }) = self.machines.get_mut(machine) else {
            eprintln!("fleet: link event for {machine}, which has no bridge");

            return;
        };

        *current = link;
    }

    fn record_machines_event(&mut self, event: &machines::Event) {
        match event {
            machines::Event::Listed {
                rows,
                listed_providers,
            } => self.handle_listed(rows, listed_providers),
            machines::Event::Probed { key, reachable } => self.handle_probed(key, *reachable),
            machines::Event::ProviderFailed { .. }
            | machines::Event::HomeResolved { .. }
            | machines::Event::DirListed { .. }
            | machines::Event::WorkspaceValidated { .. }
            | machines::Event::BranchResolved { .. } => {}
        }
    }

    fn handle_listed(&mut self, rows: &[MachineRow], listed_providers: &[String]) {
        let mut rows = rows.to_vec();

        rows.sort_by(|a, b| a.key.cmp(&b.key));
        self.provider_rows = rows;
        self.listed_providers = listed_providers.iter().cloned().collect();
        self.forget_unknown_machines();
        self.auto_connect_tracked();
    }

    fn handle_probed(&mut self, key: &str, reachable: bool) {
        match self.machines.get_mut(key) {
            Some(machine) => machine.record_probe(reachable),
            None => {
                self.machines
                    .insert(key.to_string(), Machine::Probed { reachable });
            }
        }
    }

    pub fn state(&self, machine: &str) -> MachineState<'_> {
        let state = self
            .machines
            .get(machine)
            .map_or(MachineState::Unknown, Machine::state);

        if state.is_live() || !self.is_missing(machine) {
            return state;
        }

        MachineState::Missing
    }

    fn is_missing(&self, machine: &str) -> bool {
        let MachineRef::Provider(key) = MachineRef::parse(machine) else {
            return false;
        };

        self.listed_providers.contains(&key.provider) && self.provider_row(machine).is_none()
    }

    pub fn missing_tracked_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter(|machine| self.is_missing(machine))
            .cloned()
            .collect()
    }

    fn is_linked(&self, machine: &str) -> bool {
        matches!(self.machines.get(machine), Some(Machine::Linked { .. }))
    }

    pub fn host_for(&self, machine: &str) -> Result<Host, FleetError> {
        let host = match MachineRef::parse(machine) {
            MachineRef::Local => Some(Host::Local),
            MachineRef::Provider(_) => self
                .provider_row(machine)
                .and_then(|row| row.target.clone())
                .map(Host::Ssh),
            MachineRef::AdHoc(dest) => Some(Host::Ssh(SshTarget { dest, port: None })),
        };

        host.ok_or_else(|| FleetError::NotConnectable {
            machine: machine.to_string(),
        })
    }

    pub fn label(&self, machine: &str) -> String {
        match MachineRef::parse(machine) {
            MachineRef::Provider(key) => self
                .provider_row(machine)
                .map(|row| row.label.clone())
                .unwrap_or(key.id),
            MachineRef::Local | MachineRef::AdHoc(_) => machine.to_string(),
        }
    }

    fn provider_row(&self, machine: &str) -> Option<&MachineRow> {
        self.provider_rows.iter().find(|row| row.key == machine)
    }

    pub fn provider_machines(&self) -> &[MachineRow] {
        &self.provider_rows
    }

    pub fn ad_hoc_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter_map(|machine| match MachineRef::parse(machine) {
                MachineRef::Local | MachineRef::Provider(_) => None,
                MachineRef::AdHoc(dest) => Some(dest),
            })
            .collect()
    }

    pub fn machine_candidates(&self) -> Vec<MachineCandidate> {
        std::iter::once(LOCAL_MACHINE.to_string())
            .chain(self.provider_rows.iter().map(|row| row.key.clone()))
            .chain(self.ad_hoc_machines())
            .filter(|machine| self.host_for(machine).is_ok())
            .map(|machine| MachineCandidate {
                label: self.label(&machine),
                key: machine,
            })
            .collect()
    }

    pub fn linked_machine_count(&self) -> usize {
        self.machines
            .values()
            .filter(|machine| matches!(machine, Machine::Linked { .. }))
            .count()
    }

    pub fn connected_machine_count(&self) -> usize {
        self.machines
            .values()
            .filter(|machine| {
                matches!(
                    machine,
                    Machine::Linked {
                        link: Link::Connected,
                        ..
                    }
                )
            })
            .count()
    }
}

#[derive(Debug)]
enum ConnectDecision {
    Reuse,
    Spawn(Host),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet() -> Fleet {
        let (bridge_events, _events) = unbounded_channel();

        Fleet {
            daemon_binary: Arc::new(Vec::new()),
            bridge_events,
            machines: HashMap::new(),
            provider_rows: Vec::new(),
            listed_providers: HashSet::new(),
            host_commands: None,
            tracked: Vec::new(),
        }
    }

    fn list(fleet: &mut Fleet, rows: &[MachineRow]) {
        let mut providers: Vec<String> = rows.iter().map(|row| row.provider.clone()).collect();

        providers.sort();
        providers.dedup();
        fleet.handle_listed(rows, &providers);
    }

    fn provider_row(key: &str, target: Option<&str>) -> MachineRow {
        MachineRow {
            key: key.to_string(),
            label: format!("{key} label"),
            provider: key.split('/').next().unwrap_or(key).to_string(),
            target: target.map(|dest| SshTarget {
                dest: dest.to_string(),
                port: None,
            }),
        }
    }

    fn with_link(fleet: &mut Fleet, machine: &str, link: Link) {
        fleet.machines.insert(
            machine.to_string(),
            Machine::Linked {
                bridge: BridgeHandle {
                    commands: unbounded_channel().0,
                },
                link,
            },
        );
    }

    #[test]
    fn a_probe_verdict_is_the_state_when_nothing_is_linked() {
        let mut fleet = fleet();

        fleet.handle_probed("exedev@vm", true);

        assert!(matches!(fleet.state("exedev@vm"), MachineState::Reachable));

        fleet.handle_probed("exedev@vm", false);

        assert!(matches!(
            fleet.state("exedev@vm"),
            MachineState::Unreachable
        ));
    }

    #[test]
    fn a_probe_never_overwrites_a_daemon_link() {
        let mut fleet = fleet();

        for linked in [
            Link::Connecting,
            Link::Connected,
            Link::Failed {
                detail: "no route to host".to_string(),
            },
            Link::Lost,
        ] {
            with_link(&mut fleet, "vm", linked);
            fleet.handle_probed("vm", true);
            fleet.handle_probed("vm", false);

            assert!(
                fleet.is_linked("vm"),
                "a probe demoted a machine that has a bridge"
            );
        }
    }

    #[test]
    fn an_unknown_machine_has_a_state_to_render() {
        assert!(matches!(
            fleet().state("never-heard-of-it"),
            MachineState::Unknown
        ));
    }

    #[test]
    fn a_connect_failure_keeps_its_reason() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connecting);
        fleet.record_bridge_event(
            "vm",
            &bridge::Event::ConnectFailed {
                detail: "daemon connect: handshake timed out".to_string(),
            },
        );

        let MachineState::ConnectFailed { detail } = fleet.state("vm") else {
            panic!("the link state should be ConnectFailed");
        };

        assert_eq!(detail, "daemon connect: handshake timed out");
    }

    #[test]
    fn observing_an_event_is_what_applies_it() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connecting);

        let (machine, event) =
            fleet.observe_bridge_event(Unapplied(("vm".to_string(), bridge::Event::Disconnected)));

        assert_eq!(machine, "vm");
        assert!(
            matches!(event, bridge::Event::Disconnected),
            "the frontend still gets its event"
        );
        assert!(matches!(fleet.state("vm"), MachineState::ConnectionLost));
    }

    #[test]
    fn session_traffic_does_not_touch_the_link() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connecting);
        fleet.record_bridge_event(
            "vm",
            &bridge::Event::RequestFailed {
                detail: "attach: no such session".to_string(),
            },
        );

        assert!(
            matches!(fleet.state("vm"), MachineState::Connecting),
            "a failed request says nothing about the connection"
        );
    }

    #[test]
    fn a_live_bridge_is_reused() {
        let mut fleet = fleet();

        for live in [Link::Connecting, Link::Connected] {
            with_link(&mut fleet, "vm", live);

            assert!(matches!(
                fleet.connect_decision("vm"),
                Ok(ConnectDecision::Reuse)
            ));
        }
    }

    #[test]
    fn a_failed_or_lost_bridge_is_respawned() {
        let mut fleet = fleet();

        for dead in [
            Link::Failed {
                detail: "boom".to_string(),
            },
            Link::Lost,
        ] {
            with_link(&mut fleet, "vm", dead);

            assert!(matches!(
                fleet.connect_decision("vm"),
                Ok(ConnectDecision::Spawn(_))
            ));
        }
    }

    #[test]
    fn an_unreachable_probe_does_not_veto_connecting() {
        let mut fleet = fleet();

        fleet.handle_probed("exedev@vm", false);

        assert!(matches!(
            fleet.connect_decision("exedev@vm"),
            Ok(ConnectDecision::Spawn(_))
        ));
    }

    #[test]
    fn an_unlisted_provider_machine_is_not_connectable() {
        let fleet = fleet();
        let error = fleet
            .connect_decision("exe-dev/bloggy")
            .expect_err("no listing has resolved this key");

        assert_eq!(
            error.to_string(),
            "machine exe-dev/bloggy is not connectable"
        );
    }

    #[test]
    fn a_listed_provider_machine_resolves_to_its_target() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row(
                "exe-dev/bloggy",
                Some("exedev@bloggy.exe.dev"),
            )],
        );

        let Ok(ConnectDecision::Spawn(Host::Ssh(target))) =
            fleet.connect_decision("exe-dev/bloggy")
        else {
            panic!("a listed machine with a target resolves");
        };

        assert_eq!(target.dest, "exedev@bloggy.exe.dev");
    }

    #[test]
    fn a_listed_provider_machine_without_a_target_stays_unconnectable() {
        let mut fleet = fleet();

        list(&mut fleet, &[provider_row("exe-dev/legacy", None)]);

        assert!(fleet.connect_decision("exe-dev/legacy").is_err());
    }

    #[test]
    fn the_local_machine_is_always_connectable() {
        assert!(matches!(
            fleet().connect_decision(LOCAL_MACHINE),
            Ok(ConnectDecision::Spawn(Host::Local))
        ));
    }

    #[test]
    fn only_ad_hoc_machines_join_the_probe_set() {
        let mut fleet = fleet();

        fleet.tracked = vec![
            "exe-dev/bloggy".to_string(),
            LOCAL_MACHINE.to_string(),
            "exedev@vm.exe.dev".to_string(),
        ];

        assert_eq!(
            fleet.ad_hoc_machines(),
            vec!["exedev@vm.exe.dev".to_string()],
            "`local` has no SSH to probe and provider machines probe off the listing"
        );
    }

    #[test]
    fn auto_connect_skips_machines_that_cannot_resolve_yet() {
        let mut fleet = fleet();

        fleet.tracked = vec!["exe-dev/bloggy".to_string(), LOCAL_MACHINE.to_string()];

        assert_eq!(
            fleet.connectable_tracked_machines(),
            vec![LOCAL_MACHINE.to_string()],
            "an unlisted provider machine is skipped quietly, not reported"
        );

        fleet.provider_rows = vec![provider_row("exe-dev/bloggy", Some("exedev@bloggy"))];

        assert_eq!(
            fleet.connectable_tracked_machines(),
            vec!["exe-dev/bloggy".to_string(), LOCAL_MACHINE.to_string()],
            "the listing is what makes a provider machine auto-connectable"
        );
    }

    #[test]
    fn auto_connect_does_not_retry_a_machine_that_already_has_a_bridge() {
        let mut fleet = fleet();

        fleet.tracked = vec![LOCAL_MACHINE.to_string()];
        with_link(
            &mut fleet,
            LOCAL_MACHINE,
            Link::Failed {
                detail: "boom".to_string(),
            },
        );

        assert!(fleet.connectable_tracked_machines().is_empty());
    }

    #[test]
    fn a_probed_machine_is_still_auto_connectable() {
        let mut fleet = fleet();

        fleet.tracked = vec!["exedev@vm".to_string()];
        fleet.handle_probed("exedev@vm", true);

        assert_eq!(
            fleet.connectable_tracked_machines(),
            vec!["exedev@vm".to_string()]
        );
    }

    #[test]
    fn tracking_is_deduplicated_and_ordered() {
        let mut fleet = fleet();

        fleet.track_machines(vec![
            "exedev@b".to_string(),
            "exedev@a".to_string(),
            "exedev@b".to_string(),
        ]);

        assert_eq!(
            fleet.ad_hoc_machines(),
            ["exedev@a".to_string(), "exedev@b".to_string()],
            "one row per machine, stable order — several workspaces may share one"
        );
    }

    #[test]
    fn a_machine_nobody_tracks_or_lists_any_more_is_forgotten() {
        let mut fleet = fleet();

        with_link(&mut fleet, "exedev@vm", Link::Connecting);
        with_link(&mut fleet, "exe-dev/bloggy", Link::Connecting);
        fleet.tracked = vec!["exedev@vm".to_string()];
        fleet.provider_rows = vec![provider_row("exe-dev/bloggy", Some("exedev@bloggy"))];

        fleet.forget_unknown_machines();

        assert_eq!(
            fleet.linked_machine_count(),
            2,
            "tracked and listed are both still known"
        );

        fleet.tracked = Vec::new();
        fleet.provider_rows = Vec::new();
        fleet.forget_unknown_machines();

        assert_eq!(fleet.linked_machine_count(), 0);
        assert!(matches!(fleet.state("exedev@vm"), MachineState::Unknown));
    }

    #[test]
    fn candidates_are_local_plus_resolvable_machines() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[
                provider_row("exe-dev/bloggy", Some("exedev@bloggy")),
                provider_row("exe-dev/legacy", None),
            ],
        );
        fleet.track_machines(vec!["exedev@vm".to_string()]);

        assert_eq!(
            fleet.machine_candidates(),
            vec![
                MachineCandidate {
                    key: LOCAL_MACHINE.to_string(),
                    label: LOCAL_MACHINE.to_string(),
                },
                MachineCandidate {
                    key: "exe-dev/bloggy".to_string(),
                    label: "exe-dev/bloggy label".to_string(),
                },
                MachineCandidate {
                    key: "exedev@vm".to_string(),
                    label: "exedev@vm".to_string(),
                },
            ],
            "a provider machine with no target cannot host a workspace"
        );
    }

    #[test]
    fn a_machine_its_provider_stopped_listing_is_missing() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row("exe-dev/gone", Some("exedev@gone"))],
        );
        list(
            &mut fleet,
            &[provider_row("exe-dev/kept", Some("exedev@kept"))],
        );

        assert!(matches!(fleet.state("exe-dev/gone"), MachineState::Missing));
    }

    #[test]
    fn a_machine_whose_provider_never_listed_is_only_unknown() {
        let fleet = fleet();

        assert!(matches!(
            fleet.state("exe-dev/bloggy"),
            MachineState::Unknown
        ));
    }

    #[test]
    fn only_provider_machines_can_go_missing() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row("exe-dev/kept", Some("exedev@kept"))],
        );

        assert!(matches!(fleet.state(LOCAL_MACHINE), MachineState::Unknown));
        assert!(matches!(fleet.state("exedev@vm"), MachineState::Unknown));
    }

    #[test]
    fn a_live_bridge_outranks_a_listing_that_dropped_the_machine() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row("exe-dev/kept", Some("exedev@kept"))],
        );
        with_link(&mut fleet, "exe-dev/gone", Link::Connected);

        assert!(matches!(
            fleet.state("exe-dev/gone"),
            MachineState::Connected { .. }
        ));
    }

    #[test]
    fn a_stale_probe_verdict_does_not_outrank_a_listing() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row("exe-dev/kept", Some("exedev@kept"))],
        );
        fleet.handle_probed("exe-dev/gone", true);

        assert!(matches!(fleet.state("exe-dev/gone"), MachineState::Missing));
    }

    #[test]
    fn missing_tracked_machines_are_the_tracked_ones_only() {
        let mut fleet = fleet();

        list(
            &mut fleet,
            &[provider_row("exe-dev/kept", Some("exedev@kept"))],
        );
        fleet.track_machines(vec!["exe-dev/gone".to_string()]);

        assert_eq!(fleet.missing_tracked_machines(), vec!["exe-dev/gone"]);
    }

    #[test]
    fn a_label_falls_back_to_the_key_id_until_the_provider_lists() {
        let mut fleet = fleet();

        assert_eq!(fleet.label("exe-dev/bloggy"), "bloggy");

        list(
            &mut fleet,
            &[provider_row("exe-dev/bloggy", Some("exedev@bloggy"))],
        );

        assert_eq!(fleet.label("exe-dev/bloggy"), "exe-dev/bloggy label");
        assert_eq!(fleet.label("exedev@vm"), "exedev@vm");
        assert_eq!(fleet.label(LOCAL_MACHINE), LOCAL_MACHINE);
    }

    #[test]
    fn only_machines_with_a_bridge_are_counted() {
        let mut fleet = fleet();

        fleet.handle_probed("exedev@probed-only", true);
        with_link(&mut fleet, "connecting", Link::Connecting);
        with_link(&mut fleet, "connected", Link::Connected);

        assert_eq!(fleet.connected_machine_count(), 1);
        assert_eq!(
            fleet.linked_machine_count(),
            2,
            "a probe is not a connection attempt"
        );
    }

    #[test]
    fn a_host_op_without_a_worker_says_so() {
        let error = fleet()
            .send_host_command(machines::Command::SetAdHocMachines(Vec::new()))
            .expect_err("this fleet has no worker");

        assert_eq!(error.to_string(), "machines worker is not running");
    }
}
