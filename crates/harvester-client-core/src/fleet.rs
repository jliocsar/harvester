use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::bridge::{self, BridgeHandle};
use crate::host::{Host, SshTarget};
use crate::identity::LOCAL_MACHINE;
use crate::machines::{self, MachineRow};
use crate::provider::MachineKey;
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
enum MachineRef {
    Local,
    Provider(MachineKey),
    AdHoc(String),
}

impl MachineRef {
    fn parse(stored: &str) -> Self {
        if stored == LOCAL_MACHINE {
            return Self::Local;
        }

        match MachineKey::parse(stored) {
            Some(key) => Self::Provider(key),
            None => Self::AdHoc(stored.to_string()),
        }
    }
}

enum Machine {
    Probed {
        reachable: bool,
    },
    Linked {
        bridge: BridgeHandle,
        generation: u64,
        link: Link,
    },
}

enum Link {
    Connecting,
    Connected,
    Failed { detail: String },
    Lost,
}

impl Machine {
    fn record_probe(&mut self, reachable: bool) {
        if let Self::Probed { .. } = self {
            *self = Self::Probed { reachable };
        }
    }

    fn state(&self) -> MachineState<'_> {
        match self {
            Self::Probed { reachable: true } => MachineState::Reachable,
            Self::Probed { reachable: false } => MachineState::Unreachable,

            Self::Linked { link, .. } => match link {
                Link::Connecting => MachineState::Connecting,
                Link::Connected => MachineState::Connected,
                Link::Failed { detail } => MachineState::ConnectFailed { detail },
                Link::Lost => MachineState::ConnectionLost,
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MachineState<'a> {
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
    pub(crate) const fn is_live(&self) -> bool {
        matches!(self, MachineState::Connecting | MachineState::Connected)
    }
}

pub(crate) struct FleetEvents {
    pub(crate) bridges: BridgeEvents,
    pub(crate) machines: MachinesEvents,
}

pub(crate) struct Unapplied<T>(T);

pub(crate) struct BridgeEvents(UnboundedReceiver<(String, u64, bridge::Event)>);

impl BridgeEvents {
    pub(crate) async fn next(&mut self) -> Option<Unapplied<(String, u64, bridge::Event)>> {
        self.0.recv().await.map(Unapplied)
    }
}

pub(crate) struct MachinesEvents(UnboundedReceiver<machines::Event>);

impl MachinesEvents {
    pub(crate) async fn next(&mut self) -> Option<Unapplied<machines::Event>> {
        self.0.recv().await.map(Unapplied)
    }
}

pub(crate) struct Fleet {
    daemon_binary: Arc<Vec<u8>>,
    bridge_events: UnboundedSender<(String, u64, bridge::Event)>,
    bridge_generations: HashMap<String, u64>,
    machines: HashMap<String, Machine>,
    provider_rows: Vec<MachineRow>,
    listed_providers: HashSet<String>,
    host_commands: Option<UnboundedSender<machines::Command>>,
    tracked: Vec<String>,
}

impl Fleet {
    #[cfg(test)]
    pub(crate) fn inert() -> Self {
        let (bridge_events, _events) = unbounded_channel();

        Self {
            daemon_binary: Arc::new(Vec::new()),
            bridge_events,
            bridge_generations: HashMap::new(),
            machines: HashMap::new(),
            provider_rows: Vec::new(),
            listed_providers: HashSet::new(),
            host_commands: None,
            tracked: Vec::new(),
        }
    }

    pub(crate) fn start(daemon_binary: Vec<u8>) -> (Self, FleetEvents) {
        let (bridge_sender, bridge_receiver) = unbounded_channel();
        let worker = machines::start();
        let fleet = Self {
            daemon_binary: Arc::new(daemon_binary),
            bridge_events: bridge_sender,
            bridge_generations: HashMap::new(),
            machines: HashMap::new(),
            provider_rows: Vec::new(),
            listed_providers: HashSet::new(),
            host_commands: Some(worker.commands),
            tracked: Vec::new(),
        };
        let events = FleetEvents {
            bridges: BridgeEvents(bridge_receiver),
            machines: MachinesEvents(worker.events),
        };

        (fleet, events)
    }

    pub(crate) fn track_machines(&mut self, machines: Vec<String>) {
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
            drop(commands.send(machines::Command::SetAdHocMachines(self.ad_hoc_machines())));
        }
    }

    fn forget_unknown_machines(&mut self) {
        let Self {
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
        if self.host_commands.is_none() {
            return;
        }

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

    pub(crate) fn ensure_connection(&mut self, machine: &str) -> Result<()> {
        if self.host_commands.is_none() {
            return Err(Error::HostWorkerMissing);
        }

        match self.connect_decision(machine)? {
            ConnectDecision::Reuse => Ok(()),
            ConnectDecision::Spawn(host) => self.spawn_bridge(machine, host),
        }
    }

    fn connect_decision(&self, machine: &str) -> Result<ConnectDecision> {
        if self.state(machine).is_live() {
            return Ok(ConnectDecision::Reuse);
        }

        Ok(ConnectDecision::Spawn(self.host_for(machine)?))
    }

    fn spawn_bridge(&mut self, machine: &str, host: Host) -> Result<()> {
        let generation = self
            .bridge_generations
            .entry(machine.to_string())
            .or_insert(0);

        *generation = generation
            .checked_add(1)
            .expect("bridge generation exhausted");
        let generation = *generation;
        let handle = bridge::start(
            machine.to_string(),
            generation,
            host,
            self.daemon_binary.clone(),
            self.bridge_events.clone(),
        );

        self.machines.insert(
            machine.to_string(),
            Machine::Linked {
                bridge: handle,
                generation,
                link: Link::Connecting,
            },
        );

        Ok(())
    }

    pub(crate) fn send(
        &self,
        machine: &str,
        command: bridge::Command,
    ) -> std::result::Result<(), String> {
        let Some(Machine::Linked { bridge, .. }) = self.machines.get(machine) else {
            return Err(format!("machine {machine} has no bridge"));
        };

        bridge
            .commands
            .send(command)
            .map_err(|_| format!("machine {machine} bridge has stopped"))
    }

    pub(crate) fn send_host_command(&self, command: machines::Command) -> Result<()> {
        let Some(commands) = &self.host_commands else {
            return Err(Error::HostWorkerMissing);
        };

        if commands.send(command).is_err() {
            return Err(Error::HostWorkerStopped);
        }

        Ok(())
    }

    pub(crate) fn observe_bridge_event(
        &mut self,
        event: Unapplied<(String, u64, bridge::Event)>,
    ) -> Option<(String, bridge::Event)> {
        let Unapplied((machine, generation, event)) = event;
        let current = matches!(
            self.machines.get(&machine),
            Some(Machine::Linked {
                generation: current,
                ..
            }) if *current == generation
        );

        if !current {
            return None;
        }

        self.record_bridge_event(&machine, &event);

        Some((machine, event))
    }

    pub(crate) fn observe_machines_event(
        &mut self,
        event: Unapplied<machines::Event>,
    ) -> machines::Event {
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

    pub(crate) fn state(&self, machine: &str) -> MachineState<'_> {
        let state = self
            .machines
            .get(machine)
            .map(Machine::state)
            .unwrap_or(MachineState::Unknown);

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

    pub(crate) fn missing_tracked_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter(|machine| self.is_missing(machine))
            .cloned()
            .collect()
    }

    fn is_linked(&self, machine: &str) -> bool {
        matches!(self.machines.get(machine), Some(Machine::Linked { .. }))
    }

    pub(crate) fn host_for(&self, machine: &str) -> Result<Host> {
        let host = match MachineRef::parse(machine) {
            MachineRef::Local => Some(Host::Local),
            MachineRef::Provider(_) => self
                .provider_row(machine)
                .and_then(|row| row.target.clone())
                .map(Host::Ssh),
            MachineRef::AdHoc(dest) => Some(Host::Ssh(SshTarget {
                dest,
                port: None,
                options: Vec::new(),
            })),
        };

        host.ok_or_else(|| Error::NotConnectable {
            machine: machine.to_string(),
        })
    }

    pub(crate) fn label(&self, machine: &str) -> String {
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

    pub(crate) fn provider_machines(&self) -> &[MachineRow] {
        &self.provider_rows
    }

    pub(crate) fn ad_hoc_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter_map(|machine| match MachineRef::parse(machine) {
                MachineRef::Local | MachineRef::Provider(_) => None,
                MachineRef::AdHoc(dest) => Some(dest),
            })
            .collect()
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
        Fleet::inert()
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
                options: Vec::new(),
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
                generation: 1,
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

        assert_eq!(
            fleet.state("vm"),
            MachineState::ConnectFailed {
                detail: "daemon connect: handshake timed out"
            }
        );
    }

    #[test]
    fn observing_an_event_is_what_applies_it() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connecting);

        let (machine, event) = fleet
            .observe_bridge_event(Unapplied((
                "vm".to_string(),
                1,
                bridge::Event::Disconnected,
            )))
            .expect("current bridge event");

        assert_eq!(machine, "vm");
        assert!(
            matches!(event, bridge::Event::Disconnected),
            "the frontend still gets its event"
        );
        assert!(matches!(fleet.state("vm"), MachineState::ConnectionLost));
    }

    #[test]
    fn a_stale_bridge_generation_cannot_overwrite_its_replacement() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connected);

        if let Some(Machine::Linked { generation, .. }) = fleet.machines.get_mut("vm") {
            *generation = 2;
        }

        let ignored = fleet.observe_bridge_event(Unapplied((
            "vm".to_string(),
            1,
            bridge::Event::Disconnected,
        )));

        assert!(ignored.is_none());
        assert!(matches!(fleet.state("vm"), MachineState::Connected));
    }

    #[test]
    fn session_traffic_does_not_touch_the_link() {
        let mut fleet = fleet();

        with_link(&mut fleet, "vm", Link::Connecting);
        fleet.record_bridge_event(
            "vm",
            &bridge::Event::OperationFailed {
                sid: Some("sid".to_string()),
                generation: Some(1),
                operation: bridge::Operation::Attach,
                detail: "no such session".to_string(),
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
            fleet.machines.len(),
            2,
            "tracked and listed are both still known"
        );

        fleet.tracked = Vec::new();
        fleet.provider_rows = Vec::new();
        fleet.forget_unknown_machines();

        assert!(fleet.machines.is_empty());
        assert!(matches!(fleet.state("exedev@vm"), MachineState::Unknown));
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
            MachineState::Connected
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

        let connected = fleet
            .machines
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
            .count();

        assert_eq!(connected, 1);
        assert_eq!(
            fleet.machines.len(),
            3,
            "the probe remains tracked but is not linked"
        );
    }

    #[test]
    fn a_host_op_without_a_worker_says_so() {
        let error = fleet()
            .send_host_command(machines::Command::SetAdHocMachines(Vec::new()))
            .expect_err("this fleet has no worker");

        assert_eq!(error.to_string(), "machines worker is not running");
    }

    #[test]
    fn the_local_machine_is_its_own_kind() {
        assert_eq!(MachineRef::parse(LOCAL_MACHINE), MachineRef::Local);
    }

    #[test]
    fn a_slash_separated_key_is_a_provider_machine() {
        assert_eq!(
            MachineRef::parse("exe-dev/bloggy"),
            MachineRef::Provider(MachineKey {
                provider: "exe-dev".to_string(),
                id: "bloggy".to_string(),
            })
        );
    }

    #[test]
    fn an_id_may_itself_contain_slashes() {
        assert_eq!(
            MachineRef::parse("exe-dev/team/bloggy"),
            MachineRef::Provider(MachineKey {
                provider: "exe-dev".to_string(),
                id: "team/bloggy".to_string(),
            })
        );
    }

    #[test]
    fn a_bare_destination_is_ad_hoc() {
        assert_eq!(
            MachineRef::parse("exedev@vm.exe.dev"),
            MachineRef::AdHoc("exedev@vm.exe.dev".to_string())
        );
    }

    #[test]
    fn local_only_wins_as_the_whole_string() {
        assert_eq!(
            MachineRef::parse("local/vm"),
            MachineRef::Provider(MachineKey {
                provider: "local".to_string(),
                id: "vm".to_string(),
            })
        );
        assert_eq!(
            MachineRef::parse("localhost"),
            MachineRef::AdHoc("localhost".to_string())
        );
    }
}
