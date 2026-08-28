use crate::host::Host;
use crate::provider::{
    discover_providers, install_bundled_providers, MachineKey, Provider, ProviderMachine,
};
use crate::ssh::SshTarget;
use crate::token::RequestToken;
use crate::tokio_thread::run_on_tokio_thread;
use crate::workspace::{
    list_directory, resolve_git_branch, resolve_home, validate_git_workspace, RemoteDirEntry,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const LIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug)]
pub enum Command {
    SetAdHocMachines(Vec<String>),
    ResolveHome {
        host: Host,
        token: RequestToken,
    },
    ListDir {
        host: Host,
        path: String,
        token: RequestToken,
    },
    ValidateWorkspace {
        host: Host,
        path: String,
        token: RequestToken,
    },
    ResolveBranch {
        host: Host,
        path: String,
        token: RequestToken,
    },
}

#[derive(Debug)]
pub enum Event {
    Listed {
        rows: Vec<MachineRow>,
        listed_providers: Vec<String>,
    },
    ProviderFailed {
        provider: String,
        detail: String,
    },
    Probed {
        key: String,
        reachable: bool,
    },
    HomeResolved {
        token: RequestToken,
        result: Result<String, String>,
    },
    DirListed {
        token: RequestToken,
        result: Result<Vec<RemoteDirEntry>, String>,
    },
    WorkspaceValidated {
        token: RequestToken,
        error: Option<String>,
    },
    BranchResolved {
        token: RequestToken,
        result: Result<String, String>,
    },
}

#[derive(Debug, Clone)]
pub struct MachineRow {
    pub key: String,
    pub label: String,
    pub provider: String,
    pub target: Option<SshTarget>,
}

pub(crate) struct MachinesWorker {
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
}

pub(crate) fn start() -> std::io::Result<MachinesWorker> {
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
    run_on_tokio_thread(worker_main(commands, events), |detail| {
        eprintln!("harvester: machines worker: {detail}");
    });
}

async fn worker_main(mut commands: UnboundedReceiver<Command>, events: UnboundedSender<Event>) {
    if let Err(error) = install_bundled_providers() {
        eprintln!("harvester: install bundled providers: {error}");
    }

    crate::connection::sweep_leaked_forward_sockets().await;

    let mut ad_hoc_machines: Vec<String> = Vec::new();
    let mut provider_rows = ProviderRows::new();
    let mut provider_machines: Vec<MachineRow> = Vec::new();
    let (listed_sender, mut listed_receiver) = unbounded_channel();
    let mut list_tick = tokio::time::interval(LIST_INTERVAL);
    let mut probe_tick = tokio::time::interval(PROBE_INTERVAL);

    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None => break,
                Some(Command::SetAdHocMachines(machines)) => {
                    ad_hoc_machines = machines;
                    probe_ad_hoc(&ad_hoc_machines, &events);
                }
                Some(Command::ResolveHome { host, token }) => spawn_host_op(&events, async move {
                    let result = resolve_home(&host).await.map_err(|error| error.to_string());

                    Event::HomeResolved { token, result }
                }),
                Some(Command::ListDir { host, path, token }) => spawn_host_op(&events, async move {
                    let result = list_directory(&host, &path)
                        .await
                        .map_err(|error| error.to_string());

                    Event::DirListed { token, result }
                }),
                Some(Command::ValidateWorkspace { host, path, token }) => {
                    spawn_host_op(&events, async move {
                        let error = validate_git_workspace(&host, &path)
                            .await
                            .err()
                            .map(|error| error.to_string());

                        Event::WorkspaceValidated { token, error }
                    })
                }
                Some(Command::ResolveBranch { host, path, token }) => {
                    spawn_host_op(&events, async move {
                        let result = resolve_git_branch(&host, &path)
                            .await
                            .map_err(|error| error.to_string());

                        Event::BranchResolved { token, result }
                    })
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

                let mut listed_providers: Vec<String> = provider_rows.keys().cloned().collect();

                listed_providers.sort();

                let listed = Event::Listed {
                    rows: provider_machines.clone(),
                    listed_providers,
                };

                if events.send(listed).is_err() {
                    break;
                }

                probe_provider_machines(&provider_machines, &events);
            }
            _ = probe_tick.tick() => {
                probe_provider_machines(&provider_machines, &events);
                probe_ad_hoc(&ad_hoc_machines, &events);
            }
        }
    }
}

fn spawn_host_op(
    events: &UnboundedSender<Event>,
    op: impl std::future::Future<Output = Event> + Send + 'static,
) {
    let events = events.clone();

    tokio::spawn(async move {
        let _ = events.send(op.await);
    });
}

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

type ProviderRows = std::collections::HashMap<String, Vec<MachineRow>>;

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
            target: machine.ssh.target(),
        })
        .collect()
}

async fn list_one_provider(provider: &Provider) -> crate::Result<Vec<ProviderMachine>> {
    provider.check_protocol().await?;

    provider.list_machines().await
}

fn probe_machine(key: String, target: SshTarget, events: &UnboundedSender<Event>) {
    let events = events.clone();

    tokio::spawn(async move {
        let _ = events.send(Event::Probed {
            key,
            reachable: probe_reachable(&target).await,
        });
    });
}

async fn probe_reachable(target: &SshTarget) -> bool {
    match Host::Ssh(target.clone())
        .exec("true", None, PROBE_TIMEOUT)
        .await
    {
        Ok(output) => output.status.success(),
        Err(_transport_error) => false,
    }
}

fn probe_provider_machines(machines: &[MachineRow], events: &UnboundedSender<Event>) {
    for machine in machines {
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
