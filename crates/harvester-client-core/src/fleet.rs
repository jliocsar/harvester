//! The fleet: every machine the client knows about, what is known about each
//! one, and the policy that decides when to connect one.
//!
//! This is the layer above `bridge` and `machines`. Those two each own one
//! machine's worth of work — a daemon connection, a provider listing, a probe.
//! Nobody owned the questions that span machines: which machines exist, which
//! of them have a bridge, whether a bridge should be started at all, and what
//! a machine's dot means. Those lived in the gpui crate's `workbench.rs`,
//! where a second frontend could not reach them.
//!
//! # Reactor boundary
//!
//! The fleet is as reactor-independent as the workers it owns, and for the
//! same reason `bridge.rs` documents: gpui's executor has no tokio reactor, so
//! a frontend must never be handed something it has to `await` on a tokio
//! primitive. So every method here is *synchronous and non-blocking*: commands
//! go out down unbounded senders (which never block and never need a reactor),
//! and answers come back as events on the two receivers [`FleetEvents`] hands
//! the frontend, which it pumps however it likes. The fleet itself never
//! awaits, never spawns a task, and holds no runtime — the two worker threads
//! it starts hold theirs.
//!
//! # Probe verdicts and daemon links are one state, not two
//!
//! [`MachineState`] is a single value per machine covering both, because they
//! used to be two maps (`connections` and `probes`) that could disagree, with
//! a precedence rule written only in the renderer that read them. The rule is
//! now enforced where the state is written: [`Machine::record_probe`] declines
//! to overwrite a machine that has a daemon link. A frontend reads one value
//! and renders it; there is no second field to keep in sync.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

use crate::bridge::{self, BridgeHandle};
use crate::host::Host;
use crate::machine::{MachineRef, LOCAL_MACHINE};
use crate::machines::{self, MachineRow};
use crate::ssh::SshTarget;

/// What the client knows about one machine. One value rather than a probe
/// verdict beside a link state, so the two cannot contradict each other.
///
/// The first three are what a probe can say; the last four are what a bridge
/// can say. A machine moves up into the bridge half when a bridge is started
/// for it and never falls back out — a probe is weaker evidence than an actual
/// connection, and a machine that failed to connect stays visibly failed until
/// something retries it.
#[derive(Debug)]
pub enum MachineState {
    /// Never probed and never connected. Indistinguishable from "reachable"
    /// without connecting, which is why both render the same.
    Unknown,
    /// Harvester's own `ssh <machine> true` probe answered.
    Reachable,
    /// The probe failed. Not a veto: connecting is still allowed to try, since
    /// the probe may simply be older than the machine booting.
    Unreachable,
    /// A bridge is up, bootstrapping or handshaking.
    Connecting,
    Connected {
        daemon_version: String,
    },
    /// The bridge reported why it could not connect. The detail is the user's
    /// only diagnosis, so it is carried, not discarded.
    ConnectFailed {
        detail: String,
    },
    /// The bridge connected and then lost the daemon.
    ConnectionLost,
}

/// Returned by [`Fleet::state`] for a machine nobody has probed or connected;
/// a `const` because a borrow of a temporary will not do.
const UNKNOWN_MACHINE_STATE: MachineState = MachineState::Unknown;

impl MachineState {
    /// A bridge has been started for this machine (in any outcome). The line
    /// between "what a probe may say" and "what only a bridge may say".
    pub fn has_link(&self) -> bool {
        matches!(
            self,
            MachineState::Connecting
                | MachineState::Connected { .. }
                | MachineState::ConnectFailed { .. }
                | MachineState::ConnectionLost
        )
    }

    /// The bridge is usable: connected, or on its way and queueing commands.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            MachineState::Connecting | MachineState::Connected { .. }
        )
    }
}

/// One machine's row in the fleet table: what is known about it, and the
/// bridge that knows it (if one was ever started).
struct Machine {
    state: MachineState,
    /// `None` until a bridge is started. Kept after a failure so a command
    /// sent to a dying machine still has somewhere to go — the send is a
    /// no-op once the thread is gone.
    bridge: Option<BridgeHandle>,
}

impl Machine {
    fn probed(reachable: bool) -> Machine {
        Machine {
            state: probe_state(reachable),
            bridge: None,
        }
    }

    fn linked(bridge: BridgeHandle) -> Machine {
        Machine {
            state: MachineState::Connecting,
            bridge: Some(bridge),
        }
    }

    /// A probe verdict never overwrites a daemon link. This is the invariant
    /// that used to live only in the renderer that read the two maps.
    fn record_probe(&mut self, reachable: bool) {
        if self.state.has_link() {
            return;
        }

        self.state = probe_state(reachable);
    }
}

fn probe_state(reachable: bool) -> MachineState {
    if reachable {
        MachineState::Reachable
    } else {
        MachineState::Unreachable
    }
}

/// A machine a workspace can be added on, or a session spawned on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineCandidate {
    /// The stored machine string — what a workspace records and a bridge keys.
    pub key: String,
    /// What a frontend shows for it.
    pub label: String,
}

/// Why a fleet operation could not be carried out.
///
/// Every variant renders the exact sentence the statusbar showed before this
/// type existed. The human-readable reason *is* the payload: a frontend that
/// collapsed this to "not connectable" would throw away the only diagnosis a
/// user gets for a machine that will not come up.
#[derive(Debug)]
pub enum FleetError {
    /// No host to exec on: a provider machine whose provider has not listed it
    /// (yet, or at all).
    NotConnectable { machine: String },
    /// The bridge thread itself would not start.
    BridgeStart { machine: String, detail: String },
    /// The machines worker never started, so there is no runtime to exec host
    /// ops on and the caller has nothing to wait for.
    HostWorkerMissing,
    /// The machines worker's thread ended.
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

/// The two event streams a frontend pumps, handed over once at
/// [`Fleet::start`]. Kept separate from the [`Fleet`] itself because a
/// frontend owns *how* it pumps them (a gpui task, a tokio task, a thread) —
/// the fleet only cares that whatever it reads gets fed back in through
/// [`Fleet::record_bridge_event`] and [`Fleet::record_machines_event`].
pub struct FleetEvents {
    /// Every bridge's events, tagged with the machine key.
    pub bridges: UnboundedReceiver<(String, bridge::Event)>,
    /// The machines worker's events; `None` if its thread failed to start (the
    /// machines list then stays empty — the reason is on stderr).
    pub machines: Option<UnboundedReceiver<machines::Event>>,
}

/// Owns the bridge pool, the machine table, the probe verdicts and the
/// connect policy. Synchronous throughout — see the module docs.
pub struct Fleet {
    /// The embedded musl harvesterd, shared by every bridge's bootstrap.
    daemon_binary: Arc<Vec<u8>>,
    /// Cloned into each bridge so every machine's events land on one channel.
    bridge_events: UnboundedSender<(String, bridge::Event)>,
    /// Every machine the client has an opinion about — probed, connected, or
    /// both. Absent means nothing is known yet.
    machines: HashMap<String, Machine>,
    /// Provider machines from the worker's last listing cycle, sorted by key.
    provider_rows: Vec<MachineRow>,
    /// The machines worker's command sender; `None` if its thread failed.
    host_commands: Option<UnboundedSender<machines::Command>>,
    /// The machines a frontend told the fleet to care about (see
    /// [`Fleet::track_machines`]), deduplicated and sorted. The ad-hoc subset
    /// is derived from it on read, never stored: it was a second copy that only
    /// one site recomputed, so any other way of changing `tracked` desynced it.
    tracked: Vec<String>,
}

impl Fleet {
    /// Starts the machines worker and hands back the event streams. Nothing
    /// connects here: bridges are lazy, and which machines earn one is
    /// [`Fleet::track_machines`]'s and [`Fleet::ensure_connection`]'s call.
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
            host_commands,
            tracked: Vec::new(),
        };
        let events = FleetEvents {
            bridges: bridge_receiver,
            machines: machine_events,
        };

        (fleet, events)
    }

    // --- what the frontend tells the fleet ---------------------------------

    /// The machines the client cares about: every stored workspace's machine,
    /// plus the argv destination. Call it again whenever that set changes.
    ///
    /// Two policies hang off this list, both of them the fleet's:
    /// the ad-hoc destinations the worker should probe, and the background
    /// connects that make a reopened client show its sessions again without
    /// being asked (docs/SPEC.md §Flows "Reopen client").
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
            // A dead worker means no probes; the dots stay unknown.
            let _ = commands.send(machines::Command::SetAdHocMachines(self.ad_hoc_machines()));
        }
    }

    /// Drops every machine the client no longer knows about — untracked and
    /// unlisted. Without it a machine deleted upstream stayed `Connected`
    /// forever and kept being counted; dropping the row drops its
    /// [`BridgeHandle`], which is what actually stops the bridge thread.
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

    /// Connects every tracked machine that has no bridge yet and resolves to a
    /// host. Quiet by design: an unresolvable machine (a provider that has not
    /// listed yet) is skipped without an error, and the next listing cycle
    /// retries it. A machine whose bridge already failed is *not* retried here
    /// — retouching it is the retry (see [`Fleet::ensure_connection`]).
    fn auto_connect_tracked(&mut self) {
        for machine in self.connectable_tracked_machines() {
            if let Err(error) = self.ensure_connection(&machine) {
                eprintln!("fleet: auto-connect {machine}: {error}");
            }
        }
    }

    /// Split out from [`Fleet::auto_connect_tracked`] both to end the borrow of
    /// `self.tracked` before connecting and because it is the whole policy —
    /// testable without starting a thread.
    fn connectable_tracked_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter(|machine| !self.state(machine).has_link())
            .filter(|machine| self.host_for(machine).is_ok())
            .cloned()
            .collect()
    }

    /// Lazily connects: reuses a live bridge, respawns a failed or lost one
    /// (retouching a machine is the retry), spawns a fresh one otherwise.
    pub fn ensure_connection(&mut self, machine: &str) -> Result<(), FleetError> {
        match self.connect_decision(machine)? {
            ConnectDecision::Reuse => Ok(()),
            ConnectDecision::Spawn(host) => self.spawn_bridge(machine, host),
        }
    }

    /// The connect policy, decided before anything is spawned.
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

        // The mirror of [`Machine::record_probe`], and the half that was
        // written nowhere: a daemon link always supersedes a probe verdict, so
        // an `Unreachable` dot gives way to `Connecting` rather than vetoing it.
        // Replacing any previous handle is what makes retouching a failed
        // machine the retry.
        self.machines
            .insert(machine.to_string(), Machine::linked(handle));

        Ok(())
    }

    /// Routes a command to the machine's bridge. A dead or missing bridge
    /// needs no handling: the link events drive the visible state.
    pub fn send(&self, machine: &str, command: bridge::Command) {
        let Some(bridge) = self
            .machines
            .get(machine)
            .and_then(|machine| machine.bridge.as_ref())
        else {
            return;
        };

        let _ = bridge.commands.send(command);
    }

    /// A clone of one machine's command sender, for a frontend surface that
    /// owns its own conversation with a bridge (a chat pane) rather than
    /// sending through [`Fleet::send`] on every keystroke. `None` when no
    /// bridge was ever started for the machine.
    pub fn commands_for(&self, machine: &str) -> Option<UnboundedSender<bridge::Command>> {
        self.machines
            .get(machine)
            .and_then(|machine| machine.bridge.as_ref())
            .map(|bridge| bridge.commands.clone())
    }

    /// Routes a host op (`$HOME`, `ls`, `git rev-parse`) to the machines
    /// worker. These never touch a daemon — docs/SPEC.md §Workspace, "no
    /// daemon or provider involvement" — which is why they do not go through a
    /// bridge and do not need one to exist.
    pub fn send_host_command(&self, command: machines::Command) -> Result<(), FleetError> {
        let Some(commands) = &self.host_commands else {
            return Err(FleetError::HostWorkerMissing);
        };

        if commands.send(command).is_err() {
            return Err(FleetError::HostWorkerStopped);
        }

        Ok(())
    }

    // --- what the fleet learns from the workers ----------------------------

    /// The link half of one bridge's event stream. Everything else in
    /// [`bridge::Event`] is session or request traffic the fleet has no
    /// opinion about, so this takes the event by reference and leaves the
    /// frontend's own routing untouched.
    pub fn record_bridge_event(&mut self, machine: &str, event: &bridge::Event) {
        match event {
            bridge::Event::Connected { daemon_version } => self.set_link_state(
                machine,
                MachineState::Connected {
                    daemon_version: daemon_version.clone(),
                },
            ),
            bridge::Event::ConnectFailed { detail } => self.set_link_state(
                machine,
                MachineState::ConnectFailed {
                    detail: detail.clone(),
                },
            ),
            bridge::Event::Disconnected => {
                self.set_link_state(machine, MachineState::ConnectionLost)
            }
            _ => {}
        }
    }

    /// Only ever called for a machine whose bridge we started, so the entry
    /// exists; the `else` is there so a stray event cannot panic a frontend.
    fn set_link_state(&mut self, machine: &str, state: MachineState) {
        let Some(entry) = self.machines.get_mut(machine) else {
            eprintln!("fleet: link event for unknown machine {machine}");

            return;
        };

        entry.state = state;
    }

    /// The fleet's half of the machines worker's event stream: the provider
    /// listing and the probe verdicts. The rest — the host-op answers, which
    /// belong to whoever asked, and the provider failure, which is statusbar
    /// material — is the frontend's.
    pub fn record_machines_event(&mut self, event: &machines::Event) {
        match event {
            machines::Event::Listed(rows) => self.handle_listed(rows),
            machines::Event::Probed { key, reachable } => self.handle_probed(key, *reachable),
            machines::Event::ProviderFailed { .. }
            | machines::Event::HomeResolved { .. }
            | machines::Event::DirListed { .. }
            | machines::Event::WorkspaceValidated { .. } => {}
        }
    }

    fn handle_listed(&mut self, rows: &[MachineRow]) {
        let mut rows = rows.to_vec();

        rows.sort_by(|a, b| a.key.cmp(&b.key));
        self.provider_rows = rows;
        // A machine that vanished from the listing is gone; a provider-key
        // tracked machine that just appeared in it becomes resolvable, so
        // connect it and its sessions show up.
        self.forget_unknown_machines();
        self.auto_connect_tracked();
    }

    fn handle_probed(&mut self, key: &str, reachable: bool) {
        match self.machines.get_mut(key) {
            Some(machine) => machine.record_probe(reachable),
            None => {
                self.machines
                    .insert(key.to_string(), Machine::probed(reachable));
            }
        }
    }

    // --- what the frontend reads ------------------------------------------

    /// The one value a frontend renders a machine from.
    pub fn state(&self, machine: &str) -> &MachineState {
        self.machines
            .get(machine)
            .map(|machine| &machine.state)
            .unwrap_or(&UNKNOWN_MACHINE_STATE)
    }

    /// Machine string → the host its execs run on, through the one place that
    /// decides what a machine string means ([`MachineRef`]). A provider
    /// machine resolves through the worker's last listing, so an error means
    /// the key is unknown (not listed yet) or the machine uses the
    /// unsupported argv ssh form.
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

    /// Display label for a stored machine string: the provider machine's
    /// label when listed, the key's id half as a fallback, the string itself
    /// for ad-hoc destinations and `local`.
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

    /// Provider machines from the last listing cycle, sorted by key.
    pub fn provider_machines(&self) -> &[MachineRow] {
        &self.provider_rows
    }

    /// The tracked ad-hoc SSH destinations — their own sidebar rows, since no
    /// provider lists them, and the set the worker probes alongside provider
    /// machines. `local` has no SSH to probe and provider machines are probed
    /// off the worker's own listing, so neither belongs here.
    pub fn ad_hoc_machines(&self) -> Vec<String> {
        self.tracked
            .iter()
            .filter_map(|machine| match MachineRef::parse(machine) {
                MachineRef::Local | MachineRef::Provider(_) => None,
                MachineRef::AdHoc(dest) => Some(dest),
            })
            .collect()
    }

    /// Machines a workspace can be added on (or a session spawned on).
    ///
    /// "Connectable" is [`Fleet::host_for`] and nothing else. It used to be
    /// spelled twice — once as "resolves to a host" and once as a re-derived
    /// provider-target filter here — and the two disagreed about ad-hoc
    /// destinations.
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

    /// How many machines have a bridge at all — the denominator a frontend
    /// counts connections against. Probe-only machines do not count: nobody
    /// asked them to connect.
    pub fn linked_machine_count(&self) -> usize {
        self.machines
            .values()
            .filter(|machine| machine.state.has_link())
            .count()
    }

    pub fn connected_machine_count(&self) -> usize {
        self.machines
            .values()
            .filter(|machine| matches!(machine.state, MachineState::Connected { .. }))
            .count()
    }
}

/// What [`Fleet::ensure_connection`] should do about a machine, decided before
/// a thread is spawned — which is what makes the connect policy testable.
#[derive(Debug)]
enum ConnectDecision {
    /// A live bridge already exists; commands queue on it.
    Reuse,
    /// No usable bridge: start one against this host.
    Spawn(Host),
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fleet with no workers: `start` would spawn a machines thread, and
    /// none of the policy under test needs one. Everything below drives the
    /// table through the same methods the event pump does.
    fn fleet() -> Fleet {
        // Nothing here spawns a bridge, so the sender is never used and the
        // dropped receiver costs nothing.
        let (bridge_events, _events) = unbounded_channel();

        Fleet {
            daemon_binary: Arc::new(Vec::new()),
            bridge_events,
            machines: HashMap::new(),
            provider_rows: Vec::new(),
            host_commands: None,
            tracked: Vec::new(),
        }
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

    fn with_state(fleet: &mut Fleet, machine: &str, state: MachineState) {
        fleet.machines.insert(
            machine.to_string(),
            Machine {
                state,
                bridge: None,
            },
        );
    }

    // --- probe verdict vs daemon link -------------------------------------

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
        // The invariant that used to live only in the renderer reading two
        // maps: an actual connection outranks `ssh <machine> true`.
        let mut fleet = fleet();

        for linked in [
            MachineState::Connecting,
            MachineState::Connected {
                daemon_version: "0.1.0".to_string(),
            },
            MachineState::ConnectFailed {
                detail: "no route to host".to_string(),
            },
            MachineState::ConnectionLost,
        ] {
            with_state(&mut fleet, "vm", linked);
            fleet.handle_probed("vm", true);
            fleet.handle_probed("vm", false);

            assert!(
                fleet.state("vm").has_link(),
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
        // The detail is the user's only diagnosis; losing it to a tidier enum
        // was the trade this type exists to refuse.
        let mut fleet = fleet();

        with_state(&mut fleet, "vm", MachineState::Connecting);
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
    fn session_traffic_does_not_touch_the_link() {
        let mut fleet = fleet();

        with_state(&mut fleet, "vm", MachineState::Connecting);
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

    // --- the connect policy ------------------------------------------------

    #[test]
    fn a_live_bridge_is_reused() {
        let mut fleet = fleet();

        for live in [
            MachineState::Connecting,
            MachineState::Connected {
                daemon_version: "0.1.0".to_string(),
            },
        ] {
            with_state(&mut fleet, "vm", live);

            assert!(matches!(
                fleet.connect_decision("vm"),
                Ok(ConnectDecision::Reuse)
            ));
        }
    }

    #[test]
    fn a_failed_or_lost_bridge_is_respawned() {
        // Retouching a machine is the retry — the whole reason the failed
        // state stays in the table rather than being cleared.
        let mut fleet = fleet();

        for dead in [
            MachineState::ConnectFailed {
                detail: "boom".to_string(),
            },
            MachineState::ConnectionLost,
        ] {
            with_state(&mut fleet, "vm", dead);

            assert!(matches!(
                fleet.connect_decision("vm"),
                Ok(ConnectDecision::Spawn(_))
            ));
        }
    }

    #[test]
    fn an_unreachable_probe_does_not_veto_connecting() {
        // The probe may simply be older than the machine finishing its boot.
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

        fleet.handle_listed(&[provider_row(
            "exe-dev/bloggy",
            Some("exedev@bloggy.exe.dev"),
        )]);

        let Ok(ConnectDecision::Spawn(Host::Ssh(target))) =
            fleet.connect_decision("exe-dev/bloggy")
        else {
            panic!("a listed machine with a target resolves");
        };

        assert_eq!(target.dest, "exedev@bloggy.exe.dev");
    }

    #[test]
    fn a_listed_provider_machine_without_a_target_stays_unconnectable() {
        // The argv ssh escape hatch: such machines list, but stay unreachable.
        let mut fleet = fleet();

        fleet.handle_listed(&[provider_row("exe-dev/legacy", None)]);

        assert!(fleet.connect_decision("exe-dev/legacy").is_err());
    }

    #[test]
    fn the_local_machine_is_always_connectable() {
        assert!(matches!(
            fleet().connect_decision(LOCAL_MACHINE),
            Ok(ConnectDecision::Spawn(Host::Local))
        ));
    }

    // --- what tracking a machine implies -----------------------------------

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
        // Including a failed one: retouching it is the retry, and a listing
        // cycle every five minutes is not a retouch.
        let mut fleet = fleet();

        fleet.tracked = vec![LOCAL_MACHINE.to_string()];
        with_state(
            &mut fleet,
            LOCAL_MACHINE,
            MachineState::ConnectFailed {
                detail: "boom".to_string(),
            },
        );

        assert!(fleet.connectable_tracked_machines().is_empty());
    }

    #[test]
    fn a_probed_machine_is_still_auto_connectable() {
        // Probe-only entries live in the same table as linked ones now, so
        // "already in the table" must not be mistaken for "already connected".
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
        // It used to stay `Connected` forever and keep being counted. The row
        // owns the bridge handle, so forgetting it is also what stops the
        // bridge thread of a machine deleted upstream.
        let mut fleet = fleet();

        with_state(&mut fleet, "exedev@vm", MachineState::Connecting);
        with_state(&mut fleet, "exe-dev/bloggy", MachineState::Connecting);
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

    // --- what the frontend reads ------------------------------------------

    #[test]
    fn candidates_are_local_plus_resolvable_machines() {
        let mut fleet = fleet();

        fleet.handle_listed(&[
            provider_row("exe-dev/bloggy", Some("exedev@bloggy")),
            provider_row("exe-dev/legacy", None),
        ]);
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
    fn a_label_falls_back_to_the_key_id_until_the_provider_lists() {
        let mut fleet = fleet();

        assert_eq!(fleet.label("exe-dev/bloggy"), "bloggy");

        fleet.handle_listed(&[provider_row("exe-dev/bloggy", Some("exedev@bloggy"))]);

        assert_eq!(fleet.label("exe-dev/bloggy"), "exe-dev/bloggy label");
        assert_eq!(fleet.label("exedev@vm"), "exedev@vm");
        assert_eq!(fleet.label(LOCAL_MACHINE), LOCAL_MACHINE);
    }

    #[test]
    fn only_machines_with_a_bridge_are_counted() {
        let mut fleet = fleet();

        fleet.handle_probed("exedev@probed-only", true);
        with_state(&mut fleet, "connecting", MachineState::Connecting);
        with_state(
            &mut fleet,
            "connected",
            MachineState::Connected {
                daemon_version: "0.1.0".to_string(),
            },
        );

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
