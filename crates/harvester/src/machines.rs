//! The machines worker: one background thread (same tokio-on-a-thread shape
//! as `bridge.rs`) that owns everything provider- and probe-related — the
//! one-shot provider executables and the SSH probes are tokio-process work,
//! which gpui's executor cannot run.
//!
//! It enumerates `~/.harvester/providers/` (docs/PROVIDERS.md), lists each
//! provider's machines on a slow cycle, and drives the sidebar dots with
//! Harvester's own `ssh <machine> true` probe on a faster cycle — never with
//! provider-reported status (docs/SPEC.md §Machine).

use harvester_client_core::{
    discover_providers, install_bundled_providers, probe_reachable, MachineKey, Provider,
    ProviderMachine, SshTarget,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Probe cadence for the sidebar dots. A warm multiplexed probe is one
/// network round trip, so this is cheap.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Provider `list` cadence — machine sets change rarely, and each cycle
/// costs one control-plane exec per provider (exe-dev: an `ssh exe.dev ls`).
const LIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// UI → worker.
#[derive(Debug)]
pub enum Command {
    /// The full set of ad-hoc machines (argv destination + legacy workspace
    /// dests) the worker should probe alongside provider machines.
    SetAdHocMachines(Vec<String>),
}

/// worker → UI.
#[derive(Debug)]
pub enum Event {
    /// One full provider-machine listing cycle finished; replaces the
    /// previous provider-machine set wholesale.
    Listed(Vec<MachineRow>),
    /// A provider failed `info` or `list` this cycle; statusbar material.
    ProviderFailed { provider: String, detail: String },
    /// One machine's probe answered.
    Probed { key: String, reachable: bool },
}

/// One provider machine as the sidebar needs it.
#[derive(Debug, Clone)]
pub struct MachineRow {
    /// `<provider>/<id>` — how workspaces reference it and bridges key it.
    pub key: String,
    pub label: String,
    pub provider: String,
    /// Resolved connection target; `None` for the argv ssh escape hatch
    /// (unsupported yet) — such machines list, but stay unreachable.
    pub target: Option<SshTarget>,
}

pub struct MachinesWorker {
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
}

pub fn start() -> std::io::Result<MachinesWorker> {
    let (command_sender, command_receiver) = unbounded_channel();
    let (event_sender, event_receiver) = unbounded_channel();

    std::thread::Builder::new()
        .name("harvester-machines".to_string())
        .spawn(move || run_worker_thread(command_receiver, event_sender))?;

    Ok(MachinesWorker {
        commands: command_sender,
        events: event_receiver,
    })
}

fn run_worker_thread(commands: UnboundedReceiver<Command>, events: UnboundedSender<Event>) {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("harvester: machines worker tokio runtime: {error}");

            return;
        }
    };

    runtime.block_on(worker_main(commands, events));
}

async fn worker_main(mut commands: UnboundedReceiver<Command>, events: UnboundedSender<Event>) {
    // First run drops the bundled exe-dev provider in place; a failure only
    // costs the default provider, discovery still runs.
    if let Err(error) = install_bundled_providers() {
        eprintln!("harvester: install bundled providers: {error}");
    }

    let mut ad_hoc_machines: Vec<String> = Vec::new();
    let mut provider_machines: Vec<MachineRow> = Vec::new();
    // Both intervals fire immediately on the first tick, so startup does one
    // list cycle and one probe cycle without extra code.
    let mut list_tick = tokio::time::interval(LIST_INTERVAL);
    let mut probe_tick = tokio::time::interval(PROBE_INTERVAL);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                // UI dropped the worker handle — window closed.
                None => break,
                Some(Command::SetAdHocMachines(machines)) => {
                    ad_hoc_machines = machines;
                    probe_ad_hoc(&ad_hoc_machines, &events).await;
                }
            },
            _ = list_tick.tick() => {
                provider_machines = list_provider_machines(&events).await;

                if events.send(Event::Listed(provider_machines.clone())).is_err() {
                    break;
                }

                // Fresh rows get their dots right away instead of waiting
                // out the probe interval.
                probe_provider_machines(&provider_machines, &events).await;
            }
            _ = probe_tick.tick() => {
                probe_provider_machines(&provider_machines, &events).await;
                probe_ad_hoc(&ad_hoc_machines, &events).await;
            }
        }
    }
}

/// One listing cycle: discover, protocol-gate via `info`, `list`. A failing
/// provider reports and contributes nothing this cycle; the others still
/// list.
async fn list_provider_machines(events: &UnboundedSender<Event>) -> Vec<MachineRow> {
    let providers = match discover_providers() {
        Ok(providers) => providers,
        Err(error) => {
            eprintln!("harvester: discover providers: {error}");

            return Vec::new();
        }
    };

    let mut rows = Vec::new();

    for provider in &providers {
        match list_one_provider(provider).await {
            Ok(machines) => rows.extend(machines.into_iter().map(|machine| {
                let target = machine.ssh.target().ok();

                MachineRow {
                    key: MachineKey {
                        provider: provider.name.clone(),
                        id: machine.id,
                    }
                    .to_string(),
                    label: machine.label,
                    provider: provider.name.clone(),
                    target,
                }
            })),
            Err(error) => {
                let _ = events.send(Event::ProviderFailed {
                    provider: provider.name.clone(),
                    detail: error.to_string(),
                });
            }
        }
    }

    rows
}

async fn list_one_provider(
    provider: &Provider,
) -> harvester_client_core::Result<Vec<ProviderMachine>> {
    // `info` gates the protocol version before the first `list` and on every
    // cycle after — cheap next to the `list` itself, and providers can be
    // swapped out on disk between cycles.
    provider.info().await?;

    provider.list_machines().await
}

/// Probes run sequentially: the machine count is small, the worker thread is
/// free anyway, and results stream into the UI as they land.
async fn probe_provider_machines(machines: &[MachineRow], events: &UnboundedSender<Event>) {
    for machine in machines {
        // No target (argv ssh form) → no probe; the dot stays unknown.
        let Some(target) = &machine.target else {
            continue;
        };
        let reachable = probe_reachable(target).await;
        let _ = events.send(Event::Probed {
            key: machine.key.clone(),
            reachable,
        });
    }
}

async fn probe_ad_hoc(machines: &[String], events: &UnboundedSender<Event>) {
    for machine in machines {
        let target = SshTarget {
            dest: machine.clone(),
            port: None,
        };
        let reachable = probe_reachable(&target).await;
        let _ = events.send(Event::Probed {
            key: machine.clone(),
            reachable,
        });
    }
}
