//! harvesterd — per-machine daemon owning `pi --mode rpc` children.
//! Contract: docs/DAEMON.md (protocol v1). Stateless by design: pi's session
//! files are the only source of truth; we hold pids, pipes and subscribers.

mod daemon;
mod fslist;
mod ops;
mod passthrough;
mod server;
mod session;

use std::path::PathBuf;

use tokio::net::{UnixListener, UnixStream};

fn harvester_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME must be set");
    PathBuf::from(home).join(".harvester")
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // CLI surface is exactly two forms: `harvesterd` (run) and
    // `harvesterd version` (bootstrap handshake). An arg parser arrives with
    // the third form, not before.
    match std::env::args().nth(1) {
        None => {}
        Some(arg) if arg == "version" => {
            println!("{}", harvester_proto::PROTOCOL_VERSION);
            return;
        }
        Some(arg) => {
            eprintln!("unknown argument '{arg}' (usage: harvesterd [version])");
            std::process::exit(2);
        }
    }

    let dir = harvester_dir();
    let log_dir = dir.join("log");
    std::fs::create_dir_all(&log_dir).expect("create ~/.harvester/log");
    let sock_path = dir.join("harvesterd.sock");

    // The socket bind is the single-instance lock. A stale socket file (daemon
    // died without unlinking) refuses connections; a live one accepts them.
    if sock_path.exists() {
        match UnixStream::connect(&sock_path).await {
            Ok(_) => {
                eprintln!("harvesterd already running on {}", sock_path.display());
                std::process::exit(1);
            }
            Err(_) => {
                // Invariant: nobody is listening, so the file is stale.
                std::fs::remove_file(&sock_path).expect("unlink stale socket");
            }
        }
    }

    let listener = UnixListener::bind(&sock_path).expect("bind unix socket");
    eprintln!(
        "harvesterd {} listening on {}",
        harvester_proto::PROTOCOL_VERSION,
        sock_path.display()
    );

    let daemon = daemon::Daemon::new(log_dir);
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = daemon.clone();
                tokio::spawn(async move { server::serve_connection(daemon, stream).await });
            }
            Err(error) => eprintln!("accept failed: {error}"),
        }
    }
}
