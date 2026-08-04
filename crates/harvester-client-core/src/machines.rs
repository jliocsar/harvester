//! The machines worker: one background thread (same tokio-on-a-thread shape
//! as `bridge.rs`) that owns everything that execs on a machine *without* a
//! daemon — the one-shot provider executables, the SSH probes and the folder
//! picker's path work are all tokio-process work, which gpui's executor
//! cannot run.
//!
//! It enumerates `~/.harvester/providers/` (docs/PROVIDERS.md), lists each
//! provider's machines on a slow cycle, and drives the sidebar dots with
//! Harvester's own `ssh <machine> true` probe on a faster cycle — never with
//! provider-reported status (docs/SPEC.md §Machine).
//!
//! It also answers the host ops the add-workspace flow needs (`$HOME`, `ls`,
//! `git rev-parse`). Those are plain [`Host`] execs — docs/SPEC.md §Workspace:
//! "no daemon or provider involvement" — so they are keyed by `Host`, not by
//! machine key, and correlate their replies with a [`RequestToken`] rather
//! than by echoing the path back. Routing them through a machine's bridge is
//! what used to make adding a workspace on a fresh machine wait out a full
//! harvesterd bootstrap.

use crate::host::Host;
use crate::provider::{
    discover_providers, install_bundled_providers, MachineKey, Provider, ProviderMachine,
};
use crate::ssh::{probe_reachable, SshTarget};
use crate::token::RequestToken;
use crate::workspace::{list_directory, resolve_home, validate_git_workspace, RemoteDirEntry};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

/// Probe cadence for the sidebar dots. A warm multiplexed probe is one
/// network round trip, so this is cheap.
const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
/// Provider `list` cadence — machine sets change rarely, and each cycle
/// costs one control-plane exec per provider (exe-dev: an `ssh exe.dev ls`).
const LIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

/// UI → worker. Every command that is answered carries the token its answer
/// echoes; `SetAdHocMachines` has no answer, so it carries none.
#[derive(Debug)]
pub enum Command {
    /// The full set of ad-hoc machines (argv destination + legacy workspace
    /// dests) the worker should probe alongside provider machines.
    SetAdHocMachines(Vec<String>),
    /// The machine's `$HOME` — the folder picker's start directory. Answered
    /// by [`Event::HomeResolved`].
    ResolveHome { host: Host, token: RequestToken },
    /// List a directory for the folder picker. Answered by
    /// [`Event::DirListed`].
    ListDir {
        host: Host,
        path: String,
        token: RequestToken,
    },
    /// `git -C <path> rev-parse --git-dir` on the machine. Answered by
    /// [`Event::WorkspaceValidated`].
    ValidateWorkspace {
        host: Host,
        path: String,
        token: RequestToken,
    },
}

/// worker → UI. The three host-op answers carry the token of the request
/// that asked for them and nothing else identifying: a frontend applies the
/// answer only while that token is the one it still has outstanding.
#[derive(Debug)]
pub enum Event {
    /// One full provider-machine listing cycle finished; replaces the
    /// previous provider-machine set wholesale.
    Listed(Vec<MachineRow>),
    /// A provider failed `info` or `list` this cycle; statusbar material.
    ProviderFailed { provider: String, detail: String },
    /// One machine's probe answered.
    Probed { key: String, reachable: bool },
    /// A `ResolveHome` answered.
    HomeResolved {
        token: RequestToken,
        result: Result<String, String>,
    },
    /// A `ListDir` answered; errors render inside the picker, not the
    /// statusbar.
    DirListed {
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    },
    /// A `ValidateWorkspace` answered; `None` means the path is a git repo.
    WorkspaceValidated {
        token: RequestToken,
        error: Option<String>,
    },
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

    // This worker is the client's one startup point inside client-core, and
    // it runs before any bridge binds a forward — the moment to reclaim the
    // socket files a previously killed client could not clean up itself.
    crate::connection::sweep_leaked_forward_sockets().await;

    let mut ad_hoc_machines: Vec<String> = Vec::new();
    let mut provider_rows = ProviderRows::new();
    let mut provider_machines: Vec<MachineRow> = Vec::new();
    // Listing runs off the loop and reports back here, so a slow provider
    // never delays a probe tick or a command.
    let (listed_sender, mut listed_receiver) = unbounded_channel();
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
                    probe_ad_hoc(&ad_hoc_machines, &events);
                }
                Some(Command::ResolveHome { host, token }) => {
                    spawn_resolve_home(host, token, &events);
                }
                Some(Command::ListDir { host, path, token }) => {
                    spawn_list_dir(host, path, token, &events);
                }
                Some(Command::ValidateWorkspace { host, path, token }) => {
                    spawn_validate_workspace(host, path, token, &events);
                }
            },
            _ = list_tick.tick() => {
                spawn_list_cycle(provider_rows.clone(), events.clone(), listed_sender.clone());
            }
            listed = listed_receiver.recv() => {
                let Some(rows) = listed else {
                    break;
                };

                provider_rows = rows;
                provider_machines = provider_rows.values().flatten().cloned().collect();

                if events.send(Event::Listed(provider_machines.clone())).is_err() {
                    break;
                }

                // Fresh rows get their dots right away instead of waiting
                // out the probe interval.
                probe_provider_machines(&provider_machines, &events);
            }
            _ = probe_tick.tick() => {
                probe_provider_machines(&provider_machines, &events);
                probe_ad_hoc(&ad_hoc_machines, &events);
            }
        }
    }
}

/// Every host op rides its own task and reports as it lands: a `$HOME` on a
/// cold machine costs a full SSH handshake, and the loop must keep probing,
/// listing and answering commands meanwhile. Concurrent ops are fine — they
/// are one-shot execs that share nothing, and each answer carries the token
/// that tells them apart.
fn spawn_host_op(
    events: &UnboundedSender<Event>,
    op: impl std::future::Future<Output = Event> + Send + 'static,
) {
    let events = events.clone();

    tokio::spawn(async move {
        // A dropped receiver just means the window closed.
        let _ = events.send(op.await);
    });
}

fn spawn_resolve_home(host: Host, token: RequestToken, events: &UnboundedSender<Event>) {
    spawn_host_op(events, async move {
        let result = resolve_home(&host).await.map_err(|error| error.to_string());

        Event::HomeResolved { token, result }
    });
}

fn spawn_list_dir(host: Host, path: String, token: RequestToken, events: &UnboundedSender<Event>) {
    spawn_host_op(events, async move {
        let result = list_directory(&host, &path)
            .await
            .map_err(|error| error.to_string());

        Event::DirListed { token, result }
    });
}

fn spawn_validate_workspace(
    host: Host,
    path: String,
    token: RequestToken,
    events: &UnboundedSender<Event>,
) {
    spawn_host_op(events, async move {
        let error = validate_git_workspace(&host, &path)
            .await
            .err()
            .map(|error| error.to_string());

        Event::WorkspaceValidated { token, error }
    });
}

/// Run one listing cycle detached. A provider executable that hangs is bounded
/// only by its own kill-timeout, which is far longer than a probe interval —
/// so the cycle must not sit in the select loop.
fn spawn_list_cycle(
    previous: ProviderRows,
    events: UnboundedSender<Event>,
    listed: UnboundedSender<ProviderRows>,
) {
    tokio::spawn(async move {
        if let Some(rows) = list_provider_machines(&previous, &events).await {
            let _ = listed.send(rows);
        }
    });
}

/// Each provider's rows from its last *successful* listing. Keeping them per
/// provider is what makes a failed cycle survivable: a single blip on one
/// provider's control plane would otherwise unlist its machines for a full
/// interval — and since workspaces resolve their machine key through this
/// list, those workspaces become unopenable meanwhile.
type ProviderRows = std::collections::HashMap<String, Vec<MachineRow>>;

/// One listing cycle: discover, then protocol-gate and `list` every provider
/// concurrently. A provider that fails reports and keeps its previous rows;
/// `None` means discovery itself failed, so the caller changes nothing at all.
async fn list_provider_machines(
    previous: &ProviderRows,
    events: &UnboundedSender<Event>,
) -> Option<ProviderRows> {
    let providers = match discover_providers() {
        Ok(providers) => providers,
        Err(error) => {
            eprintln!("harvester: discover providers: {error}");

            return None;
        }
    };

    // Concurrent, so the cycle costs the slowest provider rather than the sum
    // of all of them.
    let mut listings = tokio::task::JoinSet::new();

    for provider in providers {
        listings.spawn(async move {
            let result = list_one_provider(&provider).await;

            (provider, result)
        });
    }

    let mut rows = ProviderRows::new();

    while let Some(joined) = listings.join_next().await {
        let Ok((provider, result)) = joined else {
            continue;
        };

        match result {
            Ok(machines) => {
                rows.insert(provider.name.clone(), machine_rows(&provider, machines));
            }
            Err(error) => {
                let _ = events.send(Event::ProviderFailed {
                    provider: provider.name.clone(),
                    detail: error.to_string(),
                });

                if let Some(last_good) = previous.get(&provider.name) {
                    rows.insert(provider.name.clone(), last_good.clone());
                }
            }
        }
    }

    Some(rows)
}

fn machine_rows(provider: &Provider, machines: Vec<ProviderMachine>) -> Vec<MachineRow> {
    machines
        .into_iter()
        .map(|machine| MachineRow {
            key: MachineKey {
                provider: provider.name.clone(),
                id: machine.id,
            }
            .to_string(),
            label: machine.label,
            provider: provider.name.clone(),
            // No target for the argv ssh form (unsupported yet): such machines
            // list, but stay unreachable.
            target: machine.ssh.target().ok(),
        })
        .collect()
}

async fn list_one_provider(provider: &Provider) -> crate::Result<Vec<ProviderMachine>> {
    // `info` gates the protocol version before the first `list` and on every
    // cycle after — cheap next to the `list` itself, and providers can be
    // swapped out on disk between cycles.
    provider.check_protocol().await?;

    provider.list_machines().await
}

/// Every probe rides its own task and streams its result as it lands: one
/// dead machine burning its ConnectTimeout no longer stalls every later
/// machine's dot, and the worker loop keeps answering commands meanwhile.
/// Overlap between cycles is bounded (probe timeout < probe interval) and a
/// duplicate probe is idempotent — the dot just updates twice.
fn probe_machine(key: String, target: SshTarget, events: &UnboundedSender<Event>) {
    let events = events.clone();

    tokio::spawn(async move {
        let reachable = probe_reachable(&target).await;
        let _ = events.send(Event::Probed { key, reachable });
    });
}

fn probe_provider_machines(machines: &[MachineRow], events: &UnboundedSender<Event>) {
    for machine in machines {
        // No target (argv ssh form) → no probe; the dot stays unknown.
        let Some(target) = &machine.target else {
            continue;
        };

        probe_machine(machine.key.clone(), target.clone(), events);
    }
}

fn probe_ad_hoc(machines: &[String], events: &UnboundedSender<Event>) {
    for machine in machines {
        let target = SshTarget {
            dest: machine.clone(),
            port: None,
        };

        probe_machine(machine.clone(), target, events);
    }
}
