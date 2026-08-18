//! Harvester — the desktop client (docs/SPEC.md).
//!
//! `harvester` is the normal launch (machines come from providers and the
//! workspace list); `harvester <ssh-destination>` adds that destination as an
//! ad-hoc machine (the dev workflow). Argument parsing stays manual — clap
//! lives in harvester-cli only, and one positional argument does not earn it
//! here either.

mod catalog;
mod chat;
mod choices;
mod desktop_shell;
mod dialogs;
mod models;
mod session_state;
mod store;
mod ui;
mod workbench;
mod workspaces;

/// The musl harvesterd baked in by build.rs; empty in dev builds made without
/// the musl artifact (then `daemon_binary` falls back to the target dir).
const EMBEDDED_DAEMON: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/harvesterd"));
/// Dev fallback, relative to the repo-root cwd of `cargo run -p harvester`.
const DEV_DAEMON_PATH: &str = "target/x86_64-unknown-linux-musl/release/harvesterd";

fn main() {
    let argv_machine = std::env::args().nth(1);
    let daemon_binary = match daemon_binary() {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("harvester: {error}");
            std::process::exit(1);
        }
    };

    desktop_shell::run(argv_machine, daemon_binary);
}

fn daemon_binary() -> Result<Vec<u8>, String> {
    if !EMBEDDED_DAEMON.is_empty() {
        return Ok(EMBEDDED_DAEMON.to_vec());
    }

    std::fs::read(DEV_DAEMON_PATH).map_err(|error| {
        format!(
            "no embedded harvesterd and reading {DEV_DAEMON_PATH} failed: {error}\n\
             build it: cargo build --release --target x86_64-unknown-linux-musl -p harvesterd"
        )
    })
}
