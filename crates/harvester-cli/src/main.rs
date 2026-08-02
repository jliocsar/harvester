//! harvester-cli — dev driver for harvester-client-core. Bootstraps
//! harvesterd on a machine, connects, and runs one command. Doubles as the
//! protocol smoke-test tool (`raw` mode replaces the throwaway python
//! drivers from the e2e rounds).

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use harvester_client_core::{bootstrap_and_connect, DaemonConnection, SshTarget};
use harvester_proto::{HarvesterCmd, Outgoing, Request};
use serde_json::json;
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
    let connected = bootstrap_and_connect(&target, &daemon_binary).await?;
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

async fn list_sessions(mut connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let request = Request::Harvester {
        id: Some(json!(1)),
        cmd: HarvesterCmd::ListSessions { cwd: None },
    };
    connection.send(&request).await?;

    while let Some(outgoing) = connection.next().await? {
        if let Outgoing::HarvesterResponse { response, .. } = outgoing {
            if response.success {
                println!("{}", serde_json::to_string_pretty(&response.data)?);

                return Ok(());
            }

            return Err(response
                .error
                .unwrap_or_else(|| "list_sessions failed".to_string())
                .into());
        }
    }

    Err("connection closed before list_sessions response".into())
}

/// Protocol REPL: each stdin line is one `Request` (validated locally so a
/// typo errors here instead of desyncing the daemon); every daemon line is
/// printed to stdout verbatim-as-JSON. Exits on stdin EOF.
async fn raw(connection: DaemonConnection) -> Result<(), Box<dyn std::error::Error>> {
    let DaemonConnection {
        mut reader,
        mut writer,
    } = connection;

    let printer = tokio::spawn(async move {
        loop {
            match reader.next().await {
                Ok(Some(outgoing)) => match serde_json::to_string(&outgoing) {
                    Ok(line) => println!("{line}"),
                    Err(error) => eprintln!("encode outgoing: {error}"),
                },
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

        match serde_json::from_str::<Request>(&line) {
            Ok(request) => writer.send(&request).await?,
            Err(error) => eprintln!("invalid request line: {error}"),
        }
    }

    printer.abort();

    Ok(())
}
