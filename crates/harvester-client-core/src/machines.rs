use crate::host::{parse_home, shell_quote, ExecOutput, Host, SshTarget, HOME_SCRIPT};
use crate::provider::{
    discover_providers, install_bundled_providers, MachineKey, Provider, ProviderMachine,
};
use crate::token::RequestToken;
use crate::Error;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};

const PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
const LIST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(300);
const WORKSPACE_EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Debug)]
pub(crate) enum Command {
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
pub(crate) enum Event {
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
        result: Result<(), String>,
    },
    BranchResolved {
        token: RequestToken,
        result: Result<String, String>,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct MachineRow {
    pub key: String,
    pub label: String,
    pub provider: String,
    pub target: Option<SshTarget>,
}

#[derive(Debug, Clone)]
pub struct RemoteDirEntry {
    pub name: String,
    pub is_dir: bool,
}

pub(crate) struct MachinesWorker {
    pub commands: UnboundedSender<Command>,
    pub events: UnboundedReceiver<Event>,
}

pub(crate) fn start() -> MachinesWorker {
    let (command_sender, command_receiver) = unbounded_channel();
    let (event_sender, event_receiver) = unbounded_channel();

    tokio::spawn(worker_main(command_receiver, event_sender));

    MachinesWorker {
        commands: command_sender,
        events: event_receiver,
    }
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
                Some(command) => handle_command(command, &mut ad_hoc_machines, &events),
            },
            _ = list_tick.tick() => {
                spawn_list_cycle(provider_rows.clone(), events.clone(), listed_sender.clone());
            }
            listed = listed_receiver.recv() => match listed {
                None => break,
                Some(rows) => match publish_listing(
                    rows,
                    &mut provider_rows,
                    &mut provider_machines,
                    &events,
                ) {
                    Listing::Published => {}
                    Listing::NobodyListening => break,
                },
            },
            _ = probe_tick.tick() => {
                probe_provider_machines(&provider_machines, &events);
                probe_ad_hoc(&ad_hoc_machines, &events);
            }
        }
    }
}

enum Listing {
    Published,
    NobodyListening,
}

fn publish_listing(
    listed_rows: ProviderRows,
    provider_rows: &mut ProviderRows,
    provider_machines: &mut Vec<MachineRow>,
    events: &UnboundedSender<Event>,
) -> Listing {
    let mut listed_providers: Vec<String> = listed_rows.keys().cloned().collect();

    listed_providers.sort();
    *provider_rows = listed_rows;
    *provider_machines = provider_rows.values().flatten().cloned().collect();

    let listed = Event::Listed {
        rows: provider_machines.clone(),
        listed_providers,
    };

    if events.send(listed).is_err() {
        return Listing::NobodyListening;
    }

    probe_provider_machines(provider_machines, events);

    Listing::Published
}

fn handle_command(
    command: Command,
    ad_hoc_machines: &mut Vec<String>,
    events: &UnboundedSender<Event>,
) {
    match command {
        Command::SetAdHocMachines(machines) => {
            set_ad_hoc_machines(machines, ad_hoc_machines, events);
        }
        Command::ResolveHome { host, token } => spawn_resolve_home(host, token, events),
        Command::ListDir { host, path, token } => spawn_list_dir(host, path, token, events),
        Command::ValidateWorkspace { host, path, token } => {
            spawn_validate_workspace(host, path, token, events);
        }
        Command::ResolveBranch { host, path, token } => {
            spawn_resolve_branch(host, path, token, events);
        }
    }
}

fn set_ad_hoc_machines(
    machines: Vec<String>,
    ad_hoc_machines: &mut Vec<String>,
    events: &UnboundedSender<Event>,
) {
    *ad_hoc_machines = machines;
    probe_ad_hoc(ad_hoc_machines, events);
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
        let result = validate_git_workspace(&host, &path)
            .await
            .map_err(|error| error.to_string());

        Event::WorkspaceValidated { token, result }
    });
}

fn spawn_resolve_branch(
    host: Host,
    path: String,
    token: RequestToken,
    events: &UnboundedSender<Event>,
) {
    spawn_host_op(events, async move {
        let result = resolve_git_branch(&host, &path)
            .await
            .map_err(|error| error.to_string());

        Event::BranchResolved { token, result }
    });
}

fn spawn_host_op(
    events: &UnboundedSender<Event>,
    op: impl std::future::Future<Output = Event> + Send + 'static,
) {
    let events = events.clone();

    tokio::spawn(async move {
        drop(events.send(op.await));
    });
}

async fn exec_workspace_script(
    host: &Host,
    script: &str,
    context: &'static str,
) -> crate::Result<ExecOutput> {
    let output = host.exec(script, None, WORKSPACE_EXEC_TIMEOUT).await?;

    output.require_success(context)?;

    Ok(output)
}

async fn resolve_home(host: &Host) -> crate::Result<String> {
    let output = exec_workspace_script(host, HOME_SCRIPT, "resolve home").await?;

    parse_home(&output.stdout)
        .map(str::to_string)
        .ok_or_else(|| Error::Exec {
            context: "resolve home",
            detail: output.describe_failure(),
        })
}

async fn list_directory(host: &Host, path: &str) -> crate::Result<Vec<RemoteDirEntry>> {
    let script = format!("ls -1Ap -- {}", shell_quote(path));
    let output = exec_workspace_script(host, &script, "list directory").await?;

    Ok(parse_ls_entries(&output.stdout))
}

fn parse_ls_entries(stdout: &str) -> Vec<RemoteDirEntry> {
    stdout.lines().filter_map(parse_ls_entry).collect()
}

fn parse_ls_entry(line: &str) -> Option<RemoteDirEntry> {
    let (name, is_dir) = line
        .strip_suffix('/')
        .map(|directory| (directory, true))
        .unwrap_or((line, false));

    if name.is_empty() || name.chars().any(char::is_control) {
        return None;
    }

    Some(RemoteDirEntry {
        name: name.to_string(),
        is_dir,
    })
}

async fn validate_git_workspace(host: &Host, path: &str) -> crate::Result<()> {
    let script = format!("git -C {} rev-parse --git-dir", shell_quote(path));

    exec_workspace_script(host, &script, "validate workspace").await?;

    Ok(())
}

async fn resolve_git_branch(host: &Host, path: &str) -> crate::Result<String> {
    let script = format!("git -C {} rev-parse --abbrev-ref HEAD", shell_quote(path));
    let output = exec_workspace_script(host, &script, "resolve branch").await?;

    let branch = output.stdout.trim();

    if branch.is_empty() {
        return Err(Error::Exec {
            context: "resolve branch",
            detail: output.describe_failure(),
        });
    }

    Ok(branch.to_string())
}

fn spawn_list_cycle(
    previous: ProviderRows,
    events: UnboundedSender<Event>,
    listed: UnboundedSender<ProviderRows>,
) {
    tokio::spawn(async move {
        if let Some(rows) = list_provider_machines(&previous, &events).await {
            drop(listed.send(rows));
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
                drop(events.send(Event::ProviderFailed {
                    provider: provider.name.clone(),
                    detail: error.to_string(),
                }));

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
            target: machine.target,
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
        drop(events.send(Event::Probed {
            key,
            reachable: probe_reachable(&target).await,
        }));
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
            options: Vec::new(),
        };

        probe_machine(machine.clone(), target, events);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(stdout: &str) -> Vec<(String, bool)> {
        parse_ls_entries(stdout)
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect()
    }

    #[test]
    fn a_trailing_slash_is_what_marks_a_directory() {
        assert_eq!(
            names("src/\nCargo.toml\ntarget/\n"),
            vec![
                ("src".to_string(), true),
                ("Cargo.toml".to_string(), false),
                ("target".to_string(), true),
            ]
        );
    }

    #[test]
    fn a_name_carrying_control_characters_is_dropped_rather_than_rendered() {
        assert_eq!(
            names("good.txt\nbell\u{7}name\nescape\u{1b}[31m\n"),
            vec![("good.txt".to_string(), false)],
            "`ls -1` cannot escape these, and a picker row is a path we later quote into a shell"
        );
    }

    #[test]
    fn a_blank_line_is_not_a_file() {
        assert_eq!(names("\n\nreal\n\n"), vec![("real".to_string(), false)]);
    }

    #[test]
    fn a_carriage_return_inside_a_name_is_dropped_but_a_crlf_line_ending_is_not() {
        assert_eq!(names("win\rdows.txt\n"), Vec::new());
        assert_eq!(
            names("windows.txt\r\n"),
            vec![("windows.txt".to_string(), false)],
            "a CRLF transport must not cost the listing every entry"
        );
    }
}
