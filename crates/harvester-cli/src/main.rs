//! harvester-cli — dev driver for harvester-client-core. Bootstraps
//! harvesterd on a machine, connects, and runs one command. Doubles as the
//! protocol smoke-test tool (`raw` mode replaces the throwaway python
//! drivers from the e2e rounds).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harvester_client_core::{
    bootstrap_and_connect, DaemonClient, DaemonConnection, Host, SshTarget,
};
use harvester_proto::HarvesterCmd;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(about = "Dev driver for the Harvester client library: bootstraps \
harvesterd over SSH, connects, and runs one command")]
struct Cli {
    /// Path to the static musl harvesterd binary to ship
    #[arg(
        long,
        value_name = "PATH",
        default_value = "target/x86_64-unknown-linux-musl/release/harvesterd"
    )]
    daemon_bin: PathBuf,

    /// SSH destination: user@host or an ssh-config alias
    dest: String,

    #[command(subcommand)]
    command: CliCommand,
}

#[derive(Subcommand)]
enum CliCommand {
    /// Bootstrap + connect + version handshake, then exit
    Ping,
    /// Print the daemon's session list as JSON
    ListSessions,
    /// Pipe request JSONL from stdin, print every daemon line
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
            "read {}: {error}\n(build it: cargo build --release --target x86_64-unknown-linux-musl -p harvesterd)",
            cli.daemon_bin.display()
        )
    })?;

    let target = SshTarget {
        dest: cli.dest,
        port: None,
    };
    let connected = bootstrap_and_connect(&Host::Ssh(target.clone()), &daemon_binary).await?;
    eprintln!(
        "harvesterd {} on {} ({:?})",
        connected.daemon_version, target.dest, connected.outcome
    );

    match cli.command {
        CliCommand::Ping => Ok(()),
        CliCommand::ListSessions => list_sessions(connected.connection).await,
        CliCommand::Raw => raw(connected.connection).await,
    }
}

async fn list_sessions(connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let (client, _events) = DaemonClient::start(connection);
    let response = client
        .harvester(HarvesterCmd::ListSessions { cwd: None })
        .await?;
    let data = response.into_result()?;

    println!("{}", serde_json::to_string_pretty(&data)?);

    Ok(())
}

/// Protocol REPL and pipe: stdin lines go to the daemon as-is, daemon lines
/// come back to stdout as-is. Deliberately a byte pump in both directions —
/// decoding either side would hide exactly what this tool exists to inspect:
/// a malformed frame must reach the daemon (its parse-error response is a
/// documented path), and a line we cannot parse must still be printed rather
/// than ending the stream.
///
/// On stdin EOF the write half is dropped, which half-closes the socket; the
/// daemon then finishes its queued responses and closes, and the printer stops
/// on that EOF. Scripted use (`echo ... | harvester-cli <dest> raw`) therefore
/// sees every answer instead of racing the round trip.
async fn raw(connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let DaemonConnection {
        mut reader,
        mut writer,
    } = connection;

    let printer = tokio::spawn(async move {
        loop {
            match reader.next_raw().await {
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

        writer.send_raw_line(line.as_bytes()).await?;
    }

    drop(writer);
    let _ = printer.await;

    Ok(())
}
