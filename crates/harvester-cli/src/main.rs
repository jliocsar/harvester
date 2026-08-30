use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harvester_client_core::{
    bootstrap_and_connect, raw, DaemonClient, DaemonConnection, Host, SshTarget,
};
use harvester_proto::HarvesterCmd;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(about = "Dev driver for the Harvester client library: bootstraps \
harvesterd over SSH, connects, and runs one command")]
struct Cli {
    #[arg(
        long,
        value_name = "PATH",
        default_value = harvester_proto::DEV_DAEMON_PATH
    )]
    daemon_bin: PathBuf,

    dest: String,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    Ping,
    ListSessions,
    Raw,
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(error) = run(Cli::parse()).await {
        eprintln!("harvester-cli: {error}");
        std::process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let daemon_binary = std::fs::read(&cli.daemon_bin).map_err(|error| {
        format!(
            "read {}: {error}\n(build it: cargo build --profile daemon-release --target x86_64-unknown-linux-musl -p harvesterd)",
            cli.daemon_bin.display()
        )
    })?;

    let target = SshTarget {
        dest: cli.dest,
        port: None,
    };
    let connected = bootstrap_and_connect(&Host::Ssh(target.clone()), &daemon_binary).await?;
    eprintln!("harvesterd on {} ({:?})", target.dest, connected.outcome);

    match cli.command {
        CliCommand::Ping => Ok(()),
        CliCommand::ListSessions => list_sessions(connected.connection).await,
        CliCommand::Raw => raw(connected.connection).await,
    }
}

async fn list_sessions(connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let (client, _events) = DaemonClient::start(connection);
    let response = client.harvester(HarvesterCmd::ListSessions).await?;
    let data = response.into_result()?;

    println!("{}", serde_json::to_string_pretty(&data)?);

    Ok(())
}

async fn raw(connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let DaemonConnection {
        mut reader,
        mut writer,
    } = connection;

    let printer = tokio::spawn(async move {
        loop {
            match raw::next_line(&mut reader).await {
                Ok(Some(line)) => println!("{}", String::from_utf8_lossy(line)),
                Ok(None) => break,
                Err(error) => {
                    eprintln!("read from daemon: {error}");

                    break;
                }
            }
        }
    });

    let mut stdin_lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = stdin_lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }

        raw::send_line(&mut writer, line.as_bytes()).await?;
    }

    drop(writer);
    drop(printer.await);

    Ok(())
}
